package io.trafficpolice.capture.okhttp;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import okhttp3.EventListener;
import okhttp3.Interceptor;
import okhttp3.WebSocket;

/**
 * What attach mode's hooks return (ARCHITECTURE.md §4.7.3). OkHttp reads
 * {@code OkHttpClient.networkInterceptors()} for every call and {@code eventListenerFactory()} for
 * every new call, so clients made before the attach are captured too. On OkHttp 4 and 5
 * {@code newBuilder()} reads the same getters, so a derived client already carries what a hook
 * added: both hooks leave a value alone that has ours already.
 */
public final class OkHttpHooks {
    private OkHttpHooks() {}

    /** The client's network interceptors with ours first, unless one is there already. */
    public static List<Interceptor> networkInterceptors(List<Interceptor> original) {
        if (original == null) {
            return Collections.<Interceptor>singletonList(CaptureInterceptor.INSTANCE);
        }
        for (Interceptor i : original) {
            // by name: the library's own interceptor, in an app that has both, counts too
            if (i != null && i.getClass().getName().equals(CaptureInterceptor.class.getName())) {
                return original;
            }
        }
        List<Interceptor> out = new ArrayList<>(original.size() + 1);
        out.add(CaptureInterceptor.INSTANCE);
        out.addAll(original);
        return Collections.unmodifiableList(out);
    }

    /** What {@code OkHttpClient.newWebSocket} returned, wrapped so its messages are captured. */
    public static WebSocket newWebSocket(WebSocket original) {
        return WebSockets.hooked(original);
    }

    /** The client's listener factory wrapped by ours, unless it is ours already. */
    public static EventListener.Factory eventListenerFactory(EventListener.Factory original) {
        if (original != null && original.getClass().getName().equals(ListenerFactory.class.getName())) {
            return original;
        }
        return new ListenerFactory(original);
    }
}
