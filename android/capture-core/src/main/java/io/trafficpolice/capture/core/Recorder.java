package io.trafficpolice.capture.core;

import java.util.concurrent.atomic.AtomicLong;

/**
 * The capture API every hook calls. Cheap on the calling thread: it builds small event objects
 * and copies body bytes; encoding and IO happen on the writer thread.
 */
public final class Recorder {
    private final CaptureRuntime rt;
    private final AtomicLong txnIds = new AtomicLong();
    private final AtomicLong callIds = new AtomicLong();

    Recorder(CaptureRuntime rt) {
        this.rt = rt;
    }

    /** False while paused: hooks then create no new transactions. */
    public boolean recording() {
        return rt.config().recording;
    }

    public CaptureConfig config() {
        return rt.config();
    }

    public long now() {
        return rt.platform.nanoTime();
    }

    public long newCallId() {
        return callIds.incrementAndGet();
    }

    /** What a hook knows when a request starts. */
    public static final class RequestInfo {
        public long ts;
        public long call;
        public int hop;
        public String method;
        public String url;
        public String[] headers;
        public String clientKind;
        public String clientVersion;
        public ThreadStack stack;
        public boolean hasBody;
        public long bodyLength = -1;
        public String bodyType;
        public boolean oneShot;
        public boolean duplex;
        public String[] markNames;
        public long[] markTimes;
        public ConnInfo conn;
    }

    /** Starts a transaction and records its {@code req} event. */
    public Txn start(RequestInfo r) {
        CaptureConfig config = rt.config();
        Txn txn = new Txn(this, txnIds.incrementAndGet(), config);
        Event.BodyInfo body = r.hasBody ? new Event.BodyInfo(r.bodyLength, r.bodyType, r.oneShot, r.duplex) : null;
        emit(new Event.Req(r.ts != 0 ? r.ts : now(), txn.id, r.call, r.hop, r.method, r.url, r.headers,
                r.clientKind, r.clientVersion, r.stack, body, r.markNames, r.markTimes, r.conn));
        return txn;
    }

    void emit(Event e) {
        rt.queue.offer(e);
    }

    void capReached() {
        rt.diagOnce("body_cap_reached", "info",
                "a body exceeded the capture cap; its size is still counted (set_config body_cap to raise it)");
    }
}
