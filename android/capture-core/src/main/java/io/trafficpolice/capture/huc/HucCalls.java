package io.trafficpolice.capture.huc;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;

/**
 * The calls both wrappers intercept, in one place. Everything else is plain delegation. The
 * transaction starts before the first delegate call that connects, because request headers can
 * only be read before the connection is made. Rules (PROTOCOL.md §8.4) delay or fail that first
 * call; when they changed the response, the app gets theirs (see {@link HucExchange#ruled()}).
 */
final class HucCalls {
    private HucCalls() {}

    static void connect(HttpURLConnection d, HucExchange x) throws IOException {
        x.start();
        x.gateOrThrow();
        try {
            d.connect();
        } catch (IOException e) {
            x.failed(e);
            throw e;
        }
    }

    static OutputStream getOutputStream(HttpURLConnection d, HucExchange x) throws IOException {
        x.start();
        x.gateOrThrow();
        OutputStream out;
        try {
            out = d.getOutputStream();
        } catch (IOException e) {
            x.failed(e);
            throw e;
        }
        return x.wrapRequestBody(out);
    }

    /** Status and headers are recorded first, so a 4xx/5xx keeps them when this then throws. */
    static InputStream getInputStream(HttpURLConnection d, HucExchange x) throws IOException {
        responseCode(d, x);
        if (x.ruled()) {
            return x.ruledInputStream();
        }
        return x.wrapResponseBody(d.getInputStream());
    }

    static InputStream getErrorStream(HttpURLConnection d, HucExchange x) {
        if (x.ruled()) {
            return x.ruledErrorStream();
        }
        InputStream in = d.getErrorStream();
        if (in != null && !x.responded()) {
            x.responseReady();
            if (x.ruled()) {
                return x.ruledErrorStream();
            }
        }
        return x.wrapResponseBody(in);
    }

    static int responseCode(HttpURLConnection d, HucExchange x) throws IOException {
        x.start();
        x.gateOrThrow();
        x.beforeResponse();
        int code;
        try {
            code = d.getResponseCode();
        } catch (IOException e) {
            x.failed(e);
            throw e;
        }
        x.responseReady();
        return x.ruled() ? x.ruledCode() : code;
    }

    static String responseMessage(HttpURLConnection d, HucExchange x) throws IOException {
        responseCode(d, x);
        return x.ruled() ? x.ruledMessage() : d.getResponseMessage();
    }

    /**
     * Before a response getter that does not throw (it returns null or -1 on failure, and so on
     * a rule's failure too, without touching the delegate).
     */
    static void beforeGetter(HucExchange x) {
        x.start();
        x.gate();
        x.beforeResponse();
    }

    /** After such a getter: record the response now that the delegate has it. */
    static void afterGetter(HucExchange x) {
        x.responseReady();
    }
}
