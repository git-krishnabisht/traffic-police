# traffic-police

Watch an Android app's HTTP and HTTPS traffic from your terminal, the way Android Studio's
Network Inspector shows it: a live traffic graph, a connection list with a timeline, full request
and response details, the thread and call stack that made each request, a thread view, and
response rewrite rules. No proxy and no certificates: a small runtime inside the (debuggable)
app hooks OkHttp and HttpURLConnection and streams events to the terminal over adb.

> **Status: Phase 0b.** The terminal UI is complete and runs on simulated traffic
> (`traffic-police demo`). Capturing from a real device arrives in Phase 1 (library mode) and
> Phase 4 (attach mode, no code changes). See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for
> the design and [docs/PROTOCOL.md](docs/PROTOCOL.md) for the device protocol.

## Install

Build from source (release binaries come later). You need Rust through rustup; the toolchain
is pinned in `host/rust-toolchain.toml` (1.98.1).

```sh
# once, if you do not have Rust yet
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# if cargo says the pinned toolchain is missing
rustup toolchain install 1.98.1 --component clippy rustfmt

cd host
cargo build --release
./target/release/traffic-police --help
```

Works on macOS, Linux and Windows Terminal, including inside tmux and over SSH. The terminal
must be at least 100 columns by 30 rows.

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
| Session | `x` clear (asks first) · `R` reveal redacted values for this session · `?` help · `q` quit |

Mouse: click rows and tabs, double-click a row to open it, wheel to scroll (on the graph the
wheel zooms and Shift+wheel moves in time), drag on the graph to select a range, drag the divider
to resize the panes.

Coming in Phase 2: `/` filter bar and body search (`n`, `N`), `y` copy (including as cURL), `e`
export (HAR), `w` save a body, `d` diff two requests, `m` pin, `:` command palette. Phase 3 adds
`r` (new rule from the selected request) and rule editing.

## Redaction

Secrets are masked by default: `Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`,
`X-Api-Key`, `Api-Key` and `X-Auth-Token` values show as `‹redacted N chars›` (the `Bearer`
scheme and cookie names stay visible). `R` reveals them until you quit, and the header shows
`REVEALED` meanwhile. Your own headers, query parameters and JSON paths (for example `$.aadhaar`,
`$..pan`) become configurable in Phase 2.

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

## Library mode (Phase 1)

Not published yet. The planned setup, from [docs/ARCHITECTURE.md §4.6](docs/ARCHITECTURE.md):

```kotlin
dependencies {
    debugImplementation("io.trafficpolice:capture:<version>")
    releaseImplementation("io.trafficpolice:capture-noop:<version>")
}

val client = OkHttpClient.Builder()
    .addNetworkInterceptor(TrafficPolice.networkInterceptor())
    .eventListenerFactory(TrafficPolice.eventListenerFactory(existingFactoryOrNull))
    .build()
```

Capture runs only in debuggable builds, and only adb (the shell user) or root can connect to it.

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
- **Device problems:** `traffic-police doctor` (Phase 1) will check adb, the device, the app and
  the connection, and print a fix for each failure.

## Development

```sh
cd host
cargo test                                    # unit and behavior tests, and 58 UI snapshots
INSTA_UPDATE=always cargo test -p traffic-police-tui   # after an intended UI change; review the snapshot diff
cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture   # frame time at 50,000 requests
cargo run -- demo --dump-frame 140x40@12 --keys 'g<Enter>l'   # print one frame as text
```

Nothing here sends data anywhere: no telemetry, and no network traffic from the host other than
adb.
