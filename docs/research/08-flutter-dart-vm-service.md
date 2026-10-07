# Flutter backend research: dart:io HTTP traffic through the Dart VM service (Android)

Date: 2026-10-07. Read-only research: sources were downloaded and read; nothing was run against a device, emulator or adb.

**Sources read** (raw files at these commits; line numbers below refer to them):

| Repo | Commit | Notes |
|---|---|---|
| dart-lang/sdk | `62b8169f2b04` (main, 2026-10-07) | plus release tags 2.12.0 … 3.13.0 for version history |
| flutter/flutter | `d454b1b841d2` (master, 2026-10-06) | the engine now lives under `engine/src/flutter/`; older engine files read at the revisions pinned by Flutter 3.7.0 / 3.10.0 / 3.13.0 |
| flutter/devtools | `e25ba1336eaa` (master, 2026-10-06) | |
| dart-lang/http | `585d433ba315` (master) | http_profile, cronet_http, cupertino_http, ok_http, http |
| cfug/dio | `4684e29dabaa` (main) | |
| grpc/grpc-dart | master | one file |
| AOSP packages/modules/adb | main | `socket_spec.cpp`, `sysdeps/posix/network.cpp` |

Flutter-to-Dart version mapping comes from the Flutter release index (`storage.googleapis.com/flutter_infra_release/releases/releases_linux.json`): Flutter 3.0.0 = Dart 2.17.0, 3.7.0 = 2.19.0, 3.10.0 = 3.0.0, 3.19.0 = 3.3.0, **3.22.0 = 3.4.0**, 3.41.0 = 3.11.0. The latest stable at the time of writing is **Flutter 3.47.6 / Dart 3.13.5** (2026-10-01).

Legend: **[src]** = confirmed by reading source at the commits above. **[doc]** = official documentation text. **[unverified]** = an inference or a recollection that I did not confirm from source or by running anything.

---

## 0. Bottom line for the design

1. **Discovery.** The app process logs `The Dart VM service is listening on http://127.0.0.1:<port>/<token>/` with tag `flutter` at INFO. Dart < 2.17 (Flutter < 3.0) logged `Observatory listening on …` instead. The port is random and on the device's loopback interface. The token is required. Android has no mDNS publication. A second channel exists: the static field `io.flutter.embedding.engine.FlutterJNI.vmServiceUri` holds the same URI inside the app process (§1.7).
2. **Availability.** The VM service runs in debug (JIT) and profile builds however the app was launched, including a tap or `am start`. Release builds never have it. DDS is involved only when a host tool (flutter run/attach, an IDE) starts one.
3. **Connecting.** Run `adb forward tcp:0 tcp:<port>` and open a WebSocket to `ws://127.0.0.1:<hostport>/<token>/ws`. If the upgrade returns **HTTP 302**, DDS owns the VM service. Follow `Location`, which is DDS's `http://127.0.0.1:<ddsport>/<ddstoken>/` on the host, and connect to `…/ws`.
4. **Per isolate.** For every isolate in `getVM().isolates`:
   - Call `ext.dart.io.getVersion` and require major 4, which means Dart ≥ 3.4 / Flutter ≥ 3.22.
   - Call `ext.dart.io.httpEnableTimelineLogging {enabled:true}`. Logging is off by default, is set per isolate, and resets on hot restart.
   - Poll `ext.dart.io.getHttpProfile {updatedSince:<last response timestamp>}`. There is no push stream.
   - Fetch bodies with `ext.dart.io.getHttpProfileRequest {id}`. Bodies come back as JSON arrays of byte values, with no size cap.
5. **Streams to subscribe.**
   - `Isolate`: new isolates, plus `ServiceExtensionAdded`.
   - `Extension`: `HttpTimelineLoggingStateChange`.
   - The `Service`-stream event `DartDevelopmentServiceConnected` is sent to every direct client even without a subscription. When it arrives, the VM is about to disconnect you; reconnect to the DDS URI it carries.
6. **Coverage.**
   - Captured: dart:io `HttpClient`. That covers package:http's default `IOClient`, dio's default adapter, `NetworkImage` and dart:io WebSocket handshakes.
   - Also captured: package:http_profile clients, namely cronet_http ≥ 1.3.0, ok_http ≥ 0.1.0 and dio's native adapter.
   - Not captured: dio's http2_adapter, grpc, and anything on the Java side (the existing OkHttp/HttpURLConnection backend covers that).

---

## 1. Finding the VM service on Android

### 1.1 The exact log line

- **Current text.** `Server.outputConnectionInformation()` calls `serverPrint('The Dart VM service is listening on $serverAddress')`, and `serverPrint` calls `print` unless the environment defines `SILENT_OBSERVATORY` or `SILENT_VM_SERVICE` [src [vmservice_server.dart L7–17, L422–424][vms]]. `serverAddress` is `http://<bound ip>:<port>/<token>/`, or `…/` with no token when auth codes are disabled [src [L268–282][vms]].
- **History.** Dart ≤ 2.16 printed `Observatory listening on $serverAddress` (tag 2.16.0, `vmservice_server.dart` L471). Dart 2.17.0 switched to the current text (L464), and Dart 2.17 = Flutter 3.0. When the server shuts down it prints `Dart VM service no longer listening on <uri>` (older: `Observatory no longer listening on …`). If the server cannot bind it prints `Could not start Dart VM service HTTP server:\n<error>` [src [L330–341, L404–408][vms]].
- **flutter_tools regex** (worth copying):
  - Current: `The Dart VM service is listening on ((http|//)[a-zA-Z0-9:/=_\-\.\[\]]+)` [src [globals.dart L287–290][ft-globals]].
  - Flutter 3.0–3.7: `(?:Observatory|The Dart VM service is) listening on …` (tag 3.7.0, globals.dart L287–288).
  - Flutter 3.10 and later: the new form only.
  - Flutter 2.10: `Observatory listening on …`.
  - Match both prefixes. The character class allows `[`/`]` for IPv6 hosts and `=` for the token.

### 1.2 Tag, priority, process

- In a Flutter isolate, Dart `print` goes to `DartRuntimeHooks::Logger_PrintString`, which calls `UIDartState::LogMessage(logger_prefix, message)` [src [dart_runtime_hooks.cc L140–143][rt-hooks]]. On Android the callback is `__android_log_print(ANDROID_LOG_INFO, tag.c_str(), "%.*s", …)` [src [flutter_main.cc L182–186][flutter-main]].
- The tag is `settings.log_tag`, which defaults to `"flutter"` [src [settings.h L329–330][settings]]. An app can change it with `FlutterLoader.Settings.setLogTag()`, which passes `--log-tag=` [src [FlutterLoader.java L572–574][loader]], so don't rely on the tag alone.
- The service isolate runs inside the app process, so the line carries the app's pid.
- Evidence of the line format, verbatim from a flutter_tools test: `'I/flutter : The Dart VM service is listening on http://127.0.0.1:12345/PTwjm8Ii8qg=/'` (`packages/flutter_tools/test/general.shard/protocol_discovery_test.dart`).
- What the line looks like in each logcat format (constructed; the format comes from logcat's `-v` option):
  ```text
  -v threadtime: 10-07 16:45:12.345  4321  4360 I flutter : The Dart VM service is listening on http://127.0.0.1:43217/Wq9tyH3o9fo=/
  -v time:       10-07 16:45:12.345 I/flutter ( 4321): The Dart VM service is listening on http://127.0.0.1:43217/Wq9tyH3o9fo=/
  ```
- The line is printed once per process, when the service isolate's HTTP server starts. It auto-starts because the engine passes port ≥ 0 [src [dart_service_isolate.cc L170–181][dsi]]. It disappears when the main log buffer rotates or someone runs `adb logcat -c`; flutter_tools itself runs `logcat -c` in `clearLogs` [src [android_device.dart L732][ft-android]].

### 1.3 Bind address, port, auth token

- **Host.**
  - `--vm-service-host=<h>` sets it explicitly.
  - Otherwise it is `"::1"` with `--ipv6` and `"127.0.0.1"` without [src [switches.cc L247–257][switches]].
  - Switch help text: "If not set, defaults to 127.0.0.1 or ::1 depending on whether --ipv6 is specified" [src [switch_defs.h L68–72, L85–88][switchdefs]].
  - The server prefers an IPv4 address from the lookup [src [vmservice_server.dart L313–321][vms]].
  - So by default it listens on device loopback only.
- **Port.** `--vm-service-port=<n>`. The default is `0`, meaning a random free port [src [settings.h L191–197][settings], [switch_defs.h L73–76][switchdefs]].
- **Token.** The token is `base64Url.encode(8 bytes from Random.secure())`, which is 12 characters ending in `=` (for example `Wq9tyH3o9fo=`). It is new for every process [src [vmservice.dart L26–34][vmservice]].
  - Auth codes are on unless `--disable-service-auth-codes` is passed [src [switches.cc L275–278][switches]].
  - A request without the right first path segment gets **403 `missing or invalid authentication code`** [src [vmservice_server.dart L498–530, L643–662][vms]].
- **INTERNET permission.** The app needs it for the server socket. Flutter's app template puts `<uses-permission android:name="android.permission.INTERNET"/>` in the **debug** and **profile** manifests. The template comment reads: *"The INTERNET permission is required for development. Specifically, the Flutter tool needs it to communicate with the running application to allow setting breakpoints, to provide hot reload, etc."* [src [templates/app/android.tmpl/app/src/debug/AndroidManifest.xml.tmpl][tmpl]]. Without the permission the bind fails and the `Could not start Dart VM service HTTP server` line is printed [unverified at runtime].

### 1.4 Launched without `flutter run`; build modes

- **On by default.** The engine enables the VM service unless the `--disable-vm-service` switch is present: `settings.enable_vm_service = !command_line.HasOption(...kDisableVmService)` [src [switches.cc L239–241][switches]]. The Android loader never adds that switch [src [FlutterLoader.java L330–600][loader]]. `start_paused` defaults to false [src [settings.h L148][settings]]. So a debug app started by tapping or `am start` has a VM service on a random port, with an auth code, and is not paused.
- **Knobs flutter run uses** (not needed for attaching):
  - flutter run passes **intent extras**: `am start -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -f 0x20000000 --ez enable-dart-profiling true … --ez start-paused true --ez disable-service-auth-codes true …` [src [android_device.dart L628–642][ft-android], [device.dart L1415–1487][ft-device]].
  - The engine reads these through the now-`@Deprecated` `FlutterShellArgs.fromIntent`. Keys include `start-paused`, `disable-service-auth-codes` and `vm-service-port` (an int extra) [src [FlutterShellArgs.java L20–35, L85–110][shellargs]].
  - Current engine master also accepts **manifest metadata** such as `io.flutter.embedding.android.VMServicePort`, `…DisableServiceAuthCodes` and `…StartPaused` [src [FlutterEngineFlags.java L33–80, L292–335][engflags]]. This was refactored again on 2026-10-02 (#191924). [unverified] which stable release first ships it.
- **Release builds** have no VM service:
  - Switch help: *"The Dart VM Service is never available in release mode."* [src [switch_defs.h L78–81][switchdefs]].
  - In PRODUCT builds the VM compiles `ServiceIsolate::Run()` to an empty stub [src [service_isolate.h L21–123][svc-iso-h]].
  - `HttpProfiler.startRequest` returns null under `dart.vm.product` [src [http_impl.dart L15][http-impl]].
  - Flutter docs [doc [build modes][buildmodes]]: release mode *"Service extensions are disabled. Debugging is disabled."*
- **Profile builds** have a VM service:
  - FlutterLoader adds `--aot-vmservice-shared-library-name=libvmservice_snapshot.so`, commented *"In profile mode, provide a separate library containing a snapshot for launching the Dart VM service isolate"* [src [FlutterLoader.java L560–565][loader]].
  - flutter run waits for the VM service URI in debug and profile builds alike [src [android_device.dart L659–664][ft-android]].
  - Flutter docs [doc][buildmodes]: profile mode *"Tools supporting source-level debugging (such as DevTools) can connect to the process."*
  - dart:io network profiling is registered under `#if !defined(PRODUCT)` [src [dart_io_api_impl.cc L145–154][io-api-impl]], and profile is a non-PRODUCT runtime, so HTTP profiling should work in profile builds [unverified at runtime].

### 1.5 DDS when not launched by a tool

- **The app never starts DDS.** The engine's `DartServiceIsolate::Startup` sets only `_ip`, `_port`, `_autoStart`, `_originCheckDisabled`, `_authCodesDisabled` and `_enableServicePortFallback` [src [dart_service_isolate.cc L136–198][dsi]]. `_waitForDdsToAdvertiseService` stays `false` and `_ddsIP` stays empty [src [vmservice_io.dart L15–66][vmsio]].
- **Host tools start DDS:**
  - flutter run and flutter attach use `DartDevelopmentServiceLauncher.start`. DDS binds 127.0.0.1 (::1 with `--ipv6`) on the **host**, on `--dds-port` or a random port, with its own auth code unless auth codes are disabled [src [base/dds.dart L68–118][ft-dds]].
  - IDEs go through the flutter tool.
  - [unverified] other launchers such as `dart devtools` and `dart development-service`.

### 1.6 How `flutter run` / `flutter attach` find it (to mirror)

- **flutter run.** It creates a logcat reader **before** `am start`:
  - The reader runs `adb shell -x logcat -v time -T '<timestamp of last line>'`, so it sees new lines only [src [android_device.dart L1028–1063][ft-android]].
  - `ProtocolDiscovery.vmService` matches each line against the regex. It optionally filters on `--device-vmservice-port` and throttles to one URI per 200 ms.
  - It then runs `adb forward tcp:<hostPort|0> tcp:<devicePort>` and swaps the port in the URI [src [protocol_discovery.dart][ft-pd]].
- **flutter attach (Android).**
  - It uses `LogScanningVMServiceDiscoveryForAttach` over `getLogReader(includePastLogs: true)`, which runs `adb shell -x logcat -v time -s flutter`: the whole buffer, tag `flutter` only [src [android_device.dart L767–783, L1043–1048][ft-android], [device_vm_service_discovery_for_attach.dart][ft-attachdisc]].
  - **The first match wins** (`firstValidUri()` → `uris.take(1)`). With several runs in the buffer it can pick a stale URI unless `--device-vmservice-port` is given [src [attach.dart L419–461][ft-attach]].
  - `--debug-url <uri>` skips discovery and just forwards that port [src [attach.dart L409–416][ft-attach], [mdns_discovery.dart L632–662][ft-mdns]].
  - After 30 s it prints a "taking longer than expected" warning [src [attach.dart L432–455][ft-attach]].
- **mDNS** (`_dartVmService._tcp`) is published **only by the iOS embedder**. `enable_vm_service_publication` is consumed only in `darwin/ios/framework/Source/FlutterEngine.mm` and `FlutterDartVMServicePublisher.mm` (code search of flutter/flutter). There is none on Android.
- **Suggested for traffic-police:** run `logcat -d -v threadtime --pid=<pid>` (`--pid` needs Android 7+ [unverified]; the project minimum is API 26) and take the **last** match for that pid. Don't filter on the tag. Treat `Dart VM service no longer listening` as invalidating the URI.

### 1.7 A logcat-independent fallback: `FlutterJNI.vmServiceUri`

- The Android engine copies every server-state URI into the Java static field `io.flutter.embedding.engine.FlutterJNI.vmServiceUri`, which has a public getter `FlutterJNI.getVMServiceUri()` [src [flutter_main.cc L209–236][flutter-main], [FlutterJNI.java L251–275][jni]].
- Naming by version (FlutterJNI.java at the engine revisions pinned by each Flutter tag):
  - Flutter ≤ 3.7: `observatoryUri` / `getObservatoryUri()`.
  - Flutter 3.10 and 3.13: `vmServiceUri` / `getVMServiceUri()`, with `getObservatoryUri()` still present.
- `ddsConnectedCallback` and `ddsDisconnectedCallback` re-notify the engine with `server.serverAddress`, and that getter returns `ddsUri` while DDS is connected [src [vmservice_io.dart L100–115][vmsio], [vmservice_server.dart L268–273][vms]]. So the field should switch to the DDS URI while a tool is attached and back afterwards [unverified at runtime].
- traffic-police's attach-mode agent (ARCHITECTURE §4.7) could read this field by reflection. That avoids losing the line to logcat rotation.

---

## 2. Connecting

### 2.1 URL, HTTP-level checks, WebSocket framing

- **URL.** Turn `http://h:p/<token>/` into `ws://h:p/<token>/ws`: switch the scheme to ws (wss for https) and append `ws` to the path (`/ws` if the path has no trailing slash) [src [vm_service/lib/utils.dart L8–20][vmutils]]. `WEBSOCKET_PATH = '/ws'` [src [vmservice_server.dart L245][vms]].
- **Checks on every request** [src [vmservice_server.dart L623–684][vms]]:
  1. *Origin check* [src [L434–494][vms]]. The `Host` header is required and must be `localhost`, `127.0.0.1` or `::1` on any port, or the server's own address. Any `Sec-WebSocket-Origin`/`Origin` header must pass the same test. Failure gives **403 `forbidden origin`**. Connecting to `127.0.0.1:<forwarded port>` passes, and the source comment says *"necessary for adb port forwarding"*.
  2. *Token.* It must be the first path segment, otherwise **403**. `/<token>` with no trailing slash is redirected to `/<token>/`.
  3. `/<token>/ws` upgrades to WebSocket. With no DDS the upgrade is accepted with `compression: CompressionOptions.compressionOff`. **If the client sends `Sec-WebSocket-Protocol`, the server selects `implicit-redirect`**, so don't offer a subprotocol [src [L570–586][vms]].
  4. While DDS is connected (`acceptNewWebSocketConnections == false`), the upgrade request gets `request.response.redirect(_service.ddsUri!)`, an **HTTP 302 (dart:io `HttpResponse.redirect` defaults to `HttpStatus.movedTemporarily` [src [http.dart L1136][http-dart]]) whose `Location` is DDS's http URI** [src [L580–585][vms]].
- **Framing.** Requests must be **text** frames holding a JSON object.
  - A binary frame closes the socket with code 4001, unparsable JSON with 4000, and a non-object with 4002 [src [L19–63][vms]].
  - Responses and events arrive as text frames [src [L65–84][vms]].
  - The server sets no keepalive ping and no request timeout [unverified: no `pingInterval` is set in that file].

### 2.2 JSON-RPC 2.0 shape

These examples are copied from service.md [doc [service.md L170–232][svcmd]]:

```json
{ "jsonrpc": "2.0", "method": "getVersion", "params": {}, "id": "1" }
```
```json
{ "jsonrpc": "2.0", "result": { "type": "Version", "major": 3, "minor": 5 }, "id": "1" }
```
```json
{ "jsonrpc": "2.0", "method": "streamListen", "params": { "streamId": "GC" }, "id": "2" }
```
```json
{ "jsonrpc": "2.0",
  "error": { "code": 103, "message": "Stream already subscribed",
             "data": { "details": "The stream 'GC' is already subscribed" } },
  "id": "2" }
```

- **`id` and params.** `id` must be a string, a number or null. `jsonrpc` is optional. Params are named only [doc service.md L184–208; src [message.dart L28–46][message]].
- **Extension calls** (`ext.*`) need `isolateId` in params.
  - The VM service **stringifies every param value** with `toString()` before sending it to the isolate: `true` becomes `"true"` and `1759831600000000` becomes `"1759831600000000"` [src [message.dart L141–170][message]]. The dart:io handler signature is `(String method, Map<String,String> parameters)` [src [network_profiling.dart L93–96][np]].
  - The isolate builds the reply as `{"jsonrpc":"2.0","result":<handler JSON>,"id":"<id>"}` or `{"jsonrpc":"2.0","error":<error JSON>,"id":…}`.
  - **The id is written back without escaping** (`sb.write('"id":"$id"}')`), so use plain alphanumeric ids [src [vm/lib/developer.dart L158–193][devpatch]].
  - A request without `id` (a notification) gets no reply.
- **Error codes.**
  - Unknown extension method on a live isolate: **-32601** "Method not found" [src [service.cc L923–925, L1075–1087][servicecc]].
  - dart:io handler errors: **-32602** "Invalid params", with `data.details` holding the message [src [network_profiling.dart L154–159][np], [extension.dart L44–81][ext]].
  - Uncaught exceptions inside an extension: **-32000** [src [vm/lib/developer.dart L131–139][devpatch]].
  - A dead or unknown isolate id is **not an error**. The result is `{"type":"Sentinel","kind":"Collected","valueAsString":"<collected>"}` [src [running_isolates.dart L153–164][runiso]].
  - Error example (the shape is from the code; the details string is from `_getHttpProfileRequest`):
    ```json
    {"jsonrpc":"2.0","error":{"code":-32602,"message":"Invalid params","data":{"details":"Unable to find request with id: '42'"}},"id":"9"}
    ```
- **Events** have the form `{"jsonrpc":"2.0","method":"streamNotify","params":{"streamId":"Isolate","event":{…}}}` [src [vmservice.dart L233–246][vmservice]]. The example in service.md L281–302 spells the key `"json-rpc"`; the implementation uses `"jsonrpc"`.

### 2.3 `getVM` → isolates, `getIsolate` → `extensionRPCs`

The field lists come from `VM`, `@Isolate`, `@IsolateGroup` and `Isolate` in service.md [doc [L5011–5060, L3763–3850, L3869–3888][svcmd]]. The values below are illustrative.

```json
{"jsonrpc":"2.0","id":"3","method":"getVM","params":{}}
```
```json
{"jsonrpc":"2.0","id":"3","result":{
  "type":"VM","name":"vm","architectureBits":64,"hostCPU":"…","operatingSystem":"android",
  "targetCPU":"arm64","version":"3.13.5 (stable) …","pid":4321,"startTime":1759831512345,
  "isolates":[{"type":"@Isolate","id":"isolates/4419843374421743","number":"4419843374421743",
               "name":"main","isSystemIsolate":false,"isolateGroupId":"isolateGroups/1838029386419383"}],
  "isolateGroups":[{"type":"@IsolateGroup","id":"isolateGroups/1838029386419383","number":"1838029386419383",
                    "name":"main","isSystemIsolateGroup":false}],
  "systemIsolates":[{"type":"@Isolate","id":"isolates/…","number":"…","name":"vm-service","isSystemIsolate":true,"isolateGroupId":"…"}],
  "systemIsolateGroups":[…]}}
```

`getIsolate` (`Isolate|Sentinel getIsolate(string isolateId)`) returns `Isolate.extensionRPCs: string[] [optional]`, *"The list of service extension RPCs that are registered for this isolate, if any."* [doc [service.md L1051–1062, L3847–3849][svcmd]]. Abbreviated example:

```json
{"jsonrpc":"2.0","id":"4","result":{"type":"Isolate","id":"isolates/4419843374421743","name":"main",
 "isSystemIsolate":false,"runnable":true,"pauseEvent":{"type":"Event","kind":"Resume","timestamp":1759831512999},
 "extensionRPCs":["ext.dart.io.httpEnableTimelineLogging","ext.dart.io.getHttpProfile",
   "ext.dart.io.getHttpProfileRequest","ext.dart.io.clearHttpProfile","ext.dart.io.getSocketProfile",
   "ext.dart.io.socketProfilingEnabled","ext.dart.io.clearSocketProfile","ext.dart.io.getVersion",
   "ext.flutter.…"], "…":"…"}}
```

- On current main the WebSocket trio (`getWebSocketProfile`, `getWebSocketConnection`, `clearWebSocketProfile`) is registered before `getVersion` [src [network_profiling.dart L69–88][np]]. That code landed on 2026-08-13 in commit `fe42b97f17` ("Add WebSocket profiling VM service support (GSoC'26)"), and **is not in Dart 3.13.x stable** (the commit has diverged from tag 3.13.0).
- `ext.dart.io.getOpenFiles` and similar extensions are registered lazily by file and process code (`sdk/lib/io/file_impl.dart`).
- Check availability the way package:vm_service does: `extensionRPCs.contains('ext.dart.io.getHttpProfile')` and `…httpEnableTimelineLogging` [src [dart_io_extensions.dart L40–118][vmsio-ext]].

### 2.4 Which isolates carry `ext.dart.io.*`

- **Every isolate the engine creates or initializes** carries them. That covers root UI isolates, extra engines in the same process, and `Isolate.spawn`/`compute`/`Isolate.run` children.
  - Each goes through `DartIsolate::InitializeIsolate` → `LoadLibraries` → `DartIO::InitForIsolate` → `dart::bin::SetupDartIoLibrary({...})` [src [dart_isolate.cc L667–693, L1153–1186, L1361–1375][dart-isolate], [lib/io/dart_io.cc L18–39][dartio]].
  - That function's `DartIoSettings.enable_network_profiling` **defaults to true** [src [dart_io_api.h L95–98][io-api-h]].
  - In non-PRODUCT builds it then calls `_NetworkProfiling._registerServiceExtension()` [src [dart_io_api_impl.cc L145–154][io-api-impl]].
- VM-internal isolates (the vm-service and kernel isolates) cannot register extensions [src [isolate.cc L3463–3466][isolatecc]].
- Every registration posts `ServiceExtensionAdded` on the Isolate stream, carrying `extensionRPC` [src [isolate.cc L3485–3490][isolatecc]].
- **All profiling state is per isolate:**
  - `HttpClient._enableTimelineLogging` [src [http.dart L1263–1286][http-dart]]
  - `HttpProfiler._profile` [src [http_impl.dart L7–37][http-impl]]
  - dart:developer's `_developerProfilingData` [src [developer/http_profiling.dart L9][devprof]]
  - Each isolate must be enabled and polled separately.

### 2.5 Streams worth listening to

| Stream | Events | Why |
|---|---|---|
| `Isolate` | `IsolateStart`, `IsolateRunnable`, `IsolateExit`, `IsolateUpdate`, `IsolateReload`, `ServiceExtensionAdded` (field `extensionRPC`) [doc [service.md L1913–1921, L2598–2601][svcmd]] | Pick up new isolates (compute, hot restart). Enable logging when `ext.dart.io.httpEnableTimelineLogging` appears. Drop isolates on exit. |
| `Extension` | kind `Extension` with `extensionKind` = `HttpTimelineLoggingStateChange`, data `{isolateId, enabled}`, posted only when the value changes [src [http.dart L1267–1278][http-dart]]. Also `SocketProfilingStateChange` [src [network_profiling.dart L264–272][np]]. | Notice someone else (another tool, or the app) turning logging off. |
| `Service` | You don't need to subscribe. The VM posts the DDS notice to each direct client and then disconnects it [src [vmservice.dart L228–249, L276–284][vmservice]]. | Reconnect through DDS. |

The DDS notice, as built by the VM (literal from source; `kServiceStream = 'Service'`). **This event kind is not documented in service.md.**

```json
{"jsonrpc":"2.0","method":"streamNotify","params":{"streamId":"Service","event":{
  "type":"Event","kind":"DartDevelopmentServiceConnected",
  "message":"A Dart Developer Service instance has connected and this direct connection to the VM service will now be closed. Please reconnect to the Dart Development Service at http://127.0.0.1:61234/QwErTy12AbC=/.",
  "uri":"http://127.0.0.1:61234/QwErTy12AbC=/","timestamp":1759831700000}}}
```

- **No HTTP-profile event stream exists.** New and updated requests are only visible by polling `getHttpProfile`.
  - The profiler does mirror each request into a `TimelineTask` with filterKey `HTTP/client`, named `HTTP CLIENT <METHOD>`, and the profile `id` *is* the task id [src [http_impl.dart L54–68][http-impl]].
  - DevTools does not use the `Timeline` stream for the Network page. [unverified] whether it would make a usable low-latency feed: timeline events are delivered in blocks, and the `Dart` stream must be recorded.
- **Hot restart.** The engine kills and recreates its UI isolates, and the tool kills isolates they spawned [src [run_hot.dart L682–692][runhot]]. The new isolate has a new id, logging is off again (a static field), and its profile is empty. Hot reload keeps the isolate and its statics. DevTools re-enables logging on every `onIsolateCreated` [src [network_controller.dart L196–203][dt-ctrl]].

### 2.6 Plain HTTP GET (undocumented, but works with DDS)

- `GET http://127.0.0.1:<hostport>/<token>/<method>?<query params>` is answered by an `HttpRequestClient`, with `Content-Type: application/json` and the JSON-RPC envelope in the body. For example: `…/ext.dart.io.getHttpProfile?isolateId=isolates%2F4419843374421743&updatedSince=0` [src [vmservice_server.dart L670–683, L108–131][vms], [message.dart L105–110][message]].
- service.md: *"It is possible to make HTTP (non-WebSocket) requests, but this does not allow access to VM events and is not documented here."* [doc [L8–12][svcmd]].
- **Only WebSocket upgrades and DevTools asset requests are redirected to DDS.** The code comment reads *"Don't redirect HTTP VM service requests, just requests for DevTools assets."* So plain GETs still reach the VM directly when DDS is connected [src [L670–683][vms]].
- This could serve as a polling-only fallback [unverified at runtime; undocumented, so it could change].

---

## 3. The dart:io HTTP profiling extensions

### 3.1 Protocol versions

The current spec is **"Dart VM Service Protocol Extension 4.0"** [doc [service_extension.md][svcext]]. `_versionMajor = 4; _versionMinor = 0` [src [network_profiling.dart L8–9][np]]. Value per release tag (each read from that tag's `network_profiling.dart`):

| Dart SDK (Flutter) | dart:io ext version | What a client sees |
|---|---|---|
| 2.12 (Flutter 2.0) | 1.5 | Request `id` is an **int**. Times are on the **monotonic timeline clock** (`Timeline.now`). The deprecated `getHttpEnableTimelineLogging`, `setHttpEnableTimelineLogging`, `startSocketProfiling` and `pauseSocketProfiling` are still registered. Before 1.4 the param was `enable`. |
| 2.14 … 2.19 (Flutter 2.5 … 3.7) | 1.6 | Same as 1.5. The deprecated RPCs are still registered at 2.19.0, although the revision history says 1.6 removed them. |
| 3.0 … 3.2 (Flutter 3.10 … 3.16) | 2.0 | `id` becomes a **String**, as do the `getHttpProfileRequest` `id` param and `SocketStatistic.id`. The param is `enabled`. The deprecated RPCs are still registered at 3.0.0. |
| 3.3 (Flutter 3.19) | 3.0 | The deprecated RPCs are gone (the 3.3.0 file registers only the 8 current names). Client-side `is…Available` helpers were added; these are package:vm_service methods, not RPCs. |
| **≥ 3.4 (Flutter ≥ 3.22)** | **4.0** | `updatedSince`, `HttpProfile.timestamp`, request `startTime`/`endTime`, response `startTime`/`endTime` and event `timestamp` all become **µs since the Unix epoch**. `events` moves to the top-level request object; `events` and `method` are removed from `HttpProfileRequestData` (the implementation still emits `method` and `uri` there). Many response fields become optional. package:http_profile entries (`from_package/N`) are merged in. |

Sources: the revision history [doc [service_extension.md L585–623][svcext]]; commits "Update HTTP Profiling ids to use Strings" `deca6a66e7` (in 3.0.0), "Update dart:io service extension spec" `60833d9cd9` (in 3.4.0), and "Add APIs to dart:developer for recording HTTP profiling information" `628d744391` (in 3.4.0). Recommendation: **require 4.x**. Supporting 2.x/3.x would mean converting monotonic timestamps with `getVMTimelineMicros`; 1.x would also need int ids.

### 3.2 Methods, params, results

All methods take `isolateId`. Every param value reaches the handler as a string (§2.2). The handler is [src [network_profiling.dart L93–160][np]].

#### `ext.dart.io.getVersion`
```json
{"jsonrpc":"2.0","id":"5","method":"ext.dart.io.getVersion","params":{"isolateId":"isolates/4419843374421743"}}
```
```json
{"jsonrpc":"2.0","result":{"type":"Version","major":4,"minor":0},"id":"5"}
```
[src [network_profiling.dart L162–166][np]]

#### `ext.dart.io.httpEnableTimelineLogging(isolateId, enabled [optional])`
- `enabled` must be `"true"` or `"false"` (case-insensitive), otherwise -32602 `Value for parameter 'enabled' is not valid: …`.
- It always returns the current state [src [network_profiling.dart L100–104, L176–192][np]].
- It sets `HttpClient.enableTimelineLogging`, which posts `HttpTimelineLoggingStateChange` when the value changes [src [http.dart L1263–1286][http-dart]].
```json
{"jsonrpc":"2.0","id":"6","method":"ext.dart.io.httpEnableTimelineLogging","params":{"isolateId":"isolates/4419843374421743","enabled":true}}
```
```json
{"jsonrpc":"2.0","result":{"type":"HttpTimelineLoggingState","enabled":true},"id":"6"}
```

#### `ext.dart.io.getHttpProfile(isolateId, updatedSince [optional])`

Implementation, verbatim [src [network_profiling.dart L105–126][np]]:

```dart
final updatedSince = switch (parameters['updatedSince']) { var updatedSince? => int.tryParse(updatedSince), _ => null };
responseJson = json.encode({
  'type': 'HttpProfile',
  'timestamp': DateTime.now().microsecondsSinceEpoch,
  'requests': [
    ...HttpProfiler.serializeHttpProfileRequests(updatedSince),            // dart:io entries, ref form
    ...getHttpClientProfilingData()                                         // package:http_profile entries
        .where((p) => updatedSince == null || (p['_lastUpdateTime'] as int) >= updatedSince)
        .map((p) => _createHttpProfileRequestFromProfileMap(p, ref: true)),
  ],
});
```

- **`updatedSince`.** An entry is returned if its `lastUpdateTime >= updatedSince`, and the comparison is **inclusive** [src [http_impl.dart L27–36][http-impl]].
  - `lastUpdateTime` is refreshed on every state change, event, and appended request or response chunk (`_updated()` [src L239][http-impl]).
  - A parameter that won't parse as an int behaves as if absent, so the full profile comes back.
- **`timestamp`** is `DateTime.now()` in µs since the epoch, read before serialization. The isolate is single-threaded, so feeding it back as the next `updatedSince` loses nothing. DevTools does exactly this (§5).
  - Because the comparison is inclusive, expect occasional duplicates; dedupe on `(isolateId, id)`.
- **Clock.** On Android, `DateTime.now()` is `OS::GetCurrentTimeMicros()` = `gettimeofday`, i.e. the wall clock (CLOCK_REALTIME) [src [os_android.cc L134–141][os-android]]. A backwards wall-clock step can hide updates that land in the gap until they change again. A periodic full poll (no `updatedSince`) or a small overlap margin covers that.
- **Order.** `_profile` is a default (insertion-ordered) Map, so dart:io entries come back in start order. Package entries come after them.

**Example response.** It is constructed from the `toJson(ref: true)` code [src [http_impl.dart L207–237, L95–168][http-impl]]. Field names and types are exact; values are illustrative.

```json
{"jsonrpc":"2.0","id":"7","result":{
 "type":"HttpProfile",
 "timestamp":1759831602512345,
 "requests":[
  {"type":"@HttpProfileRequest",
   "id":"8392571130443811",
   "isolateId":"isolates/4419843374421743",
   "method":"GET",
   "uri":"https://api.example.com/v1/orders?id=4b67",
   "events":[
     {"timestamp":1759831601010000,"event":"Connection established"},
     {"timestamp":1759831601010050,"event":"Request sent"},
     {"timestamp":1759831601200000,"event":"Waiting (TTFB)"},
     {"timestamp":1759831601230000,"event":"Content Download"}],
   "startTime":1759831600950000,
   "endTime":1759831601010100,
   "request":{
     "headers":{"user-agent":["Dart/3.13 (dart:io)"],"accept-encoding":["gzip"],"content-length":["0"],"host":["api.example.com"]},
     "connectionInfo":{"localPort":40512,"remoteAddress":"203.0.113.10","remotePort":443},
     "contentLength":0,"cookies":[],"followRedirects":true,"maxRedirects":5,"method":"GET",
     "persistentConnection":true,"uri":"https://api.example.com/v1/orders?id=4b67"},
   "response":{
     "startTime":1759831601200100,
     "headers":{"content-type":["application/json; charset=utf-8"],"content-encoding":["gzip"],"set-cookie":["a=1; Path=/","b=2; Path=/"]},
     "compressionState":"HttpClientResponseCompressionState.decompressed",
     "connectionInfo":{"localPort":40512,"remoteAddress":"203.0.113.10","remotePort":443},
     "contentLength":-1,"cookies":["a=1; Path=/","b=2; Path=/"],"isRedirect":false,
     "persistentConnection":true,"reasonPhrase":"OK","redirects":[],"statusCode":200,
     "endTime":1759831601230000}},
  {"type":"@HttpProfileRequest",
   "id":"8392571130443812",
   "isolateId":"isolates/4419843374421743",
   "method":"POST",
   "uri":"https://api.example.com/v1/upload",
   "events":[{"timestamp":1759831602400000,"event":"Connection established"},
             {"timestamp":1759831602400040,"event":"Request sent"}],
   "startTime":1759831602390000}
 ]}}
```

The second entry is still sending its body. `request` is absent until `finishRequest` (or `finishRequestWithError`) runs, and `response` is absent until headers arrive (`responseInProgress != null`) [src [http_impl.dart L217–231][http-impl]].

For a **package:http_profile** entry, `_createHttpProfileRequestFromProfileMap` builds the object [src [network_profiling.dart L19–42][np]]:
- `request` is **always present**: it is the client's `requestData` map, possibly `{}`.
- `response` is **always present** from creation, at least `{"redirects":[]}` [src [http_profile_response_data.dart L250–256][hp-resp]].
- So for these entries, use `response.startTime`, `statusCode` and `endTime`, not the mere presence of `response`, to judge progress.

```json
{"type":"@HttpProfileRequest","id":"from_package/1","isolateId":"isolates/4419843374421743",
 "method":"POST","uri":"https://api.example.com/v1/events","events":[],
 "startTime":1759831601500000,"endTime":1759831601520000,
 "request":{"connectionInfo":{"package":"package:cronet_http","client":"CronetHttp"},
            "contentLength":512,"followRedirects":true,"maxRedirects":5,
            "headers":{"Content-Length":["512"],"content-type":["application/json"]}},
 "response":{"redirects":[],"connectionInfo":{"package":"package:cronet_http","client":"CronetHttp"},
             "contentLength":2,"headers":{"content-type":["application/json"]},"isRedirect":false,
             "reasonPhrase":"OK","startTime":1759831601700000,"statusCode":200,"endTime":1759831601710000}}
```

cronet_http fills `connectionInfo` with package and client names, not addresses or ports [src [cronet_client.dart L759–800][cronet]].

#### `ext.dart.io.getHttpProfileRequest(isolateId, id)`
- The spec signature still says `int id`, but 2.0 changed it to String [doc [service_extension.md L140–149, L596–600][svcext]].
- An id starting `from_package/` indexes the developer list (1-based). Any other id is looked up in `HttpProfiler._profile` [src [network_profiling.dart L221–245][np]].
- Returns `"type":"HttpProfileRequest"`: the ref fields plus `requestBody` and `responseBody`, each a **JSON array of byte values (0–255)** [src [http_impl.dart L232–235, L261, L268][http-impl]].
  - `requestBody` appears only once the request has finished sending (`!requestInProgress`).
  - `responseBody` appears once the response has *started*, so a call in mid-download returns the bytes received so far.
  - For package entries, `requestBody` appears once `requestEndTimestamp` is set, and `responseBody` once `responseData.endTime` is set [src [network_profiling.dart L37–40][np]].
```json
{"jsonrpc":"2.0","id":"8","method":"ext.dart.io.getHttpProfileRequest","params":{"isolateId":"isolates/4419843374421743","id":"8392571130443811"}}
```
```json
{"jsonrpc":"2.0","result":{"type":"HttpProfileRequest","id":"8392571130443811","isolateId":"isolates/4419843374421743",
 "method":"GET","uri":"https://api.example.com/v1/orders?id=4b67","events":[…],"startTime":1759831600950000,
 "endTime":1759831601010100,"request":{…},"response":{…},
 "requestBody":[],"responseBody":[123,34,105,100,34,58,49,125]},"id":"8"}
```
(`[123,34,105,100,34,58,49,125]` is `{"id":1}`.) An unknown id gives -32602 `Unable to find request with id: '<id>'`.

A real fixture from DevTools tests is copied below, converted from a Dart map literal [src [devtools test_data/network.dart L122–180][dt-fixture]]. **It was captured with a pre-4.0 SDK**: its timestamps are timeline-clock µs, and its `request` still contains `filterKey`. It is still useful for header and field shapes:

```json
{"type":"HttpProfileRequest","id":"1","isolateId":"isolates/2013291945734727","method":"GET",
 "uri":"https://jsonplaceholder.typicode.com/albums/1?userId=1&title=myalbum",
 "events":[{"timestamp":6326808941,"event":"Connection established"},{"timestamp":6326808965,"event":"Request sent"},
           {"timestamp":6327090622,"event":"Waiting (TTFB)"},{"timestamp":6327091650,"event":"Content Download"}],
 "startTime":6326279935,"endTime":6326808974,
 "request":{"headers":{"content-length":["0"]},
   "connectionInfo":{"localPort":45648,"remoteAddress":"2606:4700:3033::ac43:bdd9","remotePort":443},
   "contentLength":0,"cookies":[],"followRedirects":true,"maxRedirects":5,"method":"GET","persistentConnection":true,
   "uri":"https://jsonplaceholder.typicode.com/albums/1","filterKey":"HTTP/client"},
 "response":{"startTime":6327090749,
   "headers":{"content-encoding":["gzip"],"pragma":["no-cache"],"connection":["keep-alive"],"cache-control":["max-age=43200"],
              "content-type":["application/json; charset=utf-8"]},
   "compressionState":"HttpClientResponseCompressionState.decompressed",
   "connectionInfo":{"localPort":45648,"remoteAddress":"2606:4700:3033::ac43:bdd9","remotePort":443},
   "contentLength":-1,"cookies":[],"isRedirect":false,"persistentConnection":true,"reasonPhrase":"OK","redirects":[],
   "statusCode":200,"endTime":6327091628},
 "requestBody":[],"responseBody":[…]}
```

#### `ext.dart.io.clearHttpProfile(isolateId)`
- Returns `{"type":"Success"}` [src [network_profiling.dart L129–131, L169][np]].
- **It clears only the dart:io `HttpProfiler`** (`_profile.clear()`). package:http_profile entries are never cleared; nothing removes them from `_developerProfilingData` [src [http_impl.dart L23][http-impl], [http_profiling.dart][devprof]].
- Requests still in flight when the profile is cleared stay unrecorded, because their `_HttpProfileData` is no longer in the map [doc [service_extension.md L151–161][svcext]].
- **The state is shared by all clients**: DevTools' "Clear" button calls this on every isolate (§5).

#### Socket profiling: `ext.dart.io.socketProfilingEnabled(isolateId, enabled [optional])`, `ext.dart.io.getSocketProfile`, `ext.dart.io.clearSocketProfile`
- `socketProfilingEnabled` returns `{"type":"SocketProfilingState","enabled":<bool>}` [src [network_profiling.dart L247–260][np]].
- `getSocketProfile` returns `{"type":"SocketProfile","sockets":[{"id":"<string>","startTime":…,"address":"…","port":443,"socketType":"tcp","endTime":…,"readBytes":…,"writeBytes":…,"lastWriteTime":…,"lastReadTime":…}]}` [src [L262–282, L391–406][np]].
- **Socket timestamps use `Timeline.now`** (the monotonic timeline clock, CLOCK_MONOTONIC on Android [src [os_android.cc L143–165][os-android]]), *not* the epoch. DevTools converts them with `getVMTimelineMicros()` [src [network_service.dart L36–49][dt-svc]].
- Only sockets opened after profiling was enabled are tracked. TCP `connect`/`accept` and UDP are recorded in `socket_patch.dart` [src [socket_patch.dart ~L2220–2300][sockpatch]].
- This is the only dart:io view of traffic that bypasses `HttpClient` (gRPC, http2_adapter): addresses, ports and byte counts, with no HTTP semantics.

#### WebSocket profiling (Dart main only, not in 3.13 stable)
`ext.dart.io.getWebSocketProfile(isolateId, updatedSince)` returns `{"type":"WebSocketProfile","timestamp":…,"connections":[…]}`. `getWebSocketConnection(isolateId, id)` and `clearWebSocketProfile` also exist [src [network_profiling.dart L194–219][np]]. It is gated by the same `enableTimelineLogging` flag. Nothing beyond its existence was researched.

### 3.3 Field reference for `@HttpProfileRequest` (v4.0)

| Field | Type, origin |
|---|---|
| `id` | **string, opaque** (details in §3.7). |
| `isolateId` | `"isolates/<n>"`, taken once per isolate from `Service.getIsolateId(Isolate.current)` [src [http_impl.dart L241][http-impl]]. |
| `method` | Upper-cased method. |
| `uri` | The URI passed to `openUrl`. |
| `events` | `[{timestamp, event, arguments?}]` [src [L39–52][http-impl]]. Values that occur, from the code: `Connection established`, `Request sent`, `Waiting (TTFB)`, `Content Download`, `Authentication`, `Retrying`, `Proxy tunnel established`, and `Proxy failed to establish tunnel (<code> <reason>)`. **Each timestamp marks the end of the phase it names:** `Request sent` fires when the request object is created, before any body is written; `Waiting (TTFB)` fires when response headers arrive; `Content Download` fires when the body is done [src [L1506–1530, L179–188, L3036, L776–778, L2513, L2529][http-impl]]. |
| `startTime` | µs since the epoch at `_openUrl`, before DNS and connect [src [L55–68, L3021–3025][http-impl]]. |
| `endTime` | When the request finished sending (`outgoing.done`) or failed [src [L112–131, L2345–2348, L170–176][http-impl]]. Present iff `request` is present. |
| `request` | `headers`, `connectionInfo` {`localPort`, `remoteAddress` (IP string), `remotePort`}, `contentLength` (-1 when chunked), `cookies` (`Cookie.toString()` strings), `followRedirects`, `maxRedirects`, `method`, `persistentConnection`, `uri`, `proxyDetails` {`host`, `port`, `username`}, `error` (string). On error only `proxyDetails` and `error` appear [src [L112–131, L77–88, L217–224][http-impl]]. |
| `response` | `startTime` (headers received), `headers`, `compressionState`, `connectionInfo`, `contentLength` (the Content-Length header or -1; this is the **wire** length), `cookies`, `isRedirect`, `persistentConnection`, `reasonPhrase`, `redirects` [{`location`, `method`, `statusCode`}], `statusCode`, `endTime` (body done or error), `error` [src [L134–168, L179–200, L225–231][http-impl]]. |

There is no HTTP version field; dart:io's `HttpClient` speaks HTTP/1.1 only. TLS certificate details are not exposed; the source has a TODO comment "consider exposing certificate information?".

`compressionState` comes from `response.compressionState.toString()`, giving `"HttpClientResponseCompressionState.decompressed|compressed|notCompressed"`. **package:http_profile entries write the bare enum `.name`** (`"decompressed"`) instead [src [hp-resp L150–158][hp-resp]]. Accept both.

Error strings are free text, for example a `SocketException`/`HandshakeException` `toString()`, `"Connection was upgraded"`, or `"Socket has been detached"` [src [L708–710, L733, L748–750][http-impl]].

### 3.4 Bodies: complete or capped?

- **Uncapped and complete.** Bodies are kept in plain `<int>[]` lists (`requestBody`, `responseBody` [src [L261, L268][http-impl]]). There is no size limit anywhere in the path, and they **live in the app's heap until `clearHttpProfile`**. Long sessions therefore grow app memory.
- **Request body.** These are the bytes the app writes (`add`, `addStream`, `write`) [src [L1104, L1205–1220][http-impl]], before chunked framing.
- **Response body.** These are the bytes **as the app reads the stream**, appended inside a `.map()` on the stream the app listens to [src [L702–727][http-impl]]. Consequences:
  1. Gzip with `autoUncompress == true` (the default): the captured body is **decompressed**, and `compressionState` says `decompressed`.
  2. With `autoUncompress == false` you get the **compressed** bytes. Flutter's `NetworkImage` uses `HttpClient()..autoUncompress = false` [src [_network_image_io.dart L97][netimg]].
  3. If the app never reads the body, nothing is captured.
  4. A partially read body is partial.
- **Transfer cost.** `getHttpProfileRequest` returns the body as a JSON number array, roughly 3.5–4 bytes of JSON per body byte. The whole thing is `json.encode`d **on the app's isolate**, so for a UI isolate this means jank. It arrives as one text frame. Size your WebSocket client's message and frame limits for that, or fetch bodies lazily as DevTools does. (I recall tungstenite defaulting to 64 MiB per message and 16 MiB per frame [unverified]; check the version in use.)

### 3.5 Headers: order and duplicates

- **Shape.** `headers` is `{name: [values…]}` built by `formatHeaders`: `headers.forEach((name, values) => newHeaders[name] = values)` [src [http_impl.dart L95–101][http-impl]].
- **Order across names is lost.** `_HttpHeaders` stores a `HashMap<String, List<String>>` [src [http_headers.dart L7–31][http-headers]], so the iteration order (and the JSON key order) is hash order.
- **Duplicates of one name are kept**, as separate list entries in insertion order. Repeated `set-cookie` arrives as `["a=1…","b=2…"]`.
- **Names are lowercased** (`_validateField` → `toLowerCase()` [src [L676–686][http-headers]]). The exception is request headers added with `preserveHeaderCase: true`, which `forEach` reports under their original case [src [L57–66, L120–125][http-headers]].
- **Response headers come from the parser**, which lowercases names. It splits `Connection` values into tokens, and drops `Content-Length` when `Transfer-Encoding` is present [src [http_parser.dart L759–828][http-parser]].
- **Request headers** are the final set at send time. That includes what `HttpClient` adds (`host`, `accept-encoding: gzip`, `user-agent: Dart/<major.minor> (dart:io)`, `content-length` or `transfer-encoding`) [src [http_impl.dart L2305–2312, L4130–4136][http-impl]].
- **package:http_profile entries.**
  - cronet_http and ok_http pass package:http's `Map<String,String>` through `headersCommaValues`, which **splits on commas**. `set-cookie` is split only before `token=`, so date-valued headers get split wrongly [src [http_profile utils.dart][hp-utils], [cronet_client.dart L786–800][cronet]].
  - Duplicates are approximations.
  - Case is whatever the client provided; cronet adds a `Content-Length` header with a capital C.

### 3.6 Redirects, auth retries, proxies, WebSocket upgrades

- **Redirects.** Every followed redirect goes through `_openUrlFromRequest` → `_openUrl`, so **each hop gets its own profile entry** [src [http_impl.dart L660–698, L3091–3120][http-impl]].
  - The hop-1 entry's `_responseCompleter` is completed with the *final* response returned by `redirect()` [src [L1562–1604][http-impl]].
  - So **the first entry should show the final status and headers, plus a `redirects` list, while the 3xx response itself is not recorded as such** [unverified at runtime; derived from reading the code].
  - The final hop's body is appended to the final hop's entry.
- **Auth retries.** These add `Authentication` and `Retrying` events and re-open the URL, which creates a new entry [src [L775–792][http-impl]].
- **Proxies.** HTTPS through a proxy creates a child `CONNECT` entry (`HttpProfiler.startRequest(… parentRequest: profileData)`) and adds `proxyDetails` [src [L2470–2533][http-impl]].
- **dart:io `WebSocket.connect`** uses an `HttpClient` [src [websocket_impl.dart L1354–1386][ws-impl]]. The upgrade therefore appears as an HTTP entry (status 101) that ends with `response.error = "Socket has been detached"` after `detachSocket()` [src [http_impl.dart L748–750][http-impl]].

### 3.7 IDs

- **dart:io entries.** `id = _timeline.pass().toString()`, the `TimelineTask` id [src [http_impl.dart L58–60][http-impl]].
  - Task ids come from a per-thread counter seeded with `random_.NextUInt64()` and stored in an `int64_t` [src [thread.cc L139–144][thread-cc], [thread.h L1290][thread-h], [lib/timeline.cc L18–24][timeline-cc]].
  - They are effectively unique, but **can be negative and up to 20 characters long**.
  - Treat the id as an opaque string and key on `(isolateId, id)`.
- **package:http_profile entries.** `id = "from_package/<1-based index>"`, assigned by `addHttpClientProfilingData` [src [developer/http_profiling.dart L16–19][devprof]]. These ids are unique only within one isolate.

---

## 4. What is captured, what is not

`HttpClient.enableTimelineLogging` **must be true**, and nothing turns it on by default:
- Its default is `false`: `static bool _enableTimelineLogging = false;` [src [http.dart L1263–1286][http-dart]].
- The check happens **when a request is opened**: `if (HttpClient.enableTimelineLogging && !dart.vm.product) profileData = HttpProfiler.startRequest(...)` [src [http_impl.dart L3021–3025][http-impl]]. Requests opened earlier are never recorded.
- A code search of flutter/flutter finds no use of `enableTimelineLogging` or `httpEnableTimelineLogging`, so Flutter itself never enables it.
- DevTools enables it on every isolate when the Network page starts recording (§5).
- An app can set `HttpClient.enableTimelineLogging = true` in `main()` to record from launch.
- The flag is per isolate and resets on hot restart (§2.5).

Capturing from the very first request, "from launch", is not really possible from outside the app:
- Extension handlers run as Dart code on the isolate, and calls are **queued while the isolate is paused**, including at start [src [isolate.cc L1310–1333, L3408–3460][isolatecc]; DevTools comments in §5].
- With `--start-paused` you can queue `httpEnableTimelineLogging` and then `resume`. It would run at the next event-loop turn, after `main()`'s synchronous part [unverified].

| Client | Captured? | Why (source) |
|---|---|---|
| dart:io `HttpClient` | **Yes** | It is the instrumented code (above). |
| package:http `Client()` / `IOClient` on Android | **Yes** | `Client()` resolves to `createClient()`, which imports `io_client.dart` when `dart.library.io` exists [src [http client.dart L14–16, L42][http-client]]. `IOClient` wraps `HttpClient()` and calls `openUrl` [src [io_client.dart L85–114][io-client]]. |
| dio default (`IOHttpClientAdapter`) | **Yes** | It calls `HttpClient()..idleTimeout = 3s` and then `openUrl` [src [io_adapter.dart L57, L80, L241–261][dio-io]]. |
| Flutter `NetworkImage` | **Yes**, compressed bytes | `HttpClient()..autoUncompress = false` [src [_network_image_io.dart L97][netimg]]. |
| dart:io `WebSocket.connect` | Handshake only; frames need the new WebSocket profiler on Dart main | §3.6. |
| `cronet_http` ≥ 1.3.0 (Android) | **Yes, via package:http_profile** (`from_package/N`) | CHANGELOG 1.3.0: "Add integration to the DevTools Network View" [src [cronet_http CHANGELOG][cronet-cl]]. The client calls `HttpClientRequestProfile.profile` [src [cronet_client.dart L759–800][cronet]]. |
| `ok_http` ≥ 0.1.0 (Android) | **Yes, via http_profile** | CHANGELOG 0.1.0: "Add DevTools Network View support" [src [ok_http CHANGELOG][okhttp-cl]]. Its OkHttp calls also run on the Java side, so traffic-police's OkHttp capture may report them **twice** [unverified]. |
| `cupertino_http` ≥ 1.5.0 | Yes, via http_profile (iOS/macOS only) | CHANGELOG 1.5.0 [src [cupertino CHANGELOG][cupertino-cl]]. |
| dio `native_dio_adapter` | **Yes, via http_profile** | It depends on `cronet_http ^1.9.0` and `cupertino_http >=2.3.0` [src [native_dio_adapter pubspec L24–25][dio-native]]. |
| dio `http2_adapter` | **No** (sockets only) | It uses `package:http2`'s `ClientTransportConnection` and falls back to `IOHttpClientAdapter` for non-h2 [src [http2_adapter.dart L8, L26–32, L69][dio-h2]]. |
| package:grpc | **No** (sockets only) | It uses `Socket.connect` or `SecureSocket` plus `package:http2` transport [src [grpc http2_connection.dart L21, L393–417][grpc]]. |
| Java/Kotlin networking (OkHttp, HttpURLConnection, WebView, Firebase SDKs) | No | Not Dart. traffic-police's existing device runtime covers OkHttp and HttpURLConnection. |

**package:http_profile** ([README][hp-readme]): *"A package that allows HTTP clients outside of the Dart SDK to integrate with the DevTools Network View … meant for developers implementing HTTP clients."*
- Version 0.1.x is experimental and requires SDK `^3.4.0` [src [pubspec][hp-pubspec]].
- `HttpClientRequestProfile.profile(...)` returns `null` in product mode **or when `HttpClient.enableTimelineLogging` is false**. Its `profilingEnabled` getter is simply `HttpClient.enableTimelineLogging`.
- Otherwise it builds a map: `isolateId`, `requestStartTimestamp`, `requestMethod`, `requestUri`, `events`, `requestData`, `responseData`, `requestBodyBytes`, `responseBodyBytes`, `_lastUpdateTime`, all epoch µs. It passes that map to `dart:developer`'s `addHttpClientProfilingData` [src [http_client_request_profile.dart][hp-profile]].
- `ext.dart.io.getHttpProfile` and `getHttpProfileRequest` merge those maps into the same responses [src [network_profiling.dart L19–42, L115–124, L227–236][np]].
- So the same polling loop sees both kinds of entry.

---

## 5. How the DevTools Network tab drives it

Files: `packages/devtools_app/lib/src/screens/network/network_controller.dart` [dt-ctrl], `network_service.dart` [dt-svc], `network_screen.dart` [dt-screen], `shared/http/http_service.dart` [dt-http], `shared/http/http_request_data.dart` [dt-reqdata], and `devtools_app_shared/lib/src/service/service_utils.dart` [dt-utils].

- **Start.** When the controller is initialized while connected, `startRecording()` runs [src [network_controller.dart L222–236][dt-ctrl]]:
  - It first asks each isolate whether HTTP logging and socket profiling are already on (`_recordingNetworkTraffic`, with a 500 ms timeout) [src [L414–436][dt-ctrl]].
  - It sets the refresh baseline and calls `setVMTimelineFlags(['GC','Dart','Embedder'])`.
  - It runs `toggleHttpRequestLogging(true)` and `toggleSocketProfiling(true)` on **every isolate** that advertises the extension, each with a 500 ms timeout because *"The above call won't complete immediately if the isolate is paused"* [src [L337–384][dt-ctrl], [http_service.dart][dt-http]].
  - Then it starts polling.
- **Polling.** `static const _pollingDuration = Duration(milliseconds: 2000);` with `PeriodicDebouncer.run(_pollingDuration, networkService.refreshNetworkData)` [src [L185, L307–319][dt-ctrl]]. That fires immediately, then every 2 s, skipping a tick while the previous refresh is still running [src [utils.dart L218–263][dt-utils2]].
- **Each refresh** [src [network_service.dart L99–161][dt-svc]]:
  - It calls `getVMTimelineMicros()` for the socket baseline, then refreshes sockets, WebSockets (new) and HTTP.
  - For HTTP it calls `forEachIsolate`, which runs `getVM()` and then visits all `vm.isolates` **in parallel** [src [service_utils.dart L85–93][dt-utils]].
  - Each isolate gets `getHttpProfile(isolateId, updatedSince: lastHttpDataRefreshTimePerIsolate[isolateId] ?? 0)`, and the next value is the response's `timestamp`. The code comment: *"Update the last request time using the timestamp from the HTTP profile instead of DateTime.now() to avoid missing events due to the delay…"*.
- **Detecting new or updated requests.** `CurrentNetworkRequests._updateOrAddRequest` keys by `request.id`: it adds unknown ids and `merge`s known ones, replacing the ref data [src [network_controller.dart L586–618][dt-ctrl]].
  - It ignores `isolateId` in the key, relying on random task ids being unique.
  - "In progress" means `!isResponseComplete`, or `!isRequestComplete` after an error [src [http_request_data.dart L280–290][dt-reqdata]].
- **Bodies are fetched on demand.**
  - Polled entries are wrapped with `requestFullDataFromVmService: false` [src [L604–608][dt-ctrl]].
  - Selecting a row calls `getFullRequestData()`, which calls `getHttpProfileRequest(isolateId, id)` [src [network_screen.dart L370–375][dt-screen], [http_request_data.dart L99–134][dt-reqdata]].
  - The same call runs for "Copy as cURL" [src [network_screen.dart L486–492][dt-screen]] and before an export (`fetchFullDataBeforeExport`).
  - Bodies are shown with `utf8.decode`, falling back to `[Binary data (N bytes)]`.
- **Multiple isolates.** Every refresh visits all isolates. A new isolate starts with `updatedSince` 0, and logging is re-enabled on every `onIsolateCreated` [src [network_controller.dart L196–203][dt-ctrl]].
- **Stop and clear.**
  - "Stop" only stops polling; *"Do not toggle the vm recording state"* [src [L386–402][dt-ctrl]].
  - "Clear" calls `clearSocketProfile`, `clearHttpProfile` and `clearWebSocketProfile` on every isolate, each with a 500 ms timeout [src [network_service.dart L163–175, L301–313][dt-svc]]. This wipes the profile for **every** client.
- **A clock inconsistency inside DevTools.** Comments in `network_service.dart` L17–22 and L51–60 say `updatedSince` "must use the VM's monotonic timeline clock", and `updateLastHttpDataRefreshTime` seeds the baseline from `getVMTimelineMicros()`. Since dart:io extension 4.0, though, `updatedSince` is epoch µs [doc [service_extension.md L605–609][svcext]], and `_refreshHttpProfile` does use the epoch `timestamp`. The monotonic seed is a much smaller number, which just means "return everything". **Use epoch µs** as the spec and the SDK code say.

---

## 6. Gotchas

1. **Paused or busy isolates.** Extension calls are queued to run *on the isolate* before its next event. A breakpoint, PauseStart (`--start-paused`) or a long synchronous computation delays them, possibly forever [src [isolate.cc L1310–1333, L3408–3460][isolatecc]; DevTools' 500 ms timeouts]. Mitigations:
   - Read `pauseEvent.kind` from `getIsolate` (for example `PauseStart` or `PauseBreakpoint`) and subscribe to the `Debug` stream.
   - Put a timeout on every per-isolate call, and don't let one isolate block the others.
   - Don't call `resume` yourself unless you own the session.
2. **Auth codes.** The token is new for every process and appears only in logcat (or `FlutterJNI.vmServiceUri`).
   - A wrong or missing token gives 403.
   - With `--disable-service-auth-codes` the URI is `http://127.0.0.1:<port>/` and the WebSocket path is `/ws`.
   - If the logcat line has rotated out and no in-process agent is available, the only remedy is restarting the app.
3. **DDS single-client mode.** These behaviours date from Dart 2.12 (`d9ca0514bc` "Disconnect existing clients when DDS calls", 2021).
   - When DDS calls `_yieldControlToDDS` [src [dds_impl.dart L240–275][dds-impl]], the VM sends every other client the `DartDevelopmentServiceConnected` event and **disconnects them**. After that it **redirects new WebSocket upgrades** to `ddsUri` [src [vmservice.dart L251–289][vmservice], [vmservice_server.dart L570–586][vms]].
   - **Your direct connection never blocks flutter run or attach.** `_yieldControlToDDS` fails only when *another DDS* is already connected (error 100, with `data.ddsUri`).
   - Ways to learn the DDS URI:
     - The upgrade's 302 `Location` header.
     - The event's `uri` field.
     - `FlutterJNI.vmServiceUri` (§1.7).
     - flutter run's console line `A Dart VM Service on <device> is available at: <uri>` [src [resident_runner.dart L1395–1401][ft-rr]].
     - `flutter run --machine`'s `app.debugPort` event (`wsUri`) [doc [daemon.md L174–176][daemon]].
     - `--vmservice-out-file`, which writes the ws address [src [resident_runner.dart L1145–1158][ft-rr]].
   - The DDS URI is a **host** address (DDS runs where flutter_tools runs), so connect to it directly without adb.
   - DDS multiplexes many clients and forwards unknown RPCs, including `ext.*`, to the VM [src [dds client.dart L342–356][dds-client]].
   - When DDS disconnects and no clients are left, the VM accepts direct connections again [src [vmservice.dart L340–351][vmservice]].
4. **Shared state with DevTools.** If DevTools is open on the same app:
   - Its **Clear** button calls `clearHttpProfile` and wipes un-fetched entries. Keep your own copy and never clear unless the user asks.
   - DevTools never turns logging off, but the app or another tool might. Watch `HttpTimelineLoggingStateChange`.
   - `clearHttpProfile` does not clear `from_package` entries.
5. **IPv6.** `--ipv6` binds the service to `::1` [src [switches.cc L252–257][switches]].
   - `adb forward tcp:0 tcp:<port>` still works: adbd resolves a `tcp:<port>` target with `network_loopback_client`, which *"Try IPv4 first, use IPv6 as a fallback"* [src AOSP [socket_spec.cpp L197–205][adb-spec], [sysdeps/posix/network.cpp L84–91][adb-net]].
   - The log line then contains `http://[::1]:<port>/…`, which the regex accepts.
   - The host-side WebSocket client connects to `127.0.0.1:<hostport>` either way.
6. **Android cleartext rules are irrelevant here.** The VM service is an inbound loopback server reached through adbd, not an outbound client connection. Android's own docs: *"This flag is honored on a best effort basis … there's no expectation that the Socket API will honor this flag"*, and platform HTTP stacks enforce it for *the app's* requests [doc [NetworkSecurityPolicy.isCleartextTrafficPermitted][nsp]]. flutter_tools uses `http://` and `ws://` over adb forward on every API level.
7. **Timeouts.**
   - The server has no request timeout and no WebSocket ping.
   - DevTools uses 500 ms for enable and clear calls and no explicit timeout for `getHttpProfile`.
   - flutter attach warns after 30 s of discovery; ProtocolDiscovery throttles at 200 ms.
   - Suggested: a few seconds for connecting and the upgrade; about 5 s per `getHttpProfile`; longer for body fetches, scaled by expected size.
8. **Frozen processes.** Android's cached-app freezer (ARCHITECTURE §5.5) stops the app's threads, which includes the VM service's HTTP server and isolate event loops, so connects and RPCs hang until the app thaws [unverified for Flutter specifically; follows from freezer semantics]. Reuse the existing `cgroup.events` check.
9. **Clocks.**
   - HTTP profile times are device **wall-clock** µs (`gettimeofday`).
   - Socket profile times and `getVMTimelineMicros` use **CLOCK_MONOTONIC** µs [src [os_android.cc L134–165][os-android]].
   - Neither is traffic-police's CLOCK_BOOTTIME `ts`. The backend needs a wall-to-BOOTTIME offset sampled on the device, and wall-clock steps can distort durations and `updatedSince` (see §3.2).
10. **Memory and JSON size.** Profiles and bodies grow without bound in the app (§3.4). Each body fetch builds a JSON array about 4× the body size on the app isolate.
11. **Ids.** They are opaque strings, may be negative, and `from_package/N` repeats across isolates (§3.7).
12. **New VM service implementation on the horizon.** Commit `f7a049dfb4` (2026-03-10) added an "experimental VM service implementation" based on `package:dart_runtime_service`. It sits behind `--experimental-vm-service` and a build flag, so Flutter does not use it yet. Watch for protocol differences.
13. **Before Dart 3.4 (Flutter 3.22).** Timestamps were monotonic, `events` lived inside `request`, and ids were ints before Dart 3.0. Gate on `ext.dart.io.getVersion` major 4 (§3.1).

---

## 7. Sketch: mapping to traffic-police's `SessionEvent` (suggestion only)

| Profile data | SessionEvent |
|---|---|
| Isolate becomes visible plus `getVersion` 4.x | `SourceUp` (one source per process; isolates as sub-streams), with capabilities for no rules and no pause-on-device. |
| New `(isolateId,id)` with `request` present (or `request.error`) | `Request`: method, uri, headers (map → pairs; flag that order is lost), client = `dart:io` or the `connectionInfo.package` value. |
| `response` with `statusCode` | `Response`: status, reasonPhrase, headers. Conn = `connectionInfo.remoteAddress:remotePort`; no protocol or TLS. |
| `events[]` | `Mark` (map the phase-end names). |
| `response.endTime` and no error | Fetch the body (lazily, or eagerly under a size threshold), then `Body` (offset 0) + `BodyEnd{complete}` + `Completed`. |
| `request.error` / `response.error` | `Failed` with the text (or `BodyEnd{error}`). |
| `Extension` `HttpTimelineLoggingStateChange enabled=false` | `Diagnostic` warning ("capture paused by another tool"). |
| `DartDevelopmentServiceConnected` / 302 | `Marker` ("reconnected via DDS"); no data loss as long as the polling cursor is kept. |

---

## 8. Source index

[vms]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/_internal/vm/bin/vmservice_server.dart
[vmsio]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/_internal/vm/bin/vmservice_io.dart
[vmservice]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/vmservice/vmservice.dart
[message]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/vmservice/message.dart
[runiso]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/vmservice/running_isolates.dart
[svcmd]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/vm/service/service.md
[svcext]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/vm/service/service_extension.md
[np]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/io/network_profiling.dart
[http-impl]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/_http/http_impl.dart
[http-dart]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/_http/http.dart
[http-headers]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/_http/http_headers.dart
[http-parser]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/_http/http_parser.dart
[ws-impl]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/_http/websocket_impl.dart
[devprof]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/developer/http_profiling.dart
[ext]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/developer/extension.dart
[devpatch]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/_internal/vm/lib/developer.dart
[sockpatch]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/sdk/lib/_internal/vm/bin/socket_patch.dart
[io-api-h]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/include/bin/dart_io_api.h
[io-api-impl]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/bin/dart_io_api_impl.cc
[isolatecc]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/vm/isolate.cc
[servicecc]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/vm/service.cc
[svc-iso-h]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/vm/service_isolate.h
[thread-cc]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/vm/thread.cc
[thread-h]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/vm/thread.h
[timeline-cc]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/lib/timeline.cc
[os-android]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/runtime/vm/os_android.cc
[vmsio-ext]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/pkg/vm_service/lib/src/dart_io_extensions.dart
[vmutils]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/pkg/vm_service/lib/utils.dart
[dds-impl]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/pkg/dds/lib/src/dds_impl.dart
[dds-client]: https://github.com/dart-lang/sdk/blob/62b8169f2b04/pkg/dds/lib/src/client.dart
[settings]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/common/settings.h
[switches]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/shell/common/switches.cc
[switchdefs]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/shell/common/switch_defs.h
[dsi]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/runtime/dart_service_isolate.cc
[dart-isolate]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/runtime/dart_isolate.cc
[dartio]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/lib/io/dart_io.cc
[rt-hooks]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/lib/ui/dart_runtime_hooks.cc
[flutter-main]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/shell/platform/android/flutter_main.cc
[jni]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/shell/platform/android/io/flutter/embedding/engine/FlutterJNI.java
[loader]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/shell/platform/android/io/flutter/embedding/engine/loader/FlutterLoader.java
[shellargs]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/shell/platform/android/io/flutter/embedding/engine/FlutterShellArgs.java
[engflags]: https://github.com/flutter/flutter/blob/d454b1b841d2/engine/src/flutter/shell/platform/android/io/flutter/embedding/engine/flags/FlutterEngineFlags.java
[ft-globals]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/globals.dart
[ft-pd]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/protocol_discovery.dart
[ft-android]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/android/android_device.dart
[ft-device]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/device.dart
[ft-attach]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/commands/attach.dart
[ft-attachdisc]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/device_vm_service_discovery_for_attach.dart
[ft-mdns]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/mdns_discovery.dart
[ft-dds]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/base/dds.dart
[ft-rr]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/resident_runner.dart
[runhot]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/lib/src/run_hot.dart
[daemon]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/doc/daemon.md
[tmpl]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter_tools/templates/app/android.tmpl/app/src/debug/AndroidManifest.xml.tmpl
[netimg]: https://github.com/flutter/flutter/blob/d454b1b841d2/packages/flutter/lib/src/painting/_network_image_io.dart
[buildmodes]: https://docs.flutter.dev/testing/build-modes
[dt-ctrl]: https://github.com/flutter/devtools/blob/e25ba1336eaa/packages/devtools_app/lib/src/screens/network/network_controller.dart
[dt-svc]: https://github.com/flutter/devtools/blob/e25ba1336eaa/packages/devtools_app/lib/src/screens/network/network_service.dart
[dt-screen]: https://github.com/flutter/devtools/blob/e25ba1336eaa/packages/devtools_app/lib/src/screens/network/network_screen.dart
[dt-http]: https://github.com/flutter/devtools/blob/e25ba1336eaa/packages/devtools_app/lib/src/shared/http/http_service.dart
[dt-reqdata]: https://github.com/flutter/devtools/blob/e25ba1336eaa/packages/devtools_app/lib/src/shared/http/http_request_data.dart
[dt-utils]: https://github.com/flutter/devtools/blob/e25ba1336eaa/packages/devtools_app_shared/lib/src/service/service_utils.dart
[dt-utils2]: https://github.com/flutter/devtools/blob/e25ba1336eaa/packages/devtools_app/lib/src/shared/utils/utils.dart
[dt-fixture]: https://github.com/flutter/devtools/blob/e25ba1336eaa/packages/devtools_app/test/test_infra/test_data/network.dart
[hp-readme]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/http_profile/README.md
[hp-pubspec]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/http_profile/pubspec.yaml
[hp-profile]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/http_profile/lib/src/http_client_request_profile.dart
[hp-resp]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/http_profile/lib/src/http_profile_response_data.dart
[hp-utils]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/http_profile/lib/src/utils.dart
[cronet]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/cronet_http/lib/src/cronet_client.dart
[cronet-cl]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/cronet_http/CHANGELOG.md
[okhttp-cl]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/ok_http/CHANGELOG.md
[cupertino-cl]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/cupertino_http/CHANGELOG.md
[http-client]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/http/lib/src/client.dart
[io-client]: https://github.com/dart-lang/http/blob/585d433ba315/pkgs/http/lib/src/io_client.dart
[dio-io]: https://github.com/cfug/dio/blob/4684e29dabaa/dio/lib/src/adapters/io_adapter.dart
[dio-native]: https://github.com/cfug/dio/blob/4684e29dabaa/plugins/native_dio_adapter/pubspec.yaml
[dio-h2]: https://github.com/cfug/dio/blob/4684e29dabaa/plugins/http2_adapter/lib/src/http2_adapter.dart
[grpc]: https://github.com/grpc/grpc-dart/blob/master/lib/src/client/http2_connection.dart
[adb-spec]: https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/socket_spec.cpp
[adb-net]: https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/sysdeps/posix/network.cpp
[nsp]: https://developer.android.com/reference/android/security/NetworkSecurityPolicy#isCleartextTrafficPermitted()

Plain-URL list (same targets, for readers of the raw file):

- Dart SDK, pinned at `62b8169f2b04`: `sdk/lib/_internal/vm/bin/vmservice_server.dart`, `sdk/lib/_internal/vm/bin/vmservice_io.dart`, `sdk/lib/vmservice/{vmservice,message,running_isolates}.dart`, `runtime/vm/service/{service,service_extension}.md`, `sdk/lib/io/network_profiling.dart`, `sdk/lib/_http/{http_impl,http,http_headers,http_parser,websocket_impl}.dart`, `sdk/lib/developer/{http_profiling,extension}.dart`, `sdk/lib/_internal/vm/lib/developer.dart`, `sdk/lib/_internal/vm/bin/socket_patch.dart`, `runtime/include/bin/dart_io_api.h`, `runtime/bin/dart_io_api_impl.cc`, `runtime/vm/{isolate.cc,service.cc,service_isolate.h,thread.cc,thread.h,os_android.cc}`, `runtime/lib/timeline.cc`, `pkg/vm_service/lib/{utils.dart,src/dart_io_extensions.dart}`, `pkg/dds/lib/src/{dds_impl,client}.dart`. Release-tag reads: `sdk/lib/io/network_profiling.dart` at tags 2.12.0, 2.14.0, 2.16.0, 2.17.0, 2.18.0, 2.19.0, 3.0.0, 3.1.0, 3.2.0, 3.3.0, 3.4.0, 3.12.0, 3.13.0; `sdk/lib/_internal/vm/bin/vmservice_server.dart` at 2.12.0, 2.16.0, 2.17.0, 2.18.0, 2.19.0, 3.0.0.
- Flutter, pinned at `d454b1b841d2`: the engine files above under `engine/src/flutter/`, and flutter_tools files under `packages/flutter_tools/`. FlutterJNI.java was also read at the engine revisions pinned by tags 3.7.0, 3.10.0 and 3.13.0, and `protocol_discovery.dart`/`globals.dart` at tags 2.10.0, 3.0.0, 3.7.0 and 3.10.0.
