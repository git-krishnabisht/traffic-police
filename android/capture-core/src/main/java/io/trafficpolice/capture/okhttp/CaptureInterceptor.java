package io.trafficpolice.capture.okhttp;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.ConnInfo;
import io.trafficpolice.capture.core.Recorder;
import io.trafficpolice.capture.core.RuleRun;
import io.trafficpolice.capture.core.ThreadStack;
import io.trafficpolice.capture.core.Txn;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.Proxy;
import java.util.Map;
import java.util.WeakHashMap;
import java.util.concurrent.atomic.AtomicLong;
import okhttp3.Connection;
import okhttp3.Handshake;
import okhttp3.Interceptor;
import okhttp3.Request;
import okhttp3.RequestBody;
import okhttp3.Response;
import okhttp3.ResponseBody;
import okhttp3.Route;

/**
 * The network interceptor (ARCHITECTURE.md §4.2). It sits after Bridge and Connect, so it sees
 * the wire request and the connection, once per network attempt. It calls {@code proceed()}
 * exactly once, never throws anything of its own, and without a running runtime (or while
 * paused) it only passes the call through.
 */
public final class CaptureInterceptor implements Interceptor {
    public static final CaptureInterceptor INSTANCE = new CaptureInterceptor();

    private static final Map<Connection, ConnInfo> CONNECTIONS = new WeakHashMap<>();
    private static final AtomicLong CONNECTION_IDS = new AtomicLong();

    private CaptureInterceptor() {}

    private static final class Version {
        static final String VALUE = OkHttpDetect.version();
    }

    static String okhttpVersion() {
        return Version.VALUE;
    }

    @Override
    public Response intercept(Chain chain) throws IOException {
        Request request = chain.request();
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt == null) {
            return chain.proceed(request);
        }
        RuleRun rules = null;
        try {
            rules = OkHttpRules.find(request);
        } catch (Throwable t) {
            rt.internalError("okhttp.rules", t);
        }
        if (!rt.recorder().recording()) {
            // paused: rules still apply (PROTOCOL.md §6); nothing is recorded
            if (rules == null) {
                return chain.proceed(request);
            }
            rules.beforeRequest(null, OkHttpRules.cancel(chain.call()));
            Response response = chain.proceed(request);
            return rules.editsResponse() ? OkHttpRules.apply(rules, request, response, null, chain.call(), false)
                    : response;
        }
        CallState state = null;
        Txn txn = null;
        Request wire = request;
        try {
            state = CallState.of(chain.call(), rt.recorder().newCallId());
            ConnInfo conn = connInfo(chain.connection());
            txn = start(rt, state, request, conn);
            state.setCurrent(txn);
            RequestBody body = request.body();
            if (body != null) {
                wire = request.newBuilder()
                        .method(request.method(), new TeeRequestBody(body, txn, !state.hasListener))
                        .build();
            }
        } catch (Throwable t) {
            rt.internalError("okhttp.intercept", t);
            if (txn == null) {
                return chain.proceed(request);
            }
            wire = request;
        }

        if (rules != null) {
            try {
                rules.beforeRequest(txn, OkHttpRules.cancel(chain.call()));
            } catch (IOException e) {
                // a simulated failure is recorded already; a delay cut short by a cancel is not
                fail(rt, state, txn, chain, e);
                throw e;
            }
        }
        Response response;
        try {
            response = chain.proceed(wire);
        } catch (IOException e) {
            fail(rt, state, txn, chain, e);
            throw e;
        } catch (RuntimeException e) {
            fail(rt, state, txn, chain, e);
            throw e;
        } catch (Error e) {
            fail(rt, state, txn, chain, e);
            throw e;
        }

        try {
            return capture(rt, state, txn, request, response, chain, rules);
        } catch (IOException e) {
            // reading the original body for a rule failed: the app sees it as the network's failure
            try {
                response.close();
            } catch (Throwable ignored) {
                // failing anyway
            }
            fail(rt, state, txn, chain, e);
            throw e;
        } catch (Throwable t) {
            rt.internalError("okhttp.response", t);
            return response;
        }
    }

    private static Txn start(CaptureRuntime rt, CallState state, Request request, ConnInfo conn) throws IOException {
        Recorder rec = rt.recorder();
        Recorder.RequestInfo r = new Recorder.RequestInfo();
        r.call = state.callId;
        r.hop = state.nextHop();
        r.method = request.method();
        r.url = request.url().toString();
        r.headers = OkHttpCompat.headers(request.headers());
        r.clientKind = "okhttp";
        r.clientVersion = okhttpVersion();
        ThreadStack stack = state.stack;
        r.stack = stack != null ? stack : ThreadStack.capture(ThreadStack.ORIGIN_INTERCEPTOR, rec.config().stackDepth);
        RequestBody body = request.body();
        if (body != null) {
            r.hasBody = true;
            r.bodyLength = safeLength(body);
            r.bodyType = body.contentType() != null ? body.contentType().toString() : null;
            r.oneShot = OkHttpCompat.isOneShot(body);
            r.duplex = OkHttpCompat.isDuplex(body);
        }
        CallState.Marks marks = state.takePending();
        r.markNames = marks.names;
        r.markTimes = marks.times;
        r.conn = conn;
        return rec.start(r);
    }

    private static long safeLength(RequestBody body) {
        try {
            return body.contentLength();
        } catch (Throwable t) {
            return -1;
        }
    }

    private static void fail(CaptureRuntime rt, CallState state, Txn txn, Chain chain, Throwable e) {
        if (txn == null) {
            return;
        }
        try {
            txn.request.end("error");
            txn.fail(state.failurePhase(), chain.call().isCanceled(), e, null);
        } catch (Throwable t) {
            rt.internalError("okhttp.fail", t);
        }
    }

    private static Response capture(CaptureRuntime rt, CallState state, Txn txn, Request request, Response response,
            Chain chain, RuleRun rules) throws IOException {
        txn.response(response.code(), response.message(), response.protocol().toString(),
                OkHttpCompat.headers(response.headers()), null);
        RequestBody requestBody = request.body();
        if (requestBody == null) {
            txn.request.end("none");
        } else if (!txn.request.ended() && !OkHttpCompat.isDuplex(requestBody)) {
            // OkHttp answered without sending the body (e.g. Expect: 100-continue refused)
            txn.request.end("closed_early");
        }
        ResponseBody body = response.body();
        boolean upgrade = response.code() == 101
                || ("upgrade".equalsIgnoreCase(request.header("Connection"))
                        && "upgrade".equalsIgnoreCase(response.header("Connection")));
        if (upgrade) {
            // never tee (or rewrite) an upgraded connection: its body is the socket (OkHttp 5.2+
            // makes it unreadable)
            txn.response.end("none");
            txn.done();
            return response;
        }
        if (rules != null && rules.editsResponse()) {
            return OkHttpRules.apply(rules, request, response, txn, chain.call(), !state.hasListener);
        }
        if (body == null || !promisesBody(request, response) || body.contentLength() == 0) {
            txn.response.end("none");
            txn.done();
            return response;
        }
        return response.newBuilder()
                .body(new TeeResponseBody(body, txn, chain.call(), !state.hasListener))
                .build();
    }

    /** OkHttp's own rule (internal {@code promisesBody}), re-implemented as recommended. */
    static boolean promisesBody(Request request, Response response) {
        if ("HEAD".equals(request.method())) {
            return false;
        }
        int code = response.code();
        if ((code < 100 || code >= 200) && code != 204 && code != 304) {
            return true;
        }
        String length = response.header("Content-Length");
        if (length != null) {
            try {
                if (Long.parseLong(length.trim()) != -1) {
                    return true;
                }
            } catch (NumberFormatException ignored) {
                // fall through
            }
        }
        return "chunked".equalsIgnoreCase(response.header("Transfer-Encoding"));
    }

    /** Built once per connection; later exchanges on it report it as reused. */
    static ConnInfo connInfo(Connection c) {
        if (c == null) {
            return null;
        }
        synchronized (CONNECTIONS) {
            ConnInfo known = CONNECTIONS.get(c);
            if (known != null) {
                return known.asReused();
            }
        }
        String protocol = c.protocol().toString();
        String ip = null;
        int port = 0;
        String proxy = null;
        Route route = c.route();
        if (route != null) {
            InetSocketAddress a = route.socketAddress();
            if (a != null) {
                ip = a.getAddress() != null ? a.getAddress().getHostAddress() : a.getHostString();
                port = a.getPort();
            }
            Proxy p = route.proxy();
            if (p != null) {
                proxy = p.type() == Proxy.Type.DIRECT ? "DIRECT" : p.type() + " " + p.address();
            }
        }
        String tls = null;
        String cipher = null;
        java.util.List<ConnInfo.Cert> peer = null;
        Handshake h = c.handshake();
        if (h != null) {
            tls = h.tlsVersion() != null ? h.tlsVersion().javaName() : null;
            cipher = h.cipherSuite().javaName();
            peer = ConnInfo.summarize(h.peerCertificates());
        }
        ConnInfo info = new ConnInfo("c-" + CONNECTION_IDS.incrementAndGet(), false, protocol, ip, port, proxy, tls,
                cipher, peer);
        synchronized (CONNECTIONS) {
            CONNECTIONS.put(c, info);
        }
        return info;
    }
}
