package io.trafficpolice.capture.huc;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.Txn;
import java.io.FilterOutputStream;
import java.io.IOException;
import java.io.OutputStream;

/** Captures what the app writes as the request body; the body ends when the stream closes. */
final class TeeOutputStream extends FilterOutputStream {
    private final Txn txn;
    private boolean first = true;

    TeeOutputStream(OutputStream out, Txn txn) {
        super(out);
        this.txn = txn;
    }

    @Override
    public void write(int b) throws IOException {
        out.write(b);
        try {
            started();
            txn.request.write(b); // gathered with the bytes around it, captured or not
        } catch (Throwable t) {
            internal(t);
        }
    }

    @Override
    public void write(byte[] b, int off, int len) throws IOException {
        out.write(b, off, len); // FilterOutputStream would write one byte at a time
        record(b, off, len);
    }

    private void started() {
        if (first) {
            first = false;
            txn.mark("req_body_start");
        }
    }

    private void record(byte[] b, int off, int len) {
        try {
            started();
            if (txn.request.wantsBytes()) {
                txn.request.write(b, off, len);
            } else {
                txn.request.skip(len);
            }
        } catch (Throwable t) {
            internal(t);
        }
    }

    private static void internal(Throwable t) {
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt != null) {
            rt.internalError("huc.requestTee", t);
        }
    }

    /** A close that fails (the body could not be sent whole) ends the body as {@code error}. */
    @Override
    public void close() throws IOException {
        boolean closed = false;
        try {
            super.close();
            closed = true;
        } finally {
            // the app gets close()'s own exception, never one of ours in its place
            try {
                if (txn.request.end(closed ? "complete" : "error") && closed) {
                    txn.mark("req_body_end");
                }
            } catch (Throwable t) {
                internal(t);
            }
        }
    }
}
