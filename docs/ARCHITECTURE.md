# traffic-police architecture

Status: describes what is built: Phases 0 to 4 (released as 0.1.0 to 0.3.1), the fixes and additions of 2026-10-07 (the rule form, the socket-name fallback, fuzzing, the device tests; §9.2), and Phase 5: WebSocket messages (§4.2), gRPC (§4.9) and the Flutter backend (§5.16), released with the one-line installers and the Android libraries' Maven repository (§8) as 0.4.0. Then Logdawg, the device's log as Android Studio's Logcat shows it (§5.17), and another app without quitting (§5.5), released as 0.5.0. It began as the Phase 0 design; where the building changed it, the text says what was built and why. PROTOCOL.md defines the wire format; this document defines everything else. Statements about Android, ART, OkHttp, adb and Android Studio were checked against their sources; §11 lists where.

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

- A terminal equivalent of Android Studio's Network Inspector: pick a device and a debuggable app process over adb, then watch its HTTP and HTTPS traffic live with a traffic graph, a connection list with a waterfall, full request and response details, the initiating thread and call stack, a thread view, and response rewrite rules. Then add terminal-native utilities (filters, search, copy as cURL, HAR, sessions, diff, decoders, headless modes). Nothing is redacted (decided in review, 9.2). After 0.4.0, at the user's request, also the device's log beside the traffic, as Studio's Logcat shows it (Logdawg, 5.17).
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

The device's log takes its own way in (5.17): `logcat -B` over adb → the log reader → the same event channel → the store's log → view 4.

Data flow for one request: hook → `Recorder` event (app thread, microseconds) → queue → writer thread (encode, `seq`, ring, socket) → adbd → adb server → host backend (decode, normalize) → store (apply, index) → UI (render visible rows, at most `[ui] fps` times a second: 60 unless set).

## 3. Repository layout

```
docs/                    ARCHITECTURE.md, PROTOCOL.md, research notes
host/                    Cargo workspace (Rust 2024 edition); crate names in parentheses
  crates/proto/          (traffic-police-proto)     wire codec and message types
  crates/core/           (traffic-police-core)      event model, Backend trait, store, filters, rules model, decoders, exporters
  crates/adb/            (traffic-police-adb)       adb server client and discovery
  crates/backends/       (traffic-police-backends)  demo, device socket, session file, HAR import, attach orchestration
  crates/tui/            (traffic-police-tui)       Ratatui application
  cli/                   (traffic-police)           the `traffic-police` binary
android/                 Gradle build (Kotlin DSL)
  capture-core/          the runtime without Android APIs (Java 8 jar): protocol, recorder, OkHttp and
                         HttpURLConnection capture; tested on the JVM against eight OkHttp versions
  capture/               the Android library (AAR): public API, auto-start provider, socket server
  capture-noop/          same public API, no capture code
  attach-agent/          JVMTI agent (C++17, NDK, CMake) + vendored slicer + boot trampoline dex
  sample-app/            Kotlin app exercising every capture path
testdata/protocol/v1/    golden frames shared by Java and Rust tests
testdata/logcat/         logcat's binary records from API 26, 31 and 37 emulators (5.17)
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
| Attach entry | `io.trafficpolice.internal` (in `capture`) | core, Android SDK | `AttachEntry`, started by the agent: answers the hooks, loads the OkHttp adapter against the app's OkHttp, starts the socket server |
| Boot trampoline (attach only, separate dex) | `io.trafficpolice.boot`, and `io.trafficpolice.capture.attach.ExitHandler` | `java.lang` only | `Trampoline`, `NativeBridge`, `ExitHandler` (4.7.3). `ExitHandler` is compiled with capture-core but ships only in the boot dex, so the runtime links to the one type the trampoline accepts |

The OkHttp adapter is compiled in two source sets merged into one artifact: everything against **OkHttp 3.14.9 + Okio 1.13.0** (the API baseline OkHttp 4 checks binary compatibility against, and the oldest Okio any supported OkHttp ships), except `ForwardingEventListener`, which is compiled against **OkHttp 5.5.0** so it can override and forward all 33 callbacks. A test in the OkHttp 3.9.0 suite (`ApiBaselineTest`; planned since Phase 0, built 2026-10-07) reads the constant pools of the compiled classes and looks up every `okhttp3` and `okio` class, field and method they reference in OkHttp 3.9.0 with Okio 1.13.0. Newer members are allowed only where they cannot run on an older OkHttp: `RequestBody.isOneShot()` and `isDuplex()`, which `TeeRequestBody` forwards and OkHttp calls only from 3.14 on, and the `EventListener` callbacks newer than 3.9 that `ForwardingEventListener` forwards. A guard that is no longer needed fails the test too, so the list stays exact.

**Supported OkHttp versions: 3.9.0 and later** (the first release with a public `EventListener` and `Chain.call()`). Older OkHttp 3.x (2015–2017) is detected and left alone with a `diag`. OkHttp 2 (`com.squareup.okhttp`) is out of scope; Android's own HttpURLConnection, which is built on an internal OkHttp 2 fork, is covered through 4.3.

Every hook body catches `Throwable` from our own code and falls back to pass-through. Exceptions that are the app's or the network's (for example an `IOException` from `proceed()`) are recorded and rethrown unchanged. OkHttp delivers non-`IOException`s thrown inside an interceptor of an async call to the uncaught-exception handler, which crashes the app, so our code must never throw one.

### 4.2 Capturing OkHttp

**Where we sit.** OkHttp's chain is the same in 3.x, 4.x and 5.x: application interceptors → RetryAndFollowUp → Bridge → Cache → Connect → **network interceptors** → CallServer. Network interceptors therefore see wire-accurate headers (after Bridge added `Accept-Encoding`, cookies, `Host`), run once per network attempt (each redirect or retry hop separately), never run for cache hits, and never run for WebSocket handshakes (`if (!forWebSocket)`). This matches Studio, whose interceptor sits in the same place.

**Two cooperating parts.**

1. `CaptureEventListener`, created per call by `TrafficPolice.eventListenerFactory(existing)` (library mode) or by the wrapped `eventListenerFactory()` (attach mode). `callStart()` runs on the caller's thread for both `execute()` and `enqueue()` in every supported version, so it records the real initiating thread and stack. Its other callbacks record timing marks (DNS, connect, TLS, connection acquired, request headers and body, response headers and body, call end or failure). The listener is registered in `CallRegistry` (a synchronized `WeakHashMap<Call, CaptureEventListener>`; `RealCall` uses identity equality, and the listener never references the call strongly, so abandoned calls are collected). On OkHttp 5.x, connect events can fire on background threads (fast fallback), so the listener never correlates by thread.
2. `CaptureInterceptor`, the network interceptor. It finds the call's listener through `chain.call()` (the same `RealCall` the listener received), creates the transaction, and pulls any marks recorded before it ran (DNS and connect happen in ConnectInterceptor, before network interceptors). Marks that happen during the exchange are attributed to the current transaction; a redirect's second hop gets its own transaction with the same `call` id and `hop = 1`.

Without the listener (app did not install it in library mode), the interceptor records its own thread and stack (`thread.origin = "interceptor"`, which for `enqueue()` is an OkHttp dispatcher thread) and only interceptor-level marks.

**The thread's kernel id** (after 0.4.0). With the thread's name and Java id, the runtime sends its kernel id (`thread.tid`, PROTOCOL.md §4), the one logcat shows, so the host can mark the thread's log lines (5.17). `ThreadStack` asks `android.os.Process.myTid()` through reflection, because capture-core uses no Android APIs, once per thread (`Tids`, a thread local); off Android (the JVM tests) there is none and the field is left out, so the golden frames did not change.

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
- **WebSockets** (Phase 5). Handshakes bypass network interceptors, and OkHttp builds the WebSocket client with `eventListener(EventListener.NONE)`, so neither hook sees a socket. The socket itself is wrapped instead (`WebSockets.java`): the app's `WebSocketListener` (the 101, every message that arrives, the close, a failure) and the `WebSocket` the app sends with (`send`, `close`). Library mode: `TrafficPolice.newWebSocket(client, request, listener)` in place of `client.newWebSocket(request, listener)`. Attach mode: an exit hook on `OkHttpClient.newWebSocket(Request, WebSocketListener)` (`okhttp_new_websocket`) swaps the `listener` field of the `RealWebSocket` it returns for ours (a field of that name and type from 3.9 to 5.5), and returns our wrapper. The app's listener receives the wrapper too, so everything the app sends is seen. The handshake is one transaction: its request headers are the app's, because OkHttp adds `Upgrade`, `Connection` and `Sec-WebSocket-*` later. Each message is a `ws` event (PROTOCOL.md §7.1). Pings and pongs stay inside OkHttp. One gap in attach mode: a handshake that finished before `newWebSocket` returned reaches the app's own listener with OkHttp's socket, and then what the app sends through that one is not captured and the transaction has no 101. `newWebSocket` only enqueues the handshake, a network round trip on another thread, so it takes a server that answers within a millisecond while the app's thread is paused: it happened once, on 2026-10-09, in the JVM test on a busy CI runner, with the server in the same process (the test's server now answers after 250 ms, as one over a network would). Closing it needs a hook before the handshake starts, such as one on `RealWebSocket`'s constructor; library mode does not have the gap. Tests: `WebSocketCaptureTest` on every OkHttp version in the matrix (both paths, and a refused handshake), and the sample's WebSocket scenario on devices in both modes.

### 4.3 Capturing HttpURLConnection

`TrafficPolice.wrap(connection)` (library mode) and the `URL.openConnection()` exit hooks (attach mode) return a `TrackedHttpsURLConnection` or `TrackedHttpURLConnection` that subclasses the platform type and delegates every method. Other `URLConnection` types (`file:`, `jar:`) are returned unchanged. The state machine follows Studio's, which encodes years of HttpURLConnection quirks, with fixes for the gaps its sources show:

- The transaction starts on the first call that connects: `connect()`, `getOutputStream()`, `getInputStream()`, or any response getter (`getResponseCode()`, `getHeaderField*()`, `getContent*()`, …). The method is recorded as `POST` when `doOutput` is set on a `GET` (HttpURLConnection only switches after connecting). Thread and stack are captured there, on the app's thread.
- Request bodies are teed from `getOutputStream()`. The request body ends at stream close, or, if the app never closes it, when the response is first touched.
- **Status and headers are read before `getInputStream()`**, so 4xx/5xx responses keep their status and headers even though `getInputStream()` throws `FileNotFoundException`. `getErrorStream()` is teed too (Studio passes it through, so error bodies are never captured).
- The response body is teed from `getInputStream()`/`getErrorStream()`; EOF, `close()` and `disconnect()` all end it. `disconnect()` always ends the transaction, even if no stream was opened.
- Both overloads of `URL.openConnection` are hooked in attach mode: `openConnection(Proxy)` does not delegate to the no-arg overload in libcore, so Studio misses proxied connections.
- Android's HttpURLConnection already removes its own transparent gzip before the app reads, so HUC response bodies are captured decoded, with headers that match.

### 4.4 Rules on the device

Rules (PROTOCOL.md §8) are evaluated in the process, because only there can a response be changed before the app sees it. `RuleSet` compiles the host's set and finds the rules for a request; `RuleRun` applies them without knowing the client; `OkHttpRules` (in the network interceptor) and `HucExchange` (in the HttpURLConnection wrapper) drive it.

- Matching uses the request as it goes on the wire (method, scheme, host, port with 80/443 defaults filled in, encoded path, decoded query parameters). Studio's matcher compares `URL.getPort()`, which is `-1` for default ports, so a port criterion of 443 never matches there; ours normalizes.
- Rules belong to the host connection that sent them. When it ends they stop applying (a connection that took over keeps its own), so quitting traffic-police gives the app its normal behaviour back. They keep applying while recording is paused, when nothing is recorded.
- `delay` sleeps before `proceed()`, in 50 ms slices so that a canceled call stops waiting. `fail` throws before `proceed()` (so `proceed()` runs zero times, which OkHttp allows). The exception kinds are `timeout` (`SocketTimeoutException`), `io` (`IOException`), `protocol` (`ProtocolException`), `unknown_host` (`UnknownHostException`) and `connect` (`ConnectException`). Only OkHttp 3.9 to 3.12 retry a simulated failure (`io`, `connect` and `unknown_host`, on a pooled connection or to another address), and the rule then fails the new attempt too; later versions retry only failures they saw on a connection. The tests measure this on every supported OkHttp (PROTOCOL.md §8.4).
- Status and header actions rebuild the status line and headers without buffering the body.
- Body actions read the original body completely (limit 32 MiB; above it the original streams through and the action is skipped with a `rule_skipped` diagnostic), close the original body (OkHttp refuses to continue a call whose previous body is still open), and deliver the new body with `Content-Encoding` removed and `Content-Length` set. Bridge's transparent gzip then leaves it alone, and `ResponseBody.bytes()` length checks hold (Studio never fixes `Content-Length`, which breaks `bytes()` on non-gzip bodies).
- `replace` decodes `gzip` and `deflate` per `Content-Encoding` (`br` and `zstd` cannot be decoded on the device without libraries: the action is skipped with a diagnostic), reads the text in the Content-Type charset (UTF-8 by default) and writes it back in the same charset. It is literal unless `regex = true`; a literal replacement never interprets `$` or `\` (Studio always treats the replacement as a regex template). A `body` action needs no decoding, so it works on any encoding.
- Upgrades (a 101, or `Connection: upgrade` both ways) are never rewritten, paused or not. WebSocket calls skip network interceptors altogether.
- OkHttp stores what leaves the network interceptors in its HTTP cache, so a rewritten response could outlive the rule. By default a rule that changes the status, headers or body also sets `Cache-Control: no-store` on the delivered response and lists that change in its `rule` event; `cache_rewrites = true` on a rule turns this off.
- HttpURLConnection: the wrapper finds the rules when the exchange starts. Delays and failures happen in the first call that connects, and every getter answers from the delivered response: the status line, headers by name and by index, and the input and error streams, which throw for an error status as the platform does (Android: `FileNotFoundException` for every status of 400 and above; the JDK: only for 404 and 410). The getters `URLConnection` derives from headers (content type and length, dates) run its own code over the delivered headers, so they agree. `scripts/gen_huc_wrappers.py` generates both wrappers from one method list.
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
- **Traffic sampling.** A sampler reads `TrafficStats.getUidRxBytes/TxBytes(Process.myUid())` every 500 ms and emits a `traffic` event when the values changed (Studio's graph is built from exactly these uid totals). The host draws the graph from them by default (5.8).
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
- **Public API** (identical in `capture-noop`): `networkInterceptor()`, `eventListenerFactory()`, `eventListenerFactory(EventListener.Factory)`, `wrap(HttpURLConnection)`, `wrap(URLConnection)`, `newWebSocket(OkHttpClient, Request, WebSocketListener)` (Phase 5), `start(Context)`, `isActive()`. In the no-op artifact, `networkInterceptor()` returns a trivial pass-through interceptor, `eventListenerFactory(existing)` returns `existing` (or `EventListener.NONE`'s factory), `wrap()` returns its argument, and `newWebSocket` calls `client.newWebSocket(request, listener)`; the no-op contains no capture code, no provider and no socket.
- **Order matters** for the listener: if the app calls `eventListener(...)` or `eventListenerFactory(...)` after ours, it replaces ours; the Overview then shows `origin: interceptor` and the Call Stack tab explains why.
- **R8/ProGuard.** The library ships consumer rules that keep its own classes; OkHttp's names do not matter in library mode because the app links against them directly.
- **Hidden APIs.** None. Everything used is public SDK: `LocalServerSocket`, `LocalSocket.getPeerCredentials()`, `Credentials.getUid()`, `SystemClock`, `TrafficStats`, `Process`, `ApplicationInfo`.
- **Artifacts (Phase 1).** `capture` (AAR) depends on `capture-core` (jar), which holds everything that needs no Android API and is therefore tested on the JVM; `capture-noop` stands alone. None of them has a runtime dependency (the build sets `kotlin.stdlib.default.dependency=false`, since AGP 9's built-in Kotlin would otherwise add `kotlin-stdlib` to every module). `minSdk` is 21, OkHttp's own floor, so any app that can use OkHttp can add the library; the tested range is API 26 to 37 (9.1).

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
- The agent is built with NDK r28 (installed here: 28.2) and explicitly passes `-Wl,-z,max-page-size=16384` for all three ABIs. A 4 KB-aligned `.so` fails to `dlopen` on 16 KB-page devices (Android 15+) unless the target app happens to run in page-size compat mode.
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
                                                     hooks on: set ClassFileLoadHook (+ ClassPrepare on API 26–27), so a
                                                       class defined from now on is instrumented as it is defined
                                                     start "traffic-police-init" thread ─► load runtime (InMemoryDexClassLoader),
                                                                                        AttachEntry.start → runtime, writer (no socket yet)
                                                                                        Trampoline.installHandler → hook calls reach the runtime
                                                                                        GetLoadedClasses → RetransformClasses(java.net.URL,
                                                                                          okhttp3.OkHttpClient if loaded before the attach)
                                                                                        AttachEntry.listen → the socket
 discover @traffic-police_<pkg>_<pid>, forward, connect ◄─ hello (hooks: installed / failed / pending)
```

- **Hooks before the socket (2026-10-07).** The agent instruments classes from the moment its load hook is set, and the runtime opens its socket only after the handler is installed and the classes loaded before the attach are retransformed, so a host that sees `hello` sees the hooks as they will stay, and nothing the app does after that is missed. Until then the load hook ignored classes while the runtime was still loading, and the socket opened before the hooks were live: on a slow device an `OkHttpClient` defined in that window stayed unhooked until the retransform pass, and the app's first requests after the attach were lost (the nightly API 36 failures of 2026-10-02 to 10-06: init, challenge, attest, enroll and one status poll). The trampoline passes every value through until the handler is installed, so instrumenting early changes nothing for the app.

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

#### 4.7.6 As built in Phase 4

- **Host side** (`traffic-police-backends::attach`). When the target has no capture socket, the device backend finds its process (`--pid`, `--process`, else the package's main process; names from `track-app`, else `/proc/<pid>/cmdline`), refuses a process that is not debuggable, waits while Android has it frozen, and installs the agent: the `.so` for the process's ABI (the `track-app` ISA, else the package's `primaryCpuAbi`, else the device's first ABI; `primaryCpuAbi` exists on API 26, 31 and 37 and is `null` for apps without native code) and both dex files are pushed to a staging directory of their own under `/data/local/tmp/traffic-police/`, then copied by one `run-as` script into `code_cache/traffic-police/`. The script keeps a file whose bytes are the same (a running process of the app may have it mapped) and replaces a different one by renaming a new copy over it, never by writing into it; copies are owner-only and read-only (`.so` 0500, dex 0400) and named after the script's shell pid, so two sessions installing into one app at once (two of its processes) do not collide. Then `cmd activity attach-agent <pid> <so>=dir=<dir>;package=<package>`: the options are the directory and the package, nothing more.
- **Once per pid, and a 5 s limit.** The agent is sent to a process once. When its socket has not appeared 5 s later, the backend reads the process's log (`logcat --pid`): the agent's warnings and errors, ART's (`Unable to dlopen`, `Agent attach failed`), the framework's (`Attaching agent … failed`), and the first exception printed after the agent loaded. If the log names a cause, the session ends FAILED with it, adding that the app must be restarted before another try (an agent stays in a process until it exits); if not, it keeps waiting and says that the app's main thread may be busy. A process that dies before its socket appears is checked for a crash (its fatal exception or signal) and reported the same way.
- **Debuggable only (§7).** Before anything is copied, `run-as <package> true` must succeed: `run-as` refuses packages that are not debuggable on every build, including userdebug devices where `attach-agent` alone would allow them, and it fails where it is disabled. On userdebug images (the emulator's `default` and `google_apis` images) Android runs every app with JDWP, so `track-jdwp` and `track-app` call every process debuggable; the picker therefore asks `run-as` once per package and lists only the packages it accepts (all of them where `run-as` is disabled).
- **Idempotent attach.** A second attach of the same `.so` path calls `Agent_OnAttach` again in the loaded copy, which logs it and does nothing: the first attach stays in charge. A copy from another path (the startup agent's, then a runtime attach) or of another version finds the boot dex's `Trampoline` with `FindClass` before it touches anything, and stays inactive; for another version it logs that the app must be restarted (4.7.2's version check).
- **Launch (4.7.4).** `--launch` in attach mode restarts the app (`am start -S`), so its start is a new process: on API 30+ after installing the startup agent; on 27–29 with `--attach-agent <so>=<options>` (first tried on devices on 2026-10-07: emulators of API 27, 28 and 29 capture all 26 of the sample's start-up requests this way); on API 26 plainly, and the backend attaches as soon as the process appears (it looks every 200 ms), saying that the first requests may be missed. A process that loads the agent itself (first seen while a startup agent is installed, or within 10 s of an `--attach-agent` launch) gets 5 s to open its socket before a runtime attach, so a process never gets two agents. In library mode, `--launch` is also done by the backend now (a plain `am start`, no restart).
- **Startup agent.** Installed for `--launch` on API 30+, and for `--follow` on API 30+, so that every restart is captured from its start, other processes of the app too (the sample's `:worker`). `agent.conf` holds `expires_at_ms` (the device's clock plus 5 minutes) and the package; a task renews it every minute while traffic-police runs. The startup agent goes when the first connection is made (without `--follow`) or when the session ends in any way but a kill; after a kill it does nothing once its stamp has passed, and `doctor` lists it with the command that removes it. `startup_agents` itself is removed only when empty.
- **The process name at launch.** A startup agent can run before `bindApplication` names the process (`<pre-initialized>` until then; seen on API 31), so the runtime reads the name again when a host connects.
- **Where the agent comes from** (the CLI): `--agent-dir` or `TRAFFIC_POLICE_AGENT_DIR`; else the agent built into the binary (`TRAFFIC_POLICE_EMBED_AGENT=1` at build time, or a directory: `host/cli/build.rs`; the release workflow does this, about 3.7 MB); else `android/attach-agent/build/outputs/agent` in the source tree the binary was built from, or under the working directory.
- **Picker and `--mode`.** Without `--mode`, the picker lists the processes that run capture (●) and the debuggable ones that do not (○, where Enter attaches the agent); `--mode library` offers only the first kind, and `--mode attach` also attaches after a restart of a picked process that already ran capture (`--follow`). With `--package`, the default stays library mode.
- **Verified** on emulators at API 26, 31 and 37 (37: a user build with 16 KB pages), and since 2026-10-07 also 27, 28 and 29 (`default` arm64 images), with the sample's plain build, by the end-to-end test `plain_app_attach_end_to_end`. Since 2026-10-07 it begins with an attach as soon as the app's process exists (the app loads OkHttp while the agent starts; 26 of 26), waits for the app to be ready before the attach that tests a client built before it, and requires `--launch` to capture all 26 from API 27 on. Before: a runtime attach to a running app whose OkHttp client was built before (26 of 26 requests, every detail checked); the `:worker` process and a restart followed from their start on 31 and 37 (on 26 a restart loses its first five requests, as expected); `--launch` (26 of 26 on 31 and 37); nothing left in `startup_agents` and no forwards afterwards. By hand: attaching again after the app restarts (the first host code failed there: its `cp` could not overwrite the read-only copies), three sessions on one app at once, a non-debuggable app and a missing package (refused), a broken runtime dex (the cause from the log after 5 s), a second attach over a broken agent, an expired startup agent (inactive, and found by `doctor`), the picker on API 26 and 31, and a release binary with the agent built in, run outside the source tree.
- **WebSockets** (Phase 5, 9.1 #4): the `okhttp_new_websocket` exit hook (§4.2). **gRPC** (Phase 5): the `grpc_channel_interceptors` and `grpc_stub_channel` exit hooks (§4.9). The end-to-end test checks the sample's socket (the handshake and six messages in order) and its three gRPC calls in every run. A run has had 30 requests since then, and the agent seven hooks. The attach to an app that is already running reaches its gRPC channel, built at start, through the stub hook.

### 4.8 Support matrix

| | API 26 | 27 | 28–29 | 30–33 | 34+ |
|---|---|---|---|---|---|
| Library mode | ✓ | ✓ | ✓ | ✓ | ✓ |
| Attach at runtime | ✓ | ✓ | ✓ | ✓ | ✓ (runtime dex in memory) |
| Late-loaded OkHttp in attach mode | ClassPrepare + retransform | ClassPrepare + retransform | always-on load hook | always-on load hook | always-on load hook |
| Capture from launch | — | `--attach-agent` (verified on an emulator) | `--attach-agent` (verified on emulators) | `startup_agents` | `startup_agents` |
| Agent `.so` constraints | process ABI | process ABI | process ABI | process ABI | process ABI; 16 KB-aligned on API 35+ devices |

| OkHttp runtime | What works |
|---|---|
| < 3.9 | Not supported (no public `EventListener`, no `Chain.call()`); pass-through with a `diag` |
| 3.9–3.13 | Full capture; no `requestFailed`/`responseFailed` events; no one-shot/duplex bodies exist |
| 3.14–4.x | Full capture; newer listener callbacks (proxy select, canceled, cache) as available |
| 5.x | Full capture; listener composed with `EventListener.plus()` on 5.3+; connect events may arrive on background threads |

### 4.9 Capturing gRPC (Phase 5)

grpc-java on Android talks HTTP/2 through grpc-okhttp. That transport carries its own copy of OkHttp 2's framing and never touches `okhttp3`, and the Cronet transport is invisible too. So gRPC is captured at gRPC's own API, with a `ClientInterceptor` (`io.trafficpolice.capture.grpc`). It is compiled against grpc-api 1.21.0, the oldest version it supports. Clients that are not grpc-java (Wire's `GrpcClient`, Connect) run on `okhttp3`, so they are already captured as HTTP.

- **Where it sits.**
  - Library mode: `TrafficPolice.grpcInterceptor()`, added to the channel first. Interceptors run in the reverse of the order they were added, so the first runs last, next to the transport, and sees what the app's other interceptors added.
  - Attach mode, channels built after the attach: an exit hook on the channel builder's package-private `getEffectiveInterceptors` puts ours at index 0, the same place. The method has three signatures: `AbstractManagedChannelImplBuilder()` in 1.10 to 1.32, `ManagedChannelImplBuilder()` in 1.33 to 1.63, `ManagedChannelImplBuilder(String)` from 1.64. The agent tries each. A class that exists without the method (the forwarding shim of 1.34 to 1.58) is not a failure.
  - Attach mode, channels built before the attach: these are reached through the stubs. An exit hook on `AbstractStub.getChannel()`, which every generated stub reads for each call, returns the channel with our interceptor on top.
  - Missed: calls made with `channel.newCall` on a channel built before the attach (not through a stub). With `--launch` every channel is built after the attach.
- **One capture per call.** Two of ours can see the same call: the library's and an attach hook's, or a stub's channel over a channel that has ours. A claim passed down in `CallOptions` lets the innermost capture the call; the others stand aside.
- **What is recorded** (PROTOCOL.md §7.1, gRPC calls).
  - The URL, from the call's authority and the method's full name.
  - The headers as the stream sends them, from a `ClientStreamTracer`, so call credentials (`authorization`) are included. From 1.40 they come from `streamCreated`; before 1.40, from the tracer factory, which then ran as the stream was created. 1.40 is checked by name, since the code is compiled against 1.21.
  - The transport's address and TLS session, from its attributes.
  - Each message, re-marshaled with the method's marshaller and framed as gRPC frames it. Protobuf's marshaller can be read again; gRPC itself does so for each retry. A marshaller that hands out the message itself (an `InputStream`) is not read.
  - The response headers, and the trailers with `grpc-status` and `grpc-message` rebuilt from the status, because gRPC strips them.
  - Whether the server or the client library ended the call, from the tracer's `inboundTrailers`.
- **What it cannot see.**
  - The transport adds `content-type`, `te`, `user-agent` and `grpc-timeout` below every interceptor. The first two are constant and the third follows from the deadline, so they are added to the request headers; `user-agent` is not known.
  - HTTP/2's `:status` never reaches gRPC's API, so 200 is reported.
  - Compressed messages are captured uncompressed.
  - Re-marshaled bytes can differ from the wire's in field order; for protobuf the content is the same.
- **Threads.** The stack is captured in `interceptCall`, on the thread that made the call: a stub's caller. The listener's callbacks run on gRPC's executor and only record. Every callback catches its own errors (an `internal_error` diag), because an exception from `onHeaders` or `onMessage` would cancel the app's call.
- **Tests.**
  - `GrpcCaptureTest` uses the in-process transport, on gRPC 1.21.0, 1.39.0 (the last before `streamCreated`), 1.40.1, 1.63.0 and 1.84.0. It covers unary and streaming calls, a server error with trailers, a deadline, a cancel, two interceptors on one call, both attach hooks, pause, and the cap.
  - `GrpcOkHttpTransportTest` uses grpc-okhttp, plaintext and TLS, on 1.84.0.
  - On devices: the sample app's gRPC scenario (a grpc-okhttp server inside the app) in library and attach mode.

## 5. Host side (Rust)

### 5.1 Crates

`host/` is a Cargo workspace. The split enforces the layering rule from the brief: backends produce events, the store owns state, the UI only reads the store.

| Crate | Depends on | Contents |
|---|---|---|
| `traffic-police-proto` | serde, serde_json, bytes | Wire codec for PROTOCOL.md: frame encoder/decoder, typed messages, version constants. Pure (no IO); a Tokio `Decoder`/`Encoder` behind a feature. Fuzzed. |
| `traffic-police-core` | traffic-police-proto | Normalized event model, `Backend` trait, session store, body store with disk spill, filter language, rules model (TOML), body decoders, exporters (HAR, cURL, session file), diff, the device log's store and filter (`logdawg/`, 5.17). No terminal code. |
| `traffic-police-adb` | tokio | adb smart-socket client: device tracking, process tracking, forward, shell v2, sync push, socket discovery, logcat's binary records (`logcat.rs`, 5.17). Fallback to the `adb` binary. |
| `traffic-police-backends` | traffic-police-core, traffic-police-adb, traffic-police-proto | `Backend` implementations: demo generator, device socket (library and attach share it), the Flutter backend (Dart VM service, §5.16), session file, HAR import, the device log's reader (`logdawg.rs`, 5.17). Attach orchestration (push, copy, attach). |
| `traffic-police-tui` | traffic-police-core, ratatui, crossterm | Application state, views, widgets, keymap, themes, mouse hit-testing. Reads the store; sends commands. |
| `traffic-police` (bin) | all | clap CLI, subcommands (`demo`, `open`, `tail`, `record`, `export`, `doctor`), wiring, logging, panic hook. |

Release builds embed the Android artifacts (agent `.so` per ABI, capture dex, trampoline dex) with `include_bytes!` from a build step, so attach mode needs nothing but adb (Phase 4).

### 5.2 Runtime and threading

- One Tokio multi-thread runtime (2 to 4 workers) runs all IO: adb connections, backend sockets, timers, the rules-file watcher bridge.
- The UI loop is one task that owns the `SessionStore` and the terminal. It `select!`s over: terminal input (crossterm `EventStream`), ingest batches from backends (bounded mpsc), results of background jobs, and a frame timer. There are no locks on the store: one owner, one writer.
- Frame pacing: redraw only when something changed: input, data, or the clock moving what is on screen (the live graph, the bars of open requests, a message about to expire). Redraws are paced to at most `[ui] fps` a second, 60 unless set (5.8). The Phase 0 plan was 30 a second for data and 60 for input, with a quiet live view redrawn about 4 times a second.
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
    Traffic { source: SourceId, at: Ts, rx: u64, tx: u64 },                 // whole-app counters (default graph source)
    Dropped { source: SourceId, at: Ts, events: u64, bytes: u64, txns: Vec<u64> },
    Diagnostic { source: SourceId, level: Level, code: String, message: String }, // hook status, warnings
    Marker(Marker),                  // host-side markers: pause, resume, reattach, range notes
    Logs(Vec<LogLine>),              // the device's log, in batches (5.17)
    LogInfo(Box<LogInfo>),           // the log's device, app, uid, process names, reader status
}

#[async_trait::async_trait] // or return-position impl Future, depending on the final toolchain
pub trait Backend: Send + 'static {
    fn info(&self) -> BackendInfo;                    // name, kind (Demo | Device | File | Har), capabilities
    async fn run(self: Box<Self>, sink: EventSink, commands: CommandRx) -> Result<BackendExit>;
}

pub enum BackendCommand { SetRules(RuleSet), SetCaptureConfig(CaptureConfig), Pause, Resume, Ping, Shutdown }
```

- **As built**, there is no `Backend` trait: each source is a task of the same shape, which sends `Vec<SessionEvent>` batches into a bounded channel, takes `BackendCommand`s from another, and reports its `ConnectionStatus` on a watch channel (`run_device`; the demo and the file sources feed the same channel). The UI and the headless commands treat them alike.
- `EventSink` batches events (a `Vec<SessionEvent>` per socket read) into a bounded channel. If the UI falls behind, the channel fills, the backend stops reading the socket, TCP backpressure reaches the device, and the device drops oldest events and reports the count. Nothing blocks an app thread.
- Backends that cannot accept commands (file, HAR) report that in `BackendInfo.capabilities`; the UI greys out pause and rules.
- `TxnKey = (SourceId, device txn id)`. The store maps it to a dense `TxnIdx` (u32) in arrival order.

**Demo backend.** `traffic-police demo` runs a simulated device that *encodes real protocol frames* (PROTOCOL.md) into an in-memory stream decoded by the same code as a live connection, so the demo exercises the protocol path end to end. Scenario scripts (seeded RNG, real or virtual clock):

- a pretend shop app (`com.example.shop`, a debug build) whose API is a dev server on `http://localhost:8080` (as over `adb reverse`), with sign-in, images and telemetry on example.com hosts at documentation addresses (RFC 2606, RFC 5737): a shopping session on `DefaultDispatcher-worker-*` threads, `sessions` → `products` → `cart/items` → `checkout`, then `orders/status?orderId=…` every 1.5 s until the order is confirmed (the demo replaced an identity-SDK scenario after v0.1.0, at the user's request, with placeholders only);
- background telemetry (`events`) and a notifications poll with gzip-encoded JSON;
- a 302 redirect followed to a 200 (two hops, one call), a 404, a 500, a read timeout, a cancelled call;
- a PNG avatar, a 5 MB download streamed over several seconds, a protobuf body, a multipart upload, a form POST;
- a JWT in `Authorization`, `Set-Cookie` duplicates, an active rule that rewrites one response, a `dropped` event, and a `diag` warning;
- optionally (`--restart-after 60s`) a simulated process death and relaunch to show DETACHED and reattach markers.

With a virtual clock the generator produces a fixed timeline instantly; the snapshot tests use that.

### 5.4 adb client and discovery

- `traffic-police-adb` speaks the adb server protocol on `127.0.0.1:5037` (or `ADB_SERVER_SOCKET` / `ANDROID_ADB_SERVER_PORT`). The exact requests and replies are in PROTOCOL.md Appendix A. The rules that shape the client: one outstanding request per TCP connection (the server does not support pipelining), a timeout on every request (some malformed requests get no reply), devices addressed by transport id rather than serial (ids are exact; serial matching is fuzzy), and device capabilities decided from the device's feature list rather than its API level (adbd is an updatable module).
- **Long-lived tasks.** One device tracker per server (`host:track-devices-proto-binary` when the server advertises `devicetracker_proto_format`, else `host:track-devices-l`). Per online device: a process tracker (`track-app` on Android 12+ devices with the `track_app` feature, which also reports debuggability and the process architecture; else `track-jdwp` plus `/proc/<pid>/cmdline` for names) and, while a picker or `--follow` needs it, a socket scan (`cat /proc/net/unix` every second, filtered to listening `@traffic-police_` names).
- **When `/proc/net/unix` cannot be read** (the brief: "compute the socket name from the package and the pid"; built 2026-10-07: until then an unreadable file read as "no sockets" and `doctor` called it readable). The scan reports the failure (`AdbError::Unreadable`, with what the shell said), and the runtime is then looked for by name: for each debuggable process of the package, `traffic-police_<package>_<pid>` is probed (`Adb::probe_abstract`): a forward and a connection that either closes at once (no such socket), brings the runtime's first byte (it speaks first), or stays silent (a process Android froze, which counts as there). Nothing is sent, and a runtime lets a connection take over only after `hello_ack`, so a probe never disturbs the session of another host. The device backend, `doctor` and the picker do this (the picker asks `run-as` first, so only apps that can run capture are probed, and remembers each answer per pid; a "no" is asked again after 5 s, since the agent can start a runtime later). Verified on an API 26 emulator whose `/proc/net/unix` was made unreadable for the shell, in both modes.
- **Shell commands** use `shell,v2,raw:` so every command has an exit code (minimum API 26, so the shell protocol is always there), with arguments single-quoted.
- **Pushing** the attach-mode files uses the sync protocol directly (`SEND`/`DATA`/`DONE`, 64 KiB chunks).
- **Forwards** are created per connection (`tcp:0`, so the server picks the port) and removed with `killforward:tcp:<port>` when the connection ends. They also vanish whenever the device goes offline or the server restarts, so the backend re-creates them on every transition back to `device`. A forward succeeding proves nothing about the runtime (a forward to a missing socket connects and then reads EOF); only `hello` does. The client never uses `killforward-all`, which would also remove Android Studio's forwards.
- **Server restarts.** Every connection drops and transport ids restart from 1. One supervisor reconnects with backoff (250 ms to 5 s), invalidates all cached ids, rebuilds the device table from the tracker's first message, then re-establishes per-device trackers and forwards. Live sessions show DETACHED during the gap and reattach automatically when the same process is still alive (same `instance`, resume after the last `seq`).
- **As built in Phase 1.** `watch_devices()` keeps one `track-devices` connection per session in a task that reconnects with backoff (250 ms to 2 s) and publishes each list on a watch channel; the pickers and the waiting device backend read it. Process lists are read once a second while a picker shows them (one `track-app` message, else `track-jdwp` plus `/proc/<pid>/cmdline`), and the socket scan runs once a second while the backend waits for a process; a live session needs neither, because the socket's EOF reports a process exit at once. Decisions after a connection ends ask adb directly (`host:devices-l`, the socket scan), because the tracker can report a disconnect a moment after the socket closes. The client does not restart a server that disappears mid-session (someone may have stopped it on purpose); the session waits and says so. Once per device, the backend removes forwards to `@traffic-police_*` sockets that no longer exist, left by a traffic-police that was killed; forwards to live sockets, and every other forward, are left alone.
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
- **Another app in the session** (after 0.4.0, at the user's request). `A` (or `:app`) opens the device and app picker on the session's screen, on the session's device; `Esc` there lists the devices, and `:q` goes back. A device's Android version is read only while the devices are listed, so nothing is asked of a device the user does not look at. The capture runs under `run_session` (`backends/src/session.rs`), which runs one capture at a time and passes the UI's commands to it. An app picked in the session gets the target the picker at the start would give it (the agent for one without the library, Flutter mode kept). The capture before says goodbye and removes its forward, within 3 s or it is stopped, and the next starts with the latest pause and rules. Logdawg's reader is told the app (5.17). The requests so far stay; the timeline marks the switch with a note, and the old source ends as it does on quit. Tested with the fake adb (`fake_session.rs`) and live on an API 31 emulator, from the library-mode sample app to the plain one (the agent attached).
- **Frozen processes (found in Phase 1).** Android 11 and later freeze processes that are cached in the background (the cgroup v2 freezer; on by default from Android 14): they run no code, so the runtime cannot accept a connection or answer a ping, and a connection attempt waits unanswered in the socket's accept backlog. The backend reads `frozen` from the process's `cgroup.events` (the path comes from `/proc/<pid>/cgroup`) before connecting, when `hello` is late, and when pings go unanswered past the next ping; while the process is frozen it keeps a single connection open, sends nothing, and shows WAITING with the reason. When Android thaws the process (the app comes back to the foreground, or a service or broadcast starts in it) the session continues where it was: nothing is missed, because frozen code makes no requests. The picker marks frozen processes.
- **The header** shows the backend's own account of the connection: LIVE, WAITING (for the device, the app, a thaw, or a reconnect, with the reason), DETACHED (with the reason; data kept), FAILED (protocol mismatch, with both versions and the fix).

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
    logs: LogStore,                        // the device's log (5.17)
    generation: u64,                       // bumps on every change; the UI's dirty check
    changed: Vec<TxnIdx>,                  // drained by derived views for incremental updates
}
```

- Transactions are `Arc`s updated with `Arc::make_mut` (copy-on-write). A freeze snapshot clones the `Vec<Arc<_>>` (about 400 KB of pointers at 50,000 rows, well under a millisecond) plus small metadata; later updates copy only the transactions they touch. The body store is append-only, so a snapshot bounds its reads by the lengths it recorded.
- A `Transaction` holds: key, source, call id and hop (redirect chains), client kind, method, URL (raw plus parsed scheme, host, port, path, query), ordered request headers, optional response (status, message, protocol, ordered headers, remote address, TLS), body references per direction (`Request`, `Response` as received, `ResponseDelivered` when a rule changed the body), rule effects with the original status line and headers, thread, stack (shared `Arc<[Frame]>`), timing marks, start and end timestamps, state (`Pending`, `Sending`, `Waiting`, `Receiving`, `Complete`, `Failed`, `Detached`), sizes, and flags (rule-modified, gap, truncated).
- Headers are `Vec<(String, String)>`: order and duplicates are preserved everywhere, including HAR export.
- **Body store.** Chunks are kept in memory as `Bytes` until a budget (default 256 MiB) is exceeded; then the oldest and largest bodies are spilled to an append-only file in a per-run temp directory (`<temp>/traffic-police-<pid>-<random>/`), leaving `(offset, len)` references. The directory is deleted on exit, from the panic hook, and on SIGTERM/SIGHUP; on start-up, directories of dead pids are removed. Each body records its state: `Streaming`, `Complete`, `Truncated { captured, total }`, `NotCaptured(reason)`, `NotConsumed`, `ClosedEarly`, `Gap` (chunks lost to device overflow).
- **Traffic series.** Bytes are added at the device timestamp of the chunk (or progress event) that carried them, plus header sizes at request and response start. Storage is sparse 10 ms bins (a bin exists only where bytes were seen), and a window's buckets are summed from the bins in it. Past an hour's worth of fine bins (360,000), the older half merges into 100 ms bins (built 2026-10-07; planned since Phase 0), which a window over that time spreads over its buckets as a rate, so a long session costs a tenth as much per hour and no byte is lost. HAR imports spread bytes evenly across each entry's send and receive phases. The whole-app series (the default graph source) is kept separately as the raw `traffic` samples (cumulative counters every 500 ms, per source), converted to rates per bucket at draw time; buckets narrower than the 500 ms sampling interval show the sample's rate as a step.
- **Time.** All durations and bins use device monotonic nanoseconds. Wall-clock labels use the per-source offset from `hello` and refreshed by `pong`. The session origin is the first source's `hello` time (or the first event, for files).

### 5.7 Derived views

- **Row model.** The Connection View shows `rows: Vec<Row>` where a row is a transaction or a collapsed group. It is maintained incrementally: a changed transaction is re-evaluated against the filter and inserted, moved, or removed by binary search on the sort key. Changing the filter or sort recomputes everything once (50,000 predicate evaluations take a few milliseconds; body-content filters run in the background and fill in progressively).
- **Sort.** Default chronological (request start, then arrival). Any column sorts stably; live appends insert in place.
- **Collapse repeats.** Consecutive rows (in the current sort) with the same method, host and path collapse into a group row with a count, a combined time span, and the latest status; Enter or Right expands it. Groups are recomputed only when the row set changes.
- **Graph range selection** adds a time-overlap predicate to the filter.
- **Thread lanes.** One lane per `(thread id, name)`, in order of first request; overlapping bars in a lane stack into sub-rows.

### 5.8 UI

- **Layout.** Header (device, process, pid, state, active rule count, and the session's totals on the right) · the Network box (traffic graph) · the Requests box (views) beside the request's side (resizable divider): the body explorer box (the response body, or on its second tab the request body (added after v0.1.0 at the user's request), explored with `j`/`k`, folded with Enter and `[` `]`; `h`/`l` switch between the two bodies as they switch the tabs below) above the Detail box (tabs; added in Phase 3 at the user's request) · a key-hint footer. Each box has rounded borders (`[ui] borders`: plain, double or thick instead) with its title, tabs and notes (legend, filter, match count, search result) on the border, and the focused box's border is highlighted (decided in the Phase 1 review, 9.2); the others are as bright as the text in the dark palette (dark grey until v0.1.0; the user asked for white). The active tab is bold, in the accent color when its box has focus (until v0.1.0 also underlined; the user asked for no underline). The footer shows the keys that work in the focused box or open dialog, dropping hints from the middle when the line is short so help stays visible; messages appear on its right. Under 140 columns (`[ui] side_by_side`) the detail pane opens full-width over the list. The graph is a quarter of the screen high (8 to 14 rows) unless `[ui] graph_height` sets its rows, and the body box takes 40% of the detail pane unless `[ui] body_height` sets its share. `graph_height = 0` hides the graph, and `[ui] body_box = false` the body box, which `B` also hides and shows again while running (asked for after 0.3.1; until then `body_height = 0` hid it, and still does); a hidden box leaves the focus cycle (its focus goes to the tabs), and the tabs take its room. `[ui] hints = false` leaves only the messages in the footer. Boxes one above the other are `[ui] gap` rows apart (0: their borders on neighbouring rows) and boxes side by side `2 × gap + 1` columns, because a terminal cell is about twice as tall as it is wide: so the space between any two boxes looks the same (the user found the boxes side by side closer together than the ones above each other, when both were adjacent). Under 100×30 the UI shows a "terminal too small" notice instead of a broken layout. The device and process pickers fill the screen: a box with the title on its border, a margin of one row and two columns inside it, the columns spread over the width, and the keys at the bottom (until v0.1.0 the list sat in the top left corner).
- **Frame pacing.** The loop draws at most `[ui] fps` frames a second: 60 unless set, 10 to 240 (asked for after Phase 3, 9.2). One rate covers input, new data and the clock: a live view is drawn at that rate, because the clock moves the graph, the bars and the times of open requests, and the loop sleeps until the next frame is due. A still view (an opened file, a frozen or detached session) is not drawn at all, and nothing is drawn while an editor has the terminal. The first frame after a pause is drawn at once; the frames that follow keep a beat, each due one frame time after the one before was due rather than after it was drawn, because Tokio's timer rounds up to the millisecond and the loop wakes a little after that (paced from the drawing, the demo drew 177 frames a second instead of 240, and 54 instead of 60). A frame that comes a whole frame late starts a new beat, so there is no burst to catch up. Until this setting, input and data redrew within 16 ms and the clock at about 30 frames a second. Before each frame the loop takes every key already waiting (at most 256 a turn) and every batch of data that has arrived, whichever source woke it: when frames take longer than the frame time (a slow terminal, SSH), the frame timer is ready at every turn and wins most of them, each turn draws a frame, and keys taken only when their own branch won waited a frame each time they lost. Measured with a terminal that takes 5 KB a second and 40 keys typed at 20 a second, both before the frame rate setting (its 33 ms clock frames had the same effect) and with it: the keys taken at all had waited 8 to 12 seconds, and a `q` typed afterwards took 6.6 seconds or was not taken within 8. Now all 40 are taken, a third of a second later at the median and 0.8 seconds at most (about one frame written at that rate), and `q` in 0.7 seconds. Work the frame queues (large bodies, `body:` searches) runs on workers between frames. Measurements are in section 6.
- **What a frame writes.** The UI draws through its own ratatui backend (`screen.rs`), which wraps crossterm's. A frame that changes cells is written as one synchronized update (terminal mode 2026: `CSI ? 2026 h`, the changed cells, `CSI ? 2026 l`), so a terminal that knows the mode shows the frame whole and never half-drawn; clearing the screen after a resize opens the update, so the empty screen is not shown either. A terminal that does not know the mode ignores the two sequences, as it ignores any mode it does not know (checked with tmux 3.6b, which takes them without passing them on), so the terminal is not asked first. A frame that changes nothing writes nothing, and the cursor is hidden, shown or moved only when that changes: crossterm's backend (ratatui-crossterm 0.1.2) resets the colors and hides the cursor on every frame, 25 bytes that would wake the terminal at the frame rate while the picture stands still. Leaving the UI (also on a panic) ends an update that was open.
- **Focus** cycles Graph → List → Body explorer → Detail with Tab and Shift+Tab (skipping hidden boxes; Logdawg has the graph and its list only). Each region records its screen rectangles during render; mouse events are hit-tested against them (rows, tabs, divider, graph).
- **States in the header:** LIVE, PAUSED (device not recording), FROZEN (UI shows a snapshot; ingest continues and the status bar counts what arrived since), DETACHED, REPLAY.
- **Traffic graph.** Two solid areas drawn by our own widget, their edges in eighths of a row (`▁▂▃▄▅▆▇█` rising, `▔🮂🮃▀🮄🮅🮆█` hanging), each row shaded from dim at the baseline to the series' color at the far rows (Receiving blue, Sending orange): the `smooth` style, the default since the user found the braille curves' single trail of dots, stepped at every dot and gapped by the font, not smooth enough on the mirror's halved height (2026-10-04); the curves are the `curves` style now, so the user's `graph_style = "smooth"` kept meaning the smoothest. The hanging half's partial cells need the upper-eighth blocks of Symbols for Legacy Computing (Cascadia Code, the Nerd Fonts, Iosevka; the user's CaskaydiaCove NF has them; Menlo and SF Mono do not): with `[colors] background` set to the terminal's own, those cells are drawn instead as a lower block in that color over a cell filled with the series' color, which every font can show. Without colors the solid areas draw as curves. By default the two series mirror each other around a zero line (`┄`, faint, with the markers' `┊` crossing it): receiving rises above it and sending hangs below, each half scaled to its own peak, with the tops of the two scales in their series' colors in the gutter. Android Studio draws both above one baseline on one scale, and so did this graph until v0.2.0: an SDK that polls a few KB in while pushing a few hundred KB out flattened the receiving line against the baseline, where its shape was lost (the user's screenshot of 2026-10-04). That look remains as `[ui] graph_layout = "overlay"` (or the `:` palette's layout command). A series growing downward is the same renderer with its rows mapped the other way and each turn glyph swapped for its vertical mirror; the ratatui chart of the braille style plots the values negated under a `[−max, 0]` axis. The app's counters arrive a 500 ms tick at a time, so the raw rates are steps; each curve is that traffic averaged over a second by default (`[ui] graph_smoothing`, half a second to five; captured bytes, which come as spikes, first spread over a tick), which turns the steps into slopes, and then over half that, which rounds the corners. A second is Android Studio's time base for rates; the half-second of v0.2.0 showed every poll as its own bump, and the user asked for smoother. The line is joined at every one-dot step (the dot it steps from is drawn too), since dots that touch only at corners read as a gap in a font's braille. The averaging runs on a grid fixed in time four times finer than the columns, reaching past the window's ends, and each column takes the curve at its centre, interpolated between the two fine samples around it: the slices never move, so a shape stays exactly the same while it scrolls through, and the window ends wherever "now" is, so the graph glides every frame instead of stepping a column at a time (the step styles still end their window on a column, since a step that moved within a cell would wobble). While following live with an app connected, the graph ends one second (two ticks) before "now", or as far as a wider smoothing window looks ahead (`graph::lag_ns`): the tick in progress has not arrived yet, and drawn sooner the newest stretch would drop to zero and then fill in. The step lines in heavy box-drawing characters (`┏━┓┃┗┛`, one bucket per cell column; the default from the Phase 1 review until the user asked for a smoother graph after v0.1.0), thin rounded lines and plain braille styles stay available (`area`, the receiving area under a sending line, grew into the solid areas; `area` and `solid` still parse as `smooth`) (`:` palette, or `[ui] graph_style`), with the same delay, in either layout. Each y-axis auto-scales to a "nice" maximum in human units (B/s, KB/s, MB/s), on steps of 1.5 and 1.33 in turn (1, 1.5, 2, 3, 4, 6, 8, 12 …) so a curve is never drawn under half of its axis: it is never below the highest value shown, glides to a new maximum instead of jumping, and comes down a step only when the values fit under the lower one with 15% to spare, so values near a step do not flap it; the mirror's two halves each have their own, and the column width comes from the zoom rather than the window, so the first frame of a session (an empty window) costs nothing. The x-axis shows `mm:ss.mmm` since session start or wall-clock time (toggle), with the tick labels under the plot. Markers (attach and reattach, detach, pause and resume, dropped-event notes) are dotted vertical lines drawn into the empty cells of the plot, colored by kind. Live mode keeps the right edge at "now" (a second before it while an app is connected, as above); moving back in time (←/→ with the graph focused, Shift+wheel or a horizontal wheel, a range selection) stops following until L. The wheel alone zooms. `v` starts a keyboard range selection; mouse drag selects directly. While a range is selected, the Timeline column and the Thread View span that range.
- **Graph source** (decided in review). By default the graph plots **all of the app's network traffic**: the whole-app `TrafficStats` uid counters the runtime samples every 500 ms, which is what Android Studio plots. It includes non-HTTP traffic and HTTP from clients traffic-police does not hook, so a spike may have no matching row. `T` switches to **captured requests**: bytes of the captured bodies and headers at the device time they were written or read, which line up with the rows and resolve finer than 500 ms. The legend and the header's rate readout name the active source. Sources without whole-app counters (HAR imports, or a device where `TrafficStats` is unsupported) fall back to captured requests automatically, and the legend says so.
- **Connection View.** A custom virtualized table: only visible rows are formatted each frame. Columns are one space apart, two after a right-aligned one (`302 B  json`) and three before the Timeline (`1.61 s   ▐█`; the user found `Time Timeline` run together). Columns: Name, Size, Type, Status, Time, Timeline, plus optional Method, Host, Path, Thread, Start, Request size, Protocol, Client. The Timeline column shares the graph's window. Each bar has three shades (sending, waiting, receiving) and sub-cell precision in eighths: a block anchored to the cell edge it touches (`▏▎▍▌▋▊▉█` from the left, `▕🮇🮈▐🮉🮊🮋█` from the right, the latter mostly Symbols for Legacy Computing, drawn as a left block in `[colors] background` over a cell in the bar's color when that is set), or to the nearer edge when it touches neither, always of the bar's width; where two phases meet inside a cell the bar fills, the cell shows both colors (a left block in one over the other). The bar's width and its phases are measured from its start, not rounded separately, so a bar keeps its width as it scrolls: until v0.3.0 the right-anchored blocks were ⅛, ½ and full only, so a bar's leading edge stood still and jumped as it scrolled, and a short bar whose phases met inside one cell filled the whole cell (the user asked for a smoother timeline, 2026-10-04). Status text is always present (`200`, `404`, `failed`, `···`) and colored by class. Rule-modified rows show a `✎` marker, later hops of a redirect `↪`, rows with dropped events `!`, pinned rows `★`, and the row marked for a diff `◆`. When parked at the bottom in live mode, the list follows new requests (`[ui] follow`). With a request open it stays where it is, so the request being read does not change under the reader, until `G` (`End`, `Ctrl+G`) at the bottom: then the open request follows the newest too, each from its top, until the cursor moves away (asked for after v0.1.0; before, only closing the request brought following back). Sort keys are cached per transaction and re-sorted incrementally, so a sorted list stays cheap while traffic arrives. Columns are laid out to the pane: when the list is narrow the Timeline column goes first, and Name keeps at least 12 cells.
- **Thread View.** Lanes on the shared time axis; bars are selectable and open the detail pane.
- **Logdawg** (`4`): the device's log, 5.17.
- **Rules view.** Ordered list with enabled toggles, match summary, action summary, and per-rule hit counts; errors from the file or the device shown inline; editing in `$EDITOR`, with a form planned (5.11.1).
- **Detail pane tabs.** Overview, Response, Request, Call Stack (Studio's order), with the fields from the brief, and after 0.4.0 Logs: the app's log while the request ran (5.17). `o` toggles original and modified when a rule changed the response; `p` toggles Parsed and Source. `[ui] tab` sets the tab a request opens on.
- **Wrapping** (`[ui] wrap`, on by default; asked for after v0.1.0, first for the Overview and then for every box). Text that is read wraps in its box instead of being cut at the edge: the four detail tabs (headers, bodies, stack frames), the body explorer, the diff and decoded-value boxes, menus, and the Rules view's selected rule and messages. What is a table or a bar stays one line per item so its columns and bars line up: the request list, thread lanes, the rules list, the pickers, the header and the footer (the header drops whole items when it is short). Breaking (`wrap::layout`): at spaces; a label row continues under its value, the detail's other own rows two columns in, body lines and frames two columns past their own indent. After a ` · ` (the separator between items), an item that does not fit on the line goes to the next one whole when it fits there, and also when its first word would otherwise be left alone at the end of the line (`iss` without its URL). A word wider than a line starts where it is and breaks after `/ . - _ ? & = : , ;` or before `(` `[` when that leaves the row at least half full (`…RealCall.callStart` / `(RealCall.kt:171)`, URLs after a `/`), else at the edge. Rows wrap as they are drawn, from the view's top, so a body costs only the rows on screen; a line longer than 4 KB keeps where it breaks per width (a minified body is one line of megabytes). A view's top is a line and how many of its rows are above the box (`wrap::Top`), so a line taller than the box scrolls through: `j`/`k` move by lines (a line that fits is shown whole; a taller one from its first row), Ctrl+D/Ctrl+U, the page keys and the wheel by rows, the cursor keeping its row of the box. Search runs over whole lines, so a match where a line breaks is found and highlighted on both rows and counted once; Enter and copy act on the whole line. With `wrap = false` lines are cut at the edge and `<` `>` scroll the detail tabs sideways (with wrapping they say so).
- **Half-page jumps.** Ctrl+D and Ctrl+U (also Ctrl+P) move by half the box, as Neovim's `scroll` option does: in the list, the body box and the detail tabs the view and the cursor move together, the Rules view moves its cursor, and the diff and value views scroll; on the graph they move the window by a quarter. `[ui] scroll` sets a number of lines instead of half the box.
- **Freeze** renders from a store snapshot (5.6); unfreezing jumps back to the live store.
- **Virtualization everywhere.** Ratatui's `Table` allocates every row it is given and `Paragraph` scrolls only up to 65,535 lines and re-wraps from the top each frame, so neither ever receives more than the visible slice: the list, thread lanes, body viewers, hex dumps and diffs all keep their own offsets into indexed data and build widgets for the visible rows only. A one-cell scroll indicator shows the position in the total count.
- **Terminal hygiene.** Alternate screen, raw mode, mouse capture, bracketed paste. Mouse capture enables any-motion reporting; motion without a button is ignored. The terminal is set up by our own code rather than `ratatui::init()`, whose panic hook restores only raw mode and the alternate screen: our panic hook also disables mouse capture and bracketed paste. SIGTERM, SIGHUP (the window was closed) and SIGINT (from `kill`; raw mode turns Ctrl+C into a key) end the loop the way `:q` does, so the app is told goodbye and the adb forward is removed; on Windows, closing the console window does the same. Ctrl+C typed while an editor has the terminal reaches us too and is left to the editor. The terminal lives in a guard that restores it on drop and skips ratatui's own drop, which reports a failure to show the cursor with `eprintln!` and so panics once the window is gone; and a panic in the UI still waits for the backend's goodbye before it carries on. Input is handled against the current row order (several keys can arrive between two frames).
- **Editor.** Enter on an app frame in Call Stack resolves the file through the source roots in `.traffic-police/project.toml` (package directory first, then a search by file name, since Kotlin files need not match their package) and opens `$VISUAL`/`$EDITOR` at the line (`+N file`, or `file:N` for VS Code, Cursor, Zed, Sublime and Helix). The editor runs as a child process that the event loop awaits: input is released to it, capture keeps flowing into the store, and the screen is repainted in full afterwards. The loop sleeps meanwhile; until the frame rate setting it kept waking for a frame it could not draw, a whole CPU core for as long as the editor was open.
- **Images.** The graphics protocol is detected once, after entering the alternate screen and before the input stream starts (the probe reads stdin), with a short timeout; half-blocks are the fallback. Inside tmux the default is half-blocks, because the image library enables tmux's `allow-passthrough` as a side effect of its probe; `TRAFFIC_POLICE_IMAGES=auto` opts in, and `--no-images` (or `[ui] images = false`) forces half-blocks everywhere. Snapshot tests always use half-blocks.

### 5.9 Body decoding and viewers

The pipeline for a body is: bytes (memory or spill file) → Content-Encoding decoding → kind detection → viewer model → rendered lines (visible range only).

1. **Content-Encoding** chains are decoded in reverse order (`gzip`, `x-gzip`, `deflate` with or without the zlib header, `br`, `zstd`) with pure-Rust decoders, and the output is capped (default 256 MiB) to defuse decompression bombs. Both sizes are shown ("18.2 KB transferred, 96.4 KB decoded"). A decode failure shows the raw bytes with the error.
2. **Kind detection** uses Content-Type first (`*/json` and `*+json`, `*/xml` and `*+xml`, `text/html`, `application/x-www-form-urlencoded`, `multipart/*`, `image/*`, `application/x-protobuf`, `application/protobuf`, `application/grpc*`), then magic bytes and a JSON sniff.
3. **Viewers:**
   - JSON: a hand-written single-pass tokenizer produces the pretty-printed lines, syntax spans, fold ranges and JSON paths together (fast on multi-megabyte bodies, and key order, big integers and number spelling are preserved exactly as sent); a foldable tree (fold state per node); the JSON path of the cursor line on the detail box's bottom border (until v0.1.0 it was drawn over the last row and hid what was there); a jq-style filter (jaq) whose output replaces the view until cleared. A filter can loop forever and a thread cannot be stopped, so each filter runs in a child process (`traffic-police __jq`) that is killed after 3 s; outputs are capped at 200 values and 16 MiB. The UI task only queues the filter and shows the result when it arrives. jaq's `halt` is handled as an error instead of letting the library exit the process.
   - XML and HTML: pretty-printed by one tolerant markup tokenizer of our own (HTML is not XML, and malformed XML should still be readable), which emits the highlighting spans as it goes; void elements, `<script>` and `<style>` are handled for HTML.
   - Form-urlencoded: decoded key and value table in original order, duplicates kept.
   - Multipart: parts with their headers, each with a nested preview (recursively using this pipeline).
   - Images: decoded and drawn inline through kitty, iTerm2, or sixel graphics when the terminal supports them, else half-blocks; always with format, dimensions and size.
   - Protobuf and gRPC: schemaless decode in the style of `protoc --decode_raw` (field numbers, wire types, nested messages detected heuristically, strings shown when valid UTF-8); gRPC's 5-byte message framing is split first.
   - Everything else: text if valid UTF-8, else a hex dump rendered lazily for the visible window.
4. **Body states** are always explicit: "truncated at 10 MB of 48.2 MB", "not captured (capture disabled)", "not consumed by the app", "closed early after 12 KB", "streaming…", "gap: 64 KB lost to device buffer overflow".

Bodies up to 256 KiB are decoded on the UI task; larger ones on a worker (`spawn_blocking`), one build per body at a time, with the previous view shown meanwhile (a 10 MB JSON body takes about 55 ms). Decoded views are cached in an LRU keyed by `(TxnIdx, dir)` with a byte budget (128 MiB); clearing the session discards builds still running. Pretty-printed lines and their token spans are generated once per body and are width-independent; each frame materializes only the visible columns of the visible lines, so a 50 MB single-line body costs about a microsecond per line.

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
  | `header:x-request-id`, `header:"x-request-id=abc"` | a request or response header by name, or whose value contains the text (added in Phase 2 for the value menu's "filter by this value") |
  | `body:"needle"` | request or response body contains the text (after decoding); evaluated in the background |

  Parse errors are highlighted in place and the previous valid filter stays active.
- **Search.** `/` in the detail pane searches the current body view (n and N step; matches highlighted). `body:` in the filter bar is the cross-session search ("which request returned this value").
- **Copy** (`y` menu): as cURL (POSIX single-quote escaping; text bodies inline with `--data-binary`, binary bodies written to a file next to the command and referenced as `@file`; headers in order; `--compressed` when the request asked for gzip; no PowerShell variant yet), URL, request or response headers, one header, body (decoded), the value at the cursor's JSON path. `clipboard = "auto"` uses OSC 52 over SSH or when no display server is present (it works inside tmux with `set-clipboard on|external`, which the status bar hints at on failure) and the native clipboard otherwise; `osc52`, `native` and `off` force a choice. OSC 52 is copy-only by design.
- **Save and export** (`w`, `e` menu): save a body (binary-safe, extension from Content-Type or magic); HAR 1.2 of all, filtered, or selected rows, with `_trafficPolice` custom fields for thread, stack, rules and timings beyond HAR's. An entry's timings add up to its `time`, as HAR 1.2 requires: each phase starts where the one before ended, a request that failed or was cancelled before an answer spent its last part in `wait`, and `blocked` holds what no phase covers (queueing, picking a connection). Until 2026-10-07 the timings were the bare phases, so the time between them was in none, and a timeout's ten seconds were in none at all (found by the HAR schema test, §8); HAR import through `traffic-police open file.har`. An imported entry becomes a transaction of a `har` source with its timings, headers and bodies; a file traffic-police wrote also brings back the source, thread, stack, failure, pin and exact timing marks. HAR keeps bodies decoded, so imported bodies are stored decoded and decoded without the recorded `Content-Encoding` (the headers shown stay as recorded); the transferred (compressed) size is not kept. Entries without a date or URL are left out and counted in the REPLAY note.
- **Sessions** (`e` menu, `traffic-police record`, `traffic-police open`): one file (`.trafficpolice`), see PROTOCOL.md §10. It stores the event stream exactly as captured plus host annotations, so reopening reproduces timings, threads, stacks, rule effects, pins and markers.
- **Diff** (`d` marks; the second mark opens the diff): request lines, status lines, headers (in order, or with `s` as sets: names lower-cased and sorted), and bodies. JSON bodies are canonicalized (keys sorted recursively, pretty-printed) before a line diff, so key order does not matter; other text is diffed as is, with three lines of context around each change; binary bodies, and bodies over 4 MB, compare size and SHA-256. `n`/`N` step through changes and `y` copies the diff as text.
- **Decoders.** JWTs are detected in `Authorization: Bearer` and in any string of the form `xxx.yyy.zzz` whose header decodes to JSON (in headers, and in bodies up to 256 KB); the Overview lists each with its algorithm, expiry, subject and issuer, and Enter there opens the decoder: header, claims, and `exp`/`iat`/`nbf` in local time with an age relative to the session's clock (the recording's end in a replay). The signature is not verified, and the decoder says so. Enter on any header or JSON value (a container still folds) opens a value menu: copy, decode JWT, decode base64 (standard and URL-safe; offered for values that plausibly are base64), URL-decode, filter by this value (`header:"name=value"` or `body:"value"`).
- **Command palette** (`:`): every named action with its keys, plus the graph styles, which have no key; typing narrows the list (every word must appear; matches at word starts rank first, subsequences last) and Enter runs the choice as its key would in the focused box.
- **No redaction** (decided in review, 9.2). Headers and bodies are shown and exported exactly as captured, secrets included, so screenshots, HAR files and session files need the same care as the app's own logs.
- **Pins** (`m`): bookmark rows; `is:pinned` filters.
- **Pause and freeze.** Space sends pause/resume to the device (it stops creating transactions; in-flight ones finish; rules stay active). F freezes the UI (5.8).

### 5.11 Rules (host side)

- Stored in `.traffic-police/rules.toml` in the project (the nearest ancestor of the working directory containing `.traffic-police/`, or `--project DIR`). Schema in PROTOCOL.md §8 (the TOML form mirrors the wire form); every problem is reported with its line.
- The UI and the headless commands (`tail`, `record`, `export --live`) poll the file, and the files its bodies come from, every 250 ms, and read it once it has stayed unchanged for 200 ms (polling works the same on every platform and needs no dependency). A valid change is pushed to the device at once (`set_rules`); an invalid one is shown in the Rules view and the status bar, or listed on stderr, while the last valid set stays active. Headless commands also report on stderr the rules the app refused (`rules_ack`). That the headless commands follow the file too was decided in the Phase 3 review.
- The Rules view lists the file's rules with their hit counts (from `rule` events) and the selected rule's match and actions; its footer says where the file is, how many rules are on, and how many the app says it runs (`rules_ack`). Space turns a rule on or off by rewriting its `enabled` value in place (comments and layout stay); `K` and `J` move the selected rule up or down in the file (rules apply in file order: the brief's "rules can be toggled and reordered"; built 2026-10-07), its comments with it. Enter opens the rule in the form (5.11.1), `E` opens the file in `$EDITOR` at the rule, and saving either applies it. `r` opens the form for a new rule: on a request, one that matches it exactly (method, scheme, host, port, path, and the query parameters present, with any value); in the Rules view, an empty one. Nothing is written until the form is saved; without a `.traffic-police/` directory one is created in the working directory. (Until 2026-10-07 Enter opened `$EDITOR`, and `r` appended the rule to the file, off, with a placeholder `status` action, and opened the editor.)
- Bodies loaded from `file = "…"` are read on the host and sent inline (base64 for binary).
- The device reports per-rule compile errors (`rules_ack`) and every application (`rule` events). The detail pane shows the response the app received, `o` the original; `rule:modified` and `rule:<id>` filter for what rules changed. `tail --json` and HAR exports hold the delivered response and keep the original next to it.
- The demo shows two built-in rules instead of a file.

#### 5.11.1 The rule form

Scoped after the Phase 3 review, at the user's request, for a later phase; built on 2026-10-07 when the user asked for everything the plan had left (`crates/core/src/ruleform.rs` holds the model, the checks, the saving and the matching; `crates/tui/src/ruleform.rs` the form). As planned, with these details:

- **What it is.** A form inside the Rules view for making and changing a rule without leaving traffic-police. Enter on a rule opens it in the form, and `$EDITOR` stays one key away; `r` on a request opens the form filled in from the request, instead of appending the rule to the file and starting the editor. The file stays the single source of truth: rules written by hand open in the form, and the form's saves reach the app like any saved change.
- **Fields.** The rule: id (made up from the path, editable, unique), name, on or off, `cache_rewrites`. The match: methods (a checklist), scheme, host and path (each exact, glob or regex), port, query parameters (a name and a value pattern each). The actions, in order (add, remove, move), each with its own fields: `delay` (ms), `fail` (exception, message), `status` (code, reason), `header` (add, set or remove; name, value), `body` (text, a file in `.traffic-police/`, or base64; content type), `replace` (find, with, regex).
- **Checked as you type.** The checks the file gets (PROTOCOL.md §8.1) run on every change and show next to the field they concern; a rule with problems cannot be saved.
- **Trying the match.** The form shows how many captured requests the match selects, and the Requests list can show just those, so a glob or a regex is checked against real traffic before it is saved.
- **Saving** writes the rule into `rules.toml` with comment-preserving TOML editing, so hand-written comments and layout stay. That needs the `toml_edit` crate, a new dependency (today's in-place edits only toggle `enabled` and append new rules).
- **Keys** (all in `[keymap]`): ↑/↓ (and Tab, Shift+Tab) between lines; Enter types into a text field (Enter or Tab keeps it, Esc puts back what was there), turns an on/off line, or steps a line of choices; ←/→ step choices (scheme, pattern kind, action type, exception, header op, body source) and move along the methods checklist, where Space ticks; `a` adds an action (a menu of the six types, each with a letter), Delete or Backspace removes the action or query parameter under the cursor, `K` and `J` move an action, Ctrl+S saves, Esc leaves (asking first when something changed), `E` opens the file in `$EDITOR` at the rule (after saving or leaving the changes). A click puts the cursor on a line; a second click changes it as Enter does.
- **How the checks find their field.** The form writes the rule as its own small TOML document, one field per line, and runs the file's parser on it (so the checks are the file's, not a copy of them); each problem's line says which field it belongs to, and it is shown under that line. The id must also differ from the file's other rules.
- **Saving** edits only the rule's own values with `toml_edit` (a new dependency): a value that changed is replaced where it is, keeping the comment after it; a new key is indented like its neighbours; an empty optional value is removed; a value the file states at its default (`regex = false`) stays; actions keep their own comments when they are moved, added or removed. A new rule is appended in the layout the file uses. If the file changed since the form opened (an edit in `$EDITOR`), the rule is found again by its id. A rule that has a multi-line text body shows its first line; `E` edits it.
- **Trying the match** uses the device's comparisons (PROTOCOL.md §8.2: methods and hosts without case, `*` in a host glob within a label and in a path glob within a segment, `**` across, regexes found anywhere in the value); a regex only the device's engine understands (look-around, back-references) is said to be tried on the device only. The last line lists the selected requests in the Connection View (a filter the bar shows as the form's match; `3` goes back to the form, as it was).
- **Done** as planned: every rule the file format allows can be made and changed (the rules of README's and PROTOCOL.md's examples save back byte for byte when nothing changed); comments and layout survive an edit; a rule saved from the form reads back into the same form; every action type is made and set in a behavior test, and the form has snapshots at 100×30 and 140×40.

### 5.12 CLI, headless modes, doctor

```
traffic-police                                   interactive: device picker → process picker → session
traffic-police --serial S --package P [--process NAME | --pid N] [--mode library|attach|flutter] [--launch] [--follow]
traffic-police demo [--seed N] [--speed X] [--restart-after SECS]
traffic-police open FILE                         .trafficpolice or .har (REPLAY)
traffic-police tail [--json] [TARGET] [FILTER...]  a line per finished transaction: text, or NDJSON (--events: every message)
traffic-police record --out FILE [--duration 60s] [TARGET] [--filter F]
traffic-police export --har FILE [--input FILE | TARGET --duration D] [--filter F]
traffic-police doctor [TARGET]
```

- `TARGET` is `--serial`, `--package`, `--process`/`--pid`, `--mode`, `--launch`, `--follow`. With one device attached `--serial` is optional.
- The NDJSON schema for `tail` is versioned (`"v":1`) and documented in PROTOCOL.md Appendix B. Without `--json`, `tail` prints one text line per transaction for people. `--events` prints each captured device message as received, not the host's normalized events: the messages are documented and versioned (PROTOCOL.md §7), the normalized events are an internal type. It prints every message, so the filter does not apply to it.
- As built: target flags may come before or after the subcommand; headless commands need `--package` (they have no picker); `tail` also takes `--duration` and `--bodies`; `record --filter` records to a private temporary file and writes the matching requests when the capture ends; `export --input` reads a session or a HAR file. The project's rules apply, and saved changes reach the app while a command runs (5.11). Ctrl+C, SIGTERM and a closed terminal (SIGHUP; on Windows the console window) end a command properly (the app is told goodbye, which removes the forward, and files are finished); status goes to stderr, without panicking once the terminal is gone, and data to stdout. Like the UI, `tail` and `record` begin with the requests the app made before they connected (its replay buffer, PROTOCOL.md §6). A capture that ends FAILED (an app that cannot be attached to, a protocol mismatch) makes the command exit with 1, and `export` then writes no file.
- `doctor` checks, in order, and prints a fix for each failure: adb server reachable and its version (and whether the `adb` binary on `PATH` matches it); device authorized and online; API level (≥ 26); package installed and debuggable (`run-as` works, and `ro.boot.disable_runas` is not set); socket discovery (`/proc/net/unix` readable; runtime socket present in library mode, with the process list from `track-app`/`track-jdwp`); attachability (process ABI and matching agent, device page size for 16 KB alignment, `code_cache` writable through `run-as`, stale `startup_agents` entries, the hook status of a running agent); and host checks (terminal size, color support, clipboard path, config and rules file validity). As built in Phase 2: all but the attachability checks; when `run-as` is disabled, debuggability is read from the package flags instead; frozen processes are reported. As built in Phase 4, with `--mode attach`: where the agent comes from; the target process's ABI (or the one the app starts with) against the agents built; the page size (the agent is aligned for 16 KB pages, so this only informs); `code_cache` writable through `run-as`; what `--launch` can do at the device's API level; our file in `startup_agents` with its stamp (current, or left behind, with the command that removes it) and other tools' agents; and for each capture socket of the package that is not frozen, its `hello`, read without taking the app from a session (answered with `bye`, PROTOCOL.md §6): the mode, the runtime version and each hook's status and hits. A disabled `run-as` fails in attach mode. `doctor` changes nothing (it does not start the adb server either; reading a `hello` adds a forward and removes it) and exits with 1 when a check fails. Since 2026-10-07 every failure prints its fix (three did not: an adb server that does not answer, a device list that fails, a project file that does not read), and an unreadable `/proc/net/unix` is reported as such, with the runtime then looked for by its socket name (5.4); until then the check called the file readable whatever `cat` said.
- **The default app** (2026-10-07): `package` in `.traffic-police/project.toml` (documented since Phase 0, read by nothing until then) is the app to watch when no `--package` is given, for the UI (which then skips the picker) and every command.

### 5.13 Config, themes, keymap

- **User config:** `config.toml` in `$XDG_CONFIG_HOME/traffic-police` (default `~/.config/traffic-police`) on Linux and macOS, and `%APPDATA%\traffic-police` on Windows, overridable with `TRAFFIC_POLICE_CONFIG`. macOS uses the XDG location, as most terminal tools do, rather than `~/Library/Application Support`. Sections: `[ui]` (theme, time format, visible columns, divider position), `[capture]` (body cap, stack depth), `[keymap]` (action = keys), `[adb]` (server address, adb path), `[storage]` (memory budget, spill dir), and after 0.4.0 `[logdawg]` (on or off, the buffers, the history, the memory and the filter at start; 5.17). As built (the README lists every key): `[ui]` also has `graph_style`, `clipboard`, `images` and `fps` (5.8), and `columns` names the optional columns shown besides the default ones; `[capture]` also has `request_bodies` and `response_bodies`. After v0.1.0, at the user's request to make every aspect of the UI configurable, `[ui]` gained the borders, the layout (`graph_height`, `body_height`, `side_by_side`, `hints`, `gap`; after 0.3.1 also `body_box`, the body box on or off), the behavior (`wrap`, `scroll`, `follow`) and the state at start (`view`, `tab`, `body`, `graph`, `sort`, `collapse`), and `[colors]` sets any of the theme's named colors (24, and Logdawg's six levels after 0.4.0) as `#rrggbb` (plus `background`, the terminal's own, which the graph's hanging half uses on fonts without upper-eighth blocks), for both palettes or (`[colors.dark]`, `[colors.light]`) for one. A problem in `[colors]` is reported at the line of its key in its table. Every problem is reported with its line and the default applies in its place: the UI says so briefly, headless commands print the problems, `doctor` lists them. `--theme` wins over `[ui] theme`.
- **Project config** (`.traffic-police/`): `rules.toml`, `project.toml` (default package, source roots for Call Stack → $EDITOR).
- **`[adb] path`** is the adb binary that starts a server when none runs (the client's `ensure_server`), as well as the one `doctor` compares versions with; until 2026-10-07 only `doctor` used it.
- **Color:** `NO_COLOR` switches to a monochrome theme that carries meaning in text, bold and reverse video. (crossterm 0.29 answers color commands under `NO_COLOR` with a bare reset that also clears bold and reverse, so we detect `NO_COLOR` ourselves, force crossterm's color output on, and simply never emit colors.) `COLORTERM=truecolor|24bit` enables RGB; `TERM=*256color*` (and Apple Terminal) uses the 256-color palette; otherwise 16 colors. Dark and light palettes are chosen with `--theme`; `auto` reads `COLORFGBG` when the terminal sets it and defaults to dark. Dark and light themes ship built in; themes define semantic slots (status classes, sending, waiting, receiving, selection, focus border, markers) rather than raw widget colors, and `[colors]` sets the slots by name (`theme::COLOR_SLOTS`); on a 256- or 16-color terminal each is drawn as the nearest color it has.
- **Keymap:** every action has a name, and `[keymap]` maps action names to one or more keys (`ctrl+r`, `shift+tab`, `F`, `?`); defaults follow the brief plus `T` and `R` (9.1), and `B` (the body box off or on; after 0.3.1). Multi-key sequences are not used. Invalid entries are reported with the line and the valid action names. The keys that move, open and close apply inside menus, the column chooser, the `:` palette (where letters type and only the other keys move; Esc and Enter always close and run) and the device and process pickers too, whose key hints follow them (since 2026-10-07; before, those had fixed keys). In a menu an entry's own letter wins over a key that moves or closes, so `q`, `j` and `k` run their entries (until 0.3.1 they closed the menu or moved its cursor). Quitting is `:q` and Enter in the palette (also `:q!`, `:qa`, `:wq`, `:x` and the like, which put Quit first), and in the pickers, which have a `:` line for it (since 2026-10-08, at the user's request). The `quit` action has no key: `q` and Ctrl+C say how to quit, as Neovim does, and `quit = ["q"]` brings `q` back.

### 5.14 Logging, errors, crash safety

- `tracing` logs to `$XDG_STATE_HOME/traffic-police/traffic-police.log` (default `~/.local/state/traffic-police/`; `%LOCALAPPDATA%\traffic-police\` on Windows), with `TRAFFIC_POLICE_LOG=debug` to raise the level; nothing is written to the terminal while the TUI runs. `--log-file` overrides the path.
- User-facing errors appear in the status bar with an action hint; details go to the log. Protocol mismatches produce a dialog that names both versions and the fix.
- Panic hook: restore the terminal, remove the spill directory, print the panic and the log path.

### 5.15 Key dependencies

Versions are the latest stable releases on crates.io as of 2026-09-29 (toml_edit: 2026-10-07); they are pinned by `Cargo.lock` and moved deliberately. The table lists what the code uses; the Phase 0 plan also named crates that were never needed (they are listed after it). The toolchain is pinned in `rust-toolchain.toml` (1.98.1) with `rust-version = "1.90"`: Ratatui 0.30 needs 1.88, and a transitive dependency of the image renderer needs 1.90.

| Need | Crate (version) | Notes |
|---|---|---|
| TUI | ratatui 0.30.2 (crossterm 0.29 backend) | The 0.30 facade re-exports ratatui-core, ratatui-widgets and ratatui-crossterm |
| Terminal and input | crossterm 0.29 with `event-stream`, `osc52` | One crossterm instance shared with Ratatui; `CopyToClipboard` provides OSC 52 |
| Async | tokio 1.53, tokio-stream | |
| Text input | tui-input 0.15 | Every field is one line (the filter bar, search, prompts, the palette, the rule form); multi-line text is edited in `$EDITOR` |
| Images | ratatui-image 11.1 without default features, image 0.25 (png, jpeg, gif, webp, bmp, ico) | Defaults would link the C library chafa; without it everything is pure Rust |
| jq filters | jaq-core 3.1, jaq-std 3.0, jaq-json 2.0 | `jaq_json::Val` preserves key order and number spelling |
| Highlighting | none: our JSON and markup tokenizers emit the spans | They already walk every byte to pretty-print; syntect would be a second pass plus a bundled syntax set. (Considered: syntect 5.3 with `regex-fancy`; its default features link Oniguruma.) |
| Decompression | flate2 1.1 (zlib-rs), brotli-decompressor, ruzstd 0.9 | All pure Rust, so no C toolchain for cross-builds; `zstd` (C) is avoided |
| Diff | similar 3.2 | |
| CLI, config, rules | clap 4.6, serde, serde_json (`preserve_order`), toml 1.1, toml_edit 0.25 | toml_edit keeps comments and layout when the rule form saves a rule or a rule is moved (since 2026-10-07; turning a rule on or off is a splice of its `enabled` value) |
| Files | none beyond `std` | `rules.toml` and its body files are polled (5.11), spill files are plain files (5.6), config and state directories come from the environment (5.13, 5.14) |
| Clipboard | none: OSC 52 through crossterm, else the platform's command | `pbcopy` (macOS), `clip` (Windows), `wl-copy`, `xclip` or `xsel` (Linux), fed on stdin |
| Other | regex, base64, percent-encoding, jiff (local time), bytes, unicode-width, sha2 (diff of large bodies), libc, anyhow, thiserror, tracing + tracing-subscriber + tracing-appender | |
| Tests | insta 1.48, proptest 1.11; libfuzzer-sys 0.4 through cargo-fuzz (nightly, `host/fuzz`, not in the workspace) | `assert_debug_snapshot!` of the buffer when colors matter |

Planned in Phase 0 and never needed: notify and notify-debouncer-full (polling the rules file is enough and works the same everywhere), arboard (the platform's own commands do the job without keeping a process alive for X11 selections), tempfile, memmap2 and etcetera (`std` covers them), shlex (arguments are quoted by our own `adb::quote`), url (the URL model is our own), globset (globs become regexes), async-trait (no `Backend` trait was built, 5.3), ratatui-textarea (one-line fields), and quick-xml (dropped with the tolerant markup tokenizer).

A CI step (`cargo tree -e features`) fails if a C library sneaks back in through default features (chafa, Oniguruma, zstd-sys, dav1d).

### 5.16 Flutter backend (Phase 5)

`--mode flutter` reads a Flutter app's dart:io HTTP traffic from its Dart VM service, the way DevTools' Network page does, with no library and no agent (`backends/src/flutter/`). It works with debug and profile builds (release builds have no VM service) of Flutter 3.22 (Dart 3.4) and newer. Older versions report dart:io's times on another clock, and the backend refuses them with their version. Source notes: docs/research/08-flutter-dart-vm-service.md.

- **Finding it.**
  - The app logs `The Dart VM service is listening on http://127.0.0.1:<port>/<auth code>/` when it starts. The backend reads `logcat -d -v brief --pid=<pid>` and takes the last address the process logged; a later `no longer listening` cancels it. A line that has rotated out of the log buffer means restarting the app, or `--launch`.
  - It forwards `tcp:<port>` (the service listens on the device's loopback), opens `ws://127.0.0.1:<forwarded>/<auth code>/ws` with its own small WebSocket client (`ws.rs`; text frames, fragments, pings), and speaks JSON-RPC 2.0 (`vm.rs`).
- **DDS.** When a Flutter tool (`flutter run`, an IDE) runs the app, its DDS owns the VM service. The upgrade then answers 302 with DDS's address on this computer, and the backend connects there. A `DartDevelopmentServiceConnected` event ends a direct connection; the backend reconnects, and so lands at DDS.
- **Reading.**
  - For each isolate with dart:io's extensions at version 4 or newer, it turns `httpEnableTimelineLogging` on. Pause turns it off, and dart:io then records no new request.
  - Every second it calls `getHttpProfile` with `updatedSince` set to the profile's own timestamp from the previous read. For each entry that ended, it calls `getHttpProfileRequest` once for the bodies.
  - New isolates come from the `Isolate` stream. Logging turned off by someone else is reported (`flutter_logging_off` diag).
  - When traffic-police quits, logging goes off again in the isolates where it was off, because dart:io keeps everything it records in the app's heap.
- **As protocol messages** (`profile.rs`). Each entry becomes one transaction, written as the capture runtime would write it:
  - `req` once dart:io has the request, because its headers are final then.
  - Marks from dart:io's events, each of which marks the end of its phase. `connect_start` is set at the entry's start, which dart:io takes before DNS and connecting.
  - Then `resp`, body chunks, `body_end`, and `done` or `fail`.
  - So the store, session files, `tail` and HAR take Flutter traffic unchanged.
  - The VM's times are wall-clock µs. They become the device's CLOCK_BOOTTIME ns through an offset read once per process: `/proc/uptime` and `date +%s%N` in one shell command.
- **Bodies.**
  - dart:io keeps them whole, without a cap, until the entry ends; the capture cap applies when they are sent.
  - A body dart:io decompressed for the app (`compressionState` `decompressed`) is sent with `body_end.decoded`. The host then does not decode it again, while the headers still show `content-encoding: gzip` (PROTOCOL.md §7.1).
- **What it sees.** dart:io's `HttpClient` and everything on it: package:http's `IOClient`, dio's default adapter, `NetworkImage`, and dart:io WebSocket handshakes. Also clients that report through package:http_profile: cronet_http 1.3 and newer, ok_http, dio's native adapter.
- **What it does not see.**
  - gRPC and dio's HTTP/2 adapter, which use sockets directly.
  - Anything on the Java side: library or attach mode covers that.
  - The order of headers with different names (dart:io keeps headers in a hash map), the call stack (Dart does not report it), and TLS details.
  - Requests made before logging was on.
  - No rules: the VM service cannot change a response.
- **Tests.**
  - `backends/tests/fake_flutter.rs` runs a fake VM service behind the fake adb. It covers a GET with its decompressed body and its timings, a failing POST, each entry and body read once though polled again, pause, the app's exit, DDS (with logging restored on quit), and a Dart too old.
  - Unit tests cover the translator, the WebSocket framing, and reading the address from logcat.
  - By hand (2026-10-07): a Flutter 3.44.9 (Dart 3.12.2) debug app on the API 37 emulator, with a dart:io server inside it. A gzipped GET, a POST and a refused connection were captured every 3 s, in `tail` and in the UI, and logging was off again after quit.

### 5.17 Logdawg: the device's log (after 0.4.0)

At the user's request (2026-10-10), view `4` shows the device's log as Android Studio's Logcat does, beside the app's traffic and on its timeline; the user named the feature Logdawg (only the code that reads Android's own `logcat` keeps that name). Built: the reader, the store, the view and its filter; then the pause, the whole history, the focus, another app (5.5), a request's lines, the requests among the lines, the graph's range and the log in session files, at the end of this section.

- **Reading** (`backends/src/logdawg.rs`, `adb/src/logcat.rs`). One task beside the capture, in every device mode and for the UI only (`tail` and `record` do not read the log). It runs `logcat -B -b main,system,crash` through adb's `exec:` service, which passes the bytes as they are. Without `-T` logcat writes everything the device still keeps first, as Studio reads it, so a run after `:q` shows the app's earlier lines again (until 2026-10-10 it read the last 5,000 lines, of every app). `[logdawg] history` can set a number of lines instead; `0` starts at the device's time when reading begins (before, it read one line).
  - `-B` writes each entry as liblog's `logger_entry` record: the payload's length and the header's size, pid, tid, the wall clock's seconds and nanoseconds, the buffer's id and the writer's uid (version 4, 28 bytes), then the priority, the tag and the message. Nothing is parsed out of text, and a message of several lines (a stack trace) stays one entry. Checked on API 26, 31 and 37 emulators, whose records are in `testdata/logcat/`.
  - The app's uid comes from `pm list packages -U <package>` (the exact package's line); the names of new pids from their `/proc/<pid>/cmdline`, once a second, at most 200 at a time.
  - **Time.** A record has the wall clock; the timeline is the device's boot clock (5.6). The offset is `date +%s%N` minus `/proc/uptime` (plus 5 ms, half of uptime's hundredth), read in one shell command and again every 60 s. Android 8's `date` has no `%N` and prints a literal `N`: then the shell waits for the second to turn and reads the uptime at once, which puts the clock within about 10 ms (before, Android 8's lines were up to a second off).
  - When the device or the stream goes, the reader waits for the device and starts again after the last line it has (`-T` with that line's time, skipping lines at or before it), so nothing shows twice. The view says what it waits for.
  - **Pause** (`Space` in view 4). The UI tells the reader on a watch channel. The reader closes the stream, and the device's `logcat` ends at its next write. Going on is a break like the others: reading starts after the last line, so what the device logged meanwhile comes in once, while the device still has it. Studio's pause also stops reading and reads again on resume. The demo has no reader, so its lines are not taken while paused. The border says "⏸ paused", and in views 1 to 3 `Space` pauses the network capture.
  - **Another app** (5.5). The session tells the reader the app picked. On the same device it goes on after its last line with the new app's uid; on another device it starts with that device's history. A new package makes the store drop the old uid, and the view counts only the current app's capture runtimes as its processes, so `package:mine` is the new app.
  - Lines go to the UI in batches, every 30 ms or 2,000 lines, through the capture's event channel.
- **Store** (`core/src/logdawg/`). The session store holds a `LogStore` beside the transactions.
  - Lines live in chunks of 4,096 behind `Arc`s: a 48-byte record per line (times, ids, level, buffer, an interned tag, the message's place) and the messages' text in one string per chunk; 103 bytes a line in the speed test below.
  - A freeze (5.8) and each frame's view clone the chunk list, an `Arc` per chunk, not the lines.
  - `[logdawg] keep` (64 MiB unless set) bounds the memory: past it the oldest chunks go whole. A line's id stays the same while it is kept, so a view's list of matching ids only loses its front.
  - Log events do not bump the store's `generation`, so the request list is not filtered again for them. They move `latest`, the live edge, but not the session's origin.
- **Filter** (`core/src/logdawg/filter.rs`): Studio's Logcat query language, as its lexer, grammar and filters define it (JetBrains/android `master`, `logcat/src/com/android/tools/idea/logcat/filters/`, read 2026-10-10). The keys `package` and `process`, `tag`, `message`, `line`, `level`, `is` (`crash`, `stacktrace`, a level exactly), `age` and `name`; `-`, `=:` and `~:`; `|`, `&` (which binds tighter) and parentheses; and the implicit grouping, where terms of one key are alternatives and everything else must hold. Where it differs, on purpose:
  - `package:mine` is the session's app (its uid; else its capture runtimes' pids; else processes named after the package), not the packages of an open project. It is an alternative to other `package:` terms, where Studio requires both.
  - Words and `line:` look in the tag, the message and the process's name. Studio looks in its formatted line, which also has the date, the ids and the level's letter.
  - More: `pid:`, `tid:`, `uid:`, `msg:`, a bare `/regex/`, levels by their letter, ages in milliseconds and fractions, measured by the device's clock (Studio's by the computer's), terms side by side inside parentheses, `-(…)`, and the crash buffer counted as `is:crash`.
  - A filter that does not parse is marked where it breaks, and the one before stays. Studio then searches for the whole text instead.
  - Not built: `is:firebase` and Studio's match-case switch (case never matters).
- **View** (`tui/src/logdawg.rs`). `4` puts the focus on the log's list, also from a request's tabs or body box: before 2026-10-10 that focus stayed on the hidden pane, so the keys moved nothing to be seen. Only the lines that pass are listed. Their ids are kept in a `VecDeque`, each new line is tested once as it arrives, and all of them again only when the filter, the app's uid or its pids change, or every second for `age:`. Rows are made for the screen only; a message of several lines, or a long one that wraps (`[ui] wrap`), takes the rows it needs. Control characters are drawn as `�`, a tab as four spaces, since the terminal would act on them; copies keep the text as it was. Columns: time, pid-tid, tag (a color per tag), process (from 150 columns; the package for a process of the app's that ended before its name was read, which `package:` and words match too), the level's badge, message.
- **Measured** (`tui/tests/perf.rs`, release build, Apple M5 Pro): 500,000 lines went into the store in 23 ms, 21 million a second. Testing all of them took 7 ms for `package:mine`, 3 ms with no filter, 7 ms for `level:w tag:OkHttp` and 24 ms for a regex. A frame with 20 new lines took 0.25 ms (0.3 ms at worst); with `age:`, testing everything again each second, 2.7 ms at worst.
- **Tests.** Unit tests: the record parser over the emulators' records, the store, the filter (Studio's examples among them), the clock offset. `backends/tests/fake_logdawg.rs` runs the reader against the fake adb's `exec:logcat`: history and live lines with the app's uid and process names, everything the device has (and nothing from before with `history = 0`), a stream cut and resumed without repeats, a pause and what came meanwhile, another app on the same device, a device that comes late. `device_e2e.rs` (`logdawg_reads_the_devices_log`) writes a line on a device and checks its uid, tag, level and time, and that `package:mine` passes the sample app's lines and only those; it passed on the API 26, 31 and 37 emulators. Snapshots and behavior tests cover the view, the Logs tab, the requests among the lines and the range; `session.rs` tests a log through a file and back; `ThreadStackTest` (JVM) the `tid`.
- **A request's lines** (the detail pane's Logs tab; at the user's request, as the four that follow). The app's lines from the request's start to its end (to now while it runs): the lines of the process that made it, and while it is the session's app those of the app's uid. The first line at the start is found by a binary search, since the log comes in time order, and the lines are read up to 5 s past the end, for lines a little out of order; at most 2,000 are listed, with a note of the rest. Lines of the request's thread are marked `▶`: its kernel id as the runtime sent it (4.2), else the pid for a thread named `main`. The Call Stack tab shows the tid too. Wrapped rows hang under the message (`DocRow::Hanging`).
- **Requests among the lines** (view 4). The view's rows are lines and requests in time order (`Row`): a request at its start, drawn from the transaction each frame, so its status and time stay live. After a change of the filter, the app, the range, or lines that went to make room, the view merges every line that passes with every request in one pass. Otherwise the lines and requests that came since go in near the end, before rows that started later. Enter on a request opens it in view 1, `y` copies it as cURL, `R` (`[logdawg] requests`) hides and shows them. The requests are all of them, whatever view 1's filter, which applies to view 1 only. With no line to show, the reason stays on the first row above the requests. The box's title and its "N of M" count lines only.
- **The graph's range** (`v`). The range view 1 uses holds in view 4 too: the lines in it and the requests that overlap it; the count says "in the range", and `Esc` clears it.
- **Session files** (PROTOCOL.md §10, type 20). An export from the UI writes the log after the captured stream: what the reader knew (the device, the app's package and uid, process names) and the lines in batches of 2,000, all of them, or with some requests the lines from the first one's start to the last one's end; a finished recording does the same when there is a log. A reader rebuilds the log from them, skipping lines that are not lines. Older versions skip the frames, so the format stays 1. `record` and `tail` still do not read the log.

## 6. Performance budgets

| Where | Budget | How |
|---|---|---|
| App thread, per request (excluding body copies) | < 1 ms added; target < 150 µs typical | Event objects are small; JSON encoding, ring-buffer accounting and socket writes happen on the writer thread. The stack capture (`Throwable.getStackTrace()`) is the largest cost and is bounded by a depth cap (default 64 frames). Measured in Phase 1, after moving frame resolution (`getStackTrace()`) to the writer thread and waking the writer only when it is idle. JVM (`./gradlew :capture-core:benchmarkOverhead`, Apple M5 Pro): the hooks for one 2 KB GET cost 1.5 µs at the median, 4.5 µs at p99; an OkHttp request to a loopback server gains 7 µs at the median. Devices (the sample app's `--es run overhead`: the same 2 KB GET through a plain and a captured client, alternating, 1,000 each): a physical phone (A015, Android 16) +143 to +148 µs at the median and about +200 µs at p90 over three runs (before the two changes: +236 µs); the API 37 and API 26 emulators +25 to +38 µs at the median. Emulator figures vary by a few tens of µs between runs. |
| App thread, body bytes | One `System.arraycopy` per read/write chunk into a pooled `byte[]` | Tee sources/sinks copy what the app already read; no extra reads, no pre-buffering. Over the cap, only a byte counter advances. |
| Device memory | Queue ≤ 8 MiB; ring buffer ≤ 1,000 transactions and 32 MiB of bodies (defaults) | Drop oldest on overflow; evict whole transactions from the ring. |
| Host frame time | ≤ 33 ms at 50,000 transactions (target < 8 ms) | Virtualized rows; cached, incrementally re-sorted row keys; windowed graph buckets; cached body views; heavy work off the UI task. `tests/perf.rs` renders a 50,000-transaction store into `TestBackend` at 200×50 (release build) and fails above 33 ms. Measured in Phase 0b on an Apple M5 Pro: 0.2 ms per frame when nothing changes, 1.1 ms with a new request every frame, 1.7 ms with repeats collapsed, 2.6 ms sorted by name, 0.4 ms for the Thread View. Measured again in Phase 2 (heavy-line graph, boxed layout): the same within 0.2 ms, and 3.2 ms with a filter (`method:GET path:/api/** -status:5xx header:authorization`) and a new request every frame, since filter results are kept per transaction revision (8.2 ms before that). The live loop redraws within 16 ms of input or data and at about 30 frames a second while only the clock moves: the demo draws 34 to 37 frames a second at 0.2 to 0.6 ms each. Measured again with the frame rate setting (5.8), same machine: `tests/perf.rs` gives 0.2 to 2.7 ms a frame at 50,000 transactions, so 240 frames a second (4.2 ms each) fit. The demo at 200×50, in a pseudo-terminal whose reader takes everything at once, draws 30, 60, 120 or 240 frames a second as set (the footer reads 30 to 31, 60 to 61, 119 to 121 and 240 to 241), each in 0.3 to 1.0 ms with its write, for 2%, 4%, 6% and 11% of one CPU core and 35, 42, 51 and 65 KB written a second; before the setting it drew 30 to 36 frames for 2% and 35 KB. With recording paused, so that only the clock moves the view, about 8 frames a second change the picture and only those are written; rendering the rest takes 2.7% of a core at 60 and 9.6% at 240 (1.7% before, at 30 frames a second). An opened file left alone costs no frames, no bytes and no CPU. Keys arriving 200 times a second are drawn 56 times a second at 60, 112 at 120 and 192 at 240. Inside tmux 3.6b the app's numbers are the same, and tmux passes every frame on to its terminal (254 writes a second at 240) for 1% to 2% of a core of its own. Not measured: how many of these frames a terminal emulator puts on the screen. |
| Host ingest | 20,000 events/s sustained; a 50,000-event replay applied in < 1 s | Batched ingest, bounded per loop turn. Measured from protocol bytes to the store (decode, normalize, apply) by `crates/core/tests/ingest.rs` (release build, in CI since 2026-10-07): the replay in 30 ms, and 1.7 million events a second sustained into a store already holding 10,000 requests (Apple M5 Pro). |
| Host memory | ~2 KB per transaction of metadata + bodies within the in-memory budget (default 256 MiB) | Spill store on disk for the rest. |
| Device log (Logdawg) | A frame ≤ 33 ms with 500,000 lines; memory within `[logdawg] keep` (default 64 MiB) | Lines in shared chunks, each tested once as it arrives, the screen's rows only (5.17). `tests/perf.rs` (release build, in CI with the others): 0.25 ms a frame with 20 new lines, 3 to 24 ms to test every line after a filter changes, 103 bytes a line (Apple M5 Pro). |

## 7. Security and privacy

- Capture runs only in debuggable apps. Library mode checks `ApplicationInfo.FLAG_DEBUGGABLE` before starting. In attach mode the platform enforces it at every step on user builds (`run-as`, `attach-agent`, ART's JDWP check, full JVMTI for retransformation, `startup_agents`), and the host additionally refuses packages whose debuggable flag is off, including on userdebug devices where `attach-agent` alone would allow it. `profileable` apps are not debuggable and are not attachable.
- The device socket accepts only peers whose UID is 0 (root) or 2000 (shell), checked with `LocalSocket.getPeerCredentials()` before any byte is sent. Other apps on the device cannot read traffic.
- The host opens no network connections other than to the adb server. No telemetry, no update checks.
- Nothing a device sends is trusted by the host's arithmetic: a message with a device time past 2^62 ns (146 years of uptime) is refused, and body offsets and byte counts that would overflow are dropped or saturate (found by fuzzing, 8; until 2026-10-07 they panicked in debug builds and wrapped in release ones).
- Nothing is redacted (5.10): captured values, secrets included, appear in the UI and in exports as they were sent. Logdawg reads the device's whole log, as adb's shell user can and as Studio's Logcat does: every app's lines reach the computer, and `package:mine` is a filter on the computer, not a limit on what is read (5.17). Spill files live in a private temp directory (mode 0700 on Unix) and are deleted on exit.
- The release no-op artifact contains no capture code at all, so a misconfigured release build cannot capture.
- Rules can change what the app sees; the header always shows how many rules are active, and rule-modified rows are marked.

## 8. Testing and CI

**Rust**

- Unit tests: frame codec and message decoding; adb reply parsing against transcripts; `/proc/net/unix` parsing; filter parser and evaluator; Content-Encoding decoders; protobuf raw decoder; cURL escaping (round-tripped through `sh -c` in a test on Unix); rules TOML parsing and validation; the rule form's reading, checks, saving and matching; session file round-trip; every example in the CLI's help parses.
- HAR output is validated against a vendored HAR 1.2 JSON schema (`testdata/har-schema`: har-schema 2.0.0, ISC) by a strict validator in the test (a keyword it does not know fails the test, so nothing in the schema is skipped), plus the rules of the HAR 1.2 specification the schema leaves out: timings never negative except -1, `time` the sum of the timings, `ssl` inside `connect`, query strings that match the URL (`crates/backends/tests/har_schema.rs`, over the demo's traffic: redirects, failures, gzip, binary and form bodies, a rule's rewrite). Built 2026-10-07; it found the timings that did not add up (5.10).
- UI snapshots: Ratatui `TestBackend` plus insta, at 100×30, 140×40 and 200×50, for every view, tab, dialog and state (LIVE, PAUSED, FROZEN, DETACHED, REPLAY), driven by the deterministic demo generator (fixed seed, virtual clock).
- Property and fuzz tests: arbitrary byte streams into the frame decoder never panic and never allocate more than the frame limit; arbitrary filter strings never panic; arbitrary bytes into every body viewer never panic (proptest). Fuzz targets (`host/fuzz`, cargo-fuzz on nightly, since 2026-10-07): `frame_decoder` (bytes in pieces of any size), `device_stream` (a device's stream after its `hello`, decoded, normalized, applied to a store, read back by the exports), `session_file` and `har_import` (files someone sent you), and `bodies` (any body under any Content-Type and Content-Encoding through every viewer's parser). CI runs each for a minute. In their first minutes they found three overflows in the store's arithmetic on device values (5.6, 7); the inputs are kept in `testdata/fuzz/` and run on every build by `crates/core/tests/fuzz_regressions.rs`.
- Integration (since 2026-10-07): `crates/fakeadb` is a stand-in adb server with fake devices (shell commands answered from a model of each device, forwards to fake abstract sockets, `track-devices`, a server that stops and starts with transport ids from 1 again) and fake capture runtimes (`hello`, the handshake, replay after `resume_after_seq`, live events, pings, a process that dies). `crates/backends/tests/fake_device.rs` runs the device backend against it: connecting and streaming (and the forward removed at the end), a protocol mismatch (both versions in the message), `--follow` across a restart, detaching without it, resuming the same process after the adb server restarts (nothing twice), a device that hides its socket list, forwards left by a killed traffic-police (removed; Android Studio's kept), and waiting for a device and an app. `crates/backends/tests/fake_logdawg.rs` runs Logdawg's reader against the fake adb's `exec:logcat` (5.17), and `fake_session.rs` the session's switch to another app (5.5).

**Android**

- JVM tests (JUnit 4, MockWebServer from the same OkHttp version) for gzip, chunked, streaming, redirects, errors, timeouts, cancellation, one-shot request bodies, upgrades (a call that switches protocols; WebSocket calls skip network interceptors), composition with an app `EventListener`, and every rule action, including which OkHttp versions retry a simulated failure. Duplex request bodies (HTTP/2 only) are handled but not tested yet. The same suite runs against OkHttp 3.9.0, 3.12.13, 3.14.9, 4.0.0, 4.12.0, 5.0.0 and 5.5.0, each with the Okio it ships, and 3.9.0 also with Okio 3, via Gradle test suites that swap the runtime dependency while the library stays compiled against its fixed targets (4.1).
- Protocol conformance: JVM tests write golden frame files (`testdata/protocol/v1/*.frames` plus an expected-events JSON), and Rust tests decode them; the Rust side writes host-to-device golden frames that the Java tests decode. Regenerating golden files is an explicit Gradle/cargo task, never a side effect of a normal test run.
- Instrumented tests on emulators (`capture/src/androidTest`, since 2026-10-07; the device job runs them on API 26 and 36): the library in a debuggable app starts by itself; its socket refuses this app's own uid (only shell and root may read); OkHttp capture with the caller's thread and stack; Android's own HttpURLConnection (its 404 throws, and the status, headers and error body are still captured); TLS details from the device's Conscrypt; and the app's traffic counters. They connect to the runtime in-process through `TestHost`, which the JVM tests use too (a test fixture of `capture-core`, not published). Their app allows cleartext (`src/androidTest/AndroidManifest.xml`): its servers are on 127.0.0.1, and Android 9 and newer refuse cleartext to an app that targets them unless it allows it (until 2026-10-09 the tests failed there for that, on API 31 and 36). The attach path is covered by the end-to-end test below.
- End to end: `crates/backends/tests/device_e2e.rs` drives the sample app on a device and checks every request the capture promises (methods, URLs, statuses, headers, bodies, timings, threads, stacks, failures), in library mode (both processes, an app restart followed) and in attach mode (an attach while the app starts, an attach to a running app whose client was built before, a restart followed, and `--launch`). (The Phase 0 plan was a script around `tail --json` with an instrumentation; driving the backend from a Rust test checks more.)

**CI (GitHub Actions)**

- Host: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on macOS, Ubuntu and Windows; release artifacts built per OS; frame time and ingest speed (release builds); the fuzz targets, a minute each (every target runs, also after one crashed; until 2026-10-09 the first crash ended the job).
- Advisories (`advisories.yml`, since 2026-10-09): `cargo deny check advisories`, the RustSec database's vulnerable, unsound and unmaintained crates in the host's dependencies, on changes to them and every Monday, since advisories appear without any change here.
- Android: build `capture`, `capture-noop`, `attach-agent` (NDK, three ABIs), `sample-app` and the library's device tests; JVM test matrix across OkHttp versions (with the OkHttp 3.9 API check, 4.1); an emulator job (API 26 and 36: every night, by hand, and since 2026-10-09 on pushes that change the Android side) on Linux with KVM, which runs the device tests and the end-to-end tests in both modes. A failure of the device tests no longer skips the end-to-end tests: the job fails at the end. On API 36 adb lost the emulator in the middle of the attach test in about half the runs from 2026-10-04 to 10-10 (`no device with transport id`; the device came back under a new id, and every later adb call failed). Since 2026-10-10 the emulators get 4 GB of memory, the first suspect, and a failed end-to-end run keeps the device's logs (every logcat buffer, the kernel's log, tombstones, the adb server's log) as the `device-logs-api<N>` artifact, to find the cause if it goes on.
- Release (`release.yml`, on a `v*` tag or by hand): first every job of the host and Android workflows (lint, tests on three systems, fuzzing, the JVM suites, the sample app; until 2026-10-09 a release ran no tests), and on a tag a check that the tag names the version of the host crates and of the Android libraries and that `docs/releases/<tag>.md` exists. Then the agent is built once, then the binary for Linux (built on Ubuntu 22.04; it needs glibc 2.34 or newer), macOS (Apple silicon) and Windows with the agent built in (`TRAFFIC_POLICE_EMBED_AGENT=1`), and `THIRD-PARTY-LICENSES.txt` (`host/scripts/third_party_licenses.py`: the license files of the crates the binary links, and the agent's slicer and JVMTI notices). A tag publishes them as a GitHub release with `LICENSE` (Apache-2.0), SHA-256 checksums, and `docs/releases/<tag>.md` as the notes; then the Android libraries go to the Maven repository on GitHub Pages (the `gh-pages` branch, under `maven/`: `https://git-krishnabisht.github.io/traffic-police/maven`, since 0.4.0), where a version once published is never replaced; then the one-line installers install the release on Linux, macOS and Windows.
- Installers (`install.sh` for macOS and Linux, `install.ps1` for Windows, since 0.4.0): the release's binary for the machine, checked against `SHA256SUMS.txt`, into `~/.local/bin` (`%LOCALAPPDATA%\Programs\traffic-police`) without root, and that folder onto PATH (a marked line in the shell's startup file; the user's PATH on Windows, kept as stored). They do not install adb, because a second adb of another version would fight Android Studio's over the server; they say where traffic-police will find one, in the order it looks (5.4), or how to get it. In a terminal they show each step on its own line, with a spinner while it runs and a progress bar for the download (size, percent, speed); elsewhere the same lines plainly. The binary comes in eight parts at once, each a range request over its own connection (curl processes; runspaces in PowerShell), when the server says it sends parts (`Accept-Ranges: bytes`) and the file has 1 MB or more: at times GitHub's release servers give each connection from a network about 100 KB/s while the same network takes 20 MB/s from elsewhere (on 2026-10-11, 116 KB/s over one connection and 682 KB/s over eight; when one connection is fast, parts cost nothing). Each part's size is checked and the checksum checks the whole; in PowerShell a part that ends early or gets nothing for a minute tries again from where it stopped, three tries in all. When a part fails, or the server answers a range with the whole file, the file comes over one connection instead (PowerShell 7 then draws its own bar, and 5.1, which slows a download down while it draws one, says it is under way). The download's line ends with "over 8 connections" when the parts made it. A server address that does not answer is left after 10 s (on 2026-10-10 one of GitHub's four release-file addresses did not answer from the user's network, and each connection waited out macOS's 75 s, so the install seemed to hang). `install.yml` runs them on Linux (dash), macOS and Windows (PowerShell 5.1 and 7) when they change and after each release.

## 9. Decisions to review, deviations, risks

### 9.1 Where this design departs from the brief (and why)

| # | Brief | This design | Why |
|---|---|---|---|
| 1 | slicer from AOSP `external/dexter` | `platform/tools/dexter` | That is where slicer lives (verified on AOSP Gerrit); same code Studio uses |
| 2 | OkHttp 3.x, 4.x, 5.x | 3.9.0 and later | 3.0–3.8 have no public `EventListener` and no `Chain.call()`; they are detected and left alone with a `diag` |
| 3 | `debugImplementation` auto-starts from a ContentProvider | Auto-start in the main process; secondary processes call `TrafficPolice.start(context)` | Android installs a manifest provider only in the process it is declared for (verified in `ComponentResolverBase.queryProviders`) |
| 4 | "show the 101 upgrade only" for WebSockets | Phase 5: the handshake and every message, both modes. Library mode needs `TrafficPolice.newWebSocket` | OkHttp never runs network interceptors for WebSocket handshakes and builds the WebSocket client with `EventListener.NONE`, so neither hook can see a socket; wrapping the socket sees all of it |
| 5 | Capture from launch via `am start --attach-agent` or `startup_agents` | API 30+ `startup_agents`; 27–29 `--attach-agent` (27–28 best effort); API 26 cannot capture from launch | `--attach-agent` appeared in API 27, `startup_agents` in API 30; `--attach-agent-bind` attaches twice (verified in AMS code) |
| 6 | Traffic graph like Studio | Default: Studio's whole-app `TrafficStats` series (decided in review); `T` toggles to the bytes of the captured requests | Captured bytes line up with the rows, resolve finer than 500 ms, and exist in HAR imports, so they are kept one key away |
| 7 | Rules: fail with IOException or SocketTimeoutException | Also `ProtocolException`, `ConnectException`, `UnknownHostException`, with OkHttp's retry behaviour documented | OkHttp may retry plain `IOException`s thrown by a network interceptor, re-running the rule |
| 8 | (not specified) | Rewritten responses get `Cache-Control: no-store` unless the rule opts out (decided in review) | OkHttp caches what leaves the network interceptors; a fake response must not outlive its rule |
| 9 | Suggested crates: tui-textarea, zstd | ratatui-textarea + tui-input; ruzstd (+ pure-Rust gzip/brotli) | tui-textarea is stuck on Ratatui 0.29; `zstd` needs a C toolchain per target, which works against one self-contained binary per OS. The stack itself (Rust, Ratatui, crossterm, Tokio) is unchanged |
| 10 | Keys | Adds `T` (graph source) | Not assigned in the brief |
| 11 | (not specified) | One client per app process; a new client takes over (decided in review) | Makes reconnecting after a host crash always work; `tail` and the TUI cannot watch the same process at once |
| 12 | Device clock "monotonic" | `SystemClock.elapsedRealtimeNanos()` (CLOCK_BOOTTIME) | Monotonic and shared by all processes, and keeps counting in suspend, so the wall-clock offset stays stable |
| 13 | OkHttp 2 | Not supported | Studio still supports it; the brief lists 3.x–5.x. Android's own HttpURLConnection (built on an internal OkHttp 2 fork) is covered by 4.3 |
| 14 | Suggested crate: syntect | Own tokenizers for JSON and markup | The pretty-printers already produce the token spans; see 5.15 |
| 15 | Redaction on by default, in the UI and every export | No redaction anywhere | Decided in the Phase 0b review (9.2) |
| 16 | (library API level not specified; the test matrix is API 26 and the latest) | The library builds with `minSdk` 21; it is tested on API 26 to 37 | 21 is OkHttp's own floor, so the library never blocks an app that can use OkHttp; below 26 it is untested |
| 17 | Capture a running app at any time | A process frozen by Android's cached-apps freezer (Android 11+) cannot be captured until it runs again; traffic-police waits and says so (5.5) | Platform behaviour: frozen processes run no code, and a frozen app makes no requests either |

### 9.2 Review decisions and open questions

Decided in the Phase 0 review:

- **Name:** the project is **traffic-police**. Binary `traffic-police`; Rust crates `traffic-police-*`; Java packages `io.trafficpolice.*` with the public class `TrafficPolice`; Maven coordinates `io.trafficpolice:capture` and `io.trafficpolice:capture-noop`; device socket `traffic-police_<package>_<pid>`; project directory `.traffic-police/`; session files `*.trafficpolice`.
- **One client per app process;** a new client takes over.
- **Git:** Phase 0 merged into `master`.
- **Graph default:** all of the app's network traffic (whole-app `TrafficStats`, like Android Studio); `T` switches to captured requests.
- **Rewritten responses** get `Cache-Control: no-store` by default, so a changed response never stays in the app's HTTP cache after its rule is turned off.

Decided in the Phase 0b review:

- **No redaction.** Nothing is hidden or masked, in the UI or in any export, including `Authorization`, cookies and personal data in bodies. The brief's redaction utility is dropped.
- **Git:** Phase 0b merged into `master`.

Decided in the Phase 1 review:

- **Git:** Phase 1 merged into `master`; Phase 2 is on `phase-2`.
- **Capture overhead** as measured (section 6) is accepted; no further work on it.
- **Traffic graph:** heavy step lines (blue receiving, orange sending) by default; the other styles stay selectable. After v0.1.0, at the user's request ("make it better and smooth"), smooth braille curves became the default (5.8). After v0.2.0, at the user's request on seeing receiving flattened under sending, the mirror layout became the default, and then, still not smooth enough as dots, the solid areas (5.8).
- **Layout:** boxed panels with titles, the focused box highlighted, a key-hint footer, and a colored Status column.
- **Frame rate:** redraw promptly on input and data, not only on a timer (5.8).
- **Phase order kept:** attach mode (watching an app without changing it) stays Phase 4, after rules in Phase 3.

Decided in the Phase 2 review:

- **Git:** Phase 2 merged into `master` and pushed; Phase 3 is on `phase-3`.
- **Body explorer:** the detail pane splits in two, the response body as a tree explored with `h`/`j`/`k`/`l` above the tabs (5.8). No later phase had it, so it is built in Phase 3. After v0.1.0 the box got the request body as a second tab, and at the user's request `h`/`l` switch between the two bodies (as in the tabs below) instead of folding and stepping through the tree; folding is Enter and `[` `]`.

Decided in the Phase 3 review:

- **Rules end with the connection.** When traffic-police quits or loses the app, the app's normal behaviour returns, rather than rules staying in the app until they are changed.
- **Rules are edited in `$EDITOR` for now.** Enter opens `rules.toml` at the rule and saving applies it; Space turns a rule on or off and `r` starts one from a request. A form inside the UI comes in a later phase (5.11.1). The review first answered that the editor was enough; the user then asked to keep it for now and scope the form for the future.
- **Headless commands follow the file** like the UI: `tail`, `record` and `export --live` send saved changes to the app while they run (built after the review).
- **Git:** Phase 3 merged into `master` and pushed.

Decided after the Phase 3 review:

- **Frame rate setting.** The question was whether the UI can draw 100 frames a second or more, or as many as the computer manages. `[ui] fps` sets the most frames a second: 60 unless set, up to 240 (5.8). Drawing without a limit was turned down: a terminal program cannot learn the screen's refresh rate, and frames the screen never shows only cost CPU.

Decided in the review of the frame rate setting:

- **The most is 240 frames a second.** Tried at 1000: the demo draws about 600 a second for a quarter of a CPU core, since Tokio's timer counts whole milliseconds (at 500: about 496, for a fifth), and a screen shows no more frames than its refresh rate, usually 60 or 120.
- **A quiet live view is drawn at the full rate** while only the clock moves it (the graph sliding, bars growing), for smooth movement: 2.7% of a core at 60 frames a second, where the 30 frames a second before took 1.7% (section 6).
- **Git:** committed on `ui-fps`, which starts from `master`; the paused attach-mode commits stay on `phase-4`.

Phase 4 decisions, merged without objection (2026-10-01; 4.7.6 has the details):

- **`--launch` in attach mode restarts a running app,** since only a new process can be captured from its start. Library mode's `--launch` still only starts it.
- **`--follow` on Android 11+ keeps a startup agent in the app while traffic-police runs,** so each restart is captured from its first request, other processes of the app included. It is removed at the end; if traffic-police is killed, it does nothing after 5 minutes.
- **The picker attaches to a process without the library when Enter is pressed on it,** without `--mode attach` (`--mode library` turns this off). It also hides apps that `run-as` refuses, which userdebug emulator images otherwise list by the dozen.
- **Commands without the UI exit with 1 when the capture fails.**

Decided while finishing what the plan left (2026-10-07), for review:

- **Enter in the Rules view opens the rule form; `E` opens `$EDITOR`.** Until then Enter opened the editor. `r` opens the form too (filled in from the selected request, or empty in the Rules view), and nothing is written until it is saved; a new rule starts on, with no actions (until then `r` wrote the rule into the file at once, off, with a placeholder `status` action).
- **New keys:** `K` and `J` move a rule (or the form's action), `a` adds, Delete or Backspace removes, Ctrl+S saves, `E` opens the file. All of them are in `[keymap]`.
- **The keymap reaches menus, pickers and the palette,** and a menu entry's letter wins over the keys that move and close.
- **`package` in `project.toml` skips the picker** when no `--package` is given.
- **HAR timings add up to the entry's time,** the gaps between phases counted as `blocked` and a request that never got an answer counted as `wait`.
- **The host refuses device times past 2^62 ns** and drops body chunks whose end would overflow.

Decided while building Phase 5 (2026-10-07), for review:

- **WebSockets in library mode need `TrafficPolice.newWebSocket(client, request, listener)`** in place of `client.newWebSocket(...)`: no interceptor or listener sees a socket (§4.2). Attach mode needs nothing.
- **gRPC: `TrafficPolice.grpcInterceptor()`, added to the channel first.** Attach mode hooks the channel builders (channels built after the attach) and the stubs' `getChannel` (calls through generated stubs on older channels). Calls made with `channel.newCall` on a channel built before the attach are missed; `--launch` avoids that (§4.9).
- **gRPC requests show what the transport adds** below every interceptor: `content-type: application/grpc`, `te: trailers`, and `grpc-timeout` from the deadline. Their HTTP status is shown as 200, since gRPC never exposes it.
- **The Status column shows a gRPC call's status name** (`OK`, `NOT_FOUND`) in place of the HTTP 200, colored as an API gateway maps the code to HTTP; `grpc:` filters by it.
- **Flutter mode** turns dart:io's HTTP logging on in every isolate, and off again on quit where it was off. It never clears the app's profile, which DevTools may share. Pause turns logging off, and there are no rules (§5.16).
- **The attach agent is version 0.2.0:** its boot interface gained the gRPC class loader. An app process that already has the 0.1.0 agent keeps it, and has to restart to get 0.2.0.
- **Protocol additions, protocol version unchanged (1):** the `ws` event, `trailers` and `grpc` on `done` and `fail`, and `decoded` on `body_end`. An older host ignores them.

Decided at the user's request (2026-10-08):

- **Only `:q` quits** (with Enter, as in Neovim; `:q!`, `:qa`, `:wq`, `:x` too), in the UI and the pickers. `q` and Ctrl+C no longer quit, and say how. Commands without the UI (`tail`, `record`, `export`) still stop on Ctrl+C.

### 9.3 Risks

- **Slicer aborts on unexpected input**, which would crash the app. Mitigation: pinned revision, targets validated before instrumentation, only reference-returning methods, tests on every supported API level.
- **Minified debuggable builds.** If R8 renamed or inlined OkHttp's getters, attach-mode hooks install nowhere or never fire; hook status and hit counters make this visible, and library mode is the fallback.
- **Shaded or duplicate OkHttp copies** (an SDK that repackages OkHttp, or a second class loader) are not captured in v1.
- **HttpURLConnection wrappers** must delegate every method of `HttpsURLConnection`; a missed method changes app behaviour. Studio's wrapper is the reference, and the JVM tests call every method through the wrapper.
- **ART is an updatable module from API 31**, so JVMTI behaviour tracks the module version, not only the OS version; the emulator matrix includes updated images.
- **Performance targets:** the host side was measured in Phase 0b and the device side in Phase 1 (section 6).
- **Terminal variance.** Graphics protocols, OSC 52 and mouse support differ across terminals, tmux and SSH; everything degrades to half-blocks, copy-to-file and keyboard.
- **Frame rate on Windows** is not measured. Windows wakes timers about every 15.6 ms unless a program asks for finer timing, so `[ui] fps` above about 64 may not be reached there.
- **Coexistence with Android Studio.** If Studio's inspector is attached too, both interceptors run. The runtime samples the stack inside the chain on the first eight calls of a process and every 256th after (Studio can attach later): in our interceptor (Studio's runs first) and in the listener's `requestHeadersStart`, which OkHttp calls below every network interceptor (Studio's runs after ours). A frame of `com.android.tools.appinspection.network.` (or the older profiler's `com.android.tools.profiler.support.network.`) sends `studio_inspector_present` once, which the UI's footer shows. Built 2026-10-07 (`StudioDetector`, tested with a stand-in interceptor of Studio's class name on OkHttp 3.9, 4.12 and 5.5).

## 10. Phase map

| Phase | Deliverable | Done when |
|---|---|---|
| 0a | ARCHITECTURE.md, PROTOCOL.md | Reviewed |
| 0b | Complete TUI on the demo backend | `traffic-police demo` shows every view and tab; snapshot tests pass |
| 1 | Library mode end to end | Sample app traffic live on a physical device and on API 26 and latest emulators, with correct headers, bodies, timings, thread and stack; kill and relaunch behave as specified |
| 2 | Utilities | Everything in 5.10 and 5.12 |
| 3 | Rules | MockWebServer tests for every action, gzip included |
| 4 | Attach mode | An unmodified debuggable app shows traffic, including clients created before attach; `--launch` captures start-up requests; release binaries embed the agent and dex |
| Fixes | Everything the plan left (2026-10-07) | The rule form (5.11.1) and rule reordering; the socket-name fallback (5.4); the nightly attach failure (4.7.2); the bugs of 0.3.1; the checks, tests and measurements the design promised (4.1, 8, 6); API 27–29 tried on devices |
| 5 | Optional | WebSocket frames (§4.2) and gRPC (§4.9) in both modes, built and verified on devices 2026-10-07; Flutter (Dart VM service) backend (§5.16) |
| 0.5.0 | Logdawg (§5.17), at the user's request | The device's log in view 4 with Studio's filter language, live on emulators (API 26, 31, 37); then its pause, everything the device still has at start, `4` focusing the log, and another app without quitting (§5.5), live on an API 31 emulator. then a request's lines in its detail pane (with the runtime sending each thread's kernel id), requests among the lines, the graph's range in view 4, and the log in session files |

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
| Synchronized output (terminal mode 2026) | The specification (gist `christianparpart/d8a62cc1ab659194337d73e399004036`, read 2026-09-30); crossterm 0.29.0 and ratatui-crossterm 0.1.2 source; tmux 3.6b in a private server: no answer to the mode query (DECRQM), the two sequences taken without reaching the pane or the client |
| Platform docs | developer.android.com (16 KB page sizes, Android 14 behaviour changes; Logcat's query syntax, read 2026-10-10), source.android.com (ART TI), W3C HAR 1.2 |
| Logcat (5.17) | Studio's filter language: `JetBrains/android` `master` (`LogcatFilter.flex`, `LogcatFilter.bnf`, `LogcatFilterParser.kt`, `LogcatFilter.kt`, `LogcatMessageWrapper.kt`), read 2026-10-10; logcat's binary records: read from API 26, 31 and 37 emulators (`testdata/logcat/`) |
