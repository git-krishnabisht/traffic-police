# 04 — adb host protocol (client ⇄ adb server ⇄ adbd), verified for netinspect

Legend: **[SRC]** read in source/docs during this task · **[EMP]** observed against the local adb server · **[INF]** inference from verified facts (reasoning given) · **UNVERIFIED** (what was tried is stated).

## 0. Sources, citation keys, probe setup

| Key | What it is |
|---|---|
| `adb@17` | Local clone of AOSP `platform/packages/modules/adb` (GrapheneOS mirror, branch `17`), commit `c10299c6075900ba731ef72e5ffe4b1cf7b69c42` (2026-09-09). Paths are relative to the repo root. |
| `core@<tag>` | `github.com/aosp-mirror/platform_system_core` at AOSP tag (adb lived in `system/core/adb` up to Android 11). `core@main` = archived main `a3b721a3`. `core#<sha>` = a commit there. |
| `los-adb@<branch>` / `los-adb#<sha>` | `github.com/LineageOS/android_packages_modules_adb` (lineage-19.x = Android 12, 20.0 = 13, 21.0 = 14, 22.x = 15, 23.x = 16). |
| `adblib@11ff8856` | `github.com/kroune/platform-tools-base@11ff8856` (studio-main snapshot), prefix `adblib/src/com/android/adblib/`. |
| `chromium@c6b97f11` | `github.com/chromium/chromium@c6b97f1102090cb9bf05ffa6c8cb91d8133060cf`. |
| `linux@72d3fcf8` | `github.com/torvalds/linux@72d3fcf802c4`. |
| `los-sepolicy@lineage-23.2` | `github.com/LineageOS/android_system_sepolicy@885cc500`. |
| `los-art@lineage-22.2` | `github.com/LineageOS/android_art`. |
| `bionic@731631f3` | `github.com/aosp-mirror/platform_bionic`. |
| `fwb@17` | `github.com/GrapheneOS/platform_frameworks_base@92310923` (branch 17). |
| `EMP` | Read-only probes against the running adb server on this Mac: `adb version` = "Android Debug Bridge version 1.0.41 / Version 37.0.0-14910828". One USB device `0011664BC002435`, state `unauthorized`, `transport_id:12`. Scripts: `scratchpad/tmp-adb/probe.py`, `probe2.py`; raw output: `probe-output.txt`, `probe2-output.txt`. The probes did not create forwards, authorize anything, or start or kill the server. |
| Release notes | https://developer.android.com/tools/releases/platform-tools (fetched 2026-09-29) |

Byte-order note: several binary fields are written in the host CPU's native order (`memcpy`/raw struct writes). Android devices and the hosts adb runs on (x86-64, arm64) are little-endian, and adblib explicitly decodes them as little-endian (cited per item). **[INF]** Treat them as little-endian.

---

## 1. Framing (the "smart socket")

**Request = 4 ASCII hex digits (payload length) followed by the payload.**
- [SRC] `adb@17:docs/dev/overview.md:84-93`: "1. A 4-byte hexadecimal string giving the length of the payload / 2. Followed by the payload itself." Example: "Send the string "000Chost:version"".
- [SRC] Server parser, `adb@17:sockets.cpp:800-821`: `uint32_t len = unhex(s->smart_socket_data.data(), 4); if (len == 0 || len > MAX_PAYLOAD) { ... goto fail; }` then `if ((len + 4) > s->smart_socket_data.size()) { ... waiting for ... more bytes`. `unhex` accepts `0-9a-fA-F` and returns `0xffffffff` otherwise (`sockets.cpp:613-654`).
- [EMP] `000Chost:version` (uppercase) → `OKAY00040029`. `0000` → connection closed, no bytes. `zzzzhost:version` → connection closed, no bytes.

**Reply status = `OKAY`, or `FAIL` + 4-hex length + message.**
- [SRC] `adb@17:docs/dev/overview.md:99-105`: "For failure, the 4-byte "FAIL" string, followed by a 4-byte hex length, followed by a string giving the reason". Implementation `adb@17:adb_io.cpp:68-74`: `SendOkay` writes `"OKAY"`; `SendFail` writes `"FAIL"` then `SendProtocolString(fd, reason)`.
- "Protocol string" = `%04x` length + bytes: `adb_io.cpp:37-48` (`StringPrintf("%04x", length).append(s)`). Reader: `adb_io.cpp:50-66`.
- [EMP] `host:foo` → `FAIL001aunknown host service 'foo'`. Messages can be multi-line: unauthorized → `FAIL00a7device unauthorized.\nThis adb server's $ADB_VENDOR_KEYS is not set\nTry 'adb kill-server' if that seems wrong.\nOtherwise check for a confirmation dialog on your device.` Messages can also be empty: `FAIL0000` (see §4).

**Maximum request length.**
- A 4-hex-digit length can express at most 0xFFFF (65535). The server also rejects `len > MAX_PAYLOAD`, where `MAX_PAYLOAD = 1024 * 1024` (`adb@17:adb.h:34`), so 65535 is the practical cap. **[INF]**
- Device-service strings have a tighter limit. They are sent to adbd as the OPEN payload plus a trailing NUL, and the server asserts `CHECK_LE(p->msg.data_length, s->get_max_payload());` (`adb@17:sockets.cpp:579-586`). The per-device maximum is negotiated from the device's CNXN. `MAX_PAYLOAD` was `4096` (`core@android-6.0.0_r1:adb/adb.h:26`), `256 * 1024` (`core@android-7.0.0_r1:adb/adb.h:31-33`), and `1024 * 1024` (`core@android-9.0.0_r1:adb/adb.h:34`).
- The adb client guards this: `adb@17:client/commandline.cpp:630-633` "Old devices can't handle a service string that's longer than MAX_PAYLOAD_V1 ... `if (service_string.size() > MAX_PAYLOAD_V1 && !use_shell_protocol)`". `MAX_PAYLOAD_V1 = 4 * 1024` (`adb.h:33`).
- **[INF]** Exceeding the device limit trips a CHECK in the server process. This is from reading the code; it was not tested.

**No pipelining.** Send one request, wait for its reply, then send the next.
- [SRC] The service name is taken as everything after the header: `service = std::string_view(s->smart_socket_data).substr(4);` (`sockets.cpp:821`). A transport switch discards the buffer: `s->smart_socket_data.clear();` (`sockets.cpp:867`).
- [EMP] Writing `000chost:version000chost:version` in one `write` → `FAIL002eunknown host service 'version\x0000chost:version'`.

**Connection lifetime.**
- One-shot host services: the server replies, then closes the socket. `HostRequestResult::Handled` → `goto fail` → `s->close(s)` (`sockets.cpp:860-864, 939-945`). [EMP] `host:version`, `host:devices`, `host:features`, `host:list-forward` all end in EOF.
- Transport switch keeps the socket open for one more request (`sockets.cpp:865-868`). The overview says the same: "after the "OKAY" answer, all further requests made by the client will go directly to the corresponding adbd daemon" (`overview.md:111-114`).
- Device services: the server sends an OPEN to adbd and does **not** reply immediately (`sockets.cpp:927-937`).
  - When adbd accepts, the server sends `OKAY` (`local_socket_ready_notify` → `SendOkay(s->fd)`, `sockets.cpp:594-600`).
  - When adbd refuses (unknown service, `connect()` failure), the server sends `FAIL` + `"closed"` (`local_socket_close_notify` → `SendFail(s->fd, "closed");`, `sockets.cpp:605-611`).
  - After `OKAY` the smart socket is destroyed (`sockets.cpp:935-936`). The TCP connection becomes a raw bidirectional pipe to that one device service, so you get one device service per TCP connection.
  - If there is no transport or it is not online: `FAIL "device offline (no transport)"` / `"device offline (transport offline)"` (`sockets.cpp:911-920`).

**"host:" services vs device services after a transport switch.**
- [SRC] The prefixes `host-serial:`, `host-transport-id:`, `host-usb:`, `host-local:`, and `host:` route to `handle_host_request`, which runs inside the server (`sockets.cpp:826-858`). Anything else goes to the device.
- After a switch, the socket's `s->transport` is set, and device-scoped host queries use it. Example for features: `s->transport ? s->transport : acquire_one_transport(...)` (`adb.cpp:1430-1434`). `get-state` (1523-1528) and forward (1616-1623) work the same way.
- Device services **cannot** be addressed with `host-serial:<s>:<svc>`. The prefix is consumed, then `host_service_to_socket` returns null and the server replies `unknown host service` (`sockets.cpp:879-886`; `services.cpp:268-307` only knows `track-devices*`, `wait-for-*`, `connect:`, `pair:`, and the mdns services).
- `host:<request>` alone means "the single connected device" (`docs/dev/services.md:80-83`).

**Streaming vs one-shot services** [SRC, by the code paths cited]

| Stream (socket stays open) | One-shot (reply, then close) |
|---|---|
| host: `track-devices`, `track-devices-l`, `track-devices-proto-binary`, `track-devices-proto-text`, `track-mdns-services`; `wait-for-<t>-<state>` (OKAY, then a later OKAY/FAIL) | host: `version`, `devices`, `devices-l`, `features`, `host-features`, `get-state`, `get-serialno`, `get-devpath`, `list-forward`, `forward:*`, `killforward*`, `server-status`, `kill`, `reconnect*`, `disconnect:` |
| device: `shell*`, `exec:`, `abb:`, `abb_exec:` (until the process exits), `sync:` (until QUIT or error), `track-jdwp`, `track-app`, `jdwp:<pid>`, `tcp:`/`local*:` sockets, `dev:`, `framebuffer:` | device: `jdwp` (one list, then close), `reverse:*`, `reconnect` ("done") |

---

## 2. Device tracking

**`host:devices`** → `OKAY` + hex4 + lines `<serial>\t<state>\n`.
- [SRC] `adb@17:transport.cpp:1466-1469,1484`: `*result += serial; *result += '\t'; *result += to_string(t->GetConnectionState());` then `'\n'`.
- [EMP] `OKAY001d0011664BC002435\tunauthorized\n`.

**`host:devices-l`** → `"%-22s %s"` (serial, state), then optional ` <devpath>`, ` product:<p>`, ` model:<m>`, ` device:<d>`, and always ` transport_id:<n>`.
- [SRC] `transport.cpp:1470-1483`. Comment: "Put id at the end, so that anyone parsing the output here can always find it by scanning backwards".
- Empty values are omitted (`transport.cpp:1449-1458`). `model` has every non-alphanumeric character replaced by `_`; other fields only replace `\n` (`transport.cpp:1383-1389`). An empty serial prints as `(no serial number)` (`1461-1464`).
- The list is sorted by transport type, then name (`1499-1505`).
- [EMP] `OKAY003c0011664BC002435        unauthorized usb:2-1 transport_id:12\n`. product/model/device are absent until the device completes CNXN.

**`host:track-devices` / `host:track-devices-l`**: same payload as above, re-sent as a hex4-framed message on every change.
- [SRC] `transport.cpp:705-715`: `snprintf(buf, sizeof(buf), "%04x", static_cast<int>(string.size()));`.
- The first message is sent immediately on connect ("We want to send the device list when the tracker connects for the first time, even if no update occurred", `717-726`).
- Updates fire on transport register/unregister and on any state change: `SetConnectionState(...) { ... update_transports(); }` (`1125-1129`), which loops over trackers (`763-774`). Every update carries the full list, and consecutive messages can be identical.
- The client must not write: "you can't read from a device tracker, close immediately" (`699-703`). [EMP] Writing one byte → server closes the socket (EOF).
- [EMP] `track-devices` → `OKAY001d...` and the socket stays open.

**Proto variants** `host:track-devices-proto-binary` / `-proto-text` (`adb@17:services.cpp:270-277`).
- Each message is hex4 + a serialized `adb.proto.Devices` (binary or TextFormat), built in `transport.cpp:1422-1447`.
- Schema, `adb@17:proto/adb_host.proto:47-62`: `string serial = 1; ConnectionState state = 2; string bus_address = 3; string product = 4; string model = 5; string device = 6; ConnectionType connection_type = 7; int64 negotiated_speed = 8; int64 max_speed = 9; int64 transport_id = 10;`, wrapped in `message Devices { repeated Device device = 1; }`.
- Enums (`adb_host.proto:25-45`): `ANY=0, CONNECTING=1, AUTHORIZING=2, UNAUTHORIZED=3, NOPERMISSION=4, DETACHED=5, OFFLINE=6, BOOTLOADER=7, DEVICE=8, HOST=9, RECOVERY=10, SIDELOAD=11, RESCUE=12`; `ConnectionType UNKNOWN=0, USB=1, SOCKET=2`.
- [EMP] binary: `OKAY0025\n#\n\x0f0011664BC002435\x10\x03\x1a\x07usb:2-18\x01@\xe0\x03P\x0c`, which decodes to `{serial, state=3, bus_address="usb:2-1", connection_type=1, negotiated_speed=480, transport_id=12}`. Text: `device {\n  serial: "0011664BC002435"\n  state: UNAUTHORIZED\n  bus_address: "usb:2-1"\n  connection_type: USB\n  negotiated_speed: 480\n  transport_id: 12\n}\n`.

**State strings** (text formats). [SRC] `adb@17:adb.cpp:144-173`:

| State | Text |
|---|---|
| offline | `"offline"` |
| bootloader | `"bootloader"` |
| device | `"device"` |
| host | `"host"` |
| recovery | `"recovery"` |
| rescue | `"rescue"` |
| no permissions | `UsbNoPermissionsShortHelpText()` |
| sideload | `"sideload"` |
| unauthorized | `"unauthorized"` |
| authorizing | `"authorizing"` |
| connecting | `"connecting"` |
| detached | `"detached"` |
| any | `"any"` |

- "No permissions" renders as `"no permissions"` + optional ` (<udev problem>)` + `; see [http://developer.android.com/tools/device.html]` (`core@main:diagnose_usb/diagnose_usb.cpp:83-90`). **This state text contains spaces.**
- Only `bootloader, device, host, recovery, sideload, rescue` count as "online" (`adb.h:127-139`).
- Older servers returned `"unknown"` for anything else (`core@android-9.0.0_r1:adb/transport.cpp:855-856`, `default: return "unknown";`).
- `connecting`/`authorizing` were added by `core#704494b0` (2018-05-04): "connecting -> authorizing -> online / connecting -> authorizing -> unauthorized".

**When each variant appeared.** `ADB_SERVER_VERSION` was 32 in Android 5/6, 36 in 7, 39 in 8.x, 40 in 9, and 41 from 10 onward (`core@android-*:adb/adb.h`, `adb@17:adb.h:71`).
- `host:devices`, `host:devices-l`, `host:track-devices`: present in `core@android-5.0.0_r1` (`adb/adb.c:1599-1605`, `adb/services.c:630`), server version 32.
- ` transport_id:` in `-l` output: `core#b122b175` (2017-08-16, "adb: allow selection of a specific transport"). Present in `core@android-8.1.0_r1:adb/transport.cpp:941` (server 39).
- `host:track-devices-l`: `core#b0c18026` (2017-08-15, "adb: add track-devices-l service."). Absent in the android-8.1.0_r1 tag; present in `core@android-9.0.0_r1:adb/services.cpp:441` (server 40).
- Proto variants and host feature `devicetracker_proto_format`: `los-adb#3d155b63` (2023-11-30, "Device speed/maxSpeed retrieval + API"). The `transport_id` field came later in `los-adb#e8d239fd` (2023-12-15, "Add transport_id to track-devices service"). Absent in `los-adb@lineage-20.0:services.cpp`; present in `los-adb@lineage-21.0:services.cpp:252-258`.
- The server version stayed 41 across these changes. Detect proto support via `host:host-features` containing `devicetracker_proto_format` (this is what adblib does: `adblib@11ff8856:impl/SessionDeviceTracker.kt:119-130`).
- Which platform-tools release first shipped the proto trackers: **UNVERIFIED**. The release notes don't mention it. By date it would be 35.0.0 (Feb 2024) or later **[INF]**. The local 37.0.0 has it **[EMP]**.

---

## 3. Transport selection

[SRC] `adb@17:adb.cpp:1297-1355`.

| Request | Reply |
|---|---|
| `host:transport:<serial>`, `host:transport-usb`, `host:transport-local`, `host:transport-any`, `host:transport-id:<decimal id>` | `OKAY` only |
| `host:tport:serial:<serial>`, `host:tport:usb`, `host:tport:local`, `host:tport:any` | `OKAY` + **8-byte transport id** |

- The tport form: `if (!legacy) { WriteFdExactly(reply_fd, &t->id, sizeof(t->id)); }` (`1345-1348`). `TransportId` is `uint64_t` (`adb.h:73`), written in native byte order.
- The adb client reads it: `ReadFdExactly(fd, &result, sizeof(result))` (`client/adb_client.cpp:126-131`). adblib: "Transport ID is a 64-bit integer, little endian ordering" (`adblib@11ff8856:impl/services/AdbServiceRunner.kt:424-431`).
- Failure → `FAIL` + message (`adb.cpp:1351-1353`). [EMP] `host:transport-id:999999` → `FAIL0024no device with transport id '999999'`; `host:transport-id:abc` → `FAIL0014invalid transport id`; `host:tport:serial:does-not-exist` → `FAIL0021device 'does-not-exist' not found`; unauthorized device → the 0xa7-byte multi-line message above.

**Host-query prefixes** (`sockets.cpp:826-849`): `host-serial:<serial>:<req>`, `host-transport-id:<id>:<req>`, `host-usb:<req>`, `host-local:<req>`, `host:<req>`.
- [EMP] A malformed `host-transport-id:abc:get-state` gets **no reply, and the socket stays open**. The code path is `ParseUint` failure → `return -1;` (`sockets.cpp:832-836`). Always send decimal ids.

**Which to prefer: transport ids.**
- Serial matching is fuzzy. `MatchesTarget` also matches the devpath and `product:`/`model:`/`device:` qualifiers (`transport.cpp:1340-1373`), and duplicates fail with "more than one device with serial " (`transport.cpp:993-998`).
- `host-serial:` has to guess where the serial ends. It handles `[tcp:|udp:]<serial>[:<port>]:<command>`, IPv6 brackets, and `vsock:` (`sockets.cpp:656-780`).
- Ids are unique within one server process: `static std::atomic<TransportId> next(1); return next++;` (`transport.cpp:285-288`). A re-plugged device gets a new id, so a stale id fails loudly ("no device with transport id") instead of silently hitting a new session where our forwards and agent are gone. **[INF]**
- Ids restart at 1 when the server restarts **[INF from the same code]**. Never reuse ids across tracker reconnects.

**History.**
- `transport-id`: `core#b122b175` (2017-08-16), present in `core@android-8.1.0_r1:adb/adb.cpp:1080`.
- `tport:`: `core#79797ecb` (2019-02-21), "adb: tell the client what transport it received". Server version bumped to 41 by `core#aa4f31a1` (2019-02-22): "Increment the server version for adb_connect with transport id, and wait-for-disconnect". Present in `core@android-10.0.0_r1:adb/adb.cpp:1046-1055`.
- On an older server, `host:tport:*` falls through to `unknown host service` **[INF: the pre-2019 handler only matched `transport` prefixes, `core@android-9.0.0_r1:adb/adb.cpp:1059` `if (!strncmp(service, "transport", strlen("transport")))`]**.

---

## 4. Forwarding

**Request and reply (host-scoped).**
```
C→S  <hex4>host-transport-id:<id>:forward:tcp:0;localabstract:<name>
     (or host-serial:<serial>:…, or host:tport:… first and then host:forward:…)
S→C  OKAY OKAY <hex4><decimal port>   then close
```
- [SRC] `adb@17:adb.cpp:1206-1216`: "// On the host: 1st OKAY is connect, 2nd OKAY is status. SendOkay(reply_fd); #endif SendOkay(reply_fd); // If a TCP port was resolved, send the actual port number back. if (resolved_tcp_port != 0) { SendProtocolString(reply_fd, android::base::StringPrintf("%d", resolved_tcp_port)); }".
- The port comes from the listener. `adb_listeners.cpp:220-232`: `listener->fd = socket_spec_listen(listener->local_name, error, &resolved); ... // If the caller requested port 0, update the listener name with the resolved port. if (resolved != 0) { listener->local_name = android::base::StringPrintf("tcp:%d", resolved); ... *resolved_tcp_port = resolved;`. And `socket_spec.cpp:392-394`: `if (result >= 0 && resolved_port) { *resolved_port = adb_socket_get_local_port(result); }`.
- The listener binds loopback, IPv4 first: `socket_spec_listen` → `network_loopback_server(port, SOCK_STREAM, error, true)` (`socket_spec.cpp:382-383`). `sysdeps/posix/network.cpp:122-134`: "Only attempt to listen on IPv6 if IPv4 is unavailable or prefer_ipv4 is false". Connect to `127.0.0.1:<port>`.
- The adb client reads the port as optional: `client/commandline.cpp:1939-1948`: `adb_connect(nullptr, host_prefix + cmd, &error_message, true)` ... `adb_status(...)` ... "// Server or device may optionally return a resolved TCP port number. ... if (ReadProtocolString(fd, &resolved_port, &error_message) && !resolved_port.empty()) { printf("%s\n", resolved_port.c_str()); }". adblib: `forward(...)` uses `runHostDeviceQuery2(device, service, tracker, OkayDataExpectation.OPTIONAL)` with the comment "We receive 2 OKAY answers" (`adblib@11ff8856:impl/AdbHostServicesImpl.kt:238-246`, `impl/services/AdbServiceRunner.kt:84-100`).

**Port-reply history.**
- tcp:0 support: `core#eaae97e1` (2016-04-07, "adb: support forwarding TCP port 0 ... The resolved port number will be printed to stdout"). Present in android-8.0.0_r1, absent in the android-7.0.0_r1 tag (7.0's handler sends only two OKAYs, `core@android-7.0.0_r1:adb/adb.cpp:963-969`).
- In 8.x/9.x the port was returned only for port 0: `if (result >= 0 && port == 0 && resolved_tcp_port)` (`core@android-8.0.0_r1:adb/socket_spec.cpp:208`).
- From Android 10's adb onward it is returned for every *newly bound* TCP listener: `if (result >= 0 && resolved_port)` (`core@android-10.0.0_r1:adb/socket_spec.cpp:282`).
- It is **not** returned when an existing listener is re-pointed. `install_listener` returns `INSTALL_STATUS_OK` early without setting it (`adb@17:adb_listeners.cpp:194-215`).
- Conclusion: after `OKAY OKAY`, read an optional protocol string. EOF means no port.

**Other forward services.**
- `forward:norebind:<local>;<remote>` → `FAIL "cannot rebind existing socket"` when `<local>` already exists (`adb.cpp:1229-1231`, `adb_listeners.cpp:202-206`). Irrelevant for `tcp:0`: the stored names are resolved `tcp:<port>`, so `tcp:0` never matches **[INF]**.
- Bad syntax → `FAIL "bad forward: …"` (`adb.cpp:1186-1191`). Bind failure → `"cannot bind listener: …"` (1225-1228).
- `killforward:<local>` → `OKAY OKAY`, or `FAIL "listener '<local>' not found"` (`adb.cpp:1196-1197,1232-1234`). It matches **by local name only, across all devices** (`adb_listeners.cpp:146-156`).
- `killforward-all` → `OKAY OKAY` (`adb.cpp:1145-1153`). It calls `remove_all_listeners()`, which drops **every forward of every device** except smart sockets (`adb_listeners.cpp:158-169`), including forwards owned by other tools such as Studio.
- `list-forward` → **one** `OKAY` + hex4 + lines `<serial> <local> <remote>\n` for **all** devices (`adb.cpp:1135-1143`; format `adb_listeners.cpp:136-141` `"%s %s %s\n"`, serial or `(reverse)`). [EMP] `host:list-forward` → `OKAY0000`.

**Empty-message failure.**
- [SRC] `adb.cpp:1616-1623`: `auto transport_acquirer = [=](std::string* error) { ... std::string error; return acquire_one_transport(type, serial, transport_id, nullptr, &error); };`. The inner `error` shadows the out-parameter, so the caller's message stays empty.
- [EMP] `host-serial:does-not-exist:forward:tcp:0;localabstract:netinspect_probe` → `FAIL0000`.
- Map an empty FAIL on forward to "device not found / not online".

**Automatic removal.**
- Device disconnect or offline: [SRC] every forward registers a disconnect callback, `transport->AddDisconnect(&listener->disconnect)` (`adb_listeners.cpp:248-254`), with `listener_disconnect` erasing the listener (`116-126`). `handle_offline` runs `close_all_sockets(t); t->RunDisconnects();` (`adb.cpp:201-224`). It is called on any connection error, `HandleError` → `handle_offline(this); transport_destroy(this);` (`transport.cpp:1214-1220`), **and on every (re)CNXN** (`adb.cpp:407-408`). So adbd restarts, `adb root`, and USB re-enumeration also drop forwards.
- Server restart: forwards live only in the server's in-memory `listener_list` (`adb_listeners.cpp:75-77`), so they are lost. **[INF]** Not tested, because killing the server was out of bounds.
- After removal, the old port's listener fd is closed, so a new connect gets ECONNREFUSED, or reaches whatever rebinds that port. **[INF]**

**Forwarding to an abstract socket that doesn't exist.** The forward succeeds; failure shows up only at connect time. [SRC, not empirically tested: the only device is unauthorized]
1. At `forward:` time the server only binds a local listener. `install_listener` does not talk to the device (`adb_listeners.cpp:190-258`).
2. On TCP connect the host accepts, creates a local socket, and sends OPEN to adbd: `listener_event_func` → `create_local_socket` → `connect_to_remote(s, listener->connect_to)` (`adb_listeners.cpp:97-114`). The TCP handshake has already completed.
3. adbd's connect fails: `socket_spec_connect` → `network_local_client(...)`, error "could not connect to %s address" (`socket_spec.cpp:336-353`). `create_local_service_socket` returns null → `send_close(0, p->msg.arg0, t)` (`adb.cpp:529-533`).
4. The host receives CLSE → `s->close(s)` (`adb.cpp:593-615`) → `local_socket_destroy` → `deferred_close`: `adb_shutdown(fd.get(), SHUT_WR)`, then drain reads for up to 1 s before closing. The code notes that closing with pending data "a TCP RST should be sent" and it avoids that (`sockets.cpp:280-325, 328-342`).
5. So the TCP client sees a successful `connect()` followed by **EOF** with zero bytes, not a FAIL string and normally not an RST **[INF]**. `FAIL…closed` is only sent to smart-socket clients.

**Abstract name encoding on the device.**
- adbd connects with libcutils `socket_local_client`: `sun_path[0] = 0; memcpy(p_addr->sun_path + 1, name, namelen)` and `*alen = namelen + offsetof(struct sockaddr_un, sun_path) + 1` (`core@main:libcutils/socket_local_client_unix.cpp:54-67,111`).
- That means no trailing NUL, and the name must satisfy `(namelen + 1) > sizeof(p_addr->sun_path)` → error. `UNIX_PATH_MAX 108` (`linux@72d3fcf8:include/uapi/linux/un.h:7`), so names can be at most 107 bytes.
- The type is SOCK_STREAM (`socket_spec.cpp:345-346`).
- SELinux allows it: `allow adbd appdomain:unix_stream_socket connectto;` (`los-sepolicy@lineage-23.2:private/adbd.te:131`, comment "ndk-gdb invokes adb forward to forward the gdbserver socket").

---

## 5. Shell: `shell:` / `shell,v2,…:` / `exec:`

**Service syntax.** `shell[,arg1,arg2,...]:[command]`. Parsing splits at the **first** `:`, so the command may contain `:` and `,` (`adb@17:daemon/services.cpp:86-123`).
- Args: `raw`, `pty`, `v2` (`services.h:24-26`), `TERM=<x>`. Unknown args are ignored ("not an error to allow for future expansion").
- Defaults: `SubprocessType type(command.empty() ? SubprocessType::kPty : SubprocessType::kRaw);` with no protocol and `TERM=dumb` (`daemon/services.cpp:99-105`).

**Raw without shell protocol becomes a raw-mode PTY.** `adb@17:daemon/shell_service.cpp:828-842`: "If we aren't using the shell protocol we must allocate a PTY to properly close the subprocess ... `if (protocol == SubprocessProtocol::kNone && type == SubprocessType::kRaw) { ... type = SubprocessType::kPty; make_pty_raw = true; }`". Consequences on Android ≥ 7:
- `shell:<cmd>` and `exec:<cmd>` (`daemon/services.cpp:360-362`) both run in a raw-mode PTY.
- stderr is merged into stdout: `dup2(child_stderr_sfd != -1 ? ... : child_stdinout_sfd.get(), STDERR_FILENO)` (`shell_service.cpp:345-348`).
- There is no exit code (table in `shell_service.cpp:19-31`: "Raw No | No No"; "Raw Yes | Yes Yes").
- Before Android 7: `shell:` = `SUBPROC_PTY` and `exec:` = `SUBPROC_RAW` (`core@android-6.0.0_r1:adb/services.cpp:464-467`). `create_subproc_pty` (`:242`) never calls `tcsetattr`/`cfmakeraw`, so the PTY keeps default termios; **[INF]** that means kernel-default output processing (LF→CRLF). `make_pty_raw` exists from `core@android-7.0.0_r1:adb/shell_service.cpp:215-220`.

**Execution.** `execle(_PATH_BSHELL, _PATH_BSHELL, "-c", command_.c_str(), nullptr, cenv.data())` (`shell_service.cpp:389-394`), with `#define _PATH_BSHELL "/system/bin/sh"` (`bionic@731631f3:libc/include/paths.h:42`).

**Shell protocol v2 packets.** Each packet is `[u8 id][u32 length, native/LE][payload]`.
- [SRC] `adb@17:shell_protocol.h:44-58`: `kIdStdin = 0, kIdStdout = 1, kIdStderr = 2, kIdExit = 3, kIdCloseStdin = 4, kIdWindowSizeChange = 5, kIdInvalid = 255`. `:95-105`: "Packets support 4-byte lengths ... Header is 1 byte ID + 4 bytes length". Read/write via `memcpy` of `uint32_t` (`shell_service_protocol.cpp:32-62`). adblib: "The "shell" protocol uses little endian order for serializing packet sizes" (`adblib@11ff8856:impl/ShellV2ProtocolHandler.kt:31-36`).
- Exit code: `output_->data()[0] = exit_code; output_->Write(ShellProtocol::kIdExit, 1)`. On a signal: `exit_code = 0x80 | WTERMSIG(status)` (`shell_service.cpp:761-794`). A spawn failure yields stderr `"error: ..."` plus exit 126 (`798-826`).
- The adb client decodes it as `exit_code = static_cast<uint8_t>(protocol->data()[0]);`, defaulting to 255 when the stream ends without an exit packet ("OpenSSH returns 255 on unexpected disconnection") (`client/commandline.cpp:299-326`).
- Closing stdin: send `04 00000000`. For raw sessions adbd does `adb_shutdown(stdinout_sfd_, SHUT_WR)`; PTYs "can't close just input" (`shell_service.cpp:700-716`). The adb client sends it at local stdin EOF, and only with the protocol: "For older devices we want to just leave the connection open, otherwise an unpredictable amount of return data could be lost" (`commandline.cpp:542-551`).
- Cancelling: closing our socket makes adbd send SIGHUP: "protocol FD died, sending SIGHUP to pid" (`shell_service.cpp:579-594`).

**Reliable exit code: use `shell,v2,raw:<cmd>`.** This is the form adblib builds: `ExecService.SHELL_V2 -> "shell,v2$args:$command"` (`adblib@11ff8856:impl/AdbDeviceServicesImpl.kt:597-603`).
- The feature string is `shell_v2` (`adb@17:transport.cpp:81`). It exists since `core@android-7.0.0_r1:adb/transport.cpp:48`. The adb client picks it with `bool use_shell_protocol = CanUseFeature(*features, kFeatureShell2);` (`commandline.cpp:703-704`).
- The docs agree: "shell,v2: (API>=24)" (`docs/dev/services.md:186-188`).

**Quoting.**
- The adb client does not escape: "// We don't escape here, just like ssh(1). http://b/20564385." (`commandline.cpp:788-792`).
- The string after the first `:` goes verbatim to `sh -c`. Quote each argument in single quotes, replacing `'` with `'\''`, as adb's own `escape_arg` does (`adb@17:adb_utils.cpp:81-102`).
- Newlines are allowed; Chromium sends newline-separated commands in one `shell:` request (`chromium@c6b97f11:chrome/browser/devtools/device/android_device_info_query.cc:21-34`).
- **[INF]** A NUL ends the command, because `execle` takes a C string.
- The old doc text "Arguments cannot contain double quotes" (`services.md:171-176`) describes legacy behaviour.

---

## 6. JDWP and app tracking

**`track-jdwp`** (device service): `OKAY`, then a stream of `<hex4><"pid\n"...>` messages.
- [SRC] `adb@17:daemon/jdwp_service.cpp:247-259`: `snprintf(head, sizeof head, "%04zx", len)`. The body is only debuggable processes: `if (!proc->process.debuggable) continue; ... std::to_string(proc->process.pid) + "\n"` (`195-211`).
- An initial message is sent on connect (`499-509`). Messages are capped by "the max the protocol can handle (hex4)", `UINT16_MAX`, and truncated past it (`453-472`).
- Any client write closes the tracker: "you can't write to this socket" (`511-516`).
- Very old: `core@android-5.0.0_r1:adb/jdwp_service.c:150,618` uses the same `%04x` header. Docs: `services.md:268-279`.

**`jdwp`** (no pid) is a one-shot list without a hex4 header.
- It sends `jdwp_process_list(...)` once and closes on the next ready (`jdwp_service.cpp:403-420`; dispatch `daemon/services.cpp:253-254`).
- The docs claim "there is no single-shot service" (`services.md:279`); the code contradicts that.

**`jdwp:<pid>`**
- adbd creates a socketpair and passes one end to the VM over `@jdwp-control` (`jdwp_service.cpp:328-353`, comment `51-132`).
- Only debuggable pids qualify: "Don't allow JDWP connection to a non-debuggable process" (`355-372`).
- Frozen processes are refused: "Process {} ({}) is frozen. Denying JDWP connection" (`332-337`).
- An unknown pid gives an empty fd, so the smart-socket client gets `FAIL…closed` (§1).

**`track-app`**: `OKAY`, then a stream of `<hex4><binary AppProcesses proto>`, from the same `process_list_msg` framing.
- Includes processes that are debuggable **or** profileable: `if (!proc->process.debuggable && !proc->process.profileable) continue;` (`jdwp_service.cpp:213-234`). Docs: "Each message features a hex4 length prefix followed by a binary protocol buffer" (`services.md:281-301`).
- The daemon sends binary; the "Process count: N" text in the original commit message is what the adb CLI prints (`los-adb#420ad556` diff).
- Feature: `const char* const kFeatureTrackApp = "track_app";` (`adb@17:transport.cpp:93`), documented as "adbd supports `track-app` service reporting debuggable/profileable apps" (`transport.h:90-91`).
- Proto, `adb@17:proto/app_processes.proto:24-38`:
  ```
  message ProcessEntry {
      int64 pid = 1;
      bool debuggable = 2;
      bool profileable = 3;
      string architecture = 4;  // ISA name, e.g., "arm64"
      optional int64  user_id = 5;
      optional string process_name = 6;
      repeated string package_names = 7;
      optional bool waiting_for_debugger = 8;
      optional int64 uid = 9;
  }
  message AppProcesses { repeated ProcessEntry process = 1; }
  ```
- First release on the device side:
  - Added by `los-adb#420ad556` (2020-02-14). Absent from `core@android-11.0.0_r1` (`adb/daemon/services.cpp`, no match); present in `los-adb@lineage-19.0:daemon/services.cpp:244` and `transport.cpp:84`, so Android 12 / API 31. adblib agrees: "Note: "track-app" was added in API 31 (Android "S")" (`adblib@11ff8856:AdbFeatures.kt:63-68`).
  - Fields 5–9 were added by `los-adb#15335e01` (2024-03-05, "Appinfo: Make adb the app debug source of truth"), with feature `app_info` (`los-adb@lineage-22.0:transport.cpp:102`). Android 12–14 protos have only fields 1–4 (`los-adb@lineage-21.0:proto/app_processes.proto:24-29`).
  - ART fills fields 5–9 through `dlsym` with no-op fallbacks (`los-art@lineage-22.2:adbconnection/adbconnection.cc:97-120`). Whether a given device populates them is **UNVERIFIED** (no authorized device). adblib treats them as present only if `hasUserId()` (`impl/AppProcessEntryListParser.kt:42-58`).
  - adbd ships as the updatable APEX `com.android.adbd` (`adb@17:apex/Android.bp:10-44`, defaults `"r-launched-dcla-enabled-apex-module"`; `min_sdk_version: "30"` at `Android.bp:637` (`libadbd_core`) and `:752` (`libadbd`)). Decide by the feature list, not by API level.
- When updates are pushed: when a process connects (`init_jdwp`, `552-570`), when it sends updated info (`jdwp_process_event` FDE_READ, `265-277`), and when it disconnects (`CloseProcess`, `301-307`). The socket "will be closed automatically if the JDWP process terminates (this allows adbd to detect dead processes)" (`68-70`).
- Host server version needed: **none**. The server relays any non-host service verbatim with `connect_to_remote(s->peer, …)` (`adb@17:sockets.cpp:911-937`). Only the adb *CLI* subcommand `adb track-app` needs a new enough client (added in `los-adb#420ad556`). A device without the service answers `FAIL0006closed` **[INF from §1]**.

---

## 7. Features

| Request | Returns |
|---|---|
| `host:features` / `host-serial:<s>:features` / `host-transport-id:<id>:features` | The **device's** advertised list, unfiltered: `SendOkay(reply_fd, FeatureSetToString(t->features()))` (`adb.cpp:1430-1441`). Comma-joined (`transport.cpp:1294-1304`). Fails unless the device is online: [EMP] unauthorized → FAIL "device unauthorized…"; [SRC] offline → "device offline" (`transport.cpp:1059-1062`). |
| `host:host-features` | The **server's** list: `supported_features()`, plus `libusb` if active, plus `push_sync` (`adb.cpp:1443-1452`). [EMP] `shell_v2,cmd,stat_v2,ls_v2,fixed_push_mkdir,apex,abb,fixed_push_symlink_timestamp,abb_exec,remount_shell,track_app,sendrecv_v2,sendrecv_v2_brotli,sendrecv_v2_lz4,sendrecv_v2_zstd,sendrecv_v2_dry_run_send,openscreen_mdns,devicetracker_proto_format,devraw,app_info,server_status,track_mdns,libusb,push_sync` |

The adb CLI's `features` command prints the intersection with the *client binary's* features ("Only list the features common to both the adb client and the device", `commandline.cpp:2186-2195`; `CanUseFeature` at `transport.cpp:1311-1313`). Our client should decide device-side services from the device list alone, since the server just relays them (§6).

Relevant strings, all verbatim from `adb@17:transport.cpp:81-106`. Meanings are quoted from `transport.h:67-105` where a comment exists.

| Feature | Meaning | First seen in source |
|---|---|---|
| `shell_v2` | shell protocol | `core@android-7.0.0_r1` |
| `cmd` | "The 'cmd' command is available" | android-7.0.0_r1 |
| `stat_v2` | STA2/LST2 sync | `core@android-8.0.0_r1:adb/transport.cpp:53` |
| `ls_v2` | LIS2/DNT2 | `core@android-11.0.0_r1:adb/transport.cpp:75` |
| `abb`, `abb_exec` | "android binder bridge (abb) in interactive mode using shell protocol" / "abb using raw pipe" | `core@android-10.0.0_r1:adb/transport.cpp:73,75` |
| `fixed_push_mkdir` | "adbd has b/110953234 fixed" | android-10 |
| `push_sync` | server-appended host feature, "adbd supports `push --sync`" | android-8.1 |
| `sendrecv_v2` (+`_brotli`, `_lz4`, `_zstd`, `_dry_run_send`) | SND2/RCV2 | `core@android-11.0.0_r1:adb/transport.cpp:84` (brotli there; lz4/zstd/dry_run in `los-adb@lineage-19.0`) |
| `track_app` | track-app service | `los-adb@lineage-19.0:transport.cpp:84` |
| `devicetracker_proto_format` | proto device trackers (host) | `los-adb#3d155b63`; `los-adb@lineage-21.0:transport.cpp:100` |
| `delayed_ack` | server⇄adbd flow control; the host adds it only with `ADB_BURST_MODE=1` (`transport.cpp:1281-1287`) | lineage-21.0 |
| `app_info` | extra track-app fields | `los-adb@lineage-22.0:transport.cpp:102` |
| `server_status` | `host:server-status` | `los-adb@lineage-22.1:transport.cpp:103`; adblib: "added in adb v35.0.2" |
| `devraw`, `track_mdns`, `openscreen_mdns`, `remount_shell`, `apex`, `fixed_push_symlink_timestamp`, `libusb` | not needed by netinspect | — |

Feature strings never contain `[:;=,]` ("Do not use any of [:;=,] in feature strings", `transport.h:67-68`).

---

## 8. Sync (push) without the adb binary

[SRC] `adb@17:docs/dev/sync.md`, `file_sync_protocol.h`, `daemon/file_sync_service.cpp`, `client/file_sync_client.cpp`. All integers are little-endian: "all binary integers are Little-Endian in the sync mode" (`sync.md:18-25`). IDs are `MKID(a,b,c,d)` ASCII (`file_sync_protocol.h:21-40`).

**Open the service.** Switch transport, then send `<hex4>sync:` → `OKAY` (`daemon/services.cpp:363-364`).

**Request header.** Every request starts with `SyncRequest { uint32 id; uint32 path_length; }` + path, with no NUL (`file_sync_protocol.h:42-46`). The daemon rejects `path_length > 1024` with "path too long" (`file_sync_service.cpp:811-815`).

**SEND v1** (works on every adbd):
```
"SEND" le32(len(spec)) spec          spec = "<remote path>,<mode as decimal>"   e.g. "/data/local/tmp/ni/agent.so,420"
"DATA" le32(n) <n bytes>              repeat, n ≤ 65536
"DONE" le32(mtime seconds)
← "OKAY" le32(0)   |   "FAIL" le32(len) <message>
```
- Client: `StringPrintf("%s,%d", path.c_str(), mode)` (`client/file_sync_client.cpp:585,737`). Daemon: `spec.find_last_of(',')` and `strtoul(..., nullptr, 0)` (`file_sync_service.cpp:561-579`). Base 0 means decimal or `0`-prefixed octal both parse, and this has held since `core@android-6.0.0_r1:adb/file_sync_service.cpp:331-335`.
- Chunk limit: "Each chunk must not be larger than 64k" (`sync.md:61-65`); `#define SYNC_DATA_MAX (64 * 1024)` (`file_sync_protocol.h:146`). Older adbd enforce it: "oversize data message" (`core@android-9.0.0_r1:adb/file_sync_service.cpp:253-254`).
- DONE: "a sync request "DONE" is sent, where length is set to the last modified time ... The server responds to this last request (but not to chunk requests) with an "OKAY"" (`sync.md:67-70`). The daemon applies it with `lutimes` (`file_sync_service.cpp:552-557`). Success path: `ID_OKAY`, msglen 0 (`419-421`).
- Side effects of `send_impl`:
  - It unlinks existing regular files first (`514-525`).
  - It copies user permission bits to group and other: `mode |= ((mode >> 3) & 0070); mode |= ((mode >> 3) & 0007);` (`532-535`).
  - It creates missing parent directories: `secure_mkdirs(Dirname(path))` on `ENOENT` (`365-371`).
- On failure the daemon sends FAIL, drains incoming DATA until DONE ("keep reading and throwing away ID_DATA packets", `423-453`), then the service loop exits and closes the socket (`handle_sync_command` returns false, `803-869`). After any FAIL, drop the connection and open a new one.
- `QUIT`: `"QUIT" le32(0)` ends the service (`ID_QUIT: return false;`, `852-853`). The adb client sends QUIT and then waits for orderly shutdown (`file_sync_client.cpp:253-264`).

**STAT (existence check).**
- v1: `"STAT" le32(n) path` → 16 bytes `"STAT" le32(mode) le32(size) le32(mtime)`. The daemon zero-fills when `lstat` fails (`file_sync_service.cpp:150-160`). The client treats all-zero as an error: "There's no way for us to know what the error was." (`file_sync_client.cpp:511-515`).
- v2 (needs `stat_v2`): `"STA2"` (stat) or `"LST2"` (lstat) → 72-byte `sync_stat_v2 { id, error, dev, ino, mode, nlink, uid, gid, size, atime, mtime, ctime }` (`file_sync_protocol.h:55-68`). `error = errno_to_wire(errno)`, e.g. `ENOENT` = 2 (`file_sync_service.cpp:162-191`; wire table `sysdeps/errno.cpp:27-48`).

**v2 differences (`sendrecv_v2`).**
- `"SND2" le32(n) path` followed by `sync_send_v2 { "SND2", le32 mode, le32 flags }`. The client sends both in one write (`file_sync_client.cpp:356-406`); v1 put the mode after a comma instead (`file_sync_protocol.h:110-117`).
- Flags: `kSyncFlagBrotli = 1, kSyncFlagLZ4 = 2, kSyncFlagZstd = 4, kSyncFlagDryRun = 0x8000'0000U` (`94-100`). More than one compression flag, or any unknown flag, gets FAIL (`file_sync_service.cpp:581-633`).
- DATA then carries compressed stream chunks. DONE and OKAY/FAIL work as in v1.
- For a few-MB agent `.so`/`.dex`, v1 without compression is enough and universal. **[INF]**

---

## 9. Server lifecycle

**`host:version`** → `OKAY` + `0004` + `%04x` of `ADB_SERVER_VERSION`.
- [SRC] `adb.cpp:1493-1496`: `SendOkay(reply_fd, android::base::StringPrintf("%04x", ADB_SERVER_VERSION));`. `#define ADB_SERVER_VERSION 41` (`adb.h:70-71`, "Increment this when we want to force users to start a new adb server").
- [EMP] `OKAY00040029`.
- There is no other smart-socket protocol version. `A_VERSION 0x01000001` is the server⇄adbd packet protocol (`adb.h:52-58`).
- The adb CLI **kills** any server whose version differs: "adb server version (%d) doesn't match this client (%d); killing..." (`client/adb_client.cpp:311-337`).

**`host:server-status`** (feature `server_status`) → `OKAY` + hex4 + an `AdbServerStatus` proto (`adb.cpp:1357-1397`; `proto/adb_host.proto:64-96`).
- [EMP] Decoded: `version="37.0.0"`, `build="14910828"`, `executable_absolute_path="~/Library/Android/sdk/platform-tools/adb"` (field 7), the log path, `os`, `mdns_enabled`, and so on.

**Kill and restart.**
- `host:kill` → `SendOkay(reply_fd)` then `exit(0)`, with the comment "// Rely on process exit to close the socket for us." (`adb.cpp:1281-1292`). The adb client then waits "for socket orderly shutdown or error, indicating server death" (`adb_client.cpp:232-234`).
- Every open client socket (trackers, shells, forwards' accepted connections) is closed by process exit. Clients see EOF, or ECONNRESET if unread data was pending; new connects get ECONNREFUSED until a server listens again. **[INF: standard TCP semantics after `exit`, not observed. Killing the server was out of bounds.]**
- A track-devices stream therefore ends with EOF, the same as above.

**Startup.**
- The server installs the smart-socket listener **disabled** and accepts only after the USB scan finishes, or after 3 s. "Don't actually accept any connections until adb_wait_for_device_initialization finishes" (`client/main.cpp:159-169`); "We don't accept() client connections until this point" (`client/main.cpp:218-221`); `init_cv.wait_for(lock, 3s, ...)` (`adb.cpp:1673-1676`).
- So the first request after a server start can stall about 3 s. **[INF]**
- State is fresh after a restart: no forwards, and transport ids start from 1 (`transport.cpp:285-288`).

**Address.** The adb CLI uses `ADB_SERVER_SOCKET` (e.g. `tcp:host:port`), otherwise `ANDROID_ADB_SERVER_ADDRESS` + `ANDROID_ADB_SERVER_PORT`, with default port `kDefaultServerPort = 5037` (`client/commandline.cpp:87,1650-1682`).

**How adblib (Android Studio) recovers.**
- Device tracker: `retryWhen` logs "trackDevices() reached EOF, will retry in … millis", emits an **empty device list** with `StateFlowStatus.retrying`, then `delay(retryDelay)` and retries (`adblib@11ff8856:impl/SessionDeviceTracker.kt:85-107`). The default is `TRACK_DEVICES_RETRY_DELAY ... defaultValue = Duration.ofSeconds(2)` (`AdbLibProperties.kt:28-29`).
- It picks `BINARY_PROTO_FORMAT` if `hostFeatures()` contains `devicetracker_proto_format`, else `LONG_FORMAT` (`SessionDeviceTracker.kt:119-130`).
- Channels: tries `127.0.0.1` then `::1`. On `IOException` it starts the server and retries (`impl/AdbChannelProviderWithServerStartup.kt:36-55`). The start command is `adb [-P <port>] start-server` (`AdbServerControllerImpl.kt:288-290`).

---

## 10. `/proc/net/unix` discovery

**Kernel line format** [SRC] `linux@72d3fcf8:net/unix/af_unix.c:3558-3602`:
- Header: `"Num       RefCount Protocol Flags    Type St Inode Path\n"`.
- Row: `seq_printf(seq, "%pK: %08X %08X %08X %04X %02X %5llu", s, refcount, 0, s->sk_state == TCP_LISTEN ? __SO_ACCEPTCON : 0, s->sk_type, <socket state>, sock_i_ino(s));`
- Then, only if the socket is bound: `' '`, then `'@'` for an abstract name (first byte NUL), then the name bytes with embedded NULs printed as `'@'`, then `'\n'`.
- Constants: `#define __SO_ACCEPTCON (1 << 16) /* performed a listen */`, so Flags = `00010000`. `SS_UNCONNECTED` = 1, so St = `01` for a listener. `SOCK_STREAM = 1`, so Type = `0001` (`include/uapi/linux/net.h:48-56`; `include/linux/net.h:108`).
- Accepted server-side sockets **inherit the listener's address**: "copy address information from listening to new sock" ... `smp_store_release(&newu->addr, otheru->addr)` (`af_unix.c:1758-1780`). The same `@name` therefore appears on connected sockets too, so filter on Flags.
- `%pK` prints zeros to unprivileged readers **[INF]**. Don't rely on column widths; split on whitespace. `%5llu` pads the inode.

**Chromium's parser** (DevTools over adb): `chromium@c6b97f11:chrome/browser/devtools/device/android_device_info_query.cc`.
- Command, inside one `shell:` request with newline-separated commands: `"cat /proc/net/unix\n"` (`:21-34`).
- Parsing (`:149-187`):
  ```
  // 00000000: 00000002 00000000 00010000 0001 01 358606 @xxx_devtools_remote
  // We need to find records with paths starting from '@' (abstract socket)
  ... SplitString(line, " \r", ...); if (fields.size() < 8) continue;
  if (fields[3] != "00010000" || fields[5] != "01") continue;
  std::string path_field = fields[7];
  if (path_field.empty() || path_field[0] != '@') continue;
  ... std::string socket = path_field.substr(1);
  ... if (... path_field[socket_name_end] == '_') { pid = path_field.substr(socket_name_end + 1); }
  ```
- So the fields are: [3] Flags must be `00010000` (listening), [5] St must be `01`, [7] Path must start with `@` (abstract). The pid is encoded as a `_<pid>` suffix of the name (e.g. `webview_devtools_remote_<pid>`).

**Permission.** `shell` may read `/proc/net/*`:
- `genfscon proc /net u:object_r:proc_net:s0` (`los-sepolicy@lineage-23.2:private/genfs_contexts:28`).
- `type proc_net, fs_type, proc_type, proc_net_type;` (`public/file.te:58`).
- "# allow shell to look through /proc/ for lsmod, ps, top, netstat, vmstat. r_dir_file(shell, proc_net_type)" (`private/shell.te:399-400`).
- Mapping inode → pid via `/proc/<pid>/fd` as `shell` was **not verified**. Encode the pid in the name, as Chromium does.

---

## 11. Other useful services

- **`abb_exec:`** (device): `abb_exec:` + args joined by NUL (`#define ABB_ARG_DELIMITER ('\0')`, `adb.h:205`; `commandline.h:213`).
  - Example: `abb_exec:package\0path\0<pkg>` → `OKAY`, then raw stdout until EOF, with **no exit code** (`protocol = SubprocessProtocol::kNone`, `daemon/abb.cpp:96-104`). `abb:` uses the shell-v2 protocol and has an exit code.
  - Args are split on NUL (`abb.cpp:51-68`). Dispatch is `daemon/services.cpp:288-290`.
  - Output of `package path`: `pw.print("package:"); pw.println(info.applicationInfo.sourceDir);`, plus one line per split, and exit 1 with no output when the package is absent (`fwb@17:services/core/java/com/android/server/pm/PackageManagerShellCommand.java:741-776`).
  - Features `abb`/`abb_exec`: the docs say "(API>=30)" (`services.md:193-200`), but `core@android-10.0.0_r1` adbd already dispatches `abb:`/`abb_exec:` (`adb/daemon/services.cpp:245-246`) and advertises both (`transport.cpp:1053,1055`). Go by features. adblib joins with `"\u0000"` and rejects embedded NULs (`impl/AdbDeviceServicesImpl.kt:60,340-350`).
- **`<prefix>:get-state`**
  - Returns `to_string(state)` only if `acquire_one_transport(..., accept_any_state=false)` succeeds (`adb.cpp:1523-1534`). For connecting, authorizing, unauthorized, and offline devices it FAILs with "device still connecting", "device still authorizing", "device unauthorized.…", "device offline" (`transport.cpp:1032-1067`).
  - [EMP] `host-serial:0011664BC002435:get-state` → the unauthorized FAIL. Use the tracker for state.
- **`<prefix>:wait-for-<usb|local|any>-<device|recovery|rescue|sideload|bootloader|any|disconnect>`**: the first `OKAY` comes from the smart socket, then a second `OKAY` or `FAIL` when the state is reached (`services.cpp:170-257,278-283`). [EMP] `host:wait-for-any-device` → `OKAY`, then nothing while unauthorized.
- **reconnect** (disruptive; only on explicit user request):
  - `<prefix>:reconnect` → `kick_transport(t, true)`, reply "reconnecting <serial> [<state>]\n" (`adb.cpp:1552-1564`).
  - `host:reconnect-offline` resets offline USB devices (`1414-1428`).
  - Device service `reconnect` → adbd writes "done" and kicks the transport (`daemon/services.cpp:64-67,367-369`).
- **`reverse:<forward-command>`**: device-side listeners, handled by the same `handle_forward_request` (`daemon/services.cpp:69-84`; `services.md:308-323`). Not needed.
- **Fast process-death signals:**
  1. `track-app` / `track-jdwp` push a new list when the app's `@jdwp-control` connection closes, which happens when the process dies (`jdwp_service.cpp:68-70,301-307`). This covers debuggable or profileable apps only.
  2. Our forwarded connection gets EOF when the app's socket closes. adbd sees EOF → `s->close` → remote `shutdown` sends A_CLSE (`sockets.cpp:176-247,516-526`). The host closes the TCP side (`adb.cpp:593-615`, `deferred_close`).
  3. The shell-v2 exit packet for helper commands.

---

## Design implications for netinspect

### A. Exact byte sequences the Rust client must implement

Notation: `<hex4>` = lowercase `%04x` of the payload byte length (payload ≤ 0xFFFF). `S:` = server. All strings are ASCII.

1. **Status reader.** Read 4 bytes.
   - `OKAY` → success.
   - `FAIL` → read `<hex4>` and that many bytes as the message. The message may be empty or multi-line.
   - Anything else → protocol error.
   - EOF before 4 bytes → the server closed (bad request, or a server exit).
2. **Version.** `000chost:version` → S: `OKAY` `0004` `0029`. Parse the payload as hex. Expect 41; still proceed if different, and never kill the server.
3. **Host features.** `0012host:host-features` → S: `OKAY` `<hex4>` `a,b,c`.
4. **Device tracker** (dedicated long-lived connection; never write to it).
   - If host-features contains `devicetracker_proto_format`: `001fhost:track-devices-proto-binary` → S: `OKAY`, then forever `<hex4><Devices proto>`. `0000` means an empty list.
   - Else: `0014host:track-devices-l` → S: `OKAY`, then forever `<hex4><lines>`. Parse `transport_id:<n>` from the end of each line. The state is the token after the padded serial, except "no permissions …", which contains spaces.
   - Key devices by `transport_id`. Dedupe identical consecutive messages.
5. **Device features.** `<hex4>host-transport-id:<id>:features` → S: `OKAY` `<hex4>` `csv`. Only works when state = `device`.
6. **Open a device service.** One TCP connection per service.
   - `<hex4>host:transport-id:<id>` → S: `OKAY` (no id bytes).
   - Alternative when only a serial is known: `<hex4>host:tport:serial:<serial>` → S: `OKAY` + 8-byte LE id.
   - Then `<hex4><service>` → S: `OKAY`, and the socket is now a raw stream. Or `FAIL0006closed` (service missing or connect refused), or `FAIL…device offline…`.
7. **Shell with exit code.** Service `shell,v2,raw:<sh -c string, args single-quoted>`.
   - Immediately send `04 00 00 00 00` (close stdin).
   - Read `[u8 id][u32 LE len][payload]`: `01` stdout, `02` stderr, `03` exit (payload[0] = code; 128+sig on signal).
   - EOF without `03` = transport or device loss (adb reports 255).
   - To cancel, close the socket (adbd sends SIGHUP).
8. **Forward to the app's agent socket.**
   - `<hex4>host-transport-id:<id>:forward:tcp:0;localabstract:netinspect_<pkg>_<pid>` → S: `OKAY` `OKAY` `<hex4><decimal port>`, then EOF.
   - The port string is optional in general; required here since we asked for tcp:0. `FAIL0000` = device not found or not online.
   - Connect to `127.0.0.1:<port>`.
   - Remove: `<hex4>host-transport-id:<id>:killforward:tcp:<port>` → `OKAY` `OKAY`, or FAIL "listener … not found" (fine if already gone).
   - Audit: `0011host:list-forward` → `OKAY` `<hex4>` `serial local remote\n…`. This lists all devices; filter by serial.
9. **Process tracking.**
   - If the device has `track_app`: service `track-app` → `OKAY`, then forever `<hex4><AppProcesses proto>`.
   - Else: `track-jdwp` → `OKAY`, then forever `<hex4>` + `pid\n…`.
   - Never write to these sockets.
10. **Push** (service `sync:` → `OKAY`):
    ```
    "SEND" le32(len) "<remote>,<decimal mode>"
    ("DATA" le32(n≤65536) bytes)*
    "DONE" le32(mtime)
    read 8 bytes: "OKAY" 00000000 | "FAIL" le32(n) + msg
    "QUIT" 00000000, then close
    ```
    - Optional existence check first: `"STAT" le32(n) path` → 16 bytes (all-zero = missing). Or `"STA2"` → 72 bytes with an `error` field, when `stat_v2` is present.
    - One file per SEND. A FAIL kills the session.
11. **Discovery.** `shell,v2,raw:cat /proc/net/unix`. Keep rows where `f[3]=="00010000"` and `f[7]` starts with `@netinspect_`. Take the pid as the digits after the **last** `_`, because package names may contain `_`.
12. **Package path** (optional). `abb_exec:package\0path\0<pkg>` when `abb_exec` is present → raw `package:<path>` lines. Otherwise use shell v2 `pm path '<pkg>'`, which has an exit code.

### B. Fallback matrix

| Missing capability | Fallback |
|---|---|
| Server not reachable (ECONNREFUSED on 127.0.0.1 and [::1]) | Run `adb start-server`, the same way adblib does. Prefer the binary at `server-status` → `executable_absolute_path` if a server was seen earlier, else `$ANDROID_HOME/platform-tools/adb`, else PATH. Retry with backoff (adblib: 2 s). Allow ≥ 5 s for the first reply after a start (the server may hold accepts about 3 s). |
| `devicetracker_proto_format` | `host:track-devices-l` (server ≥ 40), else `host:track-devices` plus per-device `host-transport-id:<id>:...` queries. |
| Server < 41 (no `tport`) | Use `host:transport-id:<id>` (available since 2017) or `host:transport:<serial>`. |
| `track_app` | `track-jdwp` (debuggable pids only), plus shell `cat /proc/<pid>/cmdline` or `ps -A -o PID,NAME` for names. **UNVERIFIED**: `ps` flags were not checked here. |
| `app_info` (no process_name/package_names) | Resolve names via shell as in the previous row. |
| `shell_v2` (Android < 7) | `exec:<cmd>; echo "<marker>$?"` to recover the exit code. Keep service strings ≤ 4095 bytes. Don't half-close. Out of scope if netinspect's minimum API is ≥ 24. |
| `sendrecv_v2` | Not needed; SEND v1 is always used. |
| `abb_exec` | `shell,v2,raw:pm path ...` / `cmd package path ...`. |
| Protocol trouble in general | Shell out to the adb CLI with `-t <transport_id>`: `forward tcp:0 localabstract:X` prints the port, `shell`, `push`, `track-devices [-l][--proto-binary]` (`docs/user/adb.1.md:63,91-122`). **Risk:** a CLI whose version differs from the running server's will *kill and restart the server* (`adb_client.cpp:311-337`), breaking Studio's session. Only use the binary whose version matches `host:version`. |

### C. Risks and required behaviours

1. **Forwards are fragile.**
   - They disappear whenever the transport goes offline: unplug, adbd restart, `adb root`, re-CNXN, or authorization churn. They also disappear on server restart.
   - Re-create forwards on every transition into `device` and on every new transport id.
   - Treat EOF on the forwarded TCP connection as "agent gone or forward gone", then re-check with track-app and `list-forward`.
2. **A forward's success says nothing about the agent.** A missing abstract socket shows up only as connect-then-immediate-EOF. Require an agent hello/version handshake on every connection and time it out.
3. **Never use `killforward-all`.** It removes every forward on the server, including Studio's. `killforward:tcp:<port>` also ignores the device, so only remove ports we created.
4. **Empty FAILs** (`FAIL0000`) on forward mean the device wasn't found.
5. **No pipelining.** Extra bytes corrupt the request, or are dropped after a transport switch. One outstanding request per socket.
6. **Malformed ids hang.** `host-transport-id:<non-number>:` gets no reply. Always format ids as u64 decimal, and use per-request timeouts everywhere.
7. **Transport ids are per-server-process.** Invalidate all cached ids, forwards, and trackers whenever the tracker connection drops. Ids restart at 1 and may collide with ids from before the restart.
8. **Trackers are write-sensitive.** Any byte written closes track-devices, track-jdwp, and track-app. They can send duplicates; dedupe.
9. **Text parsing hazards.** The "no permissions (...); see [...]" state has spaces. `host-serial:` must guess where a colon-bearing serial ends. Prefer proto trackers and transport-id prefixes.
10. **Length limits.** Requests ≤ 65535 bytes. Device service strings must fit the device's max payload, and the server CHECK-aborts otherwise **[INF]**: keep them ≤ 4095 unless `shell_v2` is present. Abstract names ≤ 107 bytes, so truncate or hash long package names in `netinspect_<pkg>_<pid>`.
11. **Feature detection, not API level.** adbd is an updatable APEX (min_sdk 30). Always read `host-transport-id:<id>:features` after the device reaches `device`; it FAILs before that.
12. **Legacy shell pitfalls** (Android ≤ 6). `shell:` is a default-termios PTY (**[INF]** CRLF translation), there is no exit code, and stdin must not be half-closed.
13. **Server restart ≈ reconnect storm.**
    - Every socket gets EOF or RST.
    - Handle it in one supervisor: back off, reconnect the tracker, rebuild the device table from the first snapshot, then re-establish per-device forwards and trackers.
    - Don't restart the server ourselves unless it is actually down, because the server is shared with Studio and the adb CLI.
