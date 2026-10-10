# traffic-police device protocol

Status: **draft v1 for review** (Phase 0). This document is the source of truth for the bytes exchanged between the capture runtime inside an Android app process ("device") and the `traffic-police` host. Conformance is enforced by golden-file tests in both directions (§11).

Contents

1. Conventions and versioning
2. Transport, discovery and access control
3. Framing
4. JSON messages (frame type 1)
5. Body chunks (frame type 2)
6. Connection flow
7. Message reference
8. Rules
9. Timing marks
10. Session files
11. Conformance tests
- Appendix A: adb services used by the host
- Appendix B: NDJSON output of `traffic-police tail`

---

## 1. Conventions and versioning

- "MUST", "SHOULD" and "MAY" are used in the RFC 2119 sense.
- All multi-byte integers in binary headers are **big-endian**.
- **Protocol version** is a single integer, currently `1`. It changes only for incompatible changes. Additive changes (new optional fields, new message types, new mark names, new capability strings) do not change it.
- Receivers MUST ignore unknown JSON fields, unknown message types (`"t"` values), and unknown frame types, and SHOULD log each unknown type once. Senders MUST NOT depend on the peer understanding an addition unless the peer advertised the matching capability.
- A version mismatch is always reported as a `bye` with `reason: "protocol_mismatch"` naming the versions (§7.3), and the host shows a clear message. It never crashes either side.
- JSON integers are exact 64-bit values (`txn`, `seq`, `ts`, byte counts). Decoders MUST NOT coerce them through floating point. (serde_json and the device's hand-written encoder satisfy this.)
- Strings are UTF-8. Header names and values are transmitted exactly as the HTTP client exposed them. Values that are not valid UTF-8 (rare; OkHttp and HttpURLConnection expose `String`s) are transmitted after Java's `String` decoding, i.e. as the client saw them.

### Time

- `ts` fields are **device monotonic time in nanoseconds**: `SystemClock.elapsedRealtimeNanos()` (CLOCK_BOOTTIME). It never goes backwards, keeps counting during device suspend, and is shared by every process on the device, so segments from successive processes (`--follow`) share one time axis.
- Every duration and graph bucket on the host is computed from `ts`. Wall-clock time is used only for labels: `hello` and `pong` carry a `clock` pair `{ "ts": <ns>, "wall_ms": <System.currentTimeMillis()> }` sampled back to back; the host keeps the latest offset per source.
- A device time is at most 2^62 ns (about 146 years after boot). The host treats a message with a later `ts` (or a later time in its `clock`, `started_ts`, `marks` or `since`) as malformed, and drops a body chunk whose `offset` plus length passes 2^64 (since 2026-10-07; the fuzz targets found the arithmetic this protects).

### Identifiers

- `instance`: 128-bit random hex string chosen when the capture runtime starts in a process. It distinguishes process incarnations even if a pid is reused.
- `txn`: transaction id, u64, unique within an instance, allocated from 1 upward. One transaction is one HTTP exchange on the network: an OkHttp call that follows a redirect produces two transactions that share a `call` id.
- `call`: u64 grouping transactions of one OkHttp `Call` (redirect and retry hops, `hop` = 0, 1, …). HttpURLConnection transactions have their own `call`.
- `seq`: u64 event sequence number, unique and strictly increasing within an instance across **all event frames** (JSON events and body chunks), starting at 1. Connection-control messages (`hello`, `replay`, `pong`, `*_ack`, `bye`) carry no `seq`. The host uses `seq` to resume after reconnecting and to discard duplicates.

## 2. Transport, discovery and access control

- The device listens on an **abstract-namespace** Unix socket via `android.net.LocalServerSocket(name)`, with
  `name = "traffic-police_" + package + "_" + pid`.
  Abstract names are limited to 107 bytes. The prefix, separator and a 7-digit pid take 23, which leaves 84 for the package. If the package is longer, it is replaced by its first 75 characters + `"~"` + 8 lowercase hex digits of the CRC-32 of the full package name. The host computes the same function for the fallback path below.
- **Discovery.** The host reads `/proc/net/unix` over `adb shell` (readable by the shell domain on API 26–37) and keeps rows whose flags field is `00010000` (listening), state `01`, and path starts with `@traffic-police_` (the kernel prints abstract names with a leading `@`). This is the same parse Chrome's DevTools uses for `@webview_devtools_remote_<pid>`. If the file is unreadable, the host computes the name from the package and each pid in the process list and probes it.
- **Connection.** The host runs `adb forward tcp:0 localabstract:<name>`, reads back the allocated port, and connects to `127.0.0.1:<port>`. A forward to a name nobody listens on still accepts the TCP connection and then closes it; the host treats EOF before `hello` as "no capture runtime here".
- **Access control.** Before sending or reading any byte, the device calls `LocalSocket.getPeerCredentials()` (kernel `SO_PEERCRED`, public SDK API) and accepts only UID 0 (root) or UID 2000 (shell). With `adb forward`, adbd itself connects to the app's socket (SELinux has allowed `adbd` to connect to app sockets since Android 8.0) and it runs as UID 2000, or 0 after `adb root`. Any other peer is closed immediately without a reply.
- **One client at a time.** When an authorized client connects while another is active, the device sends `bye { reason: "replaced" }` to the old client, closes it, and serves the new one. (A crashed host may leave a half-open connection; takeover makes reconnecting always work.)
- The socket exists only while the capture runtime runs, and the runtime starts only in debuggable apps.

## 3. Framing

Every frame in both directions:

```
 0        4      5
 +--------+------+----------------------------+
 | length | type | payload (length - 1 bytes) |
 +--------+------+----------------------------+
   u32 BE   u8
```

- `length` counts the type byte plus the payload, so it is at least 1.
- **Maximum frame length:** 16 MiB (`16 * 1024 * 1024`). A receiver that reads a larger `length` MUST treat the stream as corrupt: send `bye { reason: "bad_frame" }` if possible and close. It MUST NOT allocate the claimed size first.
- Device-to-host body chunks carry at most 64 KiB of body bytes. JSON messages from the device are normally under 64 KiB; the 16 MiB limit exists for host-to-device rule sets that inline replacement bodies.

| Type | Direction | Payload |
|---:|---|---|
| 1 | both | One UTF-8 JSON object (§4) |
| 2 | device → host | Body chunk (§5) |
| 3–15 | — | Reserved for future protocol use |
| 16–31 | session files only | Host records (§10); never sent on a socket |

## 4. JSON messages (frame type 1)

Every JSON frame is one object with a string field `t` (message type). Common fields:

| Field | Type | Present on | Meaning |
|---|---|---|---|
| `t` | string | all | Message type |
| `seq` | u64 | device events | Event sequence number (§1) |
| `ts` | u64 | device events | Device monotonic ns when the event happened |
| `txn` | u64 | transaction events | Transaction id |
| `id` | u64 | host commands and their replies | Correlates a command with its ack |

Shared value types:

```jsonc
// Header list: ordered, duplicates kept, name case as sent.
"headers": [["Content-Type", "application/json"], ["Set-Cookie", "a=1"], ["Set-Cookie", "b=2"]]

// Thread
"thread": { "name": "DefaultDispatcher-worker-3", "id": 57, "tid": 4371, "origin": "call" }
//   id:     Thread.getId(); tid: the kernel's thread id (android.os.Process.myTid(), the one logcat shows),
//           absent where the runtime cannot read it (off Android; runtimes up to 0.4.0)
//   origin: "call"        = captured in EventListener.callStart() on the thread that executed/enqueued the call
//           "interceptor" = captured in the network interceptor (no listener installed; for enqueue() this is an OkHttp dispatcher thread)
//           "huc"         = HttpURLConnection, captured on the thread that caused the connection

// Stack frame (innermost first). f and l are absent when unknown; l < 0 is not sent.
{ "c": "com.example.api.StatusRepository", "m": "poll", "f": "StatusRepository.kt", "l": 88 }

// Clock pair
"clock": { "ts": 1234567890123, "wall_ms": 1790658651000 }

// Error
"error": { "class": "java.net.SocketTimeoutException", "message": "timeout",
           "causes": [{ "class": "java.net.SocketException", "message": "Socket closed" }] }
```

## 5. Body chunks (frame type 2)

```
 offset  size  field
 0       8     seq      u64  event sequence number (shared with JSON events)
 8       8     txn      u64  transaction id
 16      1     dir      u8   0 = request, 1 = response (as received from the network), 2 = response as delivered to the app (present only when a rule changed the body)
 17      1     flags    u8   reserved, MUST be 0 when sent and ignored when read
 18      8     ts       u64  device monotonic ns when the app wrote (request) or read (response) these bytes
 26      8     offset   u64  position of the first byte of this chunk within the body
 34      n     bytes         1 ≤ n ≤ 65,536
```

- Chunks of one body are sent in increasing `offset` order. A gap (the next `offset` is larger than the bytes received so far) means chunks were dropped on the device; the host marks the body with a gap.
- `dir = 0` bytes are exactly what went to the wire as the request body: what the `RequestBody` wrote for OkHttp (compressed only if the app opted in, e.g. OkHttp 5's `Request.Builder.gzip()`, in which case the request headers carry `Content-Encoding` and the host decodes it), and what the app wrote to the output stream for HttpURLConnection.
- `dir = 1` bytes are the response body **as it came off the wire for OkHttp** (still Content-Encoded, because network interceptors run below OkHttp's transparent gzip), and **as returned by the input stream for HttpURLConnection** (which already removed its own transparent gzip). The host decodes according to the headers of the same direction.
- `dir = 2` bytes are the rewritten body the app actually received. It is never Content-Encoded (the rule engine removes `Content-Encoding`, §8.4).
- Once a body exceeds the capture cap, the device stops sending chunks for it and sends `prog` events instead (§7.1), so sizes and the traffic graph stay correct.

## 6. Connection flow

```
host                                   device
 |  TCP connect via adb forward          |
 |-------------------------------------->|  accept; check peer UID ∈ {0, 2000}
 |                               hello   |
 |<--------------------------------------|
 |  hello_ack (config, rules, resume)    |
 |-------------------------------------->|  apply config and rules
 |                    rules_ack          |
 |<--------------------------------------|
 |          replay{begin} … events … replay{end}
 |<--------------------------------------|  buffered events with seq > resume_after_seq
 |          live events and body chunks  |
 |<======================================|
 |  set_rules / set_config / ping        |
 |-------------------------------------->|
 |          rules_ack / config_ack / pong|
 |<--------------------------------------|
 |  bye (optional)                       |
 |-------------------------------------->|  close
```

- The device sends `hello` immediately after accepting. The host MUST answer with `hello_ack` within 10 s or the device closes the connection.
- **Replay.** The device keeps a ring buffer of recent events (default: the last 1,000 transactions or 32 MiB of body bytes, whichever limit is reached first; whole transactions are evicted, oldest first). After `hello_ack`, it sends `replay { phase: "begin" }`, every buffered event with `seq > resume_after_seq` in original order, then `replay { phase: "end" }`, then live events. Replayed frames keep their original `seq` and `ts`.
- **Resume.** A host reconnecting to the same `instance` sets `resume_after_seq` to the last `seq` it stored, so only newer events are replayed. The host still discards any event whose `seq` it has already applied.
- **Partial transactions.** Because eviction happens per transaction, a host can still receive events for a transaction whose `req` it never saw (evicted before it connected, or dropped). The host MUST create a placeholder ("start not captured") rather than discard them.
- **Keep-alive.** The host sends `ping` every 5 s. If no frame at all arrives for 15 s after a ping, the host treats the connection as dead (a stuck forward) and reconnects. The device needs no keep-alive.
- **Backpressure and loss.** App threads never block on the socket. Events go into a bounded queue (default 8 MiB) drained by one writer thread; on overflow the oldest queued events are dropped and counted, and a `dropped` event is sent when the writer catches up. Events that reached the ring buffer are never reported as dropped; eviction from the ring is normal ageing.
- **Recording paused** (`set_config { recording: false }`): the runtime stops creating transactions. Transactions already in flight finish normally. Rules keep applying.
- **Closing.** Either side MAY send `bye` before closing. The device keeps capturing into the ring buffer after a client disconnects. The rules that connection sent stop applying, so the app behaves normally again once traffic-police quits; when another host has taken over, its rules stay.

## 7. Message reference

### 7.1 Device → host events (carry `seq` and `ts`)

#### `req` — request started

Sent when a transaction starts: OkHttp network-interceptor entry, or the first HttpURLConnection call that connects.

```jsonc
{
  "t": "req", "seq": 41, "ts": 5821334411223, "txn": 7, "call": 5, "hop": 0,
  "method": "GET",
  "url": "https://api.example.com/api/v1/orders/status?orderId=ord_4b67",
  "headers": [["Host", "api.example.com"], ["Accept-Encoding", "gzip"], ["User-Agent", "okhttp/4.12.0"]],
  "client": { "kind": "okhttp", "version": "4.12.0" },          // kind: "okhttp" | "huc"
  "thread": { "name": "DefaultDispatcher-worker-11", "id": 88, "tid": 5120, "origin": "call" },
  "stack": [ { "c": "com.example.shop.orders.OrderStatusPoller", "m": "poll", "f": "OrderStatusPoller.kt", "l": 41 } ],
  "stack_truncated": false,
  "body": { "length": -1, "type": null, "one_shot": false, "duplex": false },   // absent when the request has no body
  "marks": [["call_start", 5821290000000], ["dns_start", 5821291000000], ["dns_end", 5821300000000]],
  "conn": { /* optional; see Conn below */ }
}
```

- `marks` carries timing marks that happened before the transaction existed (OkHttp resolves DNS and connects before network interceptors run). Marks after `req` arrive as `mark` events.
- `stack` is capped at the configured depth (default 64) after removing the runtime's own frames; `stack_truncated` says whether frames were cut.
- `conn` MAY appear on `req` (OkHttp: the connection already exists when network interceptors run) or on `resp` (HttpURLConnection). The host keeps the latest.

`Conn` object:

```jsonc
{
  "id": "c-17",                       // stable per connection within an instance
  "reused": true,
  "protocol": "h2",                   // OkHttp's Protocol.toString(): "http/1.0" | "http/1.1" | "h2" | "h2_prior_knowledge" | "quic" | "h3"
  "remote": { "ip": "142.250.183.14", "port": 443 },
  "proxy": "DIRECT",                  // or "HTTP 10.0.2.2:8888"
  "tls": {
    "version": "TLSv1.3",
    "cipher": "TLS_AES_128_GCM_SHA256",
    "peer": [                         // leaf first; at most 4 entries
      { "subject": "CN=*.example.com", "issuer": "CN=Example Issuing CA 1, O=Example Trust, C=US",
        "not_before_ms": 1780000000000, "not_after_ms": 1787776000000,
        "sha256": "3f…", "san": ["*.example.com", "example.com"] }
    ]
  }
}
```

#### `resp` — response headers received

```jsonc
{
  "t": "resp", "seq": 44, "ts": 5822011000000, "txn": 7,
  "status": 200, "message": "OK", "protocol": "h2",
  "headers": [["content-type", "application/json"], ["content-encoding", "gzip"], ["set-cookie", "a=1"], ["set-cookie", "b=2"]],
  "conn": { /* optional */ }
}
```

These are the **original** status line and headers as received from the network. If a rule changed them, the `rule` event carries what the app received (§8.5).

#### `body_end` — a body finished (or will not be captured further)

```jsonc
{ "t": "body_end", "seq": 51, "ts": 5822130000000, "txn": 7, "dir": "response",
  "bytes": 225, "captured": 225, "state": "complete" }
```

- `dir`: `"request"`, `"response"`, `"delivered"` (same meanings as frame type 2 `dir` 0, 1, 2).
- `bytes`: total bytes that passed through (written by the app or read by it), even beyond the cap.
- `captured`: bytes sent as chunks.
- `state`:
  - `complete`: the stream reached its end and everything was captured.
  - `truncated`: the stream reached its end, `captured < bytes` because of the cap.
  - `closed_early`: the app closed the stream before its end (`bytes` is how much it had read; `0` means "not consumed by the app").
  - `none`: there is no body (e.g. HEAD, 204, 304, GET without body).
  - `not_captured`: body capture is disabled for this direction (`bytes` still counts).
  - `error`: reading or writing failed; a `fail` event follows or preceded.
- `decoded` (optional, default `false`; Phase 5): the chunks are after Content-Encoding decoding, because the HTTP client decompressed the body before the capture saw it. Flutter mode sends it for dart:io bodies that dart:io decompressed. The headers stay as received (`content-encoding: gzip`), and the host does not decode the bytes again.

#### `prog` — body progress past the cap

```jsonc
{ "t": "prog", "seq": 60, "ts": 5823000000000, "txn": 9, "dir": "response", "bytes": 15728640 }
```

Total bytes so far for a body that is no longer being captured. Coalesced to at most 10 per second per body.

#### `mark` — timing mark

```jsonc
{ "t": "mark", "seq": 45, "ts": 5822011500000, "txn": 7, "m": "resp_body_start" }
```

Mark names are listed in §9.

#### `done` — transaction completed

```jsonc
{ "t": "done", "seq": 52, "ts": 5822130100000, "txn": 7 }
```

Sent once, after the last `body_end` of the transaction (or after `resp` when there is no response body).

A gRPC call (Phase 5) ends with its trailers and status:

```jsonc
{ "t": "done", "seq": 61, "ts": 5822930100000, "txn": 9,
  "trailers": [["grpc-status", "5"], ["grpc-message", "sku sku_9999 is not in warehouse blr-1"], ["x-detail", "…"]],
  "grpc": { "code": 5, "status": "NOT_FOUND", "message": "sku sku_9999 is not in warehouse blr-1" } }
```

- `trailers` is the response's trailers in order, as `[name, value]` pairs. gRPC strips `grpc-status` and `grpc-message` before an interceptor sees them; they are rebuilt from the call's status and come first. Binary (`-bin`) values are base64 without padding, as HTTP/2 carries them.
- `grpc` is the call's status: the code, its name, and the description when there is one. A call the server ended has `done` with its status, OK or not; `fail` is for a call the client library ended itself (a deadline, a cancel, no connection), and also carries `grpc` (and `trailers` when some arrived).
- Both fields are optional, are only sent for gRPC calls today, and a host that does not know them ignores them.

#### `fail` — transaction failed

```jsonc
{ "t": "fail", "seq": 70, "ts": 5830000000000, "txn": 11,
  "phase": "response_headers",           // "connect" | "request" | "response_headers" | "response_body" | "unknown"
  "canceled": false,                     // Call.cancel() or HttpURLConnection.disconnect() mid-flight
  "simulated": false,                    // true when a rule's fail action threw it
  "error": { "class": "java.net.SocketTimeoutException", "message": "timeout" },
  "conn": { /* optional, if a connection had been established */ } }
```

#### `rule` — rules changed this transaction

```jsonc
{ "t": "rule", "seq": 47, "ts": 5822012000000, "txn": 7,
  "rules": [{ "id": "force-paid", "name": "Force payment captured" }],
  "changes": [
    { "op": "status", "from": 200, "to": 500, "reason": "Internal Server Error" },
    { "op": "header_set", "name": "Cache-Control", "value": "no-store", "old": ["max-age=60"], "reason": "cache_guard" },
    { "op": "header_add", "name": "X-Debug", "value": "1" },
    { "op": "header_remove", "name": "ETag", "old": ["\"abc\""] },
    { "op": "body_replace", "bytes": 42 },
    { "op": "body_edit", "matches": 3 },
    { "op": "delay", "ms": 3000 },
    { "op": "fail", "exception": "java.net.SocketTimeoutException" }
  ],
  "delivered": { "status": 500, "message": "Internal Server Error",
                 "headers": [["content-type", "application/json"], ["Cache-Control", "no-store"]] } }
```

- `delivered` is present when the status line, headers or body changed; it is the response the app received. The original stays in `resp` and in `dir = 1` chunks. A changed body is sent as `dir = 2` chunks followed by `body_end { dir: "delivered" }`.
- For a `fail` action the `rule` event precedes the `fail` event (which has `simulated: true`); the change's `exception` is the class thrown.
- `reason` marks a change the runtime made itself: `cache_guard` (§8.4, rule 7), or `body_changed` for the `Content-Encoding` removed and the `Content-Length` set with a new body (rule 4).
- `rules` lists every rule that acted, including one whose actions changed nothing: a `replace` that finds nothing reports `body_edit` with `matches: 0`, and the app gets the original.

#### gRPC calls

A gRPC call (grpc-java, Phase 5) is one transaction:

- `req`: `method` `POST`, `url` `<scheme>://<authority>/<service>/<method>` (for example `https://grpc.example.com/shop.inventory.v1.Inventory/GetStock`), `client` `{ "kind": "grpc", "version": "1.84.0" }`, `body` `{ "type": "application/grpc", "duplex": … }` (`duplex` for client and bidirectional streaming).
  - `req` is sent when the call's stream exists (as OkHttp's waits for the network). By then the scheme, the address, the TLS session (`conn`) and the headers as sent are known, including call credentials such as `authorization`. Messages sent before that wait with it.
  - `call_start` in `marks` is when the app made the call.
  - gRPC 1.40 and newer report the stream's headers. Before 1.40 they come from the transport, and only when it reports them.
  - `content-type: application/grpc`, `te: trailers` and, with a deadline, `grpc-timeout` are added. The transport adds them below every interceptor, so no hook sees them. `user-agent` is not known.
- `resp`: `status` 200 (HTTP/2's status is never shown to gRPC's API; a gRPC response always has one), `protocol` `h2`, and the response headers. A trailers-only response (an error with no messages) gets a `resp` with its `content-type`, and the rest of its metadata goes to the trailers.
- Bodies: each message re-marshaled in gRPC's framing (a 0 flag byte, the length as 4 bytes big-endian, the message), the request's as `dir = 0` and the response's as `dir = 1`. Messages are always uncompressed, whatever `grpc-encoding` says. The cap cuts a body only between messages: a message that would pass it is counted (`prog`, `body_end.bytes`) and not sent.
- Marks: `req_headers_end` and `resp_headers_start` from gRPC's stream tracer, `req_body_end` at half-close, `resp_headers_end`, `resp_body_start` and `resp_body_end`.
- The end: `done` with `trailers` and `grpc`, or `fail` (see `done`).

#### `ws` — a WebSocket message

```jsonc
{ "t": "ws", "seq": 93, "ts": 5824100000000, "txn": 12,
  "dir": "out",                          // "out": the app sent it; "in": the app received it
  "op": "text",                          // "text" | "binary" | "close"
  "size": 39,                            // the payload's full size in bytes (0 for a close)
  "text": "{\"type\":\"subscribe\",\"channel\":\"orders\"}" }
{ "t": "ws", "seq": 94, "ts": 5824160000000, "txn": 12, "dir": "in", "op": "binary", "size": 3, "base64": "AQID" }
{ "t": "ws", "seq": 99, "ts": 5839000000000, "txn": 12, "dir": "out", "op": "close", "size": 0, "code": 1000, "reason": "done" }
```

- A WebSocket is one transaction. `req` is the handshake request when the app opens the socket (`GET`, the URL as OkHttp reports it, so `ws://` and `wss://` appear as `http://` and `https://`, with the app's headers; OkHttp adds `Upgrade`, `Connection` and `Sec-WebSocket-*` itself). `resp` is the `101` with its headers, followed by `body_end` with `"none"` for both directions. Then comes one `ws` event per message, in the order the app sent or received them, and `done` when the socket has closed.
- A refused handshake (any status but 101) is a `resp` with that status and a `fail` with `phase: "response_headers"`. A socket that fails after opening sends `fail` with `phase: "response_body"`. One that never connected sends `phase: "connect"`.
- Text payloads are in `text`, binary payloads in `base64` (standard alphabet, padded). Payloads are captured up to the body cap or 1 MiB, whichever is smaller. A longer payload keeps its first bytes and adds `"truncated": true`; `size` is always the full size. Messages the app sends follow `capture_request_bodies`, messages it receives follow `capture_response_bodies`: with that setting off, `text` and `base64` are absent and `size` remains.
- A `close` carries the close `code` and `reason`. `dir: "out"` is the app's close, `dir: "in"` the peer's. Pings and pongs are not reported: OkHttp answers them itself and never shows them to the app.
- Rules never see WebSockets (§8.4, rule 6).
- The capture wraps the app's `WebSocketListener` and the `WebSocket` it sends with. Library mode needs `TrafficPolice.newWebSocket(client, request, listener)` in place of `client.newWebSocket(request, listener)`. In attach mode, the exit hook of `OkHttpClient.newWebSocket` does the same to every socket. The app's listener receives the wrapper too, so everything the app sends goes through it.

#### `dropped` — events lost to queue overflow

```jsonc
{ "t": "dropped", "seq": 900, "ts": 5900000000000, "events": 412, "bytes": 3355443,
  "txns": [301, 302, 305], "txns_truncated": false }
```

`txns` lists up to 100 transactions that lost at least one event; the host marks them.

#### `traffic` — whole-app byte counters

```jsonc
{ "t": "traffic", "seq": 88, "ts": 5824000000000, "rx": 18234112, "tx": 1203340, "since": 5823500000000 }
```

Cumulative `TrafficStats.getUidRxBytes/getUidTxBytes(Process.myUid())` for the app's uid (all sockets of the app, TCP and UDP, every interface; not only captured HTTP). Sampled every 500 ms and sent only when a value changed. `since` is the time of the previous sampling tick: because unchanged samples are not sent, the change since the previous `traffic` event happened within `(since, ts]`, not across the whole quiet gap. The host derives rates from these deltas; this is the default graph source (Android Studio plots the same counters). Absent when the platform reports the counters as unsupported.

#### `diag` — runtime diagnostics

```jsonc
{ "t": "diag", "seq": 3, "ts": 5800000000000, "level": "warn",
  "code": "hook_failed",
  "message": "okhttp3.OkHttpClient.networkInterceptors()Ljava/util/List; not found (minified OkHttp?)",
  "data": { "hook": "okhttp.networkInterceptors" } }
```

Codes defined in v1: `started`, `hook_installed`, `hook_failed`, `hook_other_loader` (a second copy of a hooked class in another class loader was left alone), `okhttp_missing`, `okhttp_unsupported_version`, `studio_inspector_present` (Android Studio's network interceptor is also in the chain), `rule_skipped` (e.g. body edit on an encoding the device cannot decode), `body_cap_reached`, `internal_error`. Diagnostics are events: they are buffered (the last 200 separately from transactions) and replayed like everything else.

### 7.2 Device → host control messages (no `seq`)

#### `hello`

```jsonc
{
  "t": "hello",
  "protocol": 1,
  "runtime": { "version": "0.1.0", "build": "3f2c1ab", "mode": "library" },      // mode: "library" | "attach"
  "instance": "9d1c7e0f5b2a4c83a1e6f0d2b7c94e51",
  "app": { "package": "com.example.shop", "process": "com.example.shop",
           "pid": 4312, "uid": 10234, "debuggable": true },
  "device": { "api": 35, "release": "15", "manufacturer": "Google", "model": "Pixel 8",
              "abi": "arm64-v8a", "abis": ["arm64-v8a", "armeabi-v7a", "armeabi"] },
  "clock": { "ts": 5800000000000, "wall_ms": 1790658651000 },
  "started_ts": 5790000000000,                      // when the runtime started in this process
  "capabilities": ["okhttp", "okhttp_events", "huc", "rules", "pause", "resume", "prog", "traffic"],
  "clients": { "okhttp": "4.12.0" },                // detected versions; null when absent
  "hooks": [                                        // attach mode only; see below
    { "id": "okhttp.networkInterceptors", "target": "okhttp3.OkHttpClient#networkInterceptors()Ljava/util/List;",
      "status": "installed", "hits": 12 },
    { "id": "okhttp.eventListenerFactory", "target": "okhttp3.OkHttpClient#eventListenerFactory()Lokhttp3/EventListener$Factory;",
      "status": "pending", "hits": 0 } ],
  "buffer": { "max_txns": 1000, "max_body_bytes": 33554432,
              "txns": 37, "body_bytes": 812345, "first_seq": 1, "last_seq": 412 },
  "config": { "recording": true, "body_cap": 10485760, "capture_request_bodies": true,
              "capture_response_bodies": true, "stack_depth": 64 }
}
```

Hook `status` values (attach mode): `installed` (class instrumented), `pending` (class not loaded yet; the load hook will instrument it), `class_not_found`, `method_not_found` (typically R8-renamed OkHttp), `failed` (retransform error; `detail` has the JVMTI error name), `other_loader` (a copy in a second class loader was left alone). `hits` counts calls through the hook, which tells "installed but never used" apart from "not installed". Updates after `hello` arrive as `diag` events with codes `hook_installed` / `hook_failed`.

Capabilities in v1:

| Capability | Meaning |
|---|---|
| `okhttp` | OkHttp network interceptor capture is available in this process |
| `okhttp_events` | OkHttp EventListener capture (phase marks, real call site) is available |
| `huc` | HttpURLConnection capture is available |
| `rules` | `set_rules` supported |
| `pause` | `recording` in `set_config` supported |
| `resume` | `resume_after_seq` honoured |
| `prog` | `prog` events sent past the cap |
| `traffic` | `traffic` events are sent |

#### `replay`

```jsonc
{ "t": "replay", "phase": "begin", "from_seq": 1, "to_seq": 412, "events": 412 }
{ "t": "replay", "phase": "end" }
```

#### `rules_ack`, `config_ack`, `pong`

```jsonc
{ "t": "rules_ack", "id": 6, "version": "b7e1", "active": 2,
  "errors": [{ "rule": "bad-regex", "field": "match.path.regex", "message": "Unclosed group near index 7" }] }

{ "t": "config_ack", "id": 5, "config": { "recording": false, "body_cap": 10485760,
  "capture_request_bodies": true, "capture_response_bodies": true, "stack_depth": 64 } }

{ "t": "pong", "id": 7, "clock": { "ts": 5900000000000, "wall_ms": 1790658751000 } }
```

A rule with errors is not active; the other rules are. `config_ack` returns the effective configuration after clamping.

### 7.3 Host → device messages

#### `hello_ack`

```jsonc
{
  "t": "hello_ack", "id": 1,
  "protocol": 1,
  "host": { "name": "traffic-police", "version": "0.1.0" },
  "resume_after_seq": 0,
  "config": { "recording": true, "body_cap": 10485760, "capture_request_bodies": true,
              "capture_response_bodies": true, "stack_depth": 64 },
  "rules": { "version": "b7e1", "rules": [ /* §8.2 */ ] }
}
```

The device answers with `rules_ack { id: 1 }` and then starts the replay.

#### `set_rules`, `set_config`, `ping`

```jsonc
{ "t": "set_rules", "id": 6, "rules": { "version": "b7e1", "rules": [ /* … */ ] } }   // replaces the whole set
{ "t": "set_config", "id": 5, "config": { "recording": false } }                        // partial update
{ "t": "ping", "id": 7 }
```

#### `bye` (either direction)

```jsonc
{ "t": "bye", "reason": "protocol_mismatch", "message": "device speaks protocol 2; this traffic-police supports 1",
  "supported": [1] }
```

Reasons: `shutdown`, `replaced`, `protocol_mismatch`, `bad_frame`, `timeout`, `internal_error`.

- Host receiving a `hello` with an unsupported `protocol`: send `bye { reason: "protocol_mismatch", supported: [...] }`, close, show "The capture runtime in <process> speaks protocol N; this traffic-police supports M. Update the <library|host>."
- Device receiving a `hello_ack` with an unsupported `protocol`: send `bye { reason: "protocol_mismatch" }` and close.

## 8. Rules

### 8.1 File format (`.traffic-police/rules.toml`)

```toml
version = 1

[[rule]]
id = "slow-status"                 # stable identifier; rule-<n> (its position) when missing
name = "Slow status poll"
enabled = true                     # the default

  [rule.match]
  methods = ["GET"]                # any method when absent
  scheme = "https"                 # "http" | "https"
  host = "*.example.com"           # glob; { exact = "…" } or { regex = "…" }
  port = 443
  path = "/api/v1/*/status/**"    # glob; { exact = "…" } or { regex = "…" }
  query = { orderId = "*" }        # every listed parameter must be present and match (glob)

  [[rule.action]]
  type = "delay"
  ms = 3000

[[rule]]
id = "force-paid"
name = "Force payment captured"

  [rule.match]
  path = "/api/v1/orders/status"

  [[rule.action]]
  type = "replace"                 # find-and-replace in a text body
  find = '"payment":"pending"'
  with = '"payment":"captured"'
  regex = false                    # literal by default

  [[rule.action]]
  type = "status"
  code = 200
  reason = "OK"

  [[rule.action]]
  type = "header"
  op = "set"                       # "add" | "set" (replace all values) | "remove"
  name = "Cache-Control"
  value = "no-store"

[[rule]]
id = "checkout-down"
enabled = false

  [rule.match]
  methods = ["POST"]
  path = "/api/v1/checkout"

  [[rule.action]]
  type = "fail"
  exception = "timeout"            # see §8.4 for the list and OkHttp's retry behaviour
  message = "simulated by traffic-police"

[[rule]]
id = "stub-config"

  [rule.match]
  path = "/config"

  [[rule.action]]
  type = "body"
  file = "fixtures/config-error.json"    # relative to .traffic-police/; or text = "…", or base64 = "…"
  content_type = "application/json"      # optional; replaces Content-Type when given
```

The host checks the file before it sends anything, and reports each problem with its line: `version` must be 1; keys it does not know, and fields that belong to another action type, are mistakes; ids are unique; `delay` takes 0 to 600,000 ms; `status` takes 100 to 599, and without `reason` the standard phrase is used (empty when the code has none); header names are tokens and values printable ASCII (OkHttp refuses others); a body has exactly one of `text`, `base64` and `file`; `replace` needs a non-empty `find`, and a `regex` one must compile (§8.3). A file with problems is not used: the rules that were active stay active. A `file` is read when the rules are, and inlined (as text when it is UTF-8, else base64); traffic-police watches it with `rules.toml`, in the UI and in `tail`, `record` and `export --live`, so saving either sends the rules again (ARCHITECTURE.md §5.11).

### 8.2 Wire form

The host normalizes the TOML into explicit matcher objects and inlines files:

```jsonc
{ "id": "force-paid", "name": "Force payment captured", "enabled": true,
  "match": { "methods": ["GET"], "scheme": "https", "host": { "glob": "*.example.com" }, "port": 443,
             "path": { "exact": "/api/v1/orders/status" },
             "query": [{ "name": "orderId", "value": { "glob": "*" } }] },
  "actions": [
    { "type": "replace", "find": "\"payment\":\"pending\"", "with": "\"payment\":\"captured\"", "regex": false },
    { "type": "status", "code": 200, "reason": "OK" },
    { "type": "header", "op": "set", "name": "Cache-Control", "value": "no-store" },
    { "type": "body", "text": "{\"ok\":false}", "content_type": "application/json" },
    { "type": "body", "base64": "iVBORw0KGgo…", "content_type": "image/png" },
    { "type": "delay", "ms": 3000 },
    { "type": "fail", "exception": "timeout", "message": "simulated by traffic-police" } ] }
```

A matcher is exactly one of `{ "exact": s }`, `{ "glob": s }`, `{ "regex": s }`. Absent fields match anything. A rule may also carry `"cache_rewrites": true` (§8.4, default `false`).

### 8.3 Matching

- Glob syntax: `*` matches any run of characters except the field's separator (`.` in hosts, `/` in paths; nothing in query values), `**` matches any run including separators, `?` matches one character other than the separator. Host matching is case-insensitive; path and query matching are case-sensitive; paths are matched in their encoded form (`HttpUrl.encodedPath()`); query parameter names and values are matched decoded.
- Regexes use Java `java.util.regex` syntax on the device and match anywhere in the value (`find`), so `^…$` anchors one to the whole value; globs and exact matchers always match the whole value. The host checks a regex with Rust's `regex` crate first, letting through the Java constructs that crate lacks (look-around, backreferences); the device reports what it cannot compile in `rules_ack`, and that rule is not active.
- All enabled rules whose match succeeds apply, in list order. Within a rule, actions apply in list order.

### 8.4 Application points and semantics

| Action | OkHttp (network interceptor) | HttpURLConnection wrapper |
|---|---|---|
| `delay { ms }` | `Thread.sleep(ms)` before `chain.proceed()` | Sleep in the first call that connects (`connect()`, `getResponseCode()`, `getInputStream()`, a header getter…) |
| `fail { exception }` | Throw before `chain.proceed()`; `proceed()` is never called | Throw from the first call that connects |
| `status { code, reason }` | `response.newBuilder().code(code).message(reason)` | Overrides `getResponseCode()`/`getResponseMessage()` and the status line in `getHeaderField(0)` |
| `header { op, name, value }` | Edits response headers | Overrides header getters |
| `body { text \| base64 \| file }` | Original body read in full (captured as `dir = 1`), closed, replaced | Wrapper returns the replacement stream |
| `replace { find, with, regex }` | Original body read in full, decoded, edited, re-encoded in its own charset | Same, on the stream |

Delays and failures apply before the request is sent, in rule order: each delay sleeps, and the first `fail` throws, after which nothing more applies. A delay ends early when the call is canceled, which then fails as canceled rather than simulated. A simulated failure has phase `connect` and the message `simulated by traffic-police` unless the action gives one.

`fail` exception kinds, and whether OkHttp's retry-on-connection-failure (on by default) tries the call again, as measured on every OkHttp the runtime supports:

| `exception` | Thrown | Retried by OkHttp? |
|---|---|---|
| `timeout` | `java.net.SocketTimeoutException` | No |
| `protocol` | `java.net.ProtocolException` | No |
| `io` | `java.io.IOException` | Only by OkHttp 3.9 to 3.12: when the call was on a pooled connection, or the host has another address to try |
| `connect` | `java.net.ConnectException` | As `io` |
| `unknown_host` | `java.net.UnknownHostException` | As `io` |

A retry is a new attempt: the rule fails it again, and the host records it as another request. OkHttp 3.14 and later retry only failures they saw on a connection, and a rule throws before the request reaches one. OkHttp 3.9 to 3.12 also treat the address as failed, as after a real failure, so while the host has other addresses (IPv4 and IPv6, say) later connections try those first.

HttpURLConnection: after a simulated failure, calls that throw repeat the same exception, getters answer as for a failed connection (`null`, `-1`), and `getErrorStream()` is `null`. A rule's status decides what the app can read, as the platform does: for 400 and above, `getInputStream()` throws (`FileNotFoundException` on Android for every such code; on the JDK for 404 and 410, and `IOException` otherwise) and `getErrorStream()` returns the body; for a status below 400, `getInputStream()` returns it, even when the network's status was an error.

Rules for bodies (these follow from OkHttp's layering: network interceptors see the encoded body, and `BridgeInterceptor` decompresses afterwards based on `Content-Encoding`):

1. `chain.proceed()` is called exactly once, or not at all when a `fail` action applies (OkHttp allows an interceptor to throw before proceeding).
2. A body action reads the original body completely (up to 32 MiB; above that the action is skipped with `diag { code: "rule_skipped" }` and the original streams through), then closes it (OkHttp refuses a follow-up request while the previous body is open).
3. The original is decoded according to `Content-Encoding` (`gzip`, `deflate`; `br` and `zstd` cannot be decoded on the device without extra libraries, so body actions on them are skipped with a diagnostic), then decoded as text using the Content-Type charset (default UTF-8) for `replace`.
4. The new body is delivered **without** `Content-Encoding`, with `Content-Length` set to its length, and with `Content-Type` replaced when the action gave one. OkHttp's transparent gzip therefore leaves it alone, and `ResponseBody.bytes()` length checks hold.
5. Status-only and header-only rules do not buffer: the body streams through the normal tee.
6. Upgrade responses (101, or `Connection: upgrade` on both sides) are never rewritten, whether or not recording is paused. WebSocket calls never reach network interceptors, so rules do not see them at all.
7. OkHttp caches what leaves the network interceptors, so a rewritten response could outlive its rule. Unless the rule sets `cache_rewrites = true`, a rule that changed the status, headers or body also sets `Cache-Control: no-store` on the delivered response, and lists it as `{ "op": "header_set", "name": "Cache-Control", "value": "no-store", "reason": "cache_guard" }`.

### 8.5 Traceability

The original response (status line, headers, body) is always captured and sent (`resp`, `dir = 1`). What the app received is sent as `rule.delivered` and `dir = 2`. The host shows the delivered response by default and toggles to the original with `o`. Exports hold what the app received and keep the original next to it: `original` in `tail --json` lines (Appendix B), `_trafficPolice.original` in HAR files, which read back into the same exchange.

## 9. Timing marks

| Mark | Source (OkHttp EventListener unless noted) |
|---|---|
| `call_start` | `callStart` |
| `dns_start`, `dns_end` | `dnsStart`, `dnsEnd` |
| `connect_start`, `connect_end` | `connectStart`, `connectEnd` (includes TLS) |
| `tls_start`, `tls_end` | `secureConnectStart`, `secureConnectEnd` |
| `conn_acquired`, `conn_released` | `connectionAcquired`, `connectionReleased` |
| `req_headers_start`, `req_headers_end` | `requestHeadersStart`, `requestHeadersEnd` |
| `req_body_start`, `req_body_end` | `requestBodyStart`, `requestBodyEnd`; HttpURLConnection: first write, stream close |
| `resp_headers_start`, `resp_headers_end` | `responseHeadersStart` (first byte), `responseHeadersEnd`; HttpURLConnection: before and after the call that returned the status |
| `resp_body_start`, `resp_body_end` | `responseBodyStart`, `responseBodyEnd`; tee source first read and EOF |
| `call_end` | `callEnd` |

The host derives phases (used by the Overview timing bar and HAR `timings`): queued (`call_start` → first connection mark or `req`), DNS, connect, TLS (inside connect), send (`req_headers_start` → `req_body_end` or `req_headers_end`), wait (→ `resp_headers_start`), receive (→ `resp_body_end`). Missing marks yield "n/a", never guessed values. Without an EventListener only `req`, `resp`, and the tee's body marks are available.

- Marks may repeat. OkHttp 5's fast fallback races several connection attempts on background threads, so `connect_start` can appear more than once; the host uses the earliest start and the latest matching end, and never correlates marks by thread.
- Marks recorded before a transaction exists (DNS and connect happen in OkHttp's `ConnectInterceptor`, before network interceptors) travel in `req.marks`. After a redirect, the connection marks of the second hop belong to the second transaction.
- Unknown mark names are ignored by the host (additions do not change the protocol version).

## 10. Session files

A session file (`.trafficpolice`) is the captured stream plus host records, so opening it replays through the same decoder as a live connection.

```
 bytes 0..8   magic "TPSESS\0" + format version (0x01)
 bytes 8..    gzip stream of frames (§3 framing)
```

Frame types inside the gzip stream:

| Type | Content |
|---:|---|
| 1, 2 | Device frames exactly as received (§4, §5), except `hello`, which is kept in the source record |
| 16 | JSON `{"t":"session", "format":1, "created_wall_ms":…, "host":{"name":"traffic-police","version":…}, "filter":null}` — the first frame. `filter` is null in format 1 |
| 17 | JSON `{"t":"source", "source":N, "device":{"label":…, "serial":…}, "hello":{…the hello as received…}, "resumed":false}` — source N starts: a process the host connected to. `"resumed":true` means the host reconnected to the same process (same `instance`) and kept the source; the frames that follow continue it. The short form `{"t":"source", "source":N}` switches back to a source already started. Device frames belong to the source of the latest type-17 frame |
| 18 | JSON `{"t":"source_end", "source":N, "ts":…, "reason":"the app exited"}` — `reason` is text for people |
| 19 | JSON `{"t":"annotations", "pins":[{"source":N,"txn":T}], "markers":[{"at":…,"kind":"pause","label":…}], "notes":{}}` — written when a recording finishes; may repeat, and the last one wins. Marker kinds: `pause`, `resume`, `note` |
| 20 | JSON `{"t":"log", "info":{"device":…, "package":…, "uid":…, "processes":[[pid, name], …]}, "lines":[[ts, wall_ms, pid, tid, uid, level, buffer, tag, message], …]}` — the device's log (Logdawg, ARCHITECTURE.md 5.17), before the annotations: `info` in the first frame, then lines in batches of up to 2,000. `ts` is the device time of the requests, `uid` may be null, `level` is logcat's letter (`V` … `A`), `buffer` logcat's id. Readers skip a line that is not one; readers before type 20 skip the frames |

- Source numbers are the writing host's; a reader gives sources its own numbers (pins are mapped along).
- The log (type 20) comes from the UI's `e` menu: all of it, or with some requests the lines from the first one's start to the last one's end.
- A file saved from the UI's `e` menu, or by `traffic-police record --filter`, may hold only some requests: it keeps every source record and every frame that belongs to no request (clock, traffic, diagnostics), and only the chosen requests' frames.
- A reader keeps everything before a truncated gzip stream or frame, and says the recording was cut short (a crash, or a full disk).
- Nothing is redacted: session files hold exactly what was captured (ARCHITECTURE.md 9.2).
- Writers flush the gzip stream at least every 5 s during `traffic-police record`, so a crash loses little.
- HAR files are imported by a separate reader that synthesizes the same events for a `har` source; threads, stacks and failures come back only from the `_trafficPolice` fields traffic-police writes (ARCHITECTURE.md 5.10).

## 11. Conformance tests

- `testdata/protocol/v1/device/` holds device-to-host goldens written by the Java runtime (`ProtocolGoldenTest`, frozen clock): `<scenario>.frames` is every byte the host received on the connection, and `<scenario>.expected.json` is the encoder's reading of each frame, in order (`{"frame":"json","msg":{…}}`, or `{"frame":"body","seq","txn","dir","flags","ts","offset","len","crc32"}`).
- The Rust side checks them twice. `traffic-police-proto` (`tests/goldens.rs`) decodes every frame, compares it with the expected reading, and round-trips each JSON message through the typed model: every field the runtime sends must come back with the same value (absent, `null`, `false`, `0` and empty values count as defaults). Fields and message types named `future_*` stand for a newer runtime and must be ignored. `traffic-police-core` (`tests/goldens.rs`) runs the frames through the normalizer and the session store and pins a summary of the resulting transactions, diagnostics and markers as snapshots.
- Scenarios: handshake, replay and every control message (`rules_ack`, `config_ack`, `pong`, `replay`); GET with gzip JSON (timing marks, connection and TLS details); POST with a request body; chunked streaming response; body over the cap with `prog`; redirect (two hops, one call); failures (timeout with a cause, cancellation); `dropped` and `diag`; the largest body chunk (64 KiB); unknown fields and message types; protocol mismatch (`bye`); takeover (`bye replaced`); rules (`rule_rewrite`: a gzip response whose status, headers and decoded text a rule changes, with `delivered` and the `dir = 2` body, then a simulated failure).
- Not stored: the 16 MiB maximum frame (generated in both test suites instead: the largest legal frame decodes, one byte more is rejected).
- `testdata/protocol/v1/host/` holds host-to-device goldens written by Rust (`hello_ack`, `set_rules` with every matcher and action, `set_config`, `ping`, `bye`, each with its JSON). The Java test sends each to a live runtime and checks what it does: the configuration takes effect, `set_config` patches only the named fields, `ping` gets its `pong`, `set_rules` gets a `rules_ack` for its version, and `bye` closes the connection.
- Regenerating goldens is an explicit task (`./gradlew :capture-core:updateProtocolGoldens`, `cargo test -p traffic-police-proto --test goldens -- --ignored update_goldens`), never a side effect.
- Fuzzing: arbitrary bytes into the Rust frame decoder never panic, never allocate more than the 16 MiB limit, and always end in a frame, a clean "need more bytes", or an error.

---

## Appendix A: adb services used by the host

Verified against `platform/packages/modules/adb` (Android 17), older `system/core/adb` tags, Android Studio's `adblib`, and read-only probes of a local adb server (platform-tools 37.0.0, server version 41). Details and citations: `research/04-adb-protocol.md`.

**Framing.** Each request is `<hex4><payload>` (lowercase `%04x` byte length, payload ≤ 65,535 bytes). The reply status is `OKAY`, or `FAIL<hex4><message>`; the message may be multi-line or empty. One request per TCP connection at a time: bytes after a request are treated as part of it (no pipelining). A malformed request can get no reply at all, so every request has a timeout.

**Addressing a device.** Always by transport id, which is unique within one server process (a replugged device gets a new id; ids restart at 1 when the server restarts, so all cached ids are dropped when the device tracker reconnects).

| Purpose | Request | Reply |
|---|---|---|
| Server version | `host:version` | `OKAY` `0004` `0029` (41) |
| Server features | `host:host-features` | `OKAY <hex4>` comma list (e.g. `devicetracker_proto_format`, `track_app`, `server_status`) |
| Device features | `host-transport-id:<id>:features` | `OKAY <hex4>` comma list; fails unless the device is online |
| Track devices | `host:track-devices-proto-binary` if the server has `devicetracker_proto_format`, else `host:track-devices-l` | `OKAY`, then forever `<hex4><Devices proto>` or `<hex4><lines>`, first message immediately, full list on every change (duplicates possible). Never write to this socket: any byte closes it |
| Switch to a device | `host:transport-id:<id>` (or `host:tport:serial:<s>`, which also returns an 8-byte little-endian id) | `OKAY`; the same connection then carries one device service |
| Device service | `<service>` after the switch | `OKAY` then a raw stream, or `FAIL…closed` (service unknown or target refused), or `FAIL…device offline…` |

**Device services.**

| Purpose | Service | Stream format |
|---|---|---|
| Shell with exit code | `shell,v2,raw:<command>` (`shell_v2`, API 24+). The command runs under `/system/bin/sh -c`; arguments are single-quoted with `'` written as `'\''` | Packets `[u8 id][u32 LE length][payload]`: 1 stdout, 2 stderr, 3 exit (payload[0] = code, 128+signal). Send `04 00000000` (close stdin) first. EOF without an exit packet means the device or transport went away |
| Debuggable processes (Android 12+, device feature `track_app`) | `track-app` | `<hex4><AppProcesses proto>` per change: `pid`, `debuggable`, `profileable`, `architecture` (e.g. `arm64`); with feature `app_info` (Android 15+ adbd) also `user_id`, `process_name`, `package_names`, `waiting_for_debugger`, `uid`. A new list is pushed when a process starts, updates, or dies. Never write |
| Debuggable processes (fallback) | `track-jdwp` | `<hex4>` + `pid\n` lines per change. Names come from `cat /proc/<pid>/cmdline` over shell |
| Push a file | `sync:` | `SEND le32(len) "<path>,<decimal mode>"`, then `DATA le32(n ≤ 65,536) <bytes>`…, then `DONE le32(mtime)`; reply `OKAY 00000000` or `FAIL le32(n) <msg>` (after a FAIL, drop the connection). `QUIT 00000000` ends the session. `STAT` checks existence (16 bytes, all zero when missing) |

**Attach mode's shell commands** (ARCHITECTURE.md §4.7.6), all over `shell,v2`:

| Purpose | Command |
|---|---|
| Debuggable, and `run-as` works | `run-as <package> true` (exit 0) |
| The process's ABI when `track-app` has none | `dumpsys package <package> \| grep primaryCpuAbi`, else `getprop ro.product.cpu.abi` |
| Install | the files pushed (sync) to `/data/local/tmp/traffic-police/<host pid>-<n>/`, then `run-as <package> sh -c '<script>'`, which copies them into `code_cache/traffic-police/` (and the agent into `code_cache/startup_agents/`), prints the data directory, and is followed by `rm -rf` of the staging directory |
| Attach at run time | `cmd activity attach-agent <pid> '<data>/code_cache/traffic-police/libtrafficpolice_agent.so=dir=<data>/code_cache/traffic-police;package=<package>'` (no output on success) |
| Start with the agent (`--launch`) | `cmd package resolve-activity --brief -c android.intent.category.LAUNCHER <package>`, then `am start -S -n <component>`, with `--attach-agent '<so>=<options>'` on API 27–29 |
| The startup agent's stamp | `date +%s` (the device's clock), then `agent.conf` rewritten through `run-as` |
| Why an agent did not start, or a process died | `logcat -d -v tag -t 2000 --pid=<pid>` (with `-b main -b crash` for a crash) |

**Forwarding to the capture runtime.**

```
→ <hex4>host-transport-id:<id>:forward:tcp:0;localabstract:traffic-police_<pkg>_<pid>
← OKAY OKAY <hex4><decimal port>        (listener bound on 127.0.0.1)
← FAIL0000                              (empty message: device not found or not online)
→ <hex4>host-transport-id:<id>:killforward:tcp:<port>      when done
← OKAY OKAY                             (or FAIL "listener … not found": already gone)
```

- The forward succeeds even if nothing listens on that abstract name; the TCP client then sees a successful connect followed by EOF. Only `hello` proves the runtime is there.
- Forwards disappear whenever the device goes offline (unplug, adbd restart, `adb root`, re-authorization) and when the server restarts. The host re-creates them on every transition back to `device`.
- The host removes only the ports it created. It never sends `killforward-all`, which removes every forward on the server, including Android Studio's.

**Discovery.** `shell,v2,raw:cat /proc/net/unix`; split each line on whitespace; keep rows with field 3 = `00010000` (listening) and field 7 starting with `@traffic-police_`; the pid is the digits after the last `_` (package names may contain `_`). Accepted connections inherit the listener's name, which is why the flags field matters.

**Server lifecycle.**

- If the server is not reachable on `127.0.0.1:5037` (or `ADB_SERVER_SOCKET` / `ANDROID_ADB_SERVER_PORT`), the host runs `adb start-server` and retries with backoff; a freshly started server may hold connections for about 3 s.
- When the server restarts, every connection gets EOF or a reset. One supervisor reconnects the device tracker, rebuilds the device table from its first message, then re-creates per-device trackers and forwards.
- The `adb` binary is used as a fallback only if its version matches the running server's (`adb version` vs `host:version`): the adb client kills and restarts any server whose version differs from its own, which would also break Android Studio's session.

## Appendix B: NDJSON output of `traffic-police tail`

`tail --json` prints one JSON object per line. `v` is the schema version (1). A line is printed when a transaction completes, fails, or is cut off by the end of its source (`"state":"detached"`), in that order, not in start order. Without `--json`, `tail` prints the same transactions as text for people (`07:10:01.123  200  GET  305 ms  225 B  https://…`); scripts should use `--json`.

```jsonc
{"v":1,"type":"txn","id":"0:7",
 "source":{"serial":"emulator-5554","package":"com.example.app","process":"com.example.app","pid":4312},
 "start":"2026-09-29T07:10:01.123+05:30","start_rel_ms":1234.5,"duration_ms":305.2,
 "method":"GET","url":"https://api.example.com/status?id=7","status":200,"state":"complete",
 "request":{"headers":[["Accept-Encoding","gzip"]],"body_bytes":0},
 "response":{"protocol":"h2","headers":[["content-type","application/json"]],"body_bytes":225,"decoded_bytes":225,"content_type":"application/json"},
 "timing":{"queued":0.4,"dns":-1,"connect":-1,"ssl":-1,"send":0.2,"wait":280.1,"receive":24.5},
 "thread":{"name":"DefaultDispatcher-worker-3","origin":"call"},
 "stack":["com.example.app.Api.status(Api.kt:42)","com.example.app.MainActivity.refresh(MainActivity.kt:88)"],
 "client":"okhttp/4.12.0","rules":[]}
```

- `start` is null when the source sent no wall clock; `status` and `response` are null when no response arrived. Timing phases that did not happen (or were not observed) are `-1`.
- `stack` holds the initiating stack, innermost first, as `class.method(file:line)`; `"stack_truncated":true` is added when the runtime cut it at its depth limit.
- A failed transaction adds `"failure":{"class":…,"message":…,"phase":…,"canceled":…,"simulated":…}`; `simulated` is true when a rule's `fail` action threw it.
- `rules` lists the ids of the rules that applied, each once. When they changed the response, `status` and `response` are what the app received, and `original` adds the response as the network gave it, in the same form plus its `status`.
- `--bodies` adds `request.body` and `response.body` as `{"text": …}` for UTF-8 text or `{"base64": …}` otherwise (after Content-Encoding decoding).
- A gRPC call adds `"grpc":{"code":5,"status":"NOT_FOUND","message":…}` and `"trailers":[[name, value], …]` (`done`, §7.1).
- A WebSocket (status 101) adds `"websocket":{"sent":2,"sent_bytes":60,"received":6,"received_bytes":374}`, and its line is printed when the socket closes. `--bodies` adds `messages`, each `{"dir":"out"|"in","op":"text"|"binary"|"close","at_ms":…,"size":…}` with `text` or `base64`, or `code` and `reason` for a close, and `"truncated":true` when the capture cut it. `at_ms` counts from the transaction's start.

`tail --events` prints the captured stream instead, one line per message as the app sent it (the filter does not apply):

```jsonc
{"v":1,"type":"source","source":0,"device":{"label":"Pixel 8 [emulator-5554]","serial":"emulator-5554"},"resumed":false,"hello":{"t":"hello",…}}
{"v":1,"type":"event","source":0,"msg":{"t":"req","seq":12,"ts":5800000000000,"txn":7,…}}
{"v":1,"type":"body","source":0,"txn":7,"dir":"response","seq":14,"ts":5800123000000,"offset":0,"len":225}
{"v":1,"type":"source_end","source":0,"ts":5809000000000,"reason":"the app exited"}
```

`msg` is a device message (§7) exactly as received, `pong` included. Body lines carry sizes; with `--bodies` they add `"base64"` with the chunk's bytes (as captured, still Content-Encoded). `source` numbers are the host's and match the `id` prefix of `txn` lines.
