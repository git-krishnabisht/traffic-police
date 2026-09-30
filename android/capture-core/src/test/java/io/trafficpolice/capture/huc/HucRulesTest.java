package io.trafficpolice.capture.huc;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import java.io.ByteArrayOutputStream;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.InputStream;
import java.net.HttpURLConnection;
import java.net.SocketTimeoutException;
import java.net.URL;
import java.nio.charset.Charset;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.zip.GZIPOutputStream;
import okhttp3.mockwebserver.MockResponse;
import okhttp3.mockwebserver.MockWebServer;
import okio.Buffer;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/**
 * Rules through the HttpURLConnection wrapper (PROTOCOL.md §8.4), on the JDK's implementation:
 * a delay or failure on the first call that connects, and the rules' status, headers and body
 * everywhere the app can read them.
 */
public final class HucRulesTest {
    private static final Charset UTF_8 = Charset.forName("UTF-8");

    private final TestPlatform platform = new TestPlatform(false);
    private final AtomicLong ids = new AtomicLong(100);
    private CaptureRuntime rt;
    private TestHost host;
    private MockWebServer server;

    @Before
    public void setUp() throws Exception {
        rt = TestPlatform.runtime(platform, false);
        host = TestHost.connect(rt);
        server = new MockWebServer();
        server.start(java.net.InetAddress.getByName("127.0.0.1"), 0);
    }

    @After
    public void tearDown() throws Exception {
        host.close();
        server.shutdown();
        rt.stop();
    }

    private void rules(String array) throws Exception {
        final long id = ids.incrementAndGet();
        host.send("{\"t\":\"set_rules\",\"id\":" + id + ",\"rules\":{\"version\":\"v\",\"rules\":" + array + "}}");
        TestHost.Msg ack = host.await(m -> "rules_ack".equals(m.t()) && m.num("id") == id, 5_000);
        assertNotNull(ack);
        assertTrue("rules accepted: " + ack.json, ack.list("errors").isEmpty());
    }

    private static String rule(String id, String match, String actions) {
        return "{\"id\":\"" + id + "\",\"match\":" + match + ",\"actions\":" + actions + "}";
    }

    private HttpURLConnection open(String path) throws IOException {
        URL url = new URL("http://127.0.0.1:" + server.getPort() + path);
        return Huc.wrap((HttpURLConnection) url.openConnection());
    }

    private static String readAll(InputStream in) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        byte[] buf = new byte[1024];
        for (int n; (n = in.read(buf)) != -1; ) {
            out.write(buf, 0, n);
        }
        in.close();
        return new String(out.toByteArray(), UTF_8);
    }

    private TestHost.Msg reqFor(String path) throws InterruptedException {
        return host.await(m -> "req".equals(m.t()) && m.str("url").endsWith(path), 10_000);
    }

    private TestHost.Msg end(long txn) throws InterruptedException {
        return host.await(m -> ("done".equals(m.t()) || "fail".equals(m.t())) && m.txn == txn, 10_000);
    }

    private static String body(List<TestHost.Msg> msgs, int dir) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (TestHost.Msg m : msgs) {
            if (m.data != null && m.dir == dir) {
                out.write(m.data, 0, m.data.length);
            }
        }
        return new String(out.toByteArray(), UTF_8);
    }

    @Test
    public void status_and_headers_show_in_every_getter() throws Exception {
        rules("[" + rule("s", "{\"path\":{\"exact\":\"/s\"}}",
                "[{\"type\":\"status\",\"code\":201,\"reason\":\"Made\"},"
                        + "{\"type\":\"header\",\"op\":\"set\",\"name\":\"Content-Type\",\"value\":\"text/x-rule\"},"
                        + "{\"type\":\"header\",\"op\":\"remove\",\"name\":\"X-Gone\"}]") + "]");
        server.enqueue(new MockResponse().setBody("kept").addHeader("X-Gone", "1").addHeader("Content-Type", "text/plain"));
        HttpURLConnection c = open("/s");
        assertEquals(201, c.getResponseCode());
        assertEquals("Made", c.getResponseMessage());
        assertEquals("text/x-rule", c.getContentType());
        assertEquals("text/x-rule", c.getHeaderField("content-type"));
        assertNull(c.getHeaderField("X-Gone"));
        assertEquals("HTTP/1.1 201 Made", c.getHeaderField(0));
        assertNull(c.getHeaderFieldKey(0));
        Map<String, List<String>> all = c.getHeaderFields();
        assertEquals("no-store", all.get("Cache-Control").get(0));
        assertEquals("kept", readAll(c.getInputStream()));
        TestHost.Msg req = reqFor("/s");
        end(req.txn);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        assertEquals("the original body is recorded", "kept", body(msgs, 1));
        TestHost.Msg rule = null;
        for (TestHost.Msg m : msgs) {
            rule = "rule".equals(m.t()) ? m : rule;
        }
        assertNotNull(rule);
        assertEquals(201L, rule.obj("delivered").get("status"));
    }

    @Test
    public void an_error_status_follows_the_platform_both_ways() throws Exception {
        rules("[" + rule("to-500", "{\"path\":{\"exact\":\"/ok\"}}", "[{\"type\":\"status\",\"code\":500}]") + ","
                + rule("to-200", "{\"path\":{\"exact\":\"/err\"}}", "[{\"type\":\"status\",\"code\":200}]") + "]");
        server.enqueue(new MockResponse().setBody("fine"));
        server.enqueue(new MockResponse().setResponseCode(503).setBody("sorry"));
        HttpURLConnection ok = open("/ok");
        assertEquals(500, ok.getResponseCode());
        try {
            ok.getInputStream();
            fail("an error status throws");
        } catch (IOException expected) {
            // the JDK: IOException ("Server returned HTTP response code: 500"); Android: FileNotFoundException
        }
        assertEquals("the body moves to the error stream", "fine", readAll(ok.getErrorStream()));
        HttpURLConnection err = open("/err");
        assertEquals(200, err.getResponseCode());
        assertNull(err.getErrorStream());
        assertEquals("the error body is readable now", "sorry", readAll(err.getInputStream()));
    }

    @Test
    public void a_body_and_a_replace_on_gzip() throws Exception {
        rules("[" + rule("b", "{\"path\":{\"exact\":\"/b\"}}",
                "[{\"type\":\"body\",\"text\":\"{\\\"stub\\\":1}\",\"content_type\":\"application/json\"}]") + ","
                + rule("r", "{\"path\":{\"exact\":\"/r\"}}", "[{\"type\":\"replace\",\"find\":\"pending\",\"with\":\"pass\"}]")
                + "]");
        server.enqueue(new MockResponse().setBody("original").addHeader("Content-Type", "text/plain"));
        ByteArrayOutputStream gz = new ByteArrayOutputStream();
        GZIPOutputStream g = new GZIPOutputStream(gz);
        g.write("{\"verdict\":\"pending\"}".getBytes(UTF_8));
        g.close();
        server.enqueue(new MockResponse().setBody(new Buffer().write(gz.toByteArray())).addHeader("Content-Encoding", "gzip"));
        HttpURLConnection b = open("/b");
        assertEquals("{\"stub\":1}", readAll(b.getInputStream()));
        assertEquals("application/json", b.getContentType());
        assertEquals(10, b.getContentLength());
        HttpURLConnection r = open("/r");
        assertEquals("decoded, edited, delivered plain", "{\"verdict\":\"pass\"}", readAll(r.getInputStream()));
        assertNull(r.getContentEncoding());
        TestHost.Msg req = reqFor("/b");
        end(req.txn);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        assertEquals("original", body(msgs, 1));
        assertEquals("{\"stub\":1}", body(msgs, 2));
    }

    @Test
    public void delay_and_failure_happen_on_the_first_call_that_connects() throws Exception {
        rules("[" + rule("f", "{\"path\":{\"exact\":\"/f\"}}",
                "[{\"type\":\"delay\",\"ms\":200},{\"type\":\"fail\",\"exception\":\"timeout\"}]") + "]");
        HttpURLConnection c = open("/f");
        long start = System.nanoTime();
        assertNull("a getter answers as for a failure", c.getHeaderField("Content-Type"));
        assertTrue(System.nanoTime() - start >= TimeUnit.MILLISECONDS.toNanos(190));
        assertEquals(-1, c.getContentLength());
        try {
            c.getResponseCode();
            fail("expected the rule's failure");
        } catch (SocketTimeoutException expected) {
            assertEquals("simulated by traffic-police", expected.getMessage());
        }
        try {
            c.getInputStream();
            fail("and again");
        } catch (SocketTimeoutException expected) {
            // the same failure
        }
        assertNull(c.getErrorStream());
        assertEquals("never sent", 0, server.getRequestCount());
        TestHost.Msg req = reqFor("/f");
        assertEquals(Boolean.TRUE, end(req.txn).json.get("simulated"));
    }

    @Test
    public void a_404_turned_200_and_a_failure_on_connect() throws Exception {
        rules("[" + rule("c", "{\"path\":{\"exact\":\"/c\"}}", "[{\"type\":\"fail\",\"exception\":\"connect\"}]") + "]");
        HttpURLConnection c = open("/c");
        try {
            c.connect();
            fail("expected a ConnectException");
        } catch (java.net.ConnectException expected) {
            // thrown by connect() itself
        }
        rules("[" + rule("nf", "{}", "[{\"type\":\"status\",\"code\":404}]") + "]");
        server.enqueue(new MockResponse().setBody("here"));
        HttpURLConnection nf = open("/nf");
        try {
            nf.getInputStream();
            fail("a 404 throws FileNotFoundException");
        } catch (FileNotFoundException expected) {
            // as the platform does
        }
    }
}
