package io.trafficpolice.capture.huc;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.Txn;
import java.io.FilterInputStream;
import java.io.IOException;
import java.io.InputStream;

/**
 * Captures the response body as the app reads it. HttpURLConnection has already removed its own
 * transparent gzip, so these are the bytes the app sees. EOF completes the transaction; closing
 * early ends it as {@code closed_early}.
 */
final class TeeInputStream extends FilterInputStream {
    private final Txn txn;
    private boolean first = true;

    TeeInputStream(InputStream in, Txn txn) {
        super(in);
        this.txn = txn;
    }

    @Override
    public int read() throws IOException {
        int b;
        try {
            b = in.read();
        } catch (IOException e) {
            failed(e);
            throw e;
        }
        if (b < 0) {
            eof();
        } else {
            record(b);
        }
        return b;
    }

    @Override
    public int read(byte[] b, int off, int len) throws IOException {
        int n;
        try {
            n = in.read(b, off, len);
        } catch (IOException e) {
            failed(e);
            throw e;
        }
        if (n < 0) {
            eof();
        } else if (n > 0) {
            record(b, off, n);
        }
        return n;
    }

    @Override
    public long skip(long n) throws IOException {
        long skipped;
        try {
            skipped = in.skip(n);
        } catch (IOException e) {
            failed(e);
            throw e;
        }
        if (skipped > 0) {
            try {
                // skipped bytes still passed through the app's stream
                txn.response.skip(skipped);
            } catch (Throwable t) {
                internal(t);
            }
        }
        return skipped;
    }

    private void started() {
        if (first) {
            first = false;
            txn.mark("resp_body_start");
        }
    }

    private void record(byte[] b, int off, int len) {
        try {
            started();
            if (txn.response.wantsBytes()) {
                txn.response.write(b, off, len);
            } else {
                txn.response.skip(len);
            }
        } catch (Throwable t) {
            internal(t);
        }
    }

    /** One byte ({@code read()}): gathered with the bytes around it, captured or not. */
    private void record(int b) {
        try {
            started();
            txn.response.write(b);
        } catch (Throwable t) {
            internal(t);
        }
    }

    private void eof() {
        try {
            if (txn.response.end("complete")) {
                txn.mark("resp_body_end");
                txn.done();
            }
        } catch (Throwable t) {
            internal(t);
        }
    }

    private void failed(IOException e) {
        try {
            if (txn.response.end("error")) {
                txn.fail("response_body", false, e, null);
            }
        } catch (Throwable t) {
            internal(t);
        }
    }

    private static void internal(Throwable t) {
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt != null) {
            rt.internalError("huc.responseTee", t);
        }
    }

    @Override
    public void close() throws IOException {
        try {
            super.close();
        } finally {
            try {
                if (txn.response.end("closed_early")) {
                    txn.done();
                }
            } catch (Throwable t) {
                internal(t);
            }
        }
    }
}
