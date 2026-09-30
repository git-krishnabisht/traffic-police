package io.trafficpolice.capture.huc;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import java.io.ByteArrayOutputStream;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.Charset;
import java.util.List;
import okhttp3.mockwebserver.MockResponse;
import okhttp3.mockwebserver.MockWebServer;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/** HttpURLConnection capture through the wrapper (ARCHITECTURE.md §4.3), on the JDK's implementation. */
public final class HucCaptureTest {
    private static final Charset UTF_8 = Charset.forName("UTF-8");

    private final TestPlatform platform = new TestPlatform(false);
    private CaptureRuntime rt;
    private TestHost host;
    private MockWebServer server;

    @Before
    public void setUp() throws Exception {
        rt = TestPlatform.runtime(platform, false);
        host = TestHost.connect(rt);
        server = new MockWebServer();
        server.start();
    }

    @After
    public void tearDown() throws Exception {
        host.close();
        server.shutdown();
        rt.stop();
    }

    private HttpURLConnection open(String path) throws IOException {
        URL url = server.url(path).url();
        HttpURLConnection c = Huc.wrap((HttpURLConnection) url.openConnection());
        assertTrue(c instanceof TrackedHttpURLConnection);
        return c;
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

    private static TestHost.Msg bodyEnd(List<TestHost.Msg> msgs, String dir) {
        TestHost.Msg found = null;
        for (TestHost.Msg m : msgs) {
            if ("body_end".equals(m.t()) && dir.equals(m.str("dir"))) {
                found = m;
            }
        }
        return found;
    }

    @Test
    public void getIsCaptured() throws Exception {
        server.enqueue(new MockResponse().setBody("hello huc").addHeader("X-Dup", "1").addHeader("X-Dup", "2"));
        HttpURLConnection c = open("/huc/get");
        c.setRequestProperty("Accept", "text/plain");
        assertEquals(200, io.trafficpolice.testapp.AppCode.fetchStatus(c));
        assertEquals("hello huc", readAll(c.getInputStream()));
        TestHost.Msg req = reqFor("/huc/get");
        assertEquals("done", end(req.txn).t());
        assertEquals("GET", req.str("method"));
        assertEquals("huc", req.obj("client").get("kind"));
        assertEquals("huc", req.obj("thread").get("origin"));
        assertTrue(req.list("headers").contains(java.util.Arrays.asList("Accept", "text/plain")));
        Object top = ((java.util.Map<?, ?>) req.list("stack").get(0)).get("c");
        assertEquals("the stack starts at the app's call", "io.trafficpolice.testapp.AppCode", top);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        TestHost.Msg resp = host.await(m -> "resp".equals(m.t()) && m.txn == req.txn, 1000);
        assertEquals(200, resp.num("status"));
        assertTrue(resp.list("headers").toString(), resp.list("headers").contains(java.util.Arrays.asList("X-Dup", "1")));
        assertTrue(resp.list("headers").contains(java.util.Arrays.asList("X-Dup", "2")));
        assertEquals("hello huc", body(msgs, 1));
        assertEquals("complete", bodyEnd(msgs, "response").str("state"));
        assertEquals("none", bodyEnd(msgs, "request").str("state"));
    }

    @Test
    public void postBodyIsCaptured() throws Exception {
        server.enqueue(new MockResponse().setResponseCode(201).setBody("made"));
        HttpURLConnection c = open("/huc/post");
        c.setDoOutput(true);
        c.setRequestProperty("Content-Type", "application/json");
        OutputStream out = c.getOutputStream();
        out.write("{\"a\":1}".getBytes(UTF_8));
        out.close();
        assertEquals(201, c.getResponseCode());
        assertEquals("made", readAll(c.getInputStream()));
        TestHost.Msg req = reqFor("/huc/post");
        end(req.txn);
        assertEquals("POST", req.str("method"));
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        assertEquals("{\"a\":1}", body(msgs, 0));
        assertEquals("complete", bodyEnd(msgs, "request").str("state"));
        assertEquals("{\"a\":1}", server.takeRequest().getBody().readUtf8());
    }

    @Test
    public void errorStatusKeepsHeadersAndErrorBody() throws Exception {
        server.enqueue(new MockResponse().setResponseCode(404).setBody("{\"error\":\"missing\"}"));
        HttpURLConnection c = open("/huc/missing");
        try {
            c.getInputStream();
            fail("HttpURLConnection throws for 4xx");
        } catch (FileNotFoundException expected) {
            // the app sees what it always does
        }
        assertEquals("{\"error\":\"missing\"}", readAll(c.getErrorStream()));
        TestHost.Msg req = reqFor("/huc/missing");
        assertEquals("done", end(req.txn).t());
        TestHost.Msg resp = host.await(m -> "resp".equals(m.t()) && m.txn == req.txn, 1000);
        assertEquals(404, resp.num("status"));
        assertEquals("{\"error\":\"missing\"}", body(host.ofTxn(req.txn), 1));
    }

    @Test
    public void disconnectEndsTheTransaction() throws Exception {
        server.enqueue(new MockResponse().setBody("unread"));
        HttpURLConnection c = open("/huc/disconnect");
        assertEquals(200, c.getResponseCode());
        c.disconnect();
        TestHost.Msg req = reqFor("/huc/disconnect");
        assertEquals("done", end(req.txn).t());
        assertEquals("closed_early", bodyEnd(host.ofTxn(req.txn), "response").str("state"));
    }

    @Test
    public void connectFailureIsRecorded() throws Exception {
        int port;
        try (java.net.ServerSocket s = new java.net.ServerSocket(0)) {
            port = s.getLocalPort();
        }
        HttpURLConnection c = Huc.wrap((HttpURLConnection) new URL("http://127.0.0.1:" + port + "/huc/nobody").openConnection());
        try {
            c.getResponseCode();
            fail("expected a connect failure");
        } catch (IOException expected) {
            // ConnectException
        }
        TestHost.Msg req = reqFor("/huc/nobody");
        TestHost.Msg failure = end(req.txn);
        assertEquals("fail", failure.t());
        assertEquals("connect", failure.str("phase"));
    }

    @Test
    public void headHasNoBody() throws Exception {
        server.enqueue(new MockResponse().addHeader("Content-Length", "5"));
        HttpURLConnection c = open("/huc/head");
        c.setRequestMethod("HEAD");
        assertEquals(200, c.getResponseCode());
        TestHost.Msg req = reqFor("/huc/head");
        assertEquals("done", end(req.txn).t());
        assertEquals("none", bodyEnd(host.ofTxn(req.txn), "response").str("state"));
    }

    @Test
    public void wrapPassesThroughWithoutARuntime() throws Exception {
        rt.stop();
        URL url = server.url("/x").url();
        HttpURLConnection raw = (HttpURLConnection) url.openConnection();
        assertTrue(Huc.wrap(raw) == raw);
        rt = TestPlatform.runtime(platform, false);
    }
}
