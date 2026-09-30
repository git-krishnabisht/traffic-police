package io.trafficpolice.capture.okhttp;

import java.lang.reflect.Method;
import okhttp3.EventListener;
import okhttp3.Headers;
import okhttp3.RequestBody;

/**
 * Feature detection for the OkHttp in the process (docs/research/05-okhttp-okio.md §10). Only
 * members present in 3.9.0 are called directly; newer ones behind the checks below.
 */
public final class OkHttpCompat {
    private OkHttpCompat() {}

    private static volatile Method plus;
    private static volatile boolean plusChecked;

    /** {@code app.plus(ours)} on 5.3+, else our forwarder; either way both see every callback. */
    static EventListener compose(EventListener app, EventListener ours) {
        if (!plusChecked) {
            try {
                plus = EventListener.class.getMethod("plus", EventListener.class);
            } catch (Throwable t) {
                plus = null;
            }
            plusChecked = true;
        }
        Method p = plus;
        if (p != null) {
            try {
                return (EventListener) p.invoke(app, ours);
            } catch (Throwable ignored) {
                // fall through to the forwarder
            }
        }
        return new ForwardingEventListener(app, ours);
    }

    private static volatile int bodyFlags = -1; // bit 0: isOneShot/isDuplex exist (3.14+)

    private static boolean bodyFlagsExist() {
        if (bodyFlags < 0) {
            int f = 0;
            try {
                RequestBody.class.getMethod("isOneShot");
                f = 1;
            } catch (Throwable ignored) {
                // 3.9-3.13
            }
            bodyFlags = f;
        }
        return bodyFlags == 1;
    }

    static boolean isOneShot(RequestBody body) {
        try {
            return bodyFlagsExist() && body.isOneShot();
        } catch (Throwable t) {
            return false;
        }
    }

    static boolean isDuplex(RequestBody body) {
        try {
            return bodyFlagsExist() && body.isDuplex();
        } catch (Throwable t) {
            return false;
        }
    }

    /** Ordered name/value pairs with duplicates; never iterate (4.x/5.x yield kotlin.Pair). */
    static String[] headers(Headers h) {
        int n = h.size();
        String[] out = new String[n * 2];
        for (int i = 0; i < n; i++) {
            out[i * 2] = h.name(i);
            out[i * 2 + 1] = h.value(i);
        }
        return out;
    }
}
