package io.trafficpolice.capture.okhttp;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.net.ConnectException;
import java.net.InetAddress;
import java.net.ProtocolException;
import java.net.SocketTimeoutException;
import java.net.UnknownHostException;
import java.nio.charset.Charset;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import java.util.zip.Deflater;
import java.util.zip.DeflaterOutputStream;
import java.util.zip.GZIPOutputStream;
import okhttp3.Call;
import okhttp3.Callback;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.RequestBody;
import okhttp3.Response;
import okhttp3.mockwebserver.MockResponse;
import okhttp3.mockwebserver.MockWebServer;
import okio.Buffer;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/**
 * Rules end to end (PROTOCOL.md §8): a real client and MockWebServer, the runtime with a host that
 * pushes rules, and what the app receives and the host is told. Runs against every OkHttp in the
 * build's matrix, so every action is checked on 3.9 to 5.5.
 */
public final class OkHttpRulesTest {
    private static final Charset UTF_8 = Charset.forName("UTF-8");

    private final TestPlatform platform = new TestPlatform(false);
    private final AtomicLong ids = new AtomicLong(100);
    private CaptureRuntime rt;
    private TestHost host;
    private MockWebServer server;
    private OkHttpClient client;

    @Before
    public void setUp() throws Exception {
        rt = TestPlatform.runtime(platform, true);
        host = TestHost.connect(rt);
        server = new MockWebServer();
        // one address: a simulated failure marks the route as failed in OkHttp 3.x, and "localhost"
        // would leave only the IPv6 route, where nothing listens
        server.start(java.net.InetAddress.getByName("127.0.0.1"), 0);
        client = new OkHttpClient.Builder()
                .addNetworkInterceptor(CaptureInterceptor.INSTANCE)
                .eventListenerFactory(new ListenerFactory(null))
                .readTimeout(10, TimeUnit.SECONDS)
                .retryOnConnectionFailure(false)
                .build();
    }

    @After
    public void tearDown() throws Exception {
        host.close();
        server.shutdown();
        rt.stop();
    }

    /** Pushes rules (a JSON array of wire-form rules) and returns the device's rules_ack. */
    private TestHost.Msg rules(String array) throws Exception {
        final long id = ids.incrementAndGet();
        host.send("{\"t\":\"set_rules\",\"id\":" + id + ",\"rules\":{\"version\":\"v" + id + "\",\"rules\":" + array
                + "}}");
        TestHost.Msg ack = host.await(m -> "rules_ack".equals(m.t()) && m.num("id") == id, 5_000);
        assertNotNull("rules_ack", ack);
        return ack;
    }

    private static String rule(String id, String match, String actions) {
        return "{\"id\":\"" + id + "\",\"name\":\"" + id + " rule\",\"match\":" + match + ",\"actions\":" + actions + "}";
    }

    private String url(String path) {
        return "http://127.0.0.1:" + server.getPort() + path;
    }

    private Response get(String path) throws IOException {
        return client.newCall(new Request.Builder().url(url(path)).build()).execute();
    }

    private TestHost.Msg reqFor(String path) throws InterruptedException {
        return host.await(m -> "req".equals(m.t()) && m.str("url").endsWith(path), 10_000);
    }

    private TestHost.Msg awaitEnd(long txn) throws InterruptedException {
        return host.await(m -> ("done".equals(m.t()) || "fail".equals(m.t())) && m.txn == txn, 10_000);
    }

    private static byte[] bodyOf(List<TestHost.Msg> msgs, int dir) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (TestHost.Msg m : msgs) {
            if (m.data != null && m.dir == dir) {
                out.write(m.data, 0, m.data.length);
            }
        }
        return out.toByteArray();
    }

    private static TestHost.Msg first(List<TestHost.Msg> msgs, String type) {
        for (TestHost.Msg m : msgs) {
            if (type.equals(m.t())) {
                return m;
            }
        }
        return null;
    }

    /** The {@code op}s of a rule event's changes, in order. */
    @SuppressWarnings("unchecked")
    private static List<String> ops(TestHost.Msg rule) {
        List<String> out = new ArrayList<>();
        for (Object o : rule.list("changes")) {
            Map<String, Object> c = (Map<String, Object>) o;
            String reason = (String) c.get("reason");
            String op = (String) c.get("op");
            out.add(c.get("name") != null ? op + " " + c.get("name") + (reason != null && !"status".equals(op) ? " (" + reason + ")" : "")
                    : op);
        }
        return out;
    }

    private static byte[] gzip(String s) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        GZIPOutputStream gz = new GZIPOutputStream(out);
        gz.write(s.getBytes(UTF_8));
        gz.close();
        return out.toByteArray();
    }

    @Test
    public void status_and_header_actions_leave_the_body_streaming() throws Exception {
        TestHost.Msg ack = rules("[" + rule("down", "{\"path\":{\"exact\":\"/a\"}}",
                "[{\"type\":\"status\",\"code\":503,\"reason\":\"Down\"},"
                        + "{\"type\":\"header\",\"op\":\"set\",\"name\":\"X-A\",\"value\":\"1\"},"
                        + "{\"type\":\"header\",\"op\":\"add\",\"name\":\"X-B\",\"value\":\"2\"},"
                        + "{\"type\":\"header\",\"op\":\"remove\",\"name\":\"ETag\"}]") + "]");
        assertEquals(1, ack.num("active"));
        server.enqueue(new MockResponse().setBody("hello").addHeader("ETag", "\"abc\"").addHeader("X-A", "0"));
        try (Response r = get("/a")) {
            assertEquals(503, r.code());
            assertEquals("Down", r.message());
            assertEquals("1", r.header("X-A"));
            assertEquals("2", r.header("X-B"));
            assertNull(r.header("ETag"));
            assertEquals("no-store", r.header("Cache-Control"));
            assertEquals("hello", r.body().string());
        }
        TestHost.Msg req = reqFor("/a");
        awaitEnd(req.txn);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        assertEquals("the original is recorded", 200, first(msgs, "resp").num("status"));
        TestHost.Msg rule = first(msgs, "rule");
        assertEquals(java.util.Arrays.asList("status", "header_set X-A", "header_add X-B", "header_remove ETag",
                "header_set Cache-Control (cache_guard)"), ops(rule));
        assertEquals(503, JsonParser2.num(rule.obj("delivered"), "status"));
        assertEquals("hello", new String(bodyOf(msgs, 1), UTF_8));
        assertEquals("no body of its own", 0, bodyOf(msgs, 2).length);
        assertNotNull(first(msgs, "done"));
    }

    @Test
    public void a_body_action_replaces_a_gzip_body_and_the_original_is_kept() throws Exception {
        rules("[" + rule("stub", "{\"path\":{\"glob\":\"/config/**\"}}",
                "[{\"type\":\"body\",\"text\":\"{\\\"ok\\\":false}\",\"content_type\":\"application/json\"}]") + "]");
        byte[] gz = gzip("{\"ok\":true,\"items\":[1,2,3]}");
        server.enqueue(new MockResponse().setBody(new Buffer().write(gz)).addHeader("Content-Encoding", "gzip")
                .addHeader("Content-Type", "text/plain"));
        try (Response r = get("/config/app/v1")) {
            assertEquals(200, r.code());
            assertEquals("{\"ok\":false}", r.body().string());
            assertNull("no Content-Encoding", r.header("Content-Encoding"));
            assertEquals("application/json", r.header("Content-Type"));
        }
        TestHost.Msg req = reqFor("/config/app/v1");
        awaitEnd(req.txn);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        assertArrayEquals("the original as received, still gzip", gz, bodyOf(msgs, 1));
        assertEquals("{\"ok\":false}", new String(bodyOf(msgs, 2), UTF_8));
        TestHost.Msg rule = first(msgs, "rule");
        assertEquals(java.util.Arrays.asList("body_replace", "header_set Content-Type", "header_remove Content-Encoding (body_changed)",
                "header_set Content-Length (body_changed)", "header_set Cache-Control (cache_guard)"), ops(rule));
        boolean deliveredEnd = false;
        for (TestHost.Msg m : msgs) {
            deliveredEnd |= "body_end".equals(m.t()) && "delivered".equals(m.str("dir"));
        }
        assertTrue("body_end for the delivered body", deliveredEnd);
    }

    @Test
    public void replace_edits_the_decoded_text_of_a_gzip_body() throws Exception {
        rules("[" + rule("pass", "{\"path\":{\"exact\":\"/status\"}}",
                "[{\"type\":\"replace\",\"find\":\"\\\"verdict\\\":\\\"pending\\\"\",\"with\":\"\\\"verdict\\\":\\\"pass\\\"\"}]")
                + "]");
        server.enqueue(new MockResponse().setBody(new Buffer().write(gzip("{\"verdict\":\"pending\",\"n\":1}")))
                .addHeader("Content-Encoding", "gzip").addHeader("Content-Type", "application/json; charset=utf-8"));
        try (Response r = get("/status")) {
            assertEquals("{\"verdict\":\"pass\",\"n\":1}", r.body().string());
        }
        TestHost.Msg req = reqFor("/status");
        awaitEnd(req.txn);
        TestHost.Msg rule = first(host.ofTxn(req.txn), "rule");
        @SuppressWarnings("unchecked")
        Map<String, Object> edit = (Map<String, Object>) rule.list("changes").get(0);
        assertEquals("body_edit", edit.get("op"));
        assertEquals(1L, edit.get("matches"));
    }

    @Test
    public void replace_is_literal_unless_regex_and_a_regex_can_use_groups() throws Exception {
        rules("[" + rule("lit", "{\"path\":{\"exact\":\"/lit\"}}",
                "[{\"type\":\"replace\",\"find\":\"a.b\",\"with\":\"$1\\\\\"}]") + ","
                + rule("re", "{\"path\":{\"exact\":\"/re\"}}",
                        "[{\"type\":\"replace\",\"find\":\"n=(\\\\d+)\",\"with\":\"n=$1$1\",\"regex\":true}]") + "]");
        server.enqueue(new MockResponse().setBody("a.b axb a.b"));
        server.enqueue(new MockResponse().setBody("n=4 n=12"));
        try (Response r = get("/lit")) {
            assertEquals("$1\\ axb $1\\", r.body().string());
        }
        try (Response r = get("/re")) {
            assertEquals("n=44 n=1212", r.body().string());
        }
    }

    @Test
    public void a_replace_that_finds_nothing_delivers_the_original() throws Exception {
        rules("[" + rule("none", "{}", "[{\"type\":\"replace\",\"find\":\"zzz\",\"with\":\"y\"}]") + "]");
        byte[] gz = gzip("plain text");
        server.enqueue(new MockResponse().setBody(new Buffer().write(gz)).addHeader("Content-Encoding", "gzip"));
        try (Response r = get("/n")) {
            // OkHttp's transparent gzip still sees the original Content-Encoding
            assertEquals("plain text", r.body().string());
            assertNull("no cache guard when nothing changed", r.header("Cache-Control"));
        }
    }

    @Test
    public void delays_then_a_failure_happen_before_the_request_is_sent() throws Exception {
        rules("[" + rule("slow-fail", "{\"methods\":[\"POST\"]}",
                "[{\"type\":\"delay\",\"ms\":300},{\"type\":\"fail\",\"exception\":\"timeout\",\"message\":\"nope\"}]")
                + "]");
        long start = System.nanoTime();
        try {
            client.newCall(new Request.Builder().url(url("/enroll"))
                    .post(RequestBody.create(null, "x".getBytes(UTF_8))).build()).execute().close();
            fail("expected a timeout");
        } catch (SocketTimeoutException e) {
            assertEquals("nope", e.getMessage());
        }
        assertTrue("waited the delay", System.nanoTime() - start >= TimeUnit.MILLISECONDS.toNanos(290));
        assertEquals("never sent", 0, server.getRequestCount());
        TestHost.Msg req = reqFor("/enroll");
        TestHost.Msg end = awaitEnd(req.txn);
        assertEquals("fail", end.t());
        assertEquals(Boolean.TRUE, end.json.get("simulated"));
        TestHost.Msg rule = first(host.ofTxn(req.txn), "rule");
        assertEquals(java.util.Arrays.asList("delay", "fail"), ops(rule));
    }

    @Test
    public void every_failure_kind_throws_its_exception() throws Exception {
        String[][] kinds = {
            {"timeout", SocketTimeoutException.class.getName()},
            {"io", IOException.class.getName()},
            {"protocol", ProtocolException.class.getName()},
            {"unknown_host", UnknownHostException.class.getName()},
            {"connect", ConnectException.class.getName()},
        };
        for (String[] k : kinds) {
            rules("[" + rule("f", "{}", "[{\"type\":\"fail\",\"exception\":\"" + k[0] + "\"}]") + "]");
            try {
                get("/" + k[0]).close();
                fail("expected " + k[1]);
            } catch (IOException e) {
                assertEquals(k[1], e.getClass().getName());
                assertEquals("simulated by traffic-police", e.getMessage());
            }
        }
        assertEquals(0, server.getRequestCount());
    }

    @Test
    public void a_canceled_call_does_not_wait_out_a_delay() throws Exception {
        rules("[" + rule("long", "{}", "[{\"type\":\"delay\",\"ms\":20000}]") + "]");
        server.enqueue(new MockResponse().setBody("late"));
        final CountDownLatch done = new CountDownLatch(1);
        final AtomicReference<IOException> error = new AtomicReference<>();
        Call call = client.newCall(new Request.Builder().url(url("/slow")).build());
        call.enqueue(new Callback() {
            @Override
            public void onFailure(Call c, IOException e) {
                error.set(e);
                done.countDown();
            }

            @Override
            public void onResponse(Call c, Response r) {
                r.close();
                done.countDown();
            }
        });
        Thread.sleep(200);
        call.cancel();
        assertTrue("ended soon after the cancel", done.await(3, TimeUnit.SECONDS));
        assertNotNull(error.get());
    }

    @Test
    public void rules_apply_while_paused_but_nothing_is_recorded() throws Exception {
        rules("[" + rule("teapot", "{}", "[{\"type\":\"status\",\"code\":418}]") + "]");
        host.send("{\"t\":\"set_config\",\"id\":9,\"config\":{\"recording\":false}}");
        host.awaitType("config_ack", 5_000);
        server.enqueue(new MockResponse().setBody("x"));
        try (Response r = get("/paused")) {
            assertEquals(418, r.code());
            assertEquals("no standard reason phrase for 418", "", r.message());
            assertEquals("x", r.body().string());
        }
        Thread.sleep(200);
        for (TestHost.Msg m : host.received()) {
            assertFalse("no req while paused", "req".equals(m.t()) && m.str("url").endsWith("/paused"));
        }
    }

    @Test
    public void an_upgrade_is_never_rewritten_recording_or_paused() throws Exception {
        // a plain call that asks for an upgrade (WebSocket calls skip network interceptors)
        rules("[" + rule("up", "{\"path\":{\"exact\":\"/up\"}}",
                "[{\"type\":\"status\",\"code\":500},{\"type\":\"body\",\"text\":\"x\"}]") + "]");
        for (boolean recording : new boolean[] {true, false}) {
            if (!recording) {
                host.send("{\"t\":\"set_config\",\"id\":9,\"config\":{\"recording\":false}}");
                host.awaitType("config_ack", 5_000);
            }
            server.enqueue(new MockResponse().setResponseCode(101)
                    .setHeader("Connection", "Upgrade").setHeader("Upgrade", "tp-test"));
            Request up = new Request.Builder().url(url("/up"))
                    .header("Connection", "Upgrade").header("Upgrade", "tp-test").build();
            try (Response r = client.newCall(up).execute()) {
                assertEquals("the upgrade goes through untouched (recording: " + recording + ")", 101, r.code());
            }
        }
        for (TestHost.Msg m : host.received()) {
            assertFalse("no rule event for an upgrade", "rule".equals(m.t()));
        }
    }

    @Test
    public void a_simulated_failure_is_retried_only_by_okhttp_3_9_to_3_12() throws Exception {
        // OkHttp 4 and 5 retry only failures they saw on a connection, and a rule throws before the
        // request reaches one; 3.14 does not retry it either. 3.9 to 3.12 retry the recoverable
        // kinds when the call was on a pooled connection, or on a new one when the host has another
        // address to try, and the rule fails each attempt again.
        String v = OkHttpDetect.version();
        boolean retries = v.startsWith("3.") && !v.startsWith("3.14.");
        OkHttpClient retrying = client.newBuilder().retryOnConnectionFailure(true).build();
        final InetAddress lo = InetAddress.getByName("127.0.0.1");
        OkHttpClient twoAddresses = retrying.newBuilder().dns(host -> Arrays.asList(lo, lo)).build();
        for (String kind : new String[] {"timeout", "protocol", "io", "connect", "unknown_host"}) {
            boolean recoverable = !kind.equals("timeout") && !kind.equals("protocol");
            for (String on : new String[] {"pooled", "two-addresses"}) {
                String path = "/retry-" + kind + "-" + on;
                rules("[" + rule("f", "{\"path\":{\"exact\":\"" + path + "\"}}",
                        "[{\"type\":\"fail\",\"exception\":\"" + kind + "\"}]") + "]");
                retrying.connectionPool().evictAll();
                Request request;
                OkHttpClient c;
                if (on.equals("pooled")) {
                    // a connection that has served a call
                    server.enqueue(new MockResponse().setBody("warm"));
                    try (Response r = retrying.newCall(new Request.Builder().url(url("/warm")).build()).execute()) {
                        r.body().string();
                    }
                    c = retrying;
                    request = new Request.Builder().url(url(path)).build();
                } else {
                    c = twoAddresses;
                    request = new Request.Builder().url("http://tp-two.test:" + server.getPort() + path).build();
                }
                try {
                    c.newCall(request).execute();
                    fail("expected the rule's failure");
                } catch (IOException expected) {
                    // the rule's, after any retries
                }
                Thread.sleep(300);
                int attempts = 0;
                for (TestHost.Msg m : host.received()) {
                    if ("req".equals(m.t()) && m.str("url").endsWith(path)) {
                        attempts++;
                    }
                }
                String what = kind + " on a " + on + " connection, OkHttp " + v;
                if (retries && recoverable) {
                    assertTrue(what + " is retried: " + attempts + " attempts", attempts > 1);
                } else {
                    assertEquals(what + " is not retried", 1, attempts);
                }
            }
        }
    }

    @Test
    public void rules_stop_when_the_host_disconnects() throws Exception {
        rules("[" + rule("gone", "{}", "[{\"type\":\"status\",\"code\":500}]") + "]");
        host.close();
        Thread.sleep(300);
        server.enqueue(new MockResponse().setBody("ok"));
        try (Response r = get("/after")) {
            assertEquals("the app's normal behaviour is back", 200, r.code());
        }
    }

    @Test
    public void matching_uses_the_wire_request() throws Exception {
        int port = server.getPort();
        rules("["
                + rule("m-get", "{\"methods\":[\"get\"],\"scheme\":\"http\",\"host\":{\"glob\":\"LOCALHOST\"},\"port\":" + port
                        + ",\"path\":{\"glob\":\"/api/*/status\"},\"query\":[{\"name\":\"q\",\"value\":{\"glob\":\"a b*\"}}]}",
                        "[{\"type\":\"header\",\"op\":\"set\",\"name\":\"X-Hit\",\"value\":\"get\"}]")
                + "," + rule("m-deep", "{\"path\":{\"glob\":\"/deep/**\"}}",
                        "[{\"type\":\"header\",\"op\":\"set\",\"name\":\"X-Hit\",\"value\":\"deep\"}]")
                + ",{\"id\":\"off\",\"enabled\":false,\"match\":{},\"actions\":[{\"type\":\"status\",\"code\":500}]}"
                + "]");
        String[][] cases = {
            {"/api/v1/status?q=a%20b%20c", "get"},
            {"/api/v1/x/status?q=a+b", null}, // * stays within a segment
            {"/api/v1/status?q=zz", null},
            {"/api/v1/status", null}, // the query parameter must be present
            {"/deep/a/b/c", "deep"},
        };
        for (String[] c : cases) {
            server.enqueue(new MockResponse().setBody("x"));
            try (Response r = client.newCall(new Request.Builder().url("http://localhost:" + port + c[0]).build()).execute()) {
                assertEquals(c[0], c[1], r.header("X-Hit"));
                assertEquals("the disabled rule does nothing", 200, r.code());
            }
        }
    }

    @Test
    public void rules_ack_reports_each_bad_rule_and_keeps_the_rest() throws Exception {
        TestHost.Msg ack = rules("["
                + rule("good", "{}", "[{\"type\":\"delay\",\"ms\":0}]") + ","
                + rule("bad-regex", "{\"path\":{\"regex\":\"(open\"}}", "[]") + ","
                + rule("bad-action", "{}", "[{\"type\":\"teleport\"}]") + ","
                + rule("bad-status", "{}", "[{\"type\":\"status\",\"code\":99}]") + "]");
        assertEquals(1, ack.num("active"));
        List<Object> errors = ack.list("errors");
        assertEquals(3, errors.size());
        @SuppressWarnings("unchecked")
        Map<String, Object> e0 = (Map<String, Object>) errors.get(0);
        assertEquals("bad-regex", e0.get("rule"));
        assertEquals("match.path.regex", e0.get("field"));
        @SuppressWarnings("unchecked")
        Map<String, Object> e1 = (Map<String, Object>) errors.get(1);
        assertEquals("actions[0].type", e1.get("field"));
        assertTrue(((String) e1.get("message")).contains("teleport"));
    }

    @Test
    public void cache_rewrites_turns_the_cache_guard_off() throws Exception {
        rules("[{\"id\":\"c\",\"cache_rewrites\":true,\"match\":{},"
                + "\"actions\":[{\"type\":\"header\",\"op\":\"set\",\"name\":\"X-C\",\"value\":\"1\"}]}]");
        server.enqueue(new MockResponse().setBody("x").addHeader("Cache-Control", "max-age=60"));
        try (Response r = get("/c")) {
            assertEquals("1", r.header("X-C"));
            assertEquals("max-age=60", r.header("Cache-Control"));
        }
    }

    @Test
    public void deflate_is_decoded_and_brotli_is_left_alone() throws Exception {
        rules("[" + rule("r", "{}", "[{\"type\":\"replace\",\"find\":\"old\",\"with\":\"new\"}]") + "]");
        ByteArrayOutputStream raw = new ByteArrayOutputStream();
        DeflaterOutputStream d = new DeflaterOutputStream(raw, new Deflater());
        d.write("the old value".getBytes(UTF_8));
        d.close();
        server.enqueue(new MockResponse().setBody(new Buffer().write(raw.toByteArray())).addHeader("Content-Encoding", "deflate"));
        try (Response r = get("/deflate")) {
            assertEquals("the new value", r.body().string());
        }
        byte[] fake = {1, 2, 3, 4};
        server.enqueue(new MockResponse().setBody(new Buffer().write(fake)).addHeader("Content-Encoding", "br"));
        try (Response r = get("/br")) {
            assertArrayEquals("delivered untouched", fake, r.body().bytes());
            assertEquals("br", r.header("Content-Encoding"));
        }
        TestHost.Msg diag = host.await(m -> "diag".equals(m.t()) && "rule_skipped".equals(m.str("code")), 5_000);
        assertTrue(diag.str("message").contains("br"));
    }

    @Test
    public void a_body_over_the_limit_streams_through_untouched() throws Exception {
        rules("[" + rule("big", "{}", "[{\"type\":\"body\",\"text\":\"small\"},{\"type\":\"header\",\"op\":\"set\",\"name\":\"X-Seen\",\"value\":\"1\"}]") + "]");
        byte[] big = new byte[33 * 1024 * 1024];
        for (int i = 0; i < big.length; i++) {
            big[i] = (byte) ('a' + i % 26);
        }
        server.enqueue(new MockResponse().setBody(new Buffer().write(big)));
        try (Response r = get("/big")) {
            assertEquals("the header action still applies", "1", r.header("X-Seen"));
            assertArrayEquals(big, r.body().bytes());
        }
        host.await(m -> "diag".equals(m.t()) && "rule_skipped".equals(m.str("code")), 5_000);
    }

    /** Typed access to nested JSON in tests. */
    private static final class JsonParser2 {
        static long num(Map<String, Object> m, String key) {
            Object v = m.get(key);
            return v instanceof Long ? (Long) v : Long.MIN_VALUE;
        }
    }
}
