package io.trafficpolice.capture.core;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertTrue;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import org.junit.After;
import org.junit.Test;

/** The connection flow of PROTOCOL.md §6: handshake, replay, resume, takeover, commands. */
public final class RuntimeProtocolTest {
    private final TestPlatform platform = new TestPlatform(false);
    private final CaptureRuntime rt = TestPlatform.runtime(platform, true);

    @After
    public void stop() {
        rt.stop();
    }

    /** A complete little transaction recorded straight through the recorder. */
    private Txn exchange(String url, byte[] body) {
        Recorder.RequestInfo r = new Recorder.RequestInfo();
        r.call = rt.recorder().newCallId();
        r.method = "GET";
        r.url = url;
        r.headers = new String[] {"Accept", "application/json", "X-Dup", "1", "X-Dup", "2"};
        r.clientKind = "okhttp";
        r.stack = new ThreadStack("main", 1, ThreadStack.ORIGIN_CALL,
                new StackTraceElement[] {new StackTraceElement("com.example.A", "run", "A.kt", 12)}, false);
        Txn t = rt.recorder().start(r);
        t.request.end("none");
        t.response(200, "OK", "h2", new String[] {"Content-Type", "application/json"}, null);
        t.response.write(body, 0, body.length);
        t.response.end("complete");
        t.done();
        return t;
    }

    private static List<String> types(List<TestHost.Msg> msgs) {
        List<String> out = new ArrayList<>();
        for (TestHost.Msg m : msgs) {
            out.add(m.t());
        }
        return out;
    }

    @Test
    public void helloDescribesTheRuntime() throws Exception {
        try (TestHost host = TestHost.connect(rt)) {
            TestHost.Msg h = host.hello;
            assertEquals(1, h.num("protocol"));
            assertEquals("library", JsonParser.str(h.obj("runtime"), "mode"));
            assertEquals(CaptureRuntime.VERSION, JsonParser.str(h.obj("runtime"), "version"));
            assertEquals("0123456789abcdef0123456789abcdef", h.str("instance"));
            assertEquals("io.trafficpolice.test", JsonParser.str(h.obj("app"), "package"));
            assertEquals(4242, JsonParser.num(h.obj("app"), "pid", 0));
            assertTrue(h.list("capabilities").contains("okhttp"));
            assertTrue(h.list("capabilities").contains("huc"));
            assertTrue(h.list("capabilities").contains("rules"));
            assertNotNull(h.obj("clock"));
            assertEquals(10485760, JsonParser.num(h.obj("config"), "body_cap", 0));
            assertEquals("rules_ack", host.rulesAck.t());
            assertEquals(1, host.rulesAck.num("id"));
        }
    }

    @Test
    public void replaysWhatHappenedBeforeTheHostConnected() throws Exception {
        Txn t = exchange("https://example.com/a", "{\"ok\":true}".getBytes(Json.UTF_8));
        // the writer runs asynchronously; give it a moment to move the events into the ring
        Thread.sleep(100);
        try (TestHost host = TestHost.connect(rt)) {
            List<TestHost.Msg> txn = new ArrayList<>();
            long lastSeq = 0;
            for (TestHost.Msg m : host.replayed) {
                assertTrue("seq increases", m.seq > lastSeq);
                lastSeq = m.seq;
                if (m.txn == t.id) {
                    txn.add(m);
                }
            }
            assertEquals(java.util.Arrays.asList("req", "body_end", "resp", "body", "body_end", "done"), types(txn));
            TestHost.Msg req = txn.get(0);
            assertEquals("https://example.com/a", req.str("url"));
            List<Object> headers = req.list("headers");
            assertEquals(3, headers.size());
            assertEquals(java.util.Arrays.asList("X-Dup", "2"), headers.get(2));
            assertEquals("main", JsonParser.str(req.obj("thread"), "name"));
            TestHost.Msg body = txn.get(3);
            assertEquals("{\"ok\":true}", new String(body.data, Json.UTF_8));
            // live events follow the replay
            Txn live = exchange("https://example.com/b", new byte[0]);
            assertEquals("https://example.com/b", host.await(m -> "req".equals(m.t()) && m.txn == live.id, 5000).str("url"));
        }
    }

    @Test
    public void resumeSkipsEventsTheHostAlreadyHas() throws Exception {
        long last;
        try (TestHost host = TestHost.connect(rt)) {
            Txn t = exchange("https://example.com/1", new byte[0]);
            last = host.await(m -> "done".equals(m.t()) && m.txn == t.id, 5000).seq;
        }
        Txn later = exchange("https://example.com/2", new byte[0]);
        Thread.sleep(100);
        try (TestHost again = TestHost.connect(rt, TestHost.helloAck(last, TestHost.DEFAULT_CONFIG))) {
            assertFalse(again.replayed.isEmpty());
            for (TestHost.Msg m : again.replayed) {
                assertTrue(m.seq > last);
            }
            assertEquals(later.id, again.replayed.get(0).txn);
        }
    }

    @Test
    public void aNewHostTakesOver() throws Exception {
        TestHost first = TestHost.connect(rt);
        try (TestHost second = TestHost.connect(rt)) {
            TestHost.Msg bye = first.awaitType("bye", 5000);
            assertEquals("replaced", bye.str("reason"));
            assertTrue(first.awaitClosed(5000));
            Txn t = exchange("https://example.com/after", new byte[0]);
            second.await(m -> "done".equals(m.t()) && m.txn == t.id, 5000);
        } finally {
            first.close();
        }
    }

    @Test
    public void pauseResumeAndPing() throws Exception {
        try (TestHost host = TestHost.connect(rt)) {
            host.send("{\"t\":\"set_config\",\"id\":5,\"config\":{\"recording\":false}}");
            TestHost.Msg ack = host.awaitType("config_ack", 5000);
            assertEquals(5, ack.num("id"));
            assertEquals(Boolean.FALSE, JsonParser.bool(ack.obj("config"), "recording"));
            assertFalse(rt.recorder().recording());
            host.send("{\"t\":\"set_config\",\"id\":6,\"config\":{\"recording\":true,\"body_cap\":999999999999}}");
            TestHost.Msg ack2 = host.await(m -> "config_ack".equals(m.t()) && m.num("id") == 6, 5000);
            assertEquals("clamped", CaptureConfig.MAX_BODY_CAP, JsonParser.num(ack2.obj("config"), "body_cap", 0));
            assertTrue(rt.recorder().recording());
            host.send("{\"t\":\"ping\",\"id\":7,\"unknown_field\":[1,2]}");
            TestHost.Msg pong = host.awaitType("pong", 5000);
            assertEquals(7, pong.num("id"));
            assertNotNull(pong.obj("clock"));
            host.send("{\"t\":\"something_new\",\"id\":8}"); // ignored
            host.send("{\"t\":\"ping\",\"id\":9}");
            host.await(m -> "pong".equals(m.t()) && m.num("id") == 9, 5000);
        }
    }

    @Test
    public void rulesAreAcknowledgedWithTheirErrors() throws Exception {
        try (TestHost host = TestHost.connect(rt)) {
            host.send("{\"t\":\"set_rules\",\"id\":3,\"rules\":{\"version\":\"v2\",\"rules\":[{\"id\":\"slow\"},"
                    + "{\"id\":\"bad\",\"match\":{\"scheme\":\"ftp\"}},{\"name\":\"no id\"}]}}");
            TestHost.Msg ack = host.awaitType("rules_ack", 5000);
            assertEquals("v2", ack.str("version"));
            assertEquals("a rule without actions is still active", 1, ack.num("active"));
            assertEquals(2, ack.list("errors").size());
            // rules end with the connection
            assertEquals(1, rt.rules().rules.size());
        }
        Thread.sleep(300);
        assertEquals(0, rt.rules().rules.size());
    }

    @Test
    public void protocolMismatchSaysGoodbye() throws Exception {
        String ack = TestHost.helloAck(0, TestHost.DEFAULT_CONFIG).replace("\"protocol\":1", "\"protocol\":2");
        try {
            TestHost.connect(rt, ack);
            throw new AssertionError("expected the runtime to refuse");
        } catch (java.io.IOException expected) {
            assertTrue(expected.getMessage(), expected.getMessage().contains("protocol_mismatch"));
        }
    }

    @Test
    public void bodiesOverTheCapAreCountedNotSent() throws Exception {
        try (TestHost host = TestHost.connect(rt, TestHost.helloAck(0, TestHost.DEFAULT_CONFIG.replace("10485760", "10")))) {
            Recorder.RequestInfo r = new Recorder.RequestInfo();
            r.method = "GET";
            r.url = "https://example.com/big";
            r.clientKind = "okhttp";
            Txn t = rt.recorder().start(r);
            byte[] b = "0123456789abcdefghijklmno".getBytes(Json.UTF_8);
            t.response.write(b, 0, b.length);
            t.response.end("complete");
            TestHost.Msg end = host.await(m -> "body_end".equals(m.t()) && m.txn == t.id, 5000);
            assertEquals("truncated", end.str("state"));
            assertEquals(25, end.num("bytes"));
            assertEquals(10, end.num("captured"));
            TestHost.Msg prog = host.await(m -> "prog".equals(m.t()) && m.txn == t.id, 5000);
            assertEquals(25, prog.num("bytes"));
            int chunkBytes = 0;
            for (TestHost.Msg m : host.ofTxn(t.id)) {
                if (m.data != null) {
                    chunkBytes += m.data.length;
                }
            }
            assertEquals(10, chunkBytes);
            host.awaitType("diag", 5000);
        }
    }

    @Test
    public void queueDropsOldestAndReports() throws Exception {
        EventQueue q = new EventQueue(1000);
        for (int i = 1; i <= 50; i++) {
            q.offer(new Event.Mark(i, i, "x"));
        }
        Event.Dropped d = q.takeDropped(99);
        assertNotNull(d);
        assertTrue(d.events > 0);
        assertEquals(1, d.txns[0]);
        Map<String, Object> json = JsonParser.parseObject(new String(d.encode(1), 5, d.encode(1).length - 5, Json.UTF_8));
        assertEquals("dropped", json.get("t"));
        assertEquals(null, q.takeDropped(100));
    }

    @Test
    public void ringKeepsWholeRecentTransactions() {
        ReplayRing ring = new ReplayRing(2, 1 << 20);
        long seq = 1;
        for (long txn = 1; txn <= 3; txn++) {
            for (int i = 0; i < 3; i++) {
                Event e = new Event.Mark(seq, txn, "m");
                ring.add(seq, e, e.encode(seq));
                seq++;
            }
        }
        Event d = new Event.Diag(seq, "info", "started", "hi", null);
        ring.add(seq, d, d.encode(seq));
        List<ReplayRing.Entry> kept = ring.after(0);
        assertEquals(7, kept.size()); // txns 2 and 3 (3 frames each) and the diagnostic
        assertEquals(4, kept.get(0).seq);
        assertEquals(10, kept.get(6).seq);
    }

    @Test
    public void socketNamesFitTheAbstractNamespace() {
        assertEquals("traffic-police_com.example_4312", SocketNames.forProcess("com.example", 4312));
        StringBuilder longName = new StringBuilder("com.example");
        while (longName.length() <= 100) {
            longName.append(".segment");
        }
        String name = SocketNames.forProcess(longName.toString(), 1234567);
        assertTrue(name.length() <= 107);
        assertTrue(name.contains("~"));
    }
}
