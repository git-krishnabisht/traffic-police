package io.trafficpolice.capture.huc;

import io.trafficpolice.capture.core.CaptureRuntime;
import java.net.HttpURLConnection;
import java.net.URLConnection;
import javax.net.ssl.HttpsURLConnection;

/** Wraps HTTP(S) connections for capture; anything else, or capture not running, passes through. */
public final class Huc {
    private Huc() {}

    public static URLConnection wrap(URLConnection c) {
        if (c == null || CaptureRuntime.current() == null) {
            return c;
        }
        if (c instanceof TrackedHttpURLConnection || c instanceof TrackedHttpsURLConnection) {
            return c;
        }
        if (c instanceof HttpsURLConnection) {
            return new TrackedHttpsURLConnection((HttpsURLConnection) c);
        }
        if (c instanceof HttpURLConnection) {
            return new TrackedHttpURLConnection((HttpURLConnection) c);
        }
        return c; // file:, jar:, …
    }

    public static HttpURLConnection wrap(HttpURLConnection c) {
        return (HttpURLConnection) wrap((URLConnection) c);
    }
}
