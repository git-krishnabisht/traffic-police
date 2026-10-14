package io.trafficpolice.capture.core;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertTrue;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.List;
import org.junit.After;
import org.junit.Test;

/**
 * Short writes gathered into chunks (Txn.Body.GATHER_BELOW): byte-at-a-time bodies become few
 * chunks, the bytes still reach the host promptly when the app stops writing, nothing is lost,
 * offsets stay right, and the end of a body or a transaction sends what was gathered first.
 */
public final class BodyGatheringTest {
    private TestPlatform platform = new TestPlatform(false);
    private CaptureRuntime rt = TestPlatform.runtime(platform, true);

    @After
    public void stop() {
        rt.stop();
    }

    private Txn start(String url) {
        Recorder.RequestInfo r = new Recorder.RequestInfo();
        r.call = rt.recorder().newCallId();
        r.method = "GET";
        r.url = url;
        r.clientKind = "huc";
        Txn t = rt.recorder().start(r);
        t.request.end("none");
        t.response(200, "OK", "http/1.1", new String[0], null);
        return t;
    }

    private static List<TestHost.Msg> chunks(List<TestHost.Msg> msgs, int dir) {
        List<TestHost.Msg> out = new ArrayList<>();
        for (TestHost.Msg m : msgs) {
            if (m.data != null && m.dir == dir) {
                out.add(m);
            }
        }
        return out;
    }

    /** The chunks' bytes, checking that each starts where the one before ended. */
    private static byte[] joined(List<TestHost.Msg> chunks) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (TestHost.Msg c : chunks) {
            assertEquals("offsets follow on", out.size(), c.offset);
            out.write(c.data, 0, c.data.length);
        }
        return out.toByteArray();
    }

    private static byte[] pattern(int n) {
        byte[] b = new byte[n];
        for (int i = 0; i < n; i++) {
            b[i] = (byte) (i * 31 + (i >>> 8));
        }
        return b;
    }

    @Test
    public void aBodyWrittenByteByByteBecomesFewChunks() throws Exception {
        byte[] body = pattern(100_000);
        try (TestHost host = TestHost.connect(rt)) {
            Txn t = start("https://example.com/bytes");
            for (byte b : body) {
                t.response.write(b);
            }
            t.response.end("complete");
            t.done();
            host.await(m -> "done".equals(m.t()) && m.txn == t.id, 10_000);
            List<TestHost.Msg> chunks = chunks(host.ofTxn(t.id), Frames.DIR_RESPONSE);
            assertArrayEquals(body, joined(chunks));
            // 13 full ones; a few more when the writer sends what waited 50 ms in the meantime
            assertTrue(chunks.size() + " chunks", chunks.size() >= 13 && chunks.size() <= 40);
            for (TestHost.Msg c : chunks) {
                assertTrue(c.data.length <= Txn.Body.GATHER_MAX);
            }
            TestHost.Msg end = host.await(m -> "body_end".equals(m.t()) && m.txn == t.id && "response".equals(m.str("dir")), 1000);
            assertEquals("complete", end.str("state"));
            assertEquals(body.length, end.num("bytes"));
            assertEquals(body.length, end.num("captured"));
        }
    }

    /** Short writes of any size up to a chunk header are gathered too, and longer ones are sent as they come. */
    @Test
    public void shortWritesAreGatheredAndLongerOnesSentAsTheyCome() throws Exception {
        byte[] body = pattern(20_000);
        try (TestHost host = TestHost.connect(rt)) {
            Txn t = start("https://example.com/mixed");
            int at = 0;
            for (int n = 1; at < 10_000; n = n % (Txn.Body.GATHER_BELOW - 1) + 1) {
                int len = Math.min(n, 10_000 - at);
                t.response.write(body, at, len);
                at += len;
            }
            t.response.write(body, at, 100); // 100 bytes: one chunk of their own, after the gathered ones
            at += 100;
            t.response.write(body, at, body.length - at);
            t.response.end("complete");
            t.done();
            host.await(m -> "done".equals(m.t()) && m.txn == t.id, 10_000);
            List<TestHost.Msg> chunks = chunks(host.ofTxn(t.id), Frames.DIR_RESPONSE);
            assertArrayEquals(body, joined(chunks));
            assertTrue(chunks.size() + " chunks", chunks.size() <= 12);
            TestHost.Msg hundred = null;
            for (TestHost.Msg c : chunks) {
                hundred = c.offset == 10_000 ? c : hundred;
            }
            assertEquals("the 100-byte write is a chunk of its own", 100, hundred.data.length);
        }
    }

    /** A streaming response that sends a byte and waits: the byte reaches the host without the body ending. */
    @Test
    public void aByteThatIsNotFollowedReachesTheHostSoon() throws Exception {
        try (TestHost host = TestHost.connect(rt)) {
            Txn t = start("https://example.com/stream");
            long start = System.nanoTime();
            t.response.write('a');
            TestHost.Msg first = host.await(m -> m.data != null && m.txn == t.id, 5_000);
            long waited = (System.nanoTime() - start) / 1_000_000;
            assertEquals("a", new String(first.data, Json.UTF_8));
            // the writer sends it 50 ms after it was gathered; a loaded machine may take longer
            assertTrue("arrived after " + waited + " ms", waited < 1_000);
            t.response.write('b');
            t.response.write('c');
            TestHost.Msg second = host.await(m -> m.data != null && m.txn == t.id && m.offset == 1, 5_000);
            assertEquals("bc", new String(second.data, Json.UTF_8));
            for (TestHost.Msg m : host.ofTxn(t.id)) {
                assertTrue("the body has not ended", !"body_end".equals(m.t()) || !"response".equals(m.str("dir")));
            }
            t.response.write('d');
            t.response.end("complete");
            t.done();
            host.await(m -> "done".equals(m.t()) && m.txn == t.id, 5_000);
            List<TestHost.Msg> msgs = host.ofTxn(t.id);
            assertEquals("abcd", new String(joined(chunks(msgs, Frames.DIR_RESPONSE)), Json.UTF_8));
            int lastChunk = -1;
            int bodyEnd = -1;
            for (int i = 0; i < msgs.size(); i++) {
                if (msgs.get(i).data != null) {
                    lastChunk = i;
                } else if ("body_end".equals(msgs.get(i).t()) && "response".equals(msgs.get(i).str("dir"))) {
                    bodyEnd = i;
                }
            }
            assertTrue("the last bytes come before body_end", lastChunk < bodyEnd);
        }
    }

    /** Gathered bytes go before what ends their transaction, and a request's before the response. */
    @Test
    public void gatheredBytesComeBeforeTheResponseAndTheEnd() throws Exception {
        try (TestHost host = TestHost.connect(rt)) {
            Recorder.RequestInfo r = new Recorder.RequestInfo();
            r.method = "POST";
            r.url = "https://example.com/post";
            r.clientKind = "huc";
            Txn t = rt.recorder().start(r);
            t.request.write('x'); // the app has not closed the stream when the response comes
            t.response(200, "OK", "http/1.1", new String[0], null);
            t.response.write('y');
            t.fail("response_body", false, new IOException("reset"), null);
            host.await(m -> "fail".equals(m.t()) && m.txn == t.id, 5_000);
            List<String> order = new ArrayList<>();
            for (TestHost.Msg m : host.ofTxn(t.id)) {
                order.add(m.data != null ? "chunk " + new String(m.data, Json.UTF_8) : m.t());
            }
            assertEquals(java.util.Arrays.asList("req", "chunk x", "resp", "chunk y", "fail"), order);
        }
    }

    /**
     * Bytes that come more than 50 ms apart (by the event clock) are not one chunk: each chunk's
     * ts is when its first byte came. With a frozen clock, so that the times are exact.
     */
    @Test
    public void bytesAPauseApartAreChunksOfTheirOwnAtTheirOwnTime() throws Exception {
        rt.stop();
        platform = new TestPlatform(true);
        rt = TestPlatform.runtime(platform, true);
        try (TestHost host = TestHost.connect(rt)) {
            Txn t = start("https://example.com/slow");
            long first = platform.nanoTime();
            t.response.write('1');
            platform.advanceMillis(10);
            t.response.write('2'); // 10 ms later: the same chunk
            platform.advanceMillis(60);
            long third = platform.nanoTime();
            t.response.write('3'); // 70 ms after the first: a new one
            t.response.end("complete");
            t.done();
            host.await(m -> "done".equals(m.t()) && m.txn == t.id, 5_000);
            List<TestHost.Msg> chunks = chunks(host.ofTxn(t.id), Frames.DIR_RESPONSE);
            assertEquals(2, chunks.size());
            assertEquals("12", new String(chunks.get(0).data, Json.UTF_8));
            assertEquals(first, chunks.get(0).ts);
            assertEquals("3", new String(chunks.get(1).data, Json.UTF_8));
            assertEquals(2, chunks.get(1).offset);
            assertEquals(third, chunks.get(1).ts);
        }
    }

    /** At the cap, what was gathered goes at once, and the rest is counted, not sent. */
    @Test
    public void gatheringStopsAtTheCap() throws Exception {
        try (TestHost host = TestHost.connect(rt, TestHost.helloAck(0, TestHost.DEFAULT_CONFIG.replace("10485760", "10")))) {
            Txn t = start("https://example.com/capped");
            for (int i = 0; i < 25; i++) {
                t.response.write('a' + i);
            }
            TestHost.Msg chunk = host.await(m -> m.data != null && m.txn == t.id, 5_000);
            assertEquals("abcdefghij", new String(chunk.data, Json.UTF_8));
            // byte by byte, the cap is met exactly and passed by later writes: still reported
            host.await(m -> "body_cap_reached".equals(m.str("code")), 5_000);
            t.response.end("complete");
            TestHost.Msg end = host.await(m -> "body_end".equals(m.t()) && m.txn == t.id && "response".equals(m.str("dir")), 5_000);
            assertEquals("truncated", end.str("state"));
            assertEquals(25, end.num("bytes"));
            assertEquals(10, end.num("captured"));
        }
    }

    /**
     * A 1 MiB body read byte by byte costs the ring about what its bytes do (8 KiB chunks), not
     * 104 bytes a byte: a host that connects afterwards gets all of it back, in few frames.
     */
    @Test
    public void aByteAtATimeBodyCostsTheRingAboutItsSize() throws Exception {
        byte[] body = pattern(1 << 20);
        Txn t = start("https://example.com/big");
        for (byte b : body) {
            t.response.write(b);
        }
        t.response.end("complete");
        t.done();
        TestPlatform.awaitWriter(rt);
        long kept = rt.ring.bytes();
        try (TestHost host = TestHost.connect(rt)) {
            List<TestHost.Msg> replayed = new ArrayList<>();
            long cost = ReplayRing.TXN_OVERHEAD;
            for (TestHost.Msg m : host.replayed) {
                if (m.txn == t.id) {
                    replayed.add(m);
                    cost += m.raw.length + ReplayRing.FRAME_OVERHEAD;
                }
            }
            assertEquals(kept, cost);
            assertTrue("costs " + cost, cost < body.length * 11L / 10);
            assertArrayEquals(body, joined(chunks(replayed, Frames.DIR_RESPONSE)));
            assertTrue(replayed.size() + " frames", replayed.size() < 200);
        }
    }
}
