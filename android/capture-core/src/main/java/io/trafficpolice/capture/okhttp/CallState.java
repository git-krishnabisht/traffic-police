package io.trafficpolice.capture.okhttp;

import io.trafficpolice.capture.core.ThreadStack;
import io.trafficpolice.capture.core.Txn;
import java.util.ArrayList;
import java.util.Map;
import java.util.WeakHashMap;
import okhttp3.Call;

/**
 * What we know about one OkHttp {@code Call}: its id, the caller's thread and stack from
 * {@code callStart}, the transaction of the current network attempt, and timing marks that
 * happened before that transaction existed. Shared by the listener and the interceptor, which
 * find each other through {@code chain.call()} (the same {@code RealCall}). Weakly keyed, so
 * abandoned calls are collected.
 */
final class CallState {
    private static final Map<Call, CallState> CALLS = new WeakHashMap<>();

    final long callId;
    volatile ThreadStack stack;
    volatile boolean hasListener;
    private int hops;
    private Txn current;
    private boolean everStarted;
    private final ArrayList<String> pendingNames = new ArrayList<>(8);
    private final ArrayList<Long> pendingTimes = new ArrayList<>(8);
    private String lastMark;

    private CallState(long callId) {
        this.callId = callId;
    }

    static CallState of(Call call, long newCallId) {
        synchronized (CALLS) {
            CallState s = CALLS.get(call);
            if (s == null) {
                s = new CallState(newCallId);
                CALLS.put(call, s);
            }
            return s;
        }
    }

    /** Marks recorded before a transaction existed, for its {@code req}. */
    static final class Marks {
        final String[] names;
        final long[] times;

        Marks(String[] names, long[] times) {
            this.names = names;
            this.times = times;
        }
    }

    /** A timing mark: attributed to the current transaction, or kept for the next one. */
    synchronized void mark(String name, long ts) {
        lastMark = name;
        Txn t = current;
        if (t != null && !t.finished()) {
            t.mark(name, ts);
        } else {
            pendingNames.add(name);
            pendingTimes.add(ts);
        }
    }

    /** Starts the next network attempt: its hop number, and the marks that came before it. */
    synchronized int nextHop() {
        return hops++;
    }

    synchronized Marks takePending() {
        long[] times = new long[pendingTimes.size()];
        for (int i = 0; i < times.length; i++) {
            times[i] = pendingTimes.get(i);
        }
        Marks m = new Marks(pendingNames.toArray(new String[0]), times);
        pendingNames.clear();
        pendingTimes.clear();
        return m;
    }

    synchronized void setCurrent(Txn txn) {
        current = txn;
        everStarted = true;
    }

    synchronized Txn current() {
        return current;
    }

    synchronized boolean everStarted() {
        return everStarted;
    }

    /**
     * Where the current attempt failed, from the listener's last mark (PROTOCOL.md §7.1): the
     * connection already exists when network interceptors run, so failures there are in the
     * request, while waiting for response headers, or while reading the body.
     */
    synchronized String failurePhase() {
        if (!hasListener || lastMark == null) {
            return "unknown";
        }
        if (lastMark.startsWith("resp_body")) {
            return "response_body";
        }
        if (lastMark.startsWith("resp_headers") || lastMark.equals("req_headers_end") || lastMark.equals("req_body_end")) {
            return "response_headers";
        }
        return "request";
    }
}
