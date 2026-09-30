package io.trafficpolice.testapp;

import java.io.IOException;
import java.net.HttpURLConnection;

/** Stands in for app code: its frames must appear in captured call stacks. */
public final class AppCode {
    private AppCode() {}

    public static int fetchStatus(HttpURLConnection c) throws IOException {
        return c.getResponseCode();
    }
}
