package io.trafficpolice.capture.core;

import java.util.concurrent.atomic.AtomicBoolean;

/** One HTTP exchange on the network (PROTOCOL.md §1 {@code txn}). Thread-safe. */
public final class Txn {
    public final long id;
    public final Body request;
    public final Body response;
    final Recorder rec;
    private final AtomicBoolean finished = new AtomicBoolean();
    private volatile boolean responded;

    Txn(Recorder rec, long id, CaptureConfig config) {
        this.rec = rec;
        this.id = id;
        this.request = new Body(this, Frames.DIR_REQUEST, config.captureRequestBodies, config.bodyCap);
        this.response = new Body(this, Frames.DIR_RESPONSE, config.captureResponseBodies, config.bodyCap);
    }

    public void response(int status, String message, String protocol, String[] headers, ConnInfo conn) {
        responded = true;
        rec.emit(new Event.Resp(rec.now(), id, status, message, protocol, headers, conn));
    }

    public boolean responded() {
        return responded;
    }

    public void mark(String name) {
        mark(name, rec.now());
    }

    public void mark(String name, long ts) {
        if (!finished.get()) {
            rec.emit(new Event.Mark(ts, id, name));
        }
    }

    /** Completes the transaction (once; nothing after a failure). */
    public void done() {
        if (finished.compareAndSet(false, true)) {
            rec.emit(new Event.Done(rec.now(), id));
        }
    }

    /** Fails the transaction (once). */
    public void fail(String phase, boolean canceled, Throwable error, ConnInfo conn) {
        if (finished.compareAndSet(false, true)) {
            rec.emit(new Event.Fail(rec.now(), id, phase, canceled, false, error, conn));
        }
    }

    public boolean finished() {
        return finished.get();
    }

    /** One direction's body: teed bytes are captured up to the cap and always counted. */
    public static final class Body {
        private static final long PROG_INTERVAL_NS = 100_000_000L; // at most 10 prog events per second

        private final Txn txn;
        private final int dir;
        private final boolean capture;
        private final long cap;
        private long total;
        private long captured;
        private long lastProg;
        private boolean started;
        private boolean ended;

        Body(Txn txn, int dir, boolean capture, long cap) {
            this.txn = txn;
            this.dir = dir;
            this.capture = capture;
            this.cap = cap;
        }

        /** Whether bytes passed now would be captured (else {@link #skip} is enough). */
        public synchronized boolean wantsBytes() {
            return !ended && capture && captured < cap;
        }

        /** Bytes that passed through but are not captured: counted and reported as progress. */
        public synchronized void skip(long len) {
            if (ended || len <= 0) {
                return;
            }
            started = true;
            total += len;
            long now = txn.rec.now();
            if (now - lastProg >= PROG_INTERVAL_NS) {
                lastProg = now;
                txn.rec.emit(new Event.Prog(now, txn.id, dir, total));
            }
        }

        /** Bytes the app wrote (request) or read (response). */
        public synchronized void write(byte[] b, int off, int len) {
            if (ended || len <= 0) {
                return;
            }
            long now = txn.rec.now();
            started = true;
            if (capture && captured < cap) {
                int take = (int) Math.min(len, cap - captured);
                for (int done = 0; done < take; ) {
                    int n = Math.min(Frames.MAX_CHUNK, take - done);
                    byte[] copy = new byte[n];
                    System.arraycopy(b, off + done, copy, 0, n);
                    txn.rec.emit(new Event.Chunk(now, txn.id, dir, captured, copy));
                    captured += n;
                    done += n;
                }
                if (take < len) {
                    txn.rec.capReached();
                }
            }
            total += len;
            // bytes not sent as chunks (over the cap, or capture off) are still reported, so sizes
            // and the captured-traffic graph stay right
            if (total > captured && now - lastProg >= PROG_INTERVAL_NS) {
                lastProg = now;
                txn.rec.emit(new Event.Prog(now, txn.id, dir, total));
            }
        }

        /**
         * Ends the body with {@code complete}, {@code closed_early}, {@code error} or {@code none};
         * {@code complete} becomes {@code truncated} or {@code not_captured} when bytes were not
         * all sent. Returns false if it had already ended.
         */
        public synchronized boolean end(String state) {
            if (ended) {
                return false;
            }
            ended = true;
            String s = state;
            if ("none".equals(state)) {
                total = 0;
                captured = 0;
            } else if (!capture && ("complete".equals(state) || "closed_early".equals(state))) {
                s = "not_captured";
            } else if ("complete".equals(state) && captured < total) {
                s = "truncated";
            }
            txn.rec.emit(new Event.BodyEnd(txn.rec.now(), txn.id, dir, total, captured, s));
            return true;
        }

        public synchronized boolean ended() {
            return ended;
        }

        public synchronized boolean started() {
            return started;
        }

        public synchronized long total() {
            return total;
        }
    }
}
