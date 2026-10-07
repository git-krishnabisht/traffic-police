package io.trafficpolice.capture.grpc;

import java.lang.reflect.Field;

/** gRPC's version, for the hello's client list and the {@code client} of each call. */
public final class GrpcDetect {
    private static volatile String version;
    private static volatile boolean looked;

    private GrpcDetect() {}

    /** The grpc-java version in this process (e.g. 1.84.0), or null when it cannot be read. */
    public static String version() {
        if (!looked) {
            String v = null;
            try {
                // public from 1.33; private before (a constant, so reflection still reads it)
                Field f = Class.forName("io.grpc.internal.GrpcUtil", false, GrpcDetect.class.getClassLoader())
                        .getDeclaredField("IMPLEMENTATION_VERSION");
                f.setAccessible(true);
                Object o = f.get(null);
                v = o instanceof String ? (String) o : null;
            } catch (Throwable ignored) {
                // a minified or shaded gRPC: the version stays unknown
            }
            version = v;
            looked = true;
        }
        return version;
    }
}
