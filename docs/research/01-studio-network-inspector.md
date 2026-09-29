# 01 — Android Studio Network Inspector: verified source study (for `netinspect`)

Rule followed: **verify, do not recall**. Every factual statement below cites code or docs read during this task.
Statements tagged **(analysis)** are conclusions I drew by combining cited code paths; they were not executed on a device.
Items I could not verify are tagged **UNVERIFIED** and listed at the end.

## Source legend (citation aliases)

| Alias | Meaning |
|---|---|
| `TB` | AOSP `platform/tools/base` studio-main snapshot @`0227ef52` (local sparse clone of `kroune/platform-tools-base@11ff8856`). I diffed 7 key files (NetworkInspector.kt, OkHttp3Interceptor.kt, OkHttpUtils.kt, TrackedHttpURLConnection.kt, StreamReporter.kt, InterceptionTransformation.kt, network-inspector.proto) against Gerrit `mirror-goog-studio-main`@`873a1f53`: all identical. |
| `NI` | `TB:app-inspection/inspectors/network/src/com/android/tools/appinspection/network` |
| `NIT` | `TB:app-inspection/inspectors/network/testSrc/com/android/tools/appinspection/network` |
| `PROTO` | `TB:app-inspection/inspectors/network/resources/proto/network-inspector.proto` |
| `AGENT` | `TB:app-inspection/agent/src/main/java/com/android/tools/agent/app/inspection` |
| `NATIVE` | `TB:app-inspection/native` |
| `LEGACY` | `platform/tools/base@mirror-goog-studio-master-dev` (rev `6bbb1618`), read via Gerrit REST |
| `LP` | `LEGACY:profiler/app/common/src/main/java/com/android/tools/profiler/support/network` |
| `IDE` | `JetBrains/android@master` (HEAD `0867bfe2`, fetched 2026-09-29) `app-inspection/inspectors/network/...` |
| `NM` | `IDE:.../model/src/com/android/tools/idea/appinspection/inspectors/network/model` |
| `OK2.7.5`, `OK3.12`, `OK3.14`, `OK4.12`, `OK5.4`, `OK5.5` | `square/okhttp@parent-2.7.5 / parent-3.12.13 / parent-3.14.9 / parent-4.12.0 / parent-5.4.0 / parent-5.5.0` |
| `JAR(x)` | Maven Central `okhttp-3.12.13.jar`, `okhttp-4.12.0.jar`, `okhttp-jvm-5.5.0.jar`, disassembled with `javap -c -p` |
| `LIBCORE` | `platform/libcore@main` (`de876a01`) via Gerrit REST |
| `EXTOK` | `platform/external/okhttp@main` (`05b3c271`) via Gerrit REST |
| `CONN` | `platform/packages/modules/Connectivity@main` (`2519a787`), `framework-t/src/android/net/TrafficStats.java` |
| `AX` | `androidx/androidx` HEAD, `inspection/inspection/src/main/java/androidx/inspection/ArtTooling.java` |
| `KT` | `JetBrains/kotlin@master` (`1ff31462`), `libraries/stdlib/...` |

Notes on access: the legacy `profiler/app/perfa-okhttp` directory does **not** exist in the current studio-main snapshot (only `perfa/` with one `ProfilerService.java`); I found the legacy network profiler code on Gerrit branch `mirror-goog-studio-master-dev`. Gerrit change queries for these files return no public changes, so there is no public review history to cite (see UNVERIFIED).

---

## 1. Structure

### 1.1 Device-side source files (29 Kotlin files under `NI/`) — one line each

| File | Purpose (verified by reading it) |
|---|---|
| `NetworkInspectorFactory.kt` | `InspectorFactory` with id `"studio.network.inspection"` (`:23`), creates `NetworkInspector`. |
| `NetworkInspector.kt` | The inspector: handles `Command`s, starts speed sampling, registers all ART hooks, instruments pre-existing gRPC channels, applies rule commands. |
| `HttpTrackerFactory.kt` | `fun interface HttpTrackerFactory { fun trackConnection(url, callstack): HttpConnectionTracker }` + impl that builds a `ConnectionTracker` with a new `ConnectionReporter` (`:23-40`). |
| `TrafficStatsProvider.kt` | Interface `getUidRxBytes/getUidTxBytes` (for testability). |
| `TrafficStatsProviderImpl.kt` | Calls `android.net.TrafficStats.getUidRxBytes/getUidTxBytes` (`:24-26`). |
| `okhttp/OkHttp3Interceptor.kt` | OkHttp 3/4/5 network `Interceptor`: tracks request, applies rules to the response, tees the response body. |
| `okhttp/OkHttp2Interceptor.kt` | Same for `com.squareup.okhttp` (OkHttp 2). |
| `okhttp/OkHttpUtils.kt` | Call-stack extraction after the OkHttp frames, null `OutputStream` for request-body capture, "ignore our own requests" check, rate-limited error logging. |
| `httpurl/HttpURLTransformer.kt` | `wrapURLConnection()`: exit-hook body for `URL.openConnection()`; wraps `HttpsURLConnection`/`HttpURLConnection`, returns others unchanged. |
| `httpurl/TrackedHttpURLConnection.kt` | Delegate holding all tracking state/logic for the two wrapper classes. |
| `httpurl/HttpURLConnectionWrapper.kt` | `HttpURLConnection` subclass that forwards every method to `TrackedHttpURLConnection`. |
| `httpurl/HttpsURLConnectionWrapper.kt` | `HttpsURLConnection` subclass; forwards to `TrackedHttpURLConnection`, TLS getters/setters go straight to the wrapped connection. |
| `trackers/HttpConnectionTracker.kt` | Public tracking interface (`trackRequest`, `trackRequestBody`, `trackResponseHeaders`, `trackResponseBody`, `trackResponseInterception`, `error`, `disconnect`). |
| `trackers/ConnectionTracker.kt` | Concrete tracker: forwards to `ConnectionReporter`, wraps streams in `InputStreamTracker`/`OutputStreamTracker`. |
| `trackers/InputStreamTracker.kt` | `FilterInputStream` tee for response bodies. |
| `trackers/OutputStreamTracker.kt` | `OutputStream` tee for request bodies. |
| `trackers/GrpcTracker.kt` | Builds and sends `GrpcEvent`s (plus thread events) for one gRPC call. |
| `grpc/GrpcInterceptor.kt` | gRPC `ClientInterceptor` + `ClientStreamTracer` + forwarding listener that drive `GrpcTracker`. |
| `reporters/ConnectionReporter.kt` | Creates the connection id and emits `RequestStarted`/`ResponseStarted`/`ResponseIntercepted`/`Closed(false)`. |
| `reporters/StreamReporter.kt` | Buffers a body (≤10 MiB) and on close emits `RequestPayload`+`RequestCompleted` or `ResponsePayload`+`ResponseCompleted`+`Closed(true)`. |
| `reporters/ThreadReporter.kt` | Emits `ThreadData` when the current thread differs from the last one reported for the connection. |
| `rules/InterceptionRule.kt` | `InterceptionRuleImpl` = criteria + ordered transformations built from proto. |
| `rules/InterceptionCriteria.kt` | Method/protocol/host/port/path/query matching. |
| `rules/InterceptionTransformation.kt` | Status-code, header add/replace, body replace, body find-and-replace transformations. |
| `rules/InterceptionRuleService.kt` | Data classes (`NetworkConnection`, `NetworkResponse`, `NetworkInterceptionMetrics`) and the ordered, synchronized rule registry. |
| `rules/InterceptionRuleUtil.kt` | `MatchingText.matches`, wildcard→regex, gzip helpers, header-name constants. |
| `utils/IdGenerators.kt` | Process-wide `AtomicLong` connection-id generator. |
| `utils/InspectorProtocol.kt` | `Connection.sendHttpConnectionEvent()` wraps a builder in an `Event` stamped with `System.nanoTime()`. |
| `utils/Logger.kt` | `Log` wrapper with visible tag `"Network Inspector"` and hidden tag `"studio.inspectors"` (`:21-24`). |

Also used from `TB:app-inspection/inspectors/common/src/com/android/tools/appinspection/common/`: `Stacktrace.kt` (`getStackTrace(offset, packagePrefix)`), `ThreadLocalDelegate.kt` (`threadLocal {}` delegate), `Logs.kt` (`logError`, de-duplicated to once per 10 s per message: `LOG_BUFFER_NS = TimeUnit.SECONDS.toNanos(10)`, `Logs.kt:29,33-41`).

Non-source files: `resources/proto/network-inspector.proto` (wire schema), `resources/META-INF/services/androidx.inspection.InspectorFactory` (ServiceLoader entry: `com.android.tools.appinspection.network.NetworkInspectorFactory`), `BUILD`, `testProto/test-server.proto` (gRPC Greeter service for tests), `lint_baseline.xml` (only test `TODO()` StopShip entries).

Build facts (`TB:app-inspection/inspectors/network/BUILD`):
- Compiles against OkHttp 2 and OkHttp 3 and gRPC API: deps `"@maven//:com.squareup.okhttp.okhttp"`, `"@maven//:com.squareup.okhttp3.okhttp"`, `"@maven//:io.grpc.grpc-api"` (`:25-28`). Versions: `"com.squareup.okhttp:okhttp:2.5.0"`, `"com.squareup.okhttp3:okhttp:4.12.0"`, `"io.grpc:grpc-api:1.69.1"` (`TB:bazel/maven/artifacts.bzl:104,106,118`).
- OkHttp/gRPC are **not bundled**: `bundle_srcs` lists only the common module, kotlin-stdlib and coroutines (`BUILD:44-48`); they resolve from the app (see §2.5 classloader).
- `"--min-api 26",  # Network inspector is only supported on O+ devices.` (`BUILD:50`).

### 1.2 Wire schema: `PROTO` (package `studio.network.inspection`)

Envelope: `message Event { int64 timestamp = 1; oneof union { HttpConnectionEvent http_connection_event = 11; SpeedEvent speed_event = 12; GrpcEvent grpc_event = 13; } }` with `// Timestamp of the event in nanoseconds.` (`PROTO:6-14`).

**HTTP** — `HttpConnectionEvent` (`PROTO:17-82`), keyed by `int64 connection_id = 1` (`:69`):
- `enum HttpTransport { UNDEFINED; JAVA_NET; OKHTTP2; OKHTTP3; }` (`:18-23`) — no value for OkHttp 4/5; they report `OKHTTP3`.
- `Header { string key = 1; repeated string values = 2; }` (`:25-28`).
- `RequestStarted { url, trace, headers, method, transport }` (`:30-36`) — `trace` is the call stack as one string.
- `RequestCompleted {}` (`:38-39`), `ResponseStarted { headers, int32 response_code }` (`:41-44`).
- `ResponseIntercepted { bool status_code, header_added, header_replaced, body_replaced, body_modified }` (`:46-52`) — flags only, no original values.
- `ResponseCompleted {}` (`:54-55`); `Closed { bool completed = 1; }` with `// Sent when an http connection is closed or an error occurred.` (`:57-61`) — **no error message field**.
- `Payload { bytes payload = 1; }` with `// A connection can have up to two payloads, one for request and one for response.` (`:63-67`).
- Oneof tags: `http_request_started = 11 … http_closed = 16; request_payload = 17; response_payload = 18; ThreadData http_thread = 19;` (`:71-81`).

**Threads**: `ThreadData { int64 thread_id = 1; string thread_name = 2; }` with `// ID of the thread obtained from Java, which is different from the thread ID obtained in a JNI context.` and `// Name of the thread as obtained by Thread#getName()` (`PROTO:139-145`).

**Speed/traffic**: `SpeedEvent { int64 tx_speed = 1; // transmission speed in bytes / s  int64 rx_speed = 2; // receive speed in bytes / s }` (`PROTO:147-151`).

**gRPC** — `GrpcEvent` (`PROTO:85-137`): `GrpcCallStarted {service, method, request_headers, trace}`, `GrpcPayload { optional bytes bytes; string type; string text; }`, `GrpcMessageSent/Received {payload}`, `GrpcStreamCreated {address, request_headers}`, `GrpcResponseHeaders`, `GrpcCallEnded { string status; optional string error; repeated GrpcMetadata trailers; }`, `ThreadData grpc_thread = 8`.

**Commands/responses**: `Command { StartInspectionCommand start_inspection_command = 1; InterceptCommand intercept_command = 2; }` with the comment that start "has the side effect of causing the inspector to apply bytecode transformation in order to add hooks into Http code" (`PROTO:153-161`). `StartInspectionResponse { optional int64 timestamp // …baseline for the clock the app is running on…; optional bool speedCollectionStarted, javaNetHooksRegistered, okhttpHooksRegistered, grpcHooksRegistered, alreadyStarted; }` (`PROTO:289-303`).

**Interception rules** (`PROTO:166-279`):
- `InterceptCommand { oneof { InterceptRuleAdded; InterceptRuleUpdated; InterceptRuleRemoved; ReorderInterceptRules; } }`; `InterceptRuleAdded { int32 rule_id; InterceptRule rule; }` "adds a new rule at the end of an ordered processing sequence" (`:179-183`); `ReorderInterceptRules { repeated int32 rule_id; }` (`:175-177`).
- `InterceptRule { bool enabled; InterceptCriteria criteria; repeated Transformation transformation; }` — "matches the request with its url criteria before applying its transformations to the response" (`:194-200`).
- `MatchingText { enum Type { UNDEFINED; PLAIN; REGEX; } Type type; string text; }` — "matches the text with its plain content, wild cards or regex. Empty text matches all." (`:202-212`).
- `InterceptCriteria { Protocol protocol (UNSPECIFIED/HTTPS/HTTP); string host; string port; string path; string query; Method method (UNSPECIFIED, GET, POST, HEAD, PUT, DELETE, TRACE, CONNECT, PATCH, OPTIONS); }` (`:214-238`).
- `Transformation` oneof: `StatusCodeReplaced { MatchingText target_code; string new_code; }`, `HeaderAdded { name; value; }`, `HeaderReplaced { MatchingText target_name; MatchingText target_value; optional string new_name; optional string new_value; }`, `BodyReplaced { bytes body; }`, `BodyModified { MatchingText target_text; string new_text; }` (`:240-279`).

Transport framing (App Inspection, not the network proto): `ConnectionImpl.sendEvent` sends raw bytes if `data.length <= mChunkSize`, else `NativeTransport.sendPayload(data, data.length, mChunkSize)` + `sendRawEventPayload` (`AGENT/ConnectionImpl.java:33-40`), with `CHUNK_SIZE = 4 * 1000 * 1000` (`AGENT/InspectorContext.java:57`).

---

## 2. Hook registration

### 2.1 Every `registerEntryHook` / `registerExitHook` / `findInstances` call (exhaustive grep of `NI/`)

| # | Call | Target | Method + JNI sig | What it does |
|---|---|---|---|---|
| 1 | `findInstances` (`NI/NetworkInspector.kt:171`) | `android.app.Application` | — | `artTooling.findInstances(Application::class.java).firstNotNullOfOrNull { runCatching { it.applicationInfo?.uid }.getOrNull() }` → uid for TrafficStats; comment: "The app can have multiple Application instances… we use the first non-null uid" (`:169-170`). |
| 2 | `registerExitHook` (`:229-233`) | `java.net.URL` | `"openConnection()Ljava/net/URLConnection;"` | `urlConnection -> wrapURLConnection(urlConnection, trackerService, interceptionService)` |
| 3 | `registerExitHook` (`:252-262`) | `com.squareup.okhttp.OkHttpClient` | `"networkInterceptors()Ljava/util/List;"` | mutates the returned list in place: `if (list.none { it is OkHttp2Interceptor }) { okHttp2Interceptors = list; list.add(0, OkHttp2Interceptor(...)) }` |
| 4 | `registerExitHook` (`:270-279`) | `okhttp3.OkHttpClient` | `"networkInterceptors()Ljava/util/List;"` | returns a **new** list: `interceptors.add(OkHttp3Interceptor(...)); interceptors.addAll(list)` |
| 5 | `findInstances` (`:316`) | `io.grpc.ManagedChannel` | — | for instances whose class is `io.grpc.internal.ManagedChannelImpl`, reflectively replaces private field `interceptorChannel` with `InterceptingGrpcChannel(channel, grpcInterceptor)` (`:317-323`, constants `:61-62`). |
| 6 | `registerEntryHook` (`:342`) | 6 gRPC builder statics (below) | see below | `{ _, _ -> hookDepth++ }` |
| 7 | `registerExitHook` (`:343-353`) | same | same | `hookDepth--; if (hookDepth == 0) channelBuilder.intercept(grpcInterceptor)` |

gRPC hook list (`NI/NetworkInspector.kt:77-85`): `io.grpc.ManagedChannelBuilder` `forAddress(Ljava/lang/String;I)Lio/grpc/ManagedChannelBuilder;` and `forTarget(Ljava/lang/String;)Lio/grpc/ManagedChannelBuilder;`; `io.grpc.android.AndroidChannelBuilder` `forAddress(...)Lio/grpc/android/AndroidChannelBuilder;` / `forTarget(...)`; `io.grpc.okhttp.OkHttpChannelBuilder` `forAddress(...)Lio/grpc/okhttp/OkHttpChannelBuilder;` / `forTarget(...)`. Classes are loaded with `javaClass.classLoader.loadClass(hook.className)`; missing ones are skipped (`:335-341`).

Not hooked (verified by grep of `NI/`: no `EventListener`, WebSocket, Cronet, Volley or Ktor references):
- **`eventListenerFactory()` is not hooked**; nothing in the inspector uses OkHttp `EventListener`.
- **Only the no-arg `URL.openConnection()`** is hooked. `URL.openConnection(Proxy)` does not delegate to it — `return handler.openConnection(this, p);` (`LIBCORE:ojluni/src/main/java/java/net/URL.java:1038-1057`) — so proxied connections are invisible **(analysis)**. `openStream()`/`getContent()` do go through it: `return openConnection().getInputStream();` (`URL.java:1071-1073`, `:1085-1087`), so they are covered because the hook lives inside `openConnection()`'s body.
- `OkHttpClient` instances are never searched with `findInstances`.

Failure handling: each OkHttp hook is wrapped in `catch (e: NoClassDefFoundError) { // Ignore. App may not depend on OkHttp. }` (`:265-266`, `:282-283`); if neither registered: `"Did not instrument OkHttpClient. App does not use OKHttp or class is omitted by app reduce"` (`:285-288`). The three booleans go back in `StartInspectionResponse` (`:118-133`). A second start returns `alreadyStarted = true` (`:107-117`).

### 2.2 How they avoid adding the interceptor more than once

- **OkHttp 3+**: they never mutate the client's list. The hook returns a fresh `ArrayList` with one new `OkHttp3Interceptor` in front (`NI/NetworkInspector.kt:273-277`), so repeated getter calls don't accumulate. The hook does **not** check whether the incoming list already has an `OkHttp3Interceptor`.
- The remaining protection is `shouldIgnoreRequest(callstack, this.javaClass.name)` = `callstack.contains(className)` (`NI/okhttp/OkHttpUtils.kt:65`, called at `NI/okhttp/OkHttp3Interceptor.kt:69-70`, comment `// Do not track request if it was from this package`). If a second Studio interceptor sits deeper in the same chain, its computed stack starts at the outer `OkHttp3Interceptor.intercept` frame, so it ignores the request **(analysis)**.
- Why duplicates happen on OkHttp 4/5 **(verified in bytecode)**: `OkHttpClient.Builder(OkHttpClient)` copies through the **hooked getters**:
  - `JAR(okhttp-4.12.0)`: `public okhttp3.OkHttpClient$Builder(okhttp3.OkHttpClient);` … `invokevirtual … okhttp3/OkHttpClient.networkInterceptors:()Ljava/util/List;` and `invokevirtual … okhttp3/OkHttpClient.eventListenerFactory:()Lokhttp3/EventListener$Factory;`. `JAR(okhttp-jvm-5.5.0)` is the same.
  - Source: `this.networkInterceptors += okHttpClient.networkInterceptors` / `this.eventListenerFactory = okHttpClient.eventListenerFactory` (`OK4.12:okhttp/src/main/kotlin/okhttp3/OkHttpClient.kt:505-506`; `OK5.5:.../OkHttpClient.kt:629-630`).
  - OkHttp 3.12 reads the fields directly: `getfield … okhttp3/OkHttpClient.networkInterceptors` (`JAR(okhttp-3.12.13)`); source `this.networkInterceptors.addAll(okHttpClient.networkInterceptors);` (`OK3.12:okhttp/src/main/java/okhttp3/OkHttpClient.java:506`).
  - Consequence **(analysis)**: on OkHttp 4/5 every client made with `newBuilder()` during inspection permanently contains a Studio interceptor. It survives `onDispose`, which only cleans the OkHttp 2 list (`NI/NetworkInspector.kt:358-361`).
- **OkHttp 2**: the list is live and mutable, so they check `list.none { it is OkHttp2Interceptor }` before `add(0, …)` (`:256-258`). Comment: "In okhttp2 (unlike okhttp3), networkInterceptors() returns direct access to an OkHttpClient list of interceptors… we have to modify the list in place, whenever it is first accessed" (`:241-251`). Dispose removes it only from the **last** list seen (`okHttp2Interceptors?.removeIf { it is OkHttp2Interceptor }`, `:359`; the field is overwritten per client at `:257`). So with several OkHttp 2 clients, only one is cleaned **(analysis)**.
- **gRPC**: a thread-local `hookDepth` counts nested builder factory calls so `intercept()` runs only on the outermost exit. Comment: "keep track of depth of chained calls, so we only install the hook once. For example, `AndroidChannelBuilder` delegates to `OkHttpChannelBuilder`." (`:98-102`). Test: `hookGrpcChannelBuilder_chainedCalls_installOnce` (`NIT/NetworkInspectorTest.kt:193-211`).

### 2.3 Clients created before the inspector started

- **OkHttp 3/4/5: handled implicitly.** OkHttp reads the getter on every call while building the chain: `if (!forWebSocket) { interceptors.addAll(client.networkInterceptors()); }` (`OK3.12:okhttp/src/main/java/okhttp3/RealCall.java:248-249`; `OK3.14:.../RealCall.java:218-219`); `interceptors += client.networkInterceptors` (`OK4.12:okhttp/src/main/kotlin/okhttp3/internal/connection/RealCall.kt:183-185`; `OK5.5:okhttp/src/commonJvmAndroid/kotlin/okhttp3/internal/connection/RealCall.kt:218-220`). Bytecode shows `RealCall` calls `invokevirtual okhttp3/OkHttpClient.networkInterceptors:()` in all three jars. The getter has JVM name `networkInterceptors` in 4.x/5.x via `@get:JvmName("networkInterceptors")` (`OK4.12:OkHttpClient.kt:142`, `OK5.5:OkHttpClient.kt:156`).
- **OkHttp 2**: OkHttp 2 also uses the getter internally: `client.networkInterceptors().get(index)` (`OK2.7.5:okhttp/src/main/java/com/squareup/okhttp/internal/http/HttpEngine.java:674,691,694`).
- **HttpURLConnection**: only connections opened after registration are wrapped **(analysis)**.
- **gRPC**: channels built before registration are handled explicitly with `findInstances` + reflection. Comment: "By the time this is executed, the app could have already created channels… This is known to be brittle but there doesn't seem to be a robust way of doing this." (`NI/NetworkInspector.kt:304-314`).

### 2.4 Hook mechanics underneath `ArtTooling` (useful for attach mode)

- Contract: exit hook "performs bytecode transformation and injects a call to exitHook at the end of originMethod"; `originMethod` format `"methodName(signature)"` in JNI form (`AX:ArtTooling.java:88-104`); `ExitHook.onExit(result)` returns "an object that should be returned instead" (`:77-86`).
- One native registration per (class, method); later registrations only add to a Java list: `mExitTransforms.computeIfAbsent(createLabel(origin, method), … nativeRegisterExitHook(…) … new CopyOnWriteArrayList<>())` (`AGENT/AppInspectionService.java:318-334`). Dispatch chains every hook's return value: `returnObject = (T) info.hook.onExit(returnObject);` (`:336-348`).
- Dispose removes the Java hooks but leaves the bytecode in place (`removeHooks`, `:244-252`, `:440-449`). A disposed hook therefore costs only the dispatcher lookup **(analysis)**.
- Native side:
  - Uses a stand-alone `jvmtiEnv` "to avoid any callback conflicts with other profilers' agents" (`NATIVE/src/app_inspection_service.cc:65-67`).
  - Each `AddTransform` calls `RetransformClasses(1, &origin_class)` under a `HiddenApiSilencer` (`:277-316`, `:306`).
  - The ClassFileLoadHook re-applies all accumulated transforms for that class with slicer (`:220-254`).
  - Exit hooks use `slicer::ExitHook` with `ReturnAsObject` (for non-primitive returns) and `PassMethodSignature`, calling `AppInspectionService.onExit` (`NATIVE/include/app_inspection_transform.h:52-60`).
  - ClassFileLoadHook policy: "Before P ClassFileLoadHook has significant performance overhead so we only enable the hook during retransformation… For P+ we want to keep the hook events always on to support multiple retransforming agents" (`app_inspection_service.cc:266-274`).
- `findInstances`: uses `IterateOverInstancesOfClass` on Q+, and on older devices `IterateThroughHeap` over every loaded subclass ("IterateThroughHeap doesn't include subclasses…"). It tags matches, then calls `GetObjectsWithTags` (`app_inspection_service.cc:93-192`).

### 2.5 Classloading (why the inspector can reference `okhttp3.*`)

- The dispatcher lives on the **boot classpath**. The transport agent calls `jvmti->AddToBootstrapClassLoaderSearch(agent_lib_path.c_str())` for `perfa.jar` from the agent `.so`'s own directory ("…should be in /data/user/<USER>/<PACKAGE_NAME>") (`TB:transport/native/agent/transport_agent.cc:38-58`). `perfa` bundles `"//tools/base/app-inspection/agent"` and the androidx.inspection AAR (`TB:profiler/app/BUILD:82-89`).
- The inspector dex is loaded by `new DexClassLoader(dexPath, optimizedDir, nativePath, classLoader)` (`AGENT/InspectorContext.java:144-153`). The parent is the first `Application`'s classloader, falling back to the main looper thread's context classloader (`AGENT/AppInspectionService.java:451-472`). Classloaders are cached for the process lifetime ("Having two DexClassloaders created from the same jars is a problem…", `InspectorContext.java:94-106`).
- Legacy profiler did the same split by hand. `OkHttp3Wrapper` (boot side) loads the interceptor from a separate dex, `new DexClassLoader(DEX_PATH, optimizedDir, null, classLoader)`, with the OkHttp object's classloader as parent. `insertInterceptor` returns `new ArrayList(interceptors)` with the interceptor added at 0 (`LP/okhttp/OkHttp3Wrapper.java:51-96`). DEX path is `/data/local/tmp/perfd/perfa_okhttp.dex` (`:98-110`).

---

## 3. OkHttp3 interceptor — full flow (`NI/okhttp/OkHttp3Interceptor.kt`)

### 3.1 Control flow (`:41-65`)

```kotlin
tracker = trackRequest(request)            // try { } catch (e: Throwable) { logInterceptionError(e, "OkHttp3 request") }
response = try { chain.proceed(request) } catch (ex: IOException) { tracker?.error(ex.toString()); throw ex }
if (tracker != null) response = trackResponse(tracker, request, response)   // catch (e: Throwable) -> log, return original
```

- Only `IOException` from `proceed` is reported. A `RuntimeException` from `proceed` escapes with no `Closed` event, leaving a dangling entry **(analysis)**.

### 3.2 Request capture (`:67-84`)

- URL `request.url.toString()`, method, and `request.headers.toMultimap()` → `RequestStarted` with transport `OKHTTP3`, plus a thread event (`NI/trackers/ConnectionTracker.kt:44-47`).
- `toMultimap()` lowercases header names in every version: `String name = name(i).toLowerCase(Locale.US);` (`OK3.12:okhttp/src/main/java/okhttp3/Headers.java:179-182`); `name(i).toLowerCase(Locale.US)` (`OK4.12:Headers.kt:210-213`); `name(i).lowercase(Locale.US)` (`OK5.5:Headers.kt:191-194`).
- Body: they **do** call `body.writeTo()` into a buffered sink over a null `OutputStream` wrapped by `OutputStreamTracker`, **before** `proceed()`:
  ```kotlin
  val outputStream = tracker.trackRequestBody(createNullOutputStream())
  val bufferedSink = outputStream.sink().buffer()
  when { body.isDuplex() -> bufferedSink.writeUtf8("Duplex body omitted")
         body.isOneShot() -> bufferedSink.writeUtf8("One-shot body omitted")
         else -> body.writeTo(bufferedSink) }
  bufferedSink.close()
  ```
  `close()` fires `RequestPayload` + `RequestCompleted` (`NI/reporters/StreamReporter.kt:131-140`). So **"request completed" is stamped before the request is sent (analysis)**.
- Duplex/one-shot APIs are version-dependent:
  - `isDuplex()`/`isOneShot()` do not exist in OkHttp 3.12.13 (grep of `OK3.12:RequestBody.java`: none). They exist in 3.14.9 (`OK3.14:RequestBody.java:76-92`) and in the 4.12/5.5 jars (javap).
  - On OkHttp 3.12.x any request with a body throws `NoSuchMethodError` right after `RequestStarted` was sent. `intercept` catches it and logs "…uses an outdated version of OkHttp" (`OkHttpUtils.kt:21-22,67-72`), but the tracker stays `null`, so the entry never completes **(analysis)**.
- `isOneShot()` "returns false unless it is overridden by a subclass" (`OK3.14:RequestBody.java:80-92`). A custom streaming body that doesn't override it is written **twice**: once by Studio, once by OkHttp **(analysis)**.
- Tests cover the placeholders: `"Duplex body omitted"`, `"One-shot body omitted"` (`NIT/OkHttp3Test.kt:157-181`).

### 3.3 Response capture (`:86-115`)

- Builds `fields` from `response.headers.toMultimap()` and adds a pseudo-header `fields[FIELD_RESPONSE_STATUS_CODE] = listOf(response.code.toString())` (`:87-89`; `FIELD_RESPONSE_STATUS_CODE = "response-status-code"`, `NI/rules/InterceptionRuleUtil.kt:28`).
- Runs rules: `interceptionRuleService.interceptResponse(NetworkConnection(url, method), NetworkResponse(response.code, fields, body.source().inputStream()))` (`:92-96`).
- Reports the **intercepted** code and headers: `tracker.trackResponseHeaders(interceptedResponse.responseCode, interceptedResponse.responseHeaders)` (`:98`).
- **Tee as the app reads** (no pre-buffering unless a body rule runs): `val source = tracker.trackResponseBody(interceptedResponse.body).source().buffer()` (`:99`), wrapped as a new body with the **original** `body.contentType(), body.contentLength()` (`:101`).
  - `InputStreamTracker` extends `FilterInputStream`. It records bytes in `read()`/`read(buf,off,len)` and fires `onStreamClose()` in `close()` (`NI/trackers/InputStreamTracker.kt:24-47`).
  - `skip(n)` for `n < MAX_BUFFER_SIZE` is turned into a read so the bytes are captured. Larger skips write `"...Skipped $skipped bytes..."` into the payload (`:49-58`).
- Always rebuilds the response. Headers are re-created from the intercepted multimap (lowercased, grouped, and **including the `response-status-code` pseudo-header**). The code is taken from the pseudo-header: `return response.newBuilder().headers(headers).code(code).body(responseBody).build()` (`:105-114`). So even with no rules, the app sees lowercased header names plus an extra `response-status-code` header **(analysis)**.
- `Headers.headersOf` validates values: `require(c == '\t' || c in '\u0020'..'\u007e')` (`OK4.12:Headers.kt:378-396,447-455`), while received headers can contain lenient values (`internal fun addLenient`, `:231,321`).
  - If a header value has non-ASCII bytes, the rebuild throws after `ResponseStarted` was sent and the tee was created. The `catch (Throwable)` then returns the **original** response, so the tee is bypassed and no `ResponseCompleted`/`Closed` is ever sent **(analysis)**.
- Size cap: `MAX_BUFFER_SIZE = 10 * 1024 * 1024` per direction (`NI/reporters/StreamReporter.kt:154`). Chunks that would exceed it are dropped with a log (`:59-62`); OOM falls back to `"Payload omitted because it was too large"` (`:65-69`, `:78-84`).

### 3.4 Timing

- Every event carries `System.nanoTime()` at send time (`NI/utils/InspectorProtocol.kt:22-30`). There are no phase timings (no DNS/connect/TLS), because no `EventListener` is used.
- The IDE maps event → field: `requestStartTimeUs` ← `RequestStarted`, `requestCompleteTimeUs` ← `RequestCompleted`, `responseStartTimeUs` ← `ResponseStarted`, `responseCompleteTimeUs` ← `ResponseCompleted`, `connectionEndTimeUs` ← `Closed` (`NM/connections/HttpData.kt:125-172`).
- Network interceptors run after connection setup. Chain order: `client.interceptors`, `RetryAndFollowUpInterceptor`, `BridgeInterceptor`, `CacheInterceptor`, `ConnectInterceptor`, then `client.networkInterceptors`, then `CallServerInterceptor` (`OK4.12:RealCall.kt:175-186`; `OK5.5:RealCall.kt:210-221`). Consequences **(analysis)**:
  - "Request start" excludes queueing, DNS, TCP and TLS.
  - Each redirect/retry hop is a separate connection id.
  - Cache hits never appear.
  - "Response completed" means "app closed the body".

### 3.5 Thread and call stack capture

- Stack: `getOkHttpCallStack(request.javaClass.getPackage().name)` (`:68`). It takes `Throwable().stackTrace`, finds the first contiguous block of frames whose class starts with the OkHttp package, and returns every frame **after** that block as `"$element\n"` lines (`NI/okhttp/OkHttpUtils.kt:24-43`).
- Thread: `reportCurrentThread()` on request start, response headers, and every body read/write (`ConnectionTracker.kt:44-52`, `InputStreamTracker.kt:35,45,50`, `OutputStreamTracker.kt:37,43`).
- `enqueue()` → yes, they get the **dispatcher thread**. The interceptor runs inside `AsyncCall.run()`, which renames the worker: `threadName("OkHttp ${redactedUrl()}") { … getResponseWithInterceptorChain() … }` (`OK4.12:RealCall.kt:512-517`; `OK5.5:RealCall.kt:577-582`; 3.x `super("OkHttp %s", redactedUrl())`, `OK3.12:RealCall.java:159`). So for async calls the "trace" holds only executor frames and no app frames **(analysis)**.
- The real caller is visible to `EventListener.callStart`, which runs on the calling thread for both paths:
  ```kotlin
  override fun enqueue(responseCallback: Callback) { … callStart(); client.dispatcher.enqueue(AsyncCall(responseCallback)) }
  private fun callStart() { this.callStackTrace = …; eventListener.callStart(this) }
  ```
  (`OK4.12:RealCall.kt:160-172`, `OK5.5:RealCall.kt:195-207`; 3.12: `captureCallStackTrace(); eventListener.callStart(this);` in `enqueue`, `OK3.12:RealCall.java:120-126`). Studio does not use it.
- With `execute()`, if the app has application interceptors, the trace starts at the first non-OkHttp frame, i.e. inside the app interceptor, not at the business caller **(analysis)**.

### 3.6 Failures and cancellations

- `tracker.error(msg)` → `onError(status)` sends `Closed(completed = false)`. **`status` is dropped**: `.setHttpClosed(…Closed.newBuilder().setCompleted(false))` (`NI/reporters/ConnectionReporter.kt:111-117`). The IDE's `HttpData.error` is hard-coded `"N/A"` (`NM/connections/HttpData.kt:104-105`).
- Cancellation surfaces as an `IOException` from `proceed` → same `Closed(false)`. Tests: aborted call yields 3 events `[RequestStarted, HttpThread, Closed(false)]` (`NIT/OkHttp3Test.kt:183-208`).
- Errors while reading the body are not tracked. `InputStreamTracker.read` doesn't catch. If the app then closes the stream, `onStreamClose()` sends `Closed(completed = true)` with a partial payload, i.e. a success **(analysis)** (`InputStreamTracker.kt:26-47`, `StreamReporter.kt:107-120`).

### 3.7 OkHttp 3 vs 4 vs 5 differences

- One Kotlin class compiled against OkHttp 4.12.0 (`artifacts.bzl:106`). The accessors it uses have identical JVM signatures in all three jars: `okhttp3.HttpUrl url()`, `String method()`, `Headers headers()`, `RequestBody body()`, `int code()`, `ResponseBody body()` (javap of `JAR(okhttp-3.12.13 / 4.12.0 / jvm-5.5.0)`). So Kotlin property syntax (`request.url`, `response.code`) links on 3.x too.
- Kotlin-only 4.x APIs have reflection fallbacks:
  - `safeAsResponseBody` tries `asResponseBody`, else reflects `ResponseBody.create(MediaType, long, BufferedSource)`. Comment: "it's not possible to call the deprecated method directly because Kotlin assumes it's in a companion object which doesn't exist in the old Java implementation" (`:118-133`).
  - `headersOf` does the same with `Headers.of(String[])` (`:135-150`).
- javap shows the 3.x statics still exist as statics in 4.12 and 5.5: `public static final okhttp3.ResponseBody create(okhttp3.MediaType, long, okio.BufferedSource);` and `public static final okhttp3.Headers of(java.lang.String...);`. So a Java library can call them directly on 3.12–5.5 (`JAR(all three)`).
- `Interceptor.Chain.call()` exists in 3.12.13, 4.12.0 and 5.5.0 (javap).
- There is no transport enum for 4/5 (`PROTO:18-23`). The IDE labels `OKHTTP3` as `"OkHttp 3"` (`NM/connections/HttpData.kt:274-281`).
- Linkage failures are expected and logged: `"…which could happen if your project uses proguard to remove unused code or uses an outdated version of OkHttp"` (`OkHttpUtils.kt:21-22`).

### 3.8 Event sequences (from tests)

- GET: 6 events (`NIT/OkHttp3Test.kt:74`) — RequestStarted, HttpThread, ResponseStarted, ResponsePayload, ResponseCompleted, Closed(true). The positions of ResponseStarted (index 2) and Closed (index 5) are asserted in the abort test (`:210-220`).
- POST: 8 events, adding RequestPayload and RequestCompleted (`:116-127`).

---

## 4. Interception rules

### 4.1 Data model

- `InterceptionRule { isEnabled; transform(connection, response) }`. `InterceptionRuleImpl` builds `InterceptionCriteria` plus a list of transformations from proto (unknown kinds are dropped via `mapNotNull`) (`NI/rules/InterceptionRule.kt:22-47`).
- `NetworkConnection(url, method)`, `NetworkResponse(responseCode, responseHeaders: Map<String?, List<String>>, responseBody: InterceptedResponseBody, interception: NetworkInterceptionMetrics)`. `InterceptedResponseBody` is either a successful body (`InputStream`) or a failure (`IOException`) (`NI/rules/InterceptionRuleService.kt:22-68`).
- Registry: `rules: Map<Int, Rule>` plus ordered `ruleIdList`. All methods are `@Synchronized`.
  - `addRule` on an existing id overwrites in place; `InterceptRuleUpdated` is also routed to `addRule` (`NI/NetworkInspector.kt:144-148`).
  - `reorderRules` replaces the list (`InterceptionRuleService.kt:86-116`).
  - Apply = fold over enabled rules in list order: `ruleIdList.mapNotNull { id -> rules[id] }.filter { it.isEnabled }.fold(response) { r, rule -> rule.transform(connection, r) }` (`:91-96`).
  - A matching rule folds all its transformations and sets `criteriaMatched = true` (`InterceptionRule.kt:49-58`). Multiple matching rules chain.
- Lock contention **(analysis)**: the whole fold runs under the monitor, including `BodyModified`, which reads the entire network body. One slow matching download blocks rule evaluation for every other concurrent response.

### 4.2 Matching semantics (`NI/rules/InterceptionCriteria.kt`, `InterceptionRuleUtil.kt`)

- **Method**: enum → exact string compare; `METHOD_UNSPECIFIED` matches everything (`InterceptionCriteria.kt:41-58`).
- **Protocol**: `"https"`/`"http"` vs `URL(connection.url).protocol`; unspecified matches everything (`:60-70`).
- **Host/port/path/query**: `wildCardMatches(pattern, text)`.
  - A blank pattern matches everything. Otherwise `?`→`.` and `*`→`.*` with literal segments `Regex.escape`d, and the match is **full-string and case-sensitive** (`InterceptionRuleUtil.kt:47-49,70-91`). `Regex.matches` "matches the entire input" (`KT:.../jvm/src/kotlin/text/regex/Regex.kt:107-108`).
  - Port comes from `url.port.toString()` (`InterceptionCriteria.kt:34`). `URL.getPort()` returns "the port number, or -1 if the port is not set" (`LIBCORE:URL.java:781-786`), so a criterion `443` never matches `https://host/…` **(analysis)**.
  - `getQuery()` returns null when absent (`URL.java:738-745`), so only a blank pattern matches then.
- **MatchingText** (used by transformations):
  - `PLAIN` → `this.text.equals(text, ignoreCase)` (exact equality, not substring).
  - `REGEX` → full `Regex.matches`.
  - `UNDEFINED` → matches all (`InterceptionRuleUtil.kt:31-44`).
  - Header names are matched ignore-case (`InterceptionTransformation.kt:133`); values are case-sensitive (`:135`).

### 4.3 Transformations (`NI/rules/InterceptionTransformation.kt`)

- **StatusCodeReplaced** (`:38-105`):
  - `newCode.toIntOrNull()` must parse, else the rule is ignored with a log.
  - It first tries the null-key status line (`statusLine.startsWith("HTTP/1.")`, parse code, rebuild `"$prefix $replacingCode$suffix"`, and also update the pseudo-header).
  - Otherwise it matches and replaces the `response-status-code` pseudo-header. OkHttp has no null key, so OkHttp uses the pseudo-header path.
- **HeaderAdded**: appends a value under `name` (no dedupe) (`:108-117`).
- **HeaderReplaced**:
  - Removes every (name, value) where the name matches (ignore case) and the value matches, then re-adds them under `newName ?: oldName` and `newValue ?: oldValue`.
  - It is a no-op if neither `new_name` nor `new_value` is set (`:120-155`).
  - **There is no header-remove transformation** in the proto or the code.
- **BodyReplaced**: fixed bytes; gzipped when `isContentCompressed` (`:158-172`).
- **BodyModified**:
  - Only for text-like types: type `text`, or a subtype/suffix in `setOf("csv", "html", "json", "xml")` (`:174`, `:204-224`, comment "With suffix: vnd.api+json, svg+xml… With charset: json; charset=utf-8").
  - Gunzips if needed, reads all text, then `bodyModified.targetText.toRegex().replace(body, bodyModified.newText)`, re-encodes, and re-gzips (`:179-195`).
  - `PLAIN` uses `Regex.fromLiteral` (`InterceptionRuleUtil.kt:52-57`), but `new_text` is always a regex **replacement template**. `$`/`\` are special and malformed templates throw `RuntimeException` (`KT:Regex.kt:158-180`).
  - Only `IOException`/`ZipException` are caught (`:196-201`). A thrown `RuntimeException` after the body was consumed leaves the app with a drained body (OkHttp path) or propagates out of `getInputStream()` (HttpURLConnection path) **(analysis)**.

### 4.4 Where rules are applied

- OkHttp: after `chain.proceed()` returns and before the app reads the body (`OkHttp3Interceptor.kt:50-56,92-96`; OkHttp2 same at `OkHttp2Interceptor.kt:47-53,84-88`). Response-only.
- There is no request modification and no mocking/short-circuit. That would be illegal anyway for a network interceptor: `"network interceptor ${…} must call proceed() exactly once"` (`OK4.12:okhttp/src/main/kotlin/okhttp3/internal/http/RealInterceptorChain.kt:100,114`); OkHttp 2 docs: "These interceptors must call Interceptor.Chain#proceed exactly once" (`OK2.7.5:OkHttpClient.java:550-557`).
- HttpURLConnection: at the first response access (`TrackedHttpURLConnection.trackResponse`, see §5). `HttpURLTransformer.kt` only does the wrapping (`NI/httpurl/HttpURLTransformer.kt:30-50`).

### 4.5 Content-Encoding / gzip / Content-Length

- Compression detection: `contentHeaderValues.any { it.lowercase().contains("gzip") }` on key `"content-encoding"` (`InterceptionRuleUtil.kt:59-62`). `br`, `deflate` and `zstd` are not handled.
- At the network-interceptor layer OkHttp bodies are still compressed. BridgeInterceptor adds `Accept-Encoding: gzip` and decompresses only after `chain.proceed` returns, stripping headers and making length unknown:
  ```kotlin
  val strippedHeaders = networkResponse.headers.newBuilder().removeAll("Content-Encoding").removeAll("Content-Length").build()
  responseBuilder.body(RealResponseBody(contentType, -1L, gzipSource.buffer()))
  ```
  (`OK4.12:okhttp/src/main/kotlin/okhttp3/internal/http/BridgeInterceptor.kt:66-72,90-103`; same in `OK5.5:BridgeInterceptor.kt:65-70,92-106`). That is why Studio gzips replaced bodies.
- **Content-Length is never adjusted.**
  - The new OkHttp body keeps `body.contentLength()` of the original (`OkHttp3Interceptor.kt:101`), and the `content-length` header is copied through.
  - `ResponseBody.bytes()` throws `"Content-Length ($contentLength) and stream length ($size) disagree"` (`OK4.12:ResponseBody.kt:124,136-150`). So a replaced or modified **non-gzip** body breaks `bytes()`, while gzip responses are safe because Bridge resets the length to -1 **(analysis)**.
  - HttpURLConnection: `contentLength`/`contentLengthLong`/`contentType`/`contentEncoding` return the **wrapped** (original) values (`NI/httpurl/TrackedHttpURLConnection.kt:191-195,357-373`).
- `BodyReplaced` drops the original stream without reading or closing it (`InterceptionTransformation.kt:166-171`) **(analysis)**. Consequence for connection reuse: UNVERIFIED.

### 4.6 Reporting "rule applied" and the original response

- `ConnectionReporter.onInterception` sends `ResponseIntercepted` flags only `if (interception.criteriaMatched)` (`NI/reporters/ConnectionReporter.kt:94-109`).
- **Only the HttpURLConnection path calls it** (`TrackedHttpURLConnection.kt:117`). A grep of `NI/` shows no call in `OkHttp3Interceptor`/`OkHttp2Interceptor`, so Studio never learns that a rule changed an OkHttp response.
- The IDE ignores it for display: `HTTP_RESPONSE_INTERCEPTED -> data` and only feeds analytics (`usageTracker.trackResponseIntercepted`) (`NM/DataHandler.kt:106,198-209`).
- The **original response is not reported or kept**. `ResponseStarted` and `ResponsePayload` carry the intercepted values (`OkHttp3Interceptor.kt:98-99`; `TrackedHttpURLConnection.kt:116,339`).

### 4.7 Rules on HttpURLConnection

- `trackResponse()` builds `NetworkResponse(wrapped.responseCode, wrapped.headerFields, wrapped.inputStream)`, runs the rules, stores `interceptedResponse`/`interceptedHeaders`, then reports headers and flags (`TrackedHttpURLConnection.kt:102-125`).
- Getters serve intercepted data:
  - `getHeaderField(n)`/`getHeaderFieldKey(n)` read from `interceptedHeaders` (`:303-330`).
  - `headerFields`/`getHeaderField(key)` read from `interceptedResponse.responseHeaders` (`:312-321`).
  - `inputStream` reads `interceptedResponse.body` (`:333-344`).
  - `getResponseCode()` isn't overridden. The base implementation calls `getInputStream()` then parses `getHeaderField(0)` (`LIBCORE:HttpURLConnection.java:671-716`), so status rules act through the rewritten null-key status line.
- Case-sensitivity bug **(analysis)**:
  - Android's `getHeaderFields()` returns a `TreeMap` with a null-first, case-insensitive comparator and **original-case** names (`EXTOK:okhttp/.../internal/http/OkHeaders.java:25-38,91-109`; `HttpURLConnectionImpl.java:221-228`).
  - Transformations copy it with `toMutableMap()`, which is `LinkedHashMap(this)` (`KT:libraries/stdlib/src/kotlin/collections/Maps.kt:785-791`), i.e. case-sensitive.
  - After the first header-changing transformation, lookups of `"content-type"`/`"content-encoding"` fail when the server used `Content-Type`, so a later `BodyModified`/gzip check silently doesn't apply.
  - OkHttp is unaffected because `toMultimap()` already lowercases.
- A 404→200 rule cannot work on HttpURLConnection: `wrapped.inputStream` throws for ≥400 before the rules run (§5).

---

## 5. HttpURLConnection wrapping

- **Wrapping**: `wrapURLConnection` computes `getStackTrace(2)` and returns `HttpsURLConnectionWrapper` / `HttpURLConnectionWrapper` / the original object (`NI/httpurl/HttpURLTransformer.kt:35-49`).
  - Both wrappers subclass the platform type with `(wrapped.url)` and forward every overridden method to one `TrackedHttpURLConnection` (`HttpURLConnectionWrapper.kt:30-38`, `HttpsURLConnectionWrapper.kt:34-41`).
  - Deliberately not overridden: `getResponseCode`/`getResponseMessage` ("derived from this method [getInputStream]", `HttpURLConnectionWrapper.kt:152-155`) and `getHeaderFieldDate/Int/Long` ("derived from [getHeaderField(name)]", `:132-135`).
- **RequestStarted** (`trackPreConnect`, once) is sent on the first of:
  - `connect()` (`:155-166`);
  - `getOutputStream()` (`:290-301`);
  - `getInputStream()` (`:332-344`);
  - any response getter, via `tryTrackResponse()` → `tryConnect()` → `connect()` (`:77-92,127-140`).
  - Method fix-up: `if (wrapped.doOutput && wrapped.requestMethod == "GET") "POST"`, with comment "HttpURLConnection only updates its method to "POST" after connect is called. But for our tracking purposes, that's too late." (`:174-181`).
- **Request body**: `getOutputStream()` wraps the real stream in `OutputStreamTracker`. `RequestPayload` + `RequestCompleted` fire on `close()` (`NI/trackers/OutputStreamTracker.kt:25-28`) or when `disconnect()` closes it (`:142-153`).
- **ResponseStarted** (`trackResponse`, once) is triggered by `getInputStream`, `getHeaderField(s)`, `getHeaderFieldKey`, `getContent`, `getContentLength(Long)`, `getContentType`, `getContentEncoding` (`:191-195,303-373`; tested one by one in `NIT/httpurl/TrackedHttpURLConnectionTest.kt:35-123`). Comment: "IMPORTANT: This method, as a side effect, will cause the request to get sent if it hasn't been sent already" (`:94-101`).
- **Response body** is wrapped in `InputStreamTracker`. `ResponsePayload` + `ResponseCompleted` + `Closed(true)` fire on `close()` or on `disconnect()`. Comment: "The streams are wrappers around the actual output/input streams, so they need to be cleaned up manually. (calling disconnect won't close them)" (`:50-55`).
- **App never reads the body** **(analysis)**:
  - `getResponseCode()` already creates a response tracker (base impl → wrapper `getInputStream()`).
  - Nothing completes until `close()` or `disconnect()`. Without either, the entry stays open forever.
  - Calling `getInputStream()` twice creates two trackers over the same stream, and each close would emit its own completion events.
- **disconnect()**: closes tracked streams (swallowing exceptions), `wrapped.disconnect()`, then `connectionTracker.disconnect()`, which is **empty**: `override fun disconnect() {}` (`NI/trackers/ConnectionTracker.kt:34`). If no stream was created, disconnect emits nothing **(analysis)**.
- **Errors**:
  - `connect()`, `getOutputStream()` and `getInputStream()` `IOException`s → `connectionTracker.error()` → `Closed(false)`, no message (`:162-165,297-299,340-342`).
  - HTTP ≥400: Android's impl throws `FileNotFoundException(url.toString())` when `getResponseCode() >= HTTP_BAD_REQUEST` (`EXTOK:okhttp-urlconnection/.../huc/HttpURLConnectionImpl.java:239-255`). In `trackResponse`, `wrapped.inputStream` is evaluated inside the `NetworkResponse(...)` constructor arguments, so the exception is thrown **before** `trackResponseHeaders`. The catch then stores `NetworkResponse(-1, wrapped.headerFields, e)` (`:118-120`). Result **(analysis)**:
    - via `getInputStream()`/`getResponseCode()`: `Closed(false)` without status or headers;
    - via header getters: exception swallowed, no Closed, a dangling entry.
  - `getErrorStream()` is pure pass-through (`:168-169`), so **error bodies are never captured**.
  - Test `failed connection should not affect getting headerFields` checks empty headers don't throw (`NIT/HttpUrlTest.kt:251-265`).
- **getResponseCode() without reading**: test `getResponseCodeBeforeConnect` expects 6 events after `responseCode` then reading the stream (`NIT/HttpUrlTest.kt:219-249`).
- **HTTPS-specific methods**: `getCipherSuite`, `getLocalCertificates`, `getServerCertificates`, `getPeerPrincipal`, `getLocalPrincipal`, `set/getHostnameVerifier`, `set/getSSLSocketFactory` delegate straight to `wrappedHttps` with no tracking (`HttpsURLConnectionWrapper.kt:43-77`).
- **Status line reporting**: the null header key is sent as the literal string `"null"`: `setKey(it.key ?: "null")` (`ConnectionReporter.kt:88`). The test expects `Header("null", "HTTP/1.0 200 OK")` (`NIT/HttpUrlTest.kt:42-46`).

---

## 6. Payloads, traffic graph, ids

### 6.1 Payloads

- Bodies are buffered in memory: `ByteString.newOutput(INITIAL_BUFFER_SIZE)`, 1 KiB initial, max 10 MiB (`NI/reporters/StreamReporter.kt:45,150-155`).
- Sent **once, on stream close**, as a single `Payload` message plus completion events (`:72-87`, `:99-141`). Double close is guarded: "prevent the double reporting of stream closed events because this is reachable by both calling disconnect() on the HttpUrlConnection, and calling close() on the stream" (`:73-75`).
- No chunking at the inspector level. The App Inspection layer splits any event over `4,000,000` bytes into a chunked payload (`AGENT/InspectorContext.java:57`, `ConnectionImpl.java:33-40`).
- Over-cap behaviour: `if (buffer.size() + len > maxBufferSize) { Logger.error(...); return }` (`:59-62`). This drops that chunk but later, smaller chunks can still append, which corrupts the stored body **(analysis)**. Tests pin the drop behaviour (`NIT/reporters/StreamReporterTest.kt:59-81`).
- Legacy profiler streamed instead: `InputStreamTracker` fed a `ByteBatcher` that flushed via native `reportBytes(id, bytes, len)` (`LP/HttpTracker.java:34-124`) with `DEFAULT_THRESHOLD = 1024` (`TB:profiler/app/common/src/main/java/com/android/tools/profiler/support/util/ByteBatcher.java:42`).
- IDE decodes for display: `encodings.contains("gzip") -> GZIPInputStream`, `encodings.contains("br") -> BrotliInputStream` (`NM/connections/HttpData.kt:116-123`), based on the response `content-encoding` header.

### 6.2 Traffic graph (Receiving/Sending)

- Source: **uid-level `TrafficStats` totals**, not captured bytes (`NI/TrafficStatsProviderImpl.kt:24-26`).
  - Docs: "Return number of bytes received by the given UID since device boot. Counts packets across all network interfaces… Statistics are measured at the network layer, so they include both TCP and UDP usage."
  - "Starting in Build.VERSION_CODES.N this will only report traffic statistics for the calling UID. It will return UNSUPPORTED for all other UIDs" (`CONN:TrafficStats.java:1076-1094`; Tx: `:1054-1071`).
  - So the graph includes non-HTTP traffic. The IDE says so explicitly: OOB speed events "can be caused non-HTTP network traffic or HTTP traffic from an unsupported transport layer" (`NM/DataHandler.kt:68-80`).
- Sampling:
  - `POLL_INTERVAL_MS = 500L`, `MULTIPLIER_FACTOR = 1000 / POLL_INTERVAL_MS`, so speed = byte delta per 500 ms × 2 = **bytes/s** (`NI/NetworkInspector.kt:53-54,176-208`).
  - The multiplier is fixed even when tests shorten the interval: `speedDataIntervalMs = 10` and the test expects `speedEvent(20, 20)` for a delta of 10 (`NIT/testing/NetworkInspectorRule.kt:32`, `NIT/NetworkInspectorTest.kt:59-68`).
- Zero suppression: "There is no value in sending a constant stream of `zero` events. We just need to make sure we send the first and last `zero` event of such a sequence" (`:190-191`). A back-dated zero is emitted before a non-zero sample (`:200-202`).
- It runs on the inspector's primary executor via coroutines (`:87`) and needs an `Application` uid (`:171-175`).
- IDE chart plots `speedEvent.rxSpeed` / `txSpeed` series directly (`NM/NetworkSpeedLineChartModel.kt:52-56`).

### 6.3 Connection ids

- `sealed class IdGenerator { val id = AtomicLong(); fun nextId() = id.getAndIncrement() }` and `object ConnectionIdGenerator : IdGenerator()` (`NI/utils/IdGenerators.kt:22-29`). The id is process-wide, starts at 0, and is shared by HTTP (`ConnectionReporter.kt:52`) and gRPC (`GrpcTracker.kt:41`). Tests reset it with `ConnectionIdGenerator.id.set(0)` (`NIT/testing/NetworkInspectorRule.kt:60`).
- One id per tracker, i.e. per network-interceptor invocation for OkHttp and per wrapped `URLConnection` for java.net **(analysis)**.

---

## 7. Threads and call stacks

- **Recorded**: `ThreadData(thread_id = Thread.getId(), thread_name = Thread.getName())` (`PROTO:139-145`; `NI/reporters/ThreadReporter.kt:39-48`). Stacks are **one string** of `StackTraceElement.toString()` lines joined by `\n`, sent once in `RequestStarted.trace` (`OkHttpUtils.kt:39`, `common/Stacktrace.kt:30`). There are no structured frames.
- **ThreadReporter**: per-connection `private var lastThread: Thread? = null`. It sends a thread event only when `thread !== lastThread` (`ThreadReporter.kt:35-49`), which is not synchronized (the gRPC variant uses `AtomicReference`, `GrpcTracker.kt:43,121-127`). The interface KDoc says "Reports the current thread's call frames" (`ThreadReporter.kt:26`), but only id and name are sent.
- It is called on request start, response headers and every body read/write, so a connection can list several threads. The IDE appends them: `threads = threads + event.toJavaThread()` (`NM/connections/HttpData.kt:139-140`).
- **Where the stack is captured**:
  - OkHttp: inside the network interceptor, on the chain thread; frames after the OkHttp block (§3.5).
  - java.net: inside the `URL.openConnection()` exit hook via `getStackTrace(2)`. It drops leading frames whose class starts with `"com.android.tools.appinspection"`, then 2 more (`common/Stacktrace.kt:19-31`; comment `// Skip the irrelevant stack trace elements (including app inspection stack frames)`, `HttpURLTransformer.kt:35-36`). The 2 skipped frames correspond to `AppInspectionService.onExitInternal`/`onExit` (`AGENT/AppInspectionService.java:336-352`) **(analysis)**.
  - gRPC: `getStackTrace(1)` in `ClientCall.start` (`NI/grpc/GrpcInterceptor.kt:57`).
- **IDE use**: `StackFrameParser.parseStack(trace)` → `CodeLocation`s for the "Call Stack" tab (`IDE:view/src/.../details/CallStackTabContent.kt:35-45`).

---

## 8. Pitfall / quirk comments (verbatim)

From the inspector (`NI/`) and its agent:
1. OkHttp 2 live list: "In okhttp2 (unlike okhttp3), networkInterceptors() returns direct access to an OkHttpClient list of interceptors and uses that as the API for a user to add more. Therefore, we have to modify the list in place…" (`NetworkInspector.kt:241-251`). The legacy version adds: "If we created a copy of the list and returned that instead (a common instrumentation pattern), then the user's interceptor would never actually get added into the underlying list." (`LP/okhttp/OkHttp2Wrapper.java:77-99`).
2. gRPC double registration: "TODO(b/313873107): Find a safe way to register gRPC hooks. Note that we only hook `AndroidChannelBuilder.forTarget()` because the implementation of `forAddress` calls `forTarget` and would result in double registration." (`NetworkInspector.kt:71-76`). The list at `:77-85` nevertheless contains both `forAddress` and `forTarget` for all three builders, and `hookDepth` handles nesting.
3. Chained builders: "keep track of depth of chained calls, so we only install the hook once. For example, `AndroidChannelBuilder` delegates to `OkHttpChannelBuilder`." (`:98-101`).
4. Pre-existing gRPC channels: "This is known to be brittle but there doesn't seem to be a robust way of doing this." (`:304-314`).
5. Multiple `Application`s: "The app can have multiple Application instances. In that case, we use the first non-null uid, which is most likely from the Application created by Android." (`:169-170`).
6. R8/proguard: "App does not use OKHttp or class is omitted by app reduce" (`:287`). Also `LINK_ERROR_MESSAGE` "…project uses proguard to remove unused code or uses an outdated version of OkHttp" (`OkHttpUtils.kt:21-22`).
7. Okio vs streams: "OkHttp uses okio.Sinks instead of OutputStreams for their request bodies, so we provide a temporary stub output stream… TODO: We may want to clean up this assumption in HttpTracker so these OkHttp gymnastics are not required." (`OkHttpUtils.kt:45-53`).
8. OkHttp 3 vs 4 binary API: "Kotlin assumes it's in a companion object which doesn't exist in the old Java implementation." (`OkHttp3Interceptor.kt:118-125,135-142`).
9. HttpURLConnection state machine: "call this method just before HttpURLConnection.connect is called, after which point, HttpURLConnection throws exceptions if you try to access the fields we want to track" (`TrackedHttpURLConnection.kt:61-66`); "Calling connect ourselves is useful in case the user calls a HttpURLConnection method which would otherwise have caused a `connect` to happen as a side effect" (`:77-82`); "Just because the user connected doesn't mean the request was sent out yet… we don't call trackResponse here." (`:159-161`); "HttpURLConnection only updates its method to "POST" after connect is called" (`:174-181`); streams need manual cleanup (`:50-53`); the IMPORTANT side-effect notes (`:94-101`, `:127-135`).
10. Test comment: "HttpURLConnection has many functions to query response values which cause a connection to be made if one wasn't already established." (`NIT/HttpUrlTest.kt:219-222`).
11. Double close: "prevent the double reporting of stream closed events because this is reachable by both calling disconnect() on the HttpUrlConnection, and calling close() on the stream." (`StreamReporter.kt:73-75`).
12. gzip in rules: "If we got here, it means we failed to unzip data that was supposedly zipped." (`InterceptionTransformation.kt:197`).
13. API level: `"--min-api 26",  # Network inspector is only supported on O+ devices.` (`BUILD:50`). Agent: "Before P ClassFileLoadHook has significant performance overhead…" (`NATIVE/src/app_inspection_service.cc:266-270`). JVMTI heap walk differs before/after Q (`:82-139`).
14. Thread id semantics: "ID of the thread obtained from Java, which is different from the thread ID obtained in a JNI context." (`PROTO:140-141`).
15. Oldest supported OkHttp 2: "we need to test against OkHttp2.2 here (as it is the oldest version we support)…" (`NIT/OkHttp2Test.kt:170-179`).
16. Classloader caching: "Having two DexClassloaders created from the same jars is a problem, because they start fighting over resources… (b/187342510)" (`AGENT/InspectorContext.java:94-104`).
17. Hook registration race: "Lock to prevent race condition when registering hooks. See b/376717110." (`AGENT/AppInspectionService.java:53-54`).

**No comments exist** in the inspector about WebSocket/101, HTTP/2, BridgeInterceptor or proceed-once (grep of `src`, `BUILD`, `resources` for those terms: only gzip helper code and `chain.proceed` calls). Verified OkHttp facts for those topics instead:
- **WebSocket**: network interceptors are skipped for WebSocket calls, `if (!forWebSocket)` (`OK3.12:RealCall.java:248`, `OK4.12:RealCall.kt:183`, `OK5.5:RealCall.kt:218`). The WebSocket client is built with `client.newBuilder().eventListener(EventListener.NONE).protocols(ONLY_HTTP1)` and `RealCall(webSocketClient, request, forWebSocket = true)` (`OK4.12:.../internal/ws/RealWebSocket.kt:153-164`). So Studio never sees WebSocket upgrades.
- **Proceed-once and host/port rules** for network interceptors (`OK4.12:RealInterceptorChain.kt:97,100,114,118`).
- **Gzip**: transparent gzip happens outside network interceptors (§4.5).
- **Call-stack origin**: `callStart` runs on the caller thread (§3.5).
- **Newer OkHttp**: 5.4+ adds `Call.addEventListener` — "Configure this call to publish all future events to [eventListener], in addition to the listeners configured by [OkHttpClient.Builder.eventListener]" (`OK5.5:okhttp/src/commonJvmAndroid/kotlin/okhttp3/Call.kt:91-102`). The method is present in `OK5.4` `Call.kt` and absent in `parent-5.3.0` (grep count 1 vs 0). The 5.3.0 changelog adds "`EventListener.plus()`" (`OK5.5:CHANGELOG.md:114`).

---

## 9. gRPC (stretch goal) — summary

Studio hooks gRPC at channel-build time, not at the transport:
- Entry/exit hooks on the static factories `forAddress(String,int)` and `forTarget(String)` of `io.grpc.ManagedChannelBuilder`, `io.grpc.android.AndroidChannelBuilder` and `io.grpc.okhttp.OkHttpChannelBuilder`. A thread-local depth counter makes only the outermost exit call `channelBuilder.intercept(grpcInterceptor)` (`NI/NetworkInspector.kt:77-85,333-356`).
- Channels that already exist are patched by `findInstances(ManagedChannel)`, which swaps the private `ManagedChannelImpl.interceptorChannel` for an `InterceptingGrpcChannel` delegating to the same interceptor (`:315-331,363-373`).
- `GrpcInterceptor` (`NI/grpc/GrpcInterceptor.kt`) creates a `GrpcTracker` per call (new connection id) and adds a `ClientStreamTracer.Factory` through `CallOptions.withStreamTracerFactory` (`:40-47`).
  - `ClientStreamTracer.streamCreated` gives the remote address (`TRANSPORT_ATTR_REMOTE_ADDR`) and request headers.
  - The forwarding `ClientCall` reports `start` (service, bare method, headers, `getStackTrace(1)`) and each `sendMessage`.
  - The forwarding listener reports `onHeaders`, `onMessage`, `onClose(status, trailers)` (`:49-97`).
- Payloads are the marshalled bytes: `stream(message).readAllBytes()`. `type` = message class name; `text` = proto text when the superclass package starts with `com.google.protobuf`, else `toString()` (`NI/trackers/GrpcTracker.kt:152-167`). `GrpcCallEnded` carries `status.code` and the cause stack trace as `error` (`:104-114`).
- IDE shows gRPC only behind `StudioFlags.NETWORK_INSPECTOR_GRPC` (`NM/DataHandler.kt:125-128`).
- Caveat: `readAllBytes()` and `Class.getPackageName()` are Java 9 APIs (`LIBCORE:ojluni/src/main/java/java/io/InputStream.java` Javadoc `@since 9`). Android API-level availability is UNVERIFIED.

---

## 10. Tests (`NIT/`) — coverage and tooling

**Harness**:
- JUnit4 plus `robolectric_test` with Robolectric `4.14.1` (`BUILD:65-89`, `artifacts.bzl:258`). Most classes use `@Config(manifest = Config.NONE, minSdk = O, maxSdk = UPSIDE_DOWN_CAKE)`, `CloseGuardRule`, and `LogPrinterRule` (dumps `ShadowLog` after each test, `TB:app-inspection/inspectors/common/testSrc/.../LogPrinterRule.kt`).
- Assertions: Truth, `kotlin.test`, Mockito.
- **No MockWebServer** (grep: none; not in test deps) and no real sockets for HTTP.

**Fakes**:
- `FakeArtTooling` stores hooks keyed `"${class.name}:$method"` and exposes `triggerEntryHook`/`triggerExitHook` (`TB:app-inspection/inspectors/common/testSrc/.../FakeArtTooling.kt:26-45`).
- `NetworkArtTooling.findInstances(Application)` returns `listOf(Application(), FakeApplication())` (`NIT/testing/NetworkArtTooling.kt:23-38`).
- `FakeEnvironment` uses a direct executor (`FakeEnvironment.kt:30`).
- `FakeConnection` parses each sent `Event` into `httpData`/`grpcData`/`speedData` lists (`FakeConnection.kt:22-35`).
- `FakeTrafficStatsProvider` replays scripted rx/tx totals (`FakeTrafficStatsProvider.kt:23-40`).
- `NetworkInspectorRule` builds the inspector with a 10 ms speed interval, auto-starts it, and disposes and resets ids afterwards (`NetworkInspectorRule.kt:27-61`).
- OkHttp fakes don't run OkHttp at all:
  - `FakeOkHttp3Client(triggerExitHook(okhttp3.OkHttpClient, "networkInterceptors()…", emptyList()))` calls `networkInterceptorz.first().intercept(fakeChain)`, where `proceed` returns a canned `Response` or throws `IOException("BLOWING UP")` (`okhttp3/FakeOkHttp3Client.kt:28-83`, `NIT/OkHttp3Test.kt:223-231`).
  - The OkHttp 2 fake overrides `networkInterceptors()` with a mutable list (`okhttp2/FakeOkHttp2Client.kt:26-57`).
- HttpURLConnection fakes: `FakeHttpUrlConnection` with in-memory streams and a header map keyed by the string `"null"` (`http/FakeHttpUrlConnection.kt:22-57`). One test calls a real `URL.openConnection()` to an unresolvable host (`NIT/HttpUrlTest.kt:251-265`).
- gRPC: a real in-process server/channel (`InProcessServerBuilder`/`InProcessChannelBuilder.directExecutor().intercept(GrpcInterceptor { GrpcTracker(fakeConnection) })`) with the `Greeter.SayHello` test proto (`NIT/GrpcTest.kt:56-61`, `testProto/test-server.proto`).

**What's covered** (tests per file from grep of `@Test`):

| Test file | Count | Coverage |
|---|---|---|
| `OkHttp3Test` | 6 | get / post / intercept / duplex / one-shot / abort, with exact event counts |
| `OkHttp2Test` | 4 | get / post / intercept / abort |
| `HttpUrlTest` | 5 | GET, POST, rule add/update/reorder/remove end-to-end, `getResponseCode` before connect, failed connection |
| `WrappedUrlConnectionTest` | 2 | nullable APIs; intercepted headers via `getHeaderField(n)`, `getHeaderFieldInt/Long/Date`, `responseCode`/`responseMessage` |
| `TrackedHttpURLConnectionTest` | 10 | each getter starts tracking |
| `InterceptionRuleTest` | 21 | matching, wildcards, status/header/body transformations incl. gzip, per-method criteria |
| `InterceptionRuleServiceTest` | 6 | ordering, overwrite, remove, disabled rules |
| `StreamReporterTest` | 8 | caps and OOM paths |
| `InputStreamTrackerTest` | 5 | read variants, skip, big skip |
| `NetworkInspectorTest` | 8 | speed sampling and zero suppression; hook-registration logs and graceful `NoClassDefFoundError`; gRPC install-once |
| `GrpcTest` | 2 | exact event content; grouping by connection id |

**Not covered** (by inspection of the list above): `enqueue`/async threads; real OkHttp chains (BridgeInterceptor/gzip, redirects, cache); HTTP errors ≥400 on HttpURLConnection; `getErrorStream`; OkHttp 3.12 binary compatibility; 4.x `newBuilder()` duplication; WebSockets; mid-body failures; Content-Length after body rules.

A manual test app exists at `TB:app-inspection/test-app` ("can be used to manually test App Inspectors…Network including: Native Java, OKHttp2, OKHttp3", `README.md`). Its OkHttp3 client uses `execute()` and tests `Accept-Encoding` variants, one-shot, duplex and multipart posts (`.../network/OkHttp3.kt`).

---

## Design implications for netinspect

### Copy (proven by Studio's code)

1. **Hook OkHttp through the getter exits.** Use `okhttp3.OkHttpClient.networkInterceptors()Ljava/util/List;` and return a *new* list (never mutate OkHttp 3+ immutable lists). This automatically covers clients created before attach, because OkHttp calls the getter on every call in 3.12, 3.14, 4.12 and 5.5 (§2.3). Do the same for `eventListenerFactory()Lokhttp3/EventListener$Factory;`: `RealCall` calls `invokevirtual okhttp3/OkHttpClient.eventListenerFactory` in all three jars, per call (`OK3.12:RealCall.java:75`, `OK4.12:RealCall.kt:68`, `OK5.5:RealCall.kt:77`).
2. **Two-classloader split for attach mode.**
   - A tiny dispatcher on the boot classpath (no OkHttp references), added with `AddToBootstrapClassLoaderSearch`.
   - The capture runtime dex in a `DexClassLoader` whose **parent is the app's classloader**, so it can link against the app's own `okhttp3.*` (`transport_agent.cc:53-58`; `InspectorContext.java:144-153`; `AppInspectionService.java:451-472`).
   - Cache that classloader per dex path (b/187342510).
3. **Register native transforms once; dispatch to a mutable hook list.** Leave bytecode installed after detach and make the dispatcher a pass-through. Retransform with a dedicated `jvmtiEnv`, a hidden-API silencer, CFLH always on for P+, and re-apply all transforms for a class inside CFLH (`AppInspectionService.java:318-348`; `app_inspection_service.cc:220-316`).
4. **Tee the response body as the app reads it** (a `FilterInputStream`/`Source` wrapper), never pre-read it. Keep the 10 MiB cap idea and OOM guards.
5. **Never break the app.** Put try/catch around every tracking step and treat `LinkageError` specially with a clear log (`OkHttpUtils.kt:67-72`). Rate-limit logs (`Logs.kt:29-52`).
6. **Header-only interception for java.net.** Wrap `HttpURLConnection`/`HttpsURLConnection` by subclassing and delegating everything. Copy the "first touch sends RequestStarted" state machine and the POST fix-up (§5).
7. **Speed graph from TrafficStats** at 500 ms with zero suppression, if we want it. **Label it as whole-app uid traffic** (TCP+UDP, all interfaces, not only captured HTTP), as Studio's IDE comments do.
8. **Test approach.**
   - A fake hook registry with `triggerExitHook`, a capturing fake transport, a scripted TrafficStats fake, fake `Interceptor.Chain`s, and Robolectric for Android types.
   - An in-process gRPC server if we do gRPC.
   - **Add** MockWebServer end-to-end tests with real OkHttp 3.12 / 3.14 / 4.12 / 5.x clients (Studio has none).

### Do differently (and why)

1. **Real caller thread and stack via EventListener.** Studio's trace for `enqueue()` is the dispatcher's stack (§3.5). `EventListener.callStart` runs on the caller thread for both `execute()` and `enqueue()`, so capture thread + stack there. Correlate with the interceptor through `chain.call()`, which exists in 3.12, 4.12 and 5.5 (javap). Use `requestBodyEnd`, `responseHeadersStart`, `dns*`, `connect*`, `secureConnect*`, `callEnd`/`callFailed` (and `canceled` on 4.x+) for phase timings Studio lacks.
2. **EventListener wrapping is the riskiest piece.**
   - `EventListener` is an **abstract class** in every version (javap), so `java.lang.reflect.Proxy` cannot be used.
   - The API grew from **20 → 29 → 33** public callbacks (3.12 → 4.12 → 5.5). 4.12 added `cacheConditionalHit, cacheHit, cacheMiss, canceled, proxySelectEnd, proxySelectStart, requestFailed, responseFailed, satisfactionFailure`; 5.5 added `dispatcherQueueEnd, dispatcherQueueStart, followUpDecision, retryDecision` (javap diff).
   - A delegating wrapper compiled against an older API silently **drops** newer callbacks for the app's own listener.
   - Mitigations: compile the wrapper against the newest OkHttp and override every callback (unknown overrides are simply never called on older runtimes); prefer `EventListener.plus()` (5.3+) or `Call.addEventListener` (5.4+) when present.
3. **Idempotent hooks.**
   - Before prepending, check whether the list already contains our interceptor, and whether the factory is already ours, **by class name** (class identity can differ across reloads).
   - On OkHttp 4/5, `newBuilder()` copies both through the hooked getters (bytecode-verified), so without this every derived client gets duplicates. It also means baked-in instances outlive detach: our interceptor/listener must check a global "enabled" flag and become pass-through.
   - Studio relies on a stack-string check and leaks instances (§2.2).
4. **Don't alter responses in capture-only mode.**
   - Studio always rebuilds headers from the lowercased `toMultimap()`, adds a `response-status-code` header the app can see, and replaces the body (§3.3). When no rule matches, only swap the body for a tee: `response.newBuilder().body(tee).build()`.
   - Carry status code, reason and protocol as proto fields, not pseudo-headers or a `"null"` key.
   - Don't round-trip headers through `Headers.of`/`headersOf`, which throw on the non-ASCII values OkHttp accepts leniently.
5. **Request bodies: tee on write, don't write twice.**
   - Studio calls `body.writeTo()` into a null sink **before** `proceed()`. This doubles the work, drains non-repeatable bodies that don't override `isOneShot()`, and stamps "request completed" before sending.
   - Instead, replace the request body inside the network interceptor with a forwarding body that tees bytes as `CallServerInterceptor` writes them. Keep `contentType()`/`contentLength()` identical so Bridge's Content-Length still holds. This gives the true upload end and handles one-shot bodies.
   - Guard `isDuplex()`/`isOneShot()`: they don't exist in OkHttp 3.12 (`OK3.12:RequestBody.java` has neither). Studio's unconditional call breaks tracking for every body request on 3.12.x.
6. **Stream payloads and report outcomes precisely.**
   - Send body chunks incrementally with a total cap and an explicit `truncated` flag. Stop appending after the cap (Studio's drop-then-append can corrupt).
   - Emit "response body complete" on EOF, not only on `close()`.
   - Emit a failure with the error message and kind (IO / canceled / timeout) when a read throws. Studio drops error strings (`ConnectionReporter.kt:111-117`) and marks partially-read closed bodies as completed.
   - Add a GC/idle timeout for bodies that are never closed.
7. **java.net fixes.**
   - Hook **both** `openConnection()` and `openConnection(Ljava/net/Proxy;)` (the Proxy overload bypasses the no-arg one, `URL.java:1038-1057`).
   - For ≥400 responses, read `responseCode`/`headerFields` **before** `getInputStream()` so status and headers are reported even when it throws `FileNotFoundException` (`HttpURLConnectionImpl.java:250-251`). Tee `getErrorStream()`.
   - Emit `Closed` on `disconnect()` even if no stream was opened (Studio's `disconnect()` tracker is empty).
   - Report the request payload when the response is first touched, even if the app never closed the output stream.
8. **Rules engine.**
   - Keep case-insensitive header maps through every transformation (Studio's `toMutableMap()` loses this for java.net).
   - Rewrite `Content-Length` (or drop it and use -1) whenever the body changes.
   - Support or refuse `br`/`deflate`/`zstd`.
   - Treat PLAIN replacement text literally (escape `$` and `\`).
   - Apply rules without a global lock around body reads.
   - Normalize default ports (80/443) before port matching.
   - Add header removal.
   - Report "rule applied" for OkHttp too (Studio only does it for java.net) and keep the original response for side-by-side display.
   - Note that network-interceptor rules cannot short-circuit (proceed-once). Mocking/offline responses would need an application interceptor or a separate mechanism.
9. **Host-side decoding.** Network interceptors see raw compressed bodies when OkHttp added `Accept-Encoding: gzip` (Bridge decompresses later). The Rust UI should decode gzip and br like Studio's IDE does, and ideally deflate/zstd.
10. **Coverage gaps to design for.**
    - WebSockets: network interceptors are skipped for them. An `eventListenerFactory()` exit hook would still see the upgrade call's lifecycle, because `RealCall` calls the getter even on the `EventListener.NONE` WebSocket client **(analysis)**.
    - Cache hits: invisible to network interceptors; use `cacheHit`/`cacheMiss` events on 4.x+.
    - Redirects/retries: separate network attempts. Model "call id" and "attempt id" separately.

### Risks

- **Minified apps (R8)**: class/method names and getter inlining can make hooks silently absent. Studio just logs "class is omitted by app reduce". Attach mode should report per-hook success, like `StartInspectionResponse`'s booleans, and library mode is the fallback.
- **Multiple or relocated OkHttp copies** (SDKs shading OkHttp) aren't matched by name-based hooks. Studio resolves classes through the first `Application` classloader only.
- **Binary compatibility across OkHttp 3.12–5.5**: use only APIs present in all versions (javap confirms static `ResponseBody.create(MediaType,long,BufferedSource)`, static `Headers.of(String...)`, and `Chain.call()`); version-guard everything else.
- **Coexistence with Studio's inspector**: it rewrites OkHttp response headers and adds its own interceptors; ordering between agents is undefined **(analysis)**. Warn when both are active.
- **Platform limits**: API 26+ (O) for the JVMTI approach, extra overhead from CFLH before P, and a different heap-walk before Q. Avoid Java 9 APIs (`readAllBytes`, `getPackageName`) in on-device code; their Android API levels are unverified here.

---

## UNVERIFIED / not found

1. **Android API levels** for `InputStream.readAllBytes()` and `Class.getPackageName()`. developer.android.com method tables didn't render via WebFetch; libcore Javadoc only says `@since 9`.
2. **Public review history/rationale** for the inspector. Gerrit REST change queries for `OkHttp3Interceptor.kt`, `NetworkInspector.kt`, `network-inspector.proto`, `TrackedHttpURLConnection.kt` returned no changes; development appears to be mirrored.
3. **Legacy `profiler/app/perfa-okhttp` BUILD files**: 404 on `mirror-goog-studio-master-dev`. Its Java sources under `profiler/app/perfa-okhttp/src/main/java/com/android/tools/profiler/agent/okhttp/` were found. I read `OkHttp3Interceptor.java` and `OkHttpUtils.java` (and the `LP/` wrapper/tracker files); `OkHttp2Interceptor.java` was downloaded but not read. Where the legacy native agent registered its JVMTI hooks was not located.
4. **Runtime effects labeled (analysis)** were not reproduced on a device. These include: duplicate interceptors after `newBuilder()`, NoSuchMethodError on OkHttp 3.12, HTTP ≥400 reporting for java.net, dangling entries, Content-Length breakage, and connection-leak consequences of `BodyReplaced`.
5. **R8 behavior** (whether release builds inline or rename `networkInterceptors()`) was not checked.
6. **Where OkHttp 3.14 creates its `EventListener`**: not read. 3.14 `RealCall.enqueue` calls `transmitter.callStart()` (`OK3.14:RealCall.java:87-92`); `Transmitter.java` itself was not read.
