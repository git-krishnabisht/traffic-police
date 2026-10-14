package io.trafficpolice.capture.core;

import java.util.Arrays;
import java.util.concurrent.atomic.AtomicBoolean;

/** One HTTP exchange on the network (PROTOCOL.md §1 {@code txn}). Thread-safe. */
public final class Txn {
    public final long id;
    public final Body request;
    public final Body response;
    final Recorder rec;
    private final CaptureConfig config;
    private Body delivered;
    private final AtomicBoolean finished = new AtomicBoolean();
    private volatile boolean responded;

    Txn(Recorder rec, long id, CaptureConfig config) {
        this.rec = rec;
        this.id = id;
        this.config = config;
        this.request = new Body(this, Frames.DIR_REQUEST, config.captureRequestBodies, config.bodyCap);
        this.response = new Body(this, Frames.DIR_RESPONSE, config.captureResponseBodies, config.bodyCap);
    }

    /** The body a rule gave the app instead of the original ({@code dir = 2}). */
    synchronized Body delivered() {
        if (delivered == null) {
            delivered = new Body(this, Frames.DIR_DELIVERED, config.captureResponseBodies, config.bodyCap);
        }
        return delivered;
    }

    public void response(int status, String message, String protocol, String[] headers, ConnInfo conn) {
        responded = true;
        request.flush(); // the request's last gathered bytes come before the response
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
        done(null, null);
    }

    /** Completes the transaction with its trailers and a gRPC status (either may be null). */
    public void done(String[] trailers, GrpcStatus grpc) {
        if (finished.compareAndSet(false, true)) {
            flushBodies();
            rec.emit(new Event.Done(rec.now(), id, trailers, grpc));
        }
    }

    /** Fails the transaction (once). */
    public void fail(String phase, boolean canceled, Throwable error, ConnInfo conn) {
        fail(phase, canceled, error, conn, null, null);
    }

    /** Fails the transaction (once), with the trailers and gRPC status known (either may be null). */
    public void fail(String phase, boolean canceled, Throwable error, ConnInfo conn, String[] trailers, GrpcStatus grpc) {
        if (finished.compareAndSet(false, true)) {
            flushBodies();
            rec.emit(new Event.Fail(rec.now(), id, phase, canceled, false, error, conn, trailers, grpc));
        }
    }

    /** Sends what the bodies gathered, so that it comes before the transaction's end. */
    private void flushBodies() {
        request.flush();
        response.flush();
        Body d;
        synchronized (this) {
            d = delivered;
        }
        if (d != null) {
            d.flush();
        }
    }

    /** A gRPC call's status (PROTOCOL.md §7.1 {@code done}, {@code fail}). */
    public static final class GrpcStatus {
        final int code;
        final String name;
        final String message;

        public GrpcStatus(int code, String name, String message) {
            this.code = code;
            this.name = name;
            this.message = message;
        }
    }

    /** Fails the transaction (once) with a failure a rule made. */
    void failSimulated(String phase, Throwable error) {
        if (finished.compareAndSet(false, true)) {
            flushBodies();
            rec.emit(new Event.Fail(rec.now(), id, phase, false, true, error, null, null, null));
        }
    }

    public boolean finished() {
        return finished.get();
    }

    /** The most of one WebSocket message's payload that is captured. */
    static final int WS_MESSAGE_CAP = 1 << 20;

    /**
     * A WebSocket message on this transaction's socket: {@code op} is {@code text}, {@code binary}
     * or {@code close} (with {@code code} and {@code reason}; {@code code} -1 for none). The
     * payload is captured up to the body cap (a message at most 1 MiB), when bodies of its
     * direction are captured (sent: request bodies; received: response bodies).
     */
    public void wsMessage(boolean out, String op, byte[] payload, boolean text, int code, String reason) {
        if (finished.get()) {
            return;
        }
        long size = payload == null ? 0 : payload.length;
        byte[] kept = null;
        boolean truncated = false;
        boolean capture = out ? config.captureRequestBodies : config.captureResponseBodies;
        if (payload != null && capture) {
            int cap = (int) Math.min(Math.min(config.bodyCap, WS_MESSAGE_CAP), Integer.MAX_VALUE);
            if (payload.length > cap) {
                kept = new byte[cap];
                System.arraycopy(payload, 0, kept, 0, cap);
                truncated = true;
                rec.capReached();
            } else {
                kept = payload;
            }
        }
        rec.emit(new Event.Ws(rec.now(), id, out, op, size, kept, text, truncated, code, reason));
    }

    /**
     * One direction's body: teed bytes are captured up to the cap and always counted. Writes
     * shorter than a chunk's own header are gathered into one chunk (see {@link #GATHER_BELOW}).
     */
    public static final class Body {
        private static final long PROG_INTERVAL_NS = 100_000_000L; // at most 10 prog events per second
        /**
         * Writes shorter than this (a chunk's header) are gathered with the ones around them:
         * byte-at-a-time I/O ({@code read()}, {@code write(int)}, DataInputStream) would make an
         * event and a 40-byte frame of every byte. Longer writes are sent as they come, as before.
         */
        static final int GATHER_BELOW = Frames.BODY_HEADER;
        /** Gathered bytes go out as one chunk of at most this many... */
        static final int GATHER_MAX = 8 * 1024;
        /**
         * ...that has waited at most about this long: a write that comes later sends them first,
         * and the writer sends them once they have waited this long (EventQueue), so a stream that
         * stalls after a byte still shows it. The end of the body, of the transaction (done, fail)
         * and of the request (the response) send them too.
         */
        static final long GATHER_NS = 50_000_000L;

        private final Txn txn;
        private final int dir;
        private final boolean capture;
        private final long cap;
        private long total;
        /** Bytes taken for capture: sent as chunks, or gathered. */
        private long captured;
        private long lastProg;
        private boolean started;
        private boolean ended;
        /** Gathered bytes not sent yet: the last {@code heldLen} of those captured, read from {@code heldTs} on. */
        private byte[] held;
        private int heldLen;
        private long heldTs;
        /** Whether the writer has this body on its list of those gathering (EventQueue). */
        private boolean listed;
        private byte[] one;
        private boolean capNoted;

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

        /**
         * Whether {@code len} more bytes would all be captured: a body made of messages (gRPC) is
         * cut between them, never inside one.
         */
        public synchronized boolean fits(long len) {
            return !ended && capture && captured + len <= cap;
        }

        /** Bytes that were not captured because the cap was reached: counted, and reported once. */
        public synchronized void skipOverCap(long len) {
            if (capture && len > 0) {
                capReached();
            }
            skip(len);
        }

        /** Capture stopped at the cap: reported (once per process, Recorder.capReached). */
        private void capReached() {
            if (!capNoted) {
                capNoted = true;
                txn.rec.capReached();
            }
        }

        /** Bytes that passed through but are not captured: counted and reported as progress. */
        public synchronized void skip(long len) {
            if (ended || len <= 0) {
                return;
            }
            started = true;
            total += len;
            if (capture && captured >= cap) {
                capReached();
            }
            long now = txn.rec.now();
            if (now - lastProg >= PROG_INTERVAL_NS) {
                sendHeld();
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
                if (take < GATHER_BELOW) {
                    gather(b, off, take, now);
                } else {
                    sendHeld();
                    for (int done = 0; done < take; ) {
                        int n = Math.min(Frames.MAX_CHUNK, take - done);
                        byte[] copy = new byte[n];
                        System.arraycopy(b, off + done, copy, 0, n);
                        txn.rec.emit(new Event.Chunk(now, txn.id, dir, captured, copy));
                        captured += n;
                        done += n;
                    }
                }
                if (captured == cap) {
                    sendHeld(); // nothing more will be captured
                }
                if (take < len) {
                    capReached();
                }
            } else if (capture) {
                // past the cap: a write that met it exactly did not pass it (byte by byte, none does)
                capReached();
            }
            total += len;
            // bytes not sent as chunks (over the cap, or capture off) are still reported, so sizes
            // and the captured-traffic graph stay right
            if (total > captured && now - lastProg >= PROG_INTERVAL_NS) {
                sendHeld();
                lastProg = now;
                txn.rec.emit(new Event.Prog(now, txn.id, dir, total));
            }
        }

        /** One byte the app wrote or read ({@code write(int)}, {@code read()}): gathered. */
        public synchronized void write(int b) {
            if (one == null) {
                one = new byte[1];
            }
            one[0] = (byte) b;
            write(one, 0, 1);
        }

        /** Adds a short write to the gathered bytes, sending them first when it would not fit or they are old. */
        private void gather(byte[] b, int off, int len, long now) {
            if (heldLen > 0 && (heldLen + len > GATHER_MAX || now - heldTs >= GATHER_NS)) {
                sendHeld();
            }
            if (held == null) {
                held = new byte[256];
            } else if (heldLen + len > held.length) {
                held = Arrays.copyOf(held, Math.min(GATHER_MAX, held.length * 2));
            }
            if (heldLen == 0) {
                heldTs = now;
            }
            System.arraycopy(b, off, held, heldLen, len);
            heldLen += len;
            captured += len;
            if (heldLen == GATHER_MAX) {
                sendHeld();
            } else if (!listed) {
                listed = true;
                txn.rec.gathering(this);
            }
        }

        /** The gathered bytes as one chunk, at the time the first of them came. */
        private void sendHeld() {
            if (heldLen > 0) {
                txn.rec.emit(new Event.Chunk(heldTs, txn.id, dir, captured - heldLen, Arrays.copyOf(held, heldLen)));
                heldLen = 0;
            }
        }

        /** Sends what was gathered: before an event that must come after it. */
        synchronized void flush() {
            sendHeld();
        }

        /** The writer: what was gathered has waited long enough ({@link #GATHER_NS}). */
        synchronized void sendDue() {
            listed = false;
            sendHeld();
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
            sendHeld();
            held = null;
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
