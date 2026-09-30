package io.trafficpolice.capture.okhttp;

import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * Detects OkHttp without referring to any OkHttp type, so it is safe to load in apps that do not
 * have OkHttp at all.
 */
public final class OkHttpDetect {
    private OkHttpDetect() {}

    /** Whether OkHttp 3.x or later is on the classpath. */
    public static boolean present() {
        try {
            Class.forName("okhttp3.OkHttpClient", false, OkHttpDetect.class.getClassLoader());
            return true;
        } catch (Throwable t) {
            return false;
        }
    }

    /** 3.9.0 and later: {@code Interceptor.Chain.call()} and a public {@code EventListener}. */
    public static boolean supported() {
        try {
            Class<?> chain = Class.forName("okhttp3.Interceptor$Chain", false, OkHttpDetect.class.getClassLoader());
            chain.getMethod("call");
            return true;
        } catch (Throwable t) {
            return false;
        }
    }

    /** The version string, read reflectively (never a compile-time constant). */
    public static String version() {
        ClassLoader cl = OkHttpDetect.class.getClassLoader();
        try { // 4.7+
            Field f = Class.forName("okhttp3.OkHttp", true, cl).getField("VERSION");
            return String.valueOf(f.get(null));
        } catch (Throwable ignored) {
            // older
        }
        try { // 4.0-4.6: const val userAgent = "okhttp/x"
            Field f = Class.forName("okhttp3.internal.Version", true, cl).getField("userAgent");
            return stripAgent(String.valueOf(f.get(null)));
        } catch (Throwable ignored) {
            // older
        }
        try { // 3.x
            Method m = Class.forName("okhttp3.internal.Version", true, cl).getMethod("userAgent");
            return stripAgent(String.valueOf(m.invoke(null)));
        } catch (Throwable ignored) {
            return null;
        }
    }

    private static String stripAgent(String ua) {
        return ua.startsWith("okhttp/") ? ua.substring("okhttp/".length()) : ua;
    }

}
