package io.trafficpolice.capture.okhttp;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.Recorder;
import io.trafficpolice.capture.core.ThreadStack;
import io.trafficpolice.capture.core.Txn;
import java.lang.reflect.Field;
import java.nio.charset.Charset;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.WebSocket;
import okhttp3.WebSocketListener;
import okio.ByteString;

/**
 * WebSockets (PROTOCOL.md §7.1 {@code ws}): the handshake as a transaction (its request when the
 * socket is opened, the 101 when it is up, done when it closes) and every message sent and
 * received on it. OkHttp sends the handshake past network interceptors and event listeners, so
 * the socket itself is wrapped: the app's {@link WebSocketListener} (for what arrives) and the
 * {@link WebSocket} the app sends with. Library mode: {@code TrafficPolice.newWebSocket}. Attach
 * mode: the exit of {@code OkHttpClient.newWebSocket}, which swaps the socket's listener for
 * ours and returns our wrapper.
 */
public final class WebSockets {
    private static final Charset UTF_8 = Charset.forName("UTF-8");
    private static final String LISTENER = CaptureListener.class.getName();

    private WebSockets() {}

    /** Library mode: {@code client.newWebSocket(request, listener)}, captured. */
    public static WebSocket newWebSocket(OkHttpClient client, Request request, WebSocketListener listener) {
        CaptureRuntime rt = CaptureRuntime.current();
        Session session;
        try {
            session = rt != null && rt.recorder().recording() ? new Session(rt, request, listener) : null;
        } catch (Throwable t) {
            if (rt != null) rt.internalError("okhttp.websocket", t);
            session = null;
        }
        if (session == null) {
            return client.newWebSocket(request, listener);
        }
        WebSocket real = client.newWebSocket(request, new CaptureListener(session));
        // the attach hook of a runtime in the same process may have wrapped it already
        if (real.getClass().getName().equals(CaptureWebSocket.class.getName())) {
            return real;
        }
        return session.wrapper(real);
    }

    /** Attach mode: what {@code OkHttpClient.newWebSocket} returned, captured from now on. */
    public static WebSocket hooked(WebSocket real) {
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt == null || real == null || !rt.recorder().recording()) {
            return real;
        }
        if (real.getClass().getName().equals(CaptureWebSocket.class.getName())) {
            return real;
        }
        try {
            Field f = listenerField(real.getClass());
            if (f == null) {
                rt.diagOnceKeyed("websocket_listener", "warn", "websocket_unhooked",
                        "a WebSocket's listener field was not found (a minified OkHttp?); its messages are not captured",
                        null);
                return real;
            }
            WebSocketListener app = (WebSocketListener) f.get(real);
            if (app == null || app.getClass().getName().equals(LISTENER)) {
                // the library's own TrafficPolice.newWebSocket made it
                return real;
            }
            Session session = new Session(rt, real.request(), app);
            f.set(real, new CaptureListener(session));
            return session.wrapper(real);
        } catch (Throwable t) {
            rt.internalError("okhttp.websocket.hook", t);
            return real;
        }
    }

    private static Field listenerField(Class<?> c) {
        for (Class<?> k = c; k != null && k != Object.class; k = k.getSuperclass()) {
            try {
                Field f = k.getDeclaredField("listener");
                if (WebSocketListener.class.isAssignableFrom(f.getType())) {
                    f.setAccessible(true);
                    return f;
                }
            } catch (NoSuchFieldException ignored) {
                // a superclass may have it
            }
        }
        return null;
    }

    /** One socket: its transaction, the app's listener, and our wrapper of it. */
    static final class Session {
        final CaptureRuntime rt;
        final WebSocketListener app;
        final Txn txn;
        private volatile boolean open;
        private CaptureWebSocket wrapper;

        Session(CaptureRuntime rt, Request request, WebSocketListener app) {
            this.rt = rt;
            this.app = app;
            Recorder rec = rt.recorder();
            Recorder.RequestInfo r = new Recorder.RequestInfo();
            r.call = rec.newCallId();
            r.method = request.method();
            r.url = request.url().toString();
            // the app's headers: OkHttp adds Upgrade, Connection and Sec-WebSocket-* itself
            r.headers = OkHttpCompat.headers(request.headers());
            r.clientKind = "okhttp";
            r.clientVersion = CaptureInterceptor.okhttpVersion();
            r.stack = ThreadStack.capture(ThreadStack.ORIGIN_CALL, rec.config().stackDepth);
            this.txn = rec.start(r);
        }

        synchronized CaptureWebSocket wrapper(WebSocket real) {
            if (wrapper == null) {
                wrapper = new CaptureWebSocket(this, real);
            }
            return wrapper;
        }

        void opened(Response response) {
            open = true;
            txn.request.end("none");
            txn.response(response.code(), response.message(), response.protocol().toString(),
                    OkHttpCompat.headers(response.headers()), null);
            txn.response.end("none");
        }

        void failed(Throwable t, Response response) {
            if (!open && response != null && !txn.responded()) {
                txn.response(response.code(), response.message(), response.protocol().toString(),
                        OkHttpCompat.headers(response.headers()), null);
            }
            txn.fail(open ? "response_body" : (response != null ? "response_headers" : "connect"), false, t, null);
        }

        void message(boolean out, String op, byte[] payload, boolean text, int code, String reason) {
            try {
                txn.wsMessage(out, op, payload, text, code, reason);
            } catch (Throwable t) {
                rt.internalError("okhttp.websocket.message", t);
            }
        }
    }

    /** What the app sends with: every message it sends, and how it closes. */
    static final class CaptureWebSocket implements WebSocket {
        private final Session s;
        private final WebSocket real;

        CaptureWebSocket(Session s, WebSocket real) {
            this.s = s;
            this.real = real;
        }

        @Override
        public Request request() {
            return real.request();
        }

        @Override
        public long queueSize() {
            return real.queueSize();
        }

        @Override
        public boolean send(String text) {
            boolean sent = real.send(text);
            if (sent) {
                s.message(true, "text", text.getBytes(UTF_8), true, -1, null);
            }
            return sent;
        }

        @Override
        public boolean send(ByteString bytes) {
            boolean sent = real.send(bytes);
            if (sent) {
                s.message(true, "binary", bytes.toByteArray(), false, -1, null);
            }
            return sent;
        }

        @Override
        public boolean close(int code, String reason) {
            boolean closing = real.close(code, reason);
            if (closing) {
                s.message(true, "close", null, false, code, reason);
            }
            return closing;
        }

        @Override
        public void cancel() {
            real.cancel();
        }
    }

    /** What arrives: every message, and how the socket opens, closes or fails. */
    static final class CaptureListener extends WebSocketListener {
        private final Session s;

        CaptureListener(Session s) {
            this.s = s;
        }

        @Override
        public void onOpen(WebSocket webSocket, Response response) {
            try {
                s.opened(response);
            } catch (Throwable t) {
                s.rt.internalError("okhttp.websocket.open", t);
            }
            s.app.onOpen(s.wrapper(webSocket), response);
        }

        @Override
        public void onMessage(WebSocket webSocket, String text) {
            s.message(false, "text", text.getBytes(UTF_8), true, -1, null);
            s.app.onMessage(s.wrapper(webSocket), text);
        }

        @Override
        public void onMessage(WebSocket webSocket, ByteString bytes) {
            s.message(false, "binary", bytes.toByteArray(), false, -1, null);
            s.app.onMessage(s.wrapper(webSocket), bytes);
        }

        @Override
        public void onClosing(WebSocket webSocket, int code, String reason) {
            s.message(false, "close", null, false, code, reason);
            s.app.onClosing(s.wrapper(webSocket), code, reason);
        }

        @Override
        public void onClosed(WebSocket webSocket, int code, String reason) {
            try {
                s.txn.done();
            } catch (Throwable t) {
                s.rt.internalError("okhttp.websocket.closed", t);
            }
            s.app.onClosed(s.wrapper(webSocket), code, reason);
        }

        @Override
        public void onFailure(WebSocket webSocket, Throwable t, Response response) {
            try {
                s.failed(t, response);
            } catch (Throwable e) {
                s.rt.internalError("okhttp.websocket.failure", e);
            }
            s.app.onFailure(s.wrapper(webSocket), t, response);
        }
    }
}
