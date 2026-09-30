package io.trafficpolice.capture.okhttp;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertSame;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import okhttp3.Call;
import okhttp3.EventListener;
import okhttp3.Interceptor;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.mockwebserver.MockResponse;
import okhttp3.mockwebserver.MockWebServer;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/**
 * What attach mode's OkHttp hooks return (ARCHITECTURE.md §4.7.3), on every OkHttp in the matrix:
 * ours once, first, with the app's kept, and a client built from them captures its calls.
 */
public final class OkHttpHooksTest {
    private CaptureRuntime rt;
    private TestHost host;
    private MockWebServer server;

    /** An app interceptor that marks the requests it sees. */
    static final Interceptor APP = new Interceptor() {
        @Override
        public Response intercept(Chain chain) throws java.io.IOException {
            return chain.proceed(chain.request().newBuilder().header("X-App", "1").build());
        }
    };

    @Before
    public void setUp() throws Exception {
        rt = TestPlatform.runtime(new TestPlatform(false), true);
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

    @Test
    public void network_interceptors_get_ours_first_once() {
        OkHttpClient app = new OkHttpClient.Builder().addNetworkInterceptor(APP).build();
        List<Interceptor> hooked = OkHttpHooks.networkInterceptors(app.networkInterceptors());
        assertEquals(2, hooked.size());
        assertSame(CaptureInterceptor.INSTANCE, hooked.get(0));
        assertSame(APP, hooked.get(1));
        assertSame("a list with ours is left alone", hooked, OkHttpHooks.networkInterceptors(hooked));
        try {
            hooked.add(APP);
            fail("the list stays unmodifiable, as OkHttp's is");
        } catch (UnsupportedOperationException expected) {
            // as expected
        }
        List<Interceptor> none = OkHttpHooks.networkInterceptors(new OkHttpClient().networkInterceptors());
        assertEquals(1, none.size());
    }

    @Test
    public void the_listener_factory_is_wrapped_once() {
        final AtomicInteger created = new AtomicInteger();
        EventListener.Factory appFactory = new EventListener.Factory() {
            @Override
            public EventListener create(Call call) {
                created.incrementAndGet();
                return EventListener.NONE;
            }
        };
        EventListener.Factory hooked = OkHttpHooks.eventListenerFactory(appFactory);
        assertTrue(hooked instanceof ListenerFactory);
        assertSame(hooked, OkHttpHooks.eventListenerFactory(hooked));
        assertNotNull(OkHttpHooks.eventListenerFactory(null));
    }

    @Test
    public void a_client_built_from_the_hooked_values_is_captured_and_keeps_the_apps() throws Exception {
        final AtomicInteger created = new AtomicInteger();
        OkHttpClient app = new OkHttpClient.Builder()
                .addNetworkInterceptor(APP)
                .eventListenerFactory(new EventListener.Factory() {
                    @Override
                    public EventListener create(Call call) {
                        created.incrementAndGet();
                        return EventListener.NONE;
                    }
                })
                .build();
        // what RealCall sees once the getters are hooked
        OkHttpClient.Builder b = new OkHttpClient.Builder();
        for (Interceptor i : OkHttpHooks.networkInterceptors(app.networkInterceptors())) {
            b.addNetworkInterceptor(i);
        }
        OkHttpClient hooked = b.eventListenerFactory(OkHttpHooks.eventListenerFactory(app.eventListenerFactory())).build();
        server.enqueue(new MockResponse().setBody("ok"));
        try (Response r = hooked.newCall(new Request.Builder().url("http://127.0.0.1:" + server.getPort() + "/attached").build())
                .execute()) {
            assertEquals("ok", r.body().string());
        }
        assertEquals("the app's interceptor still ran", "1", server.takeRequest().getHeader("X-App"));
        assertEquals("the app's listener still ran", 1, created.get());
        TestHost.Msg req = host.await(m -> "req".equals(m.t()) && m.str("url").endsWith("/attached"), 10_000);
        assertNotNull("captured", req);
        TestHost.Msg done = host.await(m -> "done".equals(m.t()) && m.txn == req.txn, 10_000);
        assertNotNull(done);
    }
}
