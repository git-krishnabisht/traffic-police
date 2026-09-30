package io.trafficpolice.capture.core;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.Closeable;
import java.io.EOFException;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.zip.CRC32;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/**
 * Protocol conformance goldens (PROTOCOL.md §11).
 *
 * <p>Device to host: each scenario drives the real runtime with a frozen clock and records every
 * byte the host receives into {@code testdata/protocol/v1/device/<scenario>.frames}, with this
 * encoder's reading of each frame in {@code <scenario>.expected.json}; the Rust tests must decode
 * both the same way. Run {@code ./gradlew :capture-core:updateProtocolGoldens} to rewrite them.
 *
 * <p>Host to device: the frames in {@code testdata/protocol/v1/host/} are written by the Rust
 * encoder; here the runtime must accept each one and act on it.
 */
public class ProtocolGoldenTest {
    private static final boolean UPDATE = Boolean.getBoolean("trafficpolice.updateGoldens");
    private static final File DIR = new File(System.getProperty("trafficpolice.testdata", "../../testdata"), "protocol/v1");

    private TestPlatform platform;
    private CaptureRuntime rt;

    @Before
    public void setUp() {
        platform = new TestPlatform(true);
        rt = TestPlatform.runtime(platform, true);
    }

    @After
    public void tearDown() {
        rt.stop();
    }

    // --- device -> host --------------------------------------------------------------------

    @Test
    public void handshakeReplayAndControlMessages() throws Exception {
        // before the host connects: a finished request, whole-app traffic, a diagnostic
        rt.queue.offer(new Event.Traffic(platform.nanoTime(), 1000, 500, 0));
        Txn t = rt.recorder().start(request(1, 0, "GET", "https://api.example.app/v1/status?id=7",
                headers("Host", "api.example.app", "Accept-Encoding", "gzip", "User-Agent", "okhttp/4.12.0")));
        t.request.end("none");
        platform.advanceMillis(40);
        t.response(200, "OK", "h2", headers("content-type", "application/json", "content-length", "11"), null);
        platform.advanceMillis(5);
        writeAll(t.response, "{\"ok\":true}".getBytes(Json.UTF_8));
        t.response.end("complete");
        t.done();
        platform.advanceMillis(500);
        rt.queue.offer(new Event.Traffic(platform.nanoTime(), 1800, 900, platform.nanoTime() - 500_000_000L));
        rt.diag("info", "okhttp_detected", "OkHttp 4.12.0 found", Collections.singletonMap("version", "4.12.0"));
        awaitWriter();

        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            platform.advanceMillis(1000);
            host.send("{\"t\":\"set_config\",\"id\":2,\"config\":{\"recording\":false,\"stack_depth\":32}}");
            host.until("config_ack");
            host.send("{\"t\":\"ping\",\"id\":3}");
            host.until("pong");
            host.send("{\"t\":\"set_rules\",\"id\":4,\"rules\":{\"version\":\"r1\",\"rules\":[]}}");
            host.until("rules_ack");
            host.send("{\"t\":\"bye\",\"reason\":\"shutdown\"}");
            host.readToEnd();
            golden("handshake_replay_and_control", host);
        }
    }

    @Test
    public void getWithGzipJson() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            Recorder.RequestInfo r = request(5, 0, "GET", "https://api.example.app/v1/profile",
                    headers("Host", "api.example.app", "Accept-Encoding", "gzip", "User-Agent", "okhttp/4.12.0"));
            long start = platform.nanoTime();
            r.markNames = new String[] {"call_start", "dns_start", "dns_end", "connect_start", "secure_connect_start",
                    "secure_connect_end", "connect_end", "conn_acquired"};
            r.markTimes = new long[] {start, start + 1_000_000, start + 9_000_000, start + 9_500_000, start + 20_000_000,
                    start + 61_000_000, start + 62_000_000, start + 62_500_000};
            r.ts = start + 63_000_000;
            r.conn = conn(false);
            platform.advanceMillis(63);
            Txn t = rt.recorder().start(r);
            t.mark("req_headers_start");
            platform.advanceMillis(1);
            t.mark("req_headers_end");
            t.request.end("none");
            platform.advanceMillis(80);
            t.mark("resp_headers_start");
            t.response(200, "OK", "h2", headers("content-type", "application/json; charset=utf-8",
                    "content-encoding", "gzip", "vary", "Accept-Encoding"), conn(true));
            t.mark("resp_headers_end");
            byte[] gz = GZIPPED_PROFILE;
            platform.advanceMillis(3);
            t.response.write(gz, 0, 20);
            platform.advanceMillis(2);
            t.response.write(gz, 20, gz.length - 20);
            t.response.end("complete");
            t.mark("resp_body_end");
            t.done();
            host.untilTxnEnds(t.id);
            golden("get_gzip_json", host);
        }
    }

    @Test
    public void postWithRequestBody() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            byte[] body = "{\"session\":\"s_1\",\"document\":\"passport\"}".getBytes(Json.UTF_8);
            Recorder.RequestInfo r = request(6, 0, "POST", "https://api.example.app/v1/enroll",
                    headers("Content-Type", "application/json; charset=utf-8", "Content-Length", String.valueOf(body.length),
                            "Host", "api.example.app"));
            r.hasBody = true;
            r.bodyLength = body.length;
            r.bodyType = "application/json; charset=utf-8";
            r.conn = conn(true);
            Txn t = rt.recorder().start(r);
            platform.advanceMillis(1);
            t.request.write(body, 0, body.length);
            t.request.end("complete");
            platform.advanceMillis(120);
            t.response(201, "Created", "h2", headers("content-type", "application/json"), null);
            writeAll(t.response, "{\"enrolled\":true}".getBytes(Json.UTF_8));
            t.response.end("complete");
            t.done();
            host.untilTxnEnds(t.id);
            golden("post_request_body", host);
        }
    }

    @Test
    public void chunkedStreamingResponse() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            Txn t = rt.recorder().start(request(7, 0, "GET", "http://10.0.2.2:8080/stream", headers("Host", "10.0.2.2:8080")));
            t.request.end("none");
            platform.advanceMillis(30);
            t.response(200, "OK", "http/1.1", headers("Content-Type", "text/plain; charset=utf-8",
                    "Transfer-Encoding", "chunked"), null);
            for (int i = 1; i <= 5; i++) {
                platform.advanceMillis(100);
                writeAll(t.response, ("event " + i + ": status=running\n").getBytes(Json.UTF_8));
            }
            t.response.end("complete");
            t.done();
            host.untilTxnEnds(t.id);
            golden("chunked_streaming", host);
        }
    }

    @Test
    public void bodyOverTheCapWithProgress() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, "{\"recording\":true,\"body_cap\":1024,"
                    + "\"capture_request_bodies\":true,\"capture_response_bodies\":true,\"stack_depth\":64}"));
            Txn t = rt.recorder().start(request(8, 0, "GET", "https://cdn.example.app/model.bin", headers("Host", "cdn.example.app")));
            t.request.end("none");
            platform.advanceMillis(50);
            t.response(200, "OK", "h2", headers("content-type", "application/octet-stream", "content-length", "3000"), null);
            byte[] block = new byte[1000];
            for (int i = 0; i < block.length; i++) {
                block[i] = (byte) (i * 31);
            }
            for (int i = 0; i < 3; i++) {
                platform.advanceMillis(150);
                t.response.write(block, 0, block.length);
            }
            t.response.end("complete");
            t.done();
            host.untilTxnEnds(t.id);
            golden("body_over_cap", host);
        }
    }

    @Test
    public void redirectIsTwoHopsOfOneCall() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            Txn first = rt.recorder().start(request(9, 0, "GET", "https://api.example.app/old", headers("Host", "api.example.app")));
            first.request.end("none");
            platform.advanceMillis(40);
            first.response(302, "Found", "h2", headers("location", "/new", "content-length", "0"), conn(true));
            first.response.end("none");
            first.done();
            Txn second = rt.recorder().start(request(9, 1, "GET", "https://api.example.app/new", headers("Host", "api.example.app")));
            second.request.end("none");
            platform.advanceMillis(35);
            second.response(200, "OK", "h2", headers("content-type", "application/json"), conn(true));
            writeAll(second.response, "{\"moved\":true}".getBytes(Json.UTF_8));
            second.response.end("complete");
            second.done();
            host.untilTxnEnds(second.id);
            golden("redirect", host);
        }
    }

    @Test
    public void failureByTimeoutAndCancellation() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            Txn slow = rt.recorder().start(request(10, 0, "GET", "https://api.example.app/slow", headers("Host", "api.example.app")));
            slow.request.end("none");
            slow.mark("resp_headers_start");
            platform.advanceMillis(10_000);
            java.net.SocketTimeoutException timeout = new java.net.SocketTimeoutException("timeout");
            timeout.initCause(new java.net.SocketException("Socket closed"));
            slow.fail("response_headers", false, timeout, conn(true));
            Txn canceled = rt.recorder().start(request(11, 0, "GET", "https://api.example.app/slow", headers("Host", "api.example.app")));
            canceled.request.end("none");
            platform.advanceMillis(300);
            canceled.fail("response_headers", true, new IOException("Canceled"), null);
            host.untilTxnEnds(canceled.id);
            golden("failures", host);
        }
    }

    @Test
    public void droppedAndDiagnostics() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            Txn a = rt.recorder().start(request(12, 0, "GET", "https://api.example.app/a", headers()));
            Txn b = rt.recorder().start(request(13, 0, "GET", "https://api.example.app/b", headers()));
            platform.advanceMillis(5);
            // what the writer reports after the queue overflowed (EventQueue decides when)
            rt.queue.offer(new Event.Dropped(platform.nanoTime(), 12, 34_567, new long[] {a.id, b.id}, false));
            Map<String, String> data = new LinkedHashMap<>();
            data.put("found", "3.8.1");
            data.put("needs", "3.9.0");
            rt.diag("warn", "okhttp_unsupported_version", "OkHttp 3.8.1 has no Chain.call(); OkHttp capture is off", data);
            platform.advanceMillis(5);
            a.fail("unknown", false, new IOException("lost"), null);
            b.done();
            host.untilTxnEnds(b.id);
            golden("dropped_and_diag", host);
        }
    }

    @Test
    public void largestBodyChunk() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            Txn t = rt.recorder().start(request(14, 0, "GET", "https://cdn.example.app/blob", headers("Host", "cdn.example.app")));
            t.request.end("none");
            t.response(200, "OK", "h2", headers("content-length", String.valueOf(Frames.MAX_CHUNK)), null);
            byte[] blob = new byte[Frames.MAX_CHUNK];
            for (int i = 0; i < blob.length; i++) {
                blob[i] = (byte) (i ^ (i >>> 8));
            }
            t.response.write(blob, 0, blob.length);
            t.response.end("complete");
            t.done();
            host.untilTxnEnds(t.id);
            golden("largest_body_chunk", host);
        }
    }

    /** What a newer runtime might send: unknown fields and an unknown message type. */
    @Test
    public void unknownFieldsAndTypes() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            final long ts = platform.nanoTime();
            rt.queue.offer(new Event(ts, 15) {
                @Override
                byte[] encode(long seq) {
                    return Frames.json(start("req", seq).kv("method", "GET").kv("url", "https://api.example.app/future")
                            .key("headers").arr().endArr()
                            .key("future_field").obj().kv("nested", 1).endObj()
                            .kv("future_flag", true)
                            .endObj());
                }
            });
            rt.queue.offer(new Event(ts + 1_000_000, 0) {
                @Override
                byte[] encode(long seq) {
                    return Frames.json(start("future_event", seq).kv("future_field", "x").endObj());
                }
            });
            rt.queue.offer(new Event(ts + 2_000_000, 15) {
                @Override
                byte[] encode(long seq) {
                    return Frames.json(start("done", seq).kv("future_reason", "fine").endObj());
                }
            });
            host.untilTxnEnds(15);
            golden("unknown_fields_and_types", host);
        }
    }

    @Test
    public void protocolMismatch() throws Exception {
        try (GoldenHost host = new GoldenHost(rt)) {
            host.until("hello");
            host.send("{\"t\":\"hello_ack\",\"id\":1,\"protocol\":2,\"host\":{\"name\":\"test\",\"version\":\"9\"},"
                    + "\"resume_after_seq\":0,\"config\":" + TestHost.DEFAULT_CONFIG + ",\"rules\":{\"version\":\"none\",\"rules\":[]}}");
            host.readToEnd();
            golden("protocol_mismatch", host);
        }
    }

    @Test
    public void takeover() throws Exception {
        try (GoldenHost first = new GoldenHost(rt)) {
            first.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
            try (GoldenHost second = new GoldenHost(rt)) {
                second.handshake(TestHost.helloAck(0, TestHost.DEFAULT_CONFIG));
                first.readToEnd();
                golden("takeover", first);
            }
        }
    }

    /** The largest legal frame, generated rather than stored; one byte more is corrupt. */
    @Test
    public void largestFrameRoundTripsAndLargerIsRejected() throws Exception {
        byte[] payload = new byte[Frames.MAX_FRAME - 1];
        Arrays.fill(payload, (byte) 'a');
        payload[0] = '"';
        payload[payload.length - 1] = '"';
        byte[] frame = Frames.json(payload);
        assertEquals(4 + Frames.MAX_FRAME, frame.length);
        Frames.Frame f = Frames.read(new ByteArrayInputStream(frame));
        assertEquals(Frames.TYPE_JSON, f.type);
        assertEquals(payload.length, f.payload.length);
        byte[] tooLong = {0x01, 0x00, 0x00, 0x01, Frames.TYPE_JSON};
        try {
            Frames.read(new ByteArrayInputStream(tooLong));
            fail("a frame over 16 MiB must be rejected");
        } catch (Frames.BadFrameException expected) {
            // corrupt stream
        }
    }

    // --- host -> device --------------------------------------------------------------------

    @Test
    public void hostGoldensAreUnderstood() throws Exception {
        String helloAck = hostJson("hello_ack");
        Map<String, Object> ack = JsonParser.parseObject(helloAck);
        try (GoldenHost host = new GoldenHost(rt)) {
            host.until("hello");
            host.sendRaw(hostFrame("hello_ack"));
            TestHost.Msg rulesAck = host.until("rules_ack");
            assertEquals(1L, rulesAck.num("id"));
            host.until("replay");
            host.until("replay");
            // the configuration from hello_ack is in force
            Map<String, Object> config = JsonParser.obj(ack, "config");
            assertEquals(JsonParser.num(config, "body_cap", -1), rt.config().bodyCap);
            assertEquals(JsonParser.num(config, "stack_depth", -1), rt.config().stackDepth);
            assertEquals(JsonParser.bool(config, "capture_response_bodies"), rt.config().captureResponseBodies);

            // set_config patches only the fields it names
            Map<String, Object> set = JsonParser.parseObject(hostJson("set_config"));
            Map<String, Object> patch = JsonParser.obj(set, "config");
            host.sendRaw(hostFrame("set_config"));
            TestHost.Msg configAck = host.until("config_ack");
            assertEquals(JsonParser.num(set, "id", -1), configAck.num("id"));
            Map<String, Object> applied = configAck.obj("config");
            for (String key : patch.keySet()) {
                assertEquals(key, String.valueOf(patch.get(key)), String.valueOf(applied.get(key)));
            }
            assertEquals(String.valueOf(config.get("capture_request_bodies")), String.valueOf(applied.get("capture_request_bodies")));

            // ping: a pong with the same id
            host.sendRaw(hostFrame("ping"));
            TestHost.Msg pong = host.until("pong");
            assertEquals(JsonParser.num(JsonParser.parseObject(hostJson("ping")), "id", -1), pong.num("id"));

            // set_rules with every matcher and action parses; this runtime does not apply rules yet
            Map<String, Object> rules = JsonParser.parseObject(hostJson("set_rules"));
            host.sendRaw(hostFrame("set_rules"));
            TestHost.Msg rulesAck2 = host.until("rules_ack");
            assertEquals(JsonParser.num(rules, "id", -1), rulesAck2.num("id"));
            assertEquals(JsonParser.str(JsonParser.obj(rules, "rules"), "version"), rulesAck2.str("version"));

            // bye: the runtime closes the connection
            host.sendRaw(hostFrame("bye"));
            host.readToEnd();
        }
    }

    // --- helpers ---------------------------------------------------------------------------

    private static String[] headers(String... pairs) {
        return pairs;
    }

    private Recorder.RequestInfo request(long call, int hop, String method, String url, String[] headers) {
        Recorder.RequestInfo r = new Recorder.RequestInfo();
        r.ts = platform.nanoTime();
        r.call = call;
        r.hop = hop;
        r.method = method;
        r.url = url;
        r.headers = headers;
        r.clientKind = "okhttp";
        r.clientVersion = "4.12.0";
        r.stack = new ThreadStack("DefaultDispatcher-worker-3", 51, ThreadStack.ORIGIN_CALL, new StackTraceElement[] {
                new StackTraceElement("com.example.app.api.StatusPoller", "poll", "StatusPoller.kt", 41),
                new StackTraceElement("com.example.app.MainViewModel$refresh$1", "invokeSuspend", "MainViewModel.kt", 88),
                new StackTraceElement("kotlinx.coroutines.DispatchedTask", "run", "DispatchedTask.kt", 108),
                new StackTraceElement("java.lang.Thread", "run", null, -1)}, true);
        return r;
    }

    private static ConnInfo conn(boolean reused) {
        ConnInfo.Cert cert = new ConnInfo.Cert("CN=*.example.app", "CN=WE1, O=Google Trust Services, C=US",
                1_780_000_000_000L, 1_787_776_000_000L, "3f9a1c", Arrays.asList("*.example.app", "example.app"));
        ConnInfo c = new ConnInfo("c-17", false, "h2", "142.250.183.14", 443, "DIRECT", "TLSv1.3",
                "TLS_AES_128_GCM_SHA256", Collections.singletonList(cert));
        return reused ? c.asReused() : c;
    }

    private static void writeAll(Txn.Body body, byte[] bytes) {
        body.write(bytes, 0, bytes.length);
    }

    /**
     * {@code {"id":4821,"name":"Asha Verma","plan":"pro"}} gzipped once, stored as bytes: deflate
     * output may differ between zlib builds, and the golden must not.
     */
    private static final byte[] GZIPPED_PROFILE = hex(
            "1f8b08000000000002ffab56ca4c51b232b13032d451ca4bcc4d55b252722cce4854084b2dca4d54d2512ac849cc038a1514e52bd50200049a122d2c000000");

    private static byte[] hex(String s) {
        byte[] out = new byte[s.length() / 2];
        for (int i = 0; i < out.length; i++) {
            out[i] = (byte) Integer.parseInt(s.substring(2 * i, 2 * i + 2), 16);
        }
        return out;
    }

    /** Waits until the writer has taken everything queued so far (encoded, in the ring). */
    private void awaitWriter() throws InterruptedException {
        while (true) {
            final CountDownLatch done = new CountDownLatch(1);
            final boolean[] idle = new boolean[1];
            rt.writer.post(new Runnable() {
                @Override
                public void run() {
                    idle[0] = rt.queue.isEmpty();
                    done.countDown();
                }
            });
            assertTrue(done.await(5, TimeUnit.SECONDS));
            if (idle[0]) {
                return;
            }
        }
    }

    private void golden(String name, GoldenHost host) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        StringBuilder expected = new StringBuilder("[\n");
        for (int i = 0; i < host.frames.size(); i++) {
            TestHost.Msg m = host.frames.get(i);
            bytes.write(m.raw);
            expected.append(describe(m)).append(i + 1 < host.frames.size() ? ",\n" : "\n");
        }
        expected.append("]\n");
        File frames = new File(DIR, "device/" + name + ".frames");
        File json = new File(DIR, "device/" + name + ".expected.json");
        if (UPDATE) {
            frames.getParentFile().mkdirs();
            Files.write(frames.toPath(), bytes.toByteArray());
            Files.write(json.toPath(), expected.toString().getBytes(Json.UTF_8));
            return;
        }
        if (!frames.exists()) {
            fail("missing golden " + frames + "; run ./gradlew :capture-core:updateProtocolGoldens");
        }
        assertEquals("the encoder's reading of " + name + " changed", new String(Files.readAllBytes(json.toPath()), Json.UTF_8),
                expected.toString());
        assertArrayEquals("the bytes of " + name + " changed", Files.readAllBytes(frames.toPath()), bytes.toByteArray());
    }

    /** One frame as this encoder meant it: the JSON message as sent, or a body chunk's header. */
    private static String describe(TestHost.Msg m) {
        if (m.type == Frames.TYPE_JSON) {
            return "{\"frame\":\"json\",\"msg\":" + new String(m.raw, 5, m.raw.length - 5, Json.UTF_8) + "}";
        }
        int flags = m.raw[5 + 17] & 0xff;
        return "{\"frame\":\"body\",\"seq\":" + m.seq + ",\"txn\":" + m.txn + ",\"dir\":\"" + Event.dirName(m.dir)
                + "\",\"flags\":" + flags + ",\"ts\":" + m.ts + ",\"offset\":" + m.offset + ",\"len\":" + m.data.length
                + ",\"crc32\":\"" + crc32(m.data) + "\"}";
    }

    private static String crc32(byte[] data) {
        CRC32 crc = new CRC32();
        crc.update(data, 0, data.length);
        return String.format("%08x", crc.getValue());
    }

    private static byte[] hostFrame(String name) throws IOException {
        File f = new File(DIR, "host/" + name + ".frames");
        if (!f.exists()) {
            fail("missing host golden " + f + "; run cargo test -p traffic-police-proto -- --ignored update_goldens");
        }
        return Files.readAllBytes(f.toPath());
    }

    /** The JSON text inside a host golden frame (and a check that it is exactly one frame). */
    private static String hostJson(String name) throws IOException {
        byte[] bytes = hostFrame(name);
        InputStream in = new ByteArrayInputStream(bytes);
        Frames.Frame f = Frames.read(in);
        assertEquals(Frames.TYPE_JSON, f.type);
        assertEquals("one frame per host golden", -1, in.read());
        String text = f.text();
        assertEquals(name, JsonParser.str(JsonParser.parseObject(text), "t"));
        return text;
    }

    /** A host on a loopback socket that keeps every frame it receives, raw and in order. */
    private static final class GoldenHost implements Closeable {
        final List<TestHost.Msg> frames = new ArrayList<>();
        private final Socket socket;
        private final InputStream in;
        private final OutputStream out;

        GoldenHost(CaptureRuntime rt) throws IOException {
            ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
            Socket client = new Socket(InetAddress.getLoopbackAddress(), server.getLocalPort());
            final Socket accepted = server.accept();
            server.close();
            rt.serve(new Transport() {
                @Override
                public InputStream input() throws IOException {
                    return accepted.getInputStream();
                }

                @Override
                public OutputStream output() throws IOException {
                    return accepted.getOutputStream();
                }

                @Override
                public void setReadTimeout(int millis) throws IOException {
                    accepted.setSoTimeout(millis);
                }

                @Override
                public void close() throws IOException {
                    accepted.close();
                }
            });
            socket = client;
            socket.setSoTimeout(5_000);
            in = client.getInputStream();
            out = client.getOutputStream();
        }

        void handshake(String helloAck) throws IOException {
            until("hello");
            send(helloAck);
            until("rules_ack");
            until("replay");
            TestHost.Msg end;
            do {
                end = next();
            } while (!("replay".equals(end.t()) && "end".equals(end.str("phase"))));
        }

        TestHost.Msg next() throws IOException {
            byte[] head = new byte[5];
            readFully(head);
            int length = ((head[0] & 0xff) << 24) | ((head[1] & 0xff) << 16) | ((head[2] & 0xff) << 8) | (head[3] & 0xff);
            byte[] payload = new byte[length - 1];
            readFully(payload);
            byte[] raw = new byte[5 + payload.length];
            System.arraycopy(head, 0, raw, 0, 5);
            System.arraycopy(payload, 0, raw, 5, payload.length);
            TestHost.Msg m = new TestHost.Msg(raw, new Frames.Frame(head[4] & 0xff, payload));
            frames.add(m);
            return m;
        }

        TestHost.Msg until(String type) throws IOException {
            while (true) {
                TestHost.Msg m = next();
                if (type.equals(m.t())) {
                    return m;
                }
            }
        }

        void untilTxnEnds(long txn) throws IOException {
            while (true) {
                TestHost.Msg m = next();
                if (m.txn == txn && ("done".equals(m.t()) || "fail".equals(m.t()))) {
                    return;
                }
            }
        }

        /** Reads until the runtime closes the connection. */
        void readToEnd() throws IOException {
            try {
                while (true) {
                    next();
                }
            } catch (EOFException | java.net.SocketException e) {
                // closed
            } catch (SocketTimeoutException e) {
                throw new AssertionError("the runtime did not close the connection; got " + frames);
            }
        }

        void send(String json) throws IOException {
            sendRaw(Frames.json(json.getBytes(Json.UTF_8)));
        }

        void sendRaw(byte[] frame) throws IOException {
            out.write(frame);
            out.flush();
        }

        private void readFully(byte[] b) throws IOException {
            int off = 0;
            while (off < b.length) {
                int n = in.read(b, off, b.length - off);
                if (n < 0) {
                    throw new EOFException();
                }
                off += n;
            }
        }

        @Override
        public void close() throws IOException {
            socket.close();
        }
    }
}
