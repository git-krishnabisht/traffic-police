package io.trafficpolice.capture.attach;

/** Bootstrap-safe callbacks implemented by the dynamically loaded attach runtime. */
public interface ExitHandler {
    Object onExit(String method, Object result);

    void onOkHttpLoader(ClassLoader loader);

    /** The class loader of the app's gRPC (the first that defined a hooked gRPC class). */
    void onGrpcLoader(ClassLoader loader);

    void onHook(String id, String target, String status, String detail);

    /** A diagnostic from the native agent, with a PROTOCOL.md §7.1 code. */
    void onDiag(String level, String code, String message);
}
