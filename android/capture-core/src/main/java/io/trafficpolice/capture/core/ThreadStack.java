package io.trafficpolice.capture.core;

/**
 * The thread and call stack that started a request (PROTOCOL.md §4 {@code thread}, {@code stack}).
 * The app's thread only records where it is ({@code new Throwable()}); turning that into frames
 * ({@code getStackTrace()}: class, method, file and line for every frame), the costly part on
 * ART, happens on the writer thread.
 */
public final class ThreadStack {
    public static final String ORIGIN_CALL = "call";
    public static final String ORIGIN_INTERCEPTOR = "interceptor";
    public static final String ORIGIN_HUC = "huc";

    final String threadName;
    final long threadId;
    final String origin;
    private final int depth;
    /** Where the app was; resolved (and dropped) on first use of {@link #frames()}. */
    private Throwable site;
    private StackTraceElement[] frames;
    private boolean truncated;

    public ThreadStack(String threadName, long threadId, String origin, StackTraceElement[] frames, boolean truncated) {
        this.threadName = threadName;
        this.threadId = threadId;
        this.origin = origin;
        this.depth = frames.length;
        this.frames = frames;
        this.truncated = truncated;
    }

    private ThreadStack(String threadName, long threadId, String origin, Throwable site, int depth) {
        this.threadName = threadName;
        this.threadId = threadId;
        this.origin = origin;
        this.depth = depth;
        this.site = site;
    }

    /**
     * The current thread and its stack, without the runtime's own frames at the top, keeping at
     * most {@code depth} frames.
     */
    public static ThreadStack capture(String origin, int depth) {
        Thread t = Thread.currentThread();
        return new ThreadStack(t.getName(), threadIdOf(t), origin, depth > 0 ? new Throwable() : null, depth);
    }

    /** The frames, resolved on first use (the writer thread). */
    synchronized StackTraceElement[] frames() {
        if (frames == null) {
            StackTraceElement[] all = site != null ? site.getStackTrace() : new StackTraceElement[0];
            site = null;
            int start = 0;
            while (start < all.length && isRuntimeFrame(all[start].getClassName())) {
                start++;
            }
            int n = Math.max(0, Math.min(depth, all.length - start));
            frames = new StackTraceElement[n];
            System.arraycopy(all, start, frames, 0, n);
            truncated = all.length - start > n;
        }
        return frames;
    }

    synchronized boolean truncated() {
        frames();
        return truncated;
    }

    /** Frames this stack will have at most, for memory estimates (without resolving it). */
    int sizeEstimate() {
        return depth;
    }

    /** Classes of the capture runtime itself (not apps that happen to share the prefix). */
    static boolean isRuntimeFrame(String className) {
        return className.startsWith("io.trafficpolice.capture.")
                || className.startsWith("io.trafficpolice.boot.")
                || className.equals("io.trafficpolice.TrafficPolice")
                || className.startsWith("io.trafficpolice.TrafficPolice$");
    }

    @SuppressWarnings("deprecation") // Thread.threadId() is Java 19+; getId() is what Android has
    private static long threadIdOf(Thread t) {
        return t.getId();
    }

    void writeThread(Json j) {
        j.key("thread").obj().kv("name", threadName).kv("id", threadId).kv("origin", origin).endObj();
    }

    void writeStack(Json j) {
        j.key("stack").arr();
        for (StackTraceElement f : frames()) {
            j.obj().kv("c", f.getClassName()).kv("m", f.getMethodName());
            if (f.getFileName() != null) {
                j.kv("f", f.getFileName());
            }
            if (f.getLineNumber() >= 0) {
                j.kv("l", f.getLineNumber());
            }
            j.endObj();
        }
        j.endArr();
        j.kv("stack_truncated", truncated());
    }
}
