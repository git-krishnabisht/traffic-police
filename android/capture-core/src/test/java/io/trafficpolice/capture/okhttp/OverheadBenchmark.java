package io.trafficpolice.capture.okhttp;

import static org.junit.Assert.assertTrue;
import static org.junit.Assume.assumeTrue;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.ConnInfo;
import io.trafficpolice.capture.core.Recorder;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import io.trafficpolice.capture.core.ThreadStack;
import io.trafficpolice.capture.core.Txn;
import java.util.Arrays;
import java.util.concurrent.TimeUnit;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.mockwebserver.Dispatcher;
import okhttp3.mockwebserver.MockResponse;
import okhttp3.mockwebserver.MockWebServer;
import okhttp3.mockwebserver.RecordedRequest;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/**
 * What capture costs the app per request on the JVM (ARCHITECTURE.md §6: under 1 ms added,
 * under 150 µs typical). Opt-in: {@code ./gradlew :capture-core:benchmarkOverhead}. The device
 * measurement is the sample app's "overhead" scenario.
 */
public final class OverheadBenchmark {
    private final TestPlatform platform = new TestPlatform(false);
    private CaptureRuntime rt;
    private TestHost host;
    private MockWebServer server;

    @Before
    public void setUp() throws Exception {
        assumeTrue("run with ./gradlew :capture-core:benchmarkOverhead", Boolean.getBoolean("trafficpolice.bench"));
        rt = TestPlatform.runtime(platform, true);
        host = TestHost.connect(rt);
        server = new MockWebServer();
        final String json = json(2048);
        server.setDispatcher(new Dispatcher() {
            @Override
            public MockResponse dispatch(RecordedRequest request) {
                return new MockResponse().setHeader("Content-Type", "application/json").setBody(json);
            }
        });
        server.start();
    }

    @After
    public void tearDown() throws Exception {
        if (rt == null) {
            return;
        }
        host.close();
        server.shutdown();
        rt.stop();
    }

    /** The hooks alone: everything a request costs the calling thread, without any network. */
    @Test
    public void hookCostPerRequest() throws Exception {
        Recorder rec = rt.recorder();
        String[] reqHeaders = {"Host", "api.example.app", "Accept-Encoding", "gzip", "User-Agent", "okhttp/4.12.0",
                "Authorization", "Bearer eyJhbGciOiJIUzI1NiJ9.e30.ZRrHA1JJJW8opsbCGfG_HACGpVUMN_a9IV7pAx_Zmeo"};
        String[] respHeaders = {"content-type", "application/json; charset=utf-8", "content-length", "2048",
                "cache-control", "no-cache", "x-request-id", "0b8d3a8e-2f1c-4e0e-9f4c-6a7f4d1b2c3e", "date",
                "Tue, 30 Sep 2026 06:00:00 GMT", "server", "nginx"};
        ConnInfo conn = new ConnInfo("c-1", true, "h2", "142.250.183.14", 443, "DIRECT", "TLSv1.3", "TLS_AES_128_GCM_SHA256", null);
        byte[] body = json(2048).getBytes("UTF-8");
        int warmup = 20_000;
        int n = 20_000;
        long[] costs = new long[n];
        for (int i = -warmup; i < n; i++) {
            long t0 = System.nanoTime();
            Recorder.RequestInfo r = new Recorder.RequestInfo();
            r.call = rec.newCallId();
            r.method = "GET";
            r.url = "https://api.example.app/v1/status?id=" + i;
            r.headers = reqHeaders;
            r.clientKind = "okhttp";
            r.clientVersion = "4.12.0";
            r.stack = ThreadStack.capture(ThreadStack.ORIGIN_CALL, 64);
            r.markNames = new String[] {"call_start", "dns_start", "dns_end", "connect_start", "connect_end", "conn_acquired"};
            r.markTimes = new long[] {t0, t0, t0, t0, t0, t0};
            r.conn = conn;
            Txn t = rec.start(r);
            t.mark("req_headers_start");
            t.mark("req_headers_end");
            t.request.end("none");
            t.mark("resp_headers_start");
            t.response(200, "OK", "h2", respHeaders, conn);
            t.mark("resp_headers_end");
            t.response.write(body, 0, 1024);
            t.response.write(body, 1024, 1024);
            t.response.end("complete");
            t.done();
            long dt = System.nanoTime() - t0;
            if (i >= 0) {
                costs[i] = dt;
            }
            if ((i & 255) == 255) {
                drain();
            }
        }
        report("hooks, one 2 KB GET (stack depth 64, 8 marks)", costs);
        Arrays.sort(costs);
        assertTrue("median hook cost under 1 ms", costs[n / 2] < 1_000_000);
    }

    /** OkHttp against a loopback server, the same requests with and without capture. */
    @Test
    public void okhttpLatencyWithAndWithoutCapture() throws Exception {
        OkHttpClient plain = new OkHttpClient.Builder().readTimeout(5, TimeUnit.SECONDS).build();
        OkHttpClient captured = plain.newBuilder()
                .addNetworkInterceptor(CaptureInterceptor.INSTANCE)
                .eventListenerFactory(new ListenerFactory(null))
                .build();
        int warmup = 2_000;
        int n = 5_000;
        long[] a = new long[n];
        long[] b = new long[n];
        for (int i = -warmup; i < n; i++) {
            long pa = time(plain, i);
            long pb = time(captured, i);
            if (i >= 0) {
                a[i] = pa;
                b[i] = pb;
            }
            if ((i & 255) == 255) {
                drain();
            }
        }
        report("OkHttp GET 2 KB, without capture", a);
        report("OkHttp GET 2 KB, with capture", b);
        long[] sa = a.clone();
        long[] sb = b.clone();
        Arrays.sort(sa);
        Arrays.sort(sb);
        System.out.printf("  added at the median: %.1f µs, at p90: %.1f µs%n",
                (sb[n / 2] - sa[n / 2]) / 1e3, (sb[n * 9 / 10] - sa[n * 9 / 10]) / 1e3);
        assertTrue("capture adds under 1 ms at the median", sb[n / 2] - sa[n / 2] < 1_000_000);
    }

    private long time(OkHttpClient client, int i) throws Exception {
        long t0 = System.nanoTime();
        Request request = new Request.Builder().url(server.url("/v1/status?id=" + i)).build();
        Response response = client.newCall(request).execute();
        try {
            response.body().bytes();
        } finally {
            response.close();
        }
        return System.nanoTime() - t0;
    }

    /** Lets the writer catch up, so the queue never overflows (dropping would be cheaper). */
    private void drain() throws InterruptedException {
        TestPlatform.awaitWriter(rt);
        host.clearReceived();
    }

    private static void report(String what, long[] ns) {
        long[] s = ns.clone();
        Arrays.sort(s);
        double mean = 0;
        for (long v : s) {
            mean += v;
        }
        mean /= s.length;
        System.out.printf("%-52s median %8.1f µs  p90 %8.1f µs  p99 %8.1f µs  mean %8.1f µs  (n=%d)%n", what,
                s[s.length / 2] / 1e3, s[s.length * 9 / 10] / 1e3, s[s.length * 99 / 100] / 1e3, mean / 1e3, s.length);
    }

    private static String json(int size) {
        StringBuilder sb = new StringBuilder("{\"items\":[");
        int i = 0;
        while (sb.length() < size - 40) {
            sb.append(i == 0 ? "" : ",").append("{\"id\":").append(i).append(",\"ok\":true}");
            i++;
        }
        sb.append("],\"pad\":\"");
        while (sb.length() < size - 2) {
            sb.append('x');
        }
        return sb.append("\"}").toString();
    }
}
