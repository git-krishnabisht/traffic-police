package io.trafficpolice;

import android.content.Context;
import java.io.IOException;
import java.net.HttpURLConnection;
import java.net.URLConnection;
import okhttp3.Call;
import okhttp3.EventListener;
import okhttp3.Interceptor;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.WebSocket;
import okhttp3.WebSocketListener;

/**
 * The release-build stand-in for traffic-police's library mode: the same API as the
 * {@code capture} artifact, and nothing else. It never captures and opens no socket.
 */
public final class TrafficPolice {
    private TrafficPolice() {}

    public static Interceptor networkInterceptor() {
        return PassThrough.INSTANCE;
    }

    public static EventListener.Factory eventListenerFactory() {
        return NoListener.INSTANCE;
    }

    public static EventListener.Factory eventListenerFactory(EventListener.Factory existing) {
        return existing != null ? existing : NoListener.INSTANCE;
    }

    public static WebSocket newWebSocket(OkHttpClient client, Request request, WebSocketListener listener) {
        return client.newWebSocket(request, listener);
    }

    public static HttpURLConnection wrap(HttpURLConnection connection) {
        return connection;
    }

    public static URLConnection wrap(URLConnection connection) {
        return connection;
    }

    public static void start(Context context) {
        // nothing to start
    }

    public static boolean isActive() {
        return false;
    }

    private static final class PassThrough implements Interceptor {
        static final PassThrough INSTANCE = new PassThrough();

        @Override
        public Response intercept(Chain chain) throws IOException {
            return chain.proceed(chain.request());
        }
    }

    private static final class NoListener implements EventListener.Factory {
        static final NoListener INSTANCE = new NoListener();

        @Override
        public EventListener create(Call call) {
            return EventListener.NONE;
        }
    }
}
