package io.trafficpolice.capture.okhttp;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.Txn;
import java.io.IOException;
import okhttp3.Call;
import okhttp3.MediaType;
import okhttp3.ResponseBody;
import okio.Buffer;
import okio.BufferedSource;
import okio.ForwardingSource;
import okio.Okio;
import okio.Source;

/**
 * Captures a response body as the app reads it (ARCHITECTURE.md §4.2): no extra reads, and at
 * most one segment of read-ahead, exactly like the app's own buffered source. The bytes are as
 * they came off the wire (still Content-Encoded: OkHttp's transparent gzip runs after network
 * interceptors). EOF completes the transaction; closing early or a read error ends it too.
 */
final class TeeResponseBody extends ResponseBody {
    private final ResponseBody delegate;
    private final Txn txn;
    private final Call call;
    private final boolean marks;
    private BufferedSource source;

    TeeResponseBody(ResponseBody delegate, Txn txn, Call call, boolean marks) {
        this.delegate = delegate;
        this.txn = txn;
        this.call = call;
        this.marks = marks;
    }

    @Override
    public MediaType contentType() {
        return delegate.contentType();
    }

    @Override
    public long contentLength() {
        return delegate.contentLength();
    }

    @Override
    public synchronized BufferedSource source() {
        if (source == null) {
            source = Okio.buffer(new TeeSource(delegate.source()));
        }
        return source;
    }

    @Override
    public void close() {
        delegate.close();
        endEarly();
    }

    private void endEarly() {
        try {
            if (txn.response.end("closed_early")) {
                txn.done();
            }
        } catch (Throwable t) {
            internal(t);
        }
    }

    private static void internal(Throwable t) {
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt != null) {
            rt.internalError("okhttp.responseTee", t);
        }
    }

    private final class TeeSource extends ForwardingSource {
        private final Buffer scratch = new Buffer();
        private boolean first = true;

        TeeSource(Source delegate) {
            super(delegate);
        }

        @Override
        public long read(Buffer sink, long byteCount) throws IOException {
            long n;
            try {
                n = super.read(sink, byteCount);
            } catch (IOException e) {
                failed(e);
                throw e;
            } catch (RuntimeException e) {
                failed(e);
                throw e;
            }
            try {
                if (n == -1) {
                    if (txn.response.end("complete")) {
                        if (marks) {
                            txn.mark("resp_body_end");
                        }
                        txn.done();
                    }
                } else if (n > 0) {
                    if (first) {
                        first = false;
                        if (marks) {
                            txn.mark("resp_body_start");
                        }
                    }
                    if (txn.response.wantsBytes()) {
                        sink.copyTo(scratch, sink.size() - n, n);
                        byte[] bytes = scratch.readByteArray();
                        txn.response.write(bytes, 0, bytes.length);
                    } else {
                        txn.response.skip(n);
                    }
                }
            } catch (Throwable t) {
                internal(t);
            }
            return n;
        }

        private void failed(Throwable e) {
            try {
                if (txn.response.end("error")) {
                    txn.fail("response_body", call.isCanceled(), e, null);
                }
            } catch (Throwable t) {
                internal(t);
            }
        }

        @Override
        public void close() throws IOException {
            super.close();
            endEarly();
        }
    }
}
