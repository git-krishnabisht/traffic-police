package io.trafficpolice.internal;

import io.trafficpolice.capture.attach.ExitHandler;
import io.trafficpolice.capture.core.AttachState;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.Platform;
import io.trafficpolice.capture.core.SocketNames;
import java.lang.reflect.Method;
import java.net.URLConnection;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicLong;

/** Entry point called by the JVMTI agent after its bootstrap dex is installed. */
public final class AttachEntry implements ExitHandler, AttachState {
    // slicer's method labels: dotted class name, "->", method name, JNI signature
    private static final String URL_OPEN_CONNECTION = "java.net.URL->openConnection()Ljava/net/URLConnection;";
    private static final String URL_OPEN_CONNECTION_PROXY =
            "java.net.URL->openConnection(Ljava/net/Proxy;)Ljava/net/URLConnection;";
    private static final String OKHTTP_NETWORK_INTERCEPTORS =
            "okhttp3.OkHttpClient->networkInterceptors()Ljava/util/List;";
    private static final String OKHTTP_EVENT_LISTENER_FACTORY =
            "okhttp3.OkHttpClient->eventListenerFactory()Lokhttp3/EventListener$Factory;";

    private final String runtimeDir;
    private final String packageName;
    private final byte[] runtimeDex;
    private final Map<String, MutableHook> hooks = new LinkedHashMap<>();
    private volatile ClassLoader appLoader;
    private volatile ClassLoader adapterLoader;
    /** OkHttpHooks' two methods, looked up once: the OkHttp hooks run for every call. */
    private volatile Method networkInterceptorsHook;
    private volatile Method eventListenerFactoryHook;
    private volatile CaptureRuntime runtime;
    private volatile AndroidPlatform platform;
    /** The entry {@link #start} made, for {@link #listen}. */
    private static volatile AttachEntry started;

    /**
     * Invoked by the agent's initialization thread: starts the runtime, without its socket yet.
     * The agent then installs the returned handler, hooks the classes loaded so far, and calls
     * {@link #listen}.
     */
    public static ExitHandler start(String runtimeDir, String packageName, byte[] runtimeDex) {
        AttachEntry entry = new AttachEntry(runtimeDir, packageName, runtimeDex);
        entry.startRuntime();
        started = entry;
        return entry;
    }

    /**
     * Invoked by the agent once the hooks are in place: opens the capture socket, so a host that
     * connects sees the hooks as they are and nothing the app does after that is missed.
     */
    public static void listen() {
        AttachEntry entry = started;
        if (entry != null) entry.openSocket();
    }

    private AttachEntry(String runtimeDir, String packageName, byte[] runtimeDex) {
        this.runtimeDir = runtimeDir;
        this.packageName = packageName;
        this.runtimeDex = runtimeDex;
        addHook("url_open_connection", "java.net.URL#openConnection()Ljava/net/URLConnection;");
        addHook("url_open_connection_proxy",
                "java.net.URL#openConnection(Ljava/net/Proxy;)Ljava/net/URLConnection;");
        addHook("okhttp_network_interceptors",
                "okhttp3.OkHttpClient#networkInterceptors()Ljava/util/List;");
        addHook("okhttp_event_listener_factory",
                "okhttp3.OkHttpClient#eventListenerFactory()Lokhttp3/EventListener$Factory;");
    }

    private void startRuntime() {
        String[][] snapshot = nativeSnapshot();
        if (snapshot != null) {
            for (String[] row : snapshot) {
                if (row != null && row.length >= 3) {
                    onHook(row[0], row[1], row[2], row.length > 3 ? row[3] : null);
                }
            }
        }
        AndroidPlatform platform = new AndroidPlatform(packageName);
        CaptureRuntime.Options options = new CaptureRuntime.Options();
        options.mode = "attach";
        options.attach = this;
        this.runtime = CaptureRuntime.start(platform, options);
        this.platform = platform;
    }

    private void openSocket() {
        CaptureRuntime rt = runtime;
        AndroidPlatform p = platform;
        if (rt == null || p == null) return;
        String name = SocketNames.forProcess(p.app().packageName, p.app().pid);
        LocalSocketServer.start(rt, name);
        android.util.Log.i(AndroidRuntime.TAG, "attach capture started in " + p.app().processName
                + "; socket @" + name + "; runtime files " + runtimeDir);
    }

    private static String[][] nativeSnapshot() {
        try {
            Class<?> bridge = Class.forName("io.trafficpolice.boot.NativeBridge", true, null);
            return (String[][]) bridge.getMethod("snapshot").invoke(null);
        } catch (Throwable ignored) {
            return null;
        }
    }

    @Override
    public boolean okhttp() {
        synchronized (hooks) {
            return canUse("okhttp_network_interceptors") || canUse("okhttp_event_listener_factory");
        }
    }

    private boolean canUse(String id) {
        MutableHook hook = hooks.get(id);
        return hook != null && ("pending".equals(hook.status) || "installed".equals(hook.status));
    }

    @Override
    public void clients(Map<String, String> into) {
        ClassLoader loader = adapterLoader;
        if (loader == null) {
            into.put("okhttp", null);
            return;
        }
        try {
            Class<?> detect = Class.forName("io.trafficpolice.capture.okhttp.OkHttpDetect", true, loader);
            Object version = detect.getMethod("version").invoke(null);
            into.put("okhttp", version != null ? String.valueOf(version) : null);
        } catch (Throwable ignored) {
            into.put("okhttp", null);
        }
    }

    @Override
    public List<Hook> hooks() {
        synchronized (hooks) {
            List<Hook> result = new ArrayList<>(hooks.size());
            for (MutableHook h : hooks.values()) {
                result.add(new Hook(h.id, h.target, h.status, h.hits.get(), h.detail));
            }
            return Collections.unmodifiableList(result);
        }
    }

    @Override
    public Object onExit(String method, Object result) {
        // Pausing recording does not stop response rules in the capture interceptor, so the hooks
        // stay in place while paused and the capture runtime decides what to record.
        if (method == null || runtime == null) return result;
        boolean proxy = URL_OPEN_CONNECTION_PROXY.equals(method);
        if (proxy || URL_OPEN_CONNECTION.equals(method)) {
            hit(proxy ? "url_open_connection_proxy" : "url_open_connection");
            try {
                return io.trafficpolice.capture.huc.Huc.wrap((URLConnection) result);
            } catch (Throwable t) {
                reportError("attach.url", t);
                return result;
            }
        }
        boolean interceptors = OKHTTP_NETWORK_INTERCEPTORS.equals(method);
        if (!interceptors && !OKHTTP_EVENT_LISTENER_FACTORY.equals(method)) return result;
        hit(interceptors ? "okhttp_network_interceptors" : "okhttp_event_listener_factory");
        try {
            Method hook = interceptors ? networkInterceptorsHook : eventListenerFactoryHook;
            if (hook == null) {
                // no adapter: OkHttp's loader was not reported, or loading the adapter failed (reported)
                if (!resolveOkHttpHooks()) return result;
                hook = interceptors ? networkInterceptorsHook : eventListenerFactoryHook;
            }
            return hook.invoke(null, result);
        } catch (Throwable t) {
            reportError(interceptors ? "attach.okhttp.networkInterceptors" : "attach.okhttp.eventListenerFactory", t);
            return result;
        }
    }

    /** Looks up OkHttpHooks' methods in the adapter; false while no adapter is loaded. */
    private boolean resolveOkHttpHooks() throws ReflectiveOperationException {
        ClassLoader adapter = adapterLoader;
        if (adapter == null) return false;
        Class<?> hooksClass = Class.forName("io.trafficpolice.capture.okhttp.OkHttpHooks", true, adapter);
        Class<?> factory = Class.forName("okhttp3.EventListener$Factory", false, appLoader);
        eventListenerFactoryHook = hooksClass.getMethod("eventListenerFactory", factory);
        networkInterceptorsHook = hooksClass.getMethod("networkInterceptors", List.class);
        return true;
    }

    @Override
    public void onOkHttpLoader(ClassLoader loader) {
        if (loader == null || adapterLoader != null) return;
        synchronized (this) {
            if (adapterLoader != null) return;
            try {
                ClassLoader parent = new AdapterParent(loader, AttachEntry.class.getClassLoader());
                // not read-only: the loader reads the buffer's backing array, which a read-only view refuses
                ClassLoader adapter = new dalvik.system.InMemoryDexClassLoader(ByteBuffer.wrap(runtimeDex), parent);
                appLoader = loader;
                adapterLoader = adapter;
            } catch (Throwable t) {
                // OkHttp then goes uncaptured: its hooks pass everything through
                reportError("attach.okhttp.adapter", t);
            }
        }
    }

    @Override
    public void onHook(String id, String target, String status, String detail) {
        String changedTo = null;
        String hookTarget;
        synchronized (hooks) {
            MutableHook hook = hooks.get(id);
            if (hook == null) return;
            hook.target = target != null ? target : hook.target;
            if (status != null && !status.equals(hook.status)) {
                hook.status = status;
                changedTo = status;
            }
            hook.detail = detail;
            hookTarget = hook.target;
        }
        CaptureRuntime rt = runtime;
        if (changedTo == null || rt == null || "pending".equals(changedTo)) return;
        // PROTOCOL.md §7.2: hook changes after hello reach the host as diag events
        Map<String, String> data = new LinkedHashMap<>();
        data.put("hook", id);
        data.put("status", changedTo);
        if ("installed".equals(changedTo)) {
            rt.diag("info", "hook_installed", "hooked " + hookTarget, data);
        } else {
            rt.diag("warn", "hook_failed", "not hooked (" + changedTo + "): " + hookTarget
                    + (detail != null ? ": " + detail : ""), data);
        }
    }

    @Override
    public void onDiag(String level, String code, String message) {
        CaptureRuntime rt = runtime;
        if (rt != null) rt.diag(level, code, message, null);
    }

    private void addHook(String id, String target) {
        hooks.put(id, new MutableHook(id, target));
    }

    private void hit(String id) {
        MutableHook hook = hooks.get(id);
        if (hook != null) hook.hits.incrementAndGet();
    }

    private void reportError(String site, Throwable error) {
        CaptureRuntime rt = runtime;
        if (rt != null) rt.internalError(site, error);
    }

    private static final class MutableHook {
        final String id;
        final AtomicLong hits = new AtomicLong();
        String target;
        String status = "pending";
        String detail;

        MutableHook(String id, String target) {
            this.id = id;
            this.target = target;
        }
    }

    /** Routes the runtime core and lets the dex loader define its adapter classes itself. */
    private static final class AdapterParent extends ClassLoader {
        private final ClassLoader appLoader;
        private final ClassLoader coreLoader;

        AdapterParent(ClassLoader appLoader, ClassLoader coreLoader) {
            super(null);
            this.appLoader = appLoader;
            this.coreLoader = coreLoader;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.startsWith("io.trafficpolice.capture.core.")) {
                return coreLoader.loadClass(name);
            }
            if (name.startsWith("io.trafficpolice.capture.okhttp.")) {
                throw new ClassNotFoundException(name);
            }
            return appLoader.loadClass(name);
        }
    }
}
