package io.trafficpolice.capture.core;

/**
 * What the runtime needs from the operating system. Android supplies {@code SystemClock},
 * {@code TrafficStats} and friends; JVM tests supply a deterministic fake.
 */
public interface Platform {
    int LOG_DEBUG = 3;
    int LOG_INFO = 4;
    int LOG_WARN = 5;
    int LOG_ERROR = 6;

    /** Device monotonic time in nanoseconds (Android: {@code SystemClock.elapsedRealtimeNanos()}). */
    long nanoTime();

    /** Wall clock in milliseconds, for labels only. */
    long wallMillis();

    /** Cumulative {rx, tx} bytes of the app's uid, or null when the platform cannot tell. */
    long[] trafficCounters();

    AppInfo app();

    DeviceInfo device();

    void log(int level, String message, Throwable error);

    /** The app process as reported in {@code hello}. */
    final class AppInfo {
        public final String packageName;
        public final String processName;
        public final int pid;
        public final int uid;
        public final boolean debuggable;

        public AppInfo(String packageName, String processName, int pid, int uid, boolean debuggable) {
            this.packageName = packageName;
            this.processName = processName;
            this.pid = pid;
            this.uid = uid;
            this.debuggable = debuggable;
        }
    }

    /** The device as reported in {@code hello}. */
    final class DeviceInfo {
        public final int api;
        public final String release;
        public final String manufacturer;
        public final String model;
        public final String abi;
        public final String[] abis;

        public DeviceInfo(int api, String release, String manufacturer, String model, String abi, String[] abis) {
            this.api = api;
            this.release = release;
            this.manufacturer = manufacturer;
            this.model = model;
            this.abi = abi;
            this.abis = abis;
        }
    }
}
