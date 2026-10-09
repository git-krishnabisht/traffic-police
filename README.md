# traffic-police

Watch an Android app's HTTP and HTTPS traffic from your terminal, the way Android Studio's
Network Inspector shows it: a live traffic graph, a connection list with a timeline, full request
and response details, the thread and call stack that made each request, a thread view, and
response rewrite rules. No proxy and no certificates: a small runtime inside the (debuggable)
app hooks OkHttp, HttpURLConnection and gRPC and streams events to the terminal over adb. A
Flutter app's own Dart traffic is read from its Dart VM service.

> **Status: Phase 5.** Watch an app's traffic live from a device or an emulator in one of three
> ways: add the library to its debug build ([library mode](#watch-your-app-library-mode)), attach
> to any debuggable build with no changes at all ([attach mode](#watch-any-debuggable-app-attach-mode)),
> or read a Flutter app's dart:io traffic from its Dart VM service ([flutter mode](#watch-a-flutter-app-flutter-mode)).
> WebSockets (every message) and gRPC calls (status, trailers, protobuf messages) are captured
> too. Then filter, search, copy as cURL, diff, decode tokens, export HAR, save and reopen
> sessions, or run without the UI (`tail`, `record`, `export`, `doctor`). [Rules](#rules) change
> what the app receives: delay or fail a request, or change a response's status, headers or
> body. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and
> [docs/PROTOCOL.md](docs/PROTOCOL.md) for the device protocol.



## Install

**Download** the binary for your system from the
[releases page](https://github.com/git-krishnabisht/traffic-police/releases): macOS on Apple
silicon, Linux on x86_64 (glibc 2.34 or newer: RHEL 9, Ubuntu 22.04, Debian 12, Fedora 35 and
later), or Windows on x86_64. It has the [attach mode](#watch-any-debuggable-app-attach-mode) agent
built in. The binaries are not code-signed:

```sh
chmod +x traffic-police-macos-arm64                          # macOS and Linux: make it executable
xattr -d com.apple.quarantine traffic-police-macos-arm64     # macOS: allow a downloaded file from an unidentified developer
mv traffic-police-macos-arm64 /usr/local/bin/traffic-police  # or anywhere on your PATH
```

Windows may warn that the publisher is unknown. `SHA256SUMS.txt` on the releases page has the
checksums.

**Or build from source.** You need Rust through rustup; the toolchain is pinned in
`host/rust-toolchain.toml` (1.98.1).

```sh
# once, if you do not have Rust yet; then open a new terminal (or run: source "$HOME/.cargo/env")
# so the shell can find cargo, otherwise zsh says "command not found: cargo"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# if cargo says the pinned toolchain is missing
rustup toolchain install 1.98.1 --component clippy rustfmt

cd host
cargo build --release
./target/release/traffic-police --help
```

For [attach mode](#watch-any-debuggable-app-attach-mode) build the agent too. It needs JDK 17 or
newer and the Android SDK with the NDK 28.2 and CMake 3.22.1 (Android Studio's SDK Manager, SDK
Tools tab):

```sh
cd android
./gradlew :attach-agent:agentArtifacts        # into android/attach-agent/build/outputs/agent
cd ../host
TRAFFIC_POLICE_EMBED_AGENT=1 cargo build --release   # optional: the agent inside the binary, so it works anywhere
```

A binary built without `TRAFFIC_POLICE_EMBED_AGENT` finds the agent in the source tree it was
built from (or give `--agent-dir`).

Works on macOS, Linux and Windows Terminal, including inside tmux and over SSH. The terminal
must be at least 100 columns by 30 rows. For devices you also need adb (Android SDK
platform-tools); traffic-police talks to the adb server and starts it if it is not running.

## Try it

```sh
traffic-police demo
```

A pretend shop app (`com.example.shop`, a debug build talking to a dev server on
`http://localhost:8080` and to example.com hosts) signs in, opens a session, lists products, adds
one to the cart and checks out, then polls the order's status every 1.5 s until a rule marks the
payment captured. Around that: telemetry, a notifications poll with gzip JSON, an avatar image, a
redirect, a 404, a 500 HTML page, a timeout, a cancelled call, a multipart review upload, a
protobuf body and a 5 MB download, on several threads. Nothing leaves your machine: the traffic
is made up.

| Option | What it does |
|---|---|
| `--seed N` | Different traffic (the same seed always gives the same traffic) |
| `--speed X` | Run simulated time faster, e.g. `--speed 3` |
| `--restart-after SECS` | Make the app die and restart, to see DETACHED and re-attach markers |
| `--theme dark\|light\|auto` | Color theme (`auto` reads `COLORFGBG` if your terminal sets it) |
| `--no-images` | Draw images with half-blocks instead of terminal graphics |
| `--log-file PATH` | Write the log somewhere else |

## Watch your app (library mode)

A small runtime goes into your app's debug build; release builds get a no-op stand-in.

**1. Build the library into your local Maven repository** (it is not on a public repository
yet). You need JDK 17 or newer and the Android SDK (`ANDROID_HOME`, or `sdk.dir` in
`android/local.properties`):

```sh
cd android
./gradlew publishToMavenLocal     # io.trafficpolice:capture, capture-noop and capture-core 0.1.0 into ~/.m2
```

**2. Add it to the app.** In `settings.gradle.kts`:

```kotlin
dependencyResolutionManagement {
    repositories {
        mavenLocal()   // until traffic-police is published
        google()
        mavenCentral()
    }
}
```

and in the app module's `build.gradle.kts`:

```kotlin
dependencies {
    debugImplementation("io.trafficpolice:capture:0.1.0")
    releaseImplementation("io.trafficpolice:capture-noop:0.1.0")
}
```

**3. Hook your HTTP clients.** OkHttp (and whatever uses your `OkHttpClient`: Retrofit, Coil,
Glide's OkHttp integration, ...):

```kotlin
val client = OkHttpClient.Builder()
    .addNetworkInterceptor(TrafficPolice.networkInterceptor())
    .eventListenerFactory(TrafficPolice.eventListenerFactory())   // or eventListenerFactory(yourFactory): yours keeps working
    .build()
```

HttpURLConnection:

```kotlin
val connection = TrafficPolice.wrap(url.openConnection() as HttpURLConnection)
```

gRPC (grpc-java 1.21 and newer, any transport): add the interceptor to each channel, first, so
it sees what your other interceptors add:

```kotlin
val channel = OkHttpChannelBuilder.forAddress(host, port)   // or ManagedChannelBuilder, AndroidChannelBuilder, Grpc.newChannelBuilder
    .intercept(TrafficPolice.grpcInterceptor())
    .build()
```

Each call is one row (`POST https://host/package.Service/Method`, type `grpc`): its messages as
the bodies (decoded as protobuf, one message after another), its status in the Status column
(`OK`, `NOT_FOUND`, …), and its trailers at the end of the Response tab. `grpc:not_found` or
`grpc:error` filters by status.

WebSockets (OkHttp sends a socket's handshake past interceptors, so the socket is opened through
traffic-police; you get the handshake and every message, both ways):

```kotlin
val socket = TrafficPolice.newWebSocket(client, request, listener)   // in place of client.newWebSocket(request, listener)
```

Capture starts by itself in the app's main process. Apps with more processes
(`android:process=":sync"`) call `TrafficPolice.start(context)` in `Application.onCreate` to
capture those too. Requirements: OkHttp 3.9 or newer (older versions are left alone and
reported) and a debuggable build. The library builds for Android 5.0 (API 21) and newer and is
tested on Android 8.0 (API 26) to 17 (API 37). It is plain Java with no dependencies of its own;
in a release build `capture-noop` adds a few small pass-through classes and nothing starts.

**4. Run traffic-police:**

```sh
traffic-police                                        # choose the device, then the app process
traffic-police -p com.example.app                     # the app's main process, on the only device
traffic-police -s emulator-5554 -p com.example.app --launch      # start the app first
traffic-police -p com.example.app --process com.example.app:sync # another process
traffic-police -p com.example.app --follow            # keep watching across app restarts
```

The header shows the state: **LIVE**; **PAUSED** (`Space`: the app stops recording new
requests); **WAITING** (for the device, for the app to start, or for Android to unfreeze a
cached app); **DETACHED** (the app exited; everything captured stays on screen). With
`--follow`, a restarted app continues as a new segment of the same timeline. Requests the app
made before traffic-police connected are shown too (the runtime keeps the last 1,000). One
traffic-police watches a process at a time: a second one takes over, and the first shows
DETACHED.

**What you see:** every OkHttp request that reaches the network, with the headers as sent on
the wire (including the ones OkHttp adds), bodies as transferred (gzip, brotli and zstd are
decoded for display), one row per network attempt (a redirect is two rows, the second marked
`↪`), the timing phases, and the thread and call stack that made the call; plus the
HttpURLConnection connections you wrap, and the WebSockets you open with
`TrafficPolice.newWebSocket` (a `ws` row: the Response tab lists every message, and Enter opens
one). **Not captured in library mode:** responses OkHttp serves from its cache (they never reach
the network), other HTTP stacks (Cronet, Ktor engines other than OkHttp, ...), gRPC channels
without the interceptor, and the HttpURLConnection connections and WebSockets you do not open
through traffic-police.
[Attach mode](#watch-any-debuggable-app-attach-mode) hooks every HttpURLConnection and every
OkHttp WebSocket instead.

**The sample app** in `android/sample-app` exercises every capture path (Retrofit suspend calls,
OkHttp `execute` and `enqueue`, HttpURLConnection, streaming, a 5 MB download, a multipart upload,
a redirect, errors, a timeout, a cancelled call, HTTPS, an unknown host, a WebSocket, gRPC calls
to a gRPC server in the app, a second process)
against servers inside the app, so it needs no network:

```sh
cd android && ./gradlew :sample-app:assembleDebug
adb -s <serial> install -r sample-app/build/outputs/apk/debug/sample-app-debug.apk
traffic-police -s <serial> -p io.trafficpolice.sample --launch
adb -s <serial> shell am start -n io.trafficpolice.sample/.MainActivity --es run all
```

Other `--es run` values: `poll` (a status poll every 1.5 s), `overhead` (measures what capture
costs per request, in logcat), `security` (checks that another uid is refused).

## Watch any debuggable app (attach mode)

No library and no code changes: traffic-police puts a small agent into the running app (a JVMTI
agent, the way Android Studio's inspectors get in) and captures what library mode captures,
including the requests of OkHttp clients the app built before you attached. It works with any
debuggable build (a debug build, or `android:debuggable`) on Android 8.0 (API 26) or newer.

```sh
traffic-police                                             # pick a process: ○ ones have no library, Enter attaches
traffic-police --mode attach -p com.example.app            # attach to the running app
traffic-police --mode attach -p com.example.app --launch   # restart it with the agent: start-up requests too
traffic-police --mode attach -p com.example.app --follow   # and keep watching across restarts
traffic-police tail --mode attach -p com.example.app       # the commands without the UI too
```

- **Start-up requests.** `--launch` stops the app if it runs and starts it with the agent
  loading first, so the requests it makes while starting are captured: on Android 11 and newer
  through a startup agent, on Android 10 through `am start --attach-agent` (best effort on
  Android 8.1 and 9). Android 8.0 cannot load an agent at start: traffic-police attaches right
  after, and the app's first requests may be missed. With `--follow`, a restarted app is attached
  again; on Android 11 and newer, from its start.
- **What changes in the app.** The agent and two small dex files go into the app's
  `code_cache/traffic-police` (through `run-as`, which works only for debuggable apps). With
  `--launch` or `--follow` on Android 11 and newer, a copy also sits in
  `code_cache/startup_agents` while traffic-police runs, and is removed when it quits; if
  traffic-police is killed, that copy does nothing after 5 minutes, and `doctor` finds it. The
  agent stays in the app's process until the process exits. After traffic-police quits it keeps
  a replay buffer, like the library; rules stop applying.
- **What it captures:** OkHttp 3.9 and newer and everything built on it (Retrofit, Coil, ...),
  its WebSockets, every HttpURLConnection, with no wrapping, and gRPC (grpc-java 1.10 and newer):
  every channel built after the attach, and the calls generated stubs make on channels built
  before it (`--launch` catches them all). Not captured: an OkHttp whose names a minified
  (R8) debug build changed (`doctor` shows each hook's status), and a second OkHttp copy in
  another class loader.
- **Checks:** `traffic-police doctor --mode attach -p com.example.app` checks the agent, the
  app's ABI (arm64-v8a, armeabi-v7a and x86_64 are supported), `run-as` and `code_cache`, what
  `--launch` can do on the device, startup agents left behind, and the hooks of an agent that
  runs.
- **The sample app's plain build** has no traffic-police code:

  ```sh
  cd android && ./gradlew :sample-app:assemblePlain :attach-agent:agentArtifacts
  adb -s <serial> install -r sample-app/build/outputs/apk/plain/sample-app-plain.apk
  traffic-police -s <serial> --mode attach -p io.trafficpolice.sample.plain --launch
  adb -s <serial> shell am start -n io.trafficpolice.sample.plain/io.trafficpolice.sample.MainActivity --es run all
  ```

## Watch a Flutter app (flutter mode)

A Flutter app's own HTTP traffic (dart:io: package:http, dio, `NetworkImage`, …) does not go
through OkHttp or HttpURLConnection, so library and attach mode do not see it. Flutter mode reads
it from the app's Dart VM service, the way DevTools' Network page does: no library, no agent, any
debug or profile build of Flutter 3.22 (Dart 3.4) or newer.

```sh
traffic-police --mode flutter -p com.example.app            # the running app
traffic-police --mode flutter -p com.example.app --launch   # start it first
traffic-police tail --mode flutter -p com.example.app --json
```

- **How it connects.** The app logs its VM service address when it starts; traffic-police reads
  it from logcat, forwards the port, and turns dart:io's HTTP logging on. If the app started long
  ago and the line has left the log, restart the app (or use `--launch`). An app that a Flutter
  tool runs (`flutter run`, an IDE) works too: traffic-police connects through the tool's DDS.
- **What you see.** A request appears once dart:io has sent it, with its body when it ends;
  bodies are as the app read them (decompressed, while the headers still say `gzip`). Each row's
  thread is its isolate. dart:io records only after it is asked to, so requests made before
  traffic-police connected are missing; it keeps what it recorded in the app's memory, and a
  second session shows those again. When traffic-police quits, logging is turned off again
  where it was off.
- **Pause** (`Space`) turns dart:io's recording off: requests made meanwhile are not recorded.
  **No rules:** the VM service cannot change a response.
- **Not captured:** gRPC and dio's HTTP/2 adapter (they use sockets directly), the call stack
  (Dart does not report it), TLS details, the order of headers with different names (dart:io
  keeps them in a hash map), and anything on the Java side: watch that with library or attach
  mode.

## Keys

| | Keys |
|---|---|
| Move | `↑` `↓` or `j` `k` · `g` `G` top and bottom · `PgUp` `PgDn` · `Ctrl+D` `Ctrl+U` (or `Ctrl+P`) half a page, the view and the cursor together as in Neovim (`[ui] scroll` sets how far) · `Tab` `Shift+Tab` move focus between graph, list and detail |
| Views | `1` Connection View · `2` Thread View · `3` Rules |
| Rules | `r` on a request: a new rule that matches it, in the rule form · in the Rules view: `Space` on or off · `Enter` edit in the form · `r` (or `a`) a new rule · `K` `J` move the rule up or down (rules apply in order) · `E` edit `rules.toml` in `$EDITOR` |
| Rule form | `↑` `↓` (or `Tab`) between lines · `Enter` types into a field (`Enter` keeps it, `Esc` puts back what was there), turns a yes/no line, or steps a choice · `←` `→` step a choice (`Space` ticks a method) · `a` adds an action · `Delete` or `Backspace` removes the action or query parameter · `K` `J` move an action · `Ctrl+S` saves · `Esc` leaves (asking when something changed) · `E` `$EDITOR` · the last line lists the captured requests the rule's match selects |
| Detail pane | `Enter` open · `Esc` close · `h` `l` or `←` `→` switch tabs · `p` parsed or source · `o` original or rule-modified response |
| Body explorer | The box above the tabs shows the response body and, on its second tab, the request body; `Shift+Tab` from the tabs (or `Tab` from the list, or a click) goes there: `h` `l` (or `←` `→`, `b`, a click on a tab) switch between the two bodies, as in the tabs below · `j` `k` move · `Ctrl+D` `Ctrl+U` half a page · `Enter` fold or unfold, or the value menu on a single value · `[` `]` fold or unfold all · `B` hides the box (the tabs take its room) and shows it again; `[ui] body_box = false` starts without it |
| Bodies | `Enter` fold or unfold JSON · `[` fold all · `]` unfold all · <code>&#124;</code> jq filter (empty filter clears) · long lines wrap in every box; with `[ui] wrap = false` they are cut and `<` `>` scroll sideways |
| Call Stack | `Enter` on a framework group expands it · `Enter` on an app frame opens `$EDITOR` there (needs source roots, below) |
| Live | `Space` pause or resume recording · `F` freeze the view (capture continues) · `L` back to live · `+` `-` zoom · `0` reset zoom · `v` select a time range (`v` or `Enter` again to apply) |
| Graph | Receiving above the zero line, sending below, each half scaled to its own peak (`:` layout switches to one shared scale; `:` style switches between solid areas, braille curves and step lines) · `T` whole-app traffic or captured requests · `t` time since start or wall clock · `←` `→` move when the graph has focus |
| List | New requests are followed while the cursor is on the newest; `G` (or `End`, `Ctrl+G`) goes back to it and follows again, also with a request open · `c` collapse repeated calls · `s` sort by the next column · `S` reverse the sort · `C` choose columns |
| Find | `/` in the list: the filter bar ([language below](#filters)) · `/` in the detail pane: search the tab (`n` `N` next and previous match) · `m` pin a request (`is:pinned` lists pins) |
| Copy and save | `y` copy: as cURL, the URL, headers, one header, a body, the JSON value at the cursor · `w` save a body to a file · `e` export: HAR (all, listed or selected requests) or a session file |
| Compare | `d` marks a request (◆), `d` on another compares them: request and status lines, headers (`s` in order or as sets), bodies (JSON with keys sorted); `n` `N` step through changes, `y` copies the diff |
| Decode | `Enter` on a header or a JSON value: copy it, decode a JWT, base64 or URL encoding, or filter by it · the Overview lists JWTs with their expiry (`Enter` decodes) |
| Session | `:` command palette (every command by name, and the graph styles and layouts) · `x` clear (asks first) · `?` help · `:q` Enter quits, as in Neovim (`:q!`, `:qa`, `:wq` and `:x` too, also in the device and app pickers); `q` and Ctrl+C do not quit, they say how (`quit = ["q"]` under `[keymap]` brings `q` back) |

Mouse: click rows and tabs, double-click a row to open it, wheel to scroll (on the graph the
wheel zooms and Shift+wheel moves in time), drag on the graph to select a range, drag the divider
to resize the panes.

Every key can be changed in the config file (below), also in menus, the palette and the pickers,
and so can the layout, the colors and the borders.

### Filters

Words must all match; `-` in front of one negates it; quotes keep spaces in a word.

| Filter | Matches |
|---|---|
| `login`, `"api/v2 users"` | part of the URL (any case) |
| `/regex/`, `/regex/i` | a regular expression on the URL |
| `method:GET,POST` | the method |
| `status:404`, `status:4xx`, `status:>=400`, `status:failed`, `status:pending` | the status, its class, or the state |
| `grpc:not_found`, `grpc:5`, `grpc:error` | a gRPC call's status, by name or code; `error` is any but OK |
| `host:*.example.com`, `path:/api/**/status` | the host or path (`*` within a segment, `**` across) |
| `type:json`, `thread:worker`, `header:x-request-id`, `header:"x-request-id=abc"` | type, initiating thread, a header by name or value |
| `size>10k`, `time>500ms` | response size, duration |
| `rule:modified`, `rule:<id>`, `is:pinned` | changed by a rule, pinned |
| `body:"sessionId"` | a request or response body contains the text (searched in the background) |

A filter that does not parse is underlined where it breaks, and the previous one stays active.

## Without the UI

```sh
traffic-police tail -p com.example.app                        # a line per finished request
traffic-police tail -p com.example.app --json status:5xx | jq .url   # JSON lines (docs/PROTOCOL.md Appendix B)
traffic-police tail -p com.example.app --events               # every captured message
traffic-police record -p com.example.app --out login.trafficpolice --duration 2m
traffic-police export --har out.har --input login.trafficpolice --filter 'host:api.*'
traffic-police export --har out.har -p com.example.app --duration 60s
traffic-police open login.trafficpolice                       # or a .har from any tool
traffic-police doctor -p com.example.app                      # what is wrong, and how to fix it
traffic-police tail --mode attach -p com.example.app --launch # attach mode, from the app's start
```

`tail` runs until Ctrl+C, `--duration`, or the app's exit (`--follow` waits for it to start
again); `--bodies` adds bodies to `--json` lines. `record` writes the file as it captures (with
`--filter`, only matching requests, at the end). Like the UI, both begin with the requests the
app made before they connected (the app keeps its last 1,000). Status goes to stderr, data to
stdout; a capture that fails (the app cannot be attached to, say) exits with 1. `doctor` checks
adb, the config, the terminal and clipboard, the project's `rules.toml`, each device, and with
`--package` that the app is installed, debuggable and capturing (with `--mode attach`, what
attach mode needs); it changes nothing and exits with 1 when a check fails.

Session files keep everything as captured (timings, threads, stacks, bodies, pins), so `open`
shows a session as it was. HAR files from browsers and other tools open too; their bodies are
shown decoded, and the transferred (compressed) sizes are not kept.

## What you see is what was sent

traffic-police hides nothing: headers such as `Authorization` and `Cookie`, tokens and personal
data in bodies all appear exactly as the app sent and received them, on screen, in copies, and in
HAR, session and `tail` output. Treat screenshots and exported files like the app's own logs.

## Config

Settings go in `~/.config/traffic-police/config.toml` (`$XDG_CONFIG_HOME/traffic-police` if set;
`%APPDATA%\traffic-police` on Windows; or the file `TRAFFIC_POLICE_CONFIG` names). All of it is
optional; `traffic-police doctor` reports mistakes with their line.

```toml
[ui]
theme = "dark"                # auto, dark, light (--theme wins)
borders = "rounded"           # rounded, plain, double, thick
graph_style = "smooth"        # smooth, curves, heavy, lines, braille
graph_layout = "mirror"       # mirror: receiving above the zero line, sending below, each at its own scale; overlay: both above, one scale
graph_smoothing = 1.0         # seconds the curves are averaged over (0.5 to 5); longer is calmer, and the live edge trails a little more
graph = "app"                 # the graph at start: app (all app traffic) or requests (T switches)
graph_height = 12            # rows of the graph, 0 hides it (by default a quarter of the screen, 8-14)
time = "wall"                 # relative (since the session started) or wall (clock time)
columns = ["method", "host"]  # optional columns shown besides the default ones
sort = "status desc"          # the list's order at start: a column's name, desc for the reverse
collapse = false              # start with repeated calls collapsed
view = "connections"          # the view at start: connections, threads, rules
divider = 55                  # the list's share of the width, in percent (25-80)
side_by_side = 140            # from this width on, the detail pane sits beside the list (100-500)
tab = "overview"              # the tab a request opens on: overview, response, request, call-stack
body_box = true               # the body box above the tabs, with the response and request bodies; false hides it (B shows or hides it)
body = "response"             # the body box's tab at start: response or request (b switches)
body_height = 40              # the body box's share of the detail pane, percent (15-85)
wrap = true                   # wrap long lines in every box (false: cut them; < > scroll the tabs)
scroll = 0                    # lines Ctrl+D and Ctrl+U move: 0 is half the box, as in Neovim
follow = true                 # follow new requests while the cursor is on the newest
hints = true                  # the key hints in the footer
gap = 0                       # blank rows between boxes; side by side they get 2 × gap + 1 columns
clipboard = "auto"            # auto, osc52, native, off
images = true                 # false: half-blocks (as --no-images)
fps = 60                      # frames drawn a second at most (10-240); a still screen draws none

[colors]                      # any color as "#rrggbb", with both palettes
accent = "#61afef"
[colors.dark]                 # only with the dark palette ([colors.light]: the light one)
selection = "#2a4a7f"

[capture]
body_cap = "10mb"             # bytes kept of each body
stack_depth = 64
request_bodies = true
response_bodies = true

[keymap]
pause = "p"                   # an action's name (see : or ?), then a key or a list of keys
copy = ["y", "ctrl+y"]

[adb]
server = "127.0.0.1:5037"     # otherwise ADB_SERVER_SOCKET / ANDROID_ADB_SERVER_PORT, as adb reads them
path = "/opt/android-sdk/platform-tools/adb"   # starts the server when none runs; doctor compares versions with it

[storage]
memory = "256mb"              # body bytes kept in memory before the rest goes to disk
spill_dir = "/var/tmp"        # where that goes (a private directory, removed on exit)
```

`gap` is the space between boxes. A terminal cell is about twice as tall as it is wide, so boxes
side by side get twice as many blank columns as boxes one above the other get rows, and one more:
with `gap = 0` the borders above each other are on neighbouring rows and the borders side by side
one blank column apart, which looks the same; `gap = 1` gives one blank row and three columns.

`sort` takes a column's name as the list's header shows it, in lowercase with `-` for spaces
(`status`, `size`, `time`, `req-size`); `timeline` is the order the requests started in. `time`
sorts by how long a request took.

The colors `[colors]` can set, by name:

| Name | Paints |
|---|---|
| `text` `dim` `faint` | text; labels and secondary text; hints and the least important text |
| `accent` | the focused box, links, highlights |
| `selection` | the selected row's background |
| `border` | the borders of boxes without focus (the focused one takes `accent`) |
| `receiving` `sending` `waiting` | received and sent bytes (the graph, timing bars); waiting for the server |
| `ok` `redirect` `client-error` `server-error` | 2xx; 1xx and 3xx; 4xx and warnings; 5xx, failures and errors |
| `marker` | timeline markers |
| `key` `string` `number` `keyword` | JSON keys, form fields and header names; strings; numbers; `true` `false` `null` |
| `tag` `attribute` | XML and HTML tags and attributes |
| `graph-selection` | the selected range on the graph |
| `background` | your terminal's background. Not a color traffic-police paints: set it only if the graph's sending half or the timeline bars' left ends show boxes, which means your font lacks the upper- and right-eighth blocks (Menlo and SF Mono do; Cascadia Code, the Nerd Fonts and Iosevka have them). With it, they are drawn with the blocks every font has |
| `search-match` `search-current` `search-current-text` | search matches, the current one, and its text |

On a terminal with 256 or 16 colors each color is drawn as the nearest one it has.

`fps` is the most frames traffic-police draws in a second: 60 unless you set it, from 10 to 240.
A live view is drawn that often, because the clock moves the graph and the bars. A still one,
such as an opened file, is not drawn at all. How many of the frames you see is up to the
terminal and the screen: a 60 Hz screen shows 60 a second whatever is drawn, and the rest only
costs CPU. To see what is drawn, press `:`, type `fps` and press `Enter` (or start with
`TRAFFIC_POLICE_FPS=1`): the footer then shows the frames drawn in the last second and the time
one took.

## Project config

Put a `.traffic-police/project.toml` in your project; traffic-police uses the nearest one at or
above the directory you start it from.

```toml
# the app to watch when no --package is given (the picker is skipped)
package = "com.example.app"
# paths are relative to the directory that contains .traffic-police/
source_roots = ["app/src/main/java", "app/src/main/kotlin", "sdk/src/main/kotlin"]
```

`package` is the app `traffic-police` (and `tail`, `record`, `export` and `doctor`) watches when
no `--package` is given. `source_roots` lets `Enter` on a Call Stack frame open the file at that
line in `$VISUAL` or `$EDITOR` (VS Code, Cursor, Zed, Sublime Text and Helix get `file:line`;
others `+line file`).

## Rules

Rules change what the app receives, with the server left alone: slow a request down, make it
fail, or change a response's status, headers or body. They live in `.traffic-police/rules.toml`,
next to `project.toml` (the nearest one at or above the directory you start from, or
`--project DIR`):

```toml
version = 1

[[rule]]
id = "force-paid"
name = "Force payment captured"

  [rule.match]
  host = "*.example.com"             # a glob: * stays between dots, ** crosses them
  path = "/api/v1/orders/status"

  [[rule.action]]
  type = "replace"                   # in the body's text; gzip and deflate are handled
  find = '"payment":"pending"'
  with = '"payment":"captured"'

[[rule]]
id = "checkout-down"
enabled = false

  [rule.match]
  methods = ["POST"]
  path = "/api/v1/checkout"

  [[rule.action]]
  type = "fail"
  exception = "timeout"              # timeout, io, protocol, unknown_host or connect
```

The actions are `delay` (`ms`), `fail` (`exception`), `status` (`code`, `reason`), `header`
(`op` add, set or remove), `body` (`text`, `base64`, or a `file` in `.traffic-police/`) and
`replace` (`find`, `with`, `regex`). [docs/PROTOCOL.md §8](docs/PROTOCOL.md#8-rules) has every
field, and exactly what each action does with OkHttp and HttpURLConnection.

- **Make one:** select a request and press `r`. The rule form opens with a rule that matches it
  exactly (method, scheme, host, port, path and its query parameters); add what it does with
  `a` (delay, fail, status, header, body or replace), set the values, and save with `Ctrl+S`.
  In the Rules view, `r` starts an empty one. (With no `.traffic-police` directory yet, saving
  creates one in the directory you started from.)
- **Change them:** the Rules view (`3`) lists them with how often each applied. `Space` turns
  the selected rule on or off, `K` and `J` move it (rules apply in order), `Enter` opens it in
  the form, `E` in `$EDITOR`. The form checks every field as you change it, as the file is
  checked, and says what is wrong next to it; a rule with problems is not saved. Its last line
  lists the captured requests the match selects, so a glob or a regex can be tried before it is
  saved. Saving changes only that rule's values: comments and the rest of the file stay as they
  are. Saved changes, to `rules.toml` or a body file, from the form or an editor, reach the app at
  once. A mistake in the file is reported with its line, and the rules that were active stay
  active. The footer says how many rules the app runs.
- **See what they did:** the detail pane shows the response the app received; `o` shows the
  original. `rule:modified` and `rule:<id>` filter for the requests rules changed. `tail --json`,
  HAR and session files keep both.
- Rules apply only while traffic-police is connected; when it quits, the app behaves normally
  again. They keep applying while recording is paused. A changed response gets
  `Cache-Control: no-store`, so OkHttp's cache does not keep it after the rule is off
  (`cache_rewrites = true` on a rule allows caching).
- `tail`, `record` and `export --live` apply the rules too, and saved changes reach the app while
  they run; stderr says when the file is read again, and what is wrong with it.
- The demo shows two built-in rules.

## Troubleshooting

- **Logs:** `~/.local/state/traffic-police/traffic-police.log` (or `$XDG_STATE_HOME`;
  `%LOCALAPPDATA%\traffic-police` on Windows). `TRAFFIC_POLICE_LOG=debug` logs more.
- **Images look blocky:** half-blocks are the fallback. Inside tmux they are the default,
  because probing for terminal graphics turns on tmux's `allow-passthrough`; set
  `TRAFFIC_POLICE_IMAGES=auto` to probe anyway.
- **No colors, or wrong colors:** `NO_COLOR` switches to a monochrome theme; set
  `COLORTERM=truecolor` if your terminal supports 24-bit color but does not advertise it.
- **"terminal too small":** make the window at least 100×30.
- **The UI lags in a slow terminal or over SSH, or uses more CPU than you like:** lower
  `[ui] fps` (10 is the least), so fewer frames are drawn and sent.
- **WAITING "waiting for com.example.app on ...":** the app is not running, or this build has no
  traffic-police library (then use `--mode attach`). On the device, `adb logcat -s TrafficPolice`
  shows `capturing in <process>; socket @traffic-police_...` when capture starts, or why it did
  not (`... is not debuggable`).
- **FAILED "... is not debuggable (run-as refused)":** attach mode works only with debuggable
  builds. A release build cannot be attached to, and neither can apps on a device where `run-as`
  is disabled.
- **FAILED "the agent did not start in ...":** the reason after the colon comes from the app's
  log (`adb logcat --pid=<pid>` shows all of it). An agent stays in a process until it exits, so
  restart the app (or use `--launch`) before trying again.
- **WAITING "the agent was sent to ... but has not opened its socket":** the agent loads on the
  app's main thread, so a busy or stopped main thread (a long task, or a debugger waiting)
  delays it; capture starts once it runs.
- **"attach mode needs the agent, and this build has none":** build it (see
  [Install](#install)), or give `--agent-dir`.
- **WAITING "... is frozen by Android":** Android 11 and later freeze apps cached in the
  background; they run no code, so capture cannot answer until the app runs again. Bring it to
  the foreground. On a test device you can turn freezing off in Developer options ("Suspend
  execution for cached apps"; the name varies between versions).
- **"2 devices connected; choose one with --serial":** use a serial from `adb devices`.
- **A device whose shell may not read `/proc/net/unix`:** traffic-police then finds the capture
  runtime by its socket name (from the package and the process id) instead; `doctor` says when it
  does.
- **"Android Studio's Network Inspector also watches this app":** both inspectors see every
  request, and Studio's rules may change a response before or after ours. Close Studio's Network
  Inspector to be sure what you see is the app's own traffic.
- **A device shows "unauthorized":** accept the USB debugging prompt on the device.
- **Android Studio is open too:** fine. traffic-police creates its own adb forwards and removes
  only those (and ones left behind by a traffic-police that was killed); Android Studio's are
  never touched.
- **DETACHED "another traffic-police took over":** a second traffic-police connected to the
  same process.
- **A rule does nothing:** the Rules view's footer says how many rules the app runs, and a rule
  the app refused shows why. Rules match the request as it is sent: the path is URL-encoded, and
  `*` in a host stops at dots (`*.example.com` does not match `example.com`).
- **The app crashes when a rule makes a request fail:** the app gets the exception a real
  network failure throws, so code that does not handle it would crash on a real failure too.
- **Anything else:** `traffic-police doctor -p com.example.app` checks adb, the config, the
  device, the app and its capture runtime, and prints a fix for each failure.
- **Copy does nothing inside tmux:** tmux passes OSC 52 on only with `set -g set-clipboard on`.
  `[ui] clipboard = "native"` uses pbcopy, wl-copy, xclip or xsel instead.

## Development

```sh
cd host
cargo test                                    # unit, behavior, conformance and fake-adb tests, 65 UI snapshots
INSTA_UPDATE=always cargo test -p traffic-police-tui   # after an intended UI change; review the snapshot diff
cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture   # frame time at 50,000 requests
cargo test --release -p traffic-police-core --test ingest -- --ignored --nocapture   # ingest speed
(cd fuzz && cargo +nightly fuzz run device_stream)   # fuzzing (cargo install cargo-fuzz; cargo fuzz list)
cargo run -- demo --dump-frame 140x40@12 --keys 'g<Enter>l'   # print one frame as text
TRAFFIC_POLICE_FPS=1 cargo run --release -- demo   # the footer shows the frames drawn a second and the time one takes
cargo run -- tail --demo --duration 5s --json   # the commands without the UI, on the demo app (hidden --demo)
# drives the sample app on a device (installed debug and plain builds, and the agent built): every
# scenario, both processes, kill and relaunch; in attach mode also --follow from the start and --launch
TP_E2E_SERIAL=emulator-5554 cargo test -p traffic-police-backends --test device_e2e -- --ignored --nocapture

cd android
./gradlew :capture-core:check                 # JVM tests against eight OkHttp versions, 3.9 to 5.5 (and the OkHttp 3.9 API check)
./gradlew :capture:connectedDebugAndroidTest  # the library's tests on a device or emulator (all connected ones; ANDROID_SERIAL picks one)
./gradlew :sample-app:assembleDebug           # the sample app (assembleNondebuggable: release-like, with capture; assemblePlain: no traffic-police code)
./gradlew :attach-agent:agentArtifacts        # the attach-mode agent (JVMTI .so per ABI, boot and runtime dex)
./gradlew :capture-core:benchmarkOverhead     # what capture costs per request on the JVM
./gradlew publishAllPublicationsToBuildRepository   # the artifacts in android/build/repo, not ~/.m2
```

Protocol golden files in `testdata/protocol/v1` are shared by both sides and rewritten only on
purpose: `./gradlew :capture-core:updateProtocolGoldens` (device side) and
`cargo test -p traffic-police-proto --test goldens -- --ignored update_goldens` (host side).

Nothing here sends data anywhere: no telemetry, and no network traffic from the host other than
adb.

Releases: tag `vX.Y.Z` with notes in `docs/releases/vX.Y.Z.md`, and push the tag; the release
workflow builds the binaries (agent built in), their checksums and `THIRD-PARTY-LICENSES.txt`
(`python3 host/scripts/third_party_licenses.py`), and publishes them. Run it by hand first to try
the build without publishing.

## License

traffic-police is licensed under the [Apache License, Version 2.0](LICENSE). The attach-mode agent
contains slicer from the Android Open Source Project (Apache-2.0) and uses the JVMTI header (GPL-2.0
with the Classpath exception); their texts are in `android/attach-agent/third_party`. Each release
lists the licenses of everything in its binaries in `THIRD-PARTY-LICENSES.txt`.
