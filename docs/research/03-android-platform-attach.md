# traffic-police design verification: Android platform attach/launch mechanisms

Verification research. Rule followed: **verify, do not recall**. Every claim is cited as
`repo@ref:path:line` with a short verbatim quote from source I read during this task, or a URL for
official docs. Items I could not confirm are labelled **UNVERIFIED**.

## Refs used (what each corresponds to — checked, not assumed)

- **Local checkouts** (`SRC=…/scratchpad/src`):
  - `SRC/frameworks-base` = `GrapheneOS/platform_frameworks_base@17` (Android 17), head `92310923b24c`. Cited below as `GrapheneOS/platform_frameworks_base@17`.
  - `SRC/art` = `LineageOS/android_art@lineage-23.2` (Android 16), head `18ea424e`. Cited as `LineageOS/android_art@lineage-23.2`.
  - `SRC/adb` = `GrapheneOS/platform_packages_modules_adb@17`, head `c10299c60`.
  - `SRC/tools-base` = `kroune/platform-tools-base@…` (Android Studio / android-tools mirror), head `11ff8856`.
- **History** via `aosp-mirror/platform_frameworks_base` tags (verified to exist: android-8.0.0_r1, 8.1.0_r1, 9.0.0_r1, 10.0.0_r1, 11.0.0_r1, 12.0.0_r1, 13.0.0_r1, 14.0.0_r1, 15.0.0_r1, 16.0.0_r1; android-7.1.2_r39 exists but has no attach-agent). `aosp-mirror/platform_libcore` and `platform_art` do **not** exist on GitHub (404); libcore/ART history came from `LineageOS/android_libcore` and `LineageOS/android_art` branches and `GrapheneOS/platform_*`.
- **Other repos** (branch heads recorded for citation stability):
  - `GrapheneOS/platform_system_core@17` `322ff5129d86`; `GrapheneOS/platform_libcore@17` `ffe148abedb6`; `GrapheneOS/platform_bionic@17` `e93ce48518dc`; `GrapheneOS/platform_frameworks_native@17` `1c83ac6ae533`; `GrapheneOS/platform_system_sepolicy@17` `5e359dcf9ae8`.
  - `LineageOS/android_system_sepolicy` branches: lineage-15.0 `8a142b8f28aa` (8.0), 15.1 `e1729c3760a7` (8.1), 16.0 `04e2cd1b1ffd` (9), 17.1 `b052c214f3ca` (10), 18.1 `c590512ecdcf` (11), 20.0 `60f5720f9392` (13), 21.0 `83bf38d56eac` (14), 22.2 `24428bf868b3` (15), 23.0 `b0769f30959a` (16), 23.2 `885cc500f607` (16), 24.0 `20ee50b20ec3` (17).
  - `LineageOS/android_libcore` lineage-15.1 `27ddc2cfaa88`, 16.0 `6d72bb6e6a1a`, 20.0 `5c42796ae5d8`, 21.0 `81bfea828bc7`.
  - `chromium/chromium@c6b97f110209` (WebView/DevTools); `facebook/stetho@2198797c0ff9`; `torvalds/linux@v6.6` (kernel — approximate proxy for AOSP kernels; AOSP common kernels differ but `af_unix.c`/`memfd.c` are close).
- **API level numbering** confirmed: `GrapheneOS/platform_frameworks_base@17:core/java/android/os/Build.java:1313-1329` "UPSIDE_DOWN_CAKE = 34", "VANILLA_ICE_CREAM = 35", "BAKLAVA = 36", "CINNAMON_BUN = 37". So Android 16 = API 36, Android 17 = API 37.
- **ART ships in the updatable ART APEX from Android 12 (API 31)**: source.android.com/docs/core/ota/modular-system "ART | com.android.art | APEX | Android 12". So on API 31+ the ART-side behaviour tracks the ART module version, not only the OS image.

A separate fork ("ART") verified the ART-internal details (Q5, Q6, Q8, ART side of Q7 and Q1); its findings are integrated and re-spot-checked below against `SRC/art`.

---

## Q1. `cmd activity attach-agent` / `am attach-agent`

**Syntax.** `attach-agent <PROCESS> <FILE>`.
- `GrapheneOS/platform_frameworks_base@17:services/core/java/com/android/server/am/ActivityManagerShellCommand.java:5265-5266`: `pw.println("  attach-agent <PROCESS> <FILE>");` … `"    Attach an agent to the specified <PROCESS>, which may be either a process name or a PID."`
- Parsing (`…:4130-4143` `runAttachAgent`): it reads exactly two required args and rejects a third: `String process = getNextArgRequired(); String agent = getNextArgRequired();` then `if ((opt = getNextArg()) != null) { pw.println("Error: Unknown option: " + opt); return -1; }` then `mInternal.attachAgent(process, agent);`. So `<FILE>` is a **single** shell argument.

**Process argument = PID or process name.** `findProcessLOSP` first tries `Integer.parseInt(process)` against `mPidsSelfLocked`, else matches by process name (`…/ActivityManagerService.java:16983-17010`: `int pid = Integer.parseInt(process); … proc = mPidsSelfLocked.get(pid); … SparseArray<ProcessRecord> procs = all.get(process);`). It resolves under `UserHandle.USER_SYSTEM` (`attachAgent` passes `UserHandle.USER_SYSTEM`, `…:20073`), i.e. it finds by name in the primary user unless a bare PID is given.

**`path=options` syntax.** The agent string is `path` or `path=options`, split at the **first** `=`; everything after the first `=` is the options passed to `Agent_OnAttach`.
- `LineageOS/android_art@lineage-23.2:runtime/ti/agent.cc:39-46` (`AgentSpec::AgentSpec`): `size_t eq = arg.find_first_of('='); … name_ = arg.substr(0, eq); args_ = arg.substr(eq + 1, arg.length());`.
- So the .so path cannot contain `=`, but the options string can. `Debug.attachJvmtiAgent` enforces this at the Java layer: `GrapheneOS/platform_frameworks_base@17:core/java/android/os/Debug.java:2861` `Preconditions.checkArgument(!library.contains("="));` then `VMDebug.attachAgent(library + "=" + options, classLoader)` (`:2866`).
- The official ART example (source.android.com/docs/core/runtime/art-ti) is `cmd activity attach-agent com.example… '/data/data/com.example…/code_cache/libfieldnulls.so=Ljava/lang/Class;.name:…'` — the whole `so=opts` is one quoted arg.

**Permission / debuggable checks — two independent gates:**

1. **Caller must hold `SET_ACTIVITY_WATCHER`** (held by the `shell` package). `runAttachAgent` calls `mInternal.enforceCallingPermission(android.Manifest.permission.SET_ACTIVITY_WATCHER, "attach-agent")` (`…ShellCommand.java:4132-4134`). `GrapheneOS/platform_frameworks_base@17:packages/Shell/AndroidManifest.xml:187` `<uses-permission android:name="android.permission.SET_ACTIVITY_WATCHER" />` (present identically in android-8.0.0_r1:68). So `adb shell` (uid 2000) and root may call it; a normal app cannot.
2. **Target app must be debuggable, OR the whole build is debuggable.** In current AOSP:
   - `…/ActivityManagerService.java:20081` `enforceDebuggable(proc);`
   - `…:7003-7007` `private void enforceDebuggable(ProcessRecord proc) { if (!Build.IS_DEBUGGABLE && !proc.isDebuggable()) { throw new SecurityException("Process not debuggable: " + proc.info.packageName); } }`
   - `ProcessRecord.isDebuggable()` = `(info.flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0` (`…/ProcessRecord.java:1148-1151`).
   - `Build.IS_DEBUGGABLE` is `ro.debuggable==1` (userdebug/eng). So on a **user** build only `android:debuggable=true` apps qualify; on **userdebug/eng** every app qualifies.
   - **History:** in 8.0–9 the check reads `ro.debuggable` directly by name: e.g. `aosp-mirror/platform_frameworks_base@android-8.0.0_r1:…/ActivityManagerService.java:24429` `boolean isDebuggable = "1".equals(SystemProperties.get(SYSTEM_DEBUGGABLE, "0")); if (!isDebuggable) { if ((proc.info.flags & ApplicationInfo.FLAG_DEBUGGABLE) == 0) { throw new SecurityException("Process not debuggable: " + proc); } }` (`SYSTEM_DEBUGGABLE = "ro.debuggable"`, `ActivityThread.java:509`). Same shape in 8.1 (`:24589`), 9 (`:27064`), 10 (`:18770`), 11 (`:20052`). Semantics unchanged; refactored into `enforceDebuggable` later.
3. **Runtime gate inside the app process** (independent of AMS). `LineageOS/android_art@lineage-23.2:runtime/native/dalvik_system_VMDebug.cc:581-583` `if (!Dbg::IsJdwpAllowed()) { … ThrowSecurityException("Can't attach agent, process is not debuggable."); }`. JDWP is allowed only when zygote forked the app with `DEBUG_ENABLE_JDWP`, which happens for `debuggableFlag` apps (`GrapheneOS/platform_frameworks_base@17:…/ProcessList.java:1922,1936` `boolean debuggableFlag = (app.info.flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0; … runtimeFlags |= Zygote.DEBUG_ENABLE_JDWP;`) or, on userdebug/eng, for all apps (`core/java/com/android/internal/os/Zygote.java:1038-1039` `ENABLE_JDWP = SystemProperties.get("persist.debug.dalvik.vm.jdwp.enabled").equals("1")`, applied in `applyDebuggerSystemProperty` when `Build.IS_ENG || (Build.IS_USERDEBUG && ENABLE_JDWP)`).

**Code path (with quotes):**
- `ActivityManagerShellCommand.runAttachAgent` → `ActivityManagerService.attachAgent(process, path)` (`…:20071-20087`): resolves proc, `enforceDebuggable(proc)`, then `thread.attachAgent(path);`. `IApplicationThread.attachAgent` is a **oneway** binder call (`core/java/android/app/IApplicationThread.aidl:72,166` `oneway interface IApplicationThread { … void attachAgent(String path); }`) — so `cmd` returns 0 immediately and does not learn whether the agent loaded.
- In the app: `ActivityThread.ApplicationThread.attachAgent` posts `H.ATTACH_AGENT` (`core/java/android/app/ActivityThread.java:1724-1725`) → `handleAttachAgent((String) msg.obj, app != null ? app.mLoadedApk : null)` (`:3025-3026`).
- `ActivityThread.handleAttachAgent` (`:5291-5299`): `ClassLoader classLoader = loadedApk != null ? loadedApk.getClassLoader() : null; if (attemptAttachAgent(agent, classLoader)) { return; } if (classLoader != null) { attemptAttachAgent(agent, null); }` — tries the app class loader first, then retries with `null` (boot) loader.
- `attemptAttachAgent` (`:5281-5289`): `VMDebug.attachAgent(agent, classLoader);` catches `IOException` and logs `"Attaching agent with " + classLoader + " failed: " + agent`.
- `VMDebug.attachAgent(String, ClassLoader)` → native `nativeAttachAgent` (`GrapheneOS/platform_libcore@17:dalvik/src/main/java/dalvik/system/VMDebug.java:739-744`).
- ART: `VMDebug_nativeAttachAgent` → `Runtime::AttachAgent(env, filename, classloader)` (`LineageOS/android_art@lineage-23.2:runtime/native/dalvik_system_VMDebug.cc:595`), which `EnsureJvmtiPlugin` then `agent_spec.Attach(...)` (`runtime/runtime.cc:2326-2349`).

**First release:** Android **8.0 (API 26)**. `attach-agent` and `runAttachAgent` are present in android-8.0.0_r1 (`ActivityManagerShellCommand.java:240-241,2365`) and absent in android-7.1.2_r39 (0 hits). ART attach exists in 8.0 (`LineageOS/android_art@lineage-15.0` has `VMDebug_attachAgent`/`Runtime::AttachAgent`); 7.1 has no `openjdkjvmti` and no `attachAgent` in VMDebug (fork-verified). Note 8.0 also introduced routing `am` through `cmd activity`: `aosp-mirror/platform_frameworks_base@android-8.0.0_r1:cmds/am/am` = `if [ "$1" != "instrument" ] ; then cmd activity "$@"`.

**Does any version copy the agent, or require it in the app data dir?** **No copy — ART just `dlopen`s the string.** source.android.com/docs/core/runtime/art-ti: "The ART itself is agnostic regarding the specific location from which the agent comes. The string is used for a dlopen call. File system permissions and SELinux policies restrict the actual loading." It recommends "Embed the agent in the library directory of the app's APK" or "Use run-as to copy the agent into the app's data directory".
- The nativeloader namespace does not by itself block `/data/local/tmp`: `LineageOS/android_art@lineage-23.2:libnativeloader/library_namespaces.cpp` permits `/data:/mnt/expand` (fork-verified). The real barrier is SELinux (see Q10): an app domain cannot `execute` a `shell_data_file` (a `/data/local/tmp` .so). Therefore the agent .so must be somewhere the **app domain** can `execute`, i.e. under the app's own data dir (`app_data_file`) — which is why tools copy it into `code_cache/` with `run-as`. Confirmed by Android Studio's own transport daemon: `kroune/platform-tools-base@…:transport/native/daemon/daemon.cc:81-108` `CopyFileToPackageFolder` runs `mkdir -p ./code_cache/` and `cp <daemondir>/<agent> ./code_cache/` **via run-as**, then attaches `…/code_cache/<lib>.so=<config>` (`process_manager.android.cc:95-101`, `GetAttachAgentParams`). So: yes, a real SELinux denial exists for loading an app-process .so from `/data/local/tmp`, and the established workaround is `run-as … cp` into `code_cache/`.

**What the target does if the agent fails to load.** It does **not** crash from a load failure; ART logs and throws `java.io.IOException` back to the (in-process) caller, which `ActivityThread.attemptAttachAgent` swallows with a `Slog.e`:
- `LineageOS/android_art@lineage-23.2:runtime/runtime.cc:2330` `LOG(WARNING) << "Could not load plugin: " << error_msg;` (plugin load fail) and `:2345` `LOG(WARNING) << "Agent attach failed (result=" << error << ") : " << error_msg;` — both then `ThrowIOException`.
- dlopen failure message: `runtime/ti/agent.cc:131` `"Unable to dlopen %s: %s"`. Non-zero `Agent_OnAttach`: `:100` `"Initialization of %s returned non-zero value of %d"`. Native-bridge agents are refused: `:144` `"Native-bridge agents unsupported: %s"` — **the agent .so must match the app process ABI.**
- **But**: any crash inside the agent's own `Agent_OnAttach` (e.g. an abort) does crash the app process. And from Android 9 on, a failed/unloaded agent library is deliberately never `dlclose`d (fork-verified `agent.cc:171-173` "Don't actually android::CloseNativeLibrary").
- **Idempotency (UNVERIFIED inference):** re-attaching the same path returns the already-open dlopen handle but calls `Agent_OnAttach` again. Standard dlopen behaviour; not tested on device. **Make `Agent_OnAttach` idempotent.**

---

## Q2. `am start --attach-agent <agent>` and `--attach-agent-bind <agent>`

**Syntax & help text** (`GrapheneOS/platform_frameworks_base@17:…/ActivityManagerShellCommand.java:5004-5005`):
`"      --attach-agent <agent>: attach the given agent before binding"` and
`"      --attach-agent-bind <agent>: attach the given agent during binding"`. The `<agent>` value is again a single `path[=options]` arg. Only one may be given: `…:659-674` — `--attach-agent` sets `mAttachAgentDuringBind = false`, `--attach-agent-bind` sets `= true`, and either errors `"Multiple --attach-agent(-bind) not supported"` if `mAgent` is already set.

**How the agent is threaded through** (`…ShellCommand.java:830-831`): a `ProfilerInfo` is built carrying `mAgent, mAttachAgentDuringBind`. It flows via `startActivity` → `ActivityTaskSupervisor.resolveActivity` → `ActivityManagerService.setDebugFlagsForStartingActivity` → `setProfileApp(...)`, which stores it as the single "profile app" slot (`AppProfiler.setProfileAppLPf`). When the process attaches, `AppProfiler.setupProfilerInfoLocked` (`…/AppProfiler.java:2350-2438`) consumes it.

**Semantics — exactly when each attaches** (`…/AppProfiler.java:2359-2438`, and `ActivityThread.handleBindApplication`):
- `--attach-agent` (`attachAgentDuringBind=false`): `needsInfo = profileFile != null || attachAgentDuringBind` is false, so **no** ProfilerInfo is sent in `bindApplication`; but `preBindAgent = mProfileData.getProfilerInfo().agent` is set (`:2366-2368`), and at `:2432-2434` `if (preBindAgent != null) { thread.attachAgent(preBindAgent); }` — i.e. a standalone `attachAgent` oneway is sent to the app **immediately before** `thread.bindApplication(...)` runs. Net: agent attaches once, just before app code binds.
- `--attach-agent-bind` (`attachAgentDuringBind=true`): `needsInfo` is true → the ProfilerInfo (carrying the agent) is passed into `bindApplication`, and `ActivityThread.handleBindApplication` attaches it mid-bind: `core/java/android/app/ActivityThread.java:8062-8063` `if (data.initProfilerInfo.attachAgentDuringBind) { agent = data.initProfilerInfo.agent; }` then `:8145-8146` `if (agent != null) { handleAttachAgent(agent, data.info); }`. This runs **after** `getPackageInfo` builds `data.info` (the LoadedApk, `:8138`) and **before** `makeApplicationInner` (`:8306`) / `callApplicationOnCreate` — so the app class loader exists but no app code has run.
  - **Precise observation / risk:** for `--attach-agent-bind`, the code at `:2366-2368` **also** sets `preBindAgent` (it is gated only on `agent != null`, not on `!attachAgentDuringBind`), so the standalone pre-bind `thread.attachAgent()` at `:2434` is **also** sent. Reading the code literally, `--attach-agent-bind` causes the agent to be attached **twice** (once pre-bind, once during bind). This shape has existed since API 28 (same in android-9.0.0_r1:`…/ActivityManagerService.java:7793-7794` + `ActivityThread.java:5559,5619`). **Recommendation:** use `--attach-agent` (single, deterministic, before bind) rather than `--attach-agent-bind`; and regardless make `Agent_OnAttach` idempotent.

**Debuggable restriction for launch** (`ActivityTaskSupervisor.resolveActivity`, `…/wm/ActivityTaskSupervisor.java:722-729`): `final boolean requestProfile = profilerInfo != null; … final boolean debuggable = (Build.IS_DEBUGGABLE || (aInfo.applicationInfo.flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0) && !aInfo.processName.equals("system"); if ((requestDebug && !debuggable) || (requestProfile && (!debuggable && !aInfo.applicationInfo.isProfileableByShell()))) { Slog.w(TAG, "Ignore debugging for non-debuggable app: " + aInfo.packageName); }`. Note this means: an agent launch is silently ignored (not an error) for a non-debuggable, non-profileable app. And `setProfileApp` itself throws `SecurityException("Process not debuggable, and not profileable by shell")` on a user build (`…/ActivityManagerService.java:8412-8430`).
- `isProfileableByShell()` = `(privateFlags & PRIVATE_FLAG_PROFILEABLE_BY_SHELL) != 0` (`core/java/android/content/pm/ApplicationInfo.java:2878`). **`profileable` does NOT enable agent attach** — the agent attach path (`VMDebug_nativeAttachAgent`) still requires JDWP-allowed (debuggable). Profileable-by-shell only relaxes the AMS `setProfileApp` gate for the *profiler file*; the in-process ART gate still blocks agents on a non-debuggable app. So for traffic-police: **`android:debuggable=true` is required**, `profileable` is insufficient.

**First release:** `--attach-agent` = Android **9 (API 28)**; `--attach-agent-bind` = **9 (API 28)**.
- android-8.1.0_r1 has `--attach-agent` **only** (`ActivityManagerShellCommand.java:297-298`, `mAgent = getNextArgRequired()` with no `mAttachAgentDuringBind`), attached pre-bind (`ActivityManagerService.java:7096` `thread.attachAgent(agent)`). So `am start --attach-agent` actually first appears in **8.1 (API 27)** but only in the "before bind" form.
- android-8.0.0_r1 has **no** `--attach-agent` option (only the `attach-agent` subcommand): grep of the 8.0 ShellCommand finds `"--attach-agent"` = 0.
- `--attach-agent-bind` and `mAttachAgentDuringBind` first appear in android-9.0.0_r1 (`ActivityManagerShellCommand.java:333-340`).
- **Matrix:** API 26 → only `attach-agent` subcommand (running process); API 27 → adds `am start --attach-agent` (pre-bind only); API 28+ → adds `--attach-agent-bind`.

---

## Q3. `code_cache/startup_agents`

**Exact directory:** `<app data dir>/code_cache/startup_agents`.
- `ActivityThread.handleAttachStartupAgents(String dataDir)` (`GrapheneOS/platform_frameworks_base@17:core/java/android/app/ActivityThread.java:5301-5323`): `Path codeCache = ContextImpl.getCodeCacheDirBeforeBind(new File(dataDir)).toPath(); if (!Files.exists(codeCache)) { return; } Path startupPath = codeCache.resolve("startup_agents"); if (Files.exists(startupPath)) { try (DirectoryStream<Path> startupFiles = Files.newDirectoryStream(startupPath)) { for (Path p : startupFiles) { handleAttachAgent(p.toAbsolutePath().toString() + "=" + dataDir, null); } } }`
- `getCodeCacheDirBeforeBind` = `new File(dataDir, "code_cache")` (`core/java/android/app/ContextImpl.java:1011-1012`).
- Android Studio hard-codes the same path: `kroune/platform-tools-base@…:deploy/sites/tests/src/java/com/android/tools/TestSites.java:41` `Assert.assertEquals("/data/data/foo/code_cache/startup_agents/", startup);` and generator `deploy/sites/src/com/android/tools/SitesGenerator.java:60-64` `"AppStartupAgent" = AppCodeCache(pkg) + "startup_agents/"`.

**Conditions.** Only for **debuggable** apps (checked host-side by AMS, not by `ro.debuggable` alone). `AppProfiler.setupProfilerInfoLocked` (`…/AppProfiler.java:2435-2436`): `if (app.isDebuggable()) { thread.attachStartupAgents(app.info.dataDir); }`. `isDebuggable()` is the app's `FLAG_DEBUGGABLE` (see Q1); note it does **not** OR in `Build.IS_DEBUGGABLE` here, so on a userdebug build a non-debuggable app is **not** given startup agents (unlike `attach-agent`). This is called unconditionally at process attach (right where `--attach-agent` pre-bind attach happens), i.e. for every debuggable app launch.

**How agents there are attached:** each entry is attached as `<absolute file path>=<dataDir>` with the **boot** class loader (`handleAttachAgent(..., null)` → `loadedApk` null → classLoader null). So (a) every startup agent receives the app's **data dir** as its `Agent_OnAttach` options string, and (b) the .so is resolved without the app class loader's native search path.

**File naming rules:** none — `Files.newDirectoryStream(startupPath)` iterates **all** entries in the directory (no suffix filter, no ordering guarantee). Studio versions its agent as `agent-<version#>` and deletes any file it doesn't recognise, precisely because there are no naming rules: `kroune/platform-tools-base@…:deploy/installer/agent_interaction.cc:158-161` "Clean up other agents from the startup_agent directory. Because agents are versioned (agent-<version#>) we cannot simply copy our agent on top of the previous file. If the startup_agent directory exists but our agent cannot be found in it, we assume another agent is present and delete it."

**Any copying?** The framework does **no** copying — you must place the .so in that directory yourself (Studio uses `run-as … mkdir/cp`, `agent_interaction.cc:170-188`).

**Persistence across launches:** the directory is ordinary `code_cache` content and persists until the app is upgraded/uninstalled or the cache is cleared. Docs: `core/java/android/content/Context.java:2028-2044` "The system will delete any files stored in this location both when your specific application is upgraded, and when the entire platform is upgraded." installd clears it on explicit code-cache clear: `GrapheneOS/platform_frameworks_native@17:cmds/installd/InstalldNativeService.cpp:1294-1297` `CODE_CACHE_DIR_POSTFIX = "/code_cache"` under `FLAG_CLEAR_CODE_CACHE_ONLY`. **So it survives normal relaunches → good for "capture from launch" without re-pushing each time, but you must re-verify presence and not rely on it forever.**

**First release:** Android **11 (API 30)**. `handleAttachStartupAgents` / `ATTACH_STARTUP_AGENTS` / the `thread.attachStartupAgents(...)` call all first appear in android-11.0.0_r1 (`ActivityThread.java:3952-3969`, `ActivityManagerService.java:5280-5282`) and are absent in android-10.0.0_r1 (grep `startup_agents` = 0). Confirmed by presence counts across tags: 8.0–10 = 0, 11–16 = 1.

---

## Q4. `Debug.attachJvmtiAgent(String library, String options, ClassLoader)`

**First API level: 28 (Android 9).** Present in android-9.0.0_r1 API surface: `aosp-mirror/platform_frameworks_base@android-9.0.0_r1:api/current.txt:32314` `method public static void attachJvmtiAgent(java.lang.String, java.lang.String, java.lang.ClassLoader) throws java.io.IOException;`. Absent from android-8.1.0_r1 `api/current.txt` (0 hits). The method body exists in 9 and 10 identically (`…/os/Debug.java`).

**Restrictions.** (`GrapheneOS/platform_frameworks_base@17:core/java/android/os/Debug.java:2848-2867`)
- Javadoc: "Note: agents may only be attached to debuggable apps. Otherwise, this function will throw a SecurityException." Throws `IOException` if the agent could not be attached, `SecurityException` if not debuggable (the `SecurityException` originates in ART `VMDebug_nativeAttachAgent`, Q1).
- `Preconditions.checkNotNull(library); Preconditions.checkArgument(!library.contains("="));` — the library path itself must not contain `=` (options go in the separate `options` arg).
- It just calls `VMDebug.attachAgent(library, classLoader)` or `VMDebug.attachAgent(library + "=" + options, classLoader)`.

**Does it copy the library into code_cache?** **No.** No file I/O whatsoever — it forwards the string straight to `VMDebug.attachAgent`. Same "just dlopen" behaviour as `attach-agent`. This is a **public SDK** call the debuggable app can make on *itself* (e.g. to self-attach the capture agent), but it cannot be used by traffic-police from outside the process. The `classLoader` argument selects the native library search path used by `OpenNativeLibrary` (Q7).

---

## Q5. ART TI capabilities (integrated from ART-fork, re-spot-checked)

**Where defined:** `kPotentialCapabilities` in `art_jvmti.h` (lineage-23.2 `openjdkjvmti/art_jvmti.h:256`; 8.0 `runtime/openjdkjvmti/art_jvmti.h:195`). `kNonDebuggableUnsupportedCapabilities` first appears in Android 9 (lineage-16.0 `art_jvmti.h`; lineage-23.2 `:312`); absent in 8.0/8.1.

**Values of the capabilities that matter** (spot-checked in `SRC/art` = lineage-23.2):
`openjdkjvmti/art_jvmti.h:257` `.can_tag_objects = 1`; `:266` `.can_redefine_classes = 1`; `:283` `.can_generate_all_class_hook_events = 0`; `:294` `.can_retransform_classes = 1`; `:295` `.can_retransform_any_class = 0`. These values are identical from Android 9 through 17 (fork-verified across lineage-16.0…24.0) and already present in 8.0/8.1 (`lineage-15.0:art_jvmti.h:196,205,222,233,234`).

| capability | 8.0 | 8.1 | 9 … 17 |
|---|---|---|---|
| can_tag_objects | 1 | 1 | 1 |
| can_redefine_classes | 1 | 1 | 1 (removed if not full-debuggable) |
| can_retransform_classes | 1 | 1 | 1 (removed if not full-debuggable) |
| can_retransform_any_class / can_redefine_any_class | 0 | 0 | 0 |
| can_generate_all_class_hook_events | 0 | 0 | 0 |
| can_generate_method_entry/exit_events | 0 | 1 | 1 |
| can_generate_breakpoint_events | 0 | 1 | 1 |

**Debuggability gate on capabilities.** Full JVMTI requires the process be Java-debuggable (or forced-interpret): `SRC/art:openjdkjvmti/art_jvmti.h:75-78` `IsFullJvmtiAvailable() { return runtime->GetInstrumentation()->IsForcedInterpretOnly() || runtime->IsJavaDebuggableAtInit(); }`. When not full, `GetPotentialCapabilities` strips the non-debuggable-unsupported set: `openjdkjvmti/OpenjdkJvmTi.cc:1111-1126` `*capabilities_ptr = kPotentialCapabilities; if (UNLIKELY(!IsFullJvmtiAvailable())) { … if (kNonDebuggableUnsupportedCapabilities.e == 1) { capabilities_ptr->e = 0; } … }` — and `kNonDebuggableUnsupportedCapabilities` marks `can_redefine_classes`, `can_retransform_classes`, `can_retransform_any_class`, `can_redefine_any_class`, `can_pop_frame`, `can_force_early_return` as unsupported (`art_jvmti.h:312-351`). Comment (`:309`): "We need to ensure that inlined code is either not present or can always be deoptimized. This is not guaranteed for non-debuggable processes". **Consequence: retransform is only available in a debuggable app process → traffic-police must target `android:debuggable=true` apps.** For a debuggable app, zygote sets the runtime debuggable and deoptimizes the boot image (fork-verified `dalvik_system_ZygoteHooks.cc`), which is what makes boot classes retransformable.

**Was RetransformClasses functional for debuggable apps on API 26/27?** **Yes** (per source; not tested on device). 8.0/8.1 have a real implementation: `LineageOS/android_art@lineage-15.0:runtime/openjdkjvmti/transform.cc:90-133` calls `IsModifiableClass`, dispatches the `kClassFileLoadHookRetransformable` event, then `Redefiner::RedefineClassesDirect(...)`. Boot classes are handled (`ti_redefine.cc:1296-1297` appends to boot class path). Known 8.0 caveat only for **intrinsic** methods (jitted intrinsics may keep old versions — fork-verified `ti_redefine.cc` TODO).

**Retransform/redefine restrictions (structural changes rejected).** ART allows **method-body-only** changes; adding/removing methods or fields, or changing modifiers/hierarchy, is rejected. Error codes (`SRC/art:openjdkjvmti/ti_redefine.cc`, fork-verified line ranges): method count/added/deleted → `UNSUPPORTED_REDEFINITION_METHOD_ADDED/_DELETED`; method modifiers → `_METHOD_MODIFIERS_CHANGED`; field add/remove/flags → `_SCHEMA_CHANGED`; class modifiers → `_CLASS_MODIFIERS_CHANGED`; superclass/interfaces → `_HIERARCHY_CHANGED`. The transformed dex must contain **exactly one** class_def (`ti_redefine.cc:1078-1080` "Expected 1 class def in dex file but found %d" → `ILLEGAL_ARGUMENT`), and must pass verification (`FAILS_VERIFICATION`). **This is exactly the slicer model (rewrite method bodies only).**

**Non-modifiable classes.** `IsModifiableClass` returns false only for `UNMODIFIABLE_CLASS`; `CanRedefineClass` blocks primitives, **interfaces** (`ti_redefine.cc:416` "Modification of Interface classes is currently not supported"), String, arrays, proxies, and NonDebuggableClasses (`:431` "Class might have stack frames that cannot be made obsolete"). NonDebuggableClasses are those with a frame on a thread stack at zygote fork, collected only for debuggable apps.

**Can boot classes like `java.net.URL` be retransformed in a debuggable app process?** **Yes** (per source). It is not an interface/primitive/array/proxy/String; boot classes are supported (`SRC/art:openjdkjvmti/ti_redefine.cc:2503-2507` "if (data.GetSourceClassLoader() == nullptr) { … AppendToBootClassPath("); the boot image is deoptimized for debuggable apps. It would only fail if `URL` had a live frame at zygote fork (unlikely; not device-tested). **This validates hooking `java.net.URL.openConnection()`.**

**Structural redefinition extension** (`com.android.art.class.structurally_redefine_classes`, additive only): first appears in **Android 11** (fork-verified lineage-18.1 `ti_extension.cc`; absent lineage-17.1), and only registers when JNI ids are index-based and full JVMTI is available (debuggable). Not needed for method-body hooking.

---

## Q6. Class-path search (`AddToBootstrapClassLoaderSearch`, `AddToSystemClassLoaderSearch`, extensions)

**`AddToBootstrapClassLoaderSearch` works in the live phase on every version (26→17).** Only checks: runtime & class linker non-null, segment non-null. `SRC/art:openjdkjvmti/ti_search.cc:223-250`: `art::ArtDexFileLoader dex_file_loader(segment); if (!dex_file_loader.Open(/* verify= */ true, /* verify_checksum= */ true, …)) { … return ERR(ILLEGAL_ARGUMENT); } current->AddExtraBootDexFiles(segment, segment, std::move(dex_files));`. 8.0 equivalent uses `art::DexFile::Open` + `AppendToBootClassPath` (fork-verified `lineage-15.0:runtime/openjdkjvmti/ti_search.cc:210-236`). The segment must be a dex (or jar/apk containing dex); otherwise `ILLEGAL_ARGUMENT`. **Crucially it opens the file with ART's own loader — it does NOT go through the Java `DexFile.openDexFileNative` path, so the Android-14 "writable dex" check (Q9) does not apply to it.** Boot-classpath dex from an unknown location gets hidden-API domain `kPlatform` (fork-verified `hidden_api.cc`).

This is what traffic-police's design uses for the trampoline class (`AddToBootstrapClassLoaderSearch`) — confirmed available and unrestricted-by-phase for a live/attached agent, all API levels. Android Studio uses the same call: `kroune/platform-tools-base@…:transport/native/agent/transport_agent.cc:53-57` `jvmti->AddToBootstrapClassLoaderSearch(agent_lib_path.c_str());` and `deploy/agent/native/instrumenter.cc:193` `jvmti->AddToBootstrapClassLoaderSearch(root_aware_jar_path.c_str())`.

**`AddToSystemClassLoaderSearch`:** ONLOAD phase appends to `java.class.path`; LIVE phase calls `BaseDexClassLoader.addDexPath` on `Runtime::GetSystemClassLoader()` (`SRC/art:openjdkjvmti/ti_search.cc:372-401`: `jobject loader = art::Runtime::Current()->GetSystemClassLoader(); … return AddToDexClassLoader(jvmti_env, loader, segment);`). WRONG_PHASE outside ONLOAD/LIVE. Works in live phase on API 26→17.

**What "system class loader" means in ART:** it is `ClassLoader.getSystemClassLoader()` — the **zygote's** `PathClassLoader` built from `java.class.path`, **not** the app's class loader. `SRC/art:runtime/runtime.cc` `CreateSystemClassLoader` invokes `getSystemClassLoader` (fork-verified `runtime.cc:975-1008`), created in `Runtime::Start` before the app is forked. libcore: `GrapheneOS/platform_libcore@17:ojluni/src/main/java/java/lang/ClassLoader.java:252,269` builds it from `System.getProperty("java.class.path", ".")` as `new PathClassLoader(classPath, librarySearchPath, BootClassLoader.getInstance())`. **So classes added via `AddToSystemClassLoaderSearch` are NOT visible to the app's `PathClassLoader` — do not use it to inject the capture runtime for app-loaded classes.** (This confirms the design's choice to route the trampoline through the bootstrap loader and the runtime through a child loader of the app loader, rather than the system loader.)

**Extension functions** (namespace `com.android.art.classloader.*`), first release **Android 11** (fork-verified lineage-18.1 present, lineage-17.1 absent):
- `com.android.art.classloader.add_to_dex_class_loader` — params `(jobject classloader, const char* segment)`; requires LIVE phase and a `BaseDexClassLoader` (else `CLASS_LOADER_UNSUPPORTED`); goes through `BaseDexClassLoader.addDexPath` (`SRC/art:openjdkjvmti/ti_search.cc:335-368`). This is the AddToDexClassLoader used to add a dex to an **arbitrary** class loader (e.g. the app's loader) — the mechanism for injecting the capture runtime into a child/related loader.
- `com.android.art.classloader.add_to_dex_class_loader_in_memory` — params `(jobject classloader, const char* dex_bytes, jint dex_bytes_length)`; LIVE phase only. Implementation writes bytes to a **memfd** and adds `/proc/self/fd/<n>` via `addDexPath` (`SRC/art:openjdkjvmti/ti_search.cc:252-311`: `art::memfd_create("JVMTI InMemory Added dex file", 0)` … `oss << "/proc/self/fd/" << file.Fd()`).
  - **UNVERIFIED risk for targetSdk ≥ 34:** the memfd path is added through `BaseDexClassLoader.addDexPath` → `DexPathList` → `DexFile.openDexFileNative`, which on API 34+ runs `access(path, W_OK)` and rejects writable dex (Q9). A memfd's backing shmem inode is created mode `0777` (`torvalds/linux@v6.6:mm/shmem.c:4787` `S_IFREG | S_IRWXUGO`) unless `MFD_NOEXEC_SEAL`/sealing clears it — ART passes flags `0` (`ti_search.cc:281`), so `/proc/self/fd/<n>` is likely `W_OK`-accessible and would be **rejected** on targetSdk 34+. Not device-tested. **Prefer the file-backed AddToDexClassLoader with a read-only file, or AddToBootstrapClassLoaderSearch, over the in-memory variant on API 34+.**
- `com.android.art.class.get_class_loader_class_descriptors` exists from Android 9 (fork-verified).

---

## Q7. Finding the app class loader via JVMTI

**Does ActivityThread/LoadedApk set the main thread's context class loader to the app's loader?** **Yes, for the common case** (single-package process), and this is set during app bind. `GrapheneOS/platform_frameworks_base@17:core/java/android/app/LoadedApk.java:1377-1409` `initializeJavaContextClassLoader()`: it computes `boolean sharable = (sharedUserIdSet || processNameNotDefault);` and `ClassLoader contextClassLoader = (sharable) ? new WarningContextClassLoader() : mClassLoader; Thread.currentThread().setContextClassLoader(contextClassLoader);`. It is called from `makeApplication` (`:1636-1641`, `final java.lang.ClassLoader cl = getClassLoader(); … initializeJavaContextClassLoader();`). Same in android-8.0.0_r1 (`LoadedApk.java:812-817,961`).
- So for a normal app the main thread's context class loader **is** the app `PathClassLoader` (`mClassLoader`). For apps with `sharedUserId` or a custom `android:process`, it is a `WarningContextClassLoader` whose parent is the app loader — `getContextClassLoader()` still resolves classes but via the warning proxy.
- **`GetThreadInfo(...).context_class_loader` reads exactly this field:** `SRC/art:openjdkjvmti/ti_thread.cc` maps `jvmtiThreadInfo.context_class_loader` to `Thread.contextClassLoader` (fork-verified `ti_thread.cc:191,307-314`; 8.0 `lineage-15.0:runtime/openjdkjvmti/ti_thread.cc:132,220-224`). So calling `GetThreadInfo` on the main thread yields the app loader (or the warning proxy) from API 26 on.

**What class loader does `handleAttachAgent` pass, and does ART use it in `Agent_OnAttach`?** `ActivityThread.handleAttachAgent` passes `loadedApk.getClassLoader()` (the app loader) to `VMDebug.attachAgent(agent, classLoader)` (`ActivityThread.java:5292-5296`). **But ART does NOT hand that loader to `Agent_OnAttach`** — it passes `nullptr` as `reserved`: `SRC/art:runtime/ti/agent.cc:96-98` `*call_res = callback(Runtime::Current()->GetJavaVM(), copied_args.get(), nullptr);`. The class loader is used **only** to pick the native linker search path for the dlopen: `runtime/ti/agent.cc:112-129` `JavaVMExt::GetLibrarySearchPath(env, class_loader)` then `android::OpenNativeLibrary(env, Runtime::Current()->GetTargetSdkVersion(), name_.c_str(), class_loader, …)`. (In 8.0/8.1 there is no loader parameter at all — `VMDebug.attachAgent(String)` only, fork-verified `lineage-15.1:VMDebug.java:492`; 9+ adds the `(String, ClassLoader)` overload, `lineage-16.0:VMDebug.java:515-525`.)

**JNI `FindClass` inside `Agent_OnAttach` resolves boot classes only.** ART's `GetClassLoader` uses the top non-runtime stack frame's declaring class loader: `SRC/art:runtime/jni/jni_internal.cc:382-392` `ArtMethod* method = soa.Self()->GetCurrentMethod(nullptr); … if (method != nullptr) { return method->GetDeclaringClass()->GetClassLoader(); }`. During attach that frame is native `dalvik.system.VMDebug` (boot loader), so `FindClass("Lokhttp3/...")` fails. **The agent must obtain the app loader and call `loader.loadClass(...)`.**

**Most robust approach API 26→17:** obtain the app class loader independently of the attach path. Two source-proven options, both working from API 26:
1. **`GetThreadInfo(main thread).context_class_loader`** — the app loader for normal apps (above). Android Studio's app-inspection agent uses exactly this as a fallback: `kroune/platform-tools-base@…:app-inspection/agent/…/AppInspectionService.java:454` `ClassLoader looperClassLoader = Looper.getMainLooper().getThread().getContextClassLoader();`.
2. **Enumerate loaded instances / classes and read their loader.** Studio's memory agent does `GetThreadInfo` → `ti.context_class_loader` (`profiler/native/perfa/memory/memory_tracking_env.cc:1172-1178`); its deploy agent calls `Thread.currentThread().getContextClassLoader()` and also `ActivityThread.currentApplication().mLoadedApk.getClassLoader()` via JNI (`deploy/agent/native/class_finder.cc:24-49`).
   - Note `IterateOverInstancesOfClass` is **NOT_IMPLEMENTED** in API 26–28 and only implemented from **API 29** (verified: lineage-16.0 `OpenjdkJvmTi.cc:507` returns `ERR(NOT_IMPLEMENTED)`; lineage-17.1 `:517` calls `heap_util.IterateOverInstancesOfClass`). `GetLoadedClasses`+`GetClassLoader` and `IterateThroughHeap` (needs `can_tag_objects`) work from 26.
- **Recommendation:** primary = `GetThreadInfo` context class loader of the main/Looper thread (works 26→17, matches Studio); fallback = JNI `ActivityThread.currentApplication().mLoadedApk.getClassLoader()` (works whenever the Application exists), and for classes loaded after attach use the `loader` argument the ClassFileLoadHook provides directly. Do **not** rely on the loader passed to attach (ART discards it for `Agent_OnAttach`), and do not rely on hidden `BaseDexClassLoader` reflection.

---

## Q8. ClassFileLoadHook in ART

**Bytes are dex, not class files.** Official: source.android.com/docs/core/runtime/art-ti "Class redefinition is based on Dex files, containing only a single class definition, instead of class files." In code the hook passes `def->GetDexData()`, which `ArtClassDefinition::Init` sets from the dex file bytes (fork-verified `SRC/art:openjdkjvmti/transform.cc:105-116`, `ti_class_definition.cc:173-177`).

**How many classes / dex per event:** **one class per event.** `RetransformClasses` loops over class definitions and fires the hook per class (fork-verified `transform.cc:130-147`). The `name` argument is the internal descriptor form `Lpkg/Class;` minus the wrapping (`ti_class_definition.cc:82`).

**Important caveat on the byte buffer contents** (fork-verified, matters for slicer):

| Release | Bytes passed to hook |
|---|---|
| 8.0 / 8.1 | The **entire** containing dex (dequickened copy) |
| 9 – 12L | Whole dequickened dex, **except** compact-dex (9+) or hidden-API-bearing dex (10+, e.g. boot jars) → then a **single-class** standard dex |
| 13 – 17 | The **entire** containing dex (for compact dex, the original dex reopened) |

So the buffer often contains **more than the target class**. The agent must (a) locate the target class within the passed dex, and (b) return a dex with **exactly one** class_def, or ART rejects it: first-load `ti_class.cc` "Unable to use transformed dex file of %s because it contained too many classes"; retransform → `ILLEGAL_ARGUMENT` (Q5). Unchanged classes should be left untouched (`ti_redefine.cc:683` "Only try to transform classes that have been modified."). This validates the design's use of slicer to emit a single-class dex. Also: classes loaded during runtime init get no event (`ti_class.cc:185-186` "Ignoring load of class <…> as it is being loaded during runtime initialization.") — consistent with `can_generate_all_class_hook_events = 0`, so you cannot hook classes loaded before the START phase.

---

## Q9. Android 14+ dynamic code loading must be read-only

**The rule.** developer.android.com/about/versions/14/behavior-changes-14 §"Safer dynamic code loading": "If your app targets Android 14 (API level 34) or higher and uses Dynamic Code Loading (DCL), all dynamically-loaded files must be marked as read-only. Otherwise, the system throws an exception." Recommended pattern: `jar.setReadOnly()` before writing content, then `PathClassLoader`.

**Where enforced and for which targetSdk.** In **ART's native `DexFile` JNI** (the `openDexFileNative` path used by file-backed class loaders), gated by a compat change enabled **after targetSdk TIRAMISU (33)** — i.e. targetSdk ≥ 34.
- `SRC/art:runtime/native/dalvik_system_DexFile.cc:380-386`: `if (isReadOnlyJavaDclChecked() && access(sourceName.c_str(), W_OK) == 0) { LOG(ERROR) << "Attempt to load writable dex file: " << sourceName.c_str(); if (isReadOnlyJavaDclEnforced(env)) { … env->ThrowNew(se.get() [SecurityException], StringPrintf("Writable dex file '%s' is not allowed.", …)); return nullptr; } }`.
- UID exemptions (`:353-364`): `isReadOnlyJavaDclChecked()` returns `uid != 0 && uid != 1000 && uid != 2000` — **root, system, and shell are exempt**; a normal app UID is not.
- Enforcement gate (`:317-343`): device API ≥ U, then `Compatibility.isChangeEnabled(kEnforceReadOnlyJavaDcl)` where `kEnforceReadOnlyJavaDcl = 218865702`.
- libcore change id: `GrapheneOS/platform_libcore@17:dalvik/src/main/java/dalvik/system/DexFile.java:66-76` `@ChangeId @EnabledAfter(targetSdkVersion = VersionCodes.TIRAMISU) public static final long ENFORCE_READ_ONLY_JAVA_DCL = 218865702;` (Javadoc: "Enforce the file passed to open DexFile to be set as read-only for apps targeting U+.").
- **First release:** Android **14 (API 34)**. Present in lineage-21.0 (14): `LineageOS/android_libcore@lineage-21.0:…/DexFile.java:75-76` and `LineageOS/android_art@lineage-21.0:runtime/native/dalvik_system_DexFile.cc:385` "Writable dex file '%s' is not allowed."; **absent** in lineage-20.0 (13): `ENFORCE_READ_ONLY_JAVA_DCL` count = 0.

**What it affects:**
- **`DexClassLoader` / `PathClassLoader` (file-backed) created by an agent:** **AFFECTED** on targetSdk ≥ 34 — these go through `DexPathList.makeDexElements` → `loadDexFile` → `new DexFile(file,…)` → `openDexFileNative` (`GrapheneOS/platform_libcore@17:dalvik/src/main/java/dalvik/system/DexPathList.java:380-438`, `:214-233`; `DexFile.java` `openDexFileNative`). The dex/jar/apk file must be `chmod`ped read-only (no `W_OK`) first. **This is directly relevant: the capture-runtime dex the agent loads via a `DexClassLoader` must be read-only.**
- **`InMemoryDexClassLoader` (ByteBuffer):** **NOT affected** — it uses `DexFile_openInMemoryDexFilesNative` (`SRC/art:runtime/native/dalvik_system_DexFile.cc:253`), which has **no** `access(W_OK)` check (only `openDexFileNative` does, `:380`). (`InMemoryDexClassLoader` exists since API 26: `aosp-mirror/platform_frameworks_base@android-8.0.0_r1:api/current.txt:51973-51974`.) **So an in-memory loader sidesteps the read-only rule.**
- **JVMTI `AddToBootstrapClassLoaderSearch`:** **NOT affected** — opens with `ArtDexFileLoader` directly, never `openDexFileNative` (Q6). Good for the trampoline.
- **JVMTI `AddToDexClassLoader` (file):** **AFFECTED** — routes through `addDexPath`→`openDexFileNative`, so the segment file must be read-only on targetSdk ≥ 34.
- **JVMTI `AddToDexClassLoader...InMemory` (memfd):** likely **AFFECTED** (memfd path is `W_OK`, see Q6 UNVERIFIED). Prefer file-backed read-only, or bootstrap search.

**Design implication:** whatever file the agent writes for the capture runtime (dex/jar) and then loads via a file-backed loader on targetSdk ≥ 34 must be made read-only *before* loading (`File.setReadOnly()` / `chmod a-w`). Loading via `AddToBootstrapClassLoaderSearch`, or via `InMemoryDexClassLoader`, avoids the check entirely.

---

## Q10. run-as

**Constraints on current Android** (`GrapheneOS/platform_system_core@17:run-as/run-as.cpp`):
- **Debuggable-only:** `:258-261` `// Reject any non-debuggable package. if (!info.debuggable) { error(1, 0, "package not debuggable: %s", pkgname); }`. The `debuggable` flag comes from the packages list.
- **Caller must be shell or root:** `:211-213` `if (getuid() != AID_SHELL && getuid() != AID_ROOT) { error(1, 0, "only 'shell' or 'root' users can run this program"); }`.
- **Can be disabled by kernel cmdline:** `:217-219` `if (android::base::GetBoolProperty("ro.boot.disable_runas", false)) { error(1, 0, "run-as is disabled from the kernel commandline"); }` (e.g. Chrome OS non-dev mode).
- **`--user <uid>`:** `:226-230` `if ((argc >= 4) && !strcmp(argv[2], "--user")) { userId = atoi(argv[3]); … }`. Present since 8.0 (`aosp-mirror/platform_system_core@android-8.0.0_r1:run-as/run-as.cpp:147`). The default user is 0 unless `debug.run-as.use_current_user=true` (added 2025, current tree only: `:166-201` `defaultUser()`; upstream commit `6f6a354cb096`).
- **What it can read/do:** it drops to the app's uid/gid + shared-app gid, `chdir`s into `/data/user/<user>/<pkg>`, sets the app's SELinux context, and `execvp`s the command as the app. `:283-286` `std::string seinfo = std::string(info.seinfo) + ":fromRunAs"; if (selinux_android_setcontext(uid, 0, seinfo.c_str(), pkgname) < 0) …`. The `:fromRunAs` suffix (added in **Android 10**; `aosp-mirror/platform_system_core@android-10.0.0_r1:run-as/run-as.cpp:248-249`; absent in 8.0/9 which use `info.seinfo` unchanged, `android-8.0.0_r1:199`) maps to the dedicated **`runas_app`** SELinux domain via `seapp_contexts` `fromRunAs=true` (`GrapheneOS/platform_system_sepolicy@17:private/seapp_contexts:208-209`). Before Android 10, run-as commands ran in the app's normal domain (`untrusted_app`).

**Can `run-as <pkg> cp /data/local/tmp/<file> /data/data/<pkg>/code_cache/` work?** **Yes** — this is the standard mechanism, and both the framework and Android Studio rely on it:
- **Reading `shell_data_file` (i.e. `/data/local/tmp/*`) from an app/runas domain is explicitly allowed.** `GrapheneOS/platform_system_sepolicy@17:private/untrusted_app_all.te:57-58` `allow untrusted_app_all shell_data_file:file r_file_perms; allow untrusted_app_all shell_data_file:dir r_dir_perms;` with the comment (`:51-56`) "Used by: … Android Studio when attaching a profiler to an app … TODO: Long term, we don't want apps probing into shell data files." `runas_app` inherits this (`private/runas_app.te:4` `untrusted_app_domain(runas_app)` → adds `untrusted_app_all`). Same rule since Android 8.0 (`LineageOS/android_system_sepolicy@lineage-15.0:private/untrusted_app_all.te` allows `shell_data_file:file r_file_perms`).
- **Writing into `code_cache` (app's own `app_data_file`)** is allowed for the app domain (it owns the dir).
- The `cp` reads a `shell_data_file` (`r_file_perms`, allowed) and writes an `app_data_file` (allowed) — no SELinux denial. This is precisely what Studio does: `kroune/platform-tools-base@…:transport/native/utils/bash_command.android.cc:31-56` `RunAs` builds `run-as <pkg> --user <u> sh -c '<cmd>'`, and `daemon.cc:100-107` runs `cp <daemondir>/<agent> ./code_cache/` under that. (`kRunAsExecutable = "/system/bin/run-as"`, `kRunAsUserFlag = "--user"`, `bash_command.h:25,27`.)
- **Note:** the .so must then be *executed* from the app data dir, not from `/data/local/tmp` — because `neverallow { domain -shell } shell_data_file:file no_x_file_perms;` (`GrapheneOS/platform_system_sepolicy@17:private/shell.te:671-675`, comment "Restrict execute from /data/local/tmp directories … only the shell user should be able to execute such content."). `no_x_file_perms = { execute execute_no_trans }`. This neverallow is present at least since Android 16 (lineage-23.2:659, lineage-24.0:675) but I could **not** find it in lineage-15.1…22.2 (grep = none) — however the positive allow was always narrow: apps get only `r_file_perms` (no execute) on `shell_data_file` in every version checked (8.0→17), where `r_file_perms` does **not** include execute (`LineageOS/android_system_sepolicy@lineage-15.1:public/global_macros:22` `define(r_file_perms, { getattr open read ioctl lock map })`). So loading an app-process .so directly from `/data/local/tmp` has effectively never been permitted for app domains → the copy-into-code_cache step is mandatory on all versions. Studio itself notes (Android P) that on non-user builds it uses `su root` instead: `bash_command.android.cc:42-44` "Since Android Pie (API 28), JVMTI agent can be attached to non-debuggable apps. Therefore, we use "su root" on non-user-build devices."
- **Init sets** `/data/local/tmp` = `0771 shell shell` (`GrapheneOS/platform_system_core@17:rootdir/init.rc:862`), so `adb push` lands there as shell-owned, readable by the app after the copy.

---

## Q11. 16 KB page size

All from developer.android.com/guide/practices/page-sizes and bionic sources.

- **First supported:** "Beginning with Android 15, AOSP supports devices that are configured to use a page size of 16 KB." **Google Play requirement:** "all apps targeting Android 15 (API level 35) and higher must support 16 KB memory page sizes on 64-bit devices on Google Play. Starting February 1, 2027, if your app updates don't support 16 KB memory page sizes, you won't be able to release these updates." (The Nov 1, 2025 date is **not** on this page — UNVERIFIED there.)
- **ELF requirement:** "16 KB devices require the shared libraries' ELF segments to be aligned properly using 16 KB ELF alignment in order for your app to run." LOAD segment `p_align` must be ≥ `2**14` (16384).
- **NDK defaults:** "NDK version r28 and higher compile 16 KB-aligned by default." For **r27 and lower**: `-Wl,-z,max-page-size=16384 -Wl,-z,common-page-size=16384`. (The page does **not** mention `APP_SUPPORT_FLEXIBLE_PAGE_SIZES` / `ANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES` — UNVERIFIED whether those flags apply.) Uncompressed .so packaging needs **AGP 8.5.1+**: "16 KB devices require apps that ship with uncompressed shared libraries to align them on a 16 KB zip-aligned boundary. To do this, you need to upgrade to Android Gradle Plugin (AGP) version 8.5.1 or higher."
- **Checking alignment:** `check_elf_alignment.sh APK_NAME.apk`; `llvm-objdump -p <so> | grep LOAD` (ensure LOAD values ≥ `2**14`); `zipalign -v -c -P 16 4 APK_NAME.apk` ("Verification successful").
- **PAGE_SIZE:** "Remove any hard-coded dependencies that reference the `PAGE_SIZE` constant or instances in your code logic that assume that a device's page size is 4 KB (`4096`). … Use `getpagesize()` or `sysconf(_SC_PAGESIZE)` instead." "`PAGE_SIZE` is undefined when 16 KB mode is enabled on NDK r27 and higher."
- **What happens to a 4 KB-aligned .so on a 16 KB device — verified in bionic:** without app-compat, `dlopen` **fails**. `GrapheneOS/platform_bionic@17:linker/linker_phdr.cpp:993-1008` `// Only enforce this on 16 KB systems with app compat disabled. if (kPageSize >= 16384 && min_align_ < kPageSize && !should_use_16kib_app_compat_) { … DL_ERR_AND_LOG("\"%s\" program alignment (%zu) cannot be smaller than system page size (%zu)", …); if (dlopen_16kib_err_is_fatal_) { android_set_abort_message(err_msg.c_str()); inline_raise(SIGABRT); } return false; }`. **16 KB backcompat mode** (Android 16+ on 16 KB kernels) can load 4 KB-aligned .so: gated by property `bionic.linker.16kb.app_compat.enabled` and per-app `android:pageSizeCompat` (`linker_phdr.cpp:191-206`). Docs: "16 KB backcompat mode allows some apps to work, but for best reliability and stability, apps should still be 16 KB aligned," and the app "displays a warning when it's first launched."
- **Is a JVMTI agent .so loaded via attach-agent affected?** **YES.** The agent .so is loaded with `android::OpenNativeLibrary` (`SRC/art:runtime/ti/agent.cc:122`) → bionic linker → `ElfReader::LoadSegments` (the exact code above). So **traffic-police's JVMTI agent .so must be built 16 KB-aligned** (NDK r28 default, or `-Wl,-z,max-page-size=16384 -Wl,-z,common-page-size=16384` on r27/r26) to load on 16 KB-page Android 15+ devices; otherwise `dlopen` fails (agent attach → IOException) unless the *target app* happens to be in backcompat mode (which is the app's setting, not the agent's, and unreliable). This is a hard requirement independent of the app.

---

## Q12. Local sockets (abstract namespace, peer credentials, forwarding)

**`new LocalServerSocket(String name)` binds in the abstract namespace — confirmed.**
- `GrapheneOS/platform_frameworks_base@17:core/java/android/net/LocalServerSocket.java:33-49`: Javadoc "On the Android platform, the name is created in the Linux abstract namespace (instead of on the filesystem)." Constructor: `localAddress = new LocalSocketAddress(name); impl.bind(localAddress);`.
- `LocalSocketAddress(String name)` defaults to ABSTRACT: `core/java/android/net/LocalSocketAddress.java:79-80` `public LocalSocketAddress(String name) { this(name,Namespace.ABSTRACT); }`.
- Native bind builds an abstract sockaddr_un (leading NUL, name not NUL-terminated): `GrapheneOS/platform_system_core@17:libcutils/socket_local_client_unix.cpp` `socket_make_sockaddr_un` for `ANDROID_SOCKET_NAMESPACE_ABSTRACT`: "the path in this case is *not* supposed to be '\0'-terminated" … `p_addr->sun_path[0] = 0; memcpy(p_addr->sun_path + 1, name, namelen);`. (Java `LocalSocketImpl.bindLocal` → native `socket_bind_local` → `socket_local_server_bind`, `GrapheneOS/platform_frameworks_base@17:core/jni/android_net_LocalSocketImpl.cpp`.)

**`getPeerCredentials()` / `Credentials.getUid()` are public SDK APIs (not hidden) — confirmed.**
- `GrapheneOS/platform_frameworks_base@17:core/api/current.txt:31133-31135` `public class LocalServerSocket implements java.io.Closeable { ctor public LocalServerSocket(String) throws java.io.IOException;` and `:31142,31154` `public class LocalSocket implements java.io.Closeable { … method public android.net.Credentials getPeerCredentials() throws java.io.IOException;` and `:31089-31093` `public class Credentials { ctor public Credentials(int, int, int); method public int getGid(); method public int getPid(); method public int getUid(); }`. Being in `core/api/current.txt` means they are public SDK. All three classes are also in android-8.0.0_r1 `api/current.txt` (`:25477,25498`), so public since **API 26** (and much earlier).
- Impl: `getPeerCredentials()` → native `getPeerCredentials_native` → `getsockopt(fd, SOL_SOCKET, SO_PEERCRED, …)` (`core/jni/android_net_LocalSocketImpl.cpp:399`). `SO_PEERCRED` returns the peer's kernel-verified uid/pid/gid at connect time. **This validates the design's plan to authorize only UID 2000 (shell) / 0 (root) peers.**

**Restrictions on apps creating abstract unix sockets / adbd connecting — SELinux (`GrapheneOS/platform_system_sepolicy@17`):**
- Apps may create/listen on unix stream sockets: `private/domain.te:30` `allow domain self:unix_stream_socket { create_stream_socket_perms connectto };` (`create_stream_socket_perms` includes `listen accept`). No SELinux type governs *abstract* socket names specifically (SELinux does not label abstract socket names; it governs the socket object and connectto). So an app freely creates an abstract-namespace `LocalServerSocket`.
- **adbd → app abstract socket is explicitly allowed:** `private/adbd.te:129` `allow adbd appdomain:unix_stream_socket connectto;` (comment `:126` "ndk-gdb invokes adb forward to forward the gdbserver socket."). Present since 8.0 (`LineageOS/android_system_sepolicy@lineage-15.0:private/adbd.te:97`). So when `adb forward tcp:N localabstract:<name>` fires, **adbd itself** connects to the app's abstract socket (`SRC/adb:services.cpp:88-91` `if (is_socket_spec(name)) { … socket_spec_connect(&ret, name, …)` and `socket_spec.cpp:78` `{ "localabstract", { ANDROID_SOCKET_NAMESPACE_ABSTRACT, ADB_LINUX } }`). adbd runs as the `shell` UID after dropping privileges (`SRC/adb:daemon/main.cpp:141-142` `minijail_change_gid(jail.get(), AID_SHELL); minijail_change_uid(jail.get(), AID_SHELL);`), so `getPeerCredentials().getUid()` on the app side sees **uid 2000** — exactly what the design filters on.
- The reverse (app → runas_app abstract socket) is the Instant-Run pattern, also permitted: `private/untrusted_app_all.te:101-104` "Android Studio Instant Run has the application connect to a runas_app socket listening in the abstract namespace. https://developer.android.com/studio/run/ … allow untrusted_app_all runas_app:unix_stream_socket connectto;".

**Evidence `adb forward tcp:N localabstract:<name>` to an app socket works (real tools):**
- **Chrome/WebView DevTools:** `chromium/chromium@c6b97f110209:android_webview/browser/aw_devtools_server.cc:32,52-54` `kSocketNameFormat[] = "webview_devtools_remote_%d"` bound with `new net::UnixDomainServerSocket(..., true /* use_abstract_namespace */)`. Its peer check mirrors the design: `content/browser/android/devtools_auth.cc:18-35` `CanUserConnectToDevTools`: allows only when peer's `pw_name` is `"root"` or `"shell"`, or same uid. And the DevTools client uses `localabstract:`: `chrome/browser/devtools/device/adb/adb_device_provider.cc:18` `kLocalAbstractCommand[] = "localabstract:%s";`.
- **Stetho:** `facebook/stetho@2198797c0ff9:.../server/AddressNameHelper.java:13-18` prefixes `stetho_` + process name (abstract), and `SecureSocketHandler.java:41-51` checks `peer.getPeerCredentials()` and requires the peer hold `Manifest.permission.DUMP` (held by shell). This confirms the whole pattern (app LocalServerSocket + adb forward + peer-cred authorization) is a long-standing, working technique.

---

## Q13. `/proc/net/unix` from adb shell

**Readable by the `shell` domain on Android 10+ — confirmed.** `/proc/net` is labelled `proc_net`, `/proc/net/unix` inherits it (there is no more-specific genfs label for `unix`), and shell has read on the whole `proc_net_type` attribute.
- Label: `GrapheneOS/platform_system_sepolicy@17:private/genfs_contexts:29` `genfscon proc /net u:object_r:proc_net:s0` (no `/net/unix` entry; grep for `/net/unix` = 0 in all versions checked). `proc_net` carries the `proc_net_type` attribute: `public/file.te:59` `type proc_net, fs_type, proc_type, proc_net_type;`.
- Shell read: `private/shell.te:411-412` `# allow shell to look through /proc/ for lsmod, ps, top, netstat, vmstat.` `r_dir_file(shell, proc_net_type)`. This grants `/proc/net/unix` read. (Note: `/proc/net/tcp|udp` were separated into `proc_net_tcp_udp` and restricted, but `unix` stays `proc_net`, which shell can read.)
- **History:** shell has had `r_dir_file(shell, proc_net…)` since 8.0: `LineageOS/android_system_sepolicy@lineage-15.0:public/shell.te:96` `r_dir_file(shell, proc_net)`; lineage-16.0:121 `r_dir_file(shell, proc_net)`; the split to `proc_net_type` happened by lineage-17.1 (`public/shell.te:134` `r_dir_file(shell, proc_net_type)`). So `/proc/net/unix` is readable from `adb shell` on **8.0 through 17**. (Apps, by contrast, are increasingly denied `proc_net` — `private/app.te:1-20` negates most app domains — but that does not affect the host reading it over `adb shell`.)

**Line format** (kernel `unix_seq_show`, `torvalds/linux@v6.6:net/unix/af_unix.c:3261-3305`):
- Header: `:3265` `seq_puts(seq, "Num       RefCount Protocol Flags    Type St " "Inode Path\n");`
- Row: `seq_printf(seq, "%pK: %08X %08X %08X %04X %02X %5lu", …)` = kernel ptr, refcount, protocol(0), flags, type, state, inode, then the path.
- **Abstract names appear with a leading `@`:** `:3289-3296` when `u->addr->name->sun_path[0]` is 0 (abstract), it emits `seq_putc(seq, '@'); i++;` and then substitutes embedded NULs with `@` (`sun_path[i] ?: '@'`). Flags field `00010000` = `__SO_ACCEPTCON` (`include/uapi/linux/net.h:56` `#define __SO_ACCEPTCON (1 << 16)`) = a listening socket; St `01` = `SS_UNCONNECTED` (`net.h:49-53`). Type `0001` = `SOCK_STREAM`.
- Real parsing evidence: `chromium/chromium@c6b97f110209:chrome/browser/devtools/device/android_device_info_query.cc:29,150-176` runs `cat /proc/net/unix` and parses "records with paths starting from '@' (abstract socket)" — it filters `fields[3] != "00010000" || fields[5] != "01"` (listening + unconnected) and `path_field[0] != '@'`, then strips the `@` (`path_field.substr(1)`). **This is exactly the host-side discovery traffic-police plans; the field indices (flags=field[3]=`00010000`, state=field[5]=`01`, path=field[7]) and the `@` prefix are confirmed.**

---

## Design implications for traffic-police

### API-level matrix (which attach/launch mechanisms work)

Assumes the standard traffic-police case: a **user-build device** with a **`android:debuggable=true`** target app, host driving via `adb shell`/`cmd`. "✓" = supported by source; caveats noted.

| Mechanism | 26 | 27 | 28 | 29 | 30 | 31–33 | 34 | 35 | 36 | 37 |
|---|---|---|---|---|---|---|---|---|---|---|
| `cmd activity attach-agent <pid/name> <so=opts>` (running app) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `am start --attach-agent <so>` (before bind) | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `am start --attach-agent-bind <so>` (during bind; see double-attach note) | ✗ | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `code_cache/startup_agents/` (capture from launch, debuggable only) | ✗ | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `Debug.attachJvmtiAgent` (self-attach, in-process only) | ✗ | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| JVMTI RetransformClasses on boot+app classes (debuggable app) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `AddToBootstrapClassLoaderSearch` (live) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `AddToDexClassLoader` extension (inject into app loader) | ✗ | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `run-as <pkg> cp /data/local/tmp → code_cache/` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| App `LocalServerSocket` abstract + `adb forward localabstract:` + peer-cred filter | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `/proc/net/unix` readable from `adb shell` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |

Additional constraints layered on top:
- **Read-only DCL** (targetSdk ≥ 34, i.e. affects apps built for API 34+ regardless of device): any file-backed dex/jar the agent loads must be read-only before load. Applies on API 34, 35, 36, 37 rows for file-backed loaders and `AddToDexClassLoader`.
- **16 KB alignment** (device page size, Android 15+ / API 35+): agent .so must be 16 KB-ELF-aligned or `dlopen` fails on 16 KB devices. Affects the .so on API 35+ devices configured for 16 KB.
- **ART module (API 31+):** retransform/extension behaviour tracks the updatable ART APEX version, which can be newer than the OS image; treat API 31+ ART features as "≥ this version" not "exactly this OS".

### Concrete recommendations

1. **Require `android:debuggable=true`.** Every gate (AMS `enforceDebuggable`, ART `IsJdwpAllowed`, full-JVMTI `IsJavaDebuggableAtInit`, `run-as`, startup_agents `isDebuggable()`) requires it on user builds. `profileable` is **not** enough for agents.
2. **Attach mode:** use `cmd activity attach-agent <pid> <code_cache/agent.so=opts>` — works API 26→37, unchanged. Discover the pid from `/proc` (or `pidof`). The `so` path must be a single quoted arg; options go after the first `=`.
3. **Deliver the .so via `run-as`** into `/data/data/<pkg>/code_cache/` (never load from `/data/local/tmp` — `no_x_file_perms` blocks app-domain execute there on every version). `adb push` to `/data/local/tmp`, then `run-as <pkg> cp … code_cache/` (SELinux allows app read of `shell_data_file` + write of own `app_data_file`). Prefer copying into `code_cache/` (not a subdir) for `attach-agent`; use `code_cache/startup_agents/` only for launch-time capture.
4. **Capture from launch:**
   - API 30+: drop the .so into `code_cache/startup_agents/` (persists across launches; every file in the dir is attached with the data dir as its options; boot class loader). Clean out stale agents yourself (no naming rules).
   - API 27–29: use `am start --attach-agent <code_cache/agent.so>` (before-bind). Prefer plain `--attach-agent` over `--attach-agent-bind` (the latter's code path also fires the pre-bind attach → potential double `Agent_OnAttach`).
   - API 26: no launch-time attach — only post-start `attach-agent` (attach right after the process appears; race the app's first request).
5. **Class loading inside the agent:**
   - Trampoline (bootstrap): `AddToBootstrapClassLoaderSearch(<dex/jar>)` — works all versions, bypasses the read-only DCL check. Best home for the hook trampoline referenced by rewritten boot classes.
   - Capture runtime (child of app loader): create a `DexClassLoader`/`PathClassLoader` with the app loader as parent, **or** use the `AddToDexClassLoader` extension (API 30+) on the app loader. On targetSdk ≥ 34, **make the dex/jar file read-only before loading** (or use `InMemoryDexClassLoader`, which is exempt).
   - Do **not** use `AddToSystemClassLoaderSearch` for app-visible classes — ART's "system" loader is the zygote loader, invisible to the app's PathClassLoader.
6. **Find the app class loader** via `GetThreadInfo(main thread).context_class_loader` (works 26→37; matches Studio), with fallback to JNI `ActivityThread.currentApplication().mLoadedApk.getClassLoader()`. Do not rely on the loader passed to attach — ART discards it for `Agent_OnAttach` (passes `nullptr`); it only affects the .so's native search path.
7. **Build the agent .so 16 KB-aligned** (NDK r28+ default, or r27/r26 with `-Wl,-z,max-page-size=16384 -Wl,-z,common-page-size=16384`) and per-ABI matching the app process (native-bridge agents are refused).
8. **Device socket:** app-side `new LocalServerSocket(name)` (abstract), authorize peers via `getPeerCredentials().getUid() ∈ {0, 2000}`. Host reaches it with `adb forward tcp:0 localabstract:<name>` (adbd connects as uid 2000 → passes the filter). Discover sockets by parsing `cat /proc/net/unix` over `adb shell`: listening abstract sockets have flags `00010000`, state `01`, and a path starting `@`. All confirmed working (WebView DevTools, Stetho use exactly this).
9. **Make `Agent_OnAttach` idempotent** — re-attach (same path) re-invokes it, and `--attach-agent-bind` may invoke twice.

### Risks

- **User-build reachability:** on a production (user) build, only debuggable apps are attachable at all. traffic-police cannot inspect arbitrary release apps without a debuggable build or a userdebug/eng device.
- **targetSdk ≥ 34 read-only DCL:** a writable capture-runtime file → `SecurityException "Writable dex file … is not allowed"`. Mitigate with `setReadOnly()`/`chmod a-w` before load, or `InMemoryDexClassLoader` / bootstrap search.
- **16 KB devices (Android 15+):** a 4 KB-aligned agent .so fails to `dlopen` (attach → IOException) unless the target app is in backcompat mode (unreliable, app-controlled). Ship 16 KB-aligned .so per ABI.
- **`--attach-agent-bind` double attach** (code path since API 28): prefer `--attach-agent`.
- **startup_agents boot-loader context:** startup agents are attached with the boot class loader and receive only the data dir as options — the agent must still discover the app loader itself (as in attach mode).
- **ART is an APEX from API 31:** retransform/extension edge cases may vary by ART module version independent of the OS; test against updated ART, not just the base image.
- **Compact-dex / boot-jar hidden-API dex in the CFLH buffer:** the buffer may contain the whole containing dex; always emit a single-class dex from slicer or ART rejects it.
- **Structural limits:** slicer must change **method bodies only** — no added/removed methods/fields, no modifier/hierarchy changes, exactly one class_def, must pass verification.
- **`/data/local/tmp` execute ban:** never load the app-process .so directly from `/data/local/tmp`; always copy into the app's `code_cache/` first.
