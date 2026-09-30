package io.trafficpolice.capture.okhttp;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.ServerSocket;
import java.net.SocketTimeoutException;
import java.nio.charset.Charset;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import java.util.zip.GZIPOutputStream;
import okhttp3.Call;
import okhttp3.Callback;
import okhttp3.EventListener;
import okhttp3.MediaType;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.RequestBody;
import okhttp3.Response;
import okhttp3.mockwebserver.MockResponse;
import okhttp3.mockwebserver.MockWebServer;
import okhttp3.mockwebserver.SocketPolicy;
import okio.Buffer;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/**
 * OkHttp capture end to end: a real client and MockWebServer, our interceptor and listener, and
 * the events as a host receives them. Runs against every OkHttp version in the build's matrix.
 */
public final class OkHttpCaptureTest {
    private static final Charset UTF_8 = Charset.forName("UTF-8");

    private final TestPlatform platform = new TestPlatform(false);
    private CaptureRuntime rt;
    private TestHost host;
    private MockWebServer server;

    @Before
    public void setUp() throws Exception {
        rt = TestPlatform.runtime(platform, true);
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

    private static OkHttpClient client(EventListener.Factory appFactory) {
        return new OkHttpClient.Builder()
                .addNetworkInterceptor(CaptureInterceptor.INSTANCE)
                .eventListenerFactory(new ListenerFactory(appFactory))
                .readTimeout(5, TimeUnit.SECONDS)
                .build();
    }

    private static OkHttpClient client() {
        return client(null);
    }

    private TestHost.Msg awaitDone(long txn) throws InterruptedException {
        return host.await(m -> ("done".equals(m.t()) || "fail".equals(m.t())) && m.txn == txn, 10_000);
    }

    private TestHost.Msg reqFor(String path) throws InterruptedException {
        return host.await(m -> "req".equals(m.t()) && m.str("url").endsWith(path), 10_000);
    }

    private static byte[] bodyOf(List<TestHost.Msg> msgs, int dir) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (TestHost.Msg m : msgs) {
            if (m.data != null && m.dir == dir) {
                assertEquals("chunks arrive in order", out.size(), m.offset);
                out.write(m.data, 0, m.data.length);
            }
        }
        return out.toByteArray();
    }

    private static TestHost.Msg last(List<TestHost.Msg> msgs, String type, String dir) {
        TestHost.Msg found = null;
        for (TestHost.Msg m : msgs) {
            if (type.equals(m.t()) && (dir == null || dir.equals(m.str("dir")))) {
                found = m;
            }
        }
        return found;
    }

    private static List<String> markNames(List<TestHost.Msg> msgs, TestHost.Msg req) {
        List<String> out = new ArrayList<>();
        for (Object o : req.list("marks")) {
            out.add((String) ((List<?>) o).get(0));
        }
        for (TestHost.Msg m : msgs) {
            if ("mark".equals(m.t())) {
                out.add(m.str("m"));
            }
        }
        return out;
    }

    private static byte[] gzip(String s) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        GZIPOutputStream gz = new GZIPOutputStream(bytes);
        gz.write(s.getBytes(UTF_8));
        gz.close();
        return bytes.toByteArray();
    }

    @Test
    public void getWithGzipJson() throws Exception {
        String json = "{\"ok\":true,\"items\":[1,2,3]}";
        byte[] wire = gzip(json);
        server.enqueue(new MockResponse().setBody(new Buffer().write(wire)).addHeader("Content-Encoding", "gzip")
                .addHeader("Content-Type", "application/json").addHeader("Set-Cookie", "a=1")
                .addHeader("Set-Cookie", "b=2"));
        Response response = client().newCall(new Request.Builder().url(server.url("/api/items?id=7")).build()).execute();
        assertEquals(json, response.body().string()); // the app still gets the decoded body

        TestHost.Msg req = reqFor("/api/items?id=7");
        awaitDone(req.txn);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        assertEquals("GET", req.str("method"));
        assertEquals("okhttp", ((Map<?, ?>) req.obj("client")).get("kind"));
        // Bridge added Accept-Encoding before our network interceptor saw the request
        assertTrue(req.list("headers").toString(), req.list("headers").contains(java.util.Arrays.asList("Accept-Encoding", "gzip")));
        Map<String, Object> thread = req.obj("thread");
        assertEquals("call", thread.get("origin"));
        assertEquals(Thread.currentThread().getName(), thread.get("name"));
        String stack = req.list("stack").toString();
        assertTrue(stack, stack.contains("getWithGzipJson"));
        // our own frames at the top are dropped: the stack starts in OkHttp's call
        Object top = ((Map<?, ?>) req.list("stack").get(0)).get("c");
        assertTrue(String.valueOf(top), String.valueOf(top).startsWith("okhttp3."));
        assertEquals(0, req.num("hop"));
        assertNotNull(req.obj("conn"));

        TestHost.Msg resp = last(msgs, "resp", null);
        assertEquals(200, resp.num("status"));
        List<Object> headers = resp.list("headers");
        assertTrue(headers.contains(java.util.Arrays.asList("Content-Encoding", "gzip")));
        assertTrue(headers.contains(java.util.Arrays.asList("Set-Cookie", "a=1")));
        assertTrue(headers.contains(java.util.Arrays.asList("Set-Cookie", "b=2")));
        // captured as it came off the wire: still gzip
        assertArrayEquals(wire, bodyOf(msgs, 1));
        TestHost.Msg end = last(msgs, "body_end", "response");
        assertEquals("complete", end.str("state"));
        assertEquals(wire.length, end.num("bytes"));
        assertEquals("none", last(msgs, "body_end", "request").str("state"));

        List<String> marks = markNames(msgs, req);
        assertTrue(marks.toString(), marks.contains("call_start"));
        assertTrue(marks.toString(), marks.contains("conn_acquired"));
        assertTrue(marks.toString(), marks.contains("resp_headers_start"));
        assertTrue(marks.toString(), marks.contains("resp_body_end"));
    }

    @Test
    public void postCapturesTheRequestBody() throws Exception {
        server.enqueue(new MockResponse().setBody("created").setResponseCode(201));
        String payload = "{\"name\":\"traffic-police\",\"n\":42}";
        RequestBody body = RequestBody.create(MediaType.parse("application/json; charset=utf-8"), payload);
        Response response = client().newCall(new Request.Builder().url(server.url("/api/things")).post(body).build()).execute();
        response.body().string();
        assertEquals(payload, server.takeRequest().getBody().readUtf8());

        TestHost.Msg req = reqFor("/api/things");
        awaitDone(req.txn);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        assertEquals("POST", req.str("method"));
        Map<String, Object> info = req.obj("body");
        assertEquals((long) payload.length(), info.get("length"));
        assertTrue(String.valueOf(info.get("type")).startsWith("application/json"));
        assertEquals(payload, new String(bodyOf(msgs, 0), UTF_8));
        TestHost.Msg end = last(msgs, "body_end", "request");
        assertEquals("complete", end.str("state"));
        assertEquals(payload.length(), end.num("bytes"));
        assertEquals("created", new String(bodyOf(msgs, 1), UTF_8));
    }

    @Test
    public void streamingResponseArrivesInPieces() throws Exception {
        StringBuilder big = new StringBuilder();
        for (int i = 0; i < 2000; i++) {
            big.append("line ").append(i).append('\n');
        }
        server.enqueue(new MockResponse().setChunkedBody(big.toString(), 1024).throttleBody(4096, 5, TimeUnit.MILLISECONDS));
        Response response = client().newCall(new Request.Builder().url(server.url("/stream")).build()).execute();
        assertEquals(big.toString(), response.body().string());
        TestHost.Msg req = reqFor("/stream");
        awaitDone(req.txn);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        int chunks = 0;
        for (TestHost.Msg m : msgs) {
            if (m.data != null) {
                chunks++;
            }
        }
        assertTrue("several chunks: " + chunks, chunks > 1);
        assertEquals(big.toString(), new String(bodyOf(msgs, 1), UTF_8));
    }

    @Test
    public void redirectIsTwoHopsOfOneCall() throws Exception {
        server.enqueue(new MockResponse().setResponseCode(302).addHeader("Location", "/final"));
        server.enqueue(new MockResponse().setBody("here"));
        Response response = client().newCall(new Request.Builder().url(server.url("/start")).build()).execute();
        assertEquals("here", response.body().string());
        TestHost.Msg first = reqFor("/start");
        TestHost.Msg second = reqFor("/final");
        awaitDone(first.txn);
        awaitDone(second.txn);
        assertEquals(first.num("call"), second.num("call"));
        assertEquals(0, first.num("hop"));
        assertEquals(1, second.num("hop"));
        assertEquals(302, last(host.ofTxn(first.txn), "resp", null).num("status"));
    }

    @Test
    public void timeoutIsAFailure() throws Exception {
        server.enqueue(new MockResponse().setSocketPolicy(SocketPolicy.NO_RESPONSE));
        OkHttpClient c = client().newBuilder().readTimeout(300, TimeUnit.MILLISECONDS).retryOnConnectionFailure(false).build();
        try {
            c.newCall(new Request.Builder().url(server.url("/slow")).build()).execute();
            fail("expected a timeout");
        } catch (SocketTimeoutException expected) {
            // the app sees exactly what it would without us
        }
        TestHost.Msg req = reqFor("/slow");
        TestHost.Msg failure = awaitDone(req.txn);
        assertEquals("fail", failure.t());
        assertEquals("java.net.SocketTimeoutException", failure.obj("error").get("class"));
        assertEquals("response_headers", failure.str("phase"));
        assertEquals(Boolean.FALSE, failure.json.get("canceled"));
    }

    @Test
    public void cancelledCallIsMarked() throws Exception {
        // the server never answers, so the call is still waiting when it is cancelled
        server.enqueue(new MockResponse().setSocketPolicy(SocketPolicy.NO_RESPONSE));
        final Call call = client().newCall(new Request.Builder().url(server.url("/cancel")).build());
        new Thread(() -> {
            try {
                Thread.sleep(200);
            } catch (InterruptedException ignored) {
                // cancel anyway
            }
            call.cancel();
        }).start();
        try {
            call.execute();
            fail("expected cancellation");
        } catch (IOException expected) {
            // Canceled / Socket closed
        }
        TestHost.Msg req = reqFor("/cancel");
        TestHost.Msg failure = awaitDone(req.txn);
        assertEquals("fail", failure.t());
        assertEquals(Boolean.TRUE, failure.json.get("canceled"));
    }

    @Test
    public void connectFailureBeforeAnyExchangeStillShows() throws Exception {
        int closedPort;
        try (ServerSocket s = new ServerSocket(0)) {
            closedPort = s.getLocalPort();
        }
        try {
            client().newCall(new Request.Builder().url("http://127.0.0.1:" + closedPort + "/nobody").build()).execute();
            fail("expected a connect failure");
        } catch (IOException expected) {
            // ConnectException
        }
        TestHost.Msg req = reqFor("/nobody");
        TestHost.Msg failure = awaitDone(req.txn);
        assertEquals("fail", failure.t());
        assertEquals("connect", failure.str("phase"));
        assertEquals("call", req.obj("thread").get("origin"));
    }

    @Test
    public void enqueueRecordsTheCallersThreadNotTheDispatcher() throws Exception {
        server.enqueue(new MockResponse().setBody("async"));
        final CountDownLatch latch = new CountDownLatch(1);
        final AtomicReference<String> seen = new AtomicReference<>();
        client().newCall(new Request.Builder().url(server.url("/async")).build()).enqueue(new Callback() {
            @Override
            public void onFailure(Call call, IOException e) {
                latch.countDown();
            }

            @Override
            public void onResponse(Call call, Response response) throws IOException {
                seen.set(response.body().string());
                latch.countDown();
            }
        });
        assertTrue(latch.await(10, TimeUnit.SECONDS));
        assertEquals("async", seen.get());
        TestHost.Msg req = reqFor("/async");
        awaitDone(req.txn);
        assertEquals(Thread.currentThread().getName(), req.obj("thread").get("name"));
        assertTrue(req.list("stack").toString().contains("enqueueRecordsTheCallersThreadNotTheDispatcher"));
    }

    @Test
    public void withoutTheListenerTheInterceptorThreadIsUsed() throws Exception {
        server.enqueue(new MockResponse().setBody("x"));
        OkHttpClient c = new OkHttpClient.Builder().addNetworkInterceptor(CaptureInterceptor.INSTANCE).build();
        c.newCall(new Request.Builder().url(server.url("/nolistener")).build()).execute().body().string();
        TestHost.Msg req = reqFor("/nolistener");
        awaitDone(req.txn);
        assertEquals("interceptor", req.obj("thread").get("origin"));
        List<String> marks = markNames(host.ofTxn(req.txn), req);
        assertTrue(marks.toString(), marks.contains("resp_body_start"));
    }

    @Test
    public void theAppsOwnListenerStillHearsEverything() throws Exception {
        server.enqueue(new MockResponse().setBody("x"));
        final AtomicInteger starts = new AtomicInteger();
        final AtomicInteger ends = new AtomicInteger();
        final AtomicInteger bodies = new AtomicInteger();
        EventListener app = new EventListener() {
            @Override
            public void callStart(Call call) {
                starts.incrementAndGet();
            }

            @Override
            public void responseBodyEnd(Call call, long byteCount) {
                bodies.incrementAndGet();
            }

            @Override
            public void callEnd(Call call) {
                ends.incrementAndGet();
            }
        };
        final EventListener appListener = app;
        client(call -> appListener).newCall(new Request.Builder().url(server.url("/both")).build()).execute().body().string();
        TestHost.Msg req = reqFor("/both");
        awaitDone(req.txn);
        assertEquals(1, starts.get());
        assertEquals(1, bodies.get());
        assertEquals(1, ends.get());
        assertEquals("call", req.obj("thread").get("origin"));
    }

    @Test
    public void headAndNoContentHaveNoBody() throws Exception {
        server.enqueue(new MockResponse().setResponseCode(204));
        server.enqueue(new MockResponse().addHeader("Content-Length", "123"));
        client().newCall(new Request.Builder().url(server.url("/nocontent")).build()).execute().close();
        client().newCall(new Request.Builder().url(server.url("/head")).head().build()).execute().close();
        for (String path : new String[] {"/nocontent", "/head"}) {
            TestHost.Msg req = reqFor(path);
            assertEquals("done", awaitDone(req.txn).t());
            assertEquals("none", last(host.ofTxn(req.txn), "body_end", "response").str("state"));
        }
    }

    @Test
    public void closingWithoutReadingEndsEarly() throws Exception {
        server.enqueue(new MockResponse().setBody("never read"));
        Response response = client().newCall(new Request.Builder().url(server.url("/unread")).build()).execute();
        response.close();
        TestHost.Msg req = reqFor("/unread");
        assertEquals("done", awaitDone(req.txn).t());
        TestHost.Msg end = last(host.ofTxn(req.txn), "body_end", "response");
        assertEquals("closed_early", end.str("state"));
        assertEquals(0, end.num("bytes"));
    }

    @Test
    public void oneShotBodiesAreWrittenOnce() throws Exception {
        boolean supported;
        try {
            RequestBody.class.getMethod("isOneShot");
            supported = true;
        } catch (NoSuchMethodException e) {
            supported = false;
        }
        org.junit.Assume.assumeTrue("isOneShot exists from OkHttp 3.14", supported);
        server.enqueue(new MockResponse().setBody("ok"));
        final AtomicInteger writes = new AtomicInteger();
        RequestBody oneShot = new RequestBody() {
            @Override
            public MediaType contentType() {
                return MediaType.parse("text/plain");
            }

            @Override
            public void writeTo(okio.BufferedSink sink) throws IOException {
                if (writes.incrementAndGet() > 1) {
                    throw new IllegalStateException("written twice");
                }
                sink.writeUtf8("once");
            }

            // no @Override: the method exists only from OkHttp 3.14 (the test is skipped below that)
            public boolean isOneShot() {
                return true;
            }
        };
        client().newCall(new Request.Builder().url(server.url("/oneshot")).post(oneShot).build()).execute().body().string();
        assertEquals(1, writes.get());
        TestHost.Msg req = reqFor("/oneshot");
        awaitDone(req.txn);
        assertEquals(Boolean.TRUE, req.obj("body").get("one_shot"));
        assertEquals("once", new String(bodyOf(host.ofTxn(req.txn), 0), UTF_8));
    }

    @Test
    public void pausedRuntimePassesThrough() throws Exception {
        host.send("{\"t\":\"set_config\",\"id\":2,\"config\":{\"recording\":false}}");
        host.awaitType("config_ack", 5000);
        server.enqueue(new MockResponse().setBody("quiet"));
        assertEquals("quiet", client().newCall(new Request.Builder().url(server.url("/paused")).build()).execute().body().string());
        Thread.sleep(200);
        for (TestHost.Msg m : host.received()) {
            assertFalse("no transaction while paused: " + m, "req".equals(m.t()));
        }
    }

    @Test
    public void noRuntimeMeansNoCapture() throws Exception {
        rt.stop();
        server.enqueue(new MockResponse().setBody("plain"));
        Response r = client().newCall(new Request.Builder().url(server.url("/off")).build()).execute();
        assertEquals("plain", r.body().string());
        // restart for tearDown
        rt = TestPlatform.runtime(platform, true);
    }

    @Test
    public void largeBodyIsCappedButCounted() throws Exception {
        host.send("{\"t\":\"set_config\",\"id\":4,\"config\":{\"body_cap\":1000}}");
        host.awaitType("config_ack", 5000);
        byte[] big = new byte[50_000];
        for (int i = 0; i < big.length; i++) {
            big[i] = (byte) ('a' + i % 26);
        }
        server.enqueue(new MockResponse().setBody(new Buffer().write(big)));
        InputStream in = client().newCall(new Request.Builder().url(server.url("/big")).build()).execute().body().byteStream();
        byte[] buf = new byte[4096];
        long total = 0;
        for (int n; (n = in.read(buf)) != -1; ) {
            total += n;
        }
        in.close();
        assertEquals(big.length, total);
        TestHost.Msg req = reqFor("/big");
        awaitDone(req.txn);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        TestHost.Msg end = last(msgs, "body_end", "response");
        assertEquals("truncated", end.str("state"));
        assertEquals(big.length, end.num("bytes"));
        assertEquals(1000, end.num("captured"));
        assertEquals(1000, bodyOf(msgs, 1).length);
        assertNotNull(last(msgs, "prog", "response"));
    }
}
