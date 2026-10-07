package io.trafficpolice.capture.okhttp;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertSame;
import static org.junit.Assert.assertTrue;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.WebSocket;
import okhttp3.WebSocketListener;
import okhttp3.mockwebserver.MockResponse;
import okhttp3.mockwebserver.MockWebServer;
import okio.ByteString;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/**
 * WebSockets (PROTOCOL.md §7.1 {@code ws}): the handshake and every message, in library mode
 * ({@code TrafficPolice.newWebSocket}) and attach mode (the {@code newWebSocket} exit hook), on
 * every OkHttp version in the build's matrix.
 */
public final class WebSocketCaptureTest {
    private final TestPlatform platform = new TestPlatform(false);
    private CaptureRuntime rt;
    private TestHost host;
    private MockWebServer server;
    private final OkHttpClient client = new OkHttpClient.Builder().readTimeout(5, TimeUnit.SECONDS).build();

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

    /** The server: to each text message, three binary bytes and an echo; then a close. */
    private void serveEcho() {
        server.enqueue(new MockResponse().withWebSocketUpgrade(new WebSocketListener() {
            @Override
            public void onMessage(WebSocket webSocket, String text) {
                webSocket.send(ByteString.of((byte) 1, (byte) 2, (byte) 3));
                webSocket.send("echo: " + text);
            }

            @Override
            public void onClosing(WebSocket webSocket, int code, String reason) {
                webSocket.close(1000, "bye");
            }
        }));
    }

    /** The app's side: says hello once open, closes after the echo; records what it got. */
    private static final class App extends WebSocketListener {
        final CountDownLatch closed = new CountDownLatch(1);
        final List<String> got = new ArrayList<>();
        final AtomicReference<WebSocket> openedWith = new AtomicReference<>();
        volatile Throwable failure;
        volatile Response failedResponse;

        @Override
        public void onOpen(WebSocket webSocket, Response response) {
            openedWith.set(webSocket);
            webSocket.send("hello");
        }

        @Override
        public void onMessage(WebSocket webSocket, String text) {
            got.add(text);
            webSocket.close(1000, "done");
        }

        @Override
        public void onMessage(WebSocket webSocket, ByteString bytes) {
            got.add(bytes.hex());
        }

        @Override
        public void onClosed(WebSocket webSocket, int code, String reason) {
            closed.countDown();
        }

        @Override
        public void onFailure(WebSocket webSocket, Throwable t, Response response) {
            failure = t;
            failedResponse = response;
            closed.countDown();
        }
    }

    private Request request() {
        return new Request.Builder().url(server.url("/live")).header("X-App", "1").build();
    }

    private void checkSocket(WebSocket returned, App app) throws Exception {
        assertTrue("the socket closed", app.closed.await(10, TimeUnit.SECONDS));
        assertNull(String.valueOf(app.failure), app.failure);
        assertEquals(Arrays.asList("010203", "echo: hello"), app.got);
        // the app's listener got the socket the app holds, so what it sends there is captured too
        assertSame(returned, app.openedWith.get());

        TestHost.Msg req = host.await(m -> "req".equals(m.t()) && m.str("url").endsWith("/live"), 5_000);
        host.await(m -> ("done".equals(m.t()) || "fail".equals(m.t())) && m.txn == req.txn, 5_000);
        List<TestHost.Msg> msgs = host.ofTxn(req.txn);
        assertEquals("GET", req.str("method"));
        assertTrue(req.list("headers").toString(), req.list("headers").contains(Arrays.asList("X-App", "1")));
        // the call site: the app's code that opened the socket
        Object top = ((java.util.Map<?, ?>) req.list("stack").get(0)).get("c");
        assertTrue(req.list("stack").toString(), String.valueOf(top).startsWith("com.example.app.SocketCaller")
                || String.valueOf(top).startsWith("okhttp3."));
        assertTrue(req.list("stack").toString(), req.list("stack").toString().contains("com.example.app.SocketCaller"));
        TestHost.Msg resp = null;
        List<String> ws = new ArrayList<>();
        for (TestHost.Msg m : msgs) {
            if ("resp".equals(m.t())) resp = m;
            if ("ws".equals(m.t())) {
                String what = m.str("dir") + " " + m.str("op");
                if (m.str("text") != null) what += " " + m.str("text");
                if (m.str("base64") != null) what += " " + m.str("base64");
                if (m.json.get("code") != null) what += " " + m.num("code");
                ws.add(what);
            }
            assertFalse("done comes last", "fail".equals(m.t()));
        }
        assertEquals(101, resp.num("status"));
        assertEquals(Arrays.asList("out text hello", "in binary AQID", "in text echo: hello", "out close 1000",
                "in close 1000"), ws);
        assertEquals("done", msgs.get(msgs.size() - 1).t());
    }

    @Test
    public void libraryModeCapturesTheHandshakeAndEveryMessage() throws Exception {
        serveEcho();
        App app = new App();
        WebSocket ws = com.example.app.SocketCaller.library(client, request(), app);
        checkSocket(ws, app);
    }

    @Test
    public void attachModeWrapsTheSocketNewWebSocketReturned() throws Exception {
        serveEcho();
        App app = new App();
        // what the exit hook of OkHttpClient.newWebSocket does with the socket it returns
        WebSocket ws = com.example.app.SocketCaller.attached(client, request(), app);
        checkSocket(ws, app);
    }

    @Test
    public void aRefusedHandshakeIsAFailedRequestWithItsResponse() throws Exception {
        server.enqueue(new MockResponse().setResponseCode(403).setBody("no"));
        App app = new App();
        WebSockets.newWebSocket(client, request(), app);
        assertTrue(app.closed.await(10, TimeUnit.SECONDS));
        assertEquals(403, app.failedResponse.code());
        TestHost.Msg req = host.await(m -> "req".equals(m.t()) && m.str("url").endsWith("/live"), 5_000);
        TestHost.Msg fail = host.await(m -> "fail".equals(m.t()) && m.txn == req.txn, 5_000);
        assertEquals("response_headers", fail.str("phase"));
        TestHost.Msg resp = host.await(m -> "resp".equals(m.t()) && m.txn == req.txn, 5_000);
        assertEquals(403, resp.num("status"));
    }
}
