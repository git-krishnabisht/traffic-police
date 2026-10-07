package io.trafficpolice.capture.okhttp;

import static org.junit.Assert.assertTrue;

import com.android.tools.appinspection.network.okhttp.OkHttp3Interceptor;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import java.util.concurrent.TimeUnit;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.mockwebserver.MockResponse;
import okhttp3.mockwebserver.MockWebServer;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/** Android Studio's network interceptor in the same chain is reported, before or after ours. */
public final class StudioDetectorTest {
    private final TestPlatform platform = new TestPlatform(false);
    private CaptureRuntime rt;
    private TestHost host;
    private MockWebServer server;

    @Before
    public void setUp() throws Exception {
        StudioDetector.reset();
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
        StudioDetector.reset();
    }

    private void call(OkHttpClient client) throws Exception {
        server.enqueue(new MockResponse().setBody("ok"));
        try (Response r = client.newCall(new Request.Builder().url(server.url("/")).build()).execute()) {
            r.body().string();
        }
    }

    private void assertReported() throws Exception {
        TestHost.Msg diag = host.await(m -> "diag".equals(m.t()) && "studio_inspector_present".equals(m.str("code")), 5_000);
        assertTrue(diag.str("message"), diag.str("message").contains("Android Studio"));
    }

    @Test
    public void studiosInterceptorBeforeOursIsFoundWithoutTheListener() throws Exception {
        OkHttpClient client = new OkHttpClient.Builder()
                .addNetworkInterceptor(new OkHttp3Interceptor())
                .addNetworkInterceptor(CaptureInterceptor.INSTANCE)
                .readTimeout(5, TimeUnit.SECONDS)
                .build();
        call(client);
        assertReported();
    }

    @Test
    public void studiosInterceptorAfterOursIsFoundByTheListener() throws Exception {
        OkHttpClient client = new OkHttpClient.Builder()
                .addNetworkInterceptor(CaptureInterceptor.INSTANCE)
                .addNetworkInterceptor(new OkHttp3Interceptor())
                .eventListenerFactory(new ListenerFactory(null))
                .readTimeout(5, TimeUnit.SECONDS)
                .build();
        call(client);
        assertReported();
    }

    @Test
    public void nothingIsReportedWithoutIt() throws Exception {
        OkHttpClient client = new OkHttpClient.Builder()
                .addNetworkInterceptor(CaptureInterceptor.INSTANCE)
                .eventListenerFactory(new ListenerFactory(null))
                .readTimeout(5, TimeUnit.SECONDS)
                .build();
        for (int i = 0; i < 10; i++) call(client);
        // a diag would come with the events of the first request; a later one makes sure they arrived
        rt.diag("info", "marker", "end of test", null);
        TestHost.Msg next = host.await(m -> "diag".equals(m.t())
                && ("marker".equals(m.str("code")) || "studio_inspector_present".equals(m.str("code"))), 5_000);
        assertTrue(next.json.toString(), "marker".equals(next.str("code")));
    }
}
