package io.trafficpolice.capture.core;

import java.io.IOException;
import java.util.List;
import java.util.concurrent.ConcurrentLinkedQueue;

/**
 * The single writer thread (ARCHITECTURE.md §4.5): assigns {@code seq}, encodes, appends to the
 * replay ring, and writes to the attached host. Everything that writes to an attached client
 * runs here, so frames never interleave.
 */
final class EventWriter implements Runnable {
    private final CaptureRuntime rt;
    private final EventQueue queue;
    private final ReplayRing ring;
    private final ConcurrentLinkedQueue<Runnable> tasks = new ConcurrentLinkedQueue<>();
    private volatile boolean running = true;
    private long nextSeq = 1;
    /** The live client; touched on this thread only. */
    private ClientSession client;

    EventWriter(CaptureRuntime rt, EventQueue queue, ReplayRing ring) {
        this.rt = rt;
        this.queue = queue;
        this.ring = ring;
    }

    @Override
    public void run() {
        while (running) {
            runTasks();
            Event e;
            try {
                e = queue.poll(250);
            } catch (InterruptedException ie) {
                break;
            }
            Event.Dropped dropped = queue.takeDropped(rt.platform.nanoTime());
            if (dropped != null) {
                emit(dropped);
            }
            if (e != null) {
                emit(e);
            }
            if (client != null && queue.isEmpty()) {
                try {
                    client.flush();
                } catch (IOException ex) {
                    dropClient();
                }
            }
        }
        runTasks();
    }

    private void runTasks() {
        Runnable r;
        while ((r = tasks.poll()) != null) {
            try {
                r.run();
            } catch (Throwable t) {
                rt.internalError("writer.task", t);
            }
        }
    }

    private void emit(Event e) {
        byte[] frame;
        try {
            frame = e.encode(nextSeq);
        } catch (Throwable t) {
            rt.internalError("encode", t);
            return;
        }
        long seq = nextSeq++;
        ring.add(seq, e, frame);
        if (client != null) {
            try {
                client.write(frame);
            } catch (IOException ex) {
                dropClient();
            }
        }
    }

    private void dropClient() {
        if (client != null) {
            client.close();
            client = null;
        }
    }

    /** Replays the ring after {@code resumeAfterSeq} to {@code session}, then streams to it. */
    void attach(final ClientSession session, final long resumeAfterSeq) {
        post(new Runnable() {
            @Override
            public void run() {
                if (client != null && client != session) {
                    client.sayGoodbye("replaced", "another host connected");
                    client.close();
                    client = null;
                }
                List<ReplayRing.Entry> backlog = ring.after(resumeAfterSeq);
                try {
                    Json begin = new Json().obj().kv("t", "replay").kv("phase", "begin");
                    if (!backlog.isEmpty()) {
                        begin.kv("from_seq", backlog.get(0).seq).kv("to_seq", backlog.get(backlog.size() - 1).seq);
                    }
                    session.write(Frames.json(begin.kv("events", backlog.size()).endObj()));
                    for (ReplayRing.Entry entry : backlog) {
                        session.write(entry.frame);
                    }
                    session.write(Frames.json(new Json().obj().kv("t", "replay").kv("phase", "end").endObj()));
                    session.flush();
                    client = session;
                } catch (IOException ex) {
                    session.close();
                }
            }
        });
    }

    void detach(final ClientSession session) {
        post(new Runnable() {
            @Override
            public void run() {
                if (client == session) {
                    client = null;
                }
                session.close();
            }
        });
    }

    /** A control message (ack, pong) for {@code session}, written in order with events. */
    void control(final ClientSession session, final byte[] frame) {
        post(new Runnable() {
            @Override
            public void run() {
                if (client == session) {
                    try {
                        client.write(frame);
                        client.flush();
                    } catch (IOException ex) {
                        dropClient();
                    }
                }
            }
        });
    }

    boolean hasClient() {
        return client != null;
    }

    void stop() {
        running = false;
        post(new Runnable() {
            @Override
            public void run() {
                if (client != null) {
                    client.sayGoodbye("shutdown", null);
                }
                dropClient();
            }
        });
    }

    private void post(Runnable r) {
        tasks.add(r);
        queue.wake();
    }
}
