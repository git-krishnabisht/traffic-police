package io.trafficpolice.capture.core;

import java.lang.reflect.Method;

/**
 * The kernel's id of the current thread, the one logcat shows (PROTOCOL.md §4 {@code thread}):
 * {@code android.os.Process.myTid()}, asked once per thread and kept in a thread local. Off
 * Android (the JVM tests) there is none.
 */
final class Tids {
    private static final Method MY_TID = find();

    private static final ThreadLocal<Integer> TID = new ThreadLocal<Integer>() {
        @Override
        protected Integer initialValue() {
            return ask();
        }
    };

    private Tids() {}

    /** The current thread's tid, or -1. */
    static int current() {
        return MY_TID == null ? -1 : TID.get();
    }

    private static Method find() {
        try {
            return Class.forName("android.os.Process").getMethod("myTid");
        } catch (Throwable t) {
            return null;
        }
    }

    private static int ask() {
        try {
            return (Integer) MY_TID.invoke(null);
        } catch (Throwable t) {
            return -1;
        }
    }
}
