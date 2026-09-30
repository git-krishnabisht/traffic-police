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
        record(new byte[] {(byte) b}, 0, 1);
    }

    @Override
    public void write(byte[] b, int off, int len) throws IOException {
        out.write(b, off, len); // FilterOutputStream would write one byte at a time
        record(b, off, len);
    }

    private void record(byte[] b, int off, int len) {
        try {
            if (first) {
                first = false;
                txn.mark("req_body_start");
            }
            if (txn.request.wantsBytes()) {
                txn.request.write(b, off, len);
            } else {
                txn.request.skip(len);
            }
        } catch (Throwable t) {
            CaptureRuntime rt = CaptureRuntime.current();
            if (rt != null) {
                rt.internalError("huc.requestTee", t);
            }
        }
    }

    @Override
    public void close() throws IOException {
        try {
            super.close();
        } finally {
            if (txn.request.end("complete")) {
                txn.mark("req_body_end");
            }
        }
    }
}
