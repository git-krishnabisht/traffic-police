package io.trafficpolice.capture.core;

import java.util.Map;

/** Capture settings (PROTOCOL.md §7.3 {@code config}); immutable, replaced as a whole. */
public final class CaptureConfig {
    static final long DEFAULT_BODY_CAP = 10L * 1024 * 1024;
    static final long MAX_BODY_CAP = 256L * 1024 * 1024;
    static final int DEFAULT_STACK_DEPTH = 64;
    static final int MAX_STACK_DEPTH = 256;

    public final boolean recording;
    public final long bodyCap;
    public final boolean captureRequestBodies;
    public final boolean captureResponseBodies;
    public final int stackDepth;

    public CaptureConfig(boolean recording, long bodyCap, boolean captureRequestBodies, boolean captureResponseBodies,
            int stackDepth) {
        this.recording = recording;
        this.bodyCap = Math.max(0, Math.min(MAX_BODY_CAP, bodyCap));
        this.captureRequestBodies = captureRequestBodies;
        this.captureResponseBodies = captureResponseBodies;
        this.stackDepth = Math.max(0, Math.min(MAX_STACK_DEPTH, stackDepth));
    }

    public static CaptureConfig defaults() {
        return new CaptureConfig(true, DEFAULT_BODY_CAP, true, true, DEFAULT_STACK_DEPTH);
    }

    /** This config with the fields present in a (partial) {@code config} object applied. */
    CaptureConfig with(Map<String, Object> m) {
        if (m == null) {
            return this;
        }
        Boolean rec = JsonParser.bool(m, "recording");
        Boolean req = JsonParser.bool(m, "capture_request_bodies");
        Boolean resp = JsonParser.bool(m, "capture_response_bodies");
        return new CaptureConfig(
                rec != null ? rec : recording,
                JsonParser.num(m, "body_cap", bodyCap),
                req != null ? req : captureRequestBodies,
                resp != null ? resp : captureResponseBodies,
                (int) JsonParser.num(m, "stack_depth", stackDepth));
    }

    void write(Json j) {
        j.obj()
                .kv("recording", recording)
                .kv("body_cap", bodyCap)
                .kv("capture_request_bodies", captureRequestBodies)
                .kv("capture_response_bodies", captureResponseBodies)
                .kv("stack_depth", stackDepth)
                .endObj();
    }
}
