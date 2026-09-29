# 05 — OkHttp / Okio verification for the netinspect capture runtime

Rule applied: **verify, do not recall**. Every claim below cites source read during this task as
`repo@tag:path:line "verbatim quote"`. Anything not established that way is marked **UNVERIFIED**.

## 0. Sources, evidence types, conventions

- **Source**: blob-less clones of `square/okhttp` and `square/okio`, read with `git show <tag>:<path>`.
  - OkHttp tags: `parent-3.5.0` … `parent-3.14.9`, `parent-4.0.0` … `parent-4.12.0`, `parent-5.0.0-alpha.*`, `parent-5.0.0` … `parent-5.5.0`. The latest, `parent-5.5.0`, is commit a94bdf15 (2026-08-16), "Prepare for release 5.5.0".
  - Okio tags: `okio-parent-1.13.0` … `okio-parent-1.17.6`, `okio-parent-2.0.0`/`2.1.0`, `parent-2.10.0`, `parent-3.0.0` … `parent-3.18.2`. The latest, `parent-3.18.2`, is dated 2026-09-04.
- **Checked-in binary API dumps** (exact JVM descriptors):
  - OkHttp: `okhttp/api/jvm/okhttp.api` and `okhttp/api/android/okhttp.api`, present from 5.0.0. They were introduced by commit 3e16ec28f, "Adopt Kotlin's binary compatibility validator (#7112)". 4.x has no dump.
  - Okio: `okio/api/okio.api`, present from 3.4.0.
- **Bytecode**: `javap` over jars already present in the local Gradle cache (`~/.gradle/caches/modules-2/files-2.1/…`). Nothing was downloaded or installed. Cited as `bytecode:<jar> "javap text"`.
  - OkHttp: `okhttp-4.12.0.jar`, `okhttp-jvm-5.1.0.jar`, and the Android variant `okhttp-android` 5.3.2 (`okhttp-release.aar`).
  - Okio: `okio-jvm-2.9.0.jar`, `okio-jvm-3.4.0.jar`, `okio-jvm-3.16.4.jar`.
- **Prior art** (for comparison only): Android Studio's inspector, `tools-base@11ff8856` (HEAD of the local clone).
- **Path shorthand in prose only**: "3.x path" = `okhttp/src/main/java/okhttp3/`, "4.0.0 path" = `okhttp/src/main/java/okhttp3/` (Kotlin files), "4.12.0 path" = `okhttp/src/main/kotlin/okhttp3/`, "5.x path" = `okhttp/src/commonJvmAndroid/kotlin/okhttp3/`. Citations always spell out the full path.

## TL;DR

| # | Answer |
|---|---|
| 1 | `okhttp3/OkHttpClient.networkInterceptors()Ljava/util/List;` in every version 3.9.0–5.5.0: non-final in 3.x, `final` in 4.x/5.x, confirmed by the 5.x API dump and 4.12.0 bytecode. `RealCall.getResponseWithInterceptorChain()` reads it on **every call** and skips it for WebSocket calls. The list is unmodifiable, so a hook must return a new list. **Trap:** in 4.x/5.x `newBuilder()` copies through the getter (bytecode-verified), so the hook must be idempotent. |
| 2 | `eventListenerFactory()Lokhttp3/EventListener$Factory;` is public from 3.9.0 (package-private in 3.7–3.8). `create(call)` is invoked once per `RealCall` construction, i.e. per `newCall()`/`clone()`, not per execution. It was not `@Experimental`: 3.9/3.10 carried a Javadoc "unstable preview" warning, and 3.11 declared it stable. |
| 3 | 20 callbacks in 3.9 → 22 (3.14: request/responseFailed) → 24 (4.1: proxySelect*) → 25 (4.4: canceled) → 29 (4.7: cache*/satisfactionFailure) → 31 (5.0: retryDecision/followUpDecision) → 33 (5.2: dispatcherQueue*), plus a **final** `plus()` (5.3) and `Call.addEventListener` (5.4). No existing descriptor changed. The class is still abstract with no-op `public` methods in 5.5. `callStart` runs on the caller thread for both `execute()` and `enqueue()`. 5.x connect events can fire on background threads. Compile the forwarding wrapper against 5.5.0 and prefer `EventListener.plus` on ≥ 5.3. |
| 4 | `intercept(Lokhttp3/Interceptor$Chain;)Lokhttp3/Response;` is unchanged; `fun interface` (4.9+) does not alter the JVM shape. `Chain.call()` since 3.9.0. The "exactly once" check lives in `RealInterceptorChain.proceed` in every version. Throwing before `proceed()` is allowed. `Chain` gained 30 abstract methods in 5.4.0. |
| 5 | Nothing renamed or removed (3.9 → 5.5). Additions only: `Route.echConfigList()` (5.5), `Protocol.HTTP_3` (5.0.0-alpha.4), `QUIC` (3.10), `H2_PRIOR_KNOWLEDGE` (3.11). |
| 6 | Java code compiled against the 3.14 API links on 4.x and 5.x. The only public removal (4.12 → 5.x) is `OkHttpClient.clone()`/`Cloneable`. 5.x runtime traps: `Response.body()` is non-null, `Builder.body(null)` throws NPE, and stripped/101 bodies throw on read. `ResponseBody` is still abstract with exactly `contentType/contentLength/source`. `isOneShot/isDuplex` first appear in 3.14.0. |
| 7 | Bridge adds `Accept-Encoding: gzip` only if the request has no `Accept-Encoding` and no `Range`. It unzips only if it added that header and the response says `gzip` and has a body; it then strips `Content-Encoding` and `Content-Length`. Same logic in 3.x/4.x/5.x. No brotli/zstd in the default chain in 5.5. `CompressionInterceptor` (5.2+), `BrotliInterceptor` (4.1+) and zstd (5.2+) are opt-in and documented as application interceptors, i.e. outside and after our network interceptor. |
| 8 | Order: app interceptors → RetryAndFollowUp → Bridge → Cache → Connect → **network** (if `!forWebSocket`) → CallServer, in 3.x/4.x/5.x. Network interceptors run once per attempt/hop, never for cache hits, and never for WebSocket handshakes. Rewritten responses **are written to the HTTP cache**. |
| 9 | Safe Okio subset (1.13 → 3.18), no reflection needed: `Okio.buffer(Source/Sink)`, `ForwardingSource/Sink`, `Buffer()`/`size()`/`copyTo(Buffer,long,long)`/`readByteArray()`/`write(byte[],int,int)`/`clear()`, `BufferedSource/Sink.buffer()` (**not** `getBuffer()`), `GzipSource`, `InflaterSource(Source,Inflater)`. Never implement `BufferedSource`/`BufferedSink`. |
| 10 | Compile the core against **OkHttp 3.14.9 + Okio 1.13.0**, and only the EventListener forwarder against **OkHttp 5.5.0**. Small shims are listed in §10. The oldest version with EventListener + `Chain.call()` is **3.9.0**. |
| 11 | 3.9–3.12.x: Android API 9+ / Java 7. 3.13+, 4.x, 5.x: API 21+ / Java 8. 5.x ships separate JVM and Android artifacts (Gradle module metadata; Maven users pick `okhttp-jvm` or `okhttp-android`) with the **same `okhttp3.*` class names**. |

---

## 1. `OkHttpClient.networkInterceptors()` getter

### 1a. JVM name and descriptor per version

| Version | Source declaration | JVM-visible | Evidence |
|---|---|---|---|
| 3.9.0 | Java method | `networkInterceptors()Ljava/util/List;` public, non-final | `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/OkHttpClient.java:416` "public List<Interceptor> networkInterceptors() {" |
| 3.12.13 | same | same | `okhttp@parent-3.12.13:okhttp/src/main/java/okhttp3/OkHttpClient.java:415` "public List<Interceptor> networkInterceptors() {" |
| 3.14.9 | same | same | `okhttp@parent-3.14.9:okhttp/src/main/java/okhttp3/OkHttpClient.java:389` "public List<Interceptor> networkInterceptors() {" |
| 4.0.0 | Kotlin property with `@get:JvmName`, plus an ERROR-deprecated function renamed to `-deprecated_networkInterceptors` | `networkInterceptors()Ljava/util/List;` (public final) and `-deprecated_networkInterceptors()Ljava/util/List;` | `okhttp@parent-4.0.0:okhttp/src/main/java/okhttp3/OkHttpClient.kt:140-141` "@get:JvmName("networkInterceptors") val networkInterceptors: List<Interceptor> = builder.networkInterceptors.toImmutableList()"; `:267` "@JvmName("-deprecated_networkInterceptors")"; `:272` "fun networkInterceptors(): List<Interceptor> = networkInterceptors" |
| 4.12.0 | same as 4.0.0 | same | `okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/OkHttpClient.kt:142-143` (same text), `:308`, `:313`; `bytecode:okhttp-4.12.0.jar` "public final java.util.List<okhttp3.Interceptor> networkInterceptors();" and "public final java.util.List<okhttp3.Interceptor> -deprecated_networkInterceptors();" |
| 5.0.0 | same | `public final fun networkInterceptors ()Ljava/util/List;` | `okhttp@parent-5.0.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/OkHttpClient.kt:153-155`; `okhttp@parent-5.0.0:okhttp/api/jvm/okhttp.api:890` "public final fun networkInterceptors ()Ljava/util/List;" (same at `okhttp/api/android/okhttp.api:890`); `:859` "public final fun -deprecated_networkInterceptors ()Ljava/util/List;" |
| 5.5.0 | same | same | `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/OkHttpClient.kt:156-158` "@get:JvmName("networkInterceptors") / val networkInterceptors: List<Interceptor> = / builder.networkInterceptors.toImmutableList()"; `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:1012` "public final fun networkInterceptors ()Ljava/util/List;" inside `:964` "public class okhttp3/OkHttpClient : okhttp3/Call$Factory, okhttp3/WebSocket$Factory {"; android dump `:1013` identical |

The bytecode name and descriptor are the same in every version checked. Do not confuse the client getter with the Builder's mutable-list accessor of the same name: `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/OkHttpClient.kt:700` "fun networkInterceptors(): MutableList<Interceptor> = networkInterceptors", and `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:1059`, which sits inside `okhttp3/OkHttpClient$Builder` (`:1030`).

### 1b. Where it is read: on every call, in every major version

`getResponseWithInterceptorChain()` builds a **new** interceptor list per call execution and calls the getter each time. It is called once from `execute()` and once from `AsyncCall`.

- **3.9.0**: `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/RealCall.java:183-193`:
  ```
  Response getResponseWithInterceptorChain() throws IOException {
    // Build a full stack of interceptors.
    List<Interceptor> interceptors = new ArrayList<>(); …
    if (!forWebSocket) {
      interceptors.addAll(client.networkInterceptors());
  ```
  Callers: `:77` "Response result = getResponseWithInterceptorChain();" (execute) and `:147` (`AsyncCall.execute`).
- **3.12.13**: `okhttp@parent-3.12.13:okhttp/src/main/java/okhttp3/RealCall.java:249` "interceptors.addAll(client.networkInterceptors());". Callers `:93` and `:201`.
- **3.14.9**: `okhttp@parent-3.14.9:okhttp/src/main/java/okhttp3/RealCall.java:219` (same text). Callers `:81` and `:172`.
- **4.0.0**: `okhttp@parent-4.0.0:okhttp/src/main/java/okhttp3/RealCall.kt:168` "val interceptors = mutableListOf<Interceptor>()" and `:174-176` "if (!forWebSocket) { interceptors += client.networkInterceptors }". Callers `:66` and `:136`.
- **4.12.0**:
  - `okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/internal/connection/RealCall.kt:177`, `:183-185`. Callers `:154` and `:517`.
  - `bytecode:okhttp-4.12.0.jar`, in `RealCall.getResponseWithInterceptorChain$okhttp()`: "invokevirtual #294 // Method okhttp3/OkHttpClient.networkInterceptors:()Ljava/util/List;".
- **5.0.0**: `okhttp@parent-5.0.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/internal/connection/RealCall.kt:189-191`.
- **5.5.0**:
  - `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/internal/connection/RealCall.kt:212` "val interceptors = mutableListOf<Interceptor>()" and `:218-220` "if (!forWebSocket) { interceptors += client.networkInterceptors }". Callers `:189` (execute) and `:582` (`AsyncCall.run`).
  - `bytecode:okhttp-jvm-5.1.0.jar` "invokevirtual #322 // Method okhttp3/OkHttpClient.networkInterceptors:()Ljava/util/List;".
- **Production call sites** (`git grep` over `okhttp/src/main`):
  - 3.9.0 / 3.12.13 / 3.14.9: the only reader outside `OkHttpClient` is `RealCall` (`RealCall.java:192` / `:249` / `:219`).
  - 5.5.0 (`okhttp/src/commonJvmAndroid`, `jvmMain`, `androidMain`): `RealCall.kt:219`, plus `OkHttpClient$Builder`'s copy constructor (§1d).

**Conclusion.** Clients created before attach are covered, because the getter is re-read for every call started after the hook is in place. A call that is already inside `getResponseWithInterceptorChain()` keeps the list it built.

### 1c. The returned list is immutable, so the hook must return a new list

- **3.x**: the field is `Util.immutableList(builder.networkInterceptors)`:
  - Field: `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/OkHttpClient.java:235`; `okhttp@parent-3.12.13:…/OkHttpClient.java:240`; `okhttp@parent-3.14.9:…/OkHttpClient.java:211`.
  - `Util.immutableList`: `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/internal/Util.java:191-192` "public static <T> List<T> immutableList(List<T> list) { return Collections.unmodifiableList(new ArrayList<>(list));" (3.12.13 `Util.java:228-229`; 3.14.9 `Util.java:220-221`).
  - Javadoc: `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/OkHttpClient.java:412` "Returns an immutable list of interceptors that observe a single network request and response."
- **4.x**: `okhttp@parent-4.0.0:okhttp/src/main/java/okhttp3/internal/Util.kt:458-459` "fun <T> List<T>.toImmutableList(): List<T> { return Collections.unmodifiableList(toMutableList())" (4.12.0 `okhttp/src/main/kotlin/okhttp3/internal/Util.kt:473-474`).
- **5.x**: `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/internal/-UtilJvm.kt:261-269` "this.isEmpty() -> emptyList() … this.size == 1 -> Collections.singletonList(this[0]) … else -> (this as java.util.Collection<*>).toArray().asList().unmodifiable() as List<T>" (5.0.0 `-UtilJvm.kt:260-266`). Kdoc: `okhttp@parent-5.5.0:…/OkHttpClient.kt:152` "Returns an immutable list of interceptors that observe a single network request and response."

So the exit hook must allocate and return a fresh `java.util.List` (for example an `ArrayList`) containing our interceptor plus the original entries. Mutating the returned list is not possible in any version.

### 1d. Trap: in 4.x/5.x `newBuilder()` copies through the hooked getter

- **3.x** `Builder(OkHttpClient)` reads the **fields** directly (Java same-package access):
  - `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/OkHttpClient.java:502-503` "this.networkInterceptors.addAll(okHttpClient.networkInterceptors); this.eventListenerFactory = okHttpClient.eventListenerFactory;"
  - Same at `3.12.13 :506-507` and `3.14.9 :480-481`.
- **4.x/5.x** source reads like a field access:
  - `okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/OkHttpClient.kt:505-506` "this.networkInterceptors += okHttpClient.networkInterceptors / this.eventListenerFactory = okHttpClient.eventListenerFactory"
  - `okhttp@parent-5.5.0:…/OkHttpClient.kt:629-630` has the same text.
- But the **bytecode calls the getters** in constructor `OkHttpClient$Builder(okhttp3.OkHttpClient)`:
  - `bytecode:okhttp-4.12.0.jar` "invokevirtual #331 // Method okhttp3/OkHttpClient.networkInterceptors:()Ljava/util/List;" and "invokevirtual #333 // Method okhttp3/OkHttpClient.eventListenerFactory:()Lokhttp3/EventListener$Factory;"
  - `bytecode:okhttp-jvm-5.1.0.jar` "invokevirtual #347 … networkInterceptors:()Ljava/util/List;" and "invokevirtual #349 … eventListenerFactory:()Lokhttp3/EventListener$Factory;"
  - The okhttp-android 5.3.2 classes.jar shows the same, per the forked sub-investigation's javap.
- **Consequence under attach mode on 4.x/5.x.** `client.newBuilder()…build()` physically bakes our interceptor and wrapped factory into the derived client. That client's hooked getter would then prepend and wrap **again**. The hook must:
  1. skip prepending when an instance of our interceptor is already in the list;
  2. not re-wrap a factory that is already ours.

  Our interceptor and listener must also become cheap pass-throughs once the agent detaches, because derived clients keep them.
- **Prior art.** Android Studio prepends into a new list **without** a presence check for okhttp3: `tools-base@11ff8856:app-inspection/inspectors/network/src/com/android/tools/appinspection/network/NetworkInspector.kt:270-277` "\"networkInterceptors()Ljava/util/List;\", … val interceptors = ArrayList<okhttp3.Interceptor>() / interceptors.add(OkHttp3Interceptor(trackerService, interceptionService)) / interceptors.addAll(list)". Its okhttp2 hook does check: `:256` "if (list.none { it is OkHttp2Interceptor })".
- **4.x finality.** The accessor became `final`. 4.x's japicmp excludes it: `okhttp@parent-4.12.0:okhttp/build.gradle:99` "'okhttp3.OkHttpClient#networkInterceptors()',". Upgrade doc: `okhttp@parent-4.12.0:docs/upgrading_to_okhttp_4.md:36-37` "`OkHttpClient` has 26 accessors like `interceptors()` and `writeTimeoutMillis()` that were non-final in OkHttp 3.x and are final in 4.x." In 3.x an app subclass could override the getter (it is non-final), which would bypass a hook on `OkHttpClient`; this is rare.

## 2. `OkHttpClient.eventListenerFactory()` getter

### 2a. JVM name, descriptor and visibility

| Version | Evidence | JVM-visible |
|---|---|---|
| 3.7.0–3.8.1 | `okhttp@parent-3.7.0:okhttp/src/main/java/okhttp3/OkHttpClient.java:409-410` "// TODO(jwilson): make this public after the 3.7 release. / /*public*/ EventListener.Factory eventListenerFactory() {"; `okhttp@parent-3.7.0:okhttp/src/main/java/okhttp3/EventListener.java:21-22` "// TODO(jwilson): make this public after the 3.7 release. / abstract class EventListener {"; `okhttp@parent-3.8.1:okhttp/src/main/java/okhttp3/EventListener.java:21-22` "// TODO(jwilson): make this public after the 3.8 release. / abstract class EventListener {" | package-private: not usable |
| 3.9.0 | `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/OkHttpClient.java:420` "public EventListener.Factory eventListenerFactory() {" | `eventListenerFactory()Lokhttp3/EventListener$Factory;` |
| 3.12.13 / 3.14.9 | `okhttp@parent-3.12.13:…/OkHttpClient.java:419`; `okhttp@parent-3.14.9:…/OkHttpClient.java:393` (same text) | same |
| 4.0.0 / 4.12.0 | `okhttp@parent-4.0.0:okhttp/src/main/java/okhttp3/OkHttpClient.kt:143` "@get:JvmName("eventListenerFactory") val eventListenerFactory: EventListener.Factory ="; `okhttp@parent-4.12.0:…/OkHttpClient.kt:145`, with `-deprecated_eventListenerFactory` at `:315`; `bytecode:okhttp-4.12.0.jar` "public final okhttp3.EventListener$Factory eventListenerFactory();" | same (final) |
| 5.0.0 / 5.5.0 | `okhttp@parent-5.0.0:okhttp/api/jvm/okhttp.api:883` / `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:1005` "public final fun eventListenerFactory ()Lokhttp3/EventListener$Factory;" (android dump `:1006`); source `okhttp@parent-5.5.0:…/OkHttpClient.kt:160-162` | same (final) |

### 2b. When it is invoked: once per `RealCall` construction

It runs at `newCall()` and at `clone()`; it is not per execution and not per hop.

- **3.9.0 / 3.12.13**:
  - `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/RealCall.java:57-61` "static RealCall newRealCall(…) { // Safely publish the Call instance to the EventListener. RealCall call = new RealCall(client, originalRequest, forWebSocket); call.eventListener = client.eventListenerFactory().create(call);" (`3.12.13 RealCall.java:72-76`).
  - `OkHttpClient.newCall`: `okhttp@parent-3.9.0:…/OkHttpClient.java:427-428` "@Override public Call newCall(Request request) { return RealCall.newRealCall(this, request, false /* for web socket */);".
  - `clone()`: `RealCall.java:116-117` "return RealCall.newRealCall(client, originalRequest, forWebSocket);".
- **3.14.9**: `okhttp@parent-3.14.9:okhttp/src/main/java/okhttp3/RealCall.java:64` "call.transmitter = new Transmitter(client, call);" → `okhttp@parent-3.14.9:okhttp/src/main/java/okhttp3/internal/connection/Transmitter.java:83` "this.eventListener = client.eventListenerFactory().create(call);".
- **4.0.0**: `okhttp@parent-4.0.0:okhttp/src/main/java/okhttp3/internal/connection/Transmitter.kt:54` "private val eventListener: EventListener = client.eventListenerFactory.create(call)".
- **4.12.0**:
  - Field initialiser in the constructor: `okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/internal/connection/RealCall.kt:68` "internal val eventListener: EventListener = client.eventListenerFactory.create(this)".
  - `newCall`: `okhttp@parent-4.12.0:…/OkHttpClient.kt:268` "override fun newCall(request: Request): Call = RealCall(this, request, forWebSocket = false)".
  - `bytecode:okhttp-4.12.0.jar`, `RealCall.<init>`: "invokevirtual #51 // Method okhttp3/OkHttpClient.eventListenerFactory:()Lokhttp3/EventListener$Factory;" followed by "invokeinterface #57, 2 // InterfaceMethod okhttp3/EventListener$Factory.create:(Lokhttp3/Call;)Lokhttp3/EventListener;".
- **5.0.0**: `okhttp@parent-5.0.0:…/internal/connection/RealCall.kt:70` "internal val eventListener: EventListener = client.eventListenerFactory.create(this)".
- **5.5.0**:
  - `okhttp@parent-5.5.0:…/internal/connection/RealCall.kt:76-77` "@Volatile internal var eventListener: EventListener = client.eventListenerFactory.create(this)". It is now mutable, for `addEventListener`; see §3b.
  - `clone()`: `:155` "override fun clone(): Call = RealCall(client, originalRequest, forWebSocket)".
- **Implication for attach mode**: a `Call` object created before the hook was installed keeps the app's original listener, even if it is executed later.

### 2c. First versions and experimental status

- **Public API since 3.9.0**:
  - `okhttp@parent-3.9.0:…/OkHttpClient.java:898` "public Builder eventListener(EventListener eventListener) {" and `:910` "public Builder eventListenerFactory(EventListener.Factory eventListenerFactory) {".
  - In 3.8.1 it is still package-private: `okhttp@parent-3.8.1:…/OkHttpClient.java:898` "/*public*/ Builder eventListenerFactory(EventListener.Factory eventListenerFactory) {".
- **Never annotated `@Experimental`.** `okhttp@parent-3.9.0:…/EventListener.java:58` "public abstract class EventListener {" has no annotation. It was documented as unstable instead:
  - `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/EventListener.java:29-33` "<h3>Warning: This is a non-final API.</h3> … As of OkHttp 3.9, this feature is an unstable preview: the API is subject to change, and the implementation is incomplete. We expect that OkHttp 3.10 or 3.11 will finalize this API."
  - The same warning is on `Factory` at `:286-293`.
  - 3.10.0 repeats it: `okhttp@parent-3.10.0:…/EventListener.java:31` "As of OkHttp 3.10, this feature is an unstable preview".
- **Stable in 3.11.0**:
  - `okhttp@parent-3.11.0:CHANGELOG.md:32-33` "New: The `EventListener` API previewed in OkHttp 3.9 has graduated to a stable API."
  - The 3.9.0 entry of the same file (`:188-192`): "OkHttp has an experimental new API for tracking metrics. The new `EventListener` API … This feature is an unstable preview: the API is subject to change".
  - The 20-method set of 3.9.0 is unchanged in 3.11.0 (§3a).
- **`Factory` became `fun interface` in 4.9.0**: `okhttp@parent-4.9.0:okhttp/src/main/kotlin/okhttp3/EventListener.kt:460` "fun interface Factory {" (4.8.0 `:460` "interface Factory {"). The JVM shape is unchanged: `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:598-599` "public abstract interface class okhttp3/EventListener$Factory { public abstract fun create (Lokhttp3/Call;)Lokhttp3/EventListener;".

## 3. EventListener API

### 3a. Method list per version

All methods are `public void` with no-op bodies. Descriptors come from the 5.5.0 dump, `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:555-593`.

**The base 20, identical descriptors in 3.9.0 → 5.5.0** (line numbers are `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/EventListener.java`):

| Method | 3.9.0 line | 5.5.0 descriptor (dump line) |
|---|---|---|
| `callStart(Call)` | 78 | `(Lokhttp3/Call;)V` (564) |
| `dnsStart(Call, String)` | 90 | `(Lokhttp3/Call;Ljava/lang/String;)V` (574) |
| `dnsEnd(Call, String, List<InetAddress>)` | 98 | `(Lokhttp3/Call;Ljava/lang/String;Ljava/util/List;)V` (573) |
| `connectStart(Call, InetSocketAddress, Proxy)` | 110 | `(Lokhttp3/Call;Ljava/net/InetSocketAddress;Ljava/net/Proxy;)V` (568) |
| `secureConnectStart(Call)` | 125 | (592) |
| `secureConnectEnd(Call, Handshake)` | 133 | `(Lokhttp3/Call;Lokhttp3/Handshake;)V` (591) |
| `connectEnd(Call, InetSocketAddress, Proxy, Protocol)` | 143-144 | `(Lokhttp3/Call;Ljava/net/InetSocketAddress;Ljava/net/Proxy;Lokhttp3/Protocol;)V` (566) |
| `connectFailed(…, Protocol, IOException)` | 155-156 | `(…Lokhttp3/Protocol;Ljava/io/IOException;)V` (567) |
| `connectionAcquired(Call, Connection)` / `connectionReleased(Call, Connection)` | 165 / 176 | `(Lokhttp3/Call;Lokhttp3/Connection;)V` (569 / 570) |
| `requestHeadersStart(Call)` / `requestHeadersEnd(Call, Request)` | 188 / 199 | (583) / `(Lokhttp3/Call;Lokhttp3/Request;)V` (582) |
| `requestBodyStart(Call)` / `requestBodyEnd(Call, long)` | 212 / 220 | (580) / `(Lokhttp3/Call;J)V` (579) |
| `responseHeadersStart(Call)` / `responseHeadersEnd(Call, Response)` | 232 / 243 | (588) / `(Lokhttp3/Call;Lokhttp3/Response;)V` (587) |
| `responseBodyStart(Call)` / `responseBodyEnd(Call, long)` | 255 / 266 | (585) / (584) |
| `callEnd(Call)` / `callFailed(Call, IOException)` | 275 / 283 | (562) / `(Lokhttp3/Call;Ljava/io/IOException;)V` (563) |

3.10.0, 3.11.0, 3.12.0, 3.12.13 and 3.13.0 list exactly these 20 names (grep over each tag's `EventListener.java`).

**Additions**, all `open` no-ops except `plus`:

| Method | 5.5.0 descriptor (dump line) | First tag | Evidence |
|---|---|---|---|
| `requestFailed(Call, IOException)` | (581) | 3.14.0 | `okhttp@parent-3.14.0:okhttp/src/main/java/okhttp3/EventListener.java:219` "public void requestFailed(Call call, IOException ioe) {" (absent at 3.13.1) |
| `responseFailed(Call, IOException)` | (586) | 3.14.0 | same file `:274` |
| `proxySelectStart(Call, HttpUrl)` | `(Lokhttp3/Call;Lokhttp3/HttpUrl;)V` (578) | 4.1.0 | `okhttp@parent-4.1.0:okhttp/src/main/java/okhttp3/EventListener.kt:71` "open fun proxySelectStart(" (absent at 4.0.1) |
| `proxySelectEnd(Call, HttpUrl, List<Proxy>)` | `(Lokhttp3/Call;Lokhttp3/HttpUrl;Ljava/util/List;)V` (577) | 4.1.0 | same file `:92` |
| `canceled(Call)` | (565) | 4.4.0 | `okhttp@parent-4.4.0:okhttp/src/main/java/okhttp3/EventListener.kt:420` "open fun canceled(" |
| `satisfactionFailure(Call, Response)` | (590) | 4.7.0 | `okhttp@parent-4.7.0:okhttp/src/main/kotlin/okhttp3/EventListener.kt:429` "open fun satisfactionFailure(call: Call, response: Response) {" |
| `cacheHit(Call, Response)` / `cacheMiss(Call)` / `cacheConditionalHit(Call, Response)` | (560) / (561) / (559) | 4.7.0 | same file `:438` / `:447` / `:457` |
| `retryDecision(Call, IOException, boolean)` | `(Lokhttp3/Call;Ljava/io/IOException;Z)V` (589) | 5.0.0-alpha.16 | `okhttp@parent-5.0.0-alpha.16:okhttp/src/commonJvmAndroid/kotlin/okhttp3/EventListener.kt:478`; `okhttp@parent-5.5.0:CHANGELOG.md:370` "New: `EventListener.retryDecision()` is called each time a request fails with an `IOException`." |
| `followUpDecision(Call, Response, Request?)` | `(Lokhttp3/Call;Lokhttp3/Response;Lokhttp3/Request;)V` (575) | 5.0.0-alpha.16 | same file `:506`; `CHANGELOG.md:373` |
| `dispatcherQueueStart(Call, Dispatcher)` / `dispatcherQueueEnd(Call, Dispatcher)` | `(Lokhttp3/Call;Lokhttp3/Dispatcher;)V` (572 / 571) | 5.2.0 | `okhttp@parent-5.2.0:…/EventListener.kt:81` / `:92`; `okhttp@parent-5.5.0:CHANGELOG.md:174-175` "New: Publish events when calls must wait to execute. `EventListener.dispatcherQueueStart()` …" |
| **final** `plus(EventListener): EventListener` | "public final fun plus (Lokhttp3/EventListener;)Lokhttp3/EventListener;" (576) | 5.3.0 | `okhttp@parent-5.3.0:…/EventListener.kt:539` "operator fun plus(other: EventListener): EventListener {"; `okhttp@parent-5.5.0:CHANGELOG.md:114` "New: `EventListener.plus()` makes it easier to observe events in multiple listeners." |

**Counts by version:**

| Versions | Callbacks |
|---|---|
| 3.9–3.13 | 20 |
| 3.14.x, 4.0.x | 22 |
| 4.1–4.3 | 24 |
| 4.4–4.6 | 25 |
| 4.7–4.12 | 29 (`okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/EventListener.kt:69-457`) |
| 5.0–5.1 | 31 |
| 5.2–5.5 | 33 |

**Signature changes: none.** Only nullability annotations changed, and those are not part of the descriptor:
- 3.9.0 `dnsEnd(Call call, String domainName, @Nullable List<InetAddress> inetAddressList)` (`:98`) became `List<InetAddress>` in `okhttp@parent-3.10.0:…/EventListener.java:98`.
- 3.9.0 `connectEnd(… @Nullable Proxy proxy, @Nullable Protocol protocol)` (`:143-144`) likewise.
- Kotlin `List<@JvmSuppressWildcards InetAddress>` (`okhttp@parent-4.12.0:…/EventListener.kt:133`) affects only the generic signature.

### 3b. Still an abstract class with no-op defaults in 5.x

- The dump shows `public abstract class okhttp3/EventListener { … public fun <init> ()V … public fun callStart (Lokhttp3/Call;)V …`. Every callback is `public fun` (non-final, non-abstract); only `plus` is `final` (`okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:555-593`).
- Source: `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/EventListener.kt:63` "abstract class EventListener {" and `:71-72` "open fun callStart(call: Call) {  }".
- So a Java subclass compiled against any older version links and runs on 5.x. Callbacks it does not override fall back to the base no-ops.
- **Per-call listeners (5.4+)**:
  - `okhttp@parent-5.4.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/Call.kt:102` "fun addEventListener(eventListener: EventListener)" (absent at 5.3.0). It is implemented as an atomic `previous + eventListener` in `okhttp@parent-5.5.0:…/internal/connection/RealCall.kt:133-137`.
  - The chain exposes the call's listener: `okhttp@parent-5.5.0:…/Interceptor.kt:296` "val eventListener: EventListener"; `okhttp@parent-5.5.0:…/internal/http/RealInterceptorChain.kt:161-162` "override val eventListener: EventListener get() = call.eventListener".
- **AggregateEventListener** (behind `plus`) overrides every callback of its runtime. There are 33 `open fun`s and 33 `override fun`s in both 5.3.0 and 5.5.0 (awk count over `…/EventListener.kt`). `plus` itself is at `okhttp@parent-5.5.0:…/EventListener.kt:538-554`: "Returns a new `EventListener` that publishes events to this and then `other`."

### 3c. `callStart()` thread for `execute()` vs `enqueue()`

In every version, `callStart` is invoked synchronously inside `execute()` / `enqueue()` on the **caller's thread**, before the call is handed to the dispatcher:

- **3.9.0**:
  - execute: `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/RealCall.java:73-74` "captureCallStackTrace(); eventListener.callStart(this);"
  - enqueue: `:98-100` "captureCallStackTrace(); eventListener.callStart(this); client.dispatcher().enqueue(new AsyncCall(responseCallback));"
- **3.12.13**: `okhttp@parent-3.12.13:…/RealCall.java:89-90` "timeout.enter(); eventListener.callStart(this);" (execute) and `:126-127` "eventListener.callStart(this); client.dispatcher().enqueue(new AsyncCall(responseCallback));" (enqueue).
- **3.14.9**: `okhttp@parent-3.14.9:…/RealCall.java:77-78` "transmitter.timeoutEnter(); transmitter.callStart();" and `:92-93` "transmitter.callStart(); client.dispatcher().enqueue(new AsyncCall(responseCallback));". These go to `okhttp@parent-3.14.9:…/internal/connection/Transmitter.java:115-117` "public void callStart() { this.callStackTrace = Platform.get().getStackTraceForCloseable("response.body().close()"); eventListener.callStart(call);".
- **4.0.0**: `okhttp@parent-4.0.0:okhttp/src/main/java/okhttp3/RealCall.kt:62-63` and `:77-78`, via `…/internal/connection/Transmitter.kt:110-112`.
- **4.12.0**: `okhttp@parent-4.12.0:…/internal/connection/RealCall.kt:150-151` "timeout.enter() / callStart()", `:163-164` "callStart() / client.dispatcher.enqueue(AsyncCall(responseCallback))", and `:169-171` "private fun callStart() { this.callStackTrace = … eventListener.callStart(this)".
- **5.0.0**: `okhttp@parent-5.0.0:…/RealCall.kt:156-157`, `:169-170`, `:175-177`.
- **5.5.0**: `okhttp@parent-5.5.0:…/RealCall.kt:185-186`, `:198-199`, `:204-206`.

The async work itself runs on a dispatcher thread: `okhttp@parent-4.12.0:…/RealCall.kt:512-517` "override fun run() { threadName("OkHttp ${redactedUrl()}") { … val response = getResponseWithInterceptorChain()" (3.12.13 `:197-201`; 5.5.0 `:577-582`).

So the stack captured in `callStart()` is the app's caller stack for both paths. Interceptors run on the caller thread for `execute()` and on an OkHttp dispatcher thread for `enqueue()`.

**5.x caveat: callbacks are not all on the call's thread.**
- Fast fallback is on by default: `okhttp@parent-5.5.0:…/OkHttpClient.kt:597` "internal var fastFallback = true" (5.0.0 `:602`; the flag does not exist in 4.12.0).
- TCP connects then run as TaskRunner tasks: `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/internal/connection/FastFallbackExchangeFinder.kt:139-147` "// Connect TCP asynchronously. … taskRunner.newQueue().schedule( object : Task(taskName) { override fun runOnce(): Long { … plan.connectTcp()".
- `connectTcp` fires `call.eventListener.connectStart(…)` and `connectFailed(…)` (`…/internal/connection/ConnectPlan.kt:141`, `:156`).
- Changelog: "When we introduced fast fallback in OkHttp 5.0, we started using background threads while connecting" (`okhttp@parent-5.5.0:CHANGELOG.md:183-185`).
- Our listener must therefore be thread-safe and must not correlate events by thread.

### 3d. When can a delegating wrapper hit `NoSuchMethodError`?

The JVM/ART linkage points here are reasoned, not verified: **UNVERIFIED** in this task.

- **OkHttp-invoked callbacks cannot trigger it.** OkHttp only invokes callbacks that exist in its own `EventListener`, and the app's delegate is an instance of that same runtime class.
  - A wrapper compiled against 5.5.0 that overrides `cacheHit(…) { delegate.cacheHit(…) }` is simply never invoked on 3.x/4.0–4.6.
  - The unresolved `okhttp3/EventListener.cacheHit` reference is only resolved if executed. JVMS §5.4 says resolution errors surface at the point of use (UNVERIFIED here).
  - Every parameter type of all 33 callbacks (`Call`, `String`, `List`, `InetSocketAddress`, `Proxy`, `Handshake`, `Protocol`, `IOException`, `Connection`, `Request`, `Response`, `HttpUrl`, `Dispatcher`, `J`, `Z`) exists since 3.9.0, so loading the subclass needs no missing classes.
- **It occurs only when our own code executes a reference that is missing at runtime.** Examples:
  - replaying or synthesising events outside the OkHttp-invoked callback;
  - calling `plus()` on < 5.3.0 or `Call.addEventListener` on < 5.4.0;
  - in general, calling any OkHttp or Okio member newer than the app's version.
- **The opposite failure is silent.** A wrapper compiled against an *older* version does not override newer callbacks. On a newer runtime those events land in the base no-op and are **not forwarded** to the app's original listener, so the app loses `cacheHit`, `canceled`, `retryDecision` and so on. The same applies to callbacks added after our compile target.
- **Never declare `plus(Lokhttp3/EventListener;)Lokhttp3/EventListener;` in a subclass.** It is `final` since 5.3.0 (`okhttp.api:576`), and overriding a final method fails class loading (JVM rule, UNVERIFIED here). The wrapper should declare exactly the callbacks and nothing else that is public.
- **Precedent: mangled `internal` names.**
  - OkHttp 5.0.0 changelog: "Fix: Don't crash with a `NoSuchMethodError` when using OkHttp with the Sentry SDK." (`okhttp@parent-5.5.0:CHANGELOG.md:244`).
  - Commit 75661d41c, "Fix a NoSuchMethodError loading OkHttp on Android (#8898)", adds `-module-name=okhttp` to `okhttp/build.gradle.kts` because "the Sentry SDK assumes that OkHttp's internal-visibility symbols will be suffixed '$okhttp' in deployable artifacts. This isn't intended to be a published API".
  - Lesson: never link against Kotlin `internal` members such as `RealCall.getEventListener$okhttp()` or `getResponseWithInterceptorChain$okhttp()`, both visible in `bytecode:okhttp-4.12.0.jar`.

### 3e. Compile target for the listener

- Compile **only the forwarding `EventListener` subclass** against **OkHttp 5.5.0** and override-and-delegate all 33 callbacks.
- At runtime on ≥ 5.3.0, prefer `appListener.plus(ourListener)`. Detect `plus` once, reflectively. OkHttp's own `AggregateEventListener` then forwards every callback of that runtime, including any added after 5.5.0.
- Below 5.3.0, use the forwarder. The set of callbacks up to 5.2.x is closed, and the 5.5.0 list covers it.

## 4. Interceptor / Chain

**`Interceptor.intercept(Chain)` is stable.**
- Declarations:
  - 3.x: `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/Interceptor.java:28` "Response intercept(Chain chain) throws IOException;".
  - 4.0–4.8: `interface Interceptor` (`okhttp@parent-4.0.0:okhttp/src/main/java/okhttp3/Interceptor.kt:26-28`).
  - 4.9.0+: `fun interface Interceptor` (`okhttp@parent-4.9.0:okhttp/src/main/kotlin/okhttp3/Interceptor.kt:59`; `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/Interceptor.kt:66-68`).
- `fun interface` does **not** change the JVM shape: `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:810-813` "public abstract interface class okhttp3/Interceptor { public static final field Companion Lokhttp3/Interceptor$Companion; public abstract fun intercept (Lokhttp3/Interceptor$Chain;)Lokhttp3/Response;" (5.0.0 `:718-721`).
- The `Companion` field exists since 4.0.0 (`okhttp@parent-4.0.0:…/Interceptor.kt:30` "companion object {").
- A Java class implementing `okhttp3.Interceptor` works across all versions.

**`Chain` members** (5.5.0 descriptors from `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:815-856`):

| Member | 5.5.0 descriptor | Since | Notes and evidence |
|---|---|---|---|
| `request()` | `()Lokhttp3/Request;` | ≤ 3.5 | `okhttp@parent-3.5.0:…/Interceptor.java:31` "Response proceed(Request request) throws IOException;" |
| `proceed(Request)` | `(Lokhttp3/Request;)Lokhttp3/Response;` | ≤ 3.5 | same |
| `connection()` | `()Lokhttp3/Connection;` | ≤ 3.5 | Nullable. "This is only available in the chains of network interceptors; for application interceptors this is always null." (`okhttp@parent-3.9.0:…/Interceptor.java:35-39`; `okhttp@parent-5.5.0:…/Interceptor.kt:90-94`). 4.x/5.x implementation: "override fun connection(): Connection? = exchange?.connection" (`okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/internal/http/RealInterceptorChain.kt:59`). |
| `call()` | `()Lokhttp3/Call;` | **3.9.0** | `okhttp@parent-3.9.0:…/Interceptor.java:41` "Call call();". 3.8.1's `Chain` has only `request`/`proceed`/`connection`. Changelog 3.9.0: "The `Chain` interface now offers access to the call and can adjust all call timeouts." (`okhttp@parent-3.11.0:CHANGELOG.md:183-185`). Returns the same `RealCall` that `EventListener` receives: 3.9.0 builds the chain with `this` (`RealCall.java:196-198`) and creates the listener with `create(call)` (`:60`); in 5.5.0, `RealInterceptorChain(call = this, …)` (`RealCall.kt:224-230`) and `RealInterceptorChain.kt:307` "override fun call(): Call = call". |
| Timeouts: `connectTimeoutMillis`, `withConnectTimeout(int, TimeUnit)`, … | — | 3.9.0 | `okhttp@parent-3.9.0:…/Interceptor.java:43-53` |
| 30 new abstract members (`getDns/withDns`, `getCache/withCache`, …, `getEventListener`) | — | **5.4.0** | The dump has 10 abstract `Chain` methods in 5.0.0–5.3.0 and 40 in 5.4.0/5.5.0. Changelog 5.4.0: "Interceptors can now override anything settable on `OkHttpClient.Builder`" (`okhttp@parent-5.5.0:CHANGELOG.md:65-68`). **netinspect must never implement `Interceptor.Chain`.** |

**"Exactly once" check.** It lives in `RealInterceptorChain.proceed` and applies only when the chain carries a codec/exchange, i.e. to network interceptors:
- **3.9.0 / 3.12.13**, `okhttp@parent-3.12.13:okhttp/src/main/java/okhttp3/internal/http/RealInterceptorChain.java` (3.9.0 has the same messages at `:139`, `:152`):
  - `:136-140` "// If we already have a stream, confirm that this is the only call to chain.proceed(). if (this.httpCodec != null && calls > 1) { throw new IllegalStateException("network interceptor " + interceptors.get(index - 1) + " must call proceed() exactly once");"
  - `:149-153` "// Confirm that the next interceptor made its required call to chain.proceed(). if (httpCodec != null && index + 1 < interceptors.size() && next.calls != 1) {"
  - Also `:130-134` "must retain the same host and port" and `:160-163` "returned a response with no body".
- **3.14.9**: `okhttp@parent-3.14.9:…/RealInterceptorChain.java:135`, `:147` (same messages, keyed on `exchange`).
- **4.0.0**: `okhttp@parent-4.0.0:okhttp/src/main/java/okhttp3/internal/http/RealInterceptorChain.kt:103`, `:117`.
- **4.12.0**: `okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/internal/http/RealInterceptorChain.kt`:
  - `:95-101` "if (exchange != null) { check(exchange.finder.sameHostAndPort(request.url)) {…} check(calls == 1) { "network interceptor ${interceptors[index - 1]} must call proceed() exactly once" }"
  - `:112-116` "check(index + 1 >= interceptors.size || next.calls == 1)"
  - `:118` "check(response.body != null) { "interceptor $interceptor returned a response with no body" }"
- **5.0.0**: `okhttp@parent-5.0.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/internal/http/RealInterceptorChain.kt:111-117`, `:130-133`. The body-null check is gone because the body is non-null.
- **5.5.0**: `okhttp@parent-5.5.0:…/internal/http/RealInterceptorChain.kt:317-323`, `:336-339`.

**Consequences for netinspect:**
1. A network interceptor cannot return a synthetic response without calling `proceed()`; the post-check throws `IllegalStateException`. Full mocking belongs in an application interceptor.
2. Throwing an `IOException` **before** `proceed()` is allowed. It propagates out of `interceptor.intercept(next)` before the post-check runs.
3. The request passed to `proceed` must keep its host and port.
4. **Injected failures may be retried.** `RetryAndFollowUpInterceptor` treats a chain `IOException` as "may have been sent" and retries when the failure is recoverable and routes remain:
   - `okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/internal/http/RetryAndFollowUpInterceptor.kt:87-95` "catch (e: IOException) { // An attempt to communicate with a server failed. The request may have been sent. if (!recover(e, call, request, requestSendStarted = e !is ConnectionShutdownException)) {"
   - `isRecoverable` returns false for `ProtocolException`, for `InterruptedIOException` unless it is a `SocketTimeoutException && !requestSendStarted`, for `SSLHandshakeException` caused by `CertificateException`, and for `SSLPeerUnverifiedException`; otherwise true (`:172-200`).
   - `recover` also requires `client.retryOnConnectionFailure` and a body that is not one-shot (`:151-160`).
   - 5.5.0 is the same (`…/RetryAndFollowUpInterceptor.kt:74-81`, `:133-198`), now reading `chain.retryOnConnectionFailure` and emitting `retryDecision`. 3.12.13: `:136-141`, `:220-270`.
   - So an injected generic `IOException` or `ConnectException` can re-run our interceptor. Inject `ProtocolException`, or a post-send `SocketTimeoutException`, for a failure that will not be retried.
5. **Non-`IOException`s crash async calls.**
   - Kdoc: "Other exception types cancel the current call … For asynchronous calls made with [Call.enqueue] … The interceptor's exception is delivered to the current thread's [uncaught exception handler] … By default this crashes the application on Android" (`okhttp@parent-5.5.0:…/Interceptor.kt:37-45`).
   - Code: `okhttp@parent-3.12.13:…/RealCall.java:213-219` "catch (Throwable t) { cancel(); … throw t;"; 4.12.0 `RealCall.kt:527-534`; 5.5.0 `RealCall.kt:592-603`.
   - All netinspect logic inside `intercept()` must catch `Throwable` from our own code.

## 5. Connection details

Kotlin properties carry `@get:JvmName("x")`, so the bytecode getter stays `x()`; for example `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/Handshake.kt:38` "@get:JvmName("tlsVersion") val tlsVersion: TlsVersion,". The old function forms remain as ERROR-deprecated `-deprecated_x` methods, for example `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:635` "public final fun -deprecated_cipherSuite ()Lokhttp3/CipherSuite;". Java cannot call these.

| API | 3.x (3.9.0 = 3.14.9 unless noted) | 4.12.0 bytecode | 5.5.0 dump | Notes |
|---|---|---|---|---|
| `Connection.protocol()` | `okhttp@parent-3.9.0:okhttp/src/main/java/okhttp3/Connection.java:93` "Protocol protocol();" | "public abstract okhttp3.Protocol protocol();" | `okhttp.api:347` "public abstract fun protocol ()Lokhttp3/Protocol;" | "This method returns [Protocol.HTTP_1_1] even if the remote peer is using [Protocol.HTTP_1_0]." (`okhttp@parent-5.5.0:…/Connection.kt:87-89`) |
| `Connection.handshake()` | `Connection.java:86` "@Nullable Handshake handshake();" | "public abstract okhttp3.Handshake handshake();" | `:346` "handshake ()Lokhttp3/Handshake;" | Nullable in every version: "or null if the connection is not HTTPS" (`…5.5.0…/Connection.kt:81-84`) |
| `Connection.route()` | `Connection.java:73` "Route route();" | "public abstract okhttp3.Route route();" | `:348` | |
| `Connection.socket()` | `Connection.java:80` "Socket socket();" | "public abstract java.net.Socket socket();" | `:349` "socket ()Ljava/net/Socket;" | Still `java.net.Socket` in 5.x. The separate `Response.socket()` (5.2+) returns `okio.Socket`. |
| `Route.socketAddress()` | `okhttp@parent-3.9.0:…/Route.java:71` "public InetSocketAddress socketAddress() {" | "public final java.net.InetSocketAddress socketAddress();" | `:1327` | This is the **proxy's** address when a proxy is in use: "whether connecting directly to an origin server or a proxy, opening a socket requires an IP address" (`okhttp@parent-5.5.0:…/Route.kt:29-30`) |
| `Route.proxy()` / `address()` | `Route.java:67` / `:57` | "public final java.net.Proxy proxy();" / "public final okhttp3.Address address();" | — | 5.5.0 adds `echConfigList` (`…/Route.kt:44` "@get:JvmName("echConfigList") val echConfigList: ByteString?") and keeps the 3-argument constructor (`:46-50`) |
| `Handshake.tlsVersion()` | `okhttp@parent-3.9.0:…/Handshake.java:88` "public TlsVersion tlsVersion() {" (3.14.9 `:94`) | "public final okhttp3.TlsVersion tlsVersion();" | `:650` | Non-null in every version; no type change |
| `Handshake.cipherSuite()` | `:93` (3.14.9 `:99`) | "public final okhttp3.CipherSuite cipherSuite();" | `:641` | |
| `Handshake.peerCertificates()` | `:98` (3.14.9 `:104`) | "public final java.util.List<java.security.cert.Certificate> peerCertificates();" | `:648` | Lazy in 4.12/5.x: "val peerCertificates: List<Certificate> by lazy { try { peerCertificatesFn() } catch (spue: SSLPeerUnverifiedException) { listOf() }" (`okhttp@parent-5.5.0:…/Handshake.kt:47-54`). The first call may run certificate-chain cleaning on our thread (the fork's reading of `…/internal/connection/ConnectPlan.kt:396-400`, not independently re-checked). |
| `Handshake.localCertificates()` | `:110` (3.14.9 `:116`) | present | — | |
| `TlsVersion.javaName()` | `okhttp@parent-3.9.0:…/TlsVersion.java:64` "public String javaName() {" | "public final java.lang.String javaName();" | `:1341` | |
| `CipherSuite.javaName()` | `…/CipherSuite.java:437` (3.14.9 `:466`) | "public final java.lang.String javaName();" | `:326` | |
| `Protocol.toString()` | `okhttp@parent-3.14.9:…/Protocol.java:112-113` "@Override public String toString() { return protocol;" (3.9.0 `:88-89`) | "public java.lang.String toString();" | `:1096` | Returns the ALPN id, e.g. "http/1.1", "h2" |

**`Protocol` constants.**
- 3.9.0 has `HTTP_1_0`, `HTTP_1_1`, `SPDY_3`, `HTTP_2` (`okhttp@parent-3.9.0:…/Protocol.java:33`, `:41`, `:51`, `:62`).
- 3.14.9 adds `H2_PRIOR_KNOWLEDGE("h2_prior_knowledge")` at `:71` (added in 3.11.0) and `QUIC("quic")` at `:81` (added in 3.10.0), per the fork's bisection.
- 5.x adds `HTTP_3("h3")`, first at `okhttp@parent-5.0.0-alpha.4:okhttp/src/commonMain/kotlin/okhttp3/Protocol.kt:94` (fork's bisection).
- Nothing was removed. **Compare `toString()` strings; never reference newer constants.** Referencing a missing constant throws `NoSuchFieldError` on older runtimes.

**Nothing in this group was renamed or removed from 3.9 to 5.5.** The 4.x build fails on binary-incompatible changes versus 3.14.1:
- `okhttp@parent-4.12.0:okhttp/build.gradle:53-57` "task japicmp(…) { … onlyBinaryIncompatibleModified = true / failOnModification = true"
- `okhttp@parent-4.12.0:build.gradle:381` "ext.baselineVersion = "3.14.1""

## 6. Request/Response/Headers/Body API stability for Java callers

(Sub-investigation, spot-checked. The 4.12.0 column is `javap -public` over `okhttp-4.12.0.jar`; the 5.x column is `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api`.)

| API | 3.x | 4.12.0 bytecode | 5.5.0 dump | Notes |
|---|---|---|---|---|
| `Request.url/method/headers/header(String)/body/newBuilder` | `okhttp@parent-3.12.13:okhttp/src/main/java/okhttp3/Request.java:48` "public HttpUrl url() {", `:52` method, `:56` headers, `:60` "public @Nullable String header(String name) {", `:68` "public @Nullable RequestBody body() {", `:92` newBuilder | "public final okhttp3.HttpUrl url();" … "public final okhttp3.RequestBody body();" "public final okhttp3.Request$Builder newBuilder();" | `:1129` url, `:1120` method, `:1117` "headers ()Lokhttp3/Headers;", `:1113` "body ()Lokhttp3/RequestBody;", `:1121` newBuilder | The body is still nullable in 5.x: `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/Request.kt:46-47` "@get:JvmName("body") val body: RequestBody? = builder.body" |
| `Request.tag()` / `tag(Class)` | `Request.java:80` / `:88`. `tag(Class)` is 3.11+. | present | present | |
| `HttpUrl.toString/host/port/scheme/encodedPath/query` | `okhttp@parent-3.9.0:…/HttpUrl.java` scheme `:391`, host `:484`, port `:500`, encodedPath `:544`, query `:671` (nullable), toString `:948` "return url;" (fork) | "public final java.lang.String scheme();" … "public final java.lang.String query();" "public java.lang.String toString();" | host 741, port 749, scheme 759, encodedPath 731, query 750, toString 760 (fork) | Stable |
| `Headers.size()/name(int)/value(int)` | `okhttp@parent-3.14.9:…/Headers.java:88/93/98` (fork); 3.x `Headers` is **not** `Iterable` | "public final int size();" "public final java.lang.String name(int);" "public final java.lang.String value(int);"; the class "implements java.lang.Iterable<kotlin.Pair<…>>" | `:676` "size ()I", `:671` "name (I)Ljava/lang/String;", `:679` value | Index by `size/name/value`; never iterate (4.x/5.x yield `kotlin.Pair`) |
| `RequestBody.contentType/contentLength/writeTo` | `okhttp@parent-3.9.0:…/RequestBody.java:30`, `:36` "public long contentLength() throws IOException", `:41` "writeTo(BufferedSink sink) throws IOException" (fork) | "public abstract okhttp3.MediaType contentType();" "public long contentLength() throws java.io.IOException;" "public abstract void writeTo(okio.BufferedSink) throws java.io.IOException;" | `:1165` abstract contentType, `:1164` "public fun contentLength ()J", `:1184` "public abstract fun writeTo (Lokio/BufferedSink;)V" | |
| `RequestBody.isDuplex()/isOneShot()` | **first in 3.14.0**: `okhttp@parent-3.14.0:okhttp/src/main/java/okhttp3/RequestBody.java:76` "public boolean isDuplex() {", `:92` "public boolean isOneShot() {"; absent at 3.13.1 | "public boolean isDuplex();" "public boolean isOneShot();" | `:1181` "public fun isDuplex ()Z", `:1182` "public fun isOneShot ()Z" | Open; the wrapper must forward them |
| `RequestBody.create(MediaType, String/byte[])` statics | 3.x static (fork: `…/RequestBody.java:47`, `:79`, …) | "public static final okhttp3.RequestBody create(okhttp3.MediaType, java.lang.String);" | `:1170` "public static final fun create (Lokhttp3/MediaType;Ljava/lang/String;)Lokhttp3/RequestBody;", `:1172` `([B)` | 3.x argument order is still static in 5.x; reversed order is 4.x+ only |
| `ResponseBody` abstract API | `okhttp@parent-3.14.9:…/ResponseBody.java:107/113/119` (fork) | "public abstract okhttp3.MediaType contentType();" "public abstract long contentLength();" "public abstract okio.BufferedSource source();" "public void close();" | `:1277` "public abstract class okhttp3/ResponseBody : java/io/Closeable {", `:1280` "public fun <init> ()V", `:1286` "public abstract fun contentLength ()J", `:1287` "public abstract fun contentType ()Lokhttp3/MediaType;", `:1296` "public abstract fun source ()Lokio/BufferedSource;", `:1285` "public fun close ()V" | **Yes:** still abstract, Java-subclassable, and exactly these three abstract methods |
| `ResponseBody.create(MediaType, …)` statics | `okhttp@parent-3.14.9:…/ResponseBody.java:199/213/225` (fork); `(MediaType, ByteString)` 3.11+ | "public static final okhttp3.ResponseBody create(okhttp3.MediaType, java.lang.String);" / "(okhttp3.MediaType, byte[])" / "(okhttp3.MediaType, long, okio.BufferedSource)" | `:1290` `(Lokhttp3/MediaType;Ljava/lang/String;)`, `:1292` `(Lokhttp3/MediaType;[B)`, `:1289` `(Lokhttp3/MediaType;JLokio/BufferedSource;)` | Still static in 5.x; `ResponseBody.EMPTY` (`:1279`) is 5.x-only |
| `Response.code/message/protocol/headers/request/handshake/newBuilder/header(String)/peekBody/networkResponse/cacheResponse/priorResponse/sentRequestAtMillis/receivedResponseAtMillis` | `okhttp@parent-3.12.13:okhttp/src/main/java/okhttp3/Response.java`: request `:85`, protocol `:92`, code `:97`, message `:110`, handshake `:118`, header `:126`, headers `:135`, peekBody `:150`, newBuilder `:180`, networkResponse `:204`, cacheResponse `:213`, priorResponse `:223`, sentRequestAtMillis `:264`, receivedResponseAtMillis `:273` | "public final int code();" "public final okhttp3.Response$Builder newBuilder();" "public final long sentRequestAtMillis();" … (all present) | code 1231, message 1240, protocol 1246, headers 1236, request 1248, newBuilder 1242 (fork) | All stable. 5.x-only additions: `peekTrailers()` (5.1), `socket()Lokio/Socket;` (5.2) |
| `Response.body()` | `Response.java:176` "public @Nullable ResponseBody body() {" | "public final okhttp3.ResponseBody body();" | `:1226` "public final fun body ()Lokhttp3/ResponseBody;" — **non-null**: `okhttp@parent-5.5.0:…/Response.kt:80` "@get:JvmName("body") val body: ResponseBody," | Descriptor unchanged. Non-null since `okhttp@parent-5.0.0-alpha.7:okhttp/src/jvmMain/kotlin/okhttp3/Response.kt:64` "@get:JvmName("body") actual val body: ResponseBody,". Changelog: "`Response.body` is now non-null. … In such cases the body is now non-null, but attempts to read its content will fail." (`okhttp@parent-5.5.0:CHANGELOG.md:662-665`) |
| `Response.Builder.body/code/message/header/removeHeader/headers/build` | `okhttp@parent-3.12.13:okhttp/src/main/java/okhttp3/Response.java:390` "public Builder body(@Nullable ResponseBody body) {", `:346` code, `:351` message, `:365` header, `:379` removeHeader, `:385` headers, `:441` build | "public okhttp3.Response$Builder body(okhttp3.ResponseBody);" … "public okhttp3.Response build();" | `:1258` "public fun body (Lokhttp3/ResponseBody;)Lokhttp3/Response$Builder;", `:1259` build | 5.x: `okhttp@parent-5.5.0:…/Response.kt:451` "open fun body(body: ResponseBody) =", default `:360` "internal var body: ResponseBody = ResponseBody.EMPTY". **Passing `null` throws**: `bytecode:okhttp-jvm-5.1.0.jar` `Response$Builder.body` begins with "invokestatic #64 // Method kotlin/jvm/internal/Intrinsics.checkNotNullParameter"; 4.12.0 has no such check. New in 5.x: `socket(Lokio/Socket;)` (`:1273`), `trailers(…)` |
| `MediaType.toString/charset()/parse` | `okhttp@parent-3.9.0:…/MediaType.java:51` "static @Nullable parse" (fork); `get` 3.11+ | "public static final okhttp3.MediaType parse(java.lang.String);" "public final java.nio.charset.Charset charset();" | `:873` "public static final fun parse (Ljava/lang/String;)Lokhttp3/MediaType;", `:866` "charset ()Ljava/nio/charset/Charset;", `:875` toString | Use `parse`, never `get` |

**Response internals that matter for rewriting** (fork, spot-checked):
- `newBuilder()` copies the internal exchange, `socket` and trailers: `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/Response.kt:374` "internal constructor(response: Response) {", `:382` "this.socket = response.socket", `:388` "this.exchange = response.exchange", `:389` "this.trailersSource = response.trailersSource".
- **101 upgrade bodies:**
  - 3.x/4.x attach a readable empty body only for `forWebSocket` calls: `okhttp@parent-3.12.13:…/internal/http/CallServerInterceptor.java:117-121` "if (forWebSocket && code == 101) { // Connection is upgrading, but we need to ensure interceptors see a non-null response body. response = response.newBuilder() .body(Util.EMPTY_RESPONSE)".
  - 5.0.0 strips instead: `okhttp@parent-5.0.0:…/internal/http/CallServerInterceptor.kt:128-130` "if (forWebSocket && code == 101) { … response.stripBody()".
  - 5.5.0 handles any HTTP/1 upgrade: `okhttp@parent-5.5.0:…/internal/http/CallServerInterceptor.kt:43` "val isUpgradeRequest = "upgrade".equals(request.header("Connection"), ignoreCase = true)" and `:141-150` "isUpgradeRequest && isUpgradeResponse -> { response .newBuilder() .body( UnreadableResponseBody( …)).socket(exchange.upgradeToSocket())".
  - Reading such a body throws: `okhttp@parent-5.5.0:…/internal/UnreadableResponseBody.kt:41-43` "throw IllegalStateException( … |Unreadable ResponseBody! These Response objects have bodies that are stripped:".
- **The replacement body must close the original.** If our rewrite swaps the body, OkHttp still needs the original network body closed before a follow-up. Otherwise the next hop fails:
  - `okhttp@parent-4.12.0:…/internal/connection/RealCall.kt:229-231` "check(!responseBodyOpen) { "cannot make a new request because the previous response is still open: " + "please call response.close()"" (5.5.0 `:266-268`).
  - 3.x: `okhttp@parent-3.12.13:…/internal/http/RetryAndFollowUpInterceptor.java:189-191` "throw new IllegalStateException("Closing the body of " + response + " didn't close its backing stream. Bad interceptor?");".

**Binary-incompatible changes a Java library would hit:**
- **3.12 → 3.14**: additions only for the APIs above.
  - New: `Response.trailers()` (3.13), `isDuplex/isOneShot` (3.14).
  - One public field was renamed: `CipherSuite.TLS_AES_256_CCM_8_SHA256` became `TLS_AES_128_CCM_8_SHA256` (fork: 3.12.13 `:403` vs 3.14.9 `:401`). Irrelevant if only `javaName()` is used.
- **3.x → 4.x**: binary compatible apart from japicmp-excluded items.
  - `okhttp@parent-4.12.0:docs/upgrading_to_okhttp_4.md:23-24` "With a few small exceptions (below), OkHttp 4.x is both binary- and Java source-compatible with OkHttp 3.x. You can use an OkHttp 4.x .jar file with applications or libraries built for OkHttp 3.x."
  - The exceptions: the 26 `OkHttpClient` accessors became final (`okhttp@parent-4.12.0:okhttp/build.gradle:82` "methodExcludes = [", `:94` "'okhttp3.OkHttpClient#eventListenerFactory()',", `:99` "'okhttp3.OkHttpClient#networkInterceptors()',"), plus `Request$Builder#delete()` (`:111`). This only matters to subclasses and mocks.
- **4.12 → 5.x**: the only public removal is `OkHttpClient.clone()` together with `Cloneable`.
  - `okhttp@parent-5.5.0:CHANGELOG.md:673-675` "Fix: `OkHttpClient` no longer implements `Cloneable`."
  - The class header is now `okhttp.api:964` "public class okhttp3/OkHttpClient : okhttp3/Call$Factory, okhttp3/WebSocket$Factory {".
  - Project policy: `okhttp@parent-5.5.0:CHANGELOG.md:316-317` "Note that any _Breaking_ changes above impact only APIs introduced in earlier 5.0.0-alpha releasees. We don't break binary compatibility with non-alpha APIs."
- **Runtime-only changes on 5.x:**
  - `body()` is never null.
  - `Builder.body(null)` throws NPE.
  - Stripped bodies (`networkResponse`, `cacheResponse`, `priorResponse`) and 101 bodies throw on read.
  - `Chain` implementers must provide 30 more abstract methods from 5.4.0.
- **Members that fail to link on older runtimes:**

  | Member | Missing below |
  |---|---|
  | `ResponseBody.EMPTY`, `RequestBody.EMPTY`, `Headers.EMPTY` | 5.0.0-alpha.15 |
  | `Response.socket()`, `peekTrailers()`, `Route.echConfigList()`, `Protocol.HTTP_3` | 5.x |
  | Reversed-argument `create(content, MediaType)` | 4.x |
  | `RequestBody.isOneShot/isDuplex` | 3.14 |
  | `Response.trailers()` | 3.13 |
  | `MediaType.get`, `Request.tag(Class)`, `Protocol.H2_PRIOR_KNOWLEDGE` | 3.11 |
  | `Protocol.QUIC` | 3.10 |

- **Experimental APIs.** Avoid `@ExperimentalOkHttpApi`: `okhttp@parent-5.5.0:CHANGELOG.md:412-417` "Do not use these experimental APIs in modules that may be executed using a version of OkHttp different from the version that the module was compiled with. Do not use them in published libraries."

## 7. BridgeInterceptor & gzip

(Sub-investigation; the 3.12.13, 4.12.0 and 5.5.0 code was re-read and matches.)

**Request side: same condition in every version.**
- `okhttp@parent-3.12.13:okhttp/src/main/java/okhttp3/internal/http/BridgeInterceptor.java:78-82` "boolean transparentGzip = false; if (userRequest.header("Accept-Encoding") == null && userRequest.header("Range") == null) { transparentGzip = true; requestBuilder.header("Accept-Encoding", "gzip");" (3.9.0 and 3.14.9 identical).
- `okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/internal/http/BridgeInterceptor.kt:68-72` (4.0.0 `:69-73`).
- `okhttp@parent-5.5.0:okhttp/src/commonJvmAndroid/kotlin/okhttp3/internal/http/BridgeInterceptor.kt:67-71` (5.0.0 `:69-73`).
- `userRequest` is the request as it reaches Bridge, i.e. after application interceptors.

**Response side:**
- 3.x: `okhttp@parent-3.12.13:…/BridgeInterceptor.java:100-110` "if (transparentGzip && "gzip".equalsIgnoreCase(networkResponse.header("Content-Encoding")) && HttpHeaders.hasBody(networkResponse)) { GzipSource responseBody = new GzipSource(networkResponse.body().source()); Headers strippedHeaders = networkResponse.headers().newBuilder() .removeAll("Content-Encoding") .removeAll("Content-Length") .build(); … responseBuilder.body(new RealResponseBody(contentType, -1L, Okio.buffer(responseBody)));"
- 4.x: `okhttp@parent-4.12.0:…/BridgeInterceptor.kt:90-102` "if (transparentGzip && "gzip".equals(networkResponse.header("Content-Encoding"), ignoreCase = true) && networkResponse.promisesBody()) { val responseBody = networkResponse.body if (responseBody != null) { val gzipSource = GzipSource(responseBody.source()) … .removeAll("Content-Encoding") .removeAll("Content-Length") … RealResponseBody(contentType, -1L, gzipSource.buffer())".
- 5.x: `okhttp@parent-5.5.0:…/BridgeInterceptor.kt:92-106`. The logic is the same without the null check. **Bridge still does gzip itself in 5.5.0**; there is no delegation to `CompressionInterceptor`.

**Helper names:**
- 3.x: `HttpHeaders.hasBody(Response)` (`okhttp@parent-3.12.13:…/internal/http/HttpHeaders.java:322`, per the fork).
- 4.x/5.x: `Response.promisesBody()` in `@file:JvmName("HttpHeaders")` (`okhttp@parent-4.12.0:…/internal/http/HttpHeaders.kt:16` "@file:JvmName("HttpHeaders")", `:214` "fun Response.promisesBody(): Boolean {"). `hasBody` is kept only as ERROR-deprecated (`:239` "level = DeprecationLevel.ERROR,", `:241` "fun hasBody(response: Response): Boolean {").
- These are internal; netinspect should re-implement the roughly 10-line logic rather than call them.

**What happens to a response our network interceptor rewrote.** Bridge sees what the network segment returns, after `CacheInterceptor`:
- Remove `Content-Encoding` and supply identity bytes: Bridge's condition is false and it passes the response through untouched. Bridge only strips `Content-Length` inside the gzip branch, so our rewrite must set a correct `Content-Length` or remove it.
- Keep `Content-Encoding: gzip` while `transparentGzip` is true: Bridge wraps our body in `GzipSource`, so the bytes must be real gzip. Otherwise the app's read fails in Okio's header check: `okio@parent-3.18.2:okio/src/zlibMain/kotlin/okio/GzipSource.kt:108` "checkEqual("ID1ID2", 0x1f8b, id1id2.toInt())" (fork).
- If the app set `Accept-Encoding` itself, Bridge never decodes, and the app (or its `CompressionInterceptor`/`BrotliInterceptor`) decodes later. From inside a network interceptor we cannot tell who set `Accept-Encoding: gzip`, because Bridge already added it by then.

**No brotli/zstd in the default chain in 5.x.**
- The 5.5.0 chain is exactly the built-ins (`okhttp@parent-5.5.0:…/internal/connection/RealCall.kt:212-221`).
- **`CompressionInterceptor`** is opt-in:
  - First tag `parent-5.2.0`: `okhttp/src/commonJvmAndroid/kotlin/okhttp3/CompressionInterceptor.kt` and `Gzip.kt` are present at 5.2.0 and absent at 5.1.0.
  - `okhttp@parent-5.5.0:…/CompressionInterceptor.kt:34-36` "open class CompressionInterceptor( vararg val algorithms: DecompressionAlgorithm, ) : Interceptor {".
  - `:44` "if (algorithms.isNotEmpty() && chain.request().header("Accept-Encoding") == null) {".
  - `:63-80` "if (!response.promisesBody()) { return response } … val encoding = response.header("Content-Encoding") ?: return response … .removeHeader("Content-Encoding") .removeHeader("Content-Length") .body(decompressedSource.asResponseBody(body.contentType(), -1))".
- **Brotli** (`okhttp-brotli`, since 4.1.0):
  - `okhttp@parent-4.1.0:okhttp-brotli/src/main/java/okhttp3/brotli/BrotliInterceptor.kt` exists and has no 4.0.0 counterpart.
  - Kdoc: `okhttp@parent-4.12.0:okhttp-brotli/src/main/kotlin/okhttp3/brotli/BrotliInterceptor.kt:30-31` "Adds Accept-Encoding: br to request and checks (and strips) for Content-Encoding: br in responses. n.b. this replaces the transparent gzip compression in BridgeInterceptor."
- **zstd**: `okhttp-zstd` since 5.2.0 (`okhttp@parent-5.5.0:CHANGELOG.md:160-166` "New: The `okhttp-zstd` module negotiates [Zstandard (zstd)][zstd] compression …").
- **Documented placement is as an application interceptor**, which is outside, and therefore after, our network interceptor:
  - `okhttp@parent-5.5.0:okhttp-brotli/README.md:11-13` "OkHttpClient client = new OkHttpClient.Builder() .addInterceptor(BrotliInterceptor.INSTANCE) .build();"
  - `okhttp@parent-5.5.0:okhttp-zstd/README.md:11-13` ".addInterceptor(CompressionInterceptor(Zstd, Gzip))".
  - Result: our network interceptor sees the **compressed wire body** plus `Content-Encoding: br|zstd|gzip`, and Bridge does not decode because it did not add the header.

**Request compression (5.x).**
- `Request.Builder.gzip()` wraps the body in `GzipRequestBody` (fork: `okhttp@parent-5.5.0:…/Request.kt:417-430`; `…/internal/http/GzipRequestBody.kt:23-36`, which has `contentLength() = -1L` and forwards `isOneShot`).
- It is opt-in, first in 5.0.0-alpha.17 (fork). When an app uses it, our request tee captures gzip bytes.

**Other places our rewritten headers show up:** `Response.networkResponse()` (`okhttp@parent-4.12.0:…/internal/cache/CacheInterceptor.kt:128-131`) and the cookie jar (`okhttp@parent-3.12.13:…/BridgeInterceptor.java:95` "HttpHeaders.receiveHeaders(cookieJar, userRequest.url(), networkResponse.headers());").

## 8. Interceptor order, redirects, cache, WebSocket

### 8a. Default chain order: identical in 3.x, 4.x, 5.x

Application interceptors → `RetryAndFollowUpInterceptor` → `BridgeInterceptor` → `CacheInterceptor` → `ConnectInterceptor` → **network interceptors (only `if (!forWebSocket)`)** → `CallServerInterceptor`.

- 3.14.9: `okhttp@parent-3.14.9:okhttp/src/main/java/okhttp3/RealCall.java:212-221` "interceptors.addAll(client.interceptors()); interceptors.add(new RetryAndFollowUpInterceptor(client)); interceptors.add(new BridgeInterceptor(client.cookieJar())); interceptors.add(new CacheInterceptor(client.internalCache())); interceptors.add(new ConnectInterceptor(client)); if (!forWebSocket) { interceptors.addAll(client.networkInterceptors()); } interceptors.add(new CallServerInterceptor(forWebSocket));" (3.9.0 `:185-194`; 3.12.13 `:242-251`).
- 4.0.0: `okhttp@parent-4.0.0:…/RealCall.kt:168-177`. 4.12.0: `okhttp@parent-4.12.0:…/internal/connection/RealCall.kt:177-186`.
- 5.0.0: `okhttp@parent-5.0.0:…/internal/connection/RealCall.kt:183-192`. 5.5.0: `okhttp@parent-5.5.0:…/RealCall.kt:212-221` "interceptors += client.interceptors / RetryAndFollowUpInterceptor() / BridgeInterceptor() / CacheInterceptor() / ConnectInterceptor / if (!forWebSocket) { interceptors += client.networkInterceptors } / interceptors += CallServerInterceptor".
- `ConnectInterceptor` creates the connection/exchange just before the network interceptors, which is why `chain.connection()` is non-null there:
  - `okhttp@parent-3.12.13:…/internal/connection/ConnectInterceptor.java:42-45` "HttpCodec httpCodec = streamAllocation.newStream(client, chain, doExtensiveHealthChecks); … return realChain.proceed(request, streamAllocation, httpCodec, connection);"
  - `okhttp@parent-4.12.0:…/internal/connection/ConnectInterceptor.kt:32-34` "val exchange = realChain.call.initExchange(chain) / val connectedChain = realChain.copy(exchange = exchange) / return connectedChain.proceed(realChain.request)" (5.5.0 `:32-34`).

### 8b. Once per network exchange, with each redirect and retry separate

`RetryAndFollowUpInterceptor` loops. Each iteration calls `proceed()`, which runs the whole Bridge → Cache → Connect → network interceptors → CallServer tail:
- `okhttp@parent-3.12.13:…/internal/http/RetryAndFollowUpInterceptor.java:118-127` "while (true) { … response = realChain.proceed(request, streamAllocation, null, null);", then `:161` "followUp = followUpRequest(response, streamAllocation.route());" and `:194-195` "request = followUp; priorResponse = response;". Limit: `:66` "private static final int MAX_FOLLOW_UPS = 20;".
- `okhttp@parent-4.12.0:…/internal/http/RetryAndFollowUpInterceptor.kt:65-76` "while (true) { call.enterNetworkInterceptorExchange(request, newExchangeFinder) … response = realChain.proceed(request)"; `:108`; `:130-131`.
- `okhttp@parent-5.5.0:…/internal/http/RetryAndFollowUpInterceptor.kt:61-72`; `:93`; `:118-120`, which also emit `followUpDecision`.

So every redirect hop and every retried attempt reaches network interceptors separately, each with its own `chain.request()` and `chain.connection()`. An attempt that fails inside `ConnectInterceptor` never reaches them.

**5.x note.** Before returning, `RetryAndFollowUpInterceptor` resets `response.request` to the pre-Bridge request: `okhttp@parent-5.5.0:…/RetryAndFollowUpInterceptor.kt:84-90` "// Clear out downstream interceptor's additional request headers, cookies, etc. response = response .newBuilder() .request(request)". Record `chain.request()` as the wire request; do not rely on `response.request()`.

### 8c. Cache hits skip network interceptors; conditional hits show the 304; rewrites get cached

- **Cache hits.** `CacheInterceptor` returns without `proceed` when `networkRequest == null`:
  - `okhttp@parent-3.12.13:…/internal/cache/CacheInterceptor.java:84-89` "// If we don't need the network, we're done. if (networkRequest == null) { return cacheResponse.newBuilder() .cacheResponse(stripBody(cacheResponse)) .build();". The only-if-cached 504 is at `:72-81`.
  - `okhttp@parent-4.12.0:…/internal/cache/CacheInterceptor.kt:79-85`, which adds `listener.cacheHit(call, it)`.
  - `okhttp@parent-5.5.0:…/internal/cache/CacheInterceptor.kt:80-86`.
- **Conditional GET.** Network interceptors see the server's 304; the app gets the merged cached response: `okhttp@parent-3.12.13:…/CacheInterceptor.java:101-117` "if (networkResponse.code() == HTTP_NOT_MODIFIED) { Response response = cacheResponse.newBuilder() …" (4.12.0 `:104-122`).
- **Rewrites are cached.** The cache stores what leaves the network interceptors:
  - `okhttp@parent-3.12.13:…/CacheInterceptor.java:123-132` "Response response = networkResponse.newBuilder() … CacheRequest cacheRequest = cache.put(response); return cacheWritingResponse(cacheRequest, response);" (4.12.0 `:128-137`; 5.5.0 `:141-146`).
  - `no-store` prevents it: `okhttp@parent-3.12.13:…/internal/cache/CacheStrategy.java:100` "return !response.cacheControl().noStore() && !request.cacheControl().noStore();" (5.5.0 `CacheStrategy.kt:342`).

### 8d. WebSocket upgrades

(Sub-investigation, spot-checked.)

- **Handshakes never reach network interceptors, in any version.**
  - The WebSocket call is created with `forWebSocket = true`: `okhttp@parent-5.5.0:…/internal/ws/RealWebSocket.kt:168` "call = RealCall(webSocketClient, request, forWebSocket = true)". In 3.x it goes through `Internal.instance.newWebSocketCall(client, request)` (`okhttp@parent-3.12.13:…/internal/ws/RealWebSocket.java:191`), which is `okhttp@parent-3.12.13:okhttp/src/main/java/okhttp3/OkHttpClient.java:195-196` "@Override public Call newWebSocketCall(OkHttpClient client, Request originalRequest) { return RealCall.newRealCall(client, originalRequest, true);".
  - The chain then skips network interceptors via `if (!forWebSocket)` (§8a).
- **The client copy.**
  - `okhttp@parent-3.12.13:…/RealWebSocket.java:181-184` "client = client.newBuilder() .eventListener(EventListener.NONE) .protocols(ONLY_HTTP1) .build();"
  - `okhttp@parent-5.5.0:…/RealWebSocket.kt:153-158` has the same shape (fork: 3.9.0 `:171-174`, 4.12.0 `:153-156`).
  - `newBuilder()` keeps `networkInterceptors` (§1d), but they are never used for this call.
- **EventListener consequences.** `eventListener(NONE)` replaces the factory:
  - In SDK mode our wrapped factory is **dropped for WebSocket handshakes**.
  - In attach mode the copy's `RealCall` still calls the hooked `eventListenerFactory()` getter, so the hook would wrap the NONE factory and WS handshake events would become visible. Runtime behaviour is **UNVERIFIED**.
- **Body a 101 carries.** Covered in §6: 3.x/4.x use `EMPTY_RESPONSE` for WS; 5.0 uses `stripBody()`; 5.2+ uses `UnreadableResponseBody` plus `Response.socket` for **any** HTTP/1 upgrade.
  - 5.2.0 changelog: "New: Support [HTTP 101] responses with `Response.socket`. This mechanism is only supported on HTTP/1.1. We also reimplemented our websocket client to use this new mechanism." (`okhttp@parent-5.5.0:CHANGELOG.md:157-158`).
  - **Non-WebSocket upgrade calls do pass through network interceptors in 5.2+ with an unreadable body.** netinspect must not tee or replace 101 bodies.

## 9. Okio for Java callers across 1.x, 2.x, 3.x

(Sub-investigation. Evidence: source at `okio-parent-1.13.0` … `okio-parent-1.17.6`, `okio-parent-2.0.0`/`2.1.0`, `parent-2.10.0`, `parent-3.0.0` … `parent-3.18.2`; Okio's API record `okio@parent-3.18.2:okio/api/okio.api`; and `javap` of `okio-jvm-2.9.0/3.4.0/3.16.4.jar`. 1.x bytecode is inferred from Java source. Key lines re-checked by me: `BufferedSource.buffer()` in 3.18.2, `okio.api` buffer/copyTo/read lines, `@file:JvmName("Okio")`, and 1.17.6 `BufferedSink.buffer()`.)

**Okio versions each OkHttp ships with.** These are floors; an app may resolve newer.

| OkHttp | Okio | Evidence |
|---|---|---|
| 3.9.0 | 1.13.0 | `okhttp@parent-3.9.0:pom.xml:53` "<okio.version>1.13.0</okio.version>" |
| 3.12.13 | 1.15.0 | `okhttp@parent-3.12.13:pom.xml:59` |
| 3.14.9 | 1.17.2 | `okhttp@parent-3.14.9:pom.xml:51` |
| 4.0.0 | 2.2.2 | `okhttp@parent-4.0.0:build.gradle:20` "'okio': '2.2.2'," |
| 4.12.0 | 3.6.0 | `okhttp@parent-4.12.0:build.gradle:20` "'okio': '3.6.0'," |
| 5.0.0 | 3.15.0 | `okhttp@parent-5.0.0:gradle/libs.versions.toml:6` |
| 5.5.0 | 3.18.1 | `okhttp@parent-5.5.0:gradle/libs.versions.toml:58` "square-okio = "3.18.1"" |

**`Okio.buffer(Source)` and `Okio.buffer(Sink)`: same static JVM methods everywhere.**
- 1.x: `okio@okio-parent-1.17.6:okio/src/main/java/okio/Okio.java:39` "public final class Okio {", `:50` "public static BufferedSource buffer(Source source) {", `:59` "public static BufferedSink buffer(Sink sink) {" (same lines at 1.13.0).
- 2.0.0: Kotlin extensions in a file forced to class name `Okio`: `okio@okio-parent-2.0.0:okio/jvm/src/main/java/okio/Okio.kt:18` "@file:JvmName("Okio")", `:42` "fun Source.buffer(): BufferedSource = RealBufferedSource(this)".
- 2.10.0 / 3.x: a multifile facade, `okio@parent-3.18.2:okio/src/commonMain/kotlin/okio/Okio.kt:18-19` "@file:JvmMultifileClass / @file:JvmName("Okio")".
- API record: `okio@parent-3.18.2:okio/api/okio.api:671-672` "public static final fun buffer (Lokio/Sink;)Lokio/BufferedSink; / public static final fun buffer (Lokio/Source;)Lokio/BufferedSource;".
- Bytecode 2.9.0, 3.4.0, 3.16.4: "public static final okio.BufferedSource buffer(okio.Source);".
- The deprecated `-DeprecatedOkio` object (instance methods; `okio@parent-3.18.2:okio/src/jvmMain/kotlin/okio/-DeprecatedOkio.kt:27`) is a different class and irrelevant to Java.

**`ForwardingSource` / `ForwardingSink`.**
- 1.x: `okio@okio-parent-1.17.6:okio/src/main/java/okio/ForwardingSource.java:24` "public ForwardingSource(Source delegate) {", `:30` "public final Source delegate() {", `:34` "@Override public long read(Buffer sink, long byteCount) throws IOException {". `ForwardingSink.java:24/30/34` follows the same pattern with `write(Buffer source, long byteCount)`.
- 2.10 / 3.x: `okio@parent-3.18.2:okio/src/jvmMain/kotlin/okio/ForwardingSource.kt:20-22` "actual abstract class ForwardingSource actual constructor( @get:JvmName("delegate") actual val delegate: Source,", `:27` "actual override fun read(sink: Buffer, byteCount: Long): Long = delegate.read(sink, byteCount)".
- API record `okio.api:565-570` "public fun <init> (Lokio/Source;)V … public final fun delegate ()Lokio/Source; / public fun read (Lokio/Buffer;J)J". `read` and `write` are overridable; `delegate()` is final.
- Null handling differs: 1.x throws `IllegalArgumentException` ("delegate == null"), while 2.x+ uses a Kotlin null check. Never pass null.

**`Buffer`.** All of these are identical from 1.13 to 3.18:
- `new Buffer()`, `size()J`:
  - 1.13.0 `okio@okio-parent-1.13.0:okio/src/main/java/okio/Buffer.java:59` "public Buffer() {", `:63` "public long size() {".
  - 2.x+ `@get:JvmName("size") actual var size: Long`, e.g. `okio@parent-2.10.0:okio/src/jvmMain/kotlin/okio/Buffer.kt:79-81`.
  - `okio.api:166` "public final fun size ()J".
- `copyTo(Buffer,long,long)`: `okio.api:91` "public final fun copyTo (Lokio/Buffer;JJ)Lokio/Buffer;"; 1.13.0 `:170` "public Buffer copyTo(Buffer out, long offset, long byteCount) {".
- `readByteArray()[B`: 1.13.0 `:754`.
- `write([BII)Lokio/Buffer;` plus a synthetic bridge `write([BII)Lokio/BufferedSink;` (`okio.api:183-184`); 1.13.0 `:983` "@Override public Buffer write(byte[] source, int offset, int byteCount) {".
- `clear()V`: 1.13.0 `:809`; `okio.api:81`.
- **No return type changed.**

**`buffer()` vs `getBuffer()`: `buffer()` was not removed in 3.x.**
- `BufferedSource.getBuffer()` first appears in Okio 1.16.0 (`okio@okio-parent-1.16.0:okio/src/main/java/okio/BufferedSource.java:39` "Buffer getBuffer();"). It is absent in 1.13–1.15 and in 2.0.x, where 2.0.0 has only `fun buffer(): Buffer` (`okio@okio-parent-2.0.0:okio/jvm/src/main/java/okio/BufferedSource.kt:30`).
- `BufferedSink.getBuffer()` exists in **no** 1.x release: `okio@okio-parent-1.17.6:okio/src/main/java/okio/BufferedSink.java:29` has "Buffer buffer();" only.
- 3.18.2 still has `buffer()`, WARNING-deprecated: `okio@parent-3.18.2:okio/src/jvmMain/kotlin/okio/BufferedSource.kt:25-32` "@Deprecated( message = "moved to val: use getBuffer() instead", … level = DeprecationLevel.WARNING, ) fun buffer(): Buffer / actual val buffer: Buffer". `okio.api:235`, `:265` "public abstract fun buffer ()Lokio/Buffer;".
- **Use `buffer()`.** `getBuffer()` throws `NoSuchMethodError` on Okio 1.13–1.15/2.0.x (source) and on all of 1.x (sink).

**`Timeout`, `Source`, `Sink`.** Stable interfaces and class.
- `okio.api:793-797`: `Source` has only `close/read/timeout`. `:780-785`: `Sink` has `close/flush/timeout/write`.
- `Timeout.NONE` is a static field everywhere (`okio.api:817`).
- The only 2→3 break is `Timeout.intersectWith` (`okio@parent-3.18.2:CHANGELOG.md:355-356`), which we don't use.

**What breaks, and in which direction:**
- **Old bytecode on newer Okio is officially compatible and build-enforced.**
  - `okio@parent-3.18.2:CHANGELOG.md:733-734` "Okio 2.x is **binary-compatible** with Okio 1.x".
  - `okio@parent-2.10.0:okio/jvm/japicmp/build.gradle:27` "baseline('com.squareup.okio:okio:1.14.1') {" with `failOnModification`. 3.3.0 uses baseline 1.17.5.
  - From 3.4.0 on, the API record replaces this; its 3.4.0 → 3.18.2 diff removes nothing in the safe subset.
- **New bytecode on older Okio is where breaks happen.**
  - `getBuffer()`: see above.
  - `BufferedSource.peek()`: 1.16+ / 2.1+.
  - `Buffer.copy()`: 2.x+.
  - `copyTo(Buffer,long)` / `copyTo(OutputStream,long)`: 2.x (2.3+) and 3.x only.
  - `Utf8.size(String,int)`: 2.x+.
  - `InflaterSource(BufferedSource, Inflater)`: package-private in 1.x.
- **Never implement `BufferedSource`/`BufferedSink`.** They became sealed in 3.3.0 ("Use a sealed interface for `BufferedSink` and `BufferedSource`. These were never intended for end-users to implement", `okio@parent-3.18.2:CHANGELOG.md:282-283`), and minor releases keep adding abstract methods.
- **Behaviour change.** Reads past the end throw a checked `EOFException` in 2.x+ versus `IllegalStateException` in 1.x (`CHANGELOG.md:736-740`). Catch both.

**Decompression helpers.** Both exist in all versions:
- `GzipSource(Source)`: `okio@okio-parent-1.17.6:okio/src/main/java/okio/GzipSource.java:60` "public GzipSource(Source source) {"; `okio.api:603-604`.
- `InflaterSource(Source, java.util.zip.Inflater)`: `okio@okio-parent-1.17.6:…/InflaterSource.java:39` "public InflaterSource(Source source, Inflater inflater) {"; `okio.api:659`.
- Brotli and zstd are not in Okio.

**Safest subset, needing no reflection:** `Okio.buffer(Source|Sink)`; subclasses of `ForwardingSource`/`ForwardingSink` (override `read`/`write`/`flush`/`close`, call `super`, use `delegate()`); `new Buffer()`, `size()`, `copyTo(Buffer,long,long)`, `readByteArray()`/`readByteArray(long)`, `write(byte[],int,int)`, `write(byte[])`, `writeUtf8`, `readUtf8`, `readString(Charset)`, `skip`, `clear()`; `BufferedSource.buffer()` / `BufferedSink.buffer()` (never `getBuffer()`); `BufferedSink.emit()` / `flush()`; `Source`, `Sink`, `Timeout`; `GzipSource(Source)`; `InflaterSource(Source, Inflater)`.

**Streaming-tee notes** (sub-investigation):
- **Response tee.** In `ForwardingSource.read`: `n = super.read(sink, count); if (n > 0) sink.copyTo(capture, sink.size() - n, n);`. This does not consume the app's bytes. A `RealBufferedSource` over it reads at most `Segment.SIZE` (8192) ahead: `okio@okio-parent-1.13.0:okio/src/main/java/okio/RealBufferedSource.java:45-46` "long read = source.read(buffer, Segment.SIZE);".
- **Request tee.** In `ForwardingSink.write`: copy `source[0,n)` into the capture, then call `super.write`.
- **Completion signal.** OkHttp closes the sink after `writeTo` for non-duplex bodies and leaves it open for duplex ones:
  - `okhttp@parent-4.12.0:okhttp/src/main/kotlin/okhttp3/internal/http/CallServerInterceptor.kt:54-63` "if (requestBody.isDuplex()) { // Prepare a duplex body so that the application can send a request body later. … requestBody.writeTo(bufferedRequestBody) } else { … requestBody.writeTo(bufferedRequestBody) bufferedRequestBody.close()"
  - 3.x: `okhttp@parent-3.12.13:…/internal/http/CallServerInterceptor.java:72-73` "request.body().writeTo(bufferedRequestBody); bufferedRequestBody.close();"

## 10. Compile-target recommendation

**Recommendation: split compile classpaths, as two source sets merged into one jar.**

| Unit | compileOnly | Why |
|---|---|---|
| Core (interceptor, tees, rewrite engine, hooks glue) | **OkHttp 3.14.9 + Okio 1.13.0** (force Okio down) | 3.14.x is the API baseline that 4.x checks binary compatibility against (`okhttp@parent-4.12.0:build.gradle:381` "ext.baselineVersion = "3.14.1""), and 5.x removed nothing we use (§6). It has `Chain.call()`, `requestFailed/responseFailed` and `RequestBody.isOneShot/isDuplex`, so the wrapper `RequestBody` can `@Override` and delegate them; OkHttp only calls them on ≥ 3.14, where they exist. Okio 1.13.0 is the lowest version any supported OkHttp ships (3.9.0), so `javac` rejects `getBuffer()`, `peek()`, `copy()` and the 2-argument `copyTo`. Whether javac is happy with OkHttp 3.14.9 class files plus Okio 1.13.0 is **UNVERIFIED** (not compiled in this task). The Okio types in the 3.14.9 signatures we touch (`BufferedSink`, `BufferedSource`, `ByteString`, `Source`, `Sink`, `Timeout`) all exist in 1.13. |
| `ForwardingEventListener` (only this class) | **OkHttp 5.5.0** | Overrides and forwards all 33 callbacks; see §3d/§3e for why older targets silently drop events. Reference only `EventListener`, `Factory`, `Call` and the callback parameter types; never touch Kotlin-typed APIs such as `Call.tag(KClass)`. |

**If a single classpath is mandatory:** use 3.14.9 everywhere. Declare the 11 post-3.14 callbacks without `@Override`: `proxySelectStart`, `proxySelectEnd`, `canceled`, `satisfactionFailure`, `cacheHit`, `cacheMiss`, `cacheConditionalHit`, `retryDecision`, `followUpDecision`, `dispatcherQueueStart`, `dispatcherQueueEnd`. The JVM treats same name plus descriptor as an override at runtime. Forward them to the delegate through `java.lang.reflect.Method` objects cached at init. The cost is a reflective call per event.

**Shims and guards** (small, feature-detected once at init):
1. **Listener composition.** On ≥ 5.3.0 use `EventListener.plus` (detect `EventListener.class.getMethod("plus", EventListener.class)`) as `appListener.plus(ours)`. Otherwise wrap with `ForwardingEventListener`.
   - Optional: `Call.addEventListener` (≥ 5.4.0, detect `Call.class.getMethod("addEventListener", EventListener.class)`) to attach per call when no factory hook is possible. It misses `callStart`, DNS and connect events.
2. **`RequestBody.isDuplex()/isOneShot()`.** Our own calls, used to decide tee strategy, need a guard: the methods exist ≥ 3.14.0, so on older runtimes treat them as `false`. Overriding them in our wrapper is safe.
3. **`Response.body()`**: null-check (nullable in 3.x/4.x). **`Response.Builder.body(x)`**: never pass null (5.x NPE).
4. **101 / upgrade responses.** If `code == 101`, or `Connection: upgrade` on both request and response, do not tee or replace the body. Never call `Response.socket()` (5.2+ only).
5. **Stripped bodies.** Never read `networkResponse()`, `cacheResponse()` or `priorResponse()` bodies (5.x throws).
6. **Protocol and TLS.** Use `Protocol.toString()`, `TlsVersion.javaName()` and `CipherSuite.javaName()`. Never reference enum constants newer than the floor (`QUIC` 3.10, `H2_PRIOR_KNOWLEDGE` 3.11, `HTTP_3` 5.x).
7. **MediaType.** Use `MediaType.parse` (all versions), never `get` (3.11+) or the Kotlin extensions.
8. **Body construction.** Use `ResponseBody.create(MediaType, byte[])`, `ResponseBody.create(MediaType, long, BufferedSource)`, or our own `ResponseBody` subclass (the three abstract methods). Never use `ResponseBody.EMPTY` (5.x) or reversed-argument factories (4.x+).
9. **Okio.** `buffer()` rather than `getBuffer()`; no `peek()`, `copy()` or 2-argument `copyTo`. No reflection needed.
10. **Version display.** Read it reflectively and never reference it as a compile-time constant, because javac would inline the 4.x `const`:
    - 4.7+: `okhttp3.OkHttp.VERSION`. `okhttp@parent-4.7.0:okhttp/src/main/java-templates/okhttp3/OkHttp.kt:18` "object OkHttp {" with `:34` "const val VERSION = "$projectVersion"". In 5.x it is a non-const static field: `okhttp@parent-5.5.0:okhttp/api/jvm/okhttp.api:961` "public static final field VERSION Ljava/lang/String;".
    - 3.x: `okhttp3.internal.Version.userAgent()` (`okhttp@parent-3.14.9:okhttp/src/main/java-templates/okhttp3/internal/Version.java:19` "public static String userAgent() {").
    - 4.0–4.6: the static field `okhttp3.internal.Version.userAgent` (`okhttp@parent-4.0.0:okhttp/src/main/java-templates/okhttp3/internal/Version.kt:16` "@file:JvmName("Version")", `:19` "const val userAgent = "okhttp/$projectVersion"").

    Prefer feature detection over version parsing.
11. **Brotli / zstd decoding for display.** Reflectively use `org.brotli.dec.BrotliInputStream` only if the app has it; otherwise decode on the host.
12. **Hard rules.** Never call `okhttp3.internal.*` or `$okhttp`-mangled members, and never implement `Interceptor.Chain`, `Call`, `BufferedSource` or `BufferedSink`.

**Oldest supported version: 3.9.0**, the first public `EventListener` and `Chain.call()` (§2c, §4).

**What degrades, by runtime version:**

| Runtime | Degradation |
|---|---|
| 3.9.x–3.10.x | API identical to 3.11 (20 callbacks) despite the "unstable preview" label |
| < 3.14.0 | No `requestFailed/responseFailed` (failures show only as `callFailed`); no duplex/one-shot bodies exist |
| 4.0–4.6 | No proxy-select (< 4.1), `canceled` (< 4.4) or cache events (< 4.7) |
| < 5.0 | No `retryDecision/followUpDecision` |
| < 5.2 | No dispatcher-queue events |
| < 3.9.0 (3.0–3.8.x) | No public `EventListener` (package-private in 3.7–3.8) and no `Chain.call()`. Only interceptor capture is possible: no phase timings and no `callStart` caller stack. For `execute()` the interceptor runs on the caller thread, so a stack can still be taken there; for `enqueue()` it cannot. Correlation must use request identity. The attach-mode `eventListenerFactory()` hook is impossible (package-private or absent). |

**CI.** Run the capture test-suite against OkHttp {3.9.0, 3.12.13, 3.14.9, 4.0.0, 4.12.0, 5.0.0, 5.5.0}, each with its shipped Okio (§9) and with upgraded Okio (2.10.0, 3.18.x). Also add a bytecode-reference check that the core unit only references members present in OkHttp 3.9.0 + Okio 1.13.0, apart from the guarded list above. The tool choice (e.g. animal-sniffer custom signatures) is a suggestion and is not verified here.

## 11. Minimum Android API levels and the 5.x Android variant

(Sub-investigation; the changelog, README and build lines were re-read by me.)

| OkHttp | Android | Java | Evidence |
|---|---|---|---|
| 3.9–3.12.x | 2.3+ (API 9+) | 7+ | `okhttp@parent-3.14.9:CHANGELOG.md:93-94` "The OkHttp 3.12.x branch will be our long-term branch for Android 2.3+ (API level 9+) and Java 7+."; the pom has `<java.version>1.7</java.version>` (3.12.13 `pom.xml:56`; 3.9.0 `:51`, per the fork) |
| 3.13–3.14.x | 5.0+ (API 21+) | 8+ | `okhttp@parent-3.14.9:CHANGELOG.md:85-89` "## Version 3.13.0 … **This release bumps our minimum requirements to Java 8+ or Android 5+.**"; README "OkHttp works on Android 5.0+ (API level 21+) and on Java 8+." (3.13.0 `:11`, fork) |
| 4.x | 21+ | 8+ | README (4.0.0 and 4.12.0 `:75`, fork); `okhttp@parent-4.12.0:build.gradle:149` `android-api-level-21` animal-sniffer signature (fork) |
| 5.x | 21+ | 8+ | `okhttp@parent-5.5.0:README.md:97` "OkHttp works on Android 5.0+ (API level 21+) and Java 8+."; `okhttp@parent-5.5.0:okhttp/build.gradle.kts:62` "minSdk = 21"; the `okhttp-android` 5.3.2 AAR manifest has `<uses-sdk android:minSdkVersion="21" />` (fork) |

**The 5.x Android variant** is a Kotlin Multiplatform build with `jvm` and `android` targets:
- `okhttp@parent-5.5.0:okhttp/build.gradle.kts:12` "id("com.android.kotlin.multiplatform.library")", `:54` "jvm {", `:57-58` "android { namespace = "okhttp.okhttp3"".
- The shared code lives in `okhttp/src/commonJvmAndroid`.
- Publishing:
  - `okhttp@parent-5.5.0:CHANGELOG.md:211-213` "**OkHttp is now packaged as separate JVM and Android artifacts.** … If your build system handles [Gradle module metadata], this change should be automatic."
  - `okhttp@parent-5.5.0:README.md:150-152` "Maven projects must select between `okhttp-jvm` and `okhttp-android`. The `okhttp` artifact will be empty in Maven projects."
- **Class names are the same.** The Android AAR's classes.jar contains `okhttp3/OkHttpClient.class`, `Interceptor.class`, `EventListener.class`, and so on (fork, artifact inspection). The jvm and android API dumps are identical for the classes we use. At 5.5.0 the android dump only adds `OkHttp.initialize(Context)` and `okhttp3.android.AndroidDns` (fork).
- Android-only internals: `okhttp3.internal.platform.AndroidPlatform` and `PlatformInitializer`. The Android variant uses AndroidX Startup: `okhttp@parent-5.5.0:README.md:99-100` "On Android, OkHttp uses [AndroidX Startup]. If you disable the initializer in the manifest, then apps are responsible for calling `OkHttp.initialize(applicationContext)`".
- One `compileOnly` target serves both variants.

**Transitive dependencies apps bring** (no impact on us): 4.x and 5.x pull in kotlin-stdlib, and 5.x Android also pulls in androidx.startup (fork).

---

## Design implications for netinspect

### A. Compile target
- **Core**: `compileOnly` **OkHttp 3.14.9** with **Okio forced to 1.13.0**.
- **`ForwardingEventListener`** in its own source set: `compileOnly` **OkHttp 5.5.0**.
- Target **Java 8 bytecode** with zero runtime dependencies.
- At runtime, prefer `EventListener.plus` on ≥ 5.3.0.
- Minimum fully featured OkHttp: **3.9.0**. Degradations are listed in §10.

### B. Shims needed (feature-detected once, cached)
1. `EventListener.plus` (≥ 5.3.0), with the forwarder as fallback. Optionally `Call.addEventListener` (≥ 5.4.0).
2. Guard our own calls to `RequestBody.isDuplex()/isOneShot()` (≥ 3.14.0); overriding them in wrappers is safe.
3. Null-safe `Response.body()`. Never `Builder.body(null)`.
4. Skip tee/rewrite on 101/upgrade responses and on stripped bodies.
5. String-based `Protocol`/TLS reporting; `MediaType.parse`; 3.x-signature `ResponseBody.create`.
6. Reflective OkHttp version read (`okhttp3.OkHttp.VERSION`, or `okhttp3.internal.Version`).
7. Optional reflective brotli decoder.
8. **No Okio shim**: stay inside the §9 subset.

### C. Exact hook descriptors (attach mode)

| Target | Class | Method | Descriptor | Valid versions | Notes |
|---|---|---|---|---|---|
| network interceptors | `okhttp3.OkHttpClient` (`Lokhttp3/OkHttpClient;`) | `networkInterceptors` | `()Ljava/util/List;` | 3.9.0–5.5.0 (public; `final` from 4.0.0) | Exit hook returns a **new** `ArrayList` with our interceptor prepended **only if absent**. Do not hook `okhttp3.OkHttpClient$Builder.networkInterceptors()Ljava/util/List;` or `-deprecated_networkInterceptors`. |
| listener factory | `okhttp3.OkHttpClient` | `eventListenerFactory` | `()Lokhttp3/EventListener$Factory;` | public 3.9.0–5.5.0 (package-private 3.7.0–3.8.1) | Exit hook returns our wrapper factory **unless the value is already ours**. It is read once per `RealCall` construction, so calls created before attach are not covered. |

Both getters are also called by `OkHttpClient$Builder.<init>(Lokhttp3/OkHttpClient;)V` in 4.x/5.x (bytecode-verified: 4.12.0, 5.1.0, and the 5.3.2 Android AAR), so **idempotency is mandatory**. The interceptor and listener must no-op after detach, because derived clients keep them.

### D. Risks

1. **Duplicate capture** via `newBuilder()` on 4.x/5.x if the hooks aren't idempotent (§1d). Studio's okhttp3 hook has no presence check (`tools-base@11ff8856:…/NetworkInspector.kt:270-277`).
2. **Minified apps.** OkHttp's consumer ProGuard rules keep none of these names:
   - `okhttp@parent-4.12.0:okhttp/src/main/resources/META-INF/proguard/okhttp3.pro:1-14` contains only `-dontwarn`s and `-keepnames class okhttp3.internal.publicsuffix.PublicSuffixDatabase`.
   - `okhttp@parent-5.5.0:okhttp/okhttp3.pro:1-11` contains only `-dontwarn`s.
   - R8 renaming or inlining of `networkInterceptors()`/`eventListenerFactory()` in release builds is therefore possible and would silently defeat name-based hooks (R8 behaviour **UNVERIFIED**).
3. **WebSocket handshakes are never seen by network interceptors** (`if (!forWebSocket)`). In SDK mode our listener is also dropped for them (`EventListener.NONE` copy, §8d).
4. **A network interceptor cannot short-circuit** (exactly-once check, §4). Full mocks need an application interceptor. Pre-`proceed()` failure injection is allowed but may be **retried** by `RetryAndFollowUpInterceptor` depending on the exception type and `retryOnConnectionFailure` (§4). Choose `ProtocolException`, or a post-send `SocketTimeoutException`, for a failure that is not retried.
5. **Rewritten responses are cached.** They are written into the app's `Cache` and replayed later as cache hits that bypass netinspect. Add `Cache-Control: no-store` to rewritten responses (§8c).
6. **Content-Encoding contract** (§7): either emit real gzip with `Content-Encoding: gzip`, or remove `Content-Encoding` and fix or remove `Content-Length`. br/zstd are decoded later by app-level interceptors, so we see wire-compressed bytes. Find/replace on br/zstd bodies requires a decoder, or the rule must be skipped and reported.
7. **The replacement body must close the original network body.** Otherwise follow-ups fail with "cannot make a new request because the previous response is still open" (4.x/5.x) or "didn't close its backing stream. Bad interceptor?" (3.x) (§6), and connections leak.
8. **5.x unreadable bodies** (101 upgrades from 5.2, stripped `networkResponse/cacheResponse/priorResponse`) throw `IllegalStateException` on read.
9. **Crashes.** A `RuntimeException` escaping our interceptor crashes the app for `enqueue()`d calls (§4). Wrap all our logic in `catch (Throwable)`.
10. **Threading.** In 5.x, EventListener callbacks (connect/TLS) arrive on background threads and concurrently per call (fast fallback). Key state by `Call` identity plus `InetSocketAddress`; never use ThreadLocals.
11. **Linkage.** Never link Kotlin `internal` `$okhttp`-mangled members (the Sentry `NoSuchMethodError` precedent, §3d). Never implement `Interceptor.Chain` (30 new abstract methods in 5.4.0), `BufferedSource` or `BufferedSink` (sealed from Okio 3.3, and still growing).
12. **Request-body capture.** The request tee sees the app's body before any later network interceptor transforms it. Our interceptor is prepended, i.e. it runs first in the network segment. On 5.x `Request.Builder.gzip()` bodies are captured compressed. Duplex sinks stay open after `writeTo` returns (§9).
13. **Old devices.** OkHttp 3.12.x apps can run on API 9–20 (§11). If those are in scope, the device runtime must avoid Java 8 library APIs unavailable there. That Android-platform constraint is **UNVERIFIED** here; it is not an OkHttp fact.
