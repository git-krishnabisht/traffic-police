package io.trafficpolice.boot;

import io.trafficpolice.capture.attach.ExitHandler;
import java.util.concurrent.ConcurrentHashMap;

/** Small bootstrap-visible dispatch point inserted into boot and app dex methods. */
public final class Trampoline {
    /** Read by the agent (kVersion in agent.cpp): a second agent finds it and stays inactive. */
    public static final String VERSION = "0.2.0";

    private static volatile ExitHandler handler;
    private static volatile ClassLoader okhttpLoader;
    private static volatile ClassLoader grpcLoader;
    private static final ThreadLocal<Boolean> IN_HOOK = new ThreadLocal<Boolean>();
    private static final ConcurrentHashMap<String, Boolean> REPORTED = new ConcurrentHashMap<String, Boolean>();

    private Trampoline() {}

    public static Object onExit(String method, Object value) {
        ExitHandler current = handler;
        if (current == null || Boolean.TRUE.equals(IN_HOOK.get())) return value;
        IN_HOOK.set(Boolean.TRUE);
        try {
            return current.onExit(method, value);
        } catch (Throwable t) {
            failed("onExit", t);
            return value;
        } finally {
            IN_HOOK.remove();
        }
    }

    public static void onOkHttpLoader(ClassLoader loader) {
        if (loader == null || okhttpLoader != null) return;
        synchronized (Trampoline.class) {
            if (okhttpLoader == null) {
                okhttpLoader = loader;
                ExitHandler current = handler;
                if (current != null) {
                    try {
                        current.onOkHttpLoader(loader);
                    } catch (Throwable t) {
                        // Instrumentation must never change the app's class-loading result.
                        failed("onOkHttpLoader", t);
                    }
                }
            }
        }
    }

    public static void onGrpcLoader(ClassLoader loader) {
        if (loader == null || grpcLoader != null) return;
        synchronized (Trampoline.class) {
            if (grpcLoader == null) {
                grpcLoader = loader;
                ExitHandler current = handler;
                if (current != null) {
                    try {
                        current.onGrpcLoader(loader);
                    } catch (Throwable t) {
                        // Instrumentation must never change the app's class-loading result.
                        failed("onGrpcLoader", t);
                    }
                }
            }
        }
    }

    public static void onHook(String id, String target, String status, String detail) {
        ExitHandler current = handler;
        if (current == null) return;
        try {
            current.onHook(id, target, status, detail);
        } catch (Throwable t) {
            failed("onHook", t);
        }
    }

    public static void onDiag(String level, String code, String message) {
        ExitHandler current = handler;
        if (current == null) return;
        try {
            current.onDiag(level, code, message);
        } catch (Throwable t) {
            failed("onDiag", t);
        }
    }

    /** Installs the handler returned by the native agent's runtime-dex loader. */
    public static synchronized void installHandler(ExitHandler current) {
        if (handler != null || current == null) return;
        handler = current;
        ClassLoader clientLoader = okhttpLoader;
        if (clientLoader != null) {
            try {
                current.onOkHttpLoader(clientLoader);
            } catch (Throwable t) {
                // Instrumentation must never change the app's class-loading result.
                failed("onOkHttpLoader", t);
            }
        }
        ClassLoader rpcLoader = grpcLoader;
        if (rpcLoader != null) {
            try {
                current.onGrpcLoader(rpcLoader);
            } catch (Throwable t) {
                failed("onGrpcLoader", t);
            }
        }
        try {
            String[][] snapshot = NativeBridge.snapshot();
            if (snapshot != null) {
                for (String[] row : snapshot) {
                    if (row != null && row.length >= 3) {
                        current.onHook(row[0], row[1], row[2], row.length > 3 ? row[3] : null);
                    }
                }
            }
        } catch (Throwable t) {
            // The hook table remains useful through the individual class-load callbacks.
            failed("hook snapshot", t);
        }
        NativeBridge.setActive(true);
    }

    /**
     * The handler reports its own errors through the runtime, so one that reaches here is a bug in
     * it. Printed once per site (System.err goes to logcat) and never thrown into the app.
     */
    private static void failed(String site, Throwable t) {
        if (REPORTED.putIfAbsent(site, Boolean.TRUE) == null) {
            System.err.println("traffic-police: the attach handler failed in " + site);
            t.printStackTrace();
        }
    }
}
