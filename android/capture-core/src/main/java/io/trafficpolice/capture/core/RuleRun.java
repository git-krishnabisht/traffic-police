package io.trafficpolice.capture.core;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.InterruptedIOException;
import java.nio.charset.Charset;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.regex.Matcher;
import java.util.zip.GZIPInputStream;
import java.util.zip.Inflater;
import java.util.zip.InflaterInputStream;

/**
 * The host's rules for one request (ARCHITECTURE.md §4.4, PROTOCOL.md §8.4), found before it
 * goes out: delays and failures apply before it is sent, the other actions to its response.
 * Client-neutral; the OkHttp interceptor and the HttpURLConnection wrapper drive it. Every
 * application is reported as a {@code rule} event when the request is being recorded.
 */
public final class RuleRun {
    /** Bodies larger than this are not rewritten; the original then streams through. */
    public static final int BODY_LIMIT = 32 * 1024 * 1024;
    private static final Charset UTF_8 = Charset.forName("UTF-8");
    private static final long SLEEP_SLICE_MS = 50;

    /** Asked while a delay runs, so a canceled call does not wait it out. */
    public interface Cancel {
        boolean canceled();
    }

    /** What the app receives instead of the original response. */
    public static final class Delivered {
        public final int code;
        public final String message;
        /** Name/value pairs, in order. */
        public final String[] headers;
        /** The new body (not Content-Encoded), or null when the original body is kept. */
        public final byte[] body;
        /**
         * The header edits that turn the original headers into {@link #headers}, in order: each
         * {@code {"set"|"add"|"remove", name, value}}; "set" replaces every value of the name.
         */
        public final List<String[]> headerOps;

        Delivered(int code, String message, String[] headers, byte[] body, List<String[]> headerOps) {
            this.code = code;
            this.message = message;
            this.headers = headers;
            this.body = body;
            this.headerOps = headerOps;
        }
    }

    /** One change, as the {@code rule} event reports it (PROTOCOL.md §7). */
    static final class Change {
        final String op;
        Integer from;
        Integer to;
        String reason;
        String name;
        String value;
        List<String> old;
        Long bytes;
        Long matches;
        Long ms;
        String exception;

        Change(String op) {
            this.op = op;
        }

        void write(Json j) {
            j.obj().kv("op", op);
            if (from != null) {
                j.kv("from", from);
            }
            if (to != null) {
                j.kv("to", to);
            }
            if (reason != null) {
                j.kv("reason", reason);
            }
            if (name != null) {
                j.kv("name", name);
            }
            if (value != null) {
                j.kv("value", value);
            }
            if (old != null && !old.isEmpty()) {
                j.key("old").arr();
                for (String o : old) {
                    j.str(o);
                }
                j.endArr();
            }
            if (bytes != null) {
                j.kv("bytes", bytes);
            }
            if (matches != null) {
                j.kv("matches", matches);
            }
            if (ms != null) {
                j.kv("ms", ms);
            }
            if (exception != null) {
                j.kv("exception", exception);
            }
            j.endObj();
        }
    }

    private final CaptureRuntime rt;
    private final List<RuleSet.Rule> rules;

    private RuleRun(CaptureRuntime rt, List<RuleSet.Rule> rules) {
        this.rt = rt;
        this.rules = rules;
    }

    /**
     * The rules matching a request, or null when none do (the common case, and always when no
     * host is connected). {@code port} is the effective port (80 or 443 filled in); the path is
     * in its encoded form and the query parameters decoded.
     */
    public static RuleRun find(String method, String scheme, String host, int port, String encodedPath,
            Map<String, List<String>> query) {
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt == null) {
            return null;
        }
        RuleSet set = rt.rules();
        if (set.rules.isEmpty()) {
            return null;
        }
        try {
            List<RuleSet.Rule> m = set.matching(method, scheme, host, port, encodedPath, query);
            return m.isEmpty() ? null : new RuleRun(rt, m);
        } catch (Throwable t) {
            rt.internalError("rules.match", t);
            return null;
        }
    }

    /** Decoded query parameters of a raw query string ({@code a=1&b=x%20y}), for {@link #find}. */
    public static Map<String, List<String>> parseQuery(String rawQuery) {
        Map<String, List<String>> out = new LinkedHashMap<>();
        if (rawQuery == null || rawQuery.isEmpty()) {
            return out;
        }
        for (String part : rawQuery.split("&")) {
            if (part.isEmpty()) {
                continue;
            }
            int eq = part.indexOf('=');
            String name = decode(eq < 0 ? part : part.substring(0, eq));
            String value = eq < 0 ? "" : decode(part.substring(eq + 1));
            List<String> values = out.get(name);
            if (values == null) {
                values = new ArrayList<>(1);
                out.put(name, values);
            }
            values.add(value);
        }
        return out;
    }

    /** Percent-decoding with {@code +} as a space (as OkHttp decodes query parameters). */
    private static String decode(String s) {
        ByteArrayOutputStream out = new ByteArrayOutputStream(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '+') {
                out.write(' ');
            } else if (c == '%' && i + 2 < s.length() && hex(s.charAt(i + 1)) >= 0 && hex(s.charAt(i + 2)) >= 0) {
                out.write(hex(s.charAt(i + 1)) * 16 + hex(s.charAt(i + 2)));
                i += 2;
            } else {
                byte[] b = String.valueOf(c).getBytes(UTF_8);
                out.write(b, 0, b.length);
            }
        }
        return new String(out.toByteArray(), UTF_8);
    }

    private static int hex(char c) {
        if (c >= '0' && c <= '9') {
            return c - '0';
        }
        if (c >= 'a' && c <= 'f') {
            return c - 'a' + 10;
        }
        if (c >= 'A' && c <= 'F') {
            return c - 'A' + 10;
        }
        return -1;
    }

    /** Whether status, header, body or replace actions apply. */
    public boolean editsResponse() {
        for (RuleSet.Rule r : rules) {
            for (RuleSet.Action a : r.actions) {
                if (a.editsResponse()) {
                    return true;
                }
            }
        }
        return false;
    }

    /** Whether a body or replace action applies: the caller then reads the original body first. */
    public boolean editsBody() {
        for (RuleSet.Rule r : rules) {
            for (RuleSet.Action a : r.actions) {
                if (a.editsBody()) {
                    return true;
                }
            }
        }
        return false;
    }

    /**
     * Sleeps for the delay actions, in order, then throws the first fail action's exception.
     * Both are reported on {@code txn} (null while paused); a failure also fails it, as simulated.
     */
    public void beforeRequest(Txn txn, Cancel cancel) throws IOException {
        List<Change> changes = new ArrayList<>();
        Set<RuleSet.Rule> acted = new LinkedHashSet<>();
        IOException failure = null;
        outer:
        for (RuleSet.Rule r : rules) {
            for (RuleSet.Action a : r.actions) {
                if ("delay".equals(a.type)) {
                    Change c = new Change("delay");
                    c.ms = a.ms;
                    changes.add(c);
                    acted.add(r);
                    try {
                        sleep(a.ms, cancel);
                    } catch (IOException e) {
                        report(txn, acted, changes, null);
                        throw e;
                    }
                } else if ("fail".equals(a.type)) {
                    failure = Failures.create(a.exception, a.message);
                    Change c = new Change("fail");
                    c.exception = failure.getClass().getName();
                    changes.add(c);
                    acted.add(r);
                    break outer;
                }
            }
        }
        report(txn, acted, changes, null);
        if (failure != null) {
            if (txn != null) {
                try {
                    txn.request.end("error");
                    txn.failSimulated("connect", failure);
                } catch (Throwable t) {
                    rt.internalError("rules.fail", t);
                }
            }
            throw failure;
        }
    }

    private static void sleep(long ms, Cancel cancel) throws IOException {
        long end = System.nanoTime() + ms * 1_000_000L;
        while (true) {
            if (cancel != null && cancel.canceled()) {
                throw new IOException("Canceled");
            }
            long left = (end - System.nanoTime()) / 1_000_000L;
            if (left <= 0) {
                return;
            }
            try {
                Thread.sleep(Math.min(left, SLEEP_SLICE_MS));
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new InterruptedIOException("interrupted during a traffic-police rule's delay");
            }
        }
    }

    /**
     * Applies the response actions to the response the network gave. {@code original} is its
     * whole body, as received (still Content-Encoded), when {@link #editsBody()}, or null when it
     * was larger than {@link #BODY_LIMIT} (body actions are then skipped). Records the rule event
     * (and a new body as {@code dir = 2}) on {@code txn}. Returns null when nothing changed.
     */
    public Delivered editResponse(int code, String message, String[] headers, byte[] original, Txn txn) {
        List<Change> changes = new ArrayList<>();
        Set<RuleSet.Rule> acted = new LinkedHashSet<>();
        List<String> h = new ArrayList<>(Arrays.asList(headers == null ? new String[0] : headers));
        int status = code;
        String msg = message;
        boolean statusChanged = false;
        boolean headersChanged = false;
        byte[] body = null;
        boolean bodyTooLarge = original == null && editsBody();
        for (RuleSet.Rule r : rules) {
            for (RuleSet.Action a : r.actions) {
                switch (a.type) {
                    case "status": {
                        Change c = new Change("status");
                        c.from = status;
                        c.to = a.code;
                        status = a.code;
                        msg = a.reason != null ? a.reason : reasonPhrase(a.code);
                        c.reason = msg;
                        changes.add(c);
                        acted.add(r);
                        statusChanged = true;
                        break;
                    }
                    case "header": {
                        Change c = header(h, a.op, a.name, a.value);
                        if (c != null) {
                            changes.add(c);
                            headersChanged = true;
                        }
                        acted.add(r);
                        break;
                    }
                    case "body": {
                        if (bodyTooLarge) {
                            skipped(r, "the response body is over " + (BODY_LIMIT >> 20) + " MiB");
                            break;
                        }
                        body = a.body;
                        Change c = new Change("body_replace");
                        c.bytes = (long) body.length;
                        changes.add(c);
                        acted.add(r);
                        if (a.contentType != null) {
                            Change t = header(h, "set", "Content-Type", a.contentType);
                            if (t != null) {
                                changes.add(t);
                            }
                        }
                        break;
                    }
                    case "replace": {
                        if (bodyTooLarge) {
                            skipped(r, "the response body is over " + (BODY_LIMIT >> 20) + " MiB");
                            break;
                        }
                        byte[] current = body;
                        if (current == null) {
                            String encoding = first(h, "Content-Encoding");
                            try {
                                current = decode(original, encoding);
                            } catch (IOException e) {
                                skipped(r, "the body could not be decoded (" + encoding + "): " + e.getMessage());
                                break;
                            }
                            if (current == null) {
                                skipped(r, "Content-Encoding " + encoding + " cannot be decoded on the device");
                                break;
                            }
                        }
                        Charset cs = charset(first(h, "Content-Type"));
                        String text = new String(current, cs);
                        long n = 0;
                        String edited;
                        if (a.regex != null) {
                            Matcher m = a.regex.matcher(text);
                            StringBuffer sb = new StringBuffer(text.length());
                            while (m.find()) {
                                n++;
                                m.appendReplacement(sb, a.with);
                            }
                            m.appendTail(sb);
                            edited = sb.toString();
                        } else {
                            StringBuilder sb = new StringBuilder(text.length());
                            int from = 0;
                            int at;
                            while ((at = text.indexOf(a.find, from)) >= 0) {
                                sb.append(text, from, at).append(a.with);
                                from = at + a.find.length();
                                n++;
                            }
                            sb.append(text, from, text.length());
                            edited = sb.toString();
                        }
                        Change c = new Change("body_edit");
                        c.matches = n;
                        changes.add(c);
                        acted.add(r);
                        if (n > 0) {
                            body = edited.getBytes(cs);
                        }
                        break;
                    }
                    default:
                        // delay and fail ran before the request
                        break;
                }
            }
        }
        if (body != null) {
            // delivered as is: no Content-Encoding, and the length it has now (§8.4 rule 4)
            Change enc = header(h, "remove", "Content-Encoding", null);
            if (enc != null) {
                enc.reason = "body_changed";
                changes.add(enc);
            }
            Change len = header(h, "set", "Content-Length", String.valueOf(body.length));
            if (len != null) {
                len.reason = "body_changed";
                changes.add(len);
            }
        }
        boolean changed = statusChanged || headersChanged || body != null;
        if (changed && guardCache(acted)) {
            Change c = header(h, "set", "Cache-Control", "no-store");
            if (c != null) {
                c.reason = "cache_guard";
                changes.add(c);
            }
        }
        List<String[]> ops = new ArrayList<>();
        for (Change c : changes) {
            if (c.op.startsWith("header_")) {
                ops.add(new String[] {c.op.substring(7), c.name, c.value});
            }
        }
        Delivered d = changed ? new Delivered(status, msg, h.toArray(new String[0]), body, ops) : null;
        report(txn, acted, changes, d);
        if (txn != null && d != null && d.body != null) {
            try {
                Txn.Body delivered = txn.delivered();
                delivered.write(d.body, 0, d.body.length);
                delivered.end("complete");
            } catch (Throwable t) {
                rt.internalError("rules.delivered", t);
            }
        }
        return d;
    }

    /** Unless every rule that changed something opted out with {@code cache_rewrites}. */
    private static boolean guardCache(Set<RuleSet.Rule> acted) {
        for (RuleSet.Rule r : acted) {
            if (!r.cacheRewrites) {
                return true;
            }
        }
        return false;
    }

    private void skipped(RuleSet.Rule r, String why) {
        Map<String, String> data = new LinkedHashMap<>();
        data.put("rule", r.id);
        rt.diagOnceKeyed("rule_skipped:" + r.id + ":" + why, "warn", "rule_skipped",
                "rule " + r.id + " left the body alone: " + why, data);
    }

    private void report(Txn txn, Set<RuleSet.Rule> acted, List<Change> changes, Delivered d) {
        if (txn == null || (changes.isEmpty() && d == null)) {
            return;
        }
        try {
            txn.rec.emit(new Event.Rule(txn.rec.now(), txn.id, new ArrayList<>(acted), changes, d));
        } catch (Throwable t) {
            rt.internalError("rules.report", t);
        }
    }

    /** A header edit on name/value pairs; null when it changed nothing (a remove of nothing). */
    private static Change header(List<String> h, String op, String name, String value) {
        List<String> old = new ArrayList<>();
        if (!"add".equals(op)) {
            for (int i = 0; i + 1 < h.size(); ) {
                if (h.get(i).equalsIgnoreCase(name)) {
                    old.add(h.get(i + 1));
                    h.remove(i);
                    h.remove(i);
                } else {
                    i += 2;
                }
            }
        }
        if ("remove".equals(op) && old.isEmpty()) {
            return null;
        }
        if (!"remove".equals(op)) {
            h.add(name);
            h.add(value);
        }
        Change c = new Change("header_" + op);
        c.name = name;
        c.value = value;
        c.old = old;
        return c;
    }

    private static String first(List<String> h, String name) {
        for (int i = 0; i + 1 < h.size(); i += 2) {
            if (h.get(i).equalsIgnoreCase(name)) {
                return h.get(i + 1);
            }
        }
        return null;
    }

    /** The charset of a Content-Type, UTF-8 when absent or unknown. */
    static Charset charset(String contentType) {
        if (contentType != null) {
            for (String part : contentType.split(";")) {
                String p = part.trim();
                if (p.regionMatches(true, 0, "charset=", 0, 8)) {
                    String name = p.substring(8).trim().replace("\"", "");
                    try {
                        return Charset.forName(name);
                    } catch (Throwable ignored) {
                        // unknown: UTF-8
                    }
                }
            }
        }
        return UTF_8;
    }

    /**
     * Undoes a Content-Encoding (gzip, deflate, or a list of them); null when one of them cannot
     * be decoded here (br, zstd).
     */
    static byte[] decode(byte[] raw, String contentEncoding) throws IOException {
        if (contentEncoding == null || contentEncoding.trim().isEmpty()) {
            return raw;
        }
        String[] codings = contentEncoding.split(",");
        byte[] bytes = raw;
        // applied in order, so undone in reverse
        for (int i = codings.length - 1; i >= 0; i--) {
            String c = codings[i].trim().toLowerCase(Locale.ROOT);
            if (c.isEmpty() || "identity".equals(c)) {
                continue;
            }
            InputStream in;
            if ("gzip".equals(c) || "x-gzip".equals(c)) {
                in = new GZIPInputStream(new ByteArrayInputStream(bytes));
            } else if ("deflate".equals(c)) {
                // zlib-wrapped as HTTP says, or raw deflate as some servers send
                boolean zlib = bytes.length >= 2 && (bytes[0] & 0x0f) == 8 && ((bytes[0] & 0xff) * 256 + (bytes[1] & 0xff)) % 31 == 0;
                in = new InflaterInputStream(new ByteArrayInputStream(bytes), new Inflater(!zlib));
            } else {
                return null;
            }
            try {
                bytes = readAll(in, BODY_LIMIT);
            } finally {
                in.close();
            }
            if (bytes == null) {
                throw new IOException("decoded body over " + (BODY_LIMIT >> 20) + " MiB");
            }
        }
        return bytes;
    }

    /** Reads to the end, or returns null past {@code limit} bytes. */
    static byte[] readAll(InputStream in, int limit) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        byte[] buf = new byte[16 * 1024];
        int n;
        while ((n = in.read(buf)) != -1) {
            if (out.size() + n > limit) {
                return null;
            }
            out.write(buf, 0, n);
        }
        return out.toByteArray();
    }

    /** The usual reason phrase for a status code ("" for others). */
    static String reasonPhrase(int code) {
        switch (code) {
            case 200: return "OK";
            case 201: return "Created";
            case 202: return "Accepted";
            case 204: return "No Content";
            case 301: return "Moved Permanently";
            case 302: return "Found";
            case 304: return "Not Modified";
            case 307: return "Temporary Redirect";
            case 308: return "Permanent Redirect";
            case 400: return "Bad Request";
            case 401: return "Unauthorized";
            case 403: return "Forbidden";
            case 404: return "Not Found";
            case 408: return "Request Timeout";
            case 409: return "Conflict";
            case 410: return "Gone";
            case 413: return "Content Too Large";
            case 415: return "Unsupported Media Type";
            case 422: return "Unprocessable Content";
            case 429: return "Too Many Requests";
            case 500: return "Internal Server Error";
            case 501: return "Not Implemented";
            case 502: return "Bad Gateway";
            case 503: return "Service Unavailable";
            case 504: return "Gateway Timeout";
            default: return "";
        }
    }
}
