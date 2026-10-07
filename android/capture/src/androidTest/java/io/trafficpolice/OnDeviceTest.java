package io.trafficpolice;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.Build;
import android.os.Process;
import androidx.test.ext.junit.runners.AndroidJUnit4;
import androidx.test.platform.app.InstrumentationRegistry;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.SocketNames;
import io.trafficpolice.capture.core.TestHost;
import java.io.ByteArrayOutputStream;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.zip.GZIPOutputStream;
import mockwebserver3.MockResponse;
import mockwebserver3.MockWebServer;
import okhttp3.MediaType;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.RequestBody;
import okhttp3.Response;
import okhttp3.tls.HandshakeCertificates;
import okhttp3.tls.HeldCertificate;
import okio.Buffer;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;
import org.junit.runner.RunWith;

/**
 * The library on a device (ARCHITECTURE.md §8, Android): what the JVM tests cannot show. The
 * test APK is a debuggable app with the library in it, so the provider has started capture when
 * the tests run. A pretend host reads the events in-process; the abstract socket itself is
 * checked from this app's own uid, which it must refuse.
 *
 * {@code ./gradlew :capture:connectedDebugAndroidTest} (an emulator or device of API 26 or newer)
 */
@RunWith(AndroidJUnit4.class)
public final class OnDeviceTest {
    private CaptureRuntime rt;
    private TestHost host;
    private MockWebServer server;

    @Before
    public void setUp() throws Exception {
        rt = CaptureRuntime.current();
        assertNotNull("the provider started capture in this debuggable app", rt);
        host = TestHost.connect(rt);
        server = new MockWebServer();
        server.start(InetAddress.getByName("127.0.0.1"), 0);
    }

    @After
    public void tearDown() throws Exception {
        if (host != null) host.close();
        if (server != null) server.close();
    }

    private static OkHttpClient client() {
        return new OkHttpClient.Builder()
                .addNetworkInterceptor(TrafficPolice.networkInterceptor())
                .eventListenerFactory(TrafficPolice.eventListenerFactory())
                .readTimeout(10, TimeUnit.SECONDS)
                .build();
    }

    private TestHost.Msg reqFor(String path) throws InterruptedException {
        return host.await(m -> "req".equals(m.t()) && m.str("url").endsWith(path), 10_000);
    }

    private TestHost.Msg awaitEnd(long txn) throws InterruptedException {
        return host.await(m -> ("done".equals(m.t()) || "fail".equals(m.t())) && m.txn == txn, 10_000);
    }

    private TestHost.Msg respOf(long txn) throws InterruptedException {
        return host.await(m -> "resp".equals(m.t()) && m.txn == txn, 10_000);
    }

    private static byte[] bodyOf(List<TestHost.Msg> msgs, int dir) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (TestHost.Msg m : msgs) {
            if (m.data != null && m.dir == dir) out.write(m.data, 0, m.data.length);
        }
        return out.toByteArray();
    }

    @Test
    public void startsByItselfInADebuggableApp() {
        assertTrue(TrafficPolice.isActive());
        Map<String, Object> app = host.hello.obj("app");
        String pkg = InstrumentationRegistry.getInstrumentation().getTargetContext().getPackageName();
        assertEquals(pkg, app.get("package"));
        assertEquals((long) Process.myPid(), ((Number) app.get("pid")).longValue());
        assertEquals((long) Build.VERSION.SDK_INT, ((Number) host.hello.obj("device").get("api")).longValue());
        assertEquals("library", host.hello.obj("runtime").get("mode"));
    }

    /** Only adb's shell (uid 2000) and root may read the traffic; this app's own uid may not. */
    @Test
    public void theSocketRefusesOtherUids() throws Exception {
        String pkg = InstrumentationRegistry.getInstrumentation().getTargetContext().getPackageName();
        LocalSocket socket = new LocalSocket();
        socket.connect(new LocalSocketAddress(SocketNames.forProcess(pkg, Process.myPid())));
        socket.setSoTimeout(5_000);
        try {
            int first = socket.getInputStream().read();
            assertEquals("the runtime closes the connection without a word", -1, first);
        } catch (IOException closed) {
            // a reset is a refusal too
        } finally {
            socket.close();
        }
    }

    @Test
    public void capturesOkHttpOnTheDevice() throws Exception {
        Buffer gzipped = new Buffer();
        ByteArrayOutputStream raw = new ByteArrayOutputStream();
        try (GZIPOutputStream gz = new GZIPOutputStream(raw)) {
            gz.write("{\"ok\":true}".getBytes(StandardCharsets.UTF_8));
        }
        gzipped.write(raw.toByteArray());
        server.enqueue(new MockResponse.Builder().addHeader("Content-Encoding", "gzip")
                .addHeader("Content-Type", "application/json").body(gzipped).build());
        server.enqueue(new MockResponse.Builder().code(201).body("created").build());
        OkHttpClient client = client();
        try (Response r = client.newCall(new Request.Builder().url(server.url("/json")).build()).execute()) {
            assertEquals("{\"ok\":true}", r.body().string());
        }
        RequestBody body = RequestBody.create("{\"name\":\"x\"}", MediaType.get("application/json"));
        try (Response r = client.newCall(new Request.Builder().url(server.url("/items")).post(body).build()).execute()) {
            assertEquals(201, r.code());
        }
        TestHost.Msg get = reqFor("/json");
        awaitEnd(get.txn);
        assertEquals("GET", get.str("method"));
        // the thread and the stack are the caller's: this test
        assertEquals(Thread.currentThread().getName(), get.obj("thread").get("name"));
        assertTrue(get.json.toString(), get.json.toString().contains("OnDeviceTest"));
        assertEquals(200, respOf(get.txn).num("status"));
        assertTrue("captured as the network sent it (gzip)", bodyOf(host.ofTxn(get.txn), 1).length > 0);
        TestHost.Msg post = reqFor("/items");
        awaitEnd(post.txn);
        assertEquals("{\"name\":\"x\"}", new String(bodyOf(host.ofTxn(post.txn), 0), StandardCharsets.UTF_8));
        assertEquals(201, respOf(post.txn).num("status"));
    }

    /** Android's HttpURLConnection (its own OkHttp 2 fork) throws for an error status; the
     * wrapper still records the status, the headers and the error body. */
    @Test
    public void capturesAndroidsHttpURLConnection() throws Exception {
        server.enqueue(new MockResponse.Builder().code(404).addHeader("Content-Type", "text/plain")
                .body("no such thing").build());
        HttpURLConnection c = TrafficPolice.wrap((HttpURLConnection) new URL(server.url("/missing").toString()).openConnection());
        try {
            c.getInputStream();
            fail("Android throws for a 404");
        } catch (FileNotFoundException expected) {
            // as without capture
        }
        assertEquals(404, c.getResponseCode());
        InputStream err = c.getErrorStream();
        ByteArrayOutputStream read = new ByteArrayOutputStream();
        byte[] buf = new byte[256];
        for (int n; (n = err.read(buf)) > 0; ) read.write(buf, 0, n);
        err.close();
        c.disconnect();
        assertEquals("no such thing", read.toString("UTF-8"));
        TestHost.Msg req = reqFor("/missing");
        awaitEnd(req.txn);
        assertEquals("huc", req.obj("client").get("kind"));
        assertEquals(404, respOf(req.txn).num("status"));
        assertEquals("no such thing", new String(bodyOf(host.ofTxn(req.txn), 1), StandardCharsets.UTF_8));
    }

    /** TLS from the device's own provider (Conscrypt): version, cipher and the server certificate. */
    @Test
    public void recordsTlsDetails() throws Exception {
        HeldCertificate cert = new HeldCertificate.Builder().commonName("localhost")
                .addSubjectAlternativeName("localhost").build();
        HandshakeCertificates serverCerts = new HandshakeCertificates.Builder().heldCertificate(cert).build();
        HandshakeCertificates trust = new HandshakeCertificates.Builder().addTrustedCertificate(cert.certificate()).build();
        // a small server of our own: MockWebServer's TLS path fails on Android 8 (null SNI)
        ServerSocket tls = serverCerts.sslContext().getServerSocketFactory()
                .createServerSocket(0, 5, InetAddress.getByName("127.0.0.1"));
        Thread serving = new Thread(() -> {
            try (Socket s = tls.accept()) {
                InputStream in = s.getInputStream();
                int last4 = 0;
                while (last4 != 0x0d0a0d0a) {
                    int b = in.read();
                    if (b < 0) return;
                    last4 = (last4 << 8) | b;
                }
                OutputStream out = s.getOutputStream();
                out.write(("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").getBytes(StandardCharsets.UTF_8));
                out.flush();
            } catch (IOException ignored) {
                // the test fails on the client side instead
            }
        }, "tls-test-server");
        serving.start();
        OkHttpClient client = client().newBuilder().sslSocketFactory(trust.sslSocketFactory(), trust.trustManager()).build();
        try (Response r = client.newCall(new Request.Builder().url("https://localhost:" + tls.getLocalPort() + "/secure").build()).execute()) {
            assertEquals("ok", r.body().string());
        } finally {
            tls.close();
        }
        TestHost.Msg req = reqFor("/secure");
        awaitEnd(req.txn);
        // the connection is known when the request goes out (the network interceptor's chain)
        Map<String, Object> conn = req.obj("conn") != null ? req.obj("conn") : respOf(req.txn).obj("conn");
        assertNotNull("connection details: " + req, conn);
        Map<?, ?> t = (Map<?, ?>) conn.get("tls");
        assertNotNull("TLS details: " + conn, t);
        assertTrue(String.valueOf(t.get("version")), String.valueOf(t.get("version")).startsWith("TLSv1"));
        assertNotNull(t.get("cipher"));
        assertTrue(String.valueOf(t.get("peer")), String.valueOf(t.get("peer")).contains("localhost"));
    }

    /** The app's own traffic counters (TrafficStats), which the graph draws by default. */
    @Test
    public void samplesTheAppsTrafficCounters() throws Exception {
        server.enqueue(new MockResponse.Builder().body(new Buffer().write(new byte[256 * 1024])).build());
        try (Response r = client().newCall(new Request.Builder().url(server.url("/download")).build()).execute()) {
            r.body().bytes();
        }
        TestHost.Msg traffic = host.await(m -> "traffic".equals(m.t()) && m.num("rx") > 0, 5_000);
        assertTrue(traffic.json.toString(), traffic.num("rx") > 0);
    }
}
