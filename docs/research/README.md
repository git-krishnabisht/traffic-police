# Phase 0 verification notes

These notes back the claims in `../ARCHITECTURE.md` and `../PROTOCOL.md`. Each was written while reading the sources, with a citation (`repo@ref:path:line` plus a short quote) for every statement; anything that could not be confirmed is marked **UNVERIFIED**, and conclusions drawn from code rather than observed on a device are marked as inference. Nothing here was tested on a device.

| Note | Question it answers |
|---|---|
| [01-studio-network-inspector.md](01-studio-network-inspector.md) | How Android Studio's Network Inspector captures OkHttp and HttpURLConnection traffic, applies rules, and feeds its graph; what to copy and what to fix |
| [02-studio-agent-jvmti.md](02-studio-agent-jvmti.md) | How Studio's App Inspection agent attaches, rewrites dex with slicer, dispatches hooks, and loads inspector code |
| [03-android-platform-attach.md](03-android-platform-attach.md) | Android and ART facts per API level: `attach-agent`, launch-time attach, `startup_agents`, JVMTI capabilities, class loaders, read-only dex, `run-as`, 16 KB pages, local sockets, `/proc/net/unix` |
| [04-adb-protocol.md](04-adb-protocol.md) | The adb server protocol the host speaks: framing, trackers, transports, forwarding, shell v2, sync, process tracking, server lifecycle |
| [05-okhttp-okio.md](05-okhttp-okio.md) | OkHttp 3.9–5.5 and Okio 1.13–3.18 internals and binary compatibility for a Java library compiled once |
| [06-rust-crates.md](06-rust-crates.md) | Current Rust crate versions, APIs and compatibility for the host |

`android.googlesource.com` was unreachable from the machine used for Phase 0, so AOSP sources were read from GitHub mirrors (GrapheneOS, LineageOS, `aosp-mirror`, `kroune/platform-tools-base`) and from AOSP Gerrit's REST API. The notes name the mirror and commit for every citation.

## Device checks (2026-09-29)

Read-only checks against a Nothing A015 (Android 16, API 36, user build, arm64-v8a only, 4 KB pages) through adb server version 41. They confirm points the notes above had derived from source:

- The device advertises `track_app` and `app_info`; `track-app` streams `AppProcesses` entries with `process_name`, `package_names`, `uid`, `user_id` and `waiting_for_debugger` populated.
- `forward tcp:0 localabstract:<name nobody listens on>` succeeds; connecting to the returned port succeeds and then reads EOF within about 2 ms. `killforward:tcp:<port>` answers `OKAY OKAY`.
- `/proc/net/unix` is readable from `adb shell` (listening abstract sockets carry flags `00010000`, state `01`, and a `@` path).
- `run-as` works for a debuggable app and runs in the `runas_app` SELinux domain; `dumpsys package` reports `primaryCpuAbi` and the `DEBUGGABLE` flag.
- Android Studio's own forward (`localabstract:AndroidStudioTransport`) was present, which is why the host never removes forwards it did not create.
