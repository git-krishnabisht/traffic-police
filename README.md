# traffic-police

Watch an Android app's HTTP and HTTPS traffic from your terminal, the way Android Studio's
Network Inspector shows it: a live traffic graph, a connection list with a timeline, full request
and response details, the thread and call stack that made each request, a thread view, and
response rewrite rules. No proxy and no certificates: a small runtime inside the (debuggable)
app hooks OkHttp and HttpURLConnection and streams events to the terminal over adb.

> **Status: Phase 1.** Library mode works: add the library to your app's debug build and watch
> its traffic live from a device or an emulator. Attach mode (any debuggable app, no code
> changes) arrives in Phase 4; exports, filters and `doctor` in Phase 2; editable rules in
> Phase 3. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and
> [docs/PROTOCOL.md](docs/PROTOCOL.md) for the device protocol.

## Install

Build from source (release binaries come later). You need Rust through rustup; the toolchain
is pinned in `host/rust-toolchain.toml` (1.98.1).

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

Works on macOS, Linux and Windows Terminal, including inside tmux and over SSH. The terminal
must be at least 100 columns by 30 rows. For devices you also need adb (Android SDK
platform-tools); traffic-police talks to the adb server and starts it if it is not running.

## Try it

```sh
traffic-police demo
```

A pretend app runs an identity-verification SDK session (init, challenge, attest, enroll, then a
status poll every 1.5 s until a rule forces a verdict), plus telemetry, image loads, a redirect,
a 404, a 500, a timeout, a cancelled call, a multipart upload, a gzip JSON body, a protobuf body
and a 5 MB download, on several threads.

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

Capture starts by itself in the app's main process. Apps with more processes
(`android:process=":sync"`) call `TrafficPolice.start(context)` in `Application.onCreate` to
capture those too. Requirements: OkHttp 3.9 or newer (older versions are left alone and
reported) and a debuggable build. The library builds for Android 5.0 (API 21) and newer and is
tested on Android 8.0 (API 26) to 17 (API 37). It is plain Java with no dependencies of its own;
in a release build `capture-noop` adds four small pass-through classes and nothing starts.

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
HttpURLConnection connections you wrap. **Not captured in library mode:** responses OkHttp
serves from its cache (they never reach the network), WebSocket frames (Phase 5), other HTTP
stacks (Cronet, Ktor engines other than OkHttp, ...), and HttpURLConnection connections you do
not wrap. Attach mode will hook the platform instead of your clients.

**The sample app** in `android/sample-app` exercises every capture path (Retrofit suspend calls,
OkHttp `execute` and `enqueue`, HttpURLConnection, streaming, a 5 MB download, a multipart upload,
a redirect, errors, a timeout, a cancelled call, HTTPS, an unknown host, a second process)
against servers inside the app, so it needs no network:

```sh
cd android && ./gradlew :sample-app:assembleDebug
adb -s <serial> install -r sample-app/build/outputs/apk/debug/sample-app-debug.apk
traffic-police -s <serial> -p io.trafficpolice.sample --launch
adb -s <serial> shell am start -n io.trafficpolice.sample/.MainActivity --es run all
```

Other `--es run` values: `poll` (a status poll every 1.5 s), `overhead` (measures what capture
costs per request, in logcat), `security` (checks that another uid is refused).

## Keys

| | Keys |
|---|---|
| Move | `↑` `↓` or `j` `k` · `g` `G` top and bottom · `PgUp` `PgDn` · `Tab` `Shift+Tab` move focus between graph, list and detail |
| Views | `1` Connection View · `2` Thread View · `3` Rules |
| Detail pane | `Enter` open · `Esc` close · `h` `l` or `←` `→` switch tabs · `p` parsed or source · `o` original or rule-modified response |
| Bodies | `Enter` fold or unfold JSON · `[` fold all · `]` unfold all · <code>&#124;</code> jq filter (empty filter clears) · `<` `>` scroll sideways |
| Call Stack | `Enter` on a framework group expands it · `Enter` on an app frame opens `$EDITOR` there (needs source roots, below) |
| Live | `Space` pause or resume recording · `F` freeze the view (capture continues) · `L` back to live · `+` `-` zoom · `0` reset zoom · `v` select a time range (`v` or `Enter` again to apply) |
| Graph | `T` whole-app traffic or captured requests · `t` time since start or wall clock · `←` `→` move when the graph has focus |
| List | `c` collapse repeated calls · `s` sort by the next column · `S` reverse the sort · `C` choose columns |
| Session | `x` clear (asks first) · `?` help · `q` quit |

Mouse: click rows and tabs, double-click a row to open it, wheel to scroll (on the graph the
wheel zooms and Shift+wheel moves in time), drag on the graph to select a range, drag the divider
to resize the panes.

Coming in Phase 2: `/` filter bar and body search (`n`, `N`), `y` copy (including as cURL), `e`
export (HAR), `w` save a body, `d` diff two requests, `m` pin, `:` command palette. Phase 3 adds
`r` (new rule from the selected request) and rule editing.

## What you see is what was sent

traffic-police hides nothing: headers such as `Authorization` and `Cookie`, tokens and personal
data in bodies all appear exactly as the app sent and received them, on screen and (from Phase
2) in exports. Treat screenshots and exported files like the app's own logs.

## Project config

Put a `.traffic-police/project.toml` in your project; traffic-police uses the nearest one at or
above the directory you start it from.

```toml
# paths are relative to the directory that contains .traffic-police/
source_roots = ["app/src/main/java", "app/src/main/kotlin", "sdk/src/main/kotlin"]
```

`source_roots` lets `Enter` on a Call Stack frame open the file at that line in `$VISUAL` or
`$EDITOR` (VS Code, Cursor, Zed, Sublime Text and Helix get `file:line`; others `+line file`).
Rules (`.traffic-police/rules.toml`) arrive with Phase 3; the demo shows two built-in rules.

## Attach mode (Phase 4)

For any debuggable app on Android 8.0 (API 26) or newer, with no code changes. Capturing
requests made during app start-up works fully from Android 11 and on Android 10, is best effort
on Android 8.1 and 9, and is not possible on Android 8.0 (the platform has no way to attach at
launch there).

## Troubleshooting

- **Logs:** `~/.local/state/traffic-police/traffic-police.log` (or `$XDG_STATE_HOME`;
  `%LOCALAPPDATA%\traffic-police` on Windows). `TRAFFIC_POLICE_LOG=debug` logs more.
- **Images look blocky:** half-blocks are the fallback. Inside tmux they are the default,
  because probing for terminal graphics turns on tmux's `allow-passthrough`; set
  `TRAFFIC_POLICE_IMAGES=auto` to probe anyway.
- **No colors, or wrong colors:** `NO_COLOR` switches to a monochrome theme; set
  `COLORTERM=truecolor` if your terminal supports 24-bit color but does not advertise it.
- **"terminal too small":** make the window at least 100×30.
- **WAITING "waiting for com.example.app on ...":** the app is not running, or this build has no
  traffic-police library. On the device, `adb logcat -s TrafficPolice` shows `capturing in
  <process>; socket @traffic-police_...` when capture starts, or why it did not (`... is not
  debuggable`).
- **WAITING "... is frozen by Android":** Android 11 and later freeze apps cached in the
  background; they run no code, so capture cannot answer until the app runs again. Bring it to
  the foreground. On a test device you can turn freezing off in Developer options ("Suspend
  execution for cached apps"; the name varies between versions).
- **"2 devices connected; choose one with --serial":** use a serial from `adb devices`.
- **A device shows "unauthorized":** accept the USB debugging prompt on the device.
- **Android Studio is open too:** fine. traffic-police creates its own adb forwards and removes
  only those (and ones left behind by a traffic-police that was killed); Android Studio's are
  never touched.
- **DETACHED "another traffic-police took over":** a second traffic-police connected to the
  same process.
- **More:** `traffic-police doctor` (Phase 2) will check adb, the device, the app and the
  connection, and print a fix for each failure.

## Development

```sh
cd host
cargo test                                    # unit, behavior and conformance tests, 62 UI snapshots
INSTA_UPDATE=always cargo test -p traffic-police-tui   # after an intended UI change; review the snapshot diff
cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture   # frame time at 50,000 requests
cargo run -- demo --dump-frame 140x40@12 --keys 'g<Enter>l'   # print one frame as text
# drives the sample app on a device (installed debug build): every scenario, both processes, kill and relaunch
TP_E2E_SERIAL=emulator-5554 cargo test -p traffic-police-backends --test device_e2e -- --ignored --nocapture

cd android
./gradlew :capture-core:check                 # JVM tests against eight OkHttp versions, 3.9 to 5.5
./gradlew :sample-app:assembleDebug           # the sample app (assembleNondebuggable: release-like, with capture)
./gradlew :capture-core:benchmarkOverhead     # what capture costs per request on the JVM
./gradlew publishAllPublicationsToBuildRepository   # the artifacts in android/build/repo, not ~/.m2
```

Protocol golden files in `testdata/protocol/v1` are shared by both sides and rewritten only on
purpose: `./gradlew :capture-core:updateProtocolGoldens` (device side) and
`cargo test -p traffic-police-proto --test goldens -- --ignored update_goldens` (host side).

Nothing here sends data anywhere: no telemetry, and no network traffic from the host other than
adb.
