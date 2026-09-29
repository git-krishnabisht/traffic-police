# traffic-police architecture

Status: **draft for review** (Phase 0). PROTOCOL.md defines the wire format; this document defines everything else. Statements about Android, ART, OkHttp, adb and Android Studio were checked against their sources during Phase 0; §11 lists where, and every open question is in §9.

Contents

1. Goals and constraints
2. System overview
3. Repository layout
4. Device side
5. Host side (Rust)
6. Performance budgets
7. Security and privacy
8. Testing and CI
9. Decisions to review, deviations, risks
10. Phase map
11. Sources checked

---

## 1. Goals and constraints

- A terminal equivalent of Android Studio's Network Inspector: pick a device and a debuggable app process over adb, then watch its HTTP and HTTPS traffic live with a traffic graph, a connection list with a waterfall, full request and response details, the initiating thread and call stack, a thread view, and response rewrite rules. Then add terminal-native utilities (filters, search, copy as cURL, HAR, sessions, diff, decoders, redaction, headless modes).
- **Capture model: in-process hooks, like Studio.** No proxy and no certificates. Plaintext is observed inside the app at the HTTP-client layer, which also yields the initiating thread and stack and allows response rewriting.
- **Two ways in, one runtime.** Library mode (the app links the capture library in debug builds) and attach mode (a JVMTI agent injects the same runtime into an unmodified debuggable app).
- **Coverage:** OkHttp 3.9+ / 4.x / 5.x (and everything built on it: Retrofit, Ktor's OkHttp engine, Coil, Glide) and HttpURLConnection/HttpsURLConnection (and what is built on it, such as Volley). Not covered, as in Studio: Cronet, WebView, native sockets, Dart `dart:io`.
- **Hard constraints:** capture only in debuggable apps; never block or crash an app thread; never change app-visible behaviour unless a rule says so; one event model for every source; the UI reads only the store; the host talks to nothing but adb.

## 2. System overview

```
 ┌───────────────────────── Android device ─────────────────────────┐         ┌─────────────────────────────── host ───────────────────────────────┐
 │  app process (debuggable)                                        │         │                                                                    │
 │  ┌────────────────────────────────────────────────────────────┐  │         │ traffic-police-adb ──► adb server (127.0.0.1:5037) ◄── USB / TCP   │
 │  │ OkHttp ──► CaptureInterceptor ─┐                           │  │         │  │ track-devices, track-app/jdwp, shell, forward, push             │
 │  │   └─► CaptureEventListener ────┤                           │  │  adbd   │  ▼                                                                 │
 │  │ HttpURLConnection wrappers ────┼─► Recorder ─► EventQueue  │  │ ◄─────► │ traffic-police-backends: device │ demo │ session file │ HAR        │
 │  │                                │    (bounded, drop oldest) │  │ forward │  │ PROTOCOL.md frames → SessionEvent batches                       │
 │  │ RulesEngine ◄── set_rules ─────┘         │                 │  │         │  ▼                                                                 │
 │  │                     writer thread ◄──────┘                 │  │         │ traffic-police-core: SessionStore (txns, bodies, traffic)          │
 │  │                           │ encode, seq, ReplayRing        │  │         │  │                           ▲ commands (rules, pause, ping)       │
 │  │                           ▼                                │  │         │  ▼                           │                                     │
 │  │ LocalServerSocket @traffic-police_<pkg>_<pid>              │  │         │ traffic-police-tui (Ratatui)  or  headless (tail / record / export)│
 │  │   (accepts peers with UID 0 or 2000 only)                  │  │         │                                                                    │
 │  └────────────────────────────────────────────────────────────┘  │         │ .traffic-police/rules.toml (watched)    config.toml                │
 │  library mode: linked into the app's debug build                 │         │                                                                    │
 │  attach mode: JVMTI agent + boot trampoline + runtime dex        │         │                                                                    │
 └──────────────────────────────────────────────────────────────────┘         └────────────────────────────────────────────────────────────────────┘
```

Data flow for one request: hook → `Recorder` event (app thread, microseconds) → queue → writer thread (encode, `seq`, ring, socket) → adbd → adb server → host backend (decode, normalize) → store (apply, index) → UI (render visible rows at ≤ 30 fps).

## 3. Repository layout

```
docs/                    ARCHITECTURE.md, PROTOCOL.md, research notes
host/                    Cargo workspace (Rust 2024 edition); crate names in parentheses
  crates/proto/          (traffic-police-proto)     wire codec and message types
  crates/core/           (traffic-police-core)      event model, Backend trait, store, filters, rules model, decoders, exporters, redaction
  crates/adb/            (traffic-police-adb)       adb server client and discovery
  crates/backends/       (traffic-police-backends)  demo, device socket, session file, HAR import, attach orchestration
  crates/tui/            (traffic-police-tui)       Ratatui application
  cli/                   (traffic-police)           the `traffic-police` binary
android/                 Gradle build (Kotlin DSL)
  capture/               capture runtime (Java 8 library, AAR)
  capture-noop/          same public API, no capture code
  attach-agent/          JVMTI agent (C++17, NDK, CMake) + vendored slicer + boot trampoline dex
  sample-app/            Kotlin app exercising every capture path
testdata/protocol/v1/    golden frames shared by Java and Rust tests
.github/workflows/       CI
```

## 4. Device side

### 4.1 Capture runtime: layers

Java source level 8, no third-party runtime dependencies, public SDK APIs only. OkHttp and Okio are `compileOnly` and always resolve to the app's copies. The runtime is split so that nothing touches `okhttp3.*` unless OkHttp is present:

| Layer | Package | May reference | Contents |
|---|---|---|---|
| Public API | `io.trafficpolice` | Android SDK; OkHttp types only in the signatures of the OkHttp methods | `TrafficPolice` facade, `TrafficPoliceInitProvider` |
| Core | `io.trafficpolice.capture.core` | `java.*`, `android.*` | `Recorder` (the capture API every hook calls), transaction ids, `EventQueue` (bounded, drop-oldest), `Writer` thread, `ReplayRing`, frame encoder and a hand-written JSON writer, `SocketServer` (LocalServerSocket, peer check, handshake), `CommandReader`, `RulesEngine` (matching and actions over a client-neutral response model), `CaptureConfig`, `Clock`, `StackCapture`, `Diagnostics`, `TrafficSampler` |
| HttpURLConnection | `io.trafficpolice.capture.huc` | core, `java.net`, `javax.net.ssl` | `TrackedHttpURLConnection`, `TrackedHttpsURLConnection`, stream tees |
| OkHttp adapter | `io.trafficpolice.capture.okhttp` | core, `okhttp3`, `okio` | `CaptureInterceptor`, `CaptureEventListener` and `ForwardingEventListener`, `CallRegistry`, `TeeRequestBody`, `TeeResponseBody`, `OkHttpRules`, `OkHttpCompat` (feature detection) |
| Attach entry | `io.trafficpolice.capture.attach` | core | `AttachEntry` (called by the agent), `HookHandlers` |
| Boot trampoline (attach only, separate dex) | `io.trafficpolice.boot` | `java.lang` only | `Trampoline`, `ExitHandler` (4.7.3) |

The OkHttp adapter is compiled in two source sets merged into one artifact: everything against **OkHttp 3.14.9 + Okio 1.13.0** (the API baseline OkHttp 4 checks binary compatibility against, and the oldest Okio any supported OkHttp ships), except `ForwardingEventListener`, which is compiled against **OkHttp 5.5.0** so it can override and forward all 33 callbacks. A bytecode-reference check in CI fails the build if the core unit references a member missing from OkHttp 3.9.0 or Okio 1.13.0 outside a guarded list.

**Supported OkHttp versions: 3.9.0 and later** (the first release with a public `EventListener` and `Chain.call()`). Older OkHttp 3.x (2015–2017) is detected and left alone with a `diag`. OkHttp 2 (`com.squareup.okhttp`) is out of scope; Android's own HttpURLConnection, which is built on an internal OkHttp 2 fork, is covered through 4.3.

Every hook body catches `Throwable` from our own code and falls back to pass-through. Exceptions that are the app's or the network's (for example an `IOException` from `proceed()`) are recorded and rethrown unchanged. OkHttp delivers non-`IOException`s thrown inside an interceptor of an async call to the uncaught-exception handler, which crashes the app, so our code must never throw one.

### 4.2 Capturing OkHttp

**Where we sit.** OkHttp's chain is the same in 3.x, 4.x and 5.x: application interceptors → RetryAndFollowUp → Bridge → Cache → Connect → **network interceptors** → CallServer. Network interceptors therefore see wire-accurate headers (after Bridge added `Accept-Encoding`, cookies, `Host`), run once per network attempt (each redirect or retry hop separately), never run for cache hits, and never run for WebSocket handshakes (`if (!forWebSocket)`). This matches Studio, whose interceptor sits in the same place.

**Two cooperating parts.**

1. `CaptureEventListener`, created per call by `TrafficPolice.eventListenerFactory(existing)` (library mode) or by the wrapped `eventListenerFactory()` (attach mode). `callStart()` runs on the caller's thread for both `execute()` and `enqueue()` in every supported version, so it records the real initiating thread and stack. Its other callbacks record timing marks (DNS, connect, TLS, connection acquired, request headers and body, response headers and body, call end or failure). The listener is registered in `CallRegistry` (a synchronized `WeakHashMap<Call, CaptureEventListener>`; `RealCall` uses identity equality, and the listener never references the call strongly, so abandoned calls are collected). On OkHttp 5.x, connect events can fire on background threads (fast fallback), so the listener never correlates by thread.
2. `CaptureInterceptor`, the network interceptor. It finds the call's listener through `chain.call()` (the same `RealCall` the listener received), creates the transaction, and pulls any marks recorded before it ran (DNS and connect happen in ConnectInterceptor, before network interceptors). Marks that happen during the exchange are attributed to the current transaction; a redirect's second hop gets its own transaction with the same `call` id and `hop = 1`.

Without the listener (app did not install it in library mode), the interceptor records its own thread and stack (`thread.origin = "interceptor"`, which for `enqueue()` is an OkHttp dispatcher thread) and only interceptor-level marks.

**Composing with the app's own listener.** On OkHttp ≥ 5.3 (detected once with `getMethod("plus", EventListener.class)`), the factory returns `appListener.plus(ours)`, so OkHttp's own aggregate forwards every callback of that runtime, including future ones. Below 5.3 it returns `ForwardingEventListener(appListener, ours)`, which overrides all 33 callbacks known up to 5.5 (callbacks the runtime does not have are simply never called). A wrapper compiled against an older API would silently stop forwarding newer callbacks to the app's listener, which is why this one class is compiled against 5.5.

**Interceptor flow.**

```
intercept(chain):
  if capture is off (runtime stopped, paused, or version unsupported): return chain.proceed(chain.request())
  req  = chain.request()                       // the wire request (after Bridge)
  txn  = recorder.requestStarted(req line, ordered headers, client info, thread+stack, early marks, chain.connection() details)
  plan = rules.match(req)                      // may be empty
  plan.delayBefore() / plan.failBefore()       // sleep, or throw before proceed(): proceed() is then never called
  req' = req.body() == null or not capturing ? req : req.newBuilder().method(req.method(), TeeRequestBody(req.body(), txn)).build()
  try   resp = chain.proceed(req')             // exactly once
  catch IOException e: recorder.failed(txn, e, phase); rethrow
  recorder.responseStarted(txn, code, message, protocol, ordered headers, connection/TLS)
  if upgrade (101, or Connection: upgrade on both sides): recorder.done(txn); return resp   // never tee or replace an upgrade body
  resp = OkHttpRules.apply(plan, resp, txn)   // status/header/body rewrites (4.4); records the original
  body = resp.body()                           // nullable on 3.x/4.x, non-null on 5.x
  if no body or HEAD/204/304: recorder.bodyEnd(txn, response, none); recorder.done(txn); return resp
  return resp.newBuilder().body(TeeResponseBody(body, txn)).build()
```

- **Request bodies** are captured by teeing the sink while OkHttp writes them: `TeeRequestBody.writeTo(sink)` calls `delegate.writeTo(Okio.buffer(new TeeSink(sink)))`. `contentType()` and `contentLength()` delegate unchanged, so Bridge's `Content-Length` still holds. `isOneShot()` and `isDuplex()` are overridden and delegated (the methods exist from 3.14; on older runtimes OkHttp never calls them, and our own code treats them as `false`). `writeTo()` is never called a second time, so one-shot and duplex bodies behave as without the inspector, and `req_body_end` marks when the body actually finished writing. Studio instead calls `writeTo()` into a null sink before `proceed()`, which writes non-repeatable bodies twice and breaks on OkHttp 3.12.
- **Response bodies** are teed as the app reads them: `TeeResponseBody` keeps the delegate's `contentType()` and `contentLength()` and returns `Okio.buffer(new TeeSource(delegate.source()))`. `TeeSource.read()` copies what was read (`sink.copyTo(capture, sink.size() - n, n)`), so it adds no extra reads and buffers at most one segment ahead, exactly like the app's own buffered source. EOF emits `body_end { state: complete|truncated }` and `done`; `close()` before EOF emits `closed_early`; a read exception emits `fail { phase: "response_body" }`.
- **Headers** are read with `size()/name(i)/value(i)` so order, case and duplicates survive (`Headers.toMultimap()` lowercases names; 4.x/5.x `Headers` iterates `kotlin.Pair`). The response is never rebuilt when no rule applies: only its body is swapped for the tee.
- **Connection details** come from `chain.connection()` (non-null in network interceptors): `protocol().toString()`, `route().socketAddress()` (the proxy's address when a proxy is used; `route().proxy()` says which), `handshake()` (null for cleartext) with `tlsVersion().javaName()`, `cipherSuite().javaName()` and `peerCertificates()`.
- **Things we never do:** implement `Interceptor.Chain`, `Call`, `BufferedSource` or `BufferedSink`; call `okhttp3.internal.*` or Kotlin-mangled `$okhttp` members; read `networkResponse()`/`cacheResponse()`/`priorResponse()` bodies (5.x throws); pass `null` to `Response.Builder.body()` (5.x throws); reference enum constants newer than 3.9 (`Protocol.QUIC`, `H2_PRIOR_KNOWLEDGE`, `HTTP_3`); call Okio's `getBuffer()`, `peek()`, `copy()` or two-argument `copyTo`.
- **Version detection** is reflective (`okhttp3.OkHttp.VERSION` on 4.7+, `okhttp3.internal.Version.userAgent` on 4.0–4.6, `okhttp3.internal.Version.userAgent()` on 3.x) and only used for display; behaviour uses feature detection.
- **WebSockets.** Handshakes bypass network interceptors, and OkHttp builds the WebSocket client with `eventListener(EventListener.NONE)`, which drops a library-mode factory. In attach mode the hooked `eventListenerFactory()` getter still wraps that `NONE` factory, so the 101 handshake becomes visible from `requestHeadersEnd` and `responseHeadersEnd` (headers only). This is an inference from the sources, to be confirmed in Phase 4. Library mode does not see WebSocket handshakes.

### 4.3 Capturing HttpURLConnection

`TrafficPolice.wrap(connection)` (library mode) and the `URL.openConnection()` exit hooks (attach mode) return a `TrackedHttpsURLConnection` or `TrackedHttpURLConnection` that subclasses the platform type and delegates every method. Other `URLConnection` types (`file:`, `jar:`) are returned unchanged. The state machine follows Studio's, which encodes years of HttpURLConnection quirks, with fixes for the gaps its sources show:

- The transaction starts on the first call that connects: `connect()`, `getOutputStream()`, `getInputStream()`, or any response getter (`getResponseCode()`, `getHeaderField*()`, `getContent*()`, …). The method is recorded as `POST` when `doOutput` is set on a `GET` (HttpURLConnection only switches after connecting). Thread and stack are captured there, on the app's thread.
- Request bodies are teed from `getOutputStream()`. The request body ends at stream close, or, if the app never closes it, when the response is first touched.
- **Status and headers are read before `getInputStream()`**, so 4xx/5xx responses keep their status and headers even though `getInputStream()` throws `FileNotFoundException`. `getErrorStream()` is teed too (Studio passes it through, so error bodies are never captured).
- The response body is teed from `getInputStream()`/`getErrorStream()`; EOF, `close()` and `disconnect()` all end it. `disconnect()` always ends the transaction, even if no stream was opened.
- Both overloads of `URL.openConnection` are hooked in attach mode: `openConnection(Proxy)` does not delegate to the no-arg overload in libcore, so Studio misses proxied connections.
- Android's HttpURLConnection already removes its own transparent gzip before the app reads, so HUC response bodies are captured decoded, with headers that match.

### 4.4 Rules on the device

Rules (PROTOCOL.md §8) are evaluated in the process, because only there can a response be changed before the app sees it. `RulesEngine` is client-neutral; `OkHttpRules` and the HUC wrapper adapt it.

- Matching uses the request as it goes on the wire (method, scheme, host, port with 80/443 defaults filled in, encoded path, decoded query parameters). Studio's matcher compares `URL.getPort()`, which is `-1` for default ports, so a port criterion of 443 never matches there; ours normalizes.
- `delay` sleeps before `proceed()`. `fail` throws before `proceed()` (so `proceed()` runs zero times, which OkHttp allows). The exception kinds are `timeout` (`SocketTimeoutException`), `io` (`IOException`), `protocol` (`ProtocolException`), `unknown_host` (`UnknownHostException`) and `connect` (`ConnectException`). OkHttp's retry-on-connection-failure may retry `io` and `connect` failures (running the rule again as a new attempt); `timeout` and `protocol` are never retried. The docs and the Rules view say so.
- Status and header actions rebuild the status line and headers without buffering the body.
- Body actions read the original body completely (limit 32 MiB), decode `gzip` and `deflate` per `Content-Encoding` (`br` and `zstd` cannot be decoded on the device without libraries: the action is skipped with a `diag`), close the original body (OkHttp refuses to continue a call whose previous body is still open), and deliver the new body with `Content-Encoding` removed and `Content-Length` set. Bridge's transparent gzip then leaves it alone, and `ResponseBody.bytes()` length checks hold (Studio never fixes `Content-Length`, which breaks `bytes()` on non-gzip bodies).
- `replace` is literal unless `regex = true`; a literal replacement never interprets `$` or `\` (Studio always treats the replacement as a regex template).
- OkHttp stores what leaves the network interceptors in its HTTP cache, so a rewritten response could outlive the rule. By default a rule that changes the status, headers or body also sets `Cache-Control: no-store` on the delivered response and lists that change in its `rule` event; `cache_rewrites = true` on a rule turns this off.
- Rules are applied without a global lock around body reads (Studio folds all rules under one monitor, so a slow body blocks every other response).
- Every application is reported (`rule` event) with the original kept (Studio reports only HttpURLConnection rule hits, as flags, and never keeps the original).

### 4.5 Event pipeline, buffering and the socket

```
 app threads                         writer thread (1)                     server thread (1)        reader thread (1 per client)
 ───────────                         ─────────────────                     ─────────────────        ────────────────────────────
 Recorder.* ──► EventQueue ─drain──► encode frame (JSON / body chunk)      accept()                 read frames
   (small objects,   bounded:        assign seq                            peer UID check           hello_ack, set_rules,
    byte[] copies)   8 MiB, drop     append to ReplayRing                  send hello               set_config, ping → apply
                     oldest,         write to client socket if connected   hand socket to writer     → acks via writer
                     count drops     (replay first after hello_ack)
```

- **App-thread cost.** Hooks build small event objects and copy body bytes into pooled `byte[]` chunks; that is all. Sequence numbers, JSON encoding, ring accounting and socket writes happen on the writer thread. The stack capture (`new Throwable().getStackTrace()` trimmed to the configured depth, default 64) is the largest app-thread cost and is measured in the overhead benchmark (§6).
- **Queue.** Bounded by bytes (default 8 MiB). On overflow the oldest queued events are dropped and counted (per transaction where possible); the writer emits a `dropped` event when it next runs. App threads never wait on the writer or the socket.
- **Ring buffer.** Every encoded frame is also appended to `ReplayRing`, which keeps whole transactions: the last 1,000 transactions or 32 MiB of body bytes (both configurable), evicting the oldest transaction's frames together. `diag` events are kept separately (last 200) so start-up hook failures are always replayed.
- **Server.** A daemon thread owns `LocalServerSocket("traffic-police_<package>_<pid>")` (the abstract namespace). For each connection it checks `getPeerCredentials().getUid()` ∈ {0, 2000} before sending anything, sends `hello`, waits up to 10 s for `hello_ack`, applies config and rules, and hands the socket to the writer, which replays and then streams. A new authorized client replaces the old one (PROTOCOL.md §2).
- **Clock.** `SystemClock.elapsedRealtimeNanos()` for every `ts`; `System.currentTimeMillis()` only in `clock` pairs.
- **Traffic sampling.** A sampler reads `TrafficStats.getUidRxBytes/TxBytes(Process.myUid())` every 500 ms and emits a `traffic` event when the values changed (Studio's graph is built from exactly these uid totals). The host uses them for the optional "app total" graph series (5.8).
- **Pause.** `recording = false` stops new transactions at the `Recorder`; in-flight ones finish; rules keep applying.

### 4.6 Library mode

```kotlin
dependencies {
    debugImplementation("io.trafficpolice:capture:<version>")
    releaseImplementation("io.trafficpolice:capture-noop:<version>")
}

val client = OkHttpClient.Builder()
    .addNetworkInterceptor(TrafficPolice.networkInterceptor())
    .eventListenerFactory(TrafficPolice.eventListenerFactory(existingFactoryOrNull))
    .build()

val conn = TrafficPolice.wrap(URL(url).openConnection() as HttpURLConnection)
```

- **Auto-start.** The library's manifest declares `TrafficPoliceInitProvider` (a plain `ContentProvider`, no androidx) with a high `initOrder`. Its `onCreate()` starts the runtime only if `ApplicationInfo.FLAG_DEBUGGABLE` is set, and otherwise logs once and stays inert. Android installs a manifest provider only in the process named in its `android:process` (by default the main process; `ComponentResolverBase.queryProviders` filters on the process name), so **secondary processes** (`:remote`, `:sync`) need one call in `Application.onCreate()`: `TrafficPolice.start(this)` (a no-op in the release artifact). Without it, the interceptor in such a process is a pass-through, and the picker does not list the process.
- **Public API** (identical in `capture-noop`): `networkInterceptor()`, `eventListenerFactory()`, `eventListenerFactory(EventListener.Factory)`, `wrap(HttpURLConnection)`, `wrap(URLConnection)`, `start(Context)`, `isActive()`. In the no-op artifact, `networkInterceptor()` returns a trivial pass-through interceptor, `eventListenerFactory(existing)` returns `existing` (or `EventListener.NONE`'s factory), and `wrap()` returns its argument; the no-op contains no capture code, no provider and no socket.
- **Order matters** for the listener: if the app calls `eventListener(...)` or `eventListenerFactory(...)` after ours, it replaces ours; the Overview then shows `origin: interceptor` and the Call Stack tab explains why.
- **R8/ProGuard.** The library ships consumer rules that keep its own classes; OkHttp's names do not matter in library mode because the app links against them directly.
- **Hidden APIs.** None. Everything used is public SDK: `LocalServerSocket`, `LocalSocket.getPeerCredentials()`, `Credentials.getUid()`, `SystemClock`, `TrafficStats`, `Process`, `ApplicationInfo`.

### 4.7 Attach mode

Goal: capture from an unmodified debuggable app, including OkHttp clients created before attach, and from launch. The design follows what Android Studio's App Inspection does (read in `tools/base`: `app-inspection/agent`, `app-inspection/native`, `transport/native`, `deploy/`), and fixes the gaps its sources show: no loader awareness, no dedupe for OkHttp 4/5, late-loaded classes missed on API 26–27, hook failures only in logcat, and no launch-time attach.

#### 4.7.1 Pieces on the device

| File (per ABI where noted) | Built from | Where it lives on the device | Loaded how |
|---|---|---|---|
| `libtrafficpolice_agent.so` (arm64-v8a, armeabi-v7a, x86_64) | `android/attach-agent` (C++17, NDK, slicer vendored from AOSP `platform/tools/dexter`) | `/data/data/<pkg>/code_cache/traffic-police/` | `cmd activity attach-agent`, `am start --attach-agent`, or a copy in `code_cache/startup_agents/` |
| `traffic-police-boot.dex` | `io.trafficpolice.boot.*` (Java, `java.lang` only) | same dir | JVMTI `AddToBootstrapClassLoaderSearch` in `Agent_OnAttach` |
| `traffic-police-runtime.dex` | `capture` library minus the library-mode entry points | same dir | read into a `ByteBuffer`, then `InMemoryDexClassLoader` |

- Slicer lives in AOSP `platform/tools/dexter` (not `external/dexter`); we vendor a pinned revision (Apache-2.0).
- The agent ABI must match the **process** ABI (ART rejects native-bridge agents), so a 32-bit app on a 64-bit device gets the armeabi-v7a agent. The host reads the process architecture from `track-app` on Android 12+; on older devices it uses the package's `primaryCpuAbi` from `dumpsys package` (the field was confirmed on an Android 16 device; older releases to be checked in Phase 4).
- The agent is built with NDK r28 (installed here: 28.2), which aligns ELF segments to 16 KB by default. A 4 KB-aligned `.so` fails to `dlopen` on 16 KB-page devices (Android 15+) unless the target app happens to run in page-size compat mode.
- The `.so` must be executed from the app's own data directory: app domains may read `/data/local/tmp` (SELinux allows it explicitly for Android Studio's profilers) but never execute from it. Hence the `run-as … cp` into `code_cache/`.
- Using `InMemoryDexClassLoader` (API 26+) sidesteps Android 14's rule that dynamically loaded dex files must not be writable (enforced for targetSdk ≥ 34, only in the file-backed `DexFile.openDexFileNative` path). `AddToBootstrapClassLoaderSearch` opens the dex with ART's own loader and is not subject to the check either; the host still `chmod 444`s both dex files, as Studio does. The ART extension `add_to_dex_class_loader_in_memory` (Android 11+) is avoided: it goes through a writable memfd path that the Android 14 check likely rejects.

#### 4.7.2 Attach sequence

```
host                                               device (app main thread, then agent threads)
 push agent+dex to /data/local/tmp/traffic-police/  ─►
 run-as <pkg> sh -c 'mkdir -p code_cache/traffic-police && cp … && chmod …'
 cmd activity attach-agent <pid> <dataDir>/code_cache/traffic-police/libtrafficpolice_agent.so=<opts>
                                                   Agent_OnAttach (main thread; keep short; always return JNI_OK)
                                                     GetEnv(JVMTI_VERSION_1_2), AddCapabilities(can_retransform_classes, …)
                                                     AddToBootstrapClassLoaderSearch(traffic-police-boot.dex)   (skip if already present: version check)
                                                     set ClassFileLoadHook (+ ClassPrepare on API 26–27)
                                                     GetLoadedClasses → RetransformClasses(java.net.URL, okhttp3.OkHttpClient if loaded)
                                                     start "traffic-police-init" thread ─► load runtime (InMemoryDexClassLoader), RegisterNatives,
                                                                                        AttachEntry.start(opts) → socket, writer, hooks handlers
 discover @traffic-police_<pkg>_<pid>, forward, connect ◄─ hello (hooks: installed / failed / pending)
```

- Gates, all satisfied only by debuggable apps on user builds: the caller needs `SET_ACTIVITY_WATCHER` (the shell has it), AMS requires a debuggable process (`enforceDebuggable`), and ART refuses the attach unless JDWP is allowed. Full JVMTI (and therefore retransformation) also requires a debuggable process. `profileable` apps do not qualify.
- `IApplicationThread.attachAgent` is a **one-way** binder call, so `cmd activity attach-agent` exits 0 without knowing whether the agent loaded. The host treats the attach as successful only when the runtime's socket appears (5 s timeout); otherwise it reads the agent's logcat tag and ART's "Unable to dlopen" / "Agent attach failed" lines and reports them.
- The agent string is split at the first `=`: everything after it reaches `Agent_OnAttach` as options (a short key/value list: runtime version, session token, directory, flags). Startup agents (4.7.4) get only the app's data dir as options, so the agent also reads `code_cache/traffic-police/agent.conf`.
- `Agent_OnAttach` receives no class loader (ART passes `nullptr`; the loader given to `VMDebug.attachAgent` only selects the native library path), and JNI `FindClass` there sees only boot classes. The agent therefore never looks up app classes by name during attach; it uses loaders from `GetLoadedClasses`/`GetClassLoader` and from the ClassFileLoadHook.
- A crash inside `Agent_OnAttach` kills the app, so it does the minimum (above) and defers everything else to its own thread. Returning `JNI_ERR` makes the framework retry the attach with a null class loader, so the agent always returns `JNI_OK` and reports problems through `diag` events and the `hello.hooks` list.
- **Idempotent attach.** Re-attaching the same path runs `Agent_OnAttach` again in the same process. The boot dex contains a version marker class; if `FindClass` finds it with the same version, the agent does not re-append or re-register, and tells the running runtime to restart its socket if needed. A different version cannot replace the loaded one (classes cannot be unloaded), so the host asks the user to restart the app.

#### 4.7.3 Hooks: slicer exit hooks through a boot trampoline

| Target (class → method, JNI descriptor) | Why | Handler returns |
|---|---|---|
| `okhttp3.OkHttpClient` → `networkInterceptors()Ljava/util/List;` | Read by `RealCall` on every call in 3.9–5.5, so clients created before attach are covered | a **new** list with `CaptureInterceptor` first, unless one is already present (checked by class name) |
| `okhttp3.OkHttpClient` → `eventListenerFactory()Lokhttp3/EventListener$Factory;` | Read once per `RealCall` construction | the wrapping factory, unless the factory is already ours |
| `java.net.URL` → `openConnection()Ljava/net/URLConnection;` | Covers HttpURLConnection users (Volley, hand-written code) | a tracked wrapper for `HttpURLConnection`, the original otherwise |
| `java.net.URL` → `openConnection(Ljava/net/Proxy;)Ljava/net/URLConnection;` | This overload does not delegate to the first one | same |

- All four return references, so one slicer transformation serves them all: `ExitHook` with `ReturnAsObject | PassMethodSignature`. Before every `return-object`, the method calls `io.trafficpolice.boot.Trampoline.onExit(String label, Object value): Object`, moves the result into the return register, and `check-cast`s it back to the declared type. Exceptional exits are not hooked (correct: nothing to wrap).
- The trampoline is a few lines: it reads one `volatile ExitHandler handler`; if null, returns `value`; otherwise calls it inside `try { … } catch (Throwable t) { return value; }`, with a thread-local re-entrancy guard (our own code calls `URL.openConnection` indirectly). Its signatures use only `java.lang` types, so boot classes (`java.net.URL`) and app classes (`okhttp3.OkHttpClient`) can both link to it. `ExitHandler` is a boot-dex interface implemented by the runtime.
- **Dedupe matters on OkHttp 4/5.** `OkHttpClient.Builder(OkHttpClient)` (i.e. `newBuilder()`) reads the hooked getters in 4.x/5.x bytecode, so a derived client bakes in our interceptor and factory, and its own getter would add them again. Handlers check for our classes by name first. Instances baked into derived clients outlive a detach, so they check the runtime's `active` flag and pass through when it is off.
- **One OkHttp copy per process (v1).** The ClassFileLoadHook receives the defining class loader. The first loader that defines `okhttp3.OkHttpClient` is chosen; the same name defined by another loader (a plugin or shaded copy) is left unmodified and reported with a `diag`, because an interceptor built against one loader's `okhttp3.Interceptor` would fail with an `IncompatibleClassChangeError` in the other. (Studio's dispatch is loader-agnostic and would hand such a client foreign types.) Supporting several copies later means per-loader labels in a small `ExitHook` variant.
- **Which loader parents the OkHttp adapter.** The runtime core is loaded at attach time with the boot class loader as parent (it needs only `java.*` and `android.*`, and at launch no app loader exists yet; ART's "system class loader" is the zygote's, not the app's, so it is not used). The OkHttp adapter needs the app's `okhttp3`/`okio`, so it is loaded lazily, on the first OkHttp hook call, into an `InMemoryDexClassLoader` whose parent is the loader recorded for `okhttp3.OkHttpClient`, with a small override that resolves `io.trafficpolice.capture.core.*` from the core loader. The agent passes the recorded loader to Java through a native method registered with `RegisterNatives`. This avoids both Studio's `findInstances(Application)` heap walk (no `Application` exists before bind) and the main thread's context class loader (which is a `WarningContextClassLoader` for `sharedUserId` apps and apps with a non-default process name).
- **Retransformation.** The agent keeps a table keyed by class descriptor. ART re-delivers the class's **original** dex on every retransform, so the ClassFileLoadHook re-applies the complete hook set for that class each time: `dex::Reader` → `FindClassIndex` → `CreateClassIr` → `MethodInstrumenter` → `dex::Writer::CreateImage` (JVMTI `Allocate`). The buffer ART passes often holds the **whole** containing dex (Android 8.x and 13+), while ART accepts back only a dex with exactly one class definition; building the IR for the one class and writing it out satisfies that. ART also allows only method-body changes (no added or removed members, no modifier or hierarchy changes), which is exactly what an exit hook is. Classes already loaded at attach are found with `GetLoadedClasses` and retransformed. `java.net.URL` is retransformable in a debuggable process (the boot image is deoptimized for debuggable apps); interfaces, arrays, proxies and `String` are not, and none are targets.
- **Classes loaded after attach.** On API 28+ the ClassFileLoadHook stays enabled, so `okhttp3.OkHttpClient` loaded later (or at launch) is instrumented at definition; the callback is a lock-free descriptor lookup because it runs for every class. On API 26–27 an always-on hook is expensive (Studio's comment), so the agent enables `ClassPrepare` instead and retransforms `okhttp3.OkHttpClient` when it is prepared, enabling the load hook only on that thread for that call (the pattern Studio's profiler agent uses).
- **Failure reporting.** For each hook the agent records: class found or not, method found or not (`InstrumentMethod` false is the typical R8-renamed case), retransform error name, loader, and a hit counter updated by the trampoline. The runtime sends this as `hello.hooks` and `diag` events, and `doctor` prints it. Slicer calls `abort()` on internal check failures, so targets are validated before instrumentation (reference return types only; method present; not abstract or native).

#### 4.7.4 Capture from launch (`--launch`)

| API level | Mechanism | Notes |
|---|---|---|
| 26 | none | Runtime attach only; `--launch` starts the app, attaches as soon as the pid appears, and warns that start-up traffic may be missed |
| 27–29 | `am start -n <activity> --attach-agent <path>=<opts>` (attached just before bind) | Studio's profiler uses it from 27; Studio's coroutine debugger avoids 27–28 after problems, so 27–28 are best effort. `--attach-agent-bind` (28+) is not used: its code path also fires the pre-bind attach, so the agent would be attached twice. |
| 30+ | copy the agent into `code_cache/startup_agents/`, then `am start` | The framework attaches every file in that directory, before `bindApplication`, for debuggable apps only, in **every** process of the app, on **every** start, with the data dir as options and the boot class loader. The directory survives relaunches (it is cleared only on upgrade or cache clear). The host removes our file when the session ends; the agent also reads an expiry stamp next to it and stays inert when the stamp is stale, so a crashed host cannot leave an app instrumented indefinitely. Other files there (another tool's agents) are left alone. |

At launch no app class loader exists. That is why hooks are keyed by name, the ClassFileLoadHook catches later definitions, and the OkHttp adapter is created lazily. `java.net.URL` is a boot class and is retransformed immediately.

#### 4.7.5 Detach and restart

- JVMTI agents cannot be unloaded and ART keeps attached agents for the process lifetime. "Stop" sets the trampoline handler to null and the runtime's `active` flag to false; instrumented bytecode stays and costs one volatile read per hooked call. Restoring the original classes with a retransform after clearing the hook table is possible in principle but unverified on devices, so it is not in v1.
- With `--follow`, a restarted app is re-attached (runtime attach, or automatically through `startup_agents` on 30+).

### 4.8 Support matrix

| | API 26 | 27 | 28–29 | 30–33 | 34+ |
|---|---|---|---|---|---|
| Library mode | ✓ | ✓ | ✓ | ✓ | ✓ |
| Attach at runtime | ✓ | ✓ | ✓ | ✓ | ✓ (runtime dex in memory) |
| Late-loaded OkHttp in attach mode | ClassPrepare + retransform | ClassPrepare + retransform | always-on load hook | always-on load hook | always-on load hook |
| Capture from launch | — | `--attach-agent` (best effort) | `--attach-agent` (28 best effort) | `startup_agents` | `startup_agents` |
| Agent `.so` constraints | process ABI | process ABI | process ABI | process ABI | process ABI; 16 KB-aligned on API 35+ devices |

| OkHttp runtime | What works |
|---|---|
| < 3.9 | Not supported (no public `EventListener`, no `Chain.call()`); pass-through with a `diag` |
| 3.9–3.13 | Full capture; no `requestFailed`/`responseFailed` events; no one-shot/duplex bodies exist |
| 3.14–4.x | Full capture; newer listener callbacks (proxy select, canceled, cache) as available |
| 5.x | Full capture; listener composed with `EventListener.plus()` on 5.3+; connect events may arrive on background threads |

## 5. Host side (Rust)

### 5.1 Crates

`host/` is a Cargo workspace. The split enforces the layering rule from the brief: backends produce events, the store owns state, the UI only reads the store.

| Crate | Depends on | Contents |
|---|---|---|
| `traffic-police-proto` | serde, serde_json, bytes | Wire codec for PROTOCOL.md: frame encoder/decoder, typed messages, version constants. Pure (no IO); a Tokio `Decoder`/`Encoder` behind a feature. Fuzzed. |
| `traffic-police-core` | traffic-police-proto | Normalized event model, `Backend` trait, session store, body store with disk spill, filter language, redaction, rules model (TOML), body decoders, exporters (HAR, cURL, session file), diff. No terminal code. |
| `traffic-police-adb` | tokio | adb smart-socket client: device tracking, process tracking, forward, shell v2, sync push, socket discovery. Fallback to the `adb` binary. |
| `traffic-police-backends` | traffic-police-core, traffic-police-adb, traffic-police-proto | `Backend` implementations: demo generator, device socket (library and attach share it), session file, HAR import. Attach orchestration (push, copy, attach). |
| `traffic-police-tui` | traffic-police-core, ratatui, crossterm | Application state, views, widgets, keymap, themes, mouse hit-testing. Reads the store; sends commands. |
| `traffic-police` (bin) | all | clap CLI, subcommands (`demo`, `open`, `tail`, `record`, `export`, `doctor`), wiring, logging, panic hook. |

Release builds embed the Android artifacts (agent `.so` per ABI, capture dex, trampoline dex) with `include_bytes!` from a build step, so attach mode needs nothing but adb (Phase 4).

### 5.2 Runtime and threading

- One Tokio multi-thread runtime (2 to 4 workers) runs all IO: adb connections, backend sockets, timers, the rules-file watcher bridge.
- The UI loop is one task that owns the `SessionStore` and the terminal. It `select!`s over: terminal input (crossterm `EventStream`), ingest batches from backends (bounded mpsc), results of background jobs, and a frame timer. There are no locks on the store: one owner, one writer.
- Frame pacing: redraw only when something changed. While live, changes are coalesced to at most 30 redraws per second (a 33 ms timer); input is handled immediately and redraws at up to 60 Hz. Idle sessions redraw about 4 times per second only if a visible element is time-dependent (pending spinners, "live" axis).
- Ingest is bounded work per loop turn (at most N events, default 5,000, then yield to input and rendering), so a 50,000-event replay burst never freezes the UI.
- Heavy work runs on `spawn_blocking` and reports back over a channel: decoding and pretty-printing large bodies, JSON parse for the tree view, jq filters, body search across the session, HAR and session export, diff of large bodies. Results are keyed by `(TxnIdx, body generation, view variant)`, so stale results are discarded.
- Headless modes (`tail`, `record`, `export`) run the same backends and the same store without the UI task.

### 5.3 Event model and the `Backend` trait

Every source produces the same normalized events. The device protocol maps almost one-to-one onto them; HAR import and the demo generator synthesize them.

```rust
pub enum SessionEvent {
    SourceUp(SourceInfo),            // a process segment starts: device, package, process, pid, instance, clock, mode, capabilities
    SourceDown { source: SourceId, reason: DownReason, at: Ts },   // detach, crash, unplug, superseded, protocol error
    Request(RequestStarted),         // method, url, ordered headers, client, thread, stack, call id, hop, early timing marks, conn
    Response(ResponseStarted),       // status, message, protocol, ordered headers, conn (remote address, TLS)
    Body { key: TxnKey, dir: BodyDir, at: Ts, offset: u64, bytes: Bytes },   // dir: Request | Response | Delivered
    BodyProgress { key: TxnKey, dir: BodyDir, at: Ts, total: u64 },          // bytes seen but not captured (over cap)
    BodyEnd { key: TxnKey, dir: BodyDir, at: Ts, info: BodyEnd },            // totals and state (complete, truncated, closed early, ...)
    Mark { key: TxnKey, at: Ts, mark: TimingMark },
    Completed { key: TxnKey, at: Ts },
    Failed { key: TxnKey, at: Ts, error: ErrorInfo },
    RuleApplied(RuleEffect),         // rule ids, change list, delivered status line and headers (original kept)
    Traffic { source: SourceId, at: Ts, rx: u64, tx: u64 },                 // whole-app counters ("app total" graph)
    Dropped { source: SourceId, at: Ts, events: u64, bytes: u64, txns: Vec<u64> },
    Diagnostic { source: SourceId, level: Level, code: String, message: String }, // hook status, warnings
    Marker(Marker),                  // host-side markers: pause, resume, reattach, range notes
}

#[async_trait::async_trait] // or return-position impl Future, depending on the final toolchain
pub trait Backend: Send + 'static {
    fn info(&self) -> BackendInfo;                    // name, kind (Demo | Device | File | Har), capabilities
    async fn run(self: Box<Self>, sink: EventSink, commands: CommandRx) -> Result<BackendExit>;
}

pub enum BackendCommand { SetRules(RuleSet), SetCaptureConfig(CaptureConfig), Pause, Resume, Ping, Shutdown }
```

- `EventSink` batches events (a `Vec<SessionEvent>` per socket read) into a bounded channel. If the UI falls behind, the channel fills, the backend stops reading the socket, TCP backpressure reaches the device, and the device drops oldest events and reports the count. Nothing blocks an app thread.
- Backends that cannot accept commands (file, HAR) report that in `BackendInfo.capabilities`; the UI greys out pause and rules.
- `TxnKey = (SourceId, device txn id)`. The store maps it to a dense `TxnIdx` (u32) in arrival order.

**Demo backend.** `traffic-police demo` runs a simulated device that *encodes real protocol frames* (PROTOCOL.md) into an in-memory stream decoded by the same code as a live connection, so the demo exercises the protocol path end to end. Scenario scripts (seeded RNG, real or virtual clock):

- an identity-SDK session on `DefaultDispatcher-worker-*` threads: `init` → `challenge` → `attest` → `enroll`, then `status?sessionId=…` every 1.5 s until a verdict;
- background telemetry (`events`, `monitor`) on its own thread, with gzip-encoded JSON;
- a 302 redirect followed to a 200 (two hops, one call), a 404, a 500, a read timeout, a cancelled call;
- a PNG avatar, a 5 MB download streamed over several seconds, a protobuf body, a multipart upload, a form POST;
- a JWT in `Authorization`, `Set-Cookie` duplicates, an active rule that rewrites one response, a `dropped` event, and a `diag` warning;
- optionally (`--restart-after 60s`) a simulated process death and relaunch to show DETACHED and reattach markers.

With a virtual clock the generator produces a fixed timeline instantly; the snapshot tests use that.

### 5.4 adb client and discovery

- `traffic-police-adb` speaks the adb server protocol on `127.0.0.1:5037` (or `ADB_SERVER_SOCKET` / `ANDROID_ADB_SERVER_PORT`). The exact requests and replies are in PROTOCOL.md Appendix A. The rules that shape the client: one outstanding request per TCP connection (the server does not support pipelining), a timeout on every request (some malformed requests get no reply), devices addressed by transport id rather than serial (ids are exact; serial matching is fuzzy), and device capabilities decided from the device's feature list rather than its API level (adbd is an updatable module).
- **Long-lived tasks.** One device tracker per server (`host:track-devices-proto-binary` when the server advertises `devicetracker_proto_format`, else `host:track-devices-l`). Per online device: a process tracker (`track-app` on Android 12+ devices with the `track_app` feature, which also reports debuggability and the process architecture; else `track-jdwp` plus `/proc/<pid>/cmdline` for names) and, while a picker or `--follow` needs it, a socket scan (`cat /proc/net/unix` every second, filtered to listening `@traffic-police_` names).
- **Shell commands** use `shell,v2,raw:` so every command has an exit code (minimum API 26, so the shell protocol is always there), with arguments single-quoted.
- **Pushing** the attach-mode files uses the sync protocol directly (`SEND`/`DATA`/`DONE`, 64 KiB chunks).
- **Forwards** are created per connection (`tcp:0`, so the server picks the port) and removed with `killforward:tcp:<port>` when the connection ends. They also vanish whenever the device goes offline or the server restarts, so the backend re-creates them on every transition back to `device`. A forward succeeding proves nothing about the runtime (a forward to a missing socket connects and then reads EOF); only `hello` does. The client never uses `killforward-all`, which would also remove Android Studio's forwards.
- **Server restarts.** Every connection drops and transport ids restart from 1. One supervisor reconnects with backoff (250 ms to 5 s), invalidates all cached ids, rebuilds the device table from the tracker's first message, then re-establishes per-device trackers and forwards. Live sessions show DETACHED during the gap and reattach automatically when the same process is still alive (same `instance`, resume after the last `seq`).
- **Server not running.** The client starts it with `adb start-server` (the binary from `$ANDROID_HOME/platform-tools`, then `PATH`) and allows several seconds for the first reply.
- **Fallback to the adb binary** for an operation the protocol path cannot perform is allowed only when `adb version` matches the running server's version: an adb client kills and restarts any server of a different version, which would also disconnect Android Studio. `doctor` reports a mismatch.

### 5.5 Device backend lifecycle

```
          pick device+process               hello ok
 IDLE ───────────────────────────► CONNECTING ─────────► LIVE ◄──────► PAUSED (device stops recording)
   ▲                                   │                   │
   │                                   │ error             │ EOF / reset / process gone / unplug
   │                                   ▼                   ▼
   └────────── user picks again ── FAILED(msg)          DETACHED ── --follow: new pid for package ──► CONNECTING (new segment + marker)
```

- FROZEN is a UI state layered on top of LIVE or PAUSED (5.8); REPLAY is the state of file and HAR sessions.
- Connecting: find the socket (discovery or computed name), `forward tcp:0 localabstract:<name>`, connect, expect `hello` within 3 s (a forward to a missing abstract socket connects and then closes, which reads as "no capture runtime in this process").
- Detach detection: EOF or reset on the socket, the pid disappearing from process tracking, or the device leaving `track-devices`. All captured data is kept; pending transactions are shown as "detached before completion".
- `--follow`: after DETACHED, watch process tracking for a new process of the same package, wait for its socket, connect, add a new `Source` and a reattach marker on the timeline. Rules are pushed again in `hello_ack`.
- Several devices can be tracked at once; one session follows one package on one device. Multi-process apps show each process separately in the picker; the session attaches to the chosen one (with `--follow` matching on process name).

### 5.6 Session store

```rust
pub struct SessionStore {
    sources: Vec<Source>,                  // one per process segment; carries device clock offset
    txns: Vec<Arc<Transaction>>,           // dense, arrival order
    index: HashMap<TxnKey, TxnIdx>,
    bodies: BodyStore,                     // chunk storage with spill-to-disk
    traffic: TrafficSeries,                // per-direction fine bins + prefix sums
    threads: ThreadRegistry,               // interned thread names and ids -> lanes
    markers: Vec<Marker>,                  // attach, detach, reattach, pause, resume, drops
    stats: SessionStats,                   // counts, bytes in/out, failed, dropped
    annotations: Annotations,              // pins, diff marks (saved with sessions)
    generation: u64,                       // bumps on every change; the UI's dirty check
    changed: Vec<TxnIdx>,                  // drained by derived views for incremental updates
}
```

- Transactions are `Arc`s updated with `Arc::make_mut` (copy-on-write). A freeze snapshot clones the `Vec<Arc<_>>` (about 400 KB of pointers at 50,000 rows, well under a millisecond) plus small metadata; later updates copy only the transactions they touch. The body store is append-only, so a snapshot bounds its reads by the lengths it recorded.
- A `Transaction` holds: key, source, call id and hop (redirect chains), client kind, method, URL (raw plus parsed scheme, host, port, path, query), ordered request headers, optional response (status, message, protocol, ordered headers, remote address, TLS), body references per direction (`Request`, `Response` as received, `ResponseDelivered` when a rule changed the body), rule effects with the original status line and headers, thread, stack (shared `Arc<[Frame]>`), timing marks, start and end timestamps, state (`Pending`, `Sending`, `Waiting`, `Receiving`, `Complete`, `Failed`, `Detached`), sizes, and flags (rule-modified, gap, truncated).
- Headers are `Vec<(String, String)>`: order and duplicates are preserved everywhere, including HAR export.
- **Body store.** Chunks are kept in memory as `Bytes` until a budget (default 256 MiB) is exceeded; then the oldest and largest bodies are spilled to an append-only file in a per-run temp directory (`<temp>/traffic-police-<pid>-<random>/`), leaving `(offset, len)` references. The directory is deleted on exit, from the panic hook, and on SIGTERM/SIGHUP; on start-up, directories of dead pids are removed. Each body records its state: `Streaming`, `Complete`, `Truncated { captured, total }`, `NotCaptured(reason)`, `NotConsumed`, `ClosedEarly`, `Gap` (chunks lost to device overflow).
- **Traffic series.** Bytes are added at the device timestamp of the chunk (or progress event) that carried them, plus header sizes at request and response start. Storage is fine bins (10 ms) per direction with running prefix sums, so any zoom level computes N bucket sums in O(N). A one-hour session costs about 9 MB; longer sessions re-bin to 100 ms. HAR imports spread bytes evenly across each entry's send and receive phases. The "app total" series is kept separately as the raw `traffic` samples (cumulative counters every 500 ms), converted to rates per bucket at draw time.
- **Time.** All durations and bins use device monotonic nanoseconds. Wall-clock labels use the per-source offset from `hello` and refreshed by `pong`. The session origin is the first source's `hello` time (or the first event, for files).

### 5.7 Derived views

- **Row model.** The Connection View shows `rows: Vec<Row>` where a row is a transaction or a collapsed group. It is maintained incrementally: a changed transaction is re-evaluated against the filter and inserted, moved, or removed by binary search on the sort key. Changing the filter or sort recomputes everything once (50,000 predicate evaluations take a few milliseconds; body-content filters run in the background and fill in progressively).
- **Sort.** Default chronological (request start, then arrival). Any column sorts stably; live appends insert in place.
- **Collapse repeats.** Consecutive rows (in the current sort) with the same method, host and path collapse into a group row with a count, a combined time span, and the latest status; Enter or Right expands it. Groups are recomputed only when the row set changes.
- **Graph range selection** adds a time-overlap predicate to the filter.
- **Thread lanes.** One lane per `(thread id, name)`, in order of first request; overlapping bars in a lane stack into sub-rows.

### 5.8 UI

- **Layout.** Header (device, process, pid, state, active rule count) · traffic graph · split (views on the left, detail pane on the right, resizable divider) · status bar. Under about 140 columns the detail pane opens full-width over the list. Under 100×30 the UI shows a "terminal too small" notice instead of a broken layout.
- **Focus** cycles Graph → List → Detail with Tab and Shift+Tab. Each region records its screen rectangles during render; mouse events are hit-tested against them (rows, tabs, divider, graph).
- **States in the header:** LIVE, PAUSED (device not recording), FROZEN (UI shows a snapshot; ingest continues and the status bar counts what arrived since), DETACHED, REPLAY.
- **Traffic graph.** Ratatui `Chart` with braille markers, two datasets (Receiving blue, Sending orange). Bucket width = visible window ÷ (2 × plot width in cells), so each braille dot column is one bucket and each dataset has about two points per cell (Chart's cost is linear in points). The y-axis auto-scales to a "nice" maximum in human units (B/s, KB/s, MB/s). The x-axis shows `mm:ss.mmm` since session start or wall-clock time (toggle); tick labels are drawn by our own widget under the plot because Chart places more than three axis labels incorrectly. Detach and reattach markers are vertical line datasets. Live mode keeps the right edge at "now"; scrolling or zooming back stops following until L. `v` starts a keyboard range selection; mouse drag selects directly.
- **Graph source.** By default the graph plots bytes the runtime captured (request and response bodies at the device time they were written or read, plus header sizes), so every spike corresponds to rows in the list, and file and HAR sessions graph the same way. `T` switches to **app total**: the whole-app `TrafficStats` uid counters the runtime samples every 500 ms, which is what Android Studio plots (it includes non-HTTP traffic and HTTP from clients we do not hook). The legend names the active source.
- **Connection View.** A custom virtualized table: only visible rows are formatted each frame. Columns: Name, Size, Type, Status, Time, Timeline, plus optional Method, Host, Path, Thread, Start, Request size, Protocol, Client. The Timeline column shares the graph's window. Each bar has three shades (sending, waiting, receiving) and sub-cell precision with the eighth-block characters `▏▎▍▌▋▊▉█`; a bar that starts inside a cell is drawn as the inverse-colored partial block (a cell can show two colors, so the boundary between two phases inside one cell is rounded to the nearest eighth). Status text is always present (`200`, `404`, `failed`, `···`) and colored by class. Rule-modified rows show a `✎` marker; pinned rows `★`. When parked at the bottom in live mode, the list auto-scrolls.
- **Thread View.** Lanes on the shared time axis; bars are selectable and open the detail pane.
- **Rules view.** Ordered list with enabled toggles, match summary, action summary, and per-rule hit counts; a form for editing; errors from the file or the device shown inline.
- **Detail pane tabs.** Overview, Response, Request, Call Stack (Studio's order), with the fields from the brief. `o` toggles original and modified when a rule changed the response; `p` toggles Parsed and Source.
- **Freeze** renders from a store snapshot (5.6); unfreezing jumps back to the live store.
- **Virtualization everywhere.** Ratatui's `Table` allocates every row it is given and `Paragraph` scrolls only up to 65,535 lines and re-wraps from the top each frame, so neither ever receives more than the visible slice: the list, thread lanes, body viewers, hex dumps and diffs all keep their own offsets into indexed data and build widgets for the visible rows only. Scrollbars use `ScrollbarState` with the total count.
- **Terminal hygiene.** Alternate screen, raw mode, mouse capture, bracketed paste. Mouse capture enables any-motion reporting, so motion events are coalesced (only the latest per frame is processed). Our panic hook is installed before `ratatui::init()` and disables mouse capture and bracketed paste (Ratatui's own hook restores only raw mode and the alternate screen); signal handlers do the same.
- **Images.** The graphics protocol is detected once, after entering the alternate screen and before the input stream starts (the probe reads stdin), with a short timeout; half-blocks are the fallback. Inside tmux the default is half-blocks, because the image library enables tmux's `allow-passthrough` as a side effect of its probe; `ui.images = "auto"` opts in. Snapshot tests always use half-blocks.

### 5.9 Body decoding and viewers

The pipeline for a body is: bytes (memory or spill file) → Content-Encoding decoding → kind detection → viewer model → rendered lines (visible range only).

1. **Content-Encoding** chains are decoded in reverse order (`gzip`, `x-gzip`, `deflate` with or without the zlib header, `br`, `zstd`) with pure-Rust decoders, and the output is capped (default 256 MiB) to defuse decompression bombs. Both sizes are shown ("18.2 KB transferred, 96.4 KB decoded"). A decode failure shows the raw bytes with the error.
2. **Kind detection** uses Content-Type first (`*/json` and `*+json`, `*/xml` and `*+xml`, `text/html`, `application/x-www-form-urlencoded`, `multipart/*`, `image/*`, `application/x-protobuf`, `application/protobuf`, `application/grpc*`), then magic bytes and a JSON sniff.
3. **Viewers:**
   - JSON: a hand-written single-pass tokenizer produces the pretty-printed lines, syntax spans, fold ranges and JSON paths together (fast on multi-megabyte bodies, and key order, big integers and number spelling are preserved exactly as sent); a foldable tree (fold state per node); the JSON path of the cursor line in the pane footer; a jq-style filter (jaq) whose output replaces the view until cleared. Filters run on a worker thread with a time limit and an output cap (a filter can loop forever), and jaq's `halt` is handled as an error instead of letting the library exit the process.
   - XML and HTML: pretty-printed (XML with quick-xml; HTML with a tolerant tokenizer, since HTML is not XML) and highlighted with syntect's pure-Rust regex engine, highlighting only the visible range from cached parse checkpoints; malformed input falls back to text.
   - Form-urlencoded: decoded key and value table in original order, duplicates kept.
   - Multipart: parts with their headers, each with a nested preview (recursively using this pipeline).
   - Images: decoded and drawn inline through kitty, iTerm2, or sixel graphics when the terminal supports them, else half-blocks; always with format, dimensions and size.
   - Protobuf and gRPC: schemaless decode in the style of `protoc --decode_raw` (field numbers, wire types, nested messages detected heuristically, strings shown when valid UTF-8); gRPC's 5-byte message framing is split first.
   - Everything else: text if valid UTF-8, else a hex dump rendered lazily for the visible window.
4. **Body states** are always explicit: "truncated at 10 MB of 48.2 MB", "not captured (capture disabled)", "not consumed by the app", "closed early after 12 KB", "streaming…", "gap: 64 KB lost to device buffer overflow", "redacted".

Decoded bodies are cached in an LRU keyed by `(TxnIdx, dir, variant)` with a byte budget (default 128 MiB). Pretty-printed lines are generated once per body and width-independent; syntax highlighting is computed lazily for visible lines.

### 5.10 Utilities

- **Filter bar** (`/` in the list). Whitespace-separated tokens, AND-combined; quotes for spaces; a leading `-` negates any token.

  | Token | Meaning |
  |---|---|
  | `text` | case-insensitive substring of the URL |
  | `/regex/` (optional `i`) | regex on the URL |
  | `method:GET` | method (comma list allowed) |
  | `status:200`, `status:4xx`, `status:failed`, `status:pending`, `status:>=400` | status code, class, or state |
  | `host:api.example.com`, `host:*.example.com` | host, glob |
  | `path:/api/**/status` | path glob (`*` within a segment, `**` across) |
  | `type:json` | response type column |
  | `thread:worker` | substring of the initiating thread name |
  | `size>10k`, `size<=2mb`, `time>500ms`, `time<2s` | response size, duration (units b, k/kb, m/mb, g; ms, s, m) |
  | `rule:modified`, `rule:<id>` | changed by any rule, or by a given rule |
  | `is:pinned` | pinned rows |
  | `body:"needle"` | request or response body contains the text (after decoding); evaluated in the background |

  Parse errors are highlighted in place and the previous valid filter stays active.
- **Search.** `/` in the detail pane searches the current body view (n and N step; matches highlighted). `body:` in the filter bar is the cross-session search ("which request returned this value").
- **Copy** (`y` menu): as cURL (POSIX single-quote escaping; text bodies inline with `--data-binary`, binary bodies written to a file next to the command and referenced as `@file`; headers in order; `--compressed` when the request asked for gzip; a PowerShell variant is a Phase 2 stretch), URL, request or response headers, one header, body (decoded), the value at the cursor's JSON path. `clipboard = "auto"` uses OSC 52 over SSH or when no display server is present (it works inside tmux with `set-clipboard on|external`, which the status bar hints at on failure) and the native clipboard otherwise; `osc52`, `native` and `off` force a choice. OSC 52 is copy-only by design.
- **Save and export** (`w`, `e` menu): save a body (binary-safe, extension from Content-Type or magic); HAR 1.2 of all, filtered, or selected rows (redacted by default), with `_trafficPolice` custom fields for thread, stack, rules and timings beyond HAR's; HAR import through `traffic-police open file.har`.
- **Sessions** (`e` menu, `traffic-police record`, `traffic-police open`): one file (`.trafficpolice`), see PROTOCOL.md §10. It stores the event stream exactly as captured plus host annotations, so reopening reproduces timings, threads, stacks, rule effects, pins and markers.
- **Diff** (`d` marks; the second mark opens the diff): status lines, headers (as ordered lists and as sets), and bodies. JSON bodies are canonicalized (keys sorted recursively, pretty-printed) before a line diff, so key order does not matter; other text is diffed as is; binary bodies compare size and hash.
- **Decoders.** JWTs are detected in `Authorization: Bearer` and in any string of the form `xxx.yyy.zzz` whose header decodes to JSON; the Overview and a decoder popup show header, claims, and `exp`/`iat`/`nbf` in local time with a relative age. Enter on any header or JSON value opens a value menu: copy, decode base64 (standard and URL-safe), URL-decode, decode JWT, filter by this value.
- **Redaction** (on by default). Built-in header list: `Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`, `X-Api-Key`, `Api-Key`, `X-Auth-Token`, plus configurable headers, query parameters, and JSON paths (`$.aadhaar`, `$..pan`, `$.data[*].mobile`). Values are masked as `‹redacted 32 chars›` in the UI (R toggles reveal for this session, and the header shows `REVEALED`) and in every export (cURL, HAR, session, NDJSON) unless `--no-redact` or the export dialog's explicit switch is used. Body masking works on decoded JSON; masked bodies are exported decoded, without Content-Encoding.
- **Pins** (`m`): bookmark rows; `is:pinned` filters.
- **Pause and freeze.** Space sends pause/resume to the device (it stops creating transactions; in-flight ones finish; rules stay active). F freezes the UI (5.8).

### 5.11 Rules (host side)

- Stored in `.traffic-police/rules.toml` in the project (the nearest ancestor of the working directory containing `.traffic-police/`, or `--project DIR`). Schema in PROTOCOL.md §8 (the TOML form mirrors the wire form).
- The file is watched (debounced 200 ms). A valid change is pushed to the device immediately (`set_rules`); an invalid one is shown in the Rules view and status bar while the last valid set stays active.
- Edits from the TUI form are written back with comment-preserving TOML editing. `$EDITOR` can be opened on the file from the Rules view.
- `r` creates a rule prefilled from the selected request (method, scheme, host, port, exact path, and the query parameters present).
- Bodies loaded from `file = "…"` are read on the host and sent inline (base64 for binary).
- The device reports per-rule compile errors (`rules_ack`) and every application (`rule` events). The Rules view shows hit counts from those events.

### 5.12 CLI, headless modes, doctor

```
traffic-police                                   interactive: device picker → process picker → session
traffic-police --serial S --package P [--process NAME | --pid N] [--mode library|attach] [--launch] [--follow]
traffic-police demo [--seed N] [--speed X]
traffic-police open FILE                         .trafficpolice or .har (REPLAY)
traffic-police tail --json [TARGET] [FILTER...]  NDJSON, one line per completed transaction (--events for raw events)
traffic-police record --out FILE [--duration 60s] [TARGET] [--filter F]
traffic-police export --har FILE [--input FILE | TARGET --duration D] [--filter F] [--no-redact]
traffic-police doctor [TARGET]
```

- `TARGET` is `--serial`, `--package`, `--process`/`--pid`, `--mode`, `--launch`, `--follow`. With one device attached `--serial` is optional.
- The NDJSON schema for `tail` is versioned (`"v":1`) and documented in PROTOCOL.md Appendix B.
- `doctor` checks, in order, and prints a fix for each failure: adb server reachable and its version (and whether the `adb` binary on `PATH` matches it); device authorized and online; API level (≥ 26); package installed and debuggable (`run-as` works, and `ro.boot.disable_runas` is not set); socket discovery (`/proc/net/unix` readable; runtime socket present in library mode, with the process list from `track-app`/`track-jdwp`); attachability (process ABI and matching agent, device page size for 16 KB alignment, `code_cache` writable through `run-as`, stale `startup_agents` entries, the hook status of a running agent); and host checks (terminal size, color support, clipboard path, config and rules file validity).

### 5.13 Config, themes, keymap

- **User config:** `config.toml` in `$XDG_CONFIG_HOME/traffic-police` (default `~/.config/traffic-police`) on Linux and macOS, and `%APPDATA%\traffic-police` on Windows, overridable with `TRAFFIC_POLICE_CONFIG`. macOS uses the XDG location, as most terminal tools do, rather than `~/Library/Application Support`. Sections: `[ui]` (theme, time format, visible columns, divider position), `[capture]` (body cap, stack depth), `[redaction]` (enabled, headers, query params, JSON paths), `[keymap]` (action = keys), `[adb]` (server address, adb path), `[storage]` (memory budget, spill dir).
- **Project config** (`.traffic-police/`): `rules.toml`, `project.toml` (default package, source roots for Call Stack → $EDITOR, extra redaction entries).
- **Color:** `NO_COLOR` switches to a monochrome theme that carries meaning in text, bold and reverse video. (crossterm 0.29 answers color commands under `NO_COLOR` with a bare reset that also clears bold and reverse, so we detect `NO_COLOR` ourselves, force crossterm's color output on, and simply never emit colors.) `COLORTERM=truecolor|24bit` enables RGB; `TERM=*256color*` uses the 256-color palette (syntect's RGB themes are quantized); otherwise 16 colors. Dark and light themes ship built in; themes define semantic slots (status classes, sending, waiting, receiving, selection, focus border, markers) rather than raw widget colors.
- **Keymap:** every action has a name, and `[keymap]` maps action names to one or more keys (`ctrl+r`, `shift+tab`, `F`, `?`); defaults follow the brief plus `T` and `R` (9.1). Multi-key sequences are not used. Invalid entries are reported with the line and the valid action names.

### 5.14 Logging, errors, crash safety

- `tracing` logs to a file in the platform state/cache dir (`TRAFFIC_POLICE_LOG=debug` to raise the level); nothing is written to the terminal while the TUI runs. `--log-file` overrides the path.
- User-facing errors appear in the status bar with an action hint; details go to the log. Protocol mismatches produce a dialog that names both versions and the fix.
- Panic hook: restore the terminal, remove the spill directory, print the panic and the log path.

### 5.15 Key dependencies

Versions are the latest stable releases on crates.io as of 2026-09-29; they are pinned by `Cargo.lock` and moved deliberately. The toolchain is pinned in `rust-toolchain.toml` (1.98.1) with `rust-version = "1.90"`: Ratatui 0.30 needs 1.88, and a transitive dependency of the image renderer needs 1.90.

| Need | Crate (version) | Notes |
|---|---|---|
| TUI | ratatui 0.30.2 (crossterm 0.29 backend) | The 0.30 facade re-exports ratatui-core, ratatui-widgets and ratatui-crossterm |
| Terminal and input | crossterm 0.29 with `event-stream`, `osc52` | One crossterm instance shared with Ratatui; `CopyToClipboard` provides OSC 52 |
| Async | tokio 1.53, tokio-stream | |
| Text input | tui-input 0.15 (single line), ratatui-textarea 0.9 (multi-line) | `tui-textarea` 0.7 is stuck on Ratatui 0.29 and crossterm 0.28; ratatui-textarea is the maintained fork |
| Images | ratatui-image 11.1 without default features, image 0.25 (png, jpeg, gif, webp, bmp, ico) | Defaults would link the C library chafa; without it everything is pure Rust |
| jq filters | jaq-core 3.1, jaq-std 3.0, jaq-json 2.0 | `jaq_json::Val` preserves key order and number spelling |
| Highlighting | syntect 5.3 with `regex-fancy`, default syntaxes and themes only | Default features link Oniguruma (C); `syntect-tui` is incompatible with Ratatui 0.30, so the 25-line style mapping is ours |
| Decompression | flate2 1.1 (zlib-rs), brotli-decompressor, ruzstd 0.9 | All pure Rust, so no C toolchain for cross-builds; `zstd` (C) is avoided |
| Diff | similar 3.2 | |
| CLI, config, rules | clap 4.6, serde, serde_json (`preserve_order`), toml 1.1, toml_edit 0.25 | toml_edit keeps comments when the TUI edits `rules.toml` |
| Files | notify 8.2 + notify-debouncer-full 0.7, tempfile, memmap2, etcetera | The watcher watches the parent directory (editors save by rename) |
| Clipboard | arboard 3.6 (no default features, `wayland-data-control`) | Kept alive for the process lifetime (X11/Wayland selection ownership) |
| Other | regex, globset, base64, percent-encoding, url, shlex 2, jiff (local time), quick-xml, bytes, unicode-width, anyhow, thiserror, tracing + tracing-appender | |
| Tests | insta 1.48, proptest 1.11 | `assert_debug_snapshot!` of the buffer when colors matter |

A CI step (`cargo tree -e features`) fails if a C library sneaks back in through default features (chafa, Oniguruma, zstd-sys, dav1d).

## 6. Performance budgets

| Where | Budget | How |
|---|---|---|
| App thread, per request (excluding body copies) | < 1 ms added; target < 150 µs typical | Event objects are small; JSON encoding, ring-buffer accounting and socket writes happen on the writer thread. The stack capture (`Throwable.getStackTrace()`) is the largest cost and is bounded by a depth cap (default 64 frames). Measured by a JVM microbenchmark and by an instrumented test on a device that compares request latency with and without capture. |
| App thread, body bytes | One `System.arraycopy` per read/write chunk into a pooled `byte[]` | Tee sources/sinks copy what the app already read; no extra reads, no pre-buffering. Over the cap, only a byte counter advances. |
| Device memory | Queue ≤ 8 MiB; ring buffer ≤ 1,000 transactions and 32 MiB of bodies (defaults) | Drop oldest on overflow; evict whole transactions from the ring. |
| Host frame time | ≤ 33 ms at 50,000 transactions (target < 8 ms) | Virtualized rows; incremental filter; prefix-sum graph; cached body views; heavy work off the UI task. A benchmark renders 1,000 frames of a 50,000-transaction store into `TestBackend` and fails CI above a generous threshold. |
| Host ingest | 20,000 events/s sustained; a 50,000-event replay applied in < 1 s | Batched ingest, bounded per loop turn. |
| Host memory | ~2 KB per transaction of metadata + bodies within the in-memory budget (default 256 MiB) | Spill store on disk for the rest. |

## 7. Security and privacy

- Capture runs only in debuggable apps. Library mode checks `ApplicationInfo.FLAG_DEBUGGABLE` before starting. In attach mode the platform enforces it at every step on user builds (`run-as`, `attach-agent`, ART's JDWP check, full JVMTI for retransformation, `startup_agents`), and the host additionally refuses packages whose debuggable flag is off, including on userdebug devices where `attach-agent` alone would allow it. `profileable` apps are not debuggable and are not attachable.
- The device socket accepts only peers whose UID is 0 (root) or 2000 (shell), checked with `LocalSocket.getPeerCredentials()` before any byte is sent. Other apps on the device cannot read traffic.
- The host opens no network connections other than to the adb server. No telemetry, no update checks.
- Redaction is on by default in the UI and every export (5.10). Spill files live in a private temp directory (mode 0700 on Unix) and are deleted on exit.
- The release no-op artifact contains no capture code at all, so a misconfigured release build cannot capture.
- Rules can change what the app sees; the header always shows how many rules are active, and rule-modified rows are marked.

## 8. Testing and CI

**Rust**

- Unit tests: frame codec and message decoding; adb reply parsing against transcripts; `/proc/net/unix` parsing; filter parser and evaluator; Content-Encoding decoders; protobuf raw decoder; cURL escaping (round-tripped through `sh -c` in a test on Unix); HAR output validated against a vendored HAR 1.2 JSON schema; redaction; rules TOML parsing and validation; session file round-trip.
- UI snapshots: Ratatui `TestBackend` plus insta, at 100×30, 140×40 and 200×50, for every view, tab, dialog and state (LIVE, PAUSED, FROZEN, DETACHED, REPLAY), driven by the deterministic demo generator (fixed seed, virtual clock).
- Property and fuzz tests: arbitrary byte streams into the frame decoder never panic and never allocate more than the frame limit; arbitrary filter strings never panic; arbitrary bytes into every body viewer never panic. A `cargo fuzz` target for the frame decoder runs in CI for a short time budget.
- Integration: a fake adb server (scripted smart-socket responses) and a fake device (serves golden frames) exercise the device backend, reattach, `--follow`, protocol mismatch, and adb restarts.

**Android**

- JVM tests (JUnit 4, MockWebServer from the same OkHttp major version) for gzip, chunked, streaming, redirects, errors, timeouts, cancellation, one-shot and duplex request bodies, WebSocket and other upgrades, composition with an app `EventListener`, and every rule action. The same suite runs against OkHttp 3.9.0, 3.12.13, 3.14.9, 4.0.0, 4.12.0, 5.0.0 and 5.5.0, each with the Okio it ships and with the newest Okio, via Gradle test configurations that swap the runtime dependency while the library stays compiled against its fixed targets (4.1).
- Protocol conformance: JVM tests write golden frame files (`testdata/protocol/v1/*.frames` plus an expected-events JSON), and Rust tests decode them; the Rust side writes host-to-device golden frames that the Java tests decode. Regenerating golden files is an explicit Gradle/cargo task, never a side effect of a normal test run.
- Instrumented tests on emulators (API 26 and the latest API level): the sample app's scenarios with the library, and the attach path in Phase 4.
- End to end: a script installs the sample app, starts `traffic-police tail --json --package …`, triggers the scenario list through an instrumentation, and asserts the NDJSON stream (methods, URLs, statuses, body hashes, thread names, stack frames containing the sample's call sites).

**CI (GitHub Actions)**

- Host: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on macOS, Ubuntu and Windows; release artifacts built per OS.
- Android: build `capture`, `capture-noop`, `attach-agent` (NDK, three ABIs) and `sample-app`; JVM test matrix across OkHttp versions; an emulator job (API 26 and latest) on Linux with KVM.

## 9. Decisions to review, deviations, risks

### 9.1 Where this design departs from the brief (and why)

| # | Brief | This design | Why |
|---|---|---|---|
| 1 | slicer from AOSP `external/dexter` | `platform/tools/dexter` | That is where slicer lives (verified on AOSP Gerrit); same code Studio uses |
| 2 | OkHttp 3.x, 4.x, 5.x | 3.9.0 and later | 3.0–3.8 have no public `EventListener` and no `Chain.call()`; they are detected and left alone with a `diag` |
| 3 | `debugImplementation` auto-starts from a ContentProvider | Auto-start in the main process; secondary processes call `TrafficPolice.start(context)` | Android installs a manifest provider only in the process it is declared for (verified in `ComponentResolverBase.queryProviders`) |
| 4 | "show the 101 upgrade only" for WebSockets | Attach mode: handshake headers via the EventListener hook (to be confirmed in Phase 4). Library mode: not visible | OkHttp never runs network interceptors for WebSocket handshakes and builds the WebSocket client with `EventListener.NONE` |
| 5 | Capture from launch via `am start --attach-agent` or `startup_agents` | API 30+ `startup_agents`; 27–29 `--attach-agent` (27–28 best effort); API 26 cannot capture from launch | `--attach-agent` appeared in API 27, `startup_agents` in API 30; `--attach-agent-bind` attaches twice (verified in AMS code) |
| 6 | Traffic graph like Studio | Default: bytes the runtime captured; `T` toggles to Studio's whole-app `TrafficStats` series | Captured bytes line up with the rows, zoom finer than 500 ms, and exist in file and HAR sessions; Studio's series is kept for parity (see 9.2) |
| 7 | Rules: fail with IOException or SocketTimeoutException | Also `ProtocolException`, `ConnectException`, `UnknownHostException`, with OkHttp's retry behaviour documented | OkHttp may retry plain `IOException`s thrown by a network interceptor, re-running the rule |
| 8 | (not specified) | Rewritten responses get `Cache-Control: no-store` unless the rule opts out | OkHttp caches what leaves the network interceptors; a fake response must not outlive its rule |
| 9 | Suggested crates: tui-textarea, zstd | ratatui-textarea + tui-input; ruzstd (+ pure-Rust gzip/brotli) | tui-textarea is stuck on Ratatui 0.29; `zstd` needs a C toolchain per target, which works against one self-contained binary per OS. The stack itself (Rust, Ratatui, crossterm, Tokio) is unchanged |
| 10 | Keys | Adds `T` (graph source) and `R` (reveal redacted values) | Not assigned in the brief |
| 11 | (not specified) | One client per app process; a new client takes over (decided in review) | Makes reconnecting after a host crash always work; `tail` and the TUI cannot watch the same process at once |
| 12 | Device clock "monotonic" | `SystemClock.elapsedRealtimeNanos()` (CLOCK_BOOTTIME) | Monotonic and shared by all processes, and keeps counting in suspend, so the wall-clock offset stays stable |
| 13 | OkHttp 2 | Not supported | Studio still supports it; the brief lists 3.x–5.x. Android's own HttpURLConnection (built on an internal OkHttp 2 fork) is covered by 4.3 |

### 9.2 Review decisions and open questions

Decided in the Phase 0 review:

- **Name:** the project is **traffic-police**. Binary `traffic-police`; Rust crates `traffic-police-*`; Java packages `io.trafficpolice.*` with the public class `TrafficPolice`; Maven coordinates `io.trafficpolice:capture` and `io.trafficpolice:capture-noop`; device socket `traffic-police_<package>_<pid>`; project directory `.traffic-police/`; session files `*.trafficpolice`.
- **One client per app process;** a new client takes over.
- **Git:** Phase 0 merged into `master`.

Still open:

1. **Default graph source.** Should the graph at the top draw (a) only the traffic of the requests traffic-police captured, i.e. the rows in the list (proposed), or (b) all network traffic of the app as counted by Android, which is what Android Studio draws (it also includes traffic that never becomes a row, such as WebView or native code, and is only sampled every 0.5 s)? The other source is always one key away (`T`).
2. **Rewritten responses and the app's HTTP cache.** When a rule changes a response, OkHttp may store the changed response in the app's HTTP cache, so the app could keep seeing it after the rule is turned off. Should traffic-police add `Cache-Control: no-store` to every response a rule changed (proposed; the app then sees one extra header on those responses), or leave caching alone unless a rule asks?

### 9.3 Risks

- **Slicer aborts on unexpected input**, which would crash the app. Mitigation: pinned revision, targets validated before instrumentation, only reference-returning methods, tests on every supported API level.
- **Minified debuggable builds.** If R8 renamed or inlined OkHttp's getters, attach-mode hooks install nowhere or never fire; hook status and hit counters make this visible, and library mode is the fallback.
- **Shaded or duplicate OkHttp copies** (an SDK that repackages OkHttp, or a second class loader) are not captured in v1.
- **HttpURLConnection wrappers** must delegate every method of `HttpsURLConnection`; a missed method changes app behaviour. Studio's wrapper is the reference, and the JVM tests call every method through the wrapper.
- **ART is an updatable module from API 31**, so JVMTI behaviour tracks the module version, not only the OS version; the emulator matrix includes updated images.
- **Performance targets** (30 fps at 50,000 transactions; < 1 ms per request on the device) are design targets until measured in Phases 0b and 1.
- **Terminal variance.** Graphics protocols, OSC 52 and mouse support differ across terminals, tmux and SSH; everything degrades to half-blocks, copy-to-file and keyboard.
- **Coexistence with Android Studio.** If Studio's inspector is attached too, both interceptors run; we detect Studio's interceptor in the chain and warn.

## 10. Phase map

| Phase | Deliverable | Done when |
|---|---|---|
| 0a | ARCHITECTURE.md, PROTOCOL.md | Reviewed |
| 0b | Complete TUI on the demo backend | `traffic-police demo` shows every view and tab; snapshot tests pass |
| 1 | Library mode end to end | Sample app traffic live on a physical device and on API 26 and latest emulators, with correct headers, bodies, timings, thread and stack; kill and relaunch behave as specified |
| 2 | Utilities | Everything in 5.10 and 5.12 |
| 3 | Rules | MockWebServer tests for every action, gzip included |
| 4 | Attach mode | An unmodified debuggable app shows traffic, including clients created before attach; `--launch` captures start-up requests; release binaries embed the agent and dex |
| 5 | Optional | gRPC hooks, Flutter (Dart VM service) backend, WebSocket frames |

## 11. Sources checked

`android.googlesource.com` was unreachable from this machine during Phase 0 (HTTP 503), so AOSP code was read from mirrors and from AOSP Gerrit's REST API. Detailed notes with line-level citations are in `docs/research/`.

| Area | Source (ref) |
|---|---|
| Android Studio device side (network inspector, app-inspection agent, transport, deploy) | `platform/tools/base` studio-main snapshot `0227ef52` via GitHub mirror `kroune/platform-tools-base@11ff885`, spot-diffed against Gerrit `mirror-goog-studio-main` |
| Android Studio IDE side | `JetBrains/android@0867bfe` |
| androidx.inspection (`ArtTooling`) | `androidx/androidx@fc135bf`, `inspection/` |
| slicer | AOSP `platform/tools/dexter@main` (Gerrit REST) |
| Framework (`am`, ActivityThread, LoadedApk, providers, sockets) | GrapheneOS `platform_frameworks_base@17` (Android 17); `aosp-mirror/platform_frameworks_base` tags `android-8.0.0_r1` … `android-16.0.0_r1` |
| ART (JVMTI, agents, class definition, DexFile) | LineageOS `android_art` `lineage-15.0` … `lineage-23.2` |
| run-as, sepolicy, libcore, bionic | GrapheneOS `@17` and LineageOS branches |
| adb | GrapheneOS `platform_packages_modules_adb@17`, `aosp-mirror/platform_system_core` tags (adb before Android 12), LineageOS `android_packages_modules_adb`, Android Studio's `adblib`; read-only probes of a local adb server (platform-tools 37.0.0, server version 41) |
| OkHttp, Okio | `square/okhttp` tags `parent-3.5.0` … `parent-5.5.0` (source, API dumps, `javap` of release jars); `square/okio` `1.13.0` … `3.18.2` |
| Socket discovery precedent | Chromium `@c6b97f11` (`android_device_info_query.cc`), `facebook/stetho` |
| Rust crates | crates.io and docs.rs, 2026-09-29 |
| Platform docs | developer.android.com (16 KB page sizes, Android 14 behaviour changes), source.android.com (ART TI), W3C HAR 1.2 |
