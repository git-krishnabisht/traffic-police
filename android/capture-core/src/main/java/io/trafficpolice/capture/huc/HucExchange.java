package io.trafficpolice.capture.huc;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.ConnInfo;
import io.trafficpolice.capture.core.Recorder;
import io.trafficpolice.capture.core.ThreadStack;
import io.trafficpolice.capture.core.Txn;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.security.cert.Certificate;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import javax.net.ssl.HttpsURLConnection;

/**
 * The capture state of one HttpURLConnection (ARCHITECTURE.md §4.3), shared by the plain and
 * HTTPS wrappers. The transaction starts on the first call that connects; the status and headers
 * are read before the body is handed out, so 4xx/5xx responses keep them even though
 * {@code getInputStream()} then throws; request and response bodies are teed; EOF, close and
 * {@code disconnect()} end it. Never throws anything of its own.
 */
final class HucExchange {
    private final HttpURLConnection delegate;
    private Txn txn;
    private boolean startTried;
    private boolean responded;
    private boolean requestStreamOpened;
    private OutputStream rawOut;
    private TeeOutputStream wrappedOut;
    private InputStream rawIn;
    private TeeInputStream wrappedIn;

    HucExchange(HttpURLConnection delegate) {
        this.delegate = delegate;
    }

    private static void internal(String site, Throwable t) {
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt != null) {
            rt.internalError("huc." + site, t);
        }
    }

    /** Called before the first delegate call that connects. */
    synchronized void start() {
        if (startTried) {
            return;
        }
        startTried = true;
        try {
            CaptureRuntime rt = CaptureRuntime.current();
            if (rt == null || !rt.recorder().recording()) {
                return;
            }
            Recorder rec = rt.recorder();
            Recorder.RequestInfo r = new Recorder.RequestInfo();
            r.call = rec.newCallId();
            r.hop = 0;
            String method = delegate.getRequestMethod();
            // HttpURLConnection only switches GET to POST when it connects with doOutput set
            if ("GET".equals(method) && delegate.getDoOutput()) {
                method = "POST";
            }
            r.method = method;
            r.url = delegate.getURL().toString();
            r.headers = requestHeaders();
            r.clientKind = "huc";
            r.stack = ThreadStack.capture(ThreadStack.ORIGIN_HUC, rec.config().stackDepth);
            r.hasBody = delegate.getDoOutput();
            r.bodyType = r.hasBody ? delegate.getRequestProperty("Content-Type") : null;
            txn = rec.start(r);
        } catch (Throwable t) {
            internal("start", t);
        }
    }

    private String[] requestHeaders() {
        try {
            Map<String, List<String>> props = delegate.getRequestProperties();
            List<String> out = new ArrayList<>();
            for (Map.Entry<String, List<String>> e : props.entrySet()) {
                if (e.getKey() == null) {
                    continue;
                }
                for (String v : e.getValue()) {
                    out.add(e.getKey());
                    out.add(v);
                }
            }
            return out.toArray(new String[0]);
        } catch (Throwable t) {
            return new String[0];
        }
    }

    synchronized OutputStream wrapRequestBody(OutputStream out) {
        if (txn == null || out == null) {
            return out;
        }
        if (out == rawOut && wrappedOut != null) {
            return wrappedOut;
        }
        rawOut = out;
        requestStreamOpened = true;
        wrappedOut = new TeeOutputStream(out, txn);
        return wrappedOut;
    }

    /** Before a call that needs the response: the request is over by then. */
    synchronized void beforeResponse() {
        if (txn == null || responded) {
            return;
        }
        try {
            if (!requestStreamOpened) {
                txn.request.end("none");
            } else if (!txn.request.ended()) {
                txn.request.end("complete");
            }
            txn.mark("resp_headers_start");
        } catch (Throwable t) {
            internal("beforeResponse", t);
        }
    }

    /** After the delegate produced the status line: records it (once). */
    synchronized void responseReady() {
        if (txn == null || responded) {
            return;
        }
        responded = true;
        try {
            int code = delegate.getResponseCode();
            String message = delegate.getResponseMessage();
            List<String> headers = new ArrayList<>();
            String protocol = null;
            for (int i = 0; ; i++) {
                String value = delegate.getHeaderField(i);
                String name = delegate.getHeaderFieldKey(i);
                if (value == null && name == null) {
                    break;
                }
                if (name == null) {
                    continue; // the status line
                }
                headers.add(name);
                headers.add(value);
                if ("X-Android-Selected-Protocol".equalsIgnoreCase(name)) {
                    protocol = value;
                }
            }
            txn.response(code, message, protocol, headers.toArray(new String[0]), connInfo(protocol));
            txn.mark("resp_headers_end");
            if (!hasBody(code)) {
                txn.response.end("none");
                txn.done();
            }
        } catch (IOException e) {
            failed(e);
        } catch (Throwable t) {
            internal("response", t);
        }
    }

    private boolean hasBody(int code) {
        if ("HEAD".equals(delegate.getRequestMethod())) {
            return false;
        }
        if (code == 204 || code == 304 || (code >= 100 && code < 200)) {
            return false;
        }
        String length = delegate.getHeaderField("Content-Length");
        return length == null || !length.trim().equals("0");
    }

    private ConnInfo connInfo(String protocol) {
        if (!(delegate instanceof HttpsURLConnection)) {
            return null;
        }
        HttpsURLConnection https = (HttpsURLConnection) delegate;
        String cipher = null;
        List<ConnInfo.Cert> peer = null;
        try {
            cipher = https.getCipherSuite();
        } catch (Throwable ignored) {
            // not connected over TLS after all
        }
        try {
            Certificate[] chain = https.getServerCertificates();
            peer = ConnInfo.summarize(chain == null ? null : Arrays.asList(chain));
        } catch (Throwable ignored) {
            // peer not verified
        }
        if (cipher == null && (peer == null || peer.isEmpty())) {
            return null;
        }
        return new ConnInfo(null, false, protocol, null, 0, null, null, cipher, peer);
    }

    synchronized InputStream wrapResponseBody(InputStream in) {
        if (txn == null || in == null) {
            return in;
        }
        if (in == rawIn && wrappedIn != null) {
            return wrappedIn;
        }
        if (txn.response.ended()) {
            return in; // no body expected, or already consumed through another stream
        }
        rawIn = in;
        wrappedIn = new TeeInputStream(in, txn);
        return wrappedIn;
    }

    boolean responded() {
        return responded;
    }

    /** A delegate call failed before or while producing the response. */
    synchronized void failed(IOException e) {
        if (txn == null || txn.finished()) {
            return;
        }
        try {
            String phase = responded ? "response_body" : (requestStreamOpened ? "request" : "connect");
            if (!txn.request.ended()) {
                txn.request.end("error");
            }
            txn.fail(phase, false, e, null);
        } catch (Throwable t) {
            internal("failed", t);
        }
    }

    /** {@code disconnect()} always ends the transaction. */
    synchronized void disconnected() {
        if (txn == null || txn.finished()) {
            return;
        }
        try {
            if (!txn.request.ended()) {
                txn.request.end(requestStreamOpened ? "complete" : "none");
            }
            if (!responded) {
                txn.fail("unknown", true, new IOException("disconnected before a response"), null);
                return;
            }
            txn.response.end("closed_early");
            txn.done();
        } catch (Throwable t) {
            internal("disconnect", t);
        }
    }
}
