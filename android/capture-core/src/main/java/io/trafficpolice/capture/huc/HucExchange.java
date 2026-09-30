package io.trafficpolice.capture.huc;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.ConnInfo;
import io.trafficpolice.capture.core.Recorder;
import io.trafficpolice.capture.core.RuleRun;
import io.trafficpolice.capture.core.ThreadStack;
import io.trafficpolice.capture.core.Txn;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.SequenceInputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.security.cert.Certificate;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.LinkedHashMap;
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
    // rules (PROTOCOL.md §8.4): found when the exchange starts, applied around the delegate
    private RuleRun rules;
    private boolean gated;
    private IOException ruleFailure;
    private RuleRun.Delivered delivered;
    private int originalCode;
    private String statusLinePrefix = "HTTP/1.1";
    /** The original body when a rule read it in full (a replace that found nothing keeps it). */
    private byte[] originalBody;
    /** The original body when it was too large to rewrite: what was read, then the rest. */
    private InputStream oversize;

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
            if (rt == null) {
                return;
            }
            rules = findRules();
            if (!rt.recorder().recording()) {
                return; // paused: rules still apply, nothing is recorded
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

    private RuleRun findRules() {
        URL url = delegate.getURL();
        String method = delegate.getRequestMethod();
        if ("GET".equals(method) && delegate.getDoOutput()) {
            method = "POST";
        }
        int port = url.getPort() != -1 ? url.getPort() : url.getDefaultPort();
        String path = url.getPath() == null || url.getPath().isEmpty() ? "/" : url.getPath();
        return RuleRun.find(method, url.getProtocol(), url.getHost(), port, path, RuleRun.parseQuery(url.getQuery()));
    }

    /**
     * The rules' delays, then their failure, once, before the first call that connects. Returns
     * the failure (again on later calls: the connection failed), or null.
     */
    synchronized IOException gate() {
        if (!gated) {
            gated = true;
            if (rules != null) {
                try {
                    rules.beforeRequest(txn, null);
                } catch (IOException e) {
                    ruleFailure = e;
                }
            }
        }
        return ruleFailure;
    }

    /** {@link #gate()} for calls that throw. */
    void gateOrThrow() throws IOException {
        IOException e = gate();
        if (e != null) {
            throw e;
        }
    }

    /** A rule failed the connection before it was made (getters then answer as for a failure). */
    synchronized boolean ruleFailed() {
        return ruleFailure != null;
    }

    /** The app sees the rules' response (or failure), not the delegate's. */
    synchronized boolean ruled() {
        return ruleFailure != null || delivered != null;
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
        if (txn == null || responded || ruleFailure != null) {
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

    /** After the delegate produced the status line: records it, and applies the rules (once). */
    synchronized void responseReady() {
        if (responded || ruleFailure != null || (txn == null && rules == null)) {
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
                    if (value != null && value.startsWith("HTTP/") && value.indexOf(' ') > 0) {
                        statusLinePrefix = value.substring(0, value.indexOf(' '));
                    }
                    continue; // the status line
                }
                headers.add(name);
                headers.add(value);
                if ("X-Android-Selected-Protocol".equalsIgnoreCase(name)) {
                    protocol = value;
                }
            }
            String[] pairs = headers.toArray(new String[0]);
            boolean body = hasBody(code);
            if (txn != null) {
                txn.response(code, message, protocol, pairs, connInfo(protocol));
                txn.mark("resp_headers_end");
            }
            if (rules != null && rules.editsResponse()) {
                applyRules(code, message, pairs, body);
                return;
            }
            if (txn != null && !body) {
                txn.response.end("none");
                txn.done();
            }
        } catch (IOException e) {
            failed(e);
        } catch (Throwable t) {
            internal("response", t);
        }
    }

    /** Status, header and body actions: the app then reads {@link #delivered}. */
    private void applyRules(int code, String message, String[] headers, boolean hasBody) throws IOException {
        originalCode = code;
        byte[] original = null;
        boolean read = false;
        if (rules.editsBody()) {
            if (!hasBody) {
                original = new byte[0];
            } else {
                InputStream in = code >= 400 ? delegate.getErrorStream() : delegate.getInputStream();
                if (in == null) {
                    original = new byte[0];
                } else {
                    byte[] head = readUpTo(in, RuleRun.BODY_LIMIT + 1);
                    if (head.length <= RuleRun.BODY_LIMIT) {
                        original = head;
                        read = true;
                        in.close();
                    } else {
                        // too large to rewrite: body actions are skipped, the original streams on
                        oversize = new SequenceInputStream(new ByteArrayInputStream(head), in);
                    }
                }
            }
        }
        if (txn != null && original != null && read) {
            txn.response.write(original, 0, original.length);
            txn.response.end("complete");
        }
        delivered = rules.editResponse(code, message, headers, original, txn);
        if (delivered == null) {
            // nothing changed after all (a replace that found nothing): the original, as it came
            delivered = RuleRun.Delivered.unchanged(code, message, headers);
        }
        originalBody = read ? original : null;
        if (txn != null && (original != null || !hasBody)) {
            if (!read) {
                txn.response.end("none");
            }
            txn.done();
        }
    }

    /** Up to {@code max} bytes, or fewer at the end of the stream. */
    private static byte[] readUpTo(InputStream in, int max) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream(Math.min(max, 64 * 1024));
        byte[] buf = new byte[16 * 1024];
        int n;
        while (out.size() < max && (n = in.read(buf, 0, Math.min(buf.length, max - out.size()))) != -1) {
            out.write(buf, 0, n);
        }
        return out.toByteArray();
    }

    // --- what the app sees when rules changed the response -----------------------------------

    synchronized int ruledCode() throws IOException {
        if (ruleFailure != null) {
            throw ruleFailure;
        }
        return delivered.code;
    }

    synchronized String ruledMessage() throws IOException {
        if (ruleFailure != null) {
            throw ruleFailure;
        }
        return delivered.message;
    }

    /** {@code getHeaderField(name)}: the last value, as HttpURLConnection answers. */
    synchronized String ruledHeader(String name) {
        if (delivered == null || name == null) {
            return null;
        }
        String found = null;
        for (int i = 0; i + 1 < delivered.headers.length; i += 2) {
            if (delivered.headers[i].equalsIgnoreCase(name)) {
                found = delivered.headers[i + 1];
            }
        }
        return found;
    }

    /** {@code getHeaderFieldKey(n)}: null for the status line (0). */
    synchronized String ruledHeaderKey(int n) {
        if (delivered == null || n <= 0 || 2 * (n - 1) >= delivered.headers.length) {
            return null;
        }
        return delivered.headers[2 * (n - 1)];
    }

    /** {@code getHeaderField(n)}: the status line for 0. */
    synchronized String ruledHeaderAt(int n) {
        if (delivered == null || n < 0) {
            return null;
        }
        if (n == 0) {
            return statusLinePrefix + " " + delivered.code + (delivered.message == null ? "" : " " + delivered.message);
        }
        return 2 * (n - 1) + 1 < delivered.headers.length ? delivered.headers[2 * (n - 1) + 1] : null;
    }

    synchronized Map<String, List<String>> ruledHeaders() {
        if (delivered == null) {
            return Collections.emptyMap();
        }
        Map<String, List<String>> out = new LinkedHashMap<>();
        out.put(null, Collections.singletonList(ruledHeaderAt(0)));
        for (int i = 0; i + 1 < delivered.headers.length; i += 2) {
            List<String> values = out.get(delivered.headers[i]);
            if (values == null) {
                values = new ArrayList<>(1);
                out.put(delivered.headers[i], values);
            }
            values.add(delivered.headers[i + 1]);
        }
        for (Map.Entry<String, List<String>> e : out.entrySet()) {
            e.setValue(Collections.unmodifiableList(e.getValue()));
        }
        return Collections.unmodifiableMap(out);
    }

    /** {@code getInputStream()} by the delivered status: an error status throws, as the platform does. */
    synchronized InputStream ruledInputStream() throws IOException {
        int code = ruledCode();
        if (code >= 400) {
            String url = delegate.getURL().toString();
            if (ON_ANDROID || code == 404 || code == 410) {
                throw new FileNotFoundException(url);
            }
            throw new IOException("Server returned HTTP response code: " + code + " for URL: " + url);
        }
        return ruledBody();
    }

    /** {@code getErrorStream()}: the body for an error status, else null. */
    synchronized InputStream ruledErrorStream() {
        if (ruleFailure != null || delivered == null || delivered.code < 400) {
            return null;
        }
        try {
            return ruledBody();
        } catch (IOException e) {
            return null;
        }
    }

    private InputStream ruledBody() throws IOException {
        if (delivered.body != null) {
            return new ByteArrayInputStream(delivered.body);
        }
        if (originalBody != null) {
            return new ByteArrayInputStream(originalBody);
        }
        if (oversize != null) {
            InputStream o = oversize;
            oversize = null;
            return wrapResponseBody(o);
        }
        // the original body, from where the delegate keeps it for its own status
        InputStream in = originalCode >= 400 ? delegate.getErrorStream() : delegate.getInputStream();
        return in == null ? new ByteArrayInputStream(new byte[0]) : wrapResponseBody(in);
    }

    /** Android answers every error status with FileNotFoundException; the JDK only 404 and 410. */
    private static final boolean ON_ANDROID = "The Android Project".equals(System.getProperty("java.vendor"));

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
