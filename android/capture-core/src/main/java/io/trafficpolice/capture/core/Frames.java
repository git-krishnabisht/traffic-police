package io.trafficpolice.capture.core;

import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;

/** Framing (PROTOCOL.md §3) and body chunks (§5). */
final class Frames {
    static final int MAX_FRAME = 16 * 1024 * 1024;
    static final int TYPE_JSON = 1;
    static final int TYPE_BODY = 2;
    static final int BODY_HEADER = 34;
    /** Most body bytes in one chunk frame. */
    static final int MAX_CHUNK = 64 * 1024;

    static final int DIR_REQUEST = 0;
    static final int DIR_RESPONSE = 1;
    static final int DIR_DELIVERED = 2;

    private Frames() {}

    static byte[] json(byte[] payload) {
        byte[] f = new byte[5 + payload.length];
        putInt(f, 0, payload.length + 1);
        f[4] = (byte) TYPE_JSON;
        System.arraycopy(payload, 0, f, 5, payload.length);
        return f;
    }

    static byte[] json(Json j) {
        return json(j.toBytes());
    }

    static byte[] body(long seq, long txn, int dir, long ts, long offset, byte[] data, int off, int len) {
        byte[] f = new byte[5 + BODY_HEADER + len];
        putInt(f, 0, 1 + BODY_HEADER + len);
        f[4] = (byte) TYPE_BODY;
        putLong(f, 5, seq);
        putLong(f, 13, txn);
        f[21] = (byte) dir;
        f[22] = 0; // flags
        putLong(f, 23, ts);
        putLong(f, 31, offset);
        System.arraycopy(data, off, f, 5 + BODY_HEADER, len);
        return f;
    }

    static void putInt(byte[] b, int at, int v) {
        b[at] = (byte) (v >>> 24);
        b[at + 1] = (byte) (v >>> 16);
        b[at + 2] = (byte) (v >>> 8);
        b[at + 3] = (byte) v;
    }

    static void putLong(byte[] b, int at, long v) {
        for (int k = 7; k >= 0; k--) {
            b[at + 7 - k] = (byte) (v >>> (k * 8));
        }
    }

    static long getLong(byte[] b, int at) {
        long v = 0;
        for (int k = 0; k < 8; k++) {
            v = (v << 8) | (b[at + k] & 0xffL);
        }
        return v;
    }

    /** One frame read from a stream: its type and payload. */
    static final class Frame {
        final int type;
        final byte[] payload;

        Frame(int type, byte[] payload) {
            this.type = type;
            this.payload = payload;
        }

        String text() {
            return new String(payload, Json.UTF_8);
        }
    }

    /** Thrown for a frame that breaks §3 (the stream is unusable afterwards). */
    static final class BadFrameException extends IOException {
        BadFrameException(String message) {
            super(message);
        }
    }

    /** Reads one frame, or throws {@link EOFException} at a clean end of stream. */
    static Frame read(InputStream in) throws IOException {
        byte[] head = new byte[5];
        int first = in.read();
        if (first < 0) {
            throw new EOFException();
        }
        head[0] = (byte) first;
        readFully(in, head, 1, 4);
        int length = ((head[0] & 0xff) << 24) | ((head[1] & 0xff) << 16) | ((head[2] & 0xff) << 8) | (head[3] & 0xff);
        if (length < 1 || length > MAX_FRAME) {
            // never allocate the claimed size of a corrupt frame
            throw new BadFrameException("frame length " + (length & 0xffffffffL) + " out of range");
        }
        byte[] payload = new byte[length - 1];
        readFully(in, payload, 0, payload.length);
        return new Frame(head[4] & 0xff, payload);
    }

    private static void readFully(InputStream in, byte[] b, int off, int len) throws IOException {
        while (len > 0) {
            int n = in.read(b, off, len);
            if (n < 0) {
                throw new EOFException("stream ended inside a frame");
            }
            off += n;
            len -= n;
        }
    }
}
