package io.trafficpolice.capture.core;

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
            rec.emit(new Event.Fail(rec.now(), id, phase, canceled, false, error, conn, trailers, grpc));
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

        /**
         * Whether {@code len} more bytes would all be captured: a body made of messages (gRPC) is
         * cut between them, never inside one.
         */
        public synchronized boolean fits(long len) {
            return !ended && capture && captured + len <= cap;
        }

        /** Bytes that were not captured because the cap was reached: counted, and reported once. */
        public void skipOverCap(long len) {
            if (capture && len > 0) {
                txn.rec.capReached();
            }
            skip(len);
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
