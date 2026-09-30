package io.trafficpolice.capture.okhttp;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.Txn;
import java.io.IOException;
import okhttp3.MediaType;
import okhttp3.RequestBody;
import okio.Buffer;
import okio.BufferedSink;
import okio.ForwardingSink;
import okio.Okio;
import okio.Sink;

/**
 * Captures a request body while OkHttp writes it (ARCHITECTURE.md §4.2). {@code writeTo} is
 * called once, exactly as without us, so one-shot and duplex bodies keep working;
 * {@code contentType}, {@code contentLength}, {@code isOneShot} and {@code isDuplex} delegate.
 */
final class TeeRequestBody extends RequestBody {
    private final RequestBody delegate;
    private final Txn txn;
    private final boolean marks;

    TeeRequestBody(RequestBody delegate, Txn txn, boolean marks) {
        this.delegate = delegate;
        this.txn = txn;
        this.marks = marks;
    }

    @Override
    public MediaType contentType() {
        return delegate.contentType();
    }

    @Override
    public long contentLength() throws IOException {
        return delegate.contentLength();
    }

    // Both exist from OkHttp 3.14 and are only called by OkHttp versions that have them.
    @Override
    public boolean isOneShot() {
        return delegate.isOneShot();
    }

    @Override
    public boolean isDuplex() {
        return delegate.isDuplex();
    }

    @Override
    public void writeTo(BufferedSink sink) throws IOException {
        boolean duplex = OkHttpCompat.isDuplex(delegate);
        BufferedSink tee = Okio.buffer(new TeeSink(sink, txn, duplex, marks));
        try {
            delegate.writeTo(tee);
            tee.emit();
        } catch (IOException e) {
            txn.request.end("error");
            throw e;
        } catch (RuntimeException e) {
            txn.request.end("error");
            throw e;
        }
        if (!duplex) {
            txn.request.end("complete");
            if (marks) {
                txn.mark("req_body_end");
            }
        }
    }

    /** Copies what is written before passing it on; a duplex body ends when its sink closes. */
    private static final class TeeSink extends ForwardingSink {
        private final Txn txn;
        private final boolean duplex;
        private final boolean marks;
        private final Buffer scratch = new Buffer();
        private boolean first = true;

        TeeSink(Sink delegate, Txn txn, boolean duplex, boolean marks) {
            super(delegate);
            this.txn = txn;
            this.duplex = duplex;
            this.marks = marks;
        }

        @Override
        public void write(Buffer source, long byteCount) throws IOException {
            if (byteCount > 0) {
                try {
                    if (first) {
                        first = false;
                        if (marks) {
                            txn.mark("req_body_start");
                        }
                    }
                    if (txn.request.wantsBytes()) {
                        source.copyTo(scratch, 0, byteCount);
                        byte[] bytes = scratch.readByteArray();
                        txn.request.write(bytes, 0, bytes.length);
                    } else {
                        txn.request.skip(byteCount);
                    }
                } catch (Throwable t) {
                    CaptureRuntime rt = CaptureRuntime.current();
                    if (rt != null) {
                        rt.internalError("okhttp.requestTee", t);
                    }
                }
            }
            super.write(source, byteCount);
        }

        @Override
        public void close() throws IOException {
            super.close();
            if (duplex) {
                txn.request.end("complete");
            }
        }
    }
}
