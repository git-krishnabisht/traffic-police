package io.trafficpolice.capture.core;

import java.util.ArrayList;
import java.util.Collections;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.UUID;

/** The capture runtime of one app process: wiring, configuration, diagnostics, {@code hello}. */
public final class CaptureRuntime {
    public static final String VERSION = "0.4.0";
    public static final int PROTOCOL = 1;

    private static volatile CaptureRuntime current;

    /** How to start the runtime. */
    public static final class Options {
        public String mode = "library";
        public String build;
        /** Client name to detected version (null when absent), e.g. okhttp → 4.12.0. */
        public final Map<String, String> clients = new LinkedHashMap<>();
        public boolean okhttp;
        public int ringTxns = 1000;
        public long ringBodyBytes = 32L * 1024 * 1024;
        public long queueBytes = 8L * 1024 * 1024;
        public CaptureConfig config = CaptureConfig.defaults();
        /** Fixed instance id (tests); random when null. */
        public String instance;
        /** Start the whole-app traffic sampler. */
        public boolean sampleTraffic = true;
        /** Attach mode: the agent's hooks, read for every hello; null in library mode. */
        public AttachState attach;
        public long trafficIntervalMillis = 500;
    }

    final Platform platform;
    final Options options;
    final EventQueue queue;
    final ReplayRing ring;
    final EventWriter writer;
    final Recorder recorder;
    final String instance;
    final long startedTs;
    private final Thread writerThread;
    private final TrafficSampler sampler;
    private final List<String> capabilities = new ArrayList<>();
    private final Set<String> reported = Collections.synchronizedSet(new HashSet<String>());
    private volatile CaptureConfig config;
    /** The connected host's rules; they apply only while that host is connected. */
    private volatile RuleSet rules = RuleSet.EMPTY;
    private Object rulesOwner;

    private CaptureRuntime(Platform platform, Options options) {
        this.platform = platform;
        this.options = options;
        this.config = options.config;
        this.instance = options.instance != null ? options.instance : UUID.randomUUID().toString().replace("-", "");
        this.startedTs = platform.nanoTime();
        this.queue = new EventQueue(options.queueBytes);
        this.ring = new ReplayRing(options.ringTxns, options.ringBodyBytes);
        this.writer = new EventWriter(this, queue, ring);
        this.recorder = new Recorder(this);
        capabilities.add("huc");
        capabilities.add("rules");
        capabilities.add("pause");
        capabilities.add("resume");
        capabilities.add("prog");
        boolean traffic = options.sampleTraffic && platform.trafficCounters() != null;
        if (traffic) {
            capabilities.add("traffic");
        }
        this.writerThread = new Thread(writer, "traffic-police-writer");
        this.writerThread.setDaemon(true);
        this.sampler = traffic ? new TrafficSampler(this, options.trafficIntervalMillis) : null;
    }

    /** Starts the runtime once per process; later calls return the running one. */
    public static synchronized CaptureRuntime start(Platform platform, Options options) {
        if (current != null) {
            return current;
        }
        CaptureRuntime rt = new CaptureRuntime(platform, options);
        rt.writerThread.start();
        if (rt.sampler != null) {
            rt.sampler.start();
        }
        current = rt;
        Platform.AppInfo app = platform.app();
        rt.diag("info", "started", "traffic-police " + VERSION + " (" + options.mode + ") started in "
                + app.processName + " (pid " + app.pid + ")", null);
        // in attach mode OkHttp can load later; hello.hooks says whether it did
        if (!options.okhttp && options.attach == null) {
            rt.diag("info", "okhttp_missing", "OkHttp is not in this process; HttpURLConnection capture only", null);
        }
        return rt;
    }

    /** The running runtime, or null (hooks then pass everything through). */
    public static CaptureRuntime current() {
        return current;
    }

    /** Stops the runtime (tests). */
    public void stop() {
        synchronized (CaptureRuntime.class) {
            if (current == this) {
                current = null;
            }
        }
        if (sampler != null) {
            sampler.stop();
        }
        writer.stop();
        try {
            writerThread.join(2000);
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        }
    }

    public Recorder recorder() {
        return recorder;
    }

    public CaptureConfig config() {
        return config;
    }

    void setConfig(CaptureConfig c) {
        config = c;
    }

    public String instance() {
        return instance;
    }

    RuleSet rules() {
        return rules;
    }

    /** The rules of the host connection {@code owner}; a later connection's replace them. */
    synchronized void setRules(Object owner, RuleSet set) {
        rulesOwner = owner;
        rules = set;
    }

    /**
     * The connection {@code owner} ended: its rules stop applying, so closing traffic-police gives
     * the app its normal behaviour back. A connection that took over keeps its own.
     */
    synchronized void clearRules(Object owner) {
        if (rulesOwner == owner) {
            rulesOwner = null;
            rules = RuleSet.EMPTY;
        }
    }

    public Platform platform() {
        return platform;
    }

    /** Serves one authorized host connection on a new thread. */
    public void serve(Transport transport) {
        Thread t = new Thread(new ClientSession(this, transport), "traffic-police-client");
        t.setDaemon(true);
        t.start();
    }

    /** Whether a host is attached and receiving events. */
    public boolean hostAttached() {
        return writer.hasClient();
    }

    public void diag(String level, String code, String message, Map<String, String> data) {
        queue.offer(new Event.Diag(platform.nanoTime(), level, code, message, data));
    }

    void diagOnce(String code, String level, String message) {
        if (reported.add(code)) {
            diag(level, code, message, null);
        }
    }

    /** A diagnostic reported once per {@code key}. */
    public void diagOnceKeyed(String key, String level, String code, String message, Map<String, String> data) {
        if (reported.add(key)) {
            diag(level, code, message, data);
        }
    }

    /**
     * A bug in our own code. Logged, reported once per site as a diagnostic, and never thrown into
     * the app.
     */
    public void internalError(String site, Throwable t) {
        platform.log(Platform.LOG_WARN, "traffic-police internal error in " + site, t);
        if (reported.add("internal:" + site)) {
            Map<String, String> data = new LinkedHashMap<>();
            data.put("site", site);
            diag("warn", "internal_error", site + ": " + t, data);
        }
    }

    void writeClock(Json j) {
        j.key("clock").obj().kv("ts", platform.nanoTime()).kv("wall_ms", platform.wallMillis()).endObj();
    }

    byte[] helloFrame() {
        Platform.AppInfo app = platform.app();
        Platform.DeviceInfo dev = platform.device();
        Json j = new Json(1024).obj().kv("t", "hello").kv("protocol", PROTOCOL);
        j.key("runtime").obj().kv("version", VERSION);
        if (options.build != null) {
            j.kv("build", options.build);
        }
        j.kv("mode", options.mode).endObj();
        j.kv("instance", instance);
        j.key("app").obj().kv("package", app.packageName).kv("process", app.processName).kv("pid", app.pid)
                .kv("uid", app.uid).kv("debuggable", app.debuggable).endObj();
        j.key("device").obj().kv("api", dev.api);
        if (dev.release != null) {
            j.kv("release", dev.release);
        }
        if (dev.manufacturer != null) {
            j.kv("manufacturer", dev.manufacturer);
        }
        if (dev.model != null) {
            j.kv("model", dev.model);
        }
        if (dev.abi != null) {
            j.kv("abi", dev.abi);
        }
        j.key("abis").arr();
        if (dev.abis != null) {
            for (String a : dev.abis) {
                j.str(a);
            }
        }
        j.endArr().endObj();
        writeClock(j);
        j.kv("started_ts", startedTs);
        AttachState attach = options.attach;
        j.key("capabilities").arr();
        if (options.okhttp || (attach != null && attach.okhttp())) {
            j.str("okhttp").str("okhttp_events");
        }
        for (String c : capabilities) {
            j.str(c);
        }
        j.endArr();
        Map<String, String> clients = new LinkedHashMap<>(options.clients);
        if (attach != null) {
            attach.clients(clients);
        }
        j.key("clients").obj();
        for (Map.Entry<String, String> e : clients.entrySet()) {
            j.kv(e.getKey(), e.getValue());
        }
        j.endObj();
        if (attach != null) {
            j.key("hooks").arr();
            for (AttachState.Hook h : attach.hooks()) {
                j.obj().kv("id", h.id).kv("target", h.target).kv("status", h.status).kv("hits", h.hits);
                if (h.detail != null) {
                    j.kv("detail", h.detail);
                }
                j.endObj();
            }
            j.endArr();
        }
        j.key("buffer");
        ring.writeStats(j);
        j.key("config");
        config.write(j);
        return Frames.json(j.endObj());
    }
}
