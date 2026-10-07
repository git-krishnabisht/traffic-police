package io.trafficpolice.capture.grpc;

import io.grpc.Attributes;
import io.grpc.CallOptions;
import io.grpc.ClientCall;
import io.grpc.ClientStreamTracer;
import io.grpc.Context;
import io.grpc.Deadline;
import io.grpc.ForwardingClientCall;
import io.grpc.ForwardingClientCallListener;
import io.grpc.Grpc;
import io.grpc.InternalMetadata;
import io.grpc.KnownLength;
import io.grpc.Metadata;
import io.grpc.MethodDescriptor;
import io.grpc.Status;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.ConnInfo;
import io.trafficpolice.capture.core.Recorder;
import io.trafficpolice.capture.core.ThreadStack;
import io.trafficpolice.capture.core.Txn;
import java.io.IOException;
import java.io.InputStream;
import java.net.InetSocketAddress;
import java.net.SocketAddress;
import java.nio.charset.Charset;
import java.security.cert.Certificate;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.TimeUnit;
import javax.net.ssl.SSLSession;

/**
 * One captured gRPC call (PROTOCOL.md §7.1; ARCHITECTURE.md §4.9): the call the app holds, the
 * listener that sees what arrives, and the stream tracers that see the stream on the wire.
 *
 * <p>A call is one transaction: {@code POST <scheme>://<authority>/<service>/<method>}, its
 * messages as the request and response bodies in gRPC's framing (each message uncompressed,
 * after a flag byte and its length), and its trailers and status on {@code done} (or {@code fail}
 * when the client library ended it itself: a deadline, a cancel, no connection). The {@code req}
 * waits for the stream, as OkHttp's waits for the network: then the scheme, the address, the TLS
 * session and the headers as sent (with call credentials such as {@code authorization}) are
 * known. Messages sent before that wait with it.
 */
final class GrpcCall<Q, R> extends ForwardingClientCall.SimpleForwardingClientCall<Q, R> {
    private final State<Q, R> s;

    GrpcCall(ClientCall<Q, R> delegate, State<Q, R> s) {
        super(delegate);
        this.s = s;
    }

    @Override
    public void start(Listener<R> listener, Metadata headers) {
        try {
            s.started(headers);
        } catch (Throwable t) {
            s.rt.internalError("grpc.start", t);
        }
        super.start(new CaptureListener<>(listener, s, this), headers);
    }

    @Override
    public void sendMessage(Q message) {
        try {
            s.sent(message);
        } catch (Throwable t) {
            s.rt.internalError("grpc.sendMessage", t);
        }
        super.sendMessage(message);
    }

    @Override
    public void halfClose() {
        try {
            s.halfClosed();
        } catch (Throwable t) {
            s.rt.internalError("grpc.halfClose", t);
        }
        super.halfClose();
    }

    @Override
    public void cancel(String message, Throwable cause) {
        s.canceling();
        super.cancel(message, cause);
    }

    /** What arrives: headers, each message, the end. Never throws (gRPC would cancel the call). */
    static final class CaptureListener<R> extends ForwardingClientCallListener.SimpleForwardingClientCallListener<R> {
        private final State<?, R> s;
        private final ClientCall<?, ?> call;

        CaptureListener(ClientCall.Listener<R> delegate, State<?, R> s, ClientCall<?, ?> call) {
            super(delegate);
            this.s = s;
            this.call = call;
        }

        @Override
        public void onHeaders(Metadata headers) {
            try {
                s.headers(headers, attributes(call));
            } catch (Throwable t) {
                s.rt.internalError("grpc.onHeaders", t);
            }
            super.onHeaders(headers);
        }

        @Override
        public void onMessage(R message) {
            try {
                s.received(message);
            } catch (Throwable t) {
                s.rt.internalError("grpc.onMessage", t);
            }
            super.onMessage(message);
        }

        @Override
        public void onClose(Status status, Metadata trailers) {
            try {
                s.closed(status, trailers, attributes(call));
            } catch (Throwable t) {
                s.rt.internalError("grpc.onClose", t);
            }
            super.onClose(status, trailers);
        }

        private static Attributes attributes(ClientCall<?, ?> call) {
            try {
                return call.getAttributes();
            } catch (Throwable t) {
                return null;
            }
        }
    }

    /** The stream's tracer: when the stream exists, its headers on the wire, and the trailers. */
    static final class Tracer extends ClientStreamTracer {
        private final State<?, ?> s;

        Tracer(State<?, ?> s) {
            this.s = s;
        }

        /**
         * gRPC 1.40 and newer (compiled against 1.21, so not an override here): the stream exists,
         * on a transport with these attributes, and these are the headers it sends.
         */
        public void streamCreated(Attributes transportAttrs, Metadata headers) {
            s.streamCreated(transportAttrs, headers);
        }

        @Override
        public void outboundHeaders() {
            s.mark("req_headers_end");
        }

        @Override
        public void inboundHeaders() {
            s.mark("resp_headers_start");
        }

        @Override
        public void inboundTrailers(Metadata trailers) {
            s.trailersArrived();
        }
    }

    /** Everything known about one call; every callback thread goes through its lock. */
    static final class State<Q, R> {
        private static final Charset ASCII = Charset.forName("US-ASCII");
        /** gRPC 1.40 added {@code ClientStreamTracer.streamCreated}; before, the factory ran there. */
        private static final boolean STREAM_CREATED = hasStreamCreated();
        /** Connections seen, so the second call on one says it was reused. */
        private static final Set<String> CONNECTIONS =
                Collections.newSetFromMap(new ConcurrentHashMap<String, Boolean>());

        final CaptureRuntime rt;
        private final Recorder rec;
        private final MethodDescriptor<Q, R> method;
        private final String authority;
        private final ThreadStack stack;
        private final long callId;
        private final long callStart;
        private final Deadline deadline;
        private String[] appHeaders;
        private String[] wireHeaders;
        private ConnInfo conn;
        private String scheme;
        private boolean streamSeen;
        private boolean trailersSeen;
        private Txn txn;
        /** Request messages sent before the transaction: frames, or the sizes of those over the cap. */
        private final List<Object> pending = new ArrayList<>();
        private long pendingBytes;
        private boolean requestStarted;
        private boolean requestEnded;
        private boolean responseStarted;
        private boolean canceled;

        State(CaptureRuntime rt, MethodDescriptor<Q, R> method, CallOptions options, String channelAuthority) {
            this.rt = rt;
            this.rec = rt.recorder();
            this.method = method;
            this.authority = options.getAuthority() != null ? options.getAuthority() : channelAuthority;
            this.stack = ThreadStack.capture(ThreadStack.ORIGIN_CALL, rec.config().stackDepth);
            this.callId = rec.newCallId();
            this.callStart = rec.now();
            Deadline d = options.getDeadline();
            Deadline c = Context.current().getDeadline();
            this.deadline = d == null ? c : (c == null || d.isBefore(c) ? d : c);
        }

        ClientStreamTracer newTracer(ClientStreamTracer.StreamInfo info, Metadata headers) {
            if (!STREAM_CREATED) {
                // before 1.40 the transport makes the tracers as it creates the stream
                streamCreated(legacyTransportAttrs(info), headers);
            }
            return new Tracer(this);
        }

        @SuppressWarnings("deprecation") // removed in later versions; only called before 1.40
        private static Attributes legacyTransportAttrs(ClientStreamTracer.StreamInfo info) {
            try {
                return info.getTransportAttrs();
            } catch (Throwable t) {
                return null;
            }
        }

        synchronized void started(Metadata headers) {
            appHeaders = snapshot(headers);
        }

        void sent(Q message) {
            Object frame = frame(method.getRequestMarshaller(), message);
            synchronized (this) {
                requestStarted = true;
                if (txn == null) {
                    if (frame instanceof byte[] && pendingBytes + ((byte[]) frame).length > rec.config().bodyCap) {
                        frame = (long) ((byte[]) frame).length;
                    }
                    if (frame instanceof byte[]) {
                        pendingBytes += ((byte[]) frame).length;
                    }
                    pending.add(frame);
                } else {
                    write(txn.request, frame);
                }
            }
        }

        synchronized void halfClosed() {
            requestEnded = true;
            if (txn != null) {
                txn.request.end("complete");
                txn.mark("req_body_end");
            }
        }

        synchronized void canceling() {
            canceled = true;
        }

        synchronized void mark(String name) {
            if (txn != null) {
                txn.mark(name);
            }
        }

        synchronized void trailersArrived() {
            trailersSeen = true;
        }

        synchronized void streamCreated(Attributes transportAttrs, Metadata headers) {
            if (streamSeen) {
                return; // a retry's stream: the first one's request stands
            }
            streamSeen = true;
            wireHeaders = snapshot(headers);
            learn(transportAttrs);
            ensureTxn();
        }

        synchronized void headers(Metadata headers, Attributes attrs) {
            learn(attrs);
            ensureTxn();
            txn.response(200, "", "h2", snapshot(headers), conn);
            txn.mark("resp_headers_end");
        }

        void received(R message) {
            Object frame = frame(method.getResponseMarshaller(), message);
            synchronized (this) {
                ensureTxn();
                if (!responseStarted) {
                    responseStarted = true;
                    txn.mark("resp_body_start");
                }
                write(txn.response, frame);
            }
        }

        synchronized void closed(Status status, Metadata trailers, Attributes attrs) {
            learn(attrs);
            ensureTxn();
            String[] rest = snapshot(trailers);
            // the server's status arrived in its trailers; otherwise the client library made it (a
            // deadline, a cancel, no connection). Without a tracer's word, a status with a cause is
            // the library's own
            boolean fromServer = trailersSeen
                    || (!streamSeen && status.getCause() == null && (txn.responded() || rest.length > 0));
            if (fromServer && !txn.responded()) {
                // trailers only: one HEADERS frame carried the status and the metadata; its
                // content-type goes with the response, the rest stays a trailer
                List<String> headers = new ArrayList<>();
                List<String> others = new ArrayList<>();
                for (int i = 0; i + 1 < rest.length; i += 2) {
                    List<String> to = "content-type".equalsIgnoreCase(rest[i]) ? headers : others;
                    to.add(rest[i]);
                    to.add(rest[i + 1]);
                }
                txn.response(200, "", "h2", headers.toArray(new String[0]), conn);
                rest = others.toArray(new String[0]);
            }
            if (!txn.request.ended()) {
                txn.request.end(requestEnded ? "complete" : (requestStarted ? "closed_early" : "none"));
            }
            if (responseStarted) {
                txn.response.end(status.isOk() || fromServer ? "complete" : "closed_early");
                txn.mark("resp_body_end");
            } else {
                txn.response.end("none");
            }
            Txn.GrpcStatus grpc =
                    new Txn.GrpcStatus(status.getCode().value(), status.getCode().name(), status.getDescription());
            if (fromServer) {
                txn.done(trailers(status, rest), grpc);
                return;
            }
            String phase = responseStarted || txn.responded() ? "response_body"
                    : (streamSeen ? "response_headers" : "connect");
            boolean cancel = canceled || status.getCode() == Status.Code.CANCELLED;
            txn.fail(phase, cancel, status.asRuntimeException(), conn, rest.length > 0 ? rest : null, grpc);
        }

        /** The trailers as sent: {@code grpc-status} and {@code grpc-message} (gRPC strips them), then the rest. */
        private static String[] trailers(Status status, String[] rest) {
            List<String> out = new ArrayList<>(rest.length + 4);
            out.add("grpc-status");
            out.add(String.valueOf(status.getCode().value()));
            if (status.getDescription() != null) {
                out.add("grpc-message");
                out.add(status.getDescription());
            }
            out.addAll(Arrays.asList(rest));
            return out.toArray(new String[0]);
        }

        /** The {@code req}, once: when the stream exists, or at the first thing after it. */
        private void ensureTxn() {
            if (txn != null) {
                return;
            }
            Recorder.RequestInfo r = new Recorder.RequestInfo();
            r.call = callId;
            r.method = "POST";
            r.url = (scheme != null ? scheme : "https") + "://" + authority + "/" + method.getFullMethodName();
            r.headers = requestHeaders();
            r.clientKind = "grpc";
            r.clientVersion = GrpcDetect.version();
            r.stack = stack;
            r.hasBody = true;
            r.bodyType = "application/grpc";
            MethodDescriptor.MethodType type = method.getType();
            r.duplex = type == MethodDescriptor.MethodType.CLIENT_STREAMING
                    || type == MethodDescriptor.MethodType.BIDI_STREAMING;
            r.markNames = new String[] {"call_start"};
            r.markTimes = new long[] {callStart};
            r.conn = conn;
            txn = rec.start(r);
            for (Object frame : pending) {
                write(txn.request, frame);
            }
            pending.clear();
            if (requestEnded) {
                txn.request.end("complete");
                txn.mark("req_body_end");
            }
        }

        /**
         * The headers as sent: the stream's (with call credentials) when a tracer saw them, else
         * the app's; then what the transport adds below every interceptor, which no hook sees.
         */
        private String[] requestHeaders() {
            String[] base = wireHeaders != null ? wireHeaders : (appHeaders != null ? appHeaders : new String[0]);
            List<String> out = new ArrayList<>(Arrays.asList(base));
            if (!has(base, "content-type")) {
                out.add("content-type");
                out.add("application/grpc");
            }
            if (!has(base, "te")) {
                out.add("te");
                out.add("trailers");
            }
            if (deadline != null && !has(base, "grpc-timeout")) {
                out.add("grpc-timeout");
                out.add(timeout(deadline.timeRemaining(TimeUnit.NANOSECONDS)));
            }
            return out.toArray(new String[0]);
        }

        private static boolean has(String[] pairs, String name) {
            for (int i = 0; i < pairs.length; i += 2) {
                if (name.equalsIgnoreCase(pairs[i])) {
                    return true;
                }
            }
            return false;
        }

        /** {@code grpc-timeout} as gRPC writes it: at most 8 digits, then a unit. */
        static String timeout(long nanos) {
            long n = Math.max(0, nanos);
            long cutoff = 100_000_000L;
            if (n < cutoff) {
                return n + "n";
            }
            if (n < cutoff * 1_000L) {
                return n / 1_000L + "u";
            }
            if (n < cutoff * 1_000_000L) {
                return n / 1_000_000L + "m";
            }
            if (n < cutoff * 1_000_000_000L) {
                return n / 1_000_000_000L + "S";
            }
            if (n < cutoff * 60_000_000_000L) {
                return n / 60_000_000_000L + "M";
            }
            return n / 3_600_000_000_000L + "H";
        }

        /** The transport's address and TLS session, the first time they are known. */
        private void learn(Attributes attrs) {
            if (attrs == null || conn != null) {
                return;
            }
            SocketAddress remote = attrs.get(Grpc.TRANSPORT_ATTR_REMOTE_ADDR);
            if (remote == null) {
                return;
            }
            SSLSession tls = attrs.get(Grpc.TRANSPORT_ATTR_SSL_SESSION);
            scheme = tls != null ? "https" : "http";
            String ip = null;
            int port = 0;
            if (remote instanceof InetSocketAddress) {
                InetSocketAddress a = (InetSocketAddress) remote;
                ip = a.getAddress() != null ? a.getAddress().getHostAddress() : a.getHostString();
                port = a.getPort();
            }
            SocketAddress local = attrs.get(Grpc.TRANSPORT_ATTR_LOCAL_ADDR);
            String id = "grpc:" + remote + (local instanceof InetSocketAddress ? ":" + ((InetSocketAddress) local).getPort() : "");
            boolean reused = !CONNECTIONS.add(id);
            if (CONNECTIONS.size() > 512) {
                CONNECTIONS.clear();
            }
            String version = null;
            String cipher = null;
            List<ConnInfo.Cert> peer = null;
            if (tls != null) {
                version = tls.getProtocol();
                cipher = tls.getCipherSuite();
                try {
                    Certificate[] certs = tls.getPeerCertificates();
                    peer = ConnInfo.summarize(certs != null ? Arrays.asList(certs) : null);
                } catch (Exception e) {
                    peer = null;
                }
            }
            conn = new ConnInfo(id, reused, "h2", ip, port, null, version, cipher, peer);
        }

        /** Header pairs in order; binary ({@code -bin}) values in base64, as HTTP/2 carries them. */
        static String[] snapshot(Metadata md) {
            if (md == null) {
                return new String[0];
            }
            byte[][] raw;
            try {
                raw = InternalMetadata.serialize(md);
            } catch (LinkageError e) {
                return byKeys(md);
            }
            if (raw == null) {
                return new String[0]; // an empty Metadata, before gRPC 1.2x stored it as none
            }
            String[] out = new String[raw.length];
            for (int i = 0; i + 1 < raw.length; i += 2) {
                String name = new String(raw[i], ASCII);
                out[i] = name;
                out[i + 1] = name.endsWith(Metadata.BINARY_HEADER_SUFFIX) ? base64(raw[i + 1]) : new String(raw[i + 1], ASCII);
            }
            return out;
        }

        /** Without InternalMetadata: grouped by name, in no particular name order. */
        private static String[] byKeys(Metadata md) {
            List<String> out = new ArrayList<>();
            for (String name : md.keys()) {
                if (name.endsWith(Metadata.BINARY_HEADER_SUFFIX)) {
                    Iterable<byte[]> values = md.getAll(Metadata.Key.of(name, Metadata.BINARY_BYTE_MARSHALLER));
                    if (values != null) {
                        for (byte[] v : values) {
                            out.add(name);
                            out.add(base64(v));
                        }
                    }
                } else {
                    Iterable<String> values = md.getAll(Metadata.Key.of(name, Metadata.ASCII_STRING_MARSHALLER));
                    if (values != null) {
                        for (String v : values) {
                            out.add(name);
                            out.add(v);
                        }
                    }
                }
            }
            return out.toArray(new String[0]);
        }

        private static final char[] B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".toCharArray();

        /** Base64 without padding, as gRPC sends binary headers. */
        static String base64(byte[] b) {
            StringBuilder sb = new StringBuilder((b.length + 2) / 3 * 4);
            for (int i = 0; i < b.length; i += 3) {
                int n = (b[i] & 0xff) << 16;
                int left = b.length - i;
                if (left > 1) {
                    n |= (b[i + 1] & 0xff) << 8;
                }
                if (left > 2) {
                    n |= b[i + 2] & 0xff;
                }
                sb.append(B64[(n >> 18) & 63]).append(B64[(n >> 12) & 63]);
                if (left > 1) {
                    sb.append(B64[(n >> 6) & 63]);
                }
                if (left > 2) {
                    sb.append(B64[n & 63]);
                }
            }
            return sb.toString();
        }

        /** A frame into a body: whole, or only counted when it would pass the cap. */
        private static void write(Txn.Body body, Object frame) {
            if (frame instanceof byte[]) {
                byte[] b = (byte[]) frame;
                if (body.fits(b.length)) {
                    body.write(b, 0, b.length);
                } else {
                    body.skipOverCap(b.length);
                }
            } else if (frame instanceof Long) {
                body.skipOverCap((Long) frame);
            }
        }

        /**
         * A message in gRPC's framing (flag 0, its length, its bytes), re-marshaled: a byte array,
         * or its size (a Long) when it is over the cap, or null when the marshaller hands out the
         * message itself (a stream that can be read only once).
         */
        private <T> Object frame(MethodDescriptor.Marshaller<T> marshaller, T message) {
            if (message instanceof InputStream) {
                return null;
            }
            long cap = rec.config().bodyCap;
            try (InputStream in = marshaller.stream(message)) {
                if (in == null || (Object) in == message) {
                    return null;
                }
                if (in instanceof KnownLength) {
                    int size = in.available();
                    if (size + 5L > cap) {
                        return (long) size + 5;
                    }
                    byte[] out = new byte[size + 5];
                    header(out, size);
                    int off = 5;
                    while (off < out.length) {
                        int n = in.read(out, off, out.length - off);
                        if (n < 0) {
                            break;
                        }
                        off += n;
                    }
                    return off == out.length ? out : Arrays.copyOf(out, off);
                }
                java.io.ByteArrayOutputStream buf = new java.io.ByteArrayOutputStream();
                buf.write(new byte[5]);
                byte[] chunk = new byte[8192];
                long total = 0;
                for (int n; (n = in.read(chunk)) >= 0; ) {
                    total += n;
                    if (total + 5 <= cap) {
                        buf.write(chunk, 0, n);
                    }
                }
                if (total + 5 > cap) {
                    return total + 5;
                }
                byte[] out = buf.toByteArray();
                header(out, (int) total);
                return out;
            } catch (IOException | RuntimeException e) {
                rt.internalError("grpc.marshal", e);
                return null;
            }
        }

        private static void header(byte[] out, int size) {
            out[0] = 0;
            out[1] = (byte) (size >>> 24);
            out[2] = (byte) (size >>> 16);
            out[3] = (byte) (size >>> 8);
            out[4] = (byte) size;
        }

        private static boolean hasStreamCreated() {
            try {
                ClientStreamTracer.class.getMethod("streamCreated", Attributes.class, Metadata.class);
                return true;
            } catch (NoSuchMethodException e) {
                return false;
            }
        }
    }
}
