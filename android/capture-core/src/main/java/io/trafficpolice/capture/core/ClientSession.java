package io.trafficpolice.capture.core;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.List;
import java.util.Map;

/**
 * One host connection (PROTOCOL.md §6): sends {@code hello}, waits for {@code hello_ack}, hands the
 * connection to the writer for replay and live events, then reads commands until the host goes.
 * Runs on its own thread; before the hand-over only this thread writes, afterwards only the
 * writer does.
 */
final class ClientSession implements Runnable {
    static final int HELLO_ACK_TIMEOUT_MS = 10_000;

    private final CaptureRuntime rt;
    private final Transport transport;
    private volatile boolean closed;
    private OutputStream out;

    ClientSession(CaptureRuntime rt, Transport transport) {
        this.rt = rt;
        this.transport = transport;
    }

    @Override
    public void run() {
        boolean attached = false;
        try {
            InputStream in = new BufferedInputStream(transport.input(), 64 * 1024);
            out = new BufferedOutputStream(transport.output(), 64 * 1024);
            write(rt.helloFrame());
            flush();
            transport.setReadTimeout(HELLO_ACK_TIMEOUT_MS);
            Frames.Frame first = Frames.read(in);
            if (first.type != Frames.TYPE_JSON) {
                sayGoodbye("bad_frame", "expected hello_ack");
                return;
            }
            Map<String, Object> ack = JsonParser.parseObject(first.text());
            String type = JsonParser.str(ack, "t");
            if (!"hello_ack".equals(type)) {
                if (!"bye".equals(type)) {
                    sayGoodbye("bad_frame", "expected hello_ack, got " + type);
                }
                return;
            }
            long protocol = JsonParser.num(ack, "protocol", -1);
            if (protocol != CaptureRuntime.PROTOCOL) {
                sayGoodbye("protocol_mismatch", "host speaks protocol " + protocol + "; this runtime supports "
                        + CaptureRuntime.PROTOCOL);
                return;
            }
            rt.setConfig(rt.config().with(JsonParser.obj(ack, "config")));
            write(rulesAck(JsonParser.num(ack, "id", 0), JsonParser.obj(ack, "rules")));
            flush();
            transport.setReadTimeout(0);
            rt.writer.attach(this, Math.max(0, JsonParser.num(ack, "resume_after_seq", 0)));
            attached = true;
            while (!closed) {
                Frames.Frame f = Frames.read(in);
                if (f.type == Frames.TYPE_JSON) {
                    handle(JsonParser.parseObject(f.text()));
                }
            }
        } catch (Frames.BadFrameException e) {
            sayGoodbye("bad_frame", e.getMessage());
        } catch (EOFException e) {
            // the host went away
        } catch (IOException e) {
            if (!closed) {
                rt.platform.log(Platform.LOG_DEBUG, "host connection ended: " + e, null);
            }
        } catch (IllegalArgumentException e) {
            sayGoodbye("bad_frame", "malformed JSON: " + e.getMessage());
        } catch (Throwable t) {
            rt.internalError("session", t);
        } finally {
            if (attached) {
                rt.writer.detach(this);
            } else {
                close();
            }
        }
    }

    private void handle(Map<String, Object> m) {
        String type = JsonParser.str(m, "t");
        long id = JsonParser.num(m, "id", 0);
        if ("set_config".equals(type)) {
            rt.setConfig(rt.config().with(JsonParser.obj(m, "config")));
            Json j = new Json().obj().kv("t", "config_ack").kv("id", id).key("config");
            rt.config().write(j);
            rt.writer.control(this, Frames.json(j.endObj()));
        } else if ("set_rules".equals(type)) {
            rt.writer.control(this, rulesAck(id, JsonParser.obj(m, "rules")));
        } else if ("ping".equals(type)) {
            Json j = new Json().obj().kv("t", "pong").kv("id", id);
            rt.writeClock(j);
            rt.writer.control(this, Frames.json(j.endObj()));
        } else if ("bye".equals(type)) {
            closed = true;
        }
        // other types are ignored (PROTOCOL.md §1)
    }

    /**
     * This runtime does not apply rules yet (they arrive with rule support; the {@code rules}
     * capability is not advertised), so every rule is reported as not active.
     */
    private static byte[] rulesAck(long id, Map<String, Object> ruleSet) {
        Json j = new Json().obj().kv("t", "rules_ack").kv("id", id);
        String version = ruleSet == null ? null : JsonParser.str(ruleSet, "version");
        if (version != null) {
            j.kv("version", version);
        }
        j.kv("active", 0).key("errors").arr();
        List<Object> rules = ruleSet == null ? null : JsonParser.list(ruleSet, "rules");
        if (rules != null) {
            for (Object r : rules) {
                if (r instanceof Map) {
                    @SuppressWarnings("unchecked")
                    Map<String, Object> rule = (Map<String, Object>) r;
                    String ruleId = JsonParser.str(rule, "id");
                    j.obj().kv("rule", ruleId == null ? "" : ruleId)
                            .kv("message", "this capture runtime does not apply rules yet").endObj();
                }
            }
        }
        return Frames.json(j.endArr().endObj());
    }

    synchronized void write(byte[] frame) throws IOException {
        if (closed || out == null) {
            throw new IOException("closed");
        }
        out.write(frame);
    }

    synchronized void flush() throws IOException {
        if (closed || out == null) {
            throw new IOException("closed");
        }
        out.flush();
    }

    /** Best-effort {@code bye}. */
    void sayGoodbye(String reason, String message) {
        try {
            Json j = new Json().obj().kv("t", "bye").kv("reason", reason);
            if (message != null) {
                j.kv("message", message);
            }
            if ("protocol_mismatch".equals(reason)) {
                j.key("supported").arr().num(CaptureRuntime.PROTOCOL).endArr();
            }
            write(Frames.json(j.endObj()));
            flush();
        } catch (IOException ignored) {
            // the peer is gone already
        }
    }

    void close() {
        closed = true;
        try {
            transport.close();
        } catch (IOException ignored) {
            // nothing to do
        }
    }
}
