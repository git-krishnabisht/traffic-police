# gRPC capture for traffic-police: verified source study

Date: 2026-10-07. Read-only research; nothing was installed and no device, emulator or adb was used.
Every claim below cites source that was read during this task. **(analysis)** marks a conclusion drawn by
combining cited code paths (not executed). **UNVERIFIED** marks things that could not be checked from source.

## Source legend

| Alias | What | Revision |
|---|---|---|
| `GJ` | `grpc/grpc-java`, blobless clone from GitHub | tag `v1.84.1` (`c0ba89f3`, 2026-10-07) unless a tag is named, e.g. `GJ@v1.32.1` |
| matrix | the same repo, `git show <tag>:<path>` at v1.10.0, 1.14, 1.18, 1.20, 1.21, 1.25, 1.30, 1.32.1, 1.33–1.40, 1.42, 1.45, 1.48, 1.50, 1.53, 1.55–1.60, 1.62.2, 1.63, 1.64, 1.66, 1.68–1.70, 1.72, 1.74, 1.75, 1.77, 1.78, 1.80, 1.82, 1.84.1 | |
| `TB` | AOSP `platform/tools/base` (studio-main), local clone `/Users/krishnabisht/dev/personal/base` | `ddaffc19` (2026-08-17). Line numbers match the `tools-base@11ff885` cited in `docs/research/01-studio-network-inspector.md` |
| `NI` | `TB:app-inspection/inspectors/network/src/com/android/tools/appinspection/network` | |
| `IDEA` | AOSP `platform/tools/adt/idea`, local clone `/Users/krishnabisht/dev/personal/adt/idea` | `308abad8` (2026-08-17) |
| `NM`/`NV` | `IDEA:app-inspection/inspectors/network/{model,view}/src/com/android/tools/idea/appinspection/inspectors/network/{model,view}` | |
| `AX` | `androidx/androidx` `inspection/inspection/src/main/java/androidx/inspection/ArtTooling.java` | GitHub HEAD, fetched 2026-10-07 |
| `FS` | `firebase/firebase-android-sdk` `firebase-firestore/.../remote/GrpcCallProvider.java` | `bb270a6c` (2026-07-22) |
| `GK` | `grpc/grpc-kotlin` `stub/src/main/java/io/grpc/kotlin/ClientCalls.kt` | `43e1c033` (2025-08-30) |
| `SPEC` | `grpc/grpc` `doc/PROTOCOL-HTTP2.md` | `cf61c7d6` (2025-04-17) |
| `R8` | `r8.googlesource.com/r8` `src/main/java/com/android/tools/r8/ir/desugar/BackportedMethodRewriter.java` | `main`, fetched 2026-10-07 |
| `ADR` | developer.android.com reference pages (`java.io.InputStream`, `java.lang.Class`) | fetched 2026-10-07 |
| `TP` | this repo (`docs/ARCHITECTURE.md` §4.1–4.7, `docs/PROTOCOL.md`, `android/attach-agent/src/main/cpp/agent.cpp`, `.../internal/AttachEntry.java`, `host/crates/core/src/decode/protobuf.rs`) | working tree, 2026-10-07 |

The AOSP Gerrit and the kroune mirror were not needed: local AOSP clones of `tools/base` and `tools/adt/idea` exist.

---

## 0. Answers in brief

1. **Transport.** Android apps use **grpc-okhttp** (`io.grpc.okhttp`). It never touches `okhttp3`: it ships a fork of OkHttp **2.7** HTTP/2 framing under `io.grpc.okhttp.internal`, depends on Okio, and opens raw `Socket`/`SSLSocket`s. Our `okhttp3.OkHttpClient` and `URL.openConnection` hooks therefore never see gRPC. Cronet transport uses Chromium `BidirectionalStream` (also invisible). The layer to hook is the **gRPC API** (`ClientInterceptor` / `Channel` / `ClientCall`). Clients that are *not* grpc-java (Square Wire's `GrpcClient`, Connect-Kotlin) run on `okhttp3` and are already visible to our OkHttp capture, without trailers.
2. **Studio** adds a `ClientInterceptor` by **entry+exit hooks on six static builder factories** (`forAddress`/`forTarget` of `ManagedChannelBuilder`, `AndroidChannelBuilder`, `OkHttpChannelBuilder`), calling `builder.intercept(...)` on the outermost exit. Channels that already exist are patched by a **heap walk plus reflection** that replaces the private field `ManagedChannelImpl.interceptorChannel`. Message bytes come from `marshaller.stream(message).readAllBytes()`. Headers come from `Metadata.keys()` with ASCII keys. Status is recorded as the code name only. It never wraps a `ManagedChannel`, so it never meets the `ManagedChannel`/`Channel` type problem.
3. **Design.**
   - Library mode: `TrafficPolice.grpcInterceptor()`, added first via `ManagedChannelBuilder.intercept(...)`.
   - Attach mode, new channels: the cleanest **exit-only** hook is the builder's package-private `getEffectiveInterceptors` (returns a `List`, the analogue of `networkInterceptors()`). It has three signatures by version (§3.2).
   - Attach mode, channels that existed before attach: use Studio's heap-walk field swap, or entry+exit hooks on `ManagedChannelImpl$RealChannel.newCall`.
   - The static factories miss `Grpc.newChannelBuilder(...)` and the `ChannelCredentials` overloads.
4. **Fidelity.**
   - URL: `https://<callOptions.authority ?: channel.authority()>/<fullMethodName>`.
   - At interceptor level you see the app's `Metadata` only. `content-type`, `te`, `user-agent`, the pseudo-headers, `grpc-timeout`, `grpc-accept-encoding`/`grpc-encoding` and **CallCredentials headers** (e.g. `authorization`) are added below. A `ClientStreamTracer` (`streamCreated`, 1.40+) sees the per-attempt wire Metadata, including the credentials headers.
   - `:status`, `grpc-status` and `grpc-message` are stripped before `onHeaders`/`onClose`. Rebuild them from `Status`.
   - Bodies: re-marshal each message with the method's marshaller and frame it yourself as `[0][len BE32][bytes]`. A second `stream()` call is safe for protobuf (grpc itself re-marshals per retry attempt).
5. **Threading.**
   - `newCall`/`start`/`sendMessage` run on the caller's thread for blocking, async and Kotlin stubs.
   - Listener callbacks run on the call executor: the channel's or `CallOptions` executor (default `grpc-default-executor-N`); for blocking stubs, the blocked caller thread; with `directExecutor()`, the transport thread.
6. **Gotchas.** `-bin` keys, `keys()` losing order, reading `Metadata` after `start()`, trailers-only responses, locally generated statuses, per-attempt header copies, R8 renaming, and dedupe (§6).

---

## 1. Which transport, and therefore which layer

### 1.1 grpc-okhttp does not use okhttp3

- It has its own HTTP/2 framer and helpers forked from OkHttp 2, compiled in from `third_party`:
  - `GJ:okhttp/build.gradle`: `main { java { srcDir "${projectDir}/third_party/okhttp/main/java" } }`.
  - The forked package is `io.grpc.okhttp.internal.{framed,proxy,…}`. Headers say e.g. "Forked from OkHttp 2.7.0 com.squareup.okhttp.Headers" (`GJ:okhttp/third_party/okhttp/main/java/io/grpc/okhttp/internal/Headers.java:18`).
- Dependencies (`GJ:okhttp/build.gradle`):
  ```gradle
  implementation project(':grpc-util'), project(':grpc-core'), libraries.okio, libraries.guava, libraries.perfmark.api
  // Make okhttp dependencies compile only
  compileOnly libraries.okhttp
  ```
  `libraries.okhttp` is `com.squareup.okhttp:okhttp:2.7.5` (OkHttp **2**, `GJ:gradle/libs.versions.toml:113`).
- History (matrix):
  - v1.0.0 to v1.45: OkHttp 2 (`2.5.0` → `2.7.4`) was a runtime dependency.
  - v1.46.0 onward: `compileOnly`.
  - In no version is it `okhttp3`.
- The only `com.squareup.okhttp` uses in main code are `ConnectionSpec`/`TlsVersion`/`CipherSuite` conversions for the public `connectionSpec(com.squareup.okhttp.ConnectionSpec)` API (`GJ:okhttp/src/main/java/io/grpc/okhttp/OkHttpChannelBuilder.java:441`, `Utils.java:68-83`).
- `okhttp3` appears once, in a comment link (`OkHttpClientTransport.java:1355`).
- Request headers are built by its own `io.grpc.okhttp.Headers.createRequestHeaders` (§4.2) and written with the forked `frameWriter.synStream(...)` (`OkHttpClientStream.java:251`).

Consequence **(analysis)**: none of our existing hooks (`okhttp3.OkHttpClient#networkInterceptors/eventListenerFactory`, `URL#openConnection`) runs for grpc-okhttp traffic. The capture point must be grpc-java's own API.

### 1.2 Other transports and clients

- **Default on Android is OkHttp.**
  - `OkHttpChannelProvider.priority()` returns `InternalServiceProviders.isAndroid(getClass().getClassLoader()) ? 8 : 3` (`GJ:okhttp/.../OkHttpChannelProvider.java:40`).
  - `NettyChannelProvider.priority()` returns `5` (`GJ@v1.84.1:netty/.../NettyChannelProvider.java:36-37`).
  - `ManagedChannelBuilder.forTarget/forAddress` go through `ManagedChannelProvider.provider()` (`GJ:api/.../ManagedChannelBuilder.java:43-45,90-92`; `ManagedChannelProvider.java:42-43`), so on Android they return an `OkHttpChannelBuilder`.
- **Cronet** (`io.grpc.cronet.CronetChannelBuilder.forAddress(host, port, CronetEngine)`, `GJ:cronet/.../CronetChannelBuilder.java:63`): streams are `org.chromium.net.BidirectionalStream` (`CronetClientStream.java:54,87`). This is native networking, invisible to our hooks, but it still goes through `ManagedChannelImpl`, so API-level capture works.
- **Netty** (`grpc-netty[-shaded]`): possible but loses to OkHttp on Android by priority. How often apps ship it is **UNVERIFIED**.
- **Binder** (`grpc-binder`) is on-device IPC. **UDS** (`io.grpc.android.UdsChannelBuilder`) uses the OkHttp transport over local sockets. **In-process** is for tests. All go through `ManagedChannelImplBuilder` (§3.2).
- **Real-world example: Firebase Firestore.**
  - It builds its channel internally with `OkHttpChannelBuilder.forTarget(databaseInfo.getHost())`, then `AndroidChannelBuilder.usingBuilder(channelBuilder).context(context).build()` (`FS:110,132-135`).
  - It issues calls with `channel.newCall(methodDescriptor, callOptions)` directly, not via stubs (`FS:143`). The call is made on Firestore's async-queue executor.
  - The app has no public way to add an interceptor to this channel; the only override is a private static `overrideChannelBuilderSupplier` (`FS:49,102-103`). So **library mode cannot see Firestore; only attach-mode hooks can (analysis)**.
- **Not grpc-java.**
  - Square Wire's `GrpcClient` imports `okhttp3.Call`, `okhttp3.OkHttpClient`, `okhttp3.Protocol.HTTP_2` (`square/wire wire-grpc-client/src/jvmMain/kotlin/com/squareup/wire/GrpcClient.kt:24-27`).
  - Connect-Kotlin's `ConnectOkHttpClient` is built on `okhttp3` (`connectrpc/connect-kotlin okhttp/src/main/kotlin/com/connectrpc/okhttp/ConnectOkHttpClient.kt:30-39`).
  - Both are already captured as HTTP/2 POSTs with gRPC-framed bodies, which the host decodes by `Content-Type`. Their `grpc-status` lives in HTTP/2 trailers, which `PROTOCOL.md` has no field for today (grep: no "trailer" anywhere in `capture-core`, `PROTOCOL.md` or `host/crates/proto`).

---

## 2. How Android Studio captures gRPC

### 2.1 Hooks it registers (ArtTooling)

The hook API is `AX:ArtTooling.java`:
- `findInstances(Class<T>)` (`:36`).
- `EntryHook.onEntry(Object thisObject, List<Object> args)` (`:43-51`).
- `registerEntryHook(Class, String originMethod, EntryHook)` (`:69`).
- `ExitHook<T>.onExit(T result): T` (`:77-85`).
- `registerExitHook(...)` (`:103`).
- Natively, entry hooks are Studio's own `ArrayParamsEntryHook` (an `Object[]` of arguments). Exit hooks are slicer `ExitHook` with `ReturnAsObject | PassMethodSignature` (`TB:app-inspection/native/include/app_inspection_transform.h:41-59`), the same tweak our agent uses (`TP:agent.cpp` `InstrumentClass`).

`NI/NetworkInspector.kt`:

```kotlin
private const val GRPC_CHANNEL_CLASS_NAME = "io.grpc.internal.ManagedChannelImpl"   // :61
private const val GRPC_CHANNEL_FIELD_NAME = "interceptorChannel"                     // :62
  /**
   * A list of gRPC specific hooks.
   *
   * TODO(b/313873107): Find a safe way to register gRPC hooks. Note that we only hook `AndroidChannelBuilder.forTarget()` because the
   *   implementation of `forAddress` calls `forTarget` and would result in double registration.
   */
  private val grpcHooks =                                                             // :77-85
    listOf(
      GrpcHook("io.grpc.ManagedChannelBuilder", "forAddress(Ljava/lang/String;I)Lio/grpc/ManagedChannelBuilder;"),
      GrpcHook("io.grpc.ManagedChannelBuilder", "forTarget(Ljava/lang/String;)Lio/grpc/ManagedChannelBuilder;"),
      GrpcHook("io.grpc.android.AndroidChannelBuilder", "forAddress(Ljava/lang/String;I)Lio/grpc/android/AndroidChannelBuilder;"),
      GrpcHook("io.grpc.android.AndroidChannelBuilder", "forTarget(Ljava/lang/String;)Lio/grpc/android/AndroidChannelBuilder;"),
      GrpcHook("io.grpc.okhttp.OkHttpChannelBuilder", "forAddress(Ljava/lang/String;I)Lio/grpc/okhttp/OkHttpChannelBuilder;"),
      GrpcHook("io.grpc.okhttp.OkHttpChannelBuilder", "forTarget(Ljava/lang/String;)Lio/grpc/okhttp/OkHttpChannelBuilder;"),
    )
  /** When hooking channel builders, keep track of depth of chained calls, so we only install the hook once. For example,
   * `AndroidChannelBuilder` delegates to `OkHttpChannelBuilder`. */
  private var hookDepth by threadLocal { 0 }                                          // :98-102
```

Builder hooks, entry plus exit (`:333-356`):

```kotlin
      artTooling.registerEntryHook(clazz, hook.method) { _, _ -> hookDepth++ }
      artTooling.registerExitHook(
        clazz,
        hook.method,
        ArtTooling.ExitHook<ManagedChannelBuilder<*>> { channelBuilder ->
          hookDepth--
          if (hookDepth == 0) {
            channelBuilder.intercept(grpcInterceptor)
          }
          channelBuilder
        },
      )
```

Classes are loaded with `javaClass.classLoader.loadClass(hook.className)`. A `ClassNotFoundException` skips that hook (`:334-341`). A `NoClassDefFoundError` disables gRPC with "App does not use gRPC or class is omitted by app reduce" (`:292-301`).

Channels that already exist (`:303-331`). The KDoc says "This is known to be brittle but there doesn't seem to be a robust way of doing this." The code:

```kotlin
    artTooling.findInstances(ManagedChannel::class.java).forEach {
      if (it::class.java.name == GRPC_CHANNEL_CLASS_NAME) {
          val field = it::class.java.getDeclaredField(GRPC_CHANNEL_FIELD_NAME)
          field.isAccessible = true
          val channel = field.get(it) as Channel
          field.set(it, InterceptingGrpcChannel(channel, grpcInterceptor))
```

with (`:363-373`):

```kotlin
  private class InterceptingGrpcChannel(private val delegate: Channel, private val interceptor: GrpcInterceptor) : Channel() {
    override fun <Req : Any, Res : Any> newCall(methodDescriptor: MethodDescriptor<Req, Res>, callOptions: CallOptions): ClientCall<Req, Res> {
      return interceptor.interceptCall(methodDescriptor, callOptions, delegate)
    }
    override fun authority(): String = delegate.authority()
  }
```

How `findInstances` works natively (`TB:app-inspection/native/src/app_inspection_service.cc:82-185`):
- API < 29: `IterateThroughHeap` per assignable class, then tag.
- API ≥ 29: `IterateOverInstancesOfClass(clazz, JVMTI_HEAP_OBJECT_EITHER, …)`.
- Then `GetObjectsWithTags`.

So the **answer to "ManagedChannel vs Channel"**: Studio never wraps the built channel. On new channels it adds the interceptor to the *builder* (`intercept()` returns the builder, so the type is preserved). On existing channels it swaps the `Channel`-typed private field inside `ManagedChannelImpl`, so the `ManagedChannel` object, its type and its identity are unchanged. The target field is `private final Channel interceptorChannel;` (`GJ:core/.../ManagedChannelImpl.java:223`), set by `this.interceptorChannel = ClientInterceptors.intercept(channel, interceptors);` (`:638`). `newCall` reads it directly: `return interceptorChannel.newCall(method, callOptions);` (`:825-828`). The field is the same at v1.10.0, 1.21, 1.30, 1.40, 1.50, 1.60, 1.69 and 1.84.1 (matrix).

What it compiles against: `"@maven//:io.grpc.grpc-api"` (`TB:app-inspection/inspectors/network/BUILD:28`) at `io.grpc:grpc-api:1.69.1` (`TB:bazel/maven/artifacts.bzl:118`). gRPC is not bundled; it resolves to the app's copy. dex: `"--min-api 26"` (`BUILD:50`).

### 2.2 The interceptor (`NI/grpc/GrpcInterceptor.kt`)

```kotlin
  override fun <Req : Any, Res : Any> interceptCall(method: MethodDescriptor<Req, Res>, options: CallOptions, next: Channel): ClientCall<Req, Res> {
    val tracker = trackerFactory.newGrpcTracker()
    return InterceptingClientCall(tracker, method, next.newCall(method, options.withStreamTracerFactory(StreamTracer.Factory(tracker))))   // :41-47
  }
    override fun start(responseListener: Listener<Res>, headers: Metadata) {                 // :54-58
      val listener = ClientCallListener(tracker, method.responseMarshaller, responseListener)
      super.start(listener, headers)
      tracker.trackGrpcCallStarted(method.serviceName ?: UNKNOWN, method.bareMethodName ?: UNKNOWN, headers, getStackTrace(1))
    }
    override fun sendMessage(message: Req) {                                                  // :60-63
      super.sendMessage(message)
      tracker.trackGrpcMessageSent(message, method.requestMarshaller)
    }
  private class StreamTracer(private val tracker: GrpcTracker) : ClientStreamTracer() {
    override fun streamCreated(transportAttrs: Attributes, headers: Metadata) {               // :67-70
      val address: SocketAddress? = transportAttrs.get(TRANSPORT_ATTR_REMOTE_ADDR)
      tracker.trackGrpcStreamCreated(address?.toString() ?: UNKNOWN, headers)
    }
  // ClientCallListener (:77-97): onHeaders -> super then trackGrpcResponseHeaders(headers);
  //   onMessage -> super then trackGrpcMessageReceived(message, marshaller); onClose -> super then trackGrpcCallEnded(status, trailers)
```

Placement:
- New channels: the interceptor is added right after the factory returns, so it is the *first* interceptor in the builder's list. That makes it the innermost *app* interceptor (`ClientInterceptors.intercept` wraps in list order, `GJ:api/.../ClientInterceptors.java:86-91`), with census interceptors still below it (§3.2).
- Existing channels: it is *outermost* (it wraps the whole `interceptorChannel`).

### 2.3 What it records (`NI/trackers/GrpcTracker.kt`)

- **One "connection" per call**: `private val connectionId = ConnectionIdGenerator.nextId()` (`:41`), shared counter with HTTP.
- **URL parts**: service = `MethodDescriptor.getServiceName()` (`@since 1.21.0`), method = `getBareMethodName()` (`@since 1.33.0`) (`GJ:api/.../MethodDescriptor.java:261,272`). There is no authority; the address is the transport's remote socket address from the tracer.
- **Message bytes** (`:152-161`):
  ```kotlin
  private fun <T> Marshaller<T>.createGrpcPayload(message: T): GrpcPayload.Builder {
    val msg: Any = message ?: return GrpcPayload.newBuilder()
    val className = msg::class.java.name
    val text = when { message.isProto() -> message.toProtoText()  else -> message.toString() }
    return GrpcPayload.newBuilder().setBytes(ByteString.copyFrom(stream(message).readAllBytes())).setType(className).setText(text)
  }
  private fun Any.toProtoText() = "# proto-message: ${this::class.java.simpleName}\n\n$this"
  private fun Any.isProto(): Boolean { return this::class.java.superclass.packageName.startsWith("com.google.protobuf") }
  ```
  So it stores the bytes from `marshaller.stream(message)`, re-marshalled for both directions; the class name; and `toString()` as "text proto".
- **Headers** (`:144-150`):
  ```kotlin
  private fun Metadata.toGrpcMetadata(): List<GrpcMetadata> {
    return keys().map {
      val key = Metadata.Key.of(it, Metadata.ASCII_STRING_MARSHALLER)
      val values = getAll(key)?.toList() ?: emptyList()
      GrpcMetadata.newBuilder().setKey(it).addAllValues(values).build()
    }
  }
  ```
- **Status/trailers** (`:104-114`): `setStatus(status.code.toString()).addAllTrailers(trailers.toGrpcMetadata())`, plus `setError(status.cause?.stackTraceToString())` when there is a cause. `Status.getDescription()` (the `grpc-message`) is **not recorded**.
- **Timing**: each event is stamped `System.nanoTime()` when sent (`:129-137`).
- **Threads**: a `ThreadData(id, name)` event whenever the reporting thread differs from the previous one (`AtomicReference`, `:121-127`).
- **Stack**: `getStackTrace(1)`, taken in `start()` after `super.start()`. It drops leading `com.android.tools.appinspection` frames, then one more (`TB:app-inspection/inspectors/common/src/com/android/tools/appinspection/common/Stacktrace.kt`).
- **Wire schema** (`TB:app-inspection/inspectors/network/resources/proto/network-inspector.proto`, `message GrpcEvent`):
  - `GrpcCallStarted{service, method, request_headers, trace}`
  - `GrpcPayload{optional bytes bytes; string type; string text}`
  - `GrpcMessageSent/Received{payload}`
  - `GrpcStreamCreated{address, request_headers}`
  - `GrpcResponseHeaders`
  - `GrpcCallEnded{string status; optional string error; repeated GrpcMetadata trailers}`
  - `ThreadData grpc_thread = 8`
- **Expected event order** (`TB:.../testSrc/.../GrpcTest.kt`, in-process server, `directExecutor`): `STREAM_CREATED, THREAD, CALL_STARTED, MESSAGE_SENT, RESPONSE_HEADERS, MESSAGE_RECEIVED, CALL_ENDED`. The `grpc_call_started` headers in that test include `grpc-accept-encoding: gzip` and `user-agent: grpc-java-inprocess/…`, i.e. values that gRPC added *after* the app called `start()`. This shows Studio reads `Metadata` after handing it to gRPC (see §6).

### 2.4 How the IDE presents it

- **Enabled by default.** `StudioFlags.NETWORK_INSPECTOR_GRPC` (`IDEA:android-common/src/com/android/tools/idea/flags/StudioFlags.java:1778`) is set `network.inspector.grpc=COMPLETE:2023` (`IDEA:android-common/flags/resources/feature_flags.txt:126`). `DataHandler.handleGrpcEvent` drops events only when the flag is off (`NM/DataHandler.kt:126-129`).
- **Model** (`NM/connections/GrpcData.kt`):
  - `transport = "gRPC"`, `schema = "grpc"`, `url = "$schema://$address/$path"`, `path = name = "$service/$method"` (`:57-70`).
  - `address` is the tracer's `SocketAddress.toString()`.
  - Request headers = call-started headers + stream-created headers, merged into a case-insensitive `TreeMap` (`:80,104,204`), so order and duplicates are lost.
  - `withGrpcMessageSent`/`Received` **overwrite** `requestPayload`/`responsePayload` (`:85-118`), so for a stream only the **last** message per direction is shown.
  - Times: `requestStart` = call started, `requestComplete` = last sent, `responseStart` = stream created, `responseComplete` = last received, `connectionEnd` = call ended.
  - `status` = code name, shown in the Status column (`NV/connectionsview/ConnectionColumn.kt:97-98`).
- **Body view** (`NV/details/GrpcDataComponentFactory.kt`):
  - JSON or XML if the bytes parse as such.
  - Otherwise, if the text starts with `# proto-message:`, a text-proto view with a "View Proto Text"/"View Raw" switch. It also searches the project's `.proto` files for `message <Type> {` and adds `# proto-file:` (`:55-96`).
  - Otherwise raw/text (`:98-110`).
  - A trailers panel (`:112-117`).
- **Export**: JSON with `"transport" to "gRPC"`, service, method, `stack-trace` and headers (`NV/connectionsview/ConnectionsView.kt:209-…`).

### 2.5 Defects visible in Studio's code (all **analysis**, not run)

1. **Any `-bin` metadata loses the whole event.** `Metadata.Key.of(name, ASCII_STRING_MARSHALLER)` throws for names ending in `-bin`:
   ```java
   Preconditions.checkArgument(!name.endsWith(BINARY_HEADER_SUFFIX), "ASCII header is named %s.  Only binary headers may end with %s", ...)
   ```
   (`GJ:api/.../Metadata.java:966-975`). Each `track*` catches `Throwable` and logs, so the call-started, stream-created, response-headers or **call-ended** event is dropped.
   - `grpc-status-details-bin` (rich errors, `SPEC:113`) in trailers therefore means the IDE never sees the call end.
2. **gRPC < 1.33 breaks the app's call.** `method.bareMethodName` (1.33+) is evaluated in `start()` *outside* the tracker's try/catch (`GrpcInterceptor.kt:57`), so a `NoSuchMethodError` propagates into the app after `super.start()`.
3. **Payload capture fails below API 33.**
   - `InputStream.readAllBytes()` is "Added in API level 33" and `Class.getPackageName()` is "Added in API level 31" (`ADR`).
   - The inspector is dexed with `--min-api 26` and nothing else (`BUILD:49-50`).
   - `BackportedMethodRewriter` has no `InputStream` or `getPackageName` entries (`R8`, grep).
   - So message events throw `NoSuchMethodError` (caught: event lost) on API 26–32.
4. **`Metadata` is read after `super.start()`**, violating the `ClientCall` contract. The same rule appears for `start(listener, headers)` (`ClientCall.java:180`) and for `Listener.onHeaders`: "Since Metadata is not thread-safe, the caller must not access (read or write) headers after this point" (`GJ:api/.../ClientCall.java:116-124`). gRPC keeps mutating that object:
   - `ClientCallImpl.prepareHeaders`, which for a not-yet-resolved channel's `PendingCall` runs later on another thread.
   - With retries disabled, also `setDeadline` and the OkHttp transport's `stripNonApplicationHeaders` (§4.2).
   - With retries on (default since 1.40) the transport works on a per-attempt copy.
5. **`hookDepth` leaks.** It is incremented on entry but decremented only on normal exit. A factory that throws (e.g. `ProviderNotFoundException`) leaves the depth > 0 on that thread forever, disabling interception there.
6. **Missed builder paths.** Constructor-based paths have no hooked factory: `Grpc.newChannelBuilder(target, creds)` → `ManagedChannelRegistry.newChannelBuilder` → `provider.newChannelBuilder(...)` → `new OkHttpChannelBuilder(target, creds, …)` (`GJ:api/.../ManagedChannelRegistry.java:157-210`, `GJ:okhttp/.../OkHttpChannelProvider.java:55-77`). Also missed: `OkHttpChannelBuilder.forTarget(String, ChannelCredentials)` / `forAddress(String,int,ChannelCredentials)` (1.34+), Cronet/Binder/InProcess builders.
7. **R8.** Hooks are by exact class name; the existing-channel patch checks `io.grpc.internal.ManagedChannelImpl` by name. Both fail in minified builds that rename gRPC (see §3.4).
8. **Untested.** No test covers the existing-channel patch (grep of `testSrc` for `interceptorChannel`/preexisting: none). The install-once test only drives the fake hook registry (`NetworkInspectorTest.kt:194-211`).

---

## 3. Design for traffic-police

### 3.1 Library mode

```kotlin
val channel = OkHttpChannelBuilder.forTarget("api.example.com:443")   // or ManagedChannelBuilder / AndroidChannelBuilder / Grpc.newChannelBuilder
    .intercept(TrafficPolice.grpcInterceptor())   // add it FIRST: interceptors added later run before it, so it sees their headers
    .build()
```

**Order.**
- "Interceptors run in the reverse order in which they are added" (`GJ:api/.../ManagedChannelBuilder.java:146-169`, both `intercept` overloads).
- `ClientInterceptors.intercept(channel, list)` wraps in list order, so the first in the list is innermost: "The last interceptor will have its interceptCall called first" (`ClientInterceptors.java:79-91`).
- Added first, ours sees what other interceptors added and sits closest to the transport, the gRPC equivalent of OkHttp's network-interceptor position.

**Public API.**
- `public static io.grpc.ClientInterceptor grpcInterceptor()` in `TrafficPolice`. gRPC types appear only in this signature, like OkHttp's (`TP:ARCHITECTURE.md` §4.1).
- `capture-noop` returns a pass-through `next.newCall(method, callOptions)`.
- `compileOnly` grpc-api. Add `-dontwarn io.grpc.**` next to the existing `-dontwarn okhttp3.**` (`TP:android/capture/consumer-rules.pro`).

**Optional `TrafficPolice.wrap(Channel)`.** `ClientInterceptors.intercept(channel, grpcInterceptor())` returns a `Channel`, usable for stubs. The app must keep the `ManagedChannel` for shutdown, and the interceptor is outermost. Second choice.

**Implementation sketch** (Java 8, compileOnly gRPC; new adapter package `io.trafficpolice.capture.grpc`, loaded only when `io.grpc.ClientInterceptor` exists, mirroring the OkHttp adapter split):

```java
public final class CaptureClientInterceptor implements ClientInterceptor {
  static final CallOptions.Key<Claim> CLAIM = CallOptions.Key.create("io.trafficpolice.grpc"); // @since 1.13
  public <Q, R> ClientCall<Q, R> interceptCall(MethodDescriptor<Q, R> m, CallOptions o, Channel next) {
    if (!runtime.active()) return next.newCall(m, o);
    ThreadStack site = ThreadStack.capture("call", depth);          // caller thread (see §5)
    Claim outer = o.getOption(CLAIM);                                // dedupe, §6.10
    Claim mine = new Claim();
    CallState st = new CallState(m, next.authority(), o, site);
    ClientCall<Q, R> inner = next.newCall(m, o.withOption(CLAIM, mine).withStreamTracerFactory(st.tracerFactory()));
    if (outer != null) outer.claimedBelow = true;                    // let the outer copy stand down
    return mine.claimedBelow ? inner : new CaptureCall<>(inner, st); // innermost copy captures
  }
}
// CaptureCall extends ForwardingClientCall.SimpleForwardingClientCall:
//   start(): snapshot headers BEFORE super.start(); wrap listener (SimpleForwardingClientCallListener)
//   sendMessage(): frame+record bytes (try/catch Throwable), then super.sendMessage()
//   halfClose()/cancel(): record, then super
// Listener: onHeaders/onMessage/onClose snapshot BEFORE delegating; never throw.
```

Extending the app's own `SimpleForwardingClientCall`/`SimpleForwardingClientCallListener` means methods newer than our compile baseline are still forwarded by the app's copy. This avoids the "compiled against an old API" problem of OkHttp's `EventListener` (`TP:ARCHITECTURE.md` §4.2).

### 3.2 Attach mode: candidate hooks and where they exist

#### Builder hierarchy by version (matrix, from `extends` clauses)

| gRPC | `OkHttpChannelBuilder extends` | `AndroidChannelBuilder extends` | `CronetChannelBuilder extends` | where `build()` creates the channel |
|---|---|---|---|---|
| 1.10 – 1.32.1 | `AbstractManagedChannelImplBuilder<…>` (non-final `public class`) | `ForwardingChannelBuilder` (from 1.14; absent at 1.10) | `AbstractManagedChannelImplBuilder` | `io.grpc.internal.AbstractManagedChannelImplBuilder.build()` |
| 1.33 | `ForwardingChannelBuilder`, delegate `ManagedChannelImplBuilder` | same | `ForwardingChannelBuilder` | `io.grpc.internal.ManagedChannelImplBuilder.build()` |
| 1.34 – 1.58 | `AbstractManagedChannelImplBuilder` (now a forwarding shim: `public ManagedChannel build() { return delegate().build(); }`, `GJ@v1.34.0:core/.../AbstractManagedChannelImplBuilder.java:259-262`) | same | `AbstractManagedChannelImplBuilder` | `ManagedChannelImplBuilder.build()` |
| 1.59 – 1.84.1 | `ForwardingChannelBuilder2` (`@since 1.59.0`) | still `ForwardingChannelBuilder` ("Not extending ForwardingChannelBuilder2 to preserve ABI", `GJ:android/.../AndroidChannelBuilder.java:157`) | `ForwardingChannelBuilder2` | `ManagedChannelImplBuilder.build()` |

So **every** builder funnels into one non-abstract method:
- 1.33+: `io.grpc.internal.ManagedChannelImplBuilder.build()`. It is `public final class ManagedChannelImplBuilder` (`GJ:core/.../ManagedChannelImplBuilder.java:76`).
- ≤1.32: `AbstractManagedChannelImplBuilder.build()`.

`AndroidChannelBuilder.build()` returns `new AndroidChannel(delegateBuilder.build(), context)` (`GJ:android/.../AndroidChannelBuilder.java:166-168`), and `ForwardingChannelBuilder2.build()` is `return delegate().build();` (`GJ:api/.../ForwardingChannelBuilder2.java:284-286`).

`ManagedChannelImplBuilder.build()` (`:775-793`):

```java
    return new ManagedChannelOrphanWrapper(new ManagedChannelImpl(
        this, clientTransportFactory, resolvedResolver.targetUri, resolvedResolver.provider,
        new ExponentialBackoffPolicy.Provider(),
        SharedResourcePool.forResource(GrpcUtil.SHARED_CHANNEL_EXECUTOR),
        GrpcUtil.STOPWATCH_SUPPLIER,
        getEffectiveInterceptors(resolvedResolver.targetUri.toString()),
        TimeProvider.SYSTEM_TIME_PROVIDER));
```

and `getEffectiveInterceptors` (`:799-…`):

```java
  List<ClientInterceptor> getEffectiveInterceptors(String computedTarget) {
    List<ClientInterceptor> effectiveInterceptors = new ArrayList<>(this.interceptors.size());
    ... // resolves InterceptorFactoryWrapper, then:
      if (statsInterceptor != null) {
        // First interceptor runs last (see ClientInterceptors.intercept()), so that no
        // other interceptor can override the tracer factory we set in CallOptions.
        effectiveInterceptors.add(0, statsInterceptor);
      ...
      if (tracingInterceptor != null) { effectiveInterceptors.add(0, tracingInterceptor); }
    return effectiveInterceptors;
```

#### Hook candidates (JNI descriptors; versions from the matrix)

| # | Target | Descriptor | Kind | Exists in | Notes |
|---|---|---|---|---|---|
| A1 | `io.grpc.internal.AbstractManagedChannelImplBuilder#getEffectiveInterceptors` | `()Ljava/util/List;` | exit; package-private `final` | 1.10 – 1.32.1 | absent in the 1.34–1.58 shim |
| A2 | `io.grpc.internal.ManagedChannelImplBuilder#getEffectiveInterceptors` | `()Ljava/util/List;` | exit; package-private | 1.33 – 1.63 | |
| A3 | same | `(Ljava/lang/String;)Ljava/util/List;` | exit | 1.64 – 1.84.1 | |
| B1 | `io.grpc.internal.ManagedChannelImplBuilder#build` | `()Lio/grpc/ManagedChannel;` | exit; public | 1.33+ | returns `ManagedChannelOrphanWrapper` (package-private, `extends ForwardingManagedChannel`) |
| B2 | `io.grpc.internal.AbstractManagedChannelImplBuilder#build` | same | exit | real ≤1.32; **forwarding shim** 1.34–1.58 | must not double-handle in 1.34–1.58 |
| B3 | `io.grpc.ForwardingChannelBuilder#build` / `ForwardingChannelBuilder2#build` | same | exit | 1.7+ / 1.59+ | forwarders, nest with B1 |
| C | `io.grpc.ManagedChannelBuilder#forAddress` `(Ljava/lang/String;I)Lio/grpc/ManagedChannelBuilder;`, `#forTarget` `(Ljava/lang/String;)…` | | exit, static | 1.0+ (`@since 1.0.0`, `ManagedChannelBuilder.java:41,88`) | Studio's choice; nested calls into OkHttpChannelBuilder statics |
| C′ | `io.grpc.okhttp.OkHttpChannelBuilder#forAddress(String,int)`, `#forTarget(String)` | `…)Lio/grpc/okhttp/OkHttpChannelBuilder;` | exit, static | all checked (1.10–1.84.1) | |
| C″ | `OkHttpChannelBuilder#forAddress(String,int,ChannelCredentials)`, `#forTarget(String,ChannelCredentials)` | `…Lio/grpc/ChannelCredentials;)Lio/grpc/okhttp/OkHttpChannelBuilder;` | exit, static | 1.34+ | Studio does not hook these |
| C‴ | `io.grpc.android.AndroidChannelBuilder#forTarget/forAddress` | `…)Lio/grpc/android/AndroidChannelBuilder;` | exit, static | present at 1.14 (absent 1.10) | ctor calls `OkHttpChannelBuilder.forTarget` reflectively ≤1.58 (`Class.forName("io.grpc.okhttp.OkHttpChannelBuilder").getMethod("forTarget", String.class).invoke(null, target)`, `GJ@v1.45.0`); 1.59+ via `InternalManagedChannelProvider.builderForTarget(OKHTTP_CHANNEL_PROVIDER, target)` (`GJ:…/AndroidChannelBuilder.java:136-142`). Nests either way |
| — | `io.grpc.Grpc#newChannelBuilder(String, ChannelCredentials)` | `…)Lio/grpc/ManagedChannelBuilder;` | static | 1.34+ | builder made by **constructor** inside the provider: no factory exit fires |
| D | `io.grpc.internal.ManagedChannelImpl$RealChannel#newCall` | `(Lio/grpc/MethodDescriptor;Lio/grpc/CallOptions;)Lio/grpc/ClientCall;` | **entry + exit** | 1.10 – 1.84.1 | below all interceptors; every call on every channel |
| E | `io.grpc.stub.AbstractStub#getChannel` | `()Lio/grpc/Channel;` | exit; `public final` | 1.10 – 1.84.1 | read per RPC by generated stubs (23 call sites in `GJ:compiler/src/test/golden/TestService.java.txt`, e.g. `getChannel().newCall(getUnaryCallMethod(), getCallOptions())`); not used by direct `newCall` callers such as Firestore |
| F | field `io.grpc.internal.ManagedChannelImpl.interceptorChannel` | `Lio/grpc/Channel;` `private final` | heap walk + reflection | 1.10 – 1.84.1 | Studio's approach for channels that already exist |

#### Assessment

- **A1/A2/A3: the best pure exit hook for new channels.**
  - It returns a `List`, so it is the exact analogue of the OkHttp `networkInterceptors()` hook (`TP:ARCHITECTURE.md` §4.7.3): return a *new* list with our interceptor at **index 0**.
  - Index 0 is innermost, below app and census interceptors and right above `RealChannel`/binlog.
  - Skip if an element's class name is already ours (library mode).
  - There is no `ManagedChannel` wrapper at all, and it covers every builder path: `forTarget`, `forAddress`, `Grpc.newChannelBuilder`, the creds overloads, constructors, Android/Cronet/Binder/InProcess/UDS.
  - It is called once per `build()`.
  - Cost: three descriptors (only one exists per version; the others report `method_not_found`, which should be reported as "not applicable" when a sibling hook installed). The hook table would hold two classes: `Lio/grpc/internal/AbstractManagedChannelImplBuilder;` and `Lio/grpc/internal/ManagedChannelImplBuilder;`.
  - Weakness: channels built before a runtime attach are not covered. With `--launch` (startup agents / `--attach-agent`) every channel is built after attach.
- **B: possible, but worse.**
  - It returns `ManagedChannel`; `ClientInterceptors.intercept(channel, ours)` returns a `Channel`, which fails the slicer `check-cast` to `Lio/grpc/ManagedChannel;`.
  - Workarounds:
    - A forwarding `ManagedChannel` subclass (grpc does exactly this in `AndroidChannel`: it overrides `shutdown`, `isShutdown`, `isTerminated`, `shutdownNow`, `awaitTermination`, `newCall`, `authority`, `getState`, `notifyWhenStateChanged`, `resetConnectBackoff`, `enterIdle`; `AndroidChannelBuilder.java:249-303`). This changes the channel's identity and type.
    - Or reflectively patching `interceptorChannel` inside the returned wrapper (ForwardingManagedChannel's private `delegate` → `ManagedChannelImpl`).
  - In 1.34–1.58 B2 is a forwarder, so both B1 and B2 fire for one channel.
- **C (Studio's static factories): misses paths and needs entry hooks.** It misses `Grpc.newChannelBuilder*` and the creds overloads (table). It needs depth counting (entry hooks) or identity dedupe: identity fails for `AndroidChannelBuilder`, whose `intercept()` forwards to the inner builder that was already intercepted at the inner exit.
- **For channels that exist before attach, an exit hook alone cannot do it.**
  - Exit hooks receive only the return value and the method label (`TP:agent.cpp` `ExitHook … ReturnAsObject | PassMethodSignature`). Nothing per call returns an object that both carries the `MethodDescriptor` and can be wrapped.
  - Options:
    - **F (Studio's way).** JVMTI heap iteration needs `can_tag_objects` (our agent requests only `can_retransform_classes`, `TP:agent.cpp` `Attach`). Swap `interceptorChannel` for `ClientInterceptors.intercept(old, ours)`; ours is then outermost. A field swap on a `private final` field is what Studio does; JIT or visibility effects are **UNVERIFIED**.
    - **D.** slicer `EntryHook(…, Tweak::ArrayParams)` exists in our vendored slicer. Its comment: "Zero-th element of the array is the method signature. First element of the array is 'this'" (`android/attach-agent/third_party/slicer/export/slicer/instrumentation.h`). It generates a hook `static void onEntry(Object[])`, boot-safe.
      - Stash `[sig, this(RealChannel), method, callOptions]` in a single-slot `ThreadLocal`; the exit hook wraps the returned `ClientCall`.
      - It covers all channels, including Firestore's.
      - Limitation: the `ClientCall` already exists when the exit hook runs, so **no `ClientStreamTracer` can be added**. That loses wire headers and credentials headers (§4.2), keeping app-level headers plus `call.getAttributes()`.
      - Pairing via a single slot (overwrite on entry, clear on exit, pass through when empty) fails safe if `newCall` throws. Nested `RealChannel.newCall` on one thread is not expected **(analysis)**.
    - **E.** Exit-only and covers existing stubs, but only stub users.
- **Recommendation:** A1–A3 for all new channels, plus F (or D) once at runtime attach for channels that already exist. With F, our interceptor (with its tracer) sits on the channel, so fidelity matches A. Dedupe via the `CallOptions` claim (§6.10). D is simpler to build (no heap walk) at reduced header fidelity. E is a fallback that needs no agent changes beyond new targets.
- **Agent work implied (analysis).**
  - New descriptors in `InstrumentClass` (today only `java/net/URL` and `okhttp3/OkHttpClient`, `TP:agent.cpp`).
  - A loader record for `io.grpc` like `AcceptOkHttpLoader`.
  - A gRPC adapter loaded in an `InMemoryDexClassLoader` whose parent resolves the app's `io.grpc` (like `AttachEntry.AdapterParent`).
  - ClassPrepare handling on API 26–27 as for OkHttp.

### 3.3 How grpc resolves builders (for completeness)

- `ManagedChannelBuilder.forTarget(target)` = `ManagedChannelProvider.provider().builderForTarget(target)`. This body is identical at 1.14, 1.21, 1.30, 1.33, 1.36, 1.45, 1.50, 1.59, 1.69 and 1.84.1.
- The OkHttp provider then calls `OkHttpChannelBuilder.forTarget(target)` (`OkHttpChannelProvider.java:45-52`). Static factories therefore nest: `ManagedChannelBuilder.forTarget` → `OkHttpChannelBuilder.forTarget`.
- `OkHttpChannelBuilder.forAddress(String,int,ChannelCredentials)` calls `forTarget(…, creds)`, which calls a constructor (`OkHttpChannelBuilder.java:166-188`).

### 3.4 R8: which gRPC names survive minification

- gRPC ships no keep rules for its API or internals.
  - The only shipped rule file in the `GJ` tree at v1.84.1 is `cronet/proguard-rules.pro` (`-dontwarn org.chromium.**`, consumed via `consumerProguardFiles`). `android-interop-testing/proguard-rules.pro` belongs to a test app.
  - grpc-android's consumer `proguard-rules.txt` existed through **1.59** and was removed in **1.60**. It kept only `io.grpc.okhttp.OkHttpChannelBuilder { forTarget(String); scheduledExecutorService(…); sslSocketFactory(…); transportExecutor(…); }` (`GJ@v1.21.0:android/proguard-rules.txt`).
- Names gRPC loads with `Class.forName("<literal>")` include:
  - `io.grpc.okhttp.OkHttpChannelProvider`, `io.grpc.netty.NettyChannelProvider`, `io.grpc.netty.UdsNettyChannelProvider` (`ManagedChannelRegistry.java:134-152`, with the comment "Class.forName(String) is used to remove the need for ProGuard configuration").
  - `io.grpc.internal.DnsNameResolverProvider`, `io.grpc.internal.PickFirstLoadBalancerProvider`, `io.grpc.util.SecretRoundRobinLoadBalancerProvider$Provider`, the census accessors, `io.grpc.okhttp.OkHttpChannelBuilder` (in `UdsChannelBuilder`).
  - Per gRPC's comment these are kept by shrinkers. That R8 keeps the *name* in every mode is **UNVERIFIED**.
- Everything we would hook (`io.grpc.internal.ManagedChannelImplBuilder`, `ManagedChannelImpl$RealChannel`, `io.grpc.stub.AbstractStub`, `ManagedChannelBuilder`) can be renamed in a minified app unless the app keeps `io.grpc.**`. Whether apps commonly do is **UNVERIFIED**.
- The package-private, single-caller `getEffectiveInterceptors` is a candidate for R8 inlining into `build()` (**UNVERIFIED**).
- Attach mode needs a debuggable app (`TP:ARCHITECTURE.md` §4.7.2), usually an unminified debug build. A `method_not_found`/`class_not_found` status per hook is the right report. Library mode is unaffected (the app links our interceptor directly).

---

## 4. Turning a call into HTTP-like data

### 4.1 URL and method

- `:path` is `"/" + method.getFullMethodName()` (`GJ:okhttp/.../OkHttpClientStream.java:145`). `getFullMethodName()` exists since 1.0.0.
- `:method` is always `POST` with grpc-okhttp: `private final boolean useGetForSafeMethods = false;` (`OkHttpChannelBuilder.java`), and `useGet` only when `useGetForSafeMethods && method.isSafe()` (`OkHttpClientStream.java:84`).
- `:authority` is `callOptions.getAuthority()` if set (`ClientCallImpl.java:268-270` → `stream.setAuthority(...)`), else the transport's default authority. For a normal channel the transport's default equals `Channel.authority()`, which for `RealChannel` is the name resolver's service authority (`ManagedChannelImpl.java:633,995-997`). A per-address `EquivalentAddressGroup` authority override is invisible to interceptors **(analysis)**.
- `:scheme` is `https` unless plaintext (`Headers.java:64-68`).
  - Not visible at interceptor level before a stream exists.
  - Afterwards it can be read from transport attributes: `Grpc.TRANSPORT_ATTR_SSL_SESSION` is set (with `TRANSPORT_ATTR_REMOTE_ADDR`, `TRANSPORT_ATTR_LOCAL_ADDR`, `GrpcAttributes.ATTR_SECURITY_LEVEL`) in `OkHttpClientTransport.java:754-757`.
  - These reach us via `ClientStreamTracer.streamCreated(transportAttrs, …)` or `ClientCall.getAttributes()`. The latter is allowed only after `onHeaders`/`onClose` (`ClientCall.java:277-290`) and returns `stream.getAttributes()` (`ClientCallImpl`), which is the transport's attributes (`OkHttpClientStream.java:92,137-139`).
- Proposed `url`: `"https://" + authority + "/" + fullMethodName`, e.g. `https://firestore.googleapis.com/google.firestore.v1.Firestore/Listen`. Use `http` when the stream's attributes show no SSL session. The `SPEC` grammar is `Path → ":path" "/" Service-Name "/" {method name}` (`SPEC:24-28`).

### 4.2 Request headers: what each layer can see

Where things are added, in order:
1. The app or stub builds `Metadata`. Interceptors may `put` more.
2. `ClientCallImpl.startInternal` → `prepareHeaders(...)` (`ClientCallImpl.java:155-179`) discards and re-adds `grpc-encoding` (only if a compressor is set), `grpc-accept-encoding` (advertised decompressors), and `accept-encoding` (full-stream decompression only). It also discards `content-length` and `content-encoding`.
3. With retry enabled, each attempt gets a **copy**: `Metadata newHeaders = new Metadata(); newHeaders.merge(originalHeaders); if (previousAttemptCount > 0) newHeaders.put(GRPC_PREVIOUS_RPC_ATTEMPTS, …)` (`RetriableStream.java:288-296`). This is the default since 1.40: `boolean retryEnabled = true;` (`ManagedChannelImplBuilder.java:198`); it was `false` through 1.39 (matrix).
4. **CallCredentials** (per-call `CallOptions.withCallCredentials` or channel credentials' call creds) are merged into that header object in the transport layer: `origHeaders.merge(headers); … transport.newStream(method, origHeaders, callOptions, tracers)` (`MetadataApplierImpl.java:67-79`, from `CallCredentialsApplyingTransportFactory.java:114-184`). Interceptors never see these headers. Firestore auth is an example (`CallCredentials`, **UNVERIFIED** for Firestore specifically).
5. Tracers: `StatsTraceContext.newClientContext` calls `tracer.streamCreated(transportAtts, headers)` (`StatsTraceContext.java:58-65`), from `OkHttpClientTransport.newStream` (`:478`).
6. `grpc-timeout`: `stream.setDeadline(effectiveDeadline)` (`ClientCallImpl.java:278`) → `headers.discardAll(TIMEOUT_KEY); headers.put(TIMEOUT_KEY, deadline.timeRemaining(NANOSECONDS))` (`AbstractClientStream.java:125-128`). This happens **after** stream creation. With retries it is buffered and replayed on the attempt. Format: `TimeoutMarshaller`, e.g. `999999u`, at most 8 digits plus unit `n u m S M H` (`GrpcUtil.java` `TimeoutMarshaller`; `SPEC:31-39`).
7. The OkHttp transport writes the HEADERS frame (`Headers.createRequestHeaders`, `GJ:okhttp/.../Headers.java:47-87`):
   ```java
    stripNonApplicationHeaders(headers);      // discardAll(CONTENT_TYPE_KEY), discardAll(TE_HEADER), discardAll(USER_AGENT_KEY)  (:149-153)
    okhttpHeaders.add(usePlaintext ? HTTP_SCHEME_HEADER : HTTPS_SCHEME_HEADER);   // :scheme
    okhttpHeaders.add(useGet ? METHOD_GET_HEADER : METHOD_HEADER);                 // :method POST
    okhttpHeaders.add(new Header(Header.TARGET_AUTHORITY, authority));            // :authority
    okhttpHeaders.add(new Header(Header.TARGET_PATH, path));                      // :path
    okhttpHeaders.add(new Header(GrpcUtil.USER_AGENT_KEY.name(), userAgent));     // user-agent
    okhttpHeaders.add(CONTENT_TYPE_HEADER);                                       // content-type: application/grpc
    okhttpHeaders.add(TE_HEADER);                                                 // te: trailers
    return addMetadata(okhttpHeaders, headers);   // TransportFrameUtil.toHttp2Headers: -bin values base64 (no padding); drops non-compliant ASCII values with a warning
   ```
   - The UA is `GrpcUtil.getGrpcUserAgent("okhttp", builderUserAgent)` = `[<builder UA> ]grpc-java-okhttp/<IMPLEMENTATION_VERSION>` (`OkHttpClientTransport.java:348`, `GrpcUtil.java:468-480`). An app-supplied `user-agent` *metadata entry* is discarded.
   - Then `statsTraceCtx.clientOutboundHeaders()` fires (`OkHttpClientStream.java:252`), giving `ClientStreamTracer.outboundHeaders()`, "Headers has been sent to the socket".

Visibility summary:

| Header | `ClientInterceptor.start(…, headers)` (before delegating) | `ClientStreamTracer.streamCreated` (1.40+) | On the wire |
|---|---|---|---|
| app/stub/interceptor metadata | yes (only interceptors above ours) | yes | yes (non-compliant ASCII values dropped) |
| `grpc-accept-encoding`, `grpc-encoding`, `accept-encoding` | no | yes | yes |
| CallCredentials headers (e.g. `authorization`) | **no** | **yes** | yes |
| `grpc-previous-rpc-attempts` | no | yes (attempt ≥ 1) | yes |
| `grpc-timeout` | no (derive from `min(callOptions.getDeadline(), Context.current().getDeadline())`) | no (added after) | yes |
| `content-type: application/grpc`, `te: trailers` | no (synthesize; constants `GrpcUtil.java:172,182`) | no (stripped later) | yes |
| `user-agent` | no (transport value not observable; builder UA not readable) | no | yes |
| `:method :scheme :path :authority` | derivable (above) | derivable | yes |

Before 1.40 there is no `streamCreated`. `ClientStreamTracer.Factory.newClientStreamTracer(StreamInfo, Metadata)` (1.20+) was then invoked **inside** the transport's `newStream`, after credentials, with `StreamInfo.getTransportAttrs()`:
```java
ClientStreamTracer.StreamInfo.newBuilder().setTransportAttrs(transportAttrs).setCallOptions(callOptions).build();
… factories.get(i).newClientStreamTracer(info, headers);
```
(`GJ@v1.33.0:core/.../StatsTraceContext.java`, `newClientContext`). In 1.40+ the factory runs earlier, in `GrpcUtil.getClientStreamTracers` before transport and credentials (`GrpcUtil.java:767-784`), so snapshot in `streamCreated`. The factory's `headers` "should not be saved because it is not safe for read or write after the method returns" (`ClientStreamTracer.java:145-155`): copy inside the callback.

### 4.3 Response headers, trailers and status

- `Http2ClientStreamTransportState.transportHeadersReceived` validates, then `stripTransportDetails(headers)` before `inboundHeadersReceived` (`:88-122`). For trailers: `Status status = statusFromTrailers(trailers); stripTransportDetails(trailers); inboundTrailersReceived(trailers, status);` (`:172-188`). Where:
  ```java
  private static void stripTransportDetails(Metadata metadata) {
    metadata.discardAll(HTTP2_STATUS);
    metadata.discardAll(InternalStatus.CODE_KEY);      // "grpc-status"  (Status.java:355-356)
    metadata.discardAll(InternalStatus.MESSAGE_KEY);   // "grpc-message" (Status.java:386-387)
  }
  ```
  So `onHeaders` lacks `:status`; `onClose` trailers lack `grpc-status`/`grpc-message`. Rebuild those two from `Status`: `getCode().value()` and `getDescription()`. On the wire `grpc-message` is percent-encoded for bytes `< ' '`, `>= '~'` and `'%'` (`Status.java` `isEscapingChar`; `SPEC:112-114`). Response `content-type` (and `grpc-encoding` etc.) remain in `onHeaders`.
- **HTTP status is never observable** on success. `validateInitialMetadata` only requires a `:status` and a gRPC `content-type`; it does not check for 200 (`:219-233`). If `onHeaders` ran, `:status` existed and the content-type was gRPC. Report 200 as **inferred**.
  - Non-gRPC responses (wrong content-type) close with a status mapped from HTTP, e.g. `GrpcUtil.httpStatusToGrpcStatus`, "invalid content-type: …". The headers, *including* `:status`, are kept as `transportErrorMetadata` **(analysis)**.
- **Trailers-only** ("permitted for calls that produce an immediate error", `SPEC:108,118`): no `onHeaders`; `onClose(status, trailers)`, where trailers carry the response headers (e.g. `content-type`).
- **Locally generated closes** usually pass `new Metadata()`: cancel, deadline, `Context` cancellation (`ClosedByContext`, `ClientCallImpl.java:197-211`), "ClientCall started after … deadline was exceeded" (`FailingClientStream`, `:251-262`), and missing compressor.
  - Heuristic: server-sent ⇔ `onHeaders` was seen, or the trailers are non-empty, and `status.getCause() == null` **(analysis)**.
  - `onClose` docs: "An empty Metadata object is passed if no trailers are received" (`ClientCall.java:134-153`).

### 4.4 Message bytes and framing

- The wire format is `Compressed-Flag (1 byte) Message-Length (4 bytes big-endian) Message` (`SPEC:92-97`). grpc-java writes `headerScratch.put(compressed ? COMPRESSED : UNCOMPRESSED).putInt(messageLength)` with `HEADER_LENGTH = 5` (`GJ:core/.../MessageFramer.java:70-72,226,247`).
- The host splits exactly that (`TP:host/crates/core/src/decode/protobuf.rs:174-186`): `out.push((hdr[0] == 1, msg))`.
- The host shows "compressed with grpc-encoding; not decoded" for flag 1 (`host/crates/tui/src/bodyview.rs:239-246`). It picks the gRPC decoder only from `Content-Type: application/grpc*` (`host/crates/core/src/decode/kind.rs:130`). **So request headers must carry a synthesized `content-type: application/grpc`.**
- **Request:**
  - In `sendMessage(msg)` (caller thread), serialize with `method.getRequestMarshaller().stream(msg)` (1.1+) or `method.streamRequest(msg)` (1.0+), then emit `[0][len][bytes]` as body chunks.
  - Size first: `ProtoInputStream` is `KnownLength` (`available()` = serialized size) and its `read(b, off, len >= size)` path writes directly into the array (`GJ:protobuf-lite/.../ProtoInputStream.java:76-93`). One right-sized copy, no double buffering.
- **Response:** in `onMessage(msg)` (executor thread), `method.getResponseMarshaller().stream(msg)`, same framing. These are **re-encoded** bytes. For protobuf the content is equal (lite keeps unknown fields), but the bytes are not guaranteed identical to the wire (e.g. field order, packed vs. unpacked) **(analysis)**. The wire's decompressed bytes are not reachable from an interceptor without wrapping the marshaller (below).
- **Is a second `stream()` safe? Yes for protobuf:**
  - `ProtoLiteUtils.MessageMarshaller.stream(T value) { return new ProtoInputStream(value, parser); }` (`ProtoLiteUtils.java:162-164`). Reading only calls `message.getSerializedSize()/writeTo()/toByteArray()` (`ProtoInputStream.java:48-93`). Messages are immutable; only the memoized size changes.
  - `ProtoUtils.marshaller` (full protobuf) delegates to `ProtoLiteUtils.marshaller` (`GJ:protobuf/.../ProtoUtils.java:54-56`).
  - **grpc itself re-marshals the same message per attempt**:
    ```java
    class SendMessageEntry implements BufferEntry {
      public void runWith(Substream substream) {
        substream.stream.writeMessage(method.streamRequest(message));
    ```
    (`RetriableStream.java:577-597`, retries on by default since 1.40). Repeatable `stream()` is therefore already required of any marshaller used with retries.
  - Exceptions:
    - Identity `InputStream` marshallers, whose `stream(InputStream value) { return value; }`; grpc has this pattern in `ServerInterceptors.useInputStreamMessages` (`api/.../ServerInterceptors.java:140-144`). Skip capture when `msg instanceof InputStream` or when `stream(msg) == msg`.
    - Arbitrary custom marshallers.
- **Compression:** we capture uncompressed bytes with flag 0. The wire has flag 1 when `messageCompression && compressor != NONE` (`MessageFramer.java:139`), so mark bodies as "decoded" (like HttpURLConnection bodies, `TP:PROTOCOL.md` §5).
- **Exact-bytes alternative (optional):** pass `method.toBuilder(teeReqMarshaller, teeRespMarshaller).build()` to `next.newCall` (the pattern of `ClientInterceptors.wrapClientInterceptor`, `ClientInterceptors.java:98-…`). Costs:
  - It changes the `MethodDescriptor` lower layers see.
  - It defeats `KnownLength`/`Drainable` zero-copy paths, and `MessageFramer` throws "Message length inaccurate" if a `KnownLength` lies (`MessageFramer.java` `writePayload`).
  - It hides `PrototypeMarshaller`.
  - It would be teed once per retry attempt.
  - Not recommended for v1 **(analysis)**.
- **Streaming:**
  - Every `sendMessage` appends a frame to the request body (`dir=0`) at that time. Every `onMessage` appends to the response body (`dir=1`).
  - `body_end{request}` at `halfClose()` (or at `onClose` if the app never half-closes). `body_end{response}` at `onClose`.
  - Unary, client-streaming, server-streaming and bidi then differ only in how many frames, and when. `MethodDescriptor.getType()` (1.0) gives `UNARY|CLIENT_STREAMING|SERVER_STREAMING|BIDI_STREAMING|UNKNOWN` for `req.body.duplex` and display.
  - Long-lived streams (Firestore `Listen`/`Write`) keep transactions open for minutes. The body cap and `prog` apply.
  - **Cap at message boundaries.** A frame cut by the cap makes the host's `grpc_messages` return "truncated gRPC message" and fall back to hex for the whole body (`protobuf.rs:181`; `bodyview.rs:252-255`).

### 4.5 Mapping onto PROTOCOL.md events (proposal)

| gRPC callback (thread) | traffic-police |
|---|---|
| `interceptCall` (caller) | `ThreadStack.capture("call", depth)`; new `call` id |
| `start(listener, headers)` (caller) | snapshot headers **before** delegating; `req`: `method:"POST"`, `url`, `headers` = snapshot + synthesized `content-type: application/grpc`, `te: trailers`, `grpc-timeout` (derived); `client:{kind:"grpc", version}`; `body:{length:-1, type:"application/grpc", duplex: type is CLIENT_STREAMING or BIDI}`; mark `call_start` |
| `ClientStreamTracer.streamCreated(attrs, md)` (thread creating the stream) | wire-level header snapshot (+credentials, +attempt number); `conn` from attrs (`TRANSPORT_ATTR_REMOTE_ADDR`, `SSLSession` → TLS version/cipher/peer certs, protocol `h2`) |
| `outboundHeaders()` | mark `req_headers_end` |
| `sendMessage(m)` (caller) | framed chunk `dir=0`; first one marks `req_body_start` |
| `halfClose()` | `body_end{request, complete}`; mark `req_body_end` |
| tracer `inboundHeaders()` (transport thread) | mark `resp_headers_start` |
| `onHeaders(md)` (executor) | `resp{status:200 (inferred), protocol:"h2", headers}` |
| `onMessage(m)` (executor) | framed chunk `dir=1` |
| `onClose(status, trailers)` (executor) | trailers-only: emit `resp` first; `body_end{response}`; server status → `done` + trailers; local status → `fail` (`canceled` for CANCELLED from the app or Context; `phase` connect/request/response by progress) |

Additive protocol fields, allowed by `TP:PROTOCOL.md` §1 "Additive changes … do not change it":
- capability `grpc`; `client.kind:"grpc"`.
- `trailers` (ordered list) and `grpc:{status, code, message}` on `done`/`fail`.
- optionally `req.grpc:{type, service, method}`.
- a new event carrying the stream-level request headers and `conn` (e.g. `req_headers`), or delay `req` until the first `streamCreated` as OkHttp delays `req` until the network interceptor (`TP:PROTOCOL.md` §7.1 `req.marks`). With delay, calls waiting on DNS/connect are not shown until a stream exists; emit `req` from `onClose` when no stream ever appeared. Retry attempts can become `hop` transactions under one `call`, or marks.

---

## 5. Threading

- **`newCall` and `start` run on the caller's thread for stubs.**
  - Blocking: `ClientCall<ReqT, RespT> call = channel.newCall(method, callOptions.withOption(ClientCalls.STUB_TYPE_OPTION, StubType.BLOCKING).withExecutor(executor));` then `futureUnaryCall(call, req)`. That goes `asyncUnaryRequestCall` → `startCall` → `call.start(responseListener, new Metadata())`, then `sendMessage`, `halfClose` (`GJ:stub/.../ClientCalls.java:155-183,432-437`).
  - Async: `asyncUnaryCall(getChannel().newCall(getXMethod(), getCallOptions()), request, responseObserver)` (golden file).
  - Kotlin (`GK:238-283`): `channel.newCall(method, callOptions)` and `clientCall.start(…, headers.copy())` run in the collecting coroutine. `sendMessage`/`halfClose` run in a child `launch(CoroutineName("SendMessage worker for ${method.fullMethodName}"))`.
  - Whether the app's suspending frames appear in the stack is **UNVERIFIED**; it likely depends on the first suspension point.
- **Interceptor chains are synchronous.** `ClientInterceptors.InterceptorChannel.newCall` is `return interceptor.interceptCall(method, callOptions, channel);` (`ClientInterceptors.java:144-157`), and `ForwardingClientCall.start` delegates. So an innermost interceptor still runs on the caller's thread, unless an app interceptor defers `newCall`/`start` (e.g. waits for a token).
- **Where to capture.** Capture thread and stack in `interceptCall` (the newCall site), which is what PROTOCOL's `origin:"call"` means, and timestamps in `start`.
- **Calls that wait.** Before name resolution completes, `RealChannel.newCall` returns a `PendingCall extends DelayedClientCall` (`ManagedChannelImpl.java:880-931`, `:999`). Its real start happens later on another thread, but that is *below* all interceptors.
- **Firestore** creates calls on its async-queue executor (`FS:143`), so its "caller" thread is Firestore's worker.
- **Listener callbacks** go through `ClientCallImpl.callExecutor` (`ClientCallImpl.java:97-115`):
  ```java
    if (executor == directExecutor()) {
      this.callExecutor = new SerializeReentrantCallsDirectExecutor();
    } else {
      this.callExecutor = new SerializingExecutor(executor);
  ```
  - The executor is `callOptions.getExecutor()` or else the channel executor (`ManagedChannelImpl.getCallExecutor`, `:835-841`). The channel executor is `builder.executor(...)` or the default `GrpcUtil.SHARED_CHANNEL_EXECUTOR` = `Executors.newCachedThreadPool(getThreadFactory("grpc-default-executor-%d", true))` (`GrpcUtil.java:557-563`).
  - Blocking stubs use `ThreadlessExecutor`; callbacks run on the blocked caller thread in `executor.waitAndDrain()` (`ClientCalls.java:155-183`).
  - `directExecutor()` channels run callbacks on the transport thread: the grpc-okhttp reader is a `grpc-okhttp-%d` pool thread renamed `"OkHttpClientTransport"` while reading (`OkHttpChannelBuilder.java` `SHARED_EXECUTOR`; `OkHttpClientTransport.java:1321-1323`).
  - In that last case, re-serializing responses in `onMessage` delays the connection's reader. Keep it cheap: cap, and skip large messages using `available()`.
- **Tracer callbacks** run on transport or attempt threads (`ClientStreamTracer` docs, e.g. `recordAttemptDelayStart` "invoked synchronously on the attempt thread", `:59-72`).
- **Never block** in any callback.
- **Never throw.** An exception from `onHeaders`/`onMessage` cancels the call: `exceptionThrown(Status.CANCELLED.withCause(t).withDescription("Failed to read headers"))` and `"Failed to read message."` (`ClientCallImpl.java:622-624,670-672`). `onClose` "should not throw" (`ClientCall.java:142-146`).

---

## 6. Gotchas

1. **Binary headers.**
   - Names end in `-bin` (`Metadata.BINARY_HEADER_SUFFIX`, `Metadata.java:66`). Values in `Metadata` are raw bytes.
   - On HTTP/2 they are base64 sent *unpadded*: `BASE64_ENCODING_OMIT_PADDING` in `TransportFrameUtil.toHttp2Headers` (`:50-82`). The spec: "Implementations MUST accept padded and un-padded values and should emit un-padded values" (`SPEC:68`).
   - Read them with `Metadata.Key.of(name, Metadata.BINARY_BYTE_MARSHALLER)` and base64 (no padding) for display. Never use `Key.of(name, ASCII_STRING_MARSHALLER)` for them: it throws (`Metadata.java:966-975`; Studio's bug, §2.5).
   - Also: `toHttp2Headers` silently drops ASCII values that are not spec-compliant ("contains invalid ASCII characters"), so app `Metadata` can differ from the wire.
2. **Metadata iteration and order.**
   - `keys()` returns `Collections.unmodifiableSet` of a HashSet: hash order, deduplicated (`Metadata.java:324-335`).
   - `getAll(key)` gives that key's values "in the order they were received" (`:303-317`).
   - There is no public ordered iterator. `InternalMetadata.serialize(Metadata)` (`@Internal`, public, `InternalMetadata.java:77-80`) returns `byte[][]` name/value pairs in insertion order with raw binary values. It exists at v1.10, 1.21, 1.40 and 1.84.1.
   - `Metadata.toString()` is ordered and base64s `-bin` values, but `name=value` joined with `,` is ambiguous (`:545-560`).
   - Recommendation: `InternalMetadata.serialize`, guarded by `catch (LinkageError)`, with `keys()`+`getAll` as fallback. Keys are lowercase.
3. **Metadata ownership.** Snapshot request headers **before** calling `super.start()` (`ClientCall.java:180`). gRPC mutates the same object afterwards, possibly on another thread for a `PendingCall`: `prepareHeaders` always; `setDeadline` and `stripNonApplicationHeaders` when retries are off. Snapshot `onHeaders`/`onClose` Metadata **before** delegating to the app's listener, which owns it afterwards.
4. **Transport-only headers and stripping:** §4.2–4.3 table. Pseudo-headers, `content-type`, `te`, `user-agent`, `grpc-timeout` and credentials headers are not visible to interceptors. `:status`, `grpc-status` and `grpc-message` are stripped on receive.
5. **Cancellation and deadlines.**
   - `cancel(message, cause)` → `onClose(CANCELLED…)`. "If cancel won the race, onClose is called with CANCELLED… We ensure that at most one is called" (`ClientCall.java` class docs).
   - `cancel` before `start` is allowed with no `onClose` (same docs: "start must be called prior … with the exception of cancel").
   - Context cancellation → CANCELLED.
   - `DEADLINE_EXCEEDED` is usually local, and can occur before any stream ("ClientCall started after %s deadline was exceeded…", `ClientCallImpl.java:254-262`).
   - Distinguish local from server statuses with the heuristic in §4.3.
6. **Retries and hedging.**
   - Header copies per attempt; `streamCreated` per attempt (`StreamInfo.getPreviousAttempts()`/`isTransparentRetry()` 1.40+, `isHedging()` 1.74+).
   - The request is re-marshalled per attempt; listener-level messages and the status come from the committed attempt only.
7. **`ManagedChannel` vs `Channel` in hooks.**
   - Slicer's `ReturnAsObject` exit hook `check-cast`s to the declared return type (`TP:ARCHITECTURE.md` §4.7.3). From `build()` you must return a `ManagedChannel`; `ClientInterceptors.intercept` gives a `Channel`.
   - The built object is `ManagedChannelOrphanWrapper(ManagedChannelImpl)`, and `AndroidChannelBuilder` wraps it again in `AndroidChannel`.
   - Prefer `getEffectiveInterceptors` (returns `List`), or the field swap (field type `Channel`), or `RealChannel.newCall` (returns `ClientCall`).
8. **Compile baseline.** Use only APIs present in the floor version:
   - `getFullMethodName`/`getType` (1.0), `getRequestMarshaller`/`getResponseMarshaller` (1.1), `CallOptions.Key.create`/`withOption` (1.13), `ClientStreamTracer.Factory.newClientStreamTracer(StreamInfo, Metadata)` (1.20).
   - Declare `streamCreated(Attributes, Metadata)` (1.40) in our tracer *without* `@Override`, compiled against the floor. On 1.40+ it overrides by name and descriptor; below, it is never called **(analysis, standard Java binary compatibility)**.
   - Avoid `getServiceName` (1.21) and `getBareMethodName` (1.33): split `getFullMethodName()` at the last `/` instead (`MethodDescriptor.extractFullServiceName` is 1.0).
   - Avoid Java 9+ APIs (`readAllBytes`: API 33).
9. **Bodies.** Use the `InputStream` and identity-marshaller guard (§4.4). Use the size cap via `KnownLength.available()`. Cut only at message boundaries. Never call protobuf `toString()`: it is slow and reflective, and obfuscated under R8.
10. **Double interception.**
    - (a) Library and attach in one process. `TP:ARCHITECTURE.md` §4.7.6 offers attach only for processes not already running capture. Still, make the attach list hook skip a list that contains an interceptor whose class name is ours, as the OkHttp handlers do (§4.7.3).
    - (b) Several of our interceptors on one call: library mode added twice, attach builder hook plus field swap, or a stub hook. Pass a mutable claim object through `CallOptions` (`CallOptions.Key`, same class loader, so identity works). The innermost instance captures and the outer ones stand down (sketch in §3.1).
    - (c) Studio's interceptor present at the same time. Detect class `com.android.tools.appinspection.network.grpc.GrpcInterceptor` in the interceptor list and emit the existing `studio_inspector_present` diag (`TP:PROTOCOL.md` §7.1).
    - (d) After detach, interceptors baked into channels must check the runtime's `active` flag and pass through, as with OkHttp (`TP:ARCHITECTURE.md` §4.7.5).
11. **Class loaders.** The gRPC adapter must link against the app's `io.grpc`, the same split as the OkHttp adapter (`TP:AttachEntry.java` `AdapterParent`). Shaded gRPC copies (renamed packages) are invisible to name-based hooks.
12. **Studio comparison points to fix** (§2.5): `-bin` headers, `grpc-message` not kept, only the last message kept, header order and duplicates lost, reads after `start()`, `hookDepth` leak, missing builder paths, API < 33 payload loss.

---

## 7. Not verified

- Whether R8 inlines or renames `getEffectiveInterceptors`/`RealChannel` in real minified apps, and whether apps commonly keep `io.grpc.**`. Also whether R8 keeps names for `Class.forName("literal")` in all modes; only gRPC's own comment says so.
- Whether ART JIT or visibility rules affect Studio-style reflective writes to the `private final interceptorChannel` field (Studio does it; it is untested in Studio's suite).
- Actual runtime confirmation of Studio's failures (`-bin`, API < 33, gRPC < 1.33). These are inferred from source, docs and the R8 backport list, not run.
- How common Netty, Cronet or Binder transports and shaded gRPC are in Android apps.
- Whether Firestore attaches its auth via `CallCredentials` (not read).
- The stack shape for grpc-kotlin coroutine stubs.
- Earliest gRPC version for `AndroidChannelBuilder`: present at 1.14, absent at 1.10; 1.11–1.13 not checked.
- The exact behaviour of `streamCreated` and header snapshots with Cronet or Binder transports (only OkHttp and core were read).
