package io.trafficpolice.capture.core;

import java.io.BufferedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.Closeable;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.function.Predicate;

/**
 * A pretend host for JVM tests: connects to the runtime over a real loopback socket, answers
 * {@code hello}, and collects every frame it receives, decoded.
 */
public final class TestHost implements Closeable {
    /** One received frame. */
    public static final class Msg {
        public final int type;
        public final byte[] raw;
        public final Map<String, Object> json;
        public final long seq;
        public final long txn;
        public final int dir;
        public final long ts;
        public final long offset;
        public final byte[] data;

        Msg(byte[] raw, Frames.Frame f) {
            this.raw = raw;
            this.type = f.type;
            if (f.type == Frames.TYPE_JSON) {
                json = JsonParser.parseObject(f.text());
                seq = JsonParser.num(json, "seq", 0);
                txn = JsonParser.num(json, "txn", 0);
                dir = -1;
                ts = JsonParser.num(json, "ts", 0);
                offset = 0;
                data = null;
            } else {
                json = null;
                byte[] p = f.payload;
                seq = Frames.getLong(p, 0);
                txn = Frames.getLong(p, 8);
                dir = p[16];
                ts = Frames.getLong(p, 18);
                offset = Frames.getLong(p, 26);
                data = new byte[p.length - Frames.BODY_HEADER];
                System.arraycopy(p, Frames.BODY_HEADER, data, 0, data.length);
            }
        }

        public String t() {
            return json == null ? "body" : JsonParser.str(json, "t");
        }

        public String str(String key) {
            return json == null ? null : JsonParser.str(json, key);
        }

        public long num(String key) {
            return json == null ? Long.MIN_VALUE : JsonParser.num(json, key, Long.MIN_VALUE);
        }

        public Map<String, Object> obj(String key) {
            return json == null ? null : JsonParser.obj(json, key);
        }

        public List<Object> list(String key) {
            return json == null ? null : JsonParser.list(json, key);
        }

        @Override
        public String toString() {
            return json != null ? json.toString() : "body(txn=" + txn + ", dir=" + dir + ", offset=" + offset
                    + ", " + data.length + " bytes)";
        }
    }

    private final Socket socket;
    private final InputStream in;
    private final OutputStream out;
    private final List<Msg> received = new ArrayList<>();
    private final Thread reader;
    private volatile IOException readerError;
    public final Msg hello;
    public final Msg rulesAck;
    /** Frames between replay begin and end, excluding both. */
    public final List<Msg> replayed = new ArrayList<>();

    private TestHost(CaptureRuntime rt, String helloAck) throws IOException {
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
        this.socket = client;
        socket.setSoTimeout(10_000);
        this.in = new BufferedInputStream(client.getInputStream());
        this.out = client.getOutputStream();
        this.hello = read();
        if (!"hello".equals(hello.t())) {
            throw new IOException("expected hello, got " + hello);
        }
        send(helloAck);
        this.rulesAck = read();
        if ("bye".equals(rulesAck.t())) {
            client.close();
            throw new IOException("bye " + rulesAck.str("reason") + ": " + rulesAck.str("message"));
        }
        Msg begin = read();
        if (!"replay".equals(begin.t())) {
            throw new IOException("expected replay begin, got " + begin);
        }
        while (true) {
            Msg m = read();
            if ("replay".equals(m.t()) && "end".equals(m.str("phase"))) {
                break;
            }
            replayed.add(m);
        }
        socket.setSoTimeout(0);
        reader = new Thread(new Runnable() {
            @Override
            public void run() {
                try {
                    while (true) {
                        Msg m = read();
                        synchronized (received) {
                            received.add(m);
                            received.notifyAll();
                        }
                    }
                } catch (IOException e) {
                    readerError = e;
                    synchronized (received) {
                        received.notifyAll();
                    }
                }
            }
        }, "test-host-reader");
        reader.setDaemon(true);
        reader.start();
    }

    public static String helloAck(long resumeAfter, String configJson) {
        return "{\"t\":\"hello_ack\",\"id\":1,\"protocol\":1,\"host\":{\"name\":\"test\",\"version\":\"0\"},"
                + "\"resume_after_seq\":" + resumeAfter + ",\"config\":" + configJson
                + ",\"rules\":{\"version\":\"none\",\"rules\":[]}}";
    }

    public static final String DEFAULT_CONFIG = "{\"recording\":true,\"body_cap\":10485760,"
            + "\"capture_request_bodies\":true,\"capture_response_bodies\":true,\"stack_depth\":64}";

    public static TestHost connect(CaptureRuntime rt) throws IOException {
        return new TestHost(rt, helloAck(0, DEFAULT_CONFIG));
    }

    public static TestHost connect(CaptureRuntime rt, String helloAck) throws IOException {
        return new TestHost(rt, helloAck);
    }

    private Msg read() throws IOException {
        byte[] head = new byte[5];
        readFully(head);
        int length = ((head[0] & 0xff) << 24) | ((head[1] & 0xff) << 16) | ((head[2] & 0xff) << 8) | (head[3] & 0xff);
        byte[] payload = new byte[length - 1];
        readFully(payload);
        ByteArrayOutputStream raw = new ByteArrayOutputStream(5 + payload.length);
        raw.write(head);
        raw.write(payload);
        return new Msg(raw.toByteArray(), new Frames.Frame(head[4] & 0xff, payload));
    }

    private void readFully(byte[] b) throws IOException {
        int off = 0;
        while (off < b.length) {
            int n = in.read(b, off, b.length - off);
            if (n < 0) {
                throw new java.io.EOFException();
            }
            off += n;
        }
    }

    public synchronized void send(String json) throws IOException {
        out.write(Frames.json(json.getBytes(Json.UTF_8)));
        out.flush();
    }

    public void sendRaw(byte[] bytes) throws IOException {
        out.write(bytes);
        out.flush();
    }

    /** Everything received after the replay so far. */
    public List<Msg> received() {
        synchronized (received) {
            return new ArrayList<>(received);
        }
    }

    /** Waits for a message matching {@code p}. */
    public Msg await(Predicate<Msg> p, long timeoutMillis) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(timeoutMillis);
        synchronized (received) {
            while (true) {
                for (Msg m : received) {
                    if (p.test(m)) {
                        return m;
                    }
                }
                long left = deadline - System.nanoTime();
                if (left <= 0 || readerError != null) {
                    throw new AssertionError("timed out waiting; received: " + received
                            + (readerError != null ? " (reader: " + readerError + ")" : ""));
                }
                received.wait(Math.max(1, TimeUnit.NANOSECONDS.toMillis(left)));
            }
        }
    }

    public Msg awaitType(String type, long timeoutMillis) throws InterruptedException {
        return await(m -> type.equals(m.t()), timeoutMillis);
    }

    /** Whether the runtime closed the connection. */
    public boolean awaitClosed(long timeoutMillis) throws InterruptedException {
        reader.join(timeoutMillis);
        return !reader.isAlive();
    }

    public List<Msg> ofTxn(long txn) {
        List<Msg> out = new ArrayList<>();
        for (Msg m : received()) {
            if (m.txn == txn) {
                out.add(m);
            }
        }
        return out;
    }

    @Override
    public void close() throws IOException {
        socket.close();
    }
}
