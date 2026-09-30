package io.trafficpolice.capture.core;

import java.util.Map;

/**
 * A captured event, created on an app thread with raw data and encoded to a frame on the
 * writer thread, where it also gets its {@code seq} (PROTOCOL.md §7.1).
 */
abstract class Event {
    /** Which replay ring keeps the frame. */
    static final int RING_TXN = 0;
    static final int RING_DIAG = 1;
    static final int RING_TRAFFIC = 2;

    final long ts;
    /** Transaction id, or 0 for events that belong to no transaction. */
    final long txn;

    Event(long ts, long txn) {
        this.ts = ts;
        this.txn = txn;
    }

    abstract byte[] encode(long seq);

    /** Approximate memory while queued, for the queue's byte budget. */
    int size() {
        return 96;
    }

    /** Body bytes carried, for the ring's body budget. */
    int bodyBytes() {
        return 0;
    }

    int ring() {
        return txn != 0 ? RING_TXN : RING_DIAG;
    }

    Json start(String type, long seq) {
        Json j = new Json();
        j.obj().kv("t", type).kv("seq", seq).kv("ts", ts);
        if (txn != 0) {
            j.kv("txn", txn);
        }
        return j;
    }

    static int stringsSize(String[] a) {
        int n = 0;
        if (a != null) {
            for (String s : a) {
                n += s == null ? 4 : s.length() * 2 + 24;
            }
        }
        return n;
    }

    static String dirName(int dir) {
        switch (dir) {
            case Frames.DIR_REQUEST:
                return "request";
            case Frames.DIR_RESPONSE:
                return "response";
            default:
                return "delivered";
        }
    }

    // --- transaction events ---------------------------------------------------------------

    /** {@code req}: request started. */
    static final class Req extends Event {
        final long call;
        final int hop;
        final String method;
        final String url;
        final String[] headers;
        final String clientKind;
        final String clientVersion;
        final ThreadStack stack;
        final BodyInfo body;
        final String[] markNames;
        final long[] markTimes;
        final ConnInfo conn;

        Req(long ts, long txn, long call, int hop, String method, String url, String[] headers, String clientKind,
                String clientVersion, ThreadStack stack, BodyInfo body, String[] markNames, long[] markTimes,
                ConnInfo conn) {
            super(ts, txn);
            this.call = call;
            this.hop = hop;
            this.method = method;
            this.url = url;
            this.headers = headers;
            this.clientKind = clientKind;
            this.clientVersion = clientVersion;
            this.stack = stack;
            this.body = body;
            this.markNames = markNames;
            this.markTimes = markTimes;
            this.conn = conn;
        }

        @Override
        int size() {
            return 256 + url.length() * 2 + stringsSize(headers) + (stack == null ? 0 : stack.sizeEstimate() * 96);
        }

        @Override
        byte[] encode(long seq) {
            Json j = start("req", seq);
            j.kv("call", call).kv("hop", hop).kv("method", method).kv("url", url);
            j.headers("headers", headers);
            j.key("client").obj().kv("kind", clientKind);
            if (clientVersion != null) {
                j.kv("version", clientVersion);
            }
            j.endObj();
            if (stack != null) {
                stack.writeThread(j);
                stack.writeStack(j);
            }
            if (body != null) {
                j.key("body").obj().kv("length", body.length).kv("type", body.contentType)
                        .kv("one_shot", body.oneShot).kv("duplex", body.duplex).endObj();
            }
            j.key("marks").arr();
            if (markNames != null) {
                for (int i = 0; i < markNames.length; i++) {
                    j.arr().str(markNames[i]).num(markTimes[i]).endArr();
                }
            }
            j.endArr();
            if (conn != null) {
                j.key("conn");
                conn.write(j);
            }
            return Frames.json(j.endObj());
        }
    }

    /** What is known about a request body before it is written. */
    static final class BodyInfo {
        final long length;
        final String contentType;
        final boolean oneShot;
        final boolean duplex;

        BodyInfo(long length, String contentType, boolean oneShot, boolean duplex) {
            this.length = length;
            this.contentType = contentType;
            this.oneShot = oneShot;
            this.duplex = duplex;
        }
    }

    /** {@code resp}: response headers received. */
    static final class Resp extends Event {
        final int status;
        final String message;
        final String protocol;
        final String[] headers;
        final ConnInfo conn;

        Resp(long ts, long txn, int status, String message, String protocol, String[] headers, ConnInfo conn) {
            super(ts, txn);
            this.status = status;
            this.message = message;
            this.protocol = protocol;
            this.headers = headers;
            this.conn = conn;
        }

        @Override
        int size() {
            return 160 + stringsSize(headers);
        }

        @Override
        byte[] encode(long seq) {
            Json j = start("resp", seq);
            j.kv("status", status).kv("message", message == null ? "" : message);
            if (protocol != null) {
                j.kv("protocol", protocol);
            }
            j.headers("headers", headers);
            if (conn != null) {
                j.key("conn");
                conn.write(j);
            }
            return Frames.json(j.endObj());
        }
    }

    /** A body chunk (frame type 2). */
    static final class Chunk extends Event {
        final int dir;
        final long offset;
        final byte[] data;

        Chunk(long ts, long txn, int dir, long offset, byte[] data) {
            super(ts, txn);
            this.dir = dir;
            this.offset = offset;
            this.data = data;
        }

        @Override
        int size() {
            return 64 + data.length;
        }

        @Override
        int bodyBytes() {
            return data.length;
        }

        @Override
        byte[] encode(long seq) {
            return Frames.body(seq, txn, dir, ts, offset, data, 0, data.length);
        }
    }

    /** {@code body_end}. */
    static final class BodyEnd extends Event {
        final int dir;
        final long bytes;
        final long captured;
        final String state;

        BodyEnd(long ts, long txn, int dir, long bytes, long captured, String state) {
            super(ts, txn);
            this.dir = dir;
            this.bytes = bytes;
            this.captured = captured;
            this.state = state;
        }

        @Override
        byte[] encode(long seq) {
            Json j = start("body_end", seq);
            j.kv("dir", dirName(dir)).kv("bytes", bytes).kv("captured", captured).kv("state", state);
            return Frames.json(j.endObj());
        }
    }

    /** {@code prog}: progress past the capture cap. */
    static final class Prog extends Event {
        final int dir;
        final long bytes;

        Prog(long ts, long txn, int dir, long bytes) {
            super(ts, txn);
            this.dir = dir;
            this.bytes = bytes;
        }

        @Override
        byte[] encode(long seq) {
            return Frames.json(start("prog", seq).kv("dir", dirName(dir)).kv("bytes", bytes).endObj());
        }
    }

    /** {@code mark}. */
    static final class Mark extends Event {
        final String name;

        Mark(long ts, long txn, String name) {
            super(ts, txn);
            this.name = name;
        }

        @Override
        byte[] encode(long seq) {
            return Frames.json(start("mark", seq).kv("m", name).endObj());
        }
    }

    /** {@code done}. */
    static final class Done extends Event {
        Done(long ts, long txn) {
            super(ts, txn);
        }

        @Override
        byte[] encode(long seq) {
            return Frames.json(start("done", seq).endObj());
        }
    }

    /** {@code fail}. */
    static final class Fail extends Event {
        final String phase;
        final boolean canceled;
        final boolean simulated;
        final Throwable error;
        final ConnInfo conn;

        Fail(long ts, long txn, String phase, boolean canceled, boolean simulated, Throwable error, ConnInfo conn) {
            super(ts, txn);
            this.phase = phase;
            this.canceled = canceled;
            this.simulated = simulated;
            this.error = error;
            this.conn = conn;
        }

        @Override
        byte[] encode(long seq) {
            Json j = start("fail", seq);
            j.kv("phase", phase).kv("canceled", canceled).kv("simulated", simulated);
            j.key("error").obj().kv("class", error.getClass().getName()).kv("message", error.getMessage());
            j.key("causes").arr();
            Throwable c = error.getCause();
            for (int i = 0; c != null && c != error && i < 4; i++, c = c.getCause()) {
                j.obj().kv("class", c.getClass().getName()).kv("message", c.getMessage()).endObj();
            }
            j.endArr().endObj();
            if (conn != null) {
                j.key("conn");
                conn.write(j);
            }
            return Frames.json(j.endObj());
        }
    }

    // --- events outside transactions ---------------------------------------------------------

    /** {@code traffic}: whole-app byte counters. */
    static final class Traffic extends Event {
        final long rx;
        final long tx;
        final long since;

        Traffic(long ts, long rx, long tx, long since) {
            super(ts, 0);
            this.rx = rx;
            this.tx = tx;
            this.since = since;
        }

        @Override
        int ring() {
            return RING_TRAFFIC;
        }

        @Override
        byte[] encode(long seq) {
            Json j = start("traffic", seq).kv("rx", rx).kv("tx", tx);
            if (since > 0) {
                j.kv("since", since);
            }
            return Frames.json(j.endObj());
        }
    }

    /** {@code diag}. */
    static final class Diag extends Event {
        final String level;
        final String code;
        final String message;
        final Map<String, String> data;

        Diag(long ts, String level, String code, String message, Map<String, String> data) {
            super(ts, 0);
            this.level = level;
            this.code = code;
            this.message = message;
            this.data = data;
        }

        @Override
        byte[] encode(long seq) {
            Json j = start("diag", seq).kv("level", level).kv("code", code).kv("message", message);
            if (data != null && !data.isEmpty()) {
                j.key("data").obj();
                for (Map.Entry<String, String> e : data.entrySet()) {
                    j.kv(e.getKey(), e.getValue());
                }
                j.endObj();
            }
            return Frames.json(j.endObj());
        }
    }

    /** {@code dropped}: events lost to queue overflow (created by the writer). */
    static final class Dropped extends Event {
        final long events;
        final long bytes;
        final long[] txns;
        final boolean txnsTruncated;

        Dropped(long ts, long events, long bytes, long[] txns, boolean txnsTruncated) {
            super(ts, 0);
            this.events = events;
            this.bytes = bytes;
            this.txns = txns;
            this.txnsTruncated = txnsTruncated;
        }

        @Override
        byte[] encode(long seq) {
            Json j = start("dropped", seq).kv("events", events).kv("bytes", bytes);
            j.key("txns").arr();
            for (long t : txns) {
                j.num(t);
            }
            j.endArr().kv("txns_truncated", txnsTruncated);
            return Frames.json(j.endObj());
        }
    }
}
