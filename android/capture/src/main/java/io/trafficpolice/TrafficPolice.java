package io.trafficpolice;

import android.content.Context;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.huc.Huc;
import io.trafficpolice.capture.okhttp.CaptureInterceptor;
import io.trafficpolice.capture.okhttp.ListenerFactory;
import io.trafficpolice.capture.okhttp.WebSockets;
import io.trafficpolice.internal.AndroidRuntime;
import java.net.HttpURLConnection;
import java.net.URLConnection;
import okhttp3.EventListener;
import okhttp3.Interceptor;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.WebSocket;
import okhttp3.WebSocketListener;

/**
 * traffic-police library mode. Capture starts by itself in the app's main process when the app
 * is debuggable; the {@code traffic-police} tool on your computer shows the traffic over adb.
 *
 * <pre>{@code
 * OkHttpClient client = new OkHttpClient.Builder()
 *     .addNetworkInterceptor(TrafficPolice.networkInterceptor())
 *     .eventListenerFactory(TrafficPolice.eventListenerFactory(existingFactoryOrNull))
 *     .build();
 *
 * HttpURLConnection conn = TrafficPolice.wrap((HttpURLConnection) url.openConnection());
 *
 * WebSocket socket = TrafficPolice.newWebSocket(client, request, listener);
 * }</pre>
 *
 * The {@code capture-noop} artifact has this exact API and does nothing; use it for release
 * builds.
 */
public final class TrafficPolice {
    private TrafficPolice() {}

    /** Add with {@code addNetworkInterceptor}: records every network exchange of the client. */
    public static Interceptor networkInterceptor() {
        return CaptureInterceptor.INSTANCE;
    }

    /** Records the real call site (thread and stack) and timings of each call. */
    public static EventListener.Factory eventListenerFactory() {
        return new ListenerFactory(null);
    }

    /**
     * Like {@link #eventListenerFactory()}, keeping the app's own listener factory: it still
     * receives every callback, first.
     */
    public static EventListener.Factory eventListenerFactory(EventListener.Factory existing) {
        return new ListenerFactory(existing);
    }

    /**
     * {@code client.newWebSocket(request, listener)}, with the handshake and every message sent
     * and received captured. (OkHttp sends WebSocket handshakes past network interceptors and
     * event listeners, so the interceptor alone never sees them.)
     */
    public static WebSocket newWebSocket(OkHttpClient client, Request request, WebSocketListener listener) {
        return WebSockets.newWebSocket(client, request, listener);
    }

    /** Records this connection's exchange; returns it unchanged when capture is not running. */
    public static HttpURLConnection wrap(HttpURLConnection connection) {
        return Huc.wrap(connection);
    }

    /** As {@link #wrap(HttpURLConnection)}; other connection types are returned unchanged. */
    public static URLConnection wrap(URLConnection connection) {
        return Huc.wrap(connection);
    }

    /**
     * Starts capture in this process. Only needed in secondary processes ({@code :remote},
     * {@code :sync}): the main process starts by itself. Does nothing when the app is not
     * debuggable or capture is already running.
     */
    public static void start(Context context) {
        AndroidRuntime.start(context);
    }

    /** Whether capture is running in this process. */
    public static boolean isActive() {
        return CaptureRuntime.current() != null;
    }
}
