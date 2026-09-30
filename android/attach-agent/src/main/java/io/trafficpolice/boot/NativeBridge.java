package io.trafficpolice.boot;

/** JNI bridge whose class is loaded from the bootstrap search path. */
public final class NativeBridge {
    private NativeBridge() {}

    /** Rows are [id, target, status, detail]. */
    public static native String[][] snapshot();

    public static native void setActive(boolean active);
}
