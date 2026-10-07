package io.trafficpolice.capture.okhttp;

import io.trafficpolice.capture.core.CaptureRuntime;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Tells when Android Studio's Network Inspector runs in the same app (ARCHITECTURE.md §9.3): its
 * interceptor joins the chain through the same {@code networkInterceptors()} getter, so both
 * inspectors see every request, and its response rules may change what ours records. A few
 * stack samples taken inside the chain find its frames: in our interceptor (Studio's runs before
 * it) and in {@code requestHeadersStart}, which OkHttp calls below every network interceptor.
 * Reported once per process as {@code studio_inspector_present}.
 */
final class StudioDetector {
    /** The first calls are sampled, then one in 256 (Studio can attach later): a stack costs tens of µs. */
    private static final int FIRST = 8;
    private static final AtomicLong CALLS = new AtomicLong();
    private static final AtomicBoolean REPORTED = new AtomicBoolean();

    private StudioDetector() {}

    /** For tests: as in a new process. */
    static void reset() {
        CALLS.set(0);
        REPORTED.set(false);
    }

    static void sample(CaptureRuntime rt) {
        long n = CALLS.getAndIncrement();
        if (REPORTED.get() || (n >= FIRST && (n & 255) != 0)) return;
        try {
            for (StackTraceElement f : Thread.currentThread().getStackTrace()) {
                String c = f.getClassName();
                // the Network Inspector (App Inspection), and the profiler that came before it
                if (c.startsWith("com.android.tools.appinspection.network.")
                        || c.startsWith("com.android.tools.profiler.support.network.")) {
                    if (!REPORTED.compareAndSet(false, true)) return;
                    rt.diag("warn", "studio_inspector_present",
                            "Android Studio's Network Inspector also watches this app (" + c
                                    + "): both see every request, and its rules may change responses before or after ours",
                            null);
                    return;
                }
            }
        } catch (Throwable t) {
            rt.internalError("okhttp.studio", t);
        }
    }
}
