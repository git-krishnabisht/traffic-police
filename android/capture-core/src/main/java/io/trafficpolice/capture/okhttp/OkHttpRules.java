package io.trafficpolice.capture.okhttp;

import io.trafficpolice.capture.core.RuleRun;
import io.trafficpolice.capture.core.Txn;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.SequenceInputStream;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import okhttp3.Call;
import okhttp3.HttpUrl;
import okhttp3.MediaType;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.ResponseBody;
import okio.Buffer;
import okio.BufferedSource;
import okio.Okio;

/**
 * Rules in the network interceptor (PROTOCOL.md §8.4): {@code proceed()} runs once, or not at
 * all when a {@code fail} applies; body actions read the original in full, close it, and deliver
 * the new body without Content-Encoding; status and header actions leave the body streaming.
 */
final class OkHttpRules {
    private OkHttpRules() {}

    /** The rules for a request as it goes on the wire, or null. */
    static RuleRun find(Request request) {
        HttpUrl url = request.url();
        Map<String, List<String>> query = new LinkedHashMap<>();
        for (String name : url.queryParameterNames()) {
            List<String> values = new ArrayList<>();
            for (String v : url.queryParameterValues(name)) {
                values.add(v == null ? "" : v);
            }
            query.put(name, values);
        }
        return RuleRun.find(request.method(), url.scheme(), url.host(), url.port(), url.encodedPath(), query);
    }

    static RuleRun.Cancel cancel(final Call call) {
        return new RuleRun.Cancel() {
            @Override
            public boolean canceled() {
                return call.isCanceled();
            }
        };
    }

    /**
     * Applies the response actions. When {@code txn} is not null its {@code resp} is already
     * recorded; this records the body and completes it (or hands the body to the tee), and the
     * rule reports what changed.
     */
    static Response apply(RuleRun rules, Request request, Response response, Txn txn, Call call, boolean marks)
            throws IOException {
        boolean promisesBody = CaptureInterceptor.promisesBody(request, response);
        ResponseBody body = response.body();
        byte[] original = null;
        boolean read = false;
        ResponseBody streaming = body;
        if (rules.editsBody()) {
            if (body == null || !promisesBody) {
                original = new byte[0];
            } else {
                InputStream in = body.byteStream();
                byte[] head = readUpTo(in, RuleRun.BODY_LIMIT + 1);
                if (head.length <= RuleRun.BODY_LIMIT) {
                    original = head;
                    read = true;
                    body.close();
                } else {
                    // too large to rewrite: the original streams on, from where it was
                    streaming = new StreamBody(body, new SequenceInputStream(new ByteArrayInputStream(head), in));
                }
            }
        }
        if (txn != null && read) {
            if (marks) {
                txn.mark("resp_body_start");
            }
            txn.response.write(original, 0, original.length);
            txn.response.end("complete");
            if (marks) {
                txn.mark("resp_body_end");
            }
        }
        RuleRun.Delivered d = rules.editResponse(response.code(), response.message(),
                OkHttpCompat.headers(response.headers()), original, txn);
        Response out = response;
        if (d != null) {
            Response.Builder b = response.newBuilder().code(d.code).message(d.message == null ? "" : d.message);
            for (String[] op : d.headerOps) {
                if ("set".equals(op[0])) {
                    b.header(op[1], op[2]);
                } else if ("add".equals(op[0])) {
                    b.addHeader(op[1], op[2]);
                } else {
                    b.removeHeader(op[1]);
                }
            }
            if (d.body != null) {
                b.body(new BytesBody(mediaType(d.headers), d.body));
            } else if (read) {
                b.body(new BytesBody(body.contentType(), original));
            } else if (streaming != body) {
                b.body(streaming);
            }
            out = b.build();
        } else if (read) {
            // a replace found nothing: the original bytes, as they came
            out = response.newBuilder().body(new BytesBody(body.contentType(), original)).build();
        } else if (streaming != body) {
            out = response.newBuilder().body(streaming).build();
        }
        if (txn != null) {
            if (read || original != null) {
                if (!read) {
                    txn.response.end("none");
                }
                txn.done();
            } else if (out.body() != null && promisesBody && out.body().contentLength() != 0) {
                out = out.newBuilder().body(new TeeResponseBody(out.body(), txn, call, marks)).build();
            } else {
                txn.response.end("none");
                txn.done();
            }
        }
        return out;
    }

    private static MediaType mediaType(String[] headers) {
        for (int i = 0; i + 1 < headers.length; i += 2) {
            if ("Content-Type".equalsIgnoreCase(headers[i])) {
                try {
                    return MediaType.parse(headers[i + 1]);
                } catch (Throwable t) {
                    return null;
                }
            }
        }
        return null;
    }

    /** Up to {@code max} bytes, or fewer at the end of the stream. */
    private static byte[] readUpTo(InputStream in, int max) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream(Math.min(max, 64 * 1024));
        byte[] buf = new byte[16 * 1024];
        int n;
        while (out.size() < max && (n = in.read(buf, 0, Math.min(buf.length, max - out.size()))) != -1) {
            out.write(buf, 0, n);
        }
        return out.toByteArray();
    }

    /** A body from bytes in memory. */
    static final class BytesBody extends ResponseBody {
        private final MediaType type;
        private final byte[] bytes;
        private BufferedSource source;

        BytesBody(MediaType type, byte[] bytes) {
            this.type = type;
            this.bytes = bytes;
        }

        @Override
        public MediaType contentType() {
            return type;
        }

        @Override
        public long contentLength() {
            return bytes.length;
        }

        @Override
        public synchronized BufferedSource source() {
            if (source == null) {
                source = new Buffer().write(bytes);
            }
            return source;
        }
    }

    /** The original body continued from a stream (after the first bytes were read). */
    static final class StreamBody extends ResponseBody {
        private final ResponseBody original;
        private final InputStream in;
        private BufferedSource source;

        StreamBody(ResponseBody original, InputStream in) {
            this.original = original;
            this.in = in;
        }

        @Override
        public MediaType contentType() {
            return original.contentType();
        }

        @Override
        public long contentLength() {
            return original.contentLength();
        }

        @Override
        public synchronized BufferedSource source() {
            if (source == null) {
                source = Okio.buffer(Okio.source(in));
            }
            return source;
        }

        @Override
        public void close() {
            try {
                in.close();
            } catch (IOException ignored) {
                // closing anyway
            }
            original.close();
        }
    }
}
