package io.trafficpolice.capture.core;

import java.util.concurrent.atomic.AtomicLong;

/**
 * A platform for JVM tests. With {@link #frozen} the clock only moves when the test moves it,
 * which makes encoded frames reproducible (protocol goldens); otherwise it follows real time.
 */
public final class TestPlatform implements Platform {
    public static final long BASE_TS = 5_800_000_000_000L;
    public static final long BASE_WALL_MS = 1_790_658_600_000L;

    public final AtomicLong clock = new AtomicLong(BASE_TS);
    public final boolean frozen;
    public volatile long[] traffic;
    private final long start = System.nanoTime();

    public TestPlatform(boolean frozen) {
        this.frozen = frozen;
    }

    public void advanceMillis(long ms) {
        clock.addAndGet(ms * 1_000_000L);
    }

    @Override
    public long nanoTime() {
        return frozen ? clock.get() : BASE_TS + (System.nanoTime() - start);
    }

    @Override
    public long wallMillis() {
        return BASE_WALL_MS + (nanoTime() - BASE_TS) / 1_000_000L;
    }

    @Override
    public long[] trafficCounters() {
        return traffic;
    }

    @Override
    public AppInfo app() {
        return new AppInfo("io.trafficpolice.test", "io.trafficpolice.test", 4242, 10123, true);
    }

    @Override
    public DeviceInfo device() {
        return new DeviceInfo(36, "16", "traffic-police", "JVM test", "arm64-v8a", new String[] {"arm64-v8a"});
    }

    @Override
    public void log(int level, String message, Throwable error) {
        if (level >= LOG_WARN) {
            System.err.println("[traffic-police] " + message);
            if (error != null) {
                error.printStackTrace();
            }
        }
    }

    /** A started runtime for tests; stop it after the test. */
    public static CaptureRuntime runtime(TestPlatform platform, boolean okhttp) {
        CaptureRuntime.Options o = new CaptureRuntime.Options();
        o.okhttp = okhttp;
        o.clients.put("okhttp", okhttp ? "test" : null);
        o.instance = "0123456789abcdef0123456789abcdef";
        o.sampleTraffic = false;
        CaptureRuntime current = CaptureRuntime.current();
        if (current != null) {
            current.stop();
        }
        return CaptureRuntime.start(platform, o);
    }

    /** Parses a JSON object (tests use the runtime's own parser). */
    public static java.util.Map<String, Object> json(String text) {
        return JsonParser.parseObject(text);
    }
}
