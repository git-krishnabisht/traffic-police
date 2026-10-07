package io.trafficpolice.capture.grpc;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertSame;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import io.grpc.CallOptions;
import io.grpc.Channel;
import io.grpc.ClientCall;
import io.grpc.ClientInterceptor;
import io.grpc.ClientInterceptors;
import io.grpc.ForwardingClientCall;
import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.MethodDescriptor;
import io.grpc.Server;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;
import io.grpc.inprocess.InProcessChannelBuilder;
import io.grpc.inprocess.InProcessServerBuilder;
import io.grpc.stub.ClientCalls;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import java.io.ByteArrayOutputStream;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/**
 * gRPC capture (ARCHITECTURE.md §4.9) through gRPC's in-process transport, on every gRPC version
 * in the build's matrix: what each call sends to the host, in library mode and through the attach
 * hooks.
 */
public final class GrpcCaptureTest {
    private final TestPlatform platform = new TestPlatform(false);
    private CaptureRuntime rt;
    private TestHost host;
    private Server server;
    private ManagedChannel channel;
    private final String name = "echo-" + System.nanoTime();

    /** An app interceptor added after ours, so it runs before it: ours sees its header. */
    private static final ClientInterceptor APP = new ClientInterceptor() {
        @Override
        public <Q, R> ClientCall<Q, R> interceptCall(MethodDescriptor<Q, R> method, CallOptions options, Channel next) {
            return new ForwardingClientCall.SimpleForwardingClientCall<Q, R>(next.newCall(method, options)) {
                @Override
                public void start(Listener<R> listener, Metadata headers) {
                    headers.put(Metadata.Key.of("x-app", Metadata.ASCII_STRING_MARSHALLER), "1");
                    super.start(listener, headers);
                }
            };
        }
    };

    @Before
    public void setUp() throws Exception {
        rt = TestPlatform.runtime(platform, false);
        host = TestHost.connect(rt);
        server = InProcessServerBuilder.forName(name).directExecutor().addService(EchoService.definition()).build().start();
        channel = InProcessChannelBuilder.forName(name).directExecutor()
                .intercept(CaptureClientInterceptor.INSTANCE, APP)
                .build();
    }

    @After
    public void tearDown() throws Exception {
        channel.shutdownNow();
        server.shutdownNow();
        host.close();
        rt.stop();
    }

    private TestHost.Msg request(String path) throws InterruptedException {
        return host.await(m -> "req".equals(m.t()) && m.str("url").endsWith(path), 5_000);
    }

    private TestHost.Msg end(TestHost.Msg req) throws InterruptedException {
        return host.await(m -> ("done".equals(m.t()) || "fail".equals(m.t())) && m.txn == req.txn, 5_000);
    }

    private byte[] body(long txn, int dir) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (TestHost.Msg m : host.ofTxn(txn)) {
            if (m.data != null && m.dir == dir) {
                out.write(m.data, 0, m.data.length);
            }
        }
        return out.toByteArray();
    }

    private static List<List<Object>> pairs(List<Object> raw) {
        List<List<Object>> out = new ArrayList<>();
        for (Object o : raw) {
            @SuppressWarnings("unchecked")
            List<Object> p = (List<Object>) o;
            out.add(p);
        }
        return out;
    }

    private static List<Object> pair(String n, String v) {
        return Arrays.<Object>asList(n, v);
    }

    private static byte[] concat(byte[]... parts) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (byte[] p : parts) {
            out.write(p, 0, p.length);
        }
        return out.toByteArray();
    }

    @Test
    public void aUnaryCallIsOneTransaction() throws Exception {
        assertEquals("hello ana", ClientCalls.blockingUnaryCall(channel, EchoService.SAY, CallOptions.DEFAULT, "ana"));
        TestHost.Msg req = request("/test.v1.Echo/Say");
        TestHost.Msg done = end(req);
        assertEquals("done", done.t());
        assertEquals("POST", req.str("method"));
        List<List<Object>> headers = pairs(req.list("headers"));
        assertTrue(headers.toString(), headers.contains(pair("x-app", "1")));
        assertTrue(headers.toString(), headers.contains(pair("content-type", "application/grpc")));
        assertTrue(headers.toString(), headers.contains(pair("te", "trailers")));
        Map<String, Object> client = req.obj("client");
        assertEquals("grpc", client.get("kind"));
        assertEquals(System.getProperty("trafficpolice.grpc"), client.get("version"));
        assertEquals("application/grpc", req.obj("body").get("type"));
        assertTrue(req.list("stack").toString(), req.list("stack").toString().contains(GrpcCaptureTest.class.getName()));

        assertArrayEquals(EchoService.frame("ana"), body(req.txn, 0));
        assertArrayEquals(EchoService.frame("hello ana"), body(req.txn, 1));
        TestHost.Msg resp = host.await(m -> "resp".equals(m.t()) && m.txn == req.txn, 5_000);
        assertEquals(200, resp.num("status"));
        assertEquals(Collections.singletonList(pair("grpc-status", "0")), pairs(done.list("trailers")));
        Map<String, Object> grpc = done.obj("grpc");
        assertEquals(0L, ((Number) grpc.get("code")).longValue());
        assertEquals("OK", grpc.get("status"));
    }

    @Test
    public void aServerErrorIsDoneWithItsStatusAndTrailers() throws Exception {
        try {
            ClientCalls.blockingUnaryCall(channel, EchoService.FAIL, CallOptions.DEFAULT, "sku_1");
            fail("NOT_FOUND expected");
        } catch (StatusRuntimeException e) {
            assertEquals(Status.Code.NOT_FOUND, e.getStatus().getCode());
        }
        TestHost.Msg req = request("/test.v1.Echo/Fail");
        TestHost.Msg done = end(req);
        assertEquals("the server answered: done, with its status", "done", done.t());
        List<List<Object>> trailers = pairs(done.list("trailers"));
        assertEquals(pair("grpc-status", "5"), trailers.get(0));
        assertEquals(pair("grpc-message", "no such thing: sku_1"), trailers.get(1));
        assertTrue(trailers.toString(), trailers.contains(pair("x-detail", "no sku_1")));
        assertTrue("binary values in base64, as HTTP/2 carries them", trailers.contains(pair("x-trace-bin", "AQID")));
        Map<String, Object> grpc = done.obj("grpc");
        assertEquals(5L, ((Number) grpc.get("code")).longValue());
        assertEquals("NOT_FOUND", grpc.get("status"));
        assertEquals("no such thing: sku_1", grpc.get("message"));
        // trailers only: a response without messages
        TestHost.Msg resp = host.await(m -> "resp".equals(m.t()) && m.txn == req.txn, 5_000);
        assertEquals(200, resp.num("status"));
        assertEquals(0, body(req.txn, 1).length);
    }

    @Test
    public void aServerStreamHasEveryMessageInItsBody() throws Exception {
        Iterator<String> it = ClientCalls.blockingServerStreamingCall(channel, EchoService.COUNT, CallOptions.DEFAULT, "n");
        List<String> got = new ArrayList<>();
        while (it.hasNext()) {
            got.add(it.next());
        }
        assertEquals(Arrays.asList("n 1", "n 2", "n 3"), got);
        TestHost.Msg req = request("/test.v1.Echo/Count");
        assertEquals("done", end(req).t());
        assertArrayEquals(concat(EchoService.frame("n 1"), EchoService.frame("n 2"), EchoService.frame("n 3")),
                body(req.txn, 1));
    }

    @Test
    public void aDeadlineIsAFailureTheClientMade() throws Exception {
        try {
            ClientCalls.blockingUnaryCall(channel, EchoService.SLOW,
                    CallOptions.DEFAULT.withDeadlineAfter(300, TimeUnit.MILLISECONDS), "x");
            fail("DEADLINE_EXCEEDED expected");
        } catch (StatusRuntimeException e) {
            assertEquals(Status.Code.DEADLINE_EXCEEDED, e.getStatus().getCode());
        }
        TestHost.Msg req = request("/test.v1.Echo/Slow");
        TestHost.Msg fail = end(req);
        assertEquals("fail", fail.t());
        assertFalse(fail.json.get("canceled").equals(Boolean.TRUE));
        assertEquals("io.grpc.StatusRuntimeException", fail.obj("error").get("class"));
        assertEquals("DEADLINE_EXCEEDED", fail.obj("grpc").get("status"));
        String timeout = null;
        for (List<Object> h : pairs(req.list("headers"))) {
            if ("grpc-timeout".equals(h.get(0))) {
                timeout = (String) h.get(1);
            }
        }
        assertNotNull("the deadline goes as grpc-timeout", timeout);
        assertTrue(timeout, timeout.matches("\\d{1,8}[numSMH]"));
    }

    @Test
    public void aCancelIsACanceledFailure() throws Exception {
        ClientCall<String, String> call = channel.newCall(EchoService.SLOW, CallOptions.DEFAULT);
        final java.util.concurrent.CountDownLatch closed = new java.util.concurrent.CountDownLatch(1);
        call.start(new ClientCall.Listener<String>() {
            @Override
            public void onClose(Status status, Metadata trailers) {
                closed.countDown();
            }
        }, new Metadata());
        call.sendMessage("x");
        call.halfClose();
        call.cancel("the user left", null);
        assertTrue(closed.await(5, TimeUnit.SECONDS));
        TestHost.Msg fail = end(request("/test.v1.Echo/Slow"));
        assertEquals("fail", fail.t());
        assertEquals(Boolean.TRUE, fail.json.get("canceled"));
        assertEquals("CANCELLED", fail.obj("grpc").get("status"));
    }

    @Test
    public void twoOfOursOnOneCallCaptureItOnce() throws Exception {
        // a stub's channel wrapped by the attach hook, on a channel that has ours already
        Channel twice = GrpcHooks.stubChannel(channel);
        assertEquals("hello bo", ClientCalls.blockingUnaryCall(twice, EchoService.SAY, CallOptions.DEFAULT, "bo"));
        TestHost.Msg req = request("/test.v1.Echo/Say");
        end(req);
        TestPlatform.awaitWriter(rt);
        Thread.sleep(200);
        int reqs = 0;
        for (TestHost.Msg m : host.received()) {
            if ("req".equals(m.t())) {
                reqs++;
            }
        }
        assertEquals(1, reqs);
        // the inner one captured: it sees the app's header
        assertTrue(pairs(req.list("headers")).contains(pair("x-app", "1")));
    }

    @Test
    public void aChannelWithoutOursIsCapturedThroughTheStubHook() throws Exception {
        ManagedChannel plain = InProcessChannelBuilder.forName(name).directExecutor().build();
        try {
            Channel hooked = GrpcHooks.stubChannel(plain);
            assertEquals("hello cy", ClientCalls.blockingUnaryCall(hooked, EchoService.SAY, CallOptions.DEFAULT, "cy"));
            assertEquals("done", end(request("/test.v1.Echo/Say")).t());
        } finally {
            plain.shutdownNow();
        }
    }

    @Test
    public void theBuilderHookPutsOursFirstOnce() {
        List<ClientInterceptor> original = new ArrayList<>(Collections.singletonList(APP));
        List<ClientInterceptor> hooked = GrpcHooks.effectiveInterceptors(original);
        assertEquals(Arrays.asList(CaptureClientInterceptor.INSTANCE, APP), hooked);
        assertSame("ours is there already", hooked, GrpcHooks.effectiveInterceptors(hooked));
        assertEquals(Collections.singletonList(CaptureClientInterceptor.INSTANCE), GrpcHooks.effectiveInterceptors(null));
    }

    @Test
    public void pausedRecordsNothingAndPassesThrough() throws Exception {
        host.send("{\"t\":\"set_config\",\"id\":5,\"config\":{\"recording\":false}}");
        host.await(m -> "config_ack".equals(m.t()), 5_000);
        assertEquals("hello di", ClientCalls.blockingUnaryCall(channel, EchoService.SAY, CallOptions.DEFAULT, "di"));
        TestPlatform.awaitWriter(rt);
        Thread.sleep(200);
        for (TestHost.Msg m : host.received()) {
            assertFalse(m.toString(), "req".equals(m.t()));
        }
    }

    @Test
    public void messagesOverTheCapAreCountedNotCut() throws Exception {
        host.send("{\"t\":\"set_config\",\"id\":6,\"config\":{\"body_cap\":20}}");
        host.await(m -> "config_ack".equals(m.t()), 5_000);
        Iterator<String> it = ClientCalls.blockingServerStreamingCall(channel, EchoService.COUNT, CallOptions.DEFAULT,
                "abcdef");
        while (it.hasNext()) {
            it.next();
        }
        TestHost.Msg req = request("/test.v1.Echo/Count");
        end(req);
        // three 13-byte frames: the first fits in 20 bytes; the second would be cut, so it is counted only
        assertArrayEquals(EchoService.frame("abcdef 1"), body(req.txn, 1));
        TestHost.Msg bodyEnd = host.await(m -> "body_end".equals(m.t()) && m.txn == req.txn
                && "response".equals(m.str("dir")), 5_000);
        assertEquals(39, bodyEnd.num("bytes"));
        assertEquals("truncated", bodyEnd.str("state"));
    }

    @Test
    public void timeoutsAreWrittenAsGrpcDoes() {
        assertEquals("99999999n", GrpcCall.State.timeout(99_999_999L));
        assertEquals("100000u", GrpcCall.State.timeout(100_000_000L));
        assertEquals("4999000u", GrpcCall.State.timeout(4_999_000_000L));
        assertEquals("100000m", GrpcCall.State.timeout(100_000_000_000L));
        assertEquals("AQID", GrpcCall.State.base64(new byte[] {1, 2, 3}));
        assertEquals("AQ", GrpcCall.State.base64(new byte[] {1}));
    }
}
