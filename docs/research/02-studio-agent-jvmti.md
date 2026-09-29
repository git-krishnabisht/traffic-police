# 02 — Android Studio's attach agent (JVMTI + slicer), verified from source

Scope: how Android Studio's App Inspection / Network Inspector attaches a JVMTI agent to a debuggable app, rewrites dex with slicer, dispatches hooks, loads inspector code, and how the host drives it. Every claim below was read in source or official docs during this task. Items that could not be verified are marked **UNVERIFIED**.

## Source legend (all citations use `alias:path:line` + a short verbatim quote)

| Alias | Repository @ ref | Notes |
|---|---|---|
| `tools-base@11ff885` | github.com/kroune/platform-tools-base @ `11ff88561aa7` (studio-main) | Commit message: "Squashed snapshot of AOSP platform/tools/base studio-main @ 0227ef5200f8…", 2026-08-30 |
| `androidx@fc135bf` | github.com/androidx/androidx @ `fc135bf680e6` (androidx-main) | 2026-09-29 |
| `dexter@main` | AOSP platform/tools/dexter @ main (slicer sources, local copy) | no commit hash recorded |
| `art@18ea424` | github.com/LineageOS/android_art @ `18ea424e1026` (lineage-23.2) | Lineage mirror of AOSP art, 2026-08-19 |
| `fwb@9231092` | github.com/GrapheneOS/platform_frameworks_base @ `92310923b24c` (branch 17) | 2026-09-28 |
| `aosp-fwb@<tag>` | github.com/aosp-mirror/platform_frameworks_base @ tags android-8.0.0_r1 … android-11.0.0_r1 | fetched via raw.githubusercontent.com |
| `studio-ide@0867bfe` | github.com/JetBrains/android @ `0867bfe2b625` (Studio IDE plugin, host side) | 2026-09-28 |
| `okhttp@40a3b87` | github.com/square/okhttp @ `40a3b8749dea` (`parent-5.5.0-41`) | plus `javap` of Maven Central okhttp 4.12.0 and 3.14.9 jars |
| JVMTI spec | https://docs.oracle.com/javase/8/docs/platform/jvmti/jvmti.html | |
| ART TI doc | https://source.android.com/docs/core/runtime/art-ti | |
| A14 doc | https://developer.android.com/about/versions/14/behavior-changes-14 | |

## TL;DR

* **Mechanism.** `ArtTooling.registerExitHook(Class, "name(sig)ret", hook)` → JNI → native `AppInspectionService::AddTransform` → records a per-class transform, then **`RetransformClasses(1, &clazz)`**. The env's **ClassFileLoadHook** receives **dex bytes**, slicer rewrites the one class, and the result is returned as `new_class_data`. Exit hooks use `slicer::ExitHook` with `ReturnAsObject | PassMethodSignature` (reference returns) or `PassMethodSignature` (primitive/void). Entry hooks use Studio's fork of slicer's ArrayParams entry hook.
* **Callback.** The instrumented bytecode calls `static` methods on `com.android.tools.agent.app.inspection.AppInspectionService`: `onExit(String,Object)Object`, `onExit(String,<prim>)<prim>`, `onExit(String)V`, and `onEntry(Object[])V`. That class is in **`perfa.jar`**, which `Agent_OnAttach` (live phase) appends with **`AddToBootstrapClassLoaderSearch`**. The jar sits next to the agent `.so` in the app's `code_cache`. Being on the boot class path, the callback resolves from boot classes (`java.net.URL`) and from app classes alike.
* **Dispatch** is keyed by a label string: `"<dotted.Class>-><method><descriptor>"`. It is **class-loader-agnostic**. Exit hooks chain, and the final value is returned; for reference returns slicer inserts `move-result-object` + `check-cast <declared type>`.
* **Late loads.** The API needs a `Class<?>`, so the target is loaded when the hook is registered. On API ≥ 28 the ClassFileLoadHook stays enabled, so **any later definition with the same name (any loader) is also transformed**. On API 26–27 it is enabled only around the explicit retransform, so later loads are not transformed.
* **Inspector code** is loaded by a `DexClassLoader(dexPath=/data/local/tmp/perfd/<inspector>.jar, parent = app class loader)`. The app loader is found through a JVMTI heap walk for `android.app.Application` instances followed by the public `getClassLoader()`, falling back to the main looper thread's context class loader. Kotlin/coroutines are bundled and renamed by jarjar; OkHttp/okio/grpc are *not* bundled, so they resolve to the app's copies.
* **Host flow.**
  1. Push to `/data/local/tmp/perfd/`: agent `libjvmtiagent_<cpuArch>.so` (mode 755), jars (**`chmod 444`**, explicitly for the API 34 writable-dex check), and configs.
  2. The daemon, running as shell, runs `run-as <pkg> --user <u> sh -c 'cp /data/local/tmp/perfd/<f> ./code_cache/'`.
  3. The daemon runs `cmd activity attach-agent <process> <dataDir>/code_cache/libjvmtiagent_<arch>.so=/data/local/tmp/perfd/agent.config`.

  App Inspection has **no launch-time attach**. The Profiler uses `am start --attach-agent …` (API ≥ 27). Apply Changes uses `code_cache/startup_agents/` (framework support since API 30).
* **Stop inspecting** removes the Java-side handlers only. Bytecode stays instrumented and agents are never unloaded. **Failures** (method not found, retransform errors) go to logcat and are **not** reported to the IDE.
* **Minimum API 26 (O)** is enforced in the IDE UI, the daemon, the androidx.inspection `minSdk`, and the network-inspector `d8 --min-api`. **Capabilities:** Studio adds *all potential* JVMTI capabilities.

---

## 1. ArtTooling hooks end to end

### 1.1 API contract (androidx.inspection)
* `androidx@fc135bf:inspection/inspection/src/main/java/androidx/inspection/ArtTooling.java:59-63`: "`{@code originMethod} should be in the format: "methodName(signature)", where signature is JAVA VM's format (the one that JNI uses)`… `{@code bla(LpackageOfBar/Bar;)LpackageOfFoo/Foo;}`"
* The exit hook can replace the return value. `ArtTooling.java:79-85`: "`Called inline at the exit of an instrumented method and allows to intercept a returned value`… `@return an object that should be returned instead by origin method.` … `T onExit(T result);`"
* Entry hook: `ArtTooling.java:51`: "`void onEntry(@Nullable Object thisObject, @NonNull List<Object> args);`"

### 1.2 Java side in the app (Studio's implementation)
* `ArtToolingImpl` delegates to static methods on `AppInspectionService` (`tools-base@11ff885:app-inspection/agent/src/main/java/com/android/tools/agent/app/inspection/ArtToolingImpl.java:44,52`: "`AppInspectionService.addEntryHook(inspectorId, originClass, originMethod, entryHook);`" / "`AppInspectionService.addExitHook(...)`").
* The native transform is requested **once per label**, then the handler is appended. `AppInspectionService.java:318-333`:
  ```java
  sInstance.mExitTransforms.computeIfAbsent(createLabel(origin, method), … {
      synchronized (lock) { nativeRegisterExitHook(sInstance.mNativePtr, origin, method); }
      return new CopyOnWriteArrayList<>(); });
  hooks.add(new HookInfo<>(inspectorId, hook));
  ```
  The label is built at `AppInspectionService.java:296-298`: "`return origin.getName() + "->" + method;`". The lock is at `:53-54`: "`// Lock to prevent race condition when registering hooks. See b/376717110.`"
* JNI glue splits name and signature at the first `(` and logs (it does not throw) when the format is wrong. `tools-base@11ff885:app-inspection/native/src/app_inspection_java_jni.cc:338-355`: "`std::size_t found = method_str.get().find("(");` … `Log::E(... "Method should be in the format $method_name($signature)$return_type, but was %s"` … `inspector->AddExitTransform(env, origin_class, method_str.get().substr(0, found), method_str.get().substr(found));`"

### 1.3 JVMTI environment and functions used
* **Separate env per service.** `tools-base@11ff885:app-inspection/native/src/app_inspection_service.cc:65-67`: "`// Create a stand-alone jvmtiEnv to avoid any callback conflicts with other profilers' agents.` `jvmtiEnv* jvmti = CreateJvmtiEnv(vm);`"
* `GetEnv` version. `tools-base@11ff885:transport/native/jvmti/jvmti_helper.cc:30-38`: "`jint jvmti_flag = JVMTI_VERSION_1_2;` … `// On non-user-build devices ... we use flag |kArtTiVersion| ... to support non-debuggable apps. The flag was introduced in Android P (API 28).` `jvmti_flag = JVMTI_VERSION_1_2 | 0x40000000;`"
* Capabilities: `GetPotentialCapabilities` + `AddCapabilities` (`jvmti_helper.cc:61-67`, see §8).
* `SetEventCallbacks(ClassFileLoadHook)` and `SetEventNotificationMode`. `app_inspection_service.cc:256-274`: "`callbacks.ClassFileLoadHook = OnClassFileLoaded;` … `bool filter_class_load_hook = DeviceInfo::feature_level() < DeviceInfo::P;` `SetEventNotification(jvmti_, filter_class_load_hook ? JVMTI_DISABLE : JVMTI_ENABLE, JVMTI_EVENT_CLASS_FILE_LOAD_HOOK);`"
* `GetCurrentThread` + `RetransformClasses`. `app_inspection_service.cc:295-311`: "`jvmti_->GetCurrentThread(&thread);` … `if (manually_toggle_load_hook) { … SetEventNotificationMode(JVMTI_ENABLE, JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, thread)); }` `CheckJvmtiError(jvmti_, jvmti_->RetransformClasses(1, &origin_class));`"
* `Allocate` for the new class bytes (`app_inspection_service.cc:194-206`, `JvmtiAllocator` → `profiler::Allocate(jvmti_env_, size)`).
* `GetExtensionFunctions` for ART's hidden-API policy extensions, used during `AddTransform` (`app_inspection_service.cc:281`: "`HiddenApiSilencer silencer(jvmti_);`"; the ids are at `tools-base@11ff885:transport/native/jvmti/hidden_api_silencer.cc:41-50`: "`com.android.art.misc.get_hidden_api_enforcement_policy`" … "`disable_hidden_api_enforcement_policy`"). **UNVERIFIED:** why it is needed during retransformation; no comment explains it.
* For `findInstances` (see §1.8): `GetLoadedClasses`, `IterateThroughHeap` (<Q), `IterateOverInstancesOfClass` (Q+), `GetObjectsWithTags`, `Deallocate`.
* The transport agent uses `AddToBootstrapClassLoaderSearch` (§1.5).

### 1.4 Retransform → ClassFileLoadHook (dex bytes) → slicer
* The hook receives a **dex image**. It is looked up by descriptor, the class is rewritten, and a new single-class dex image is returned. `app_inspection_service.cc:225-253`:
  ```cpp
  // The tooling interface will specify class names like "java/net/URL"
  // however, in .dex these classes are stored using the "Ljava/net/URL;" format.
  std::string desc = "L" + std::string(name) + ";";
  … auto transform = class_transforms->find(desc);
  if (transform == class_transforms->end()) return;
  dex::Reader reader(class_data, class_data_len);
  auto class_index = reader.FindClassIndex(desc.c_str());
  … reader.CreateClassIr(class_index); auto dex_ir = reader.GetIr();
  transform->second->Apply(dex_ir);
  … dex::Writer writer(dex_ir); new_image = writer.CreateImage(&allocator, &new_image_size);
  *new_class_data_len = new_image_size; *new_class_data = new_image;
  ```
  Note that the `loader` parameter is **ignored**; matching is by name only.
* ART confirms the bytes are dex. On retransform it starts from the class's **original** dex. `art@18ea424:openjdkjvmti/ti_class_definition.cc:96-99,141-142`: "`art::ObjPtr<art::mirror::Object> orig_dex(ext->GetOriginalDexFile());` … `// No redefinition must have ever happened so we can use the class's dex file.` `return Init(m_klass->GetDexFile());`". Also `:173-177`: "`dex_data_ = art::ArrayRef<const unsigned char>(dex_file.Begin(), dex_file.SizeIncludingSharedData());`". JVMTI spec (RetransformClasses): "This function reruns the transformation process (whether or not a transformation has previously occurred)… starting from the initial class file bytes". Therefore Studio re-applies **all** transforms recorded for the class on every retransform (next bullet).
* Per-class transform list and slicer usage. `tools-base@11ff885:app-inspection/native/include/app_inspection_transform.h:45-71`:
  ```cpp
  for (auto transform : transforms) {
    slicer::MethodInstrumenter mi(dex_ir);
    if (transform.isEntry()) {
      mi.AddTransformation<ArrayParamsEntryHook>(ir::MethodId(
          "Lcom/android/tools/agent/app/inspection/AppInspectionService;", "onEntry"));
    } else {
      auto tweak = transform.HasPrimitiveOrVoidReturnType()
                       ? slicer::ExitHook::Tweak::None
                       : slicer::ExitHook::Tweak::ReturnAsObject;
      tweak = tweak | slicer::ExitHook::Tweak::PassMethodSignature;
      mi.AddTransformation<slicer::ExitHook>(ir::MethodId(
          "Lcom/android/tools/agent/app/inspection/AppInspectionService;", "onExit"), tweak);
    }
    if (!mi.InstrumentMethod(ir::MethodId(transform.GetClassName(), transform.GetMethod(), transform.GetSignature()))) {
      profiler::Log::E(profiler::Log::Tag::APPINSPECT, "Error instrumenting %s %s->%s%s\n", …);
  ```
  Arrays count as references. `app_inspection_transform.h:93-99`: "`// SECURITY: Correctly identify object arrays as non-primitive…` `return ret != 'L' && ret != '[';`"
* Entry hooks use Studio's own `ArrayParamsEntryHook` (`tools-base@11ff885:app-inspection/native/src/array_params_entry_hook.cc`), a fork of slicer's `EntryHook::Tweak::ArrayParams`. Differences found:
  * Scratch registers are cleared to 0 instead of slicer's `0xFEFEFEFE`. `array_params_entry_hook.cc:296-304`: "`// SECURITY: Set registers to 0 instead of 0xFEFEFEFE. The GC scans reference registers, and assigning a non-zero, non-null value can cause the GC to crash`". Upstream: `dexter@main:slicer/instrumentation.cc:344-351` "`Const32>(0xFEFEFEFE)`".
  * The fork's `GenerateShiftParamsCode` asserts `SLICER_CHECK(ir_method->code->ins_count > 0);` (`array_params_entry_hook.cc:100`). Upstream returns early instead (`dexter@main:slicer/instrumentation.cc:173-177` "`if there are no parameters this is a no-op`"). *Inference (not tested):* an entry hook on a static no-arg method with fewer than 3 registers would `abort()` in Studio's fork.
* Other Studio agents use slicer the same way:
  * Apply Changes: `EntryHook` with `Tweak::ThisAsObject` plus a plain `ExitHook`. `tools-base@11ff885:deploy/agent/native/transform/hook_transform.cc:31-37`: "`mi.AddTransformation<slicer::EntryHook>(entry_hook, slicer::EntryHook::Tweak::ThisAsObject);` … `mi.AddTransformation<slicer::ExitHook>(exit_hook);`".
  * The profiler hooks app classes such as `androidx.fragment.app.Fragment` with a boot-classpath callback taking `Object` (`tools-base@11ff885:profiler/native/perfa/transform/android_fragment_transform.h:32-37`: "`FragmentWrapper;", "wrapOnResume"), slicer::EntryHook::Tweak::ThisAsObject`"; callback `tools-base@11ff885:profiler/app/common/src/main/java/com/android/tools/profiler/support/event/FragmentWrapper.java:30`: "`public static void wrapOnResume(Object fragment) {`").

### 1.5 Where the callback class lives and why every loader can see it
* `perfa.jar` is a dex jar that contains the app-inspection agent Java code and androidx.inspection 1.0.0. `tools-base@11ff885:profiler/app/BUILD:82-89`:
  ```
  dex_library(name = "perfa", aars = ["//prebuilts/tools/common/m2:androidx.inspection.inspection.1.0.0"],
      jars = [":perfa_java", "//tools/base/app-inspection/agent"],)
  ```
  (`app-inspection/agent/BUILD:5-8` globs `src/main/java/**/*.java`, which includes `AppInspectionService`.)
* It is appended to the **bootstrap** class path from the **directory of the agent .so** (the app's `code_cache`) inside **`Agent_OnAttach`**, i.e. the live phase. `tools-base@11ff885:transport/native/agent/transport_agent.cc:38-58`:
  ```cpp
  static std::string GetAppDataCodeCachePath() { Dl_info dl_info; dladdr((void*)Agent_OnAttach, &dl_info); … }
  void LoadDex(jvmtiEnv* jvmti, JNIEnv* jni) {
    // Load perfa.jar which should be in /data/user/<USER>/<PACKAGE_NAME>.
    std::string agent_lib_path(GetAppDataCodeCachePath()); agent_lib_path.append("perfa.jar");
    jvmti->AddToBootstrapClassLoaderSearch(agent_lib_path.c_str()); }
  ```
  It is called at `transport_agent.cc:84-85`: "`JNIEnv* jni_env = GetThreadLocalJNI(vm);` `LoadDex(jvmti_env, jni_env);`".
* JVMTI spec (AddToBootstrapClassLoaderSearch): "This function can be used to cause instrumentation classes to be defined by the bootstrap class loader… In the live phase the segment may be used to specify any platform-dependent path to a JAR file. The agent should take care that the JAR file does not contain any classes or resources other than those to be defined by the bootstrap class loader for the purposes of instrumentation." It also warns that a reference that failed to resolve earlier "will fail with the same error as the initial attempt".
* ART implementation: opens the dex with `ArtDexFileLoader` and appends it to the boot class path, with **no writable-file check**. `art@18ea424:openjdkjvmti/ti_search.cc:236-249`: "`art::ArtDexFileLoader dex_file_loader(segment);` … `current->AddExtraBootDexFiles(segment, segment, std::move(dex_files));`"
* Hidden-API domain of such a dex. `art@18ea424:runtime/hidden_api.cc:229-235`: "`if (class_loader.IsNull()) { … LOG(WARNING) << "hiddenapi: DexFile " << dex_location << " is in boot class path but is not in a known location"; } return Domain::kPlatform;`"
* Visibility follows. The rewritten `java.net.URL` has a boot defining loader, and the appended boot dex resolves from it. App classes delegate parent-first to boot. This is the standard delegation model; Studio relies on it for `URL.openConnection()` (§3).
* JNI `native` methods of these boot-classpath classes bind to symbols exported by the agent `.so` without `System.loadLibrary`. `art@18ea424:runtime/jni/java_vm_ext.cc:1197-1201`: "`// Lookup JNI native methods from native TI Agent libraries… Agent libraries are searched for native methods after all jni libraries.` `native_method = FindCodeForNativeMethodInAgents(m);`". `libjvmtiagent.so` links the app-inspection JNI code (`tools-base@11ff885:transport/native/agent/BUILD:40-60`: "`name = "libjvmtiagent.so"` … `"//tools/base/app-inspection:jni",`").

### 1.6 Exact callback signatures emitted into the bytecode
* Entry, from the ArrayParams fork: hook proto `([Ljava/lang/Object;)V`, invoked with `invoke-static/range`. `array_params_entry_hook.cc:277-293`: "`hook_param_types.push_back(obj_array_type);` `auto ir_proto = builder.GetProto(builder.GetType("V"), …` `hook_invoke->opcode = dex::OP_INVOKE_STATIC_RANGE;`". The array is `[label, this|null, boxed args…]` (`:198-202` "`2 + param_types.size()));  // method signature + params + "this" object`"; `:272-274` "`// if function is static, then jumping over index 1`").
* Exit, from slicer `ExitHook::Apply`: hook proto = (`String` if PassMethodSignature) + (return type, or `Object` if ReturnAsObject), returning the same type. `dexter@main:slicer/instrumentation.cc:372-385`:
  ```cpp
  const auto return_type = return_as_object ? builder.GetType("Ljava/lang/Object;") : declared_return_type;
  … if (pass_method_signature) { param_types.push_back(builder.GetType("Ljava/lang/String;")); }
  if (!return_void) { param_types.push_back(return_type); }
  auto ir_proto = builder.GetProto(return_type, builder.GetTypeList(param_types));
  ```
* The matching Java targets in Studio. `tools-base@11ff885:app-inspection/agent/src/main/java/com/android/tools/agent/app/inspection/AppInspectionService.java:350-388`: "`public static Object onExit(String methodSignature, Object returnObject)`", "`public static void onExit(String methodSignature)`", "`public static boolean onExit(String methodSignature, boolean result)`" … byte/char/short/int/float/long/double overloads. `:406`: "`public static void onEntry(Object[] signatureThisParams)`".

| Target method return type | Tweaks | Emitted call |
|---|---|---|
| reference or array (e.g. `openConnection()Ljava/net/URLConnection;`) | `ReturnAsObject\|PassMethodSignature` | `invoke-static/range {label, ret}, AppInspectionService.onExit(Ljava/lang/String;Ljava/lang/Object;)Ljava/lang/Object;` + `move-result-object` + `check-cast <declared>` |
| `void` | `PassMethodSignature` | `onExit(Ljava/lang/String;)V` |
| primitive `I/J/Z/…` | `PassMethodSignature` | `onExit(Ljava/lang/String;I)I` etc. (wide values use a register pair) |
| entry (any) | ArrayParams fork | `onEntry([Ljava/lang/Object;)V` |

### 1.7 Dispatch and return-value flow
* The label string emitted by slicer is `Decl(class) + "->" + name + Signature()`. `dexter@main:slicer/instrumentation.cc:98-101`: "`return ir_method->decl->parent->Decl() + "->" + ir_method->decl->name->c_str() + signature_str;`". `Decl()` produces the dotted name (`dexter@main:slicer/dex_format.cc:62-64`: "`"Ljava/lang/String;" becomes "java.lang.String"`"), matching Java's `origin.getName() + "->" + method`.
  * The Javadoc on `onEntry` saying "the first parameter is the method signature" (`AppInspectionService.java:391-404`) is stale. The value is this full label, and it is what the lookup uses (`:409-411` "`AppInspectionService.instance().mEntryTransforms.get(signature);`").
* Exit dispatch chains every registered hook and returns the last value. **No try/catch**, so a hook exception propagates into the app's method. `AppInspectionService.java:336-348`:
  ```java
  private static <T> T onExitInternal(String label, T returnObject) {
      … List<HookInfo<ExitHook>> hooks = instance.mExitTransforms.get(label);
      if (hooks == null) { return returnObject; }
      for (HookInfo<ExitHook> info : hooks) { returnObject = (T) info.hook.onExit(returnObject); }
      return returnObject; }
  ```
  Primitive overloads auto-box through the generic `T` (`:374-376` "`public static int onExit(String methodSignature, int result) { return onExitInternal(methodSignature, result); }`").
* The value flows back into the instrumented method as follows. The hook's result is moved into the **original return register**, cast back for ReturnAsObject, and the original `return` then executes. `dexter@main:slicer/instrumentation.cc:504-517`:
  ```cpp
  if (move_result_opcode != dex::OP_NOP) {
    auto move_result = …; move_result->opcode = move_result_opcode;
    move_result->operands.push_back(bytecode->operands[0]);
    …
    if ((tweak_ & Tweak::ReturnAsObject) != 0) {
      check_cast->opcode = dex::OP_CHECK_CAST; … declared_return_type …
  ```
  An exit hook returning an object of the wrong type therefore throws `ClassCastException` inside the app method. This is an inference from the `check-cast`.

### 1.8 `findInstances`
* `java.lang.Class` is special-cased to `GetLoadedClasses`. Other types are tagged and then read back with `GetObjectsWithTags`. `app_inspection_service.cc:142-150,165-178`: "`if (jni->IsSameObject(clazz, jni->FindClass("java/lang/Class"))) { … GetLoadedClasses(&count, &classes)` … `bool error = DeviceInfo::feature_level() < DeviceInfo::Q ? tagClassInstancesO(jni, clazz, tag) : tagClassInstancesQ(clazz, tag);` … `jvmti_->GetObjectsWithTags(1, &tag, &count, &instances, NULL)`"
* On Q+ it uses `IterateOverInstancesOfClass(clazz, JVMTI_HEAP_OBJECT_EITHER, …)` (`:135-139`). Before Q it runs `IterateThroughHeap` per assignable loaded class (`:107-116` "`IterateThroughHeap doesn't include subclasses of the specfied class, so have to manually search for subclasses.`").

---

## 2. Late-loaded classes and multiple class loaders

* **The API requires a loaded `Class<?>`.** Registration retransforms only that `Class` object (`app_inspection_service.cc:306` "`RetransformClasses(1, &origin_class)`"). The network inspector forces the class to load by using a class literal resolved through its `DexClassLoader`, whose parent is the app loader, and treats absence as "no OkHttp". `tools-base@11ff885:app-inspection/inspectors/network/src/com/android/tools/appinspection/network/NetworkInspector.kt:269-284`: "`artTooling.registerExitHook(okhttp3.OkHttpClient::class.java, "networkInterceptors()Ljava/util/List;", …` … `} catch (e: NoClassDefFoundError) { // Ignore. App may not depend on OkHttp. }`". gRPC classes are loaded explicitly: `:335-341` "`javaClass.classLoader.loadClass(hook.className)` … `catch (e: ClassNotFoundException)`".
* **No ClassPrepare/ClassLoad events in app inspection.** The only callback set is ClassFileLoadHook (`app_inspection_service.cc:261`). The transform table is keyed by descriptor only (`:282-293`).
* **API ≥ 28:** CFLH is left enabled globally (`app_inspection_service.cc:266-274`, comment "`For P+ we want to keep the hook events always on to support multiple retransforming agents (and therefore don't need to perform retransformation on class prepare).`"). ART sends the retransformable CFLH on **first definition** of every class in START/LIVE phase. `art@18ea424:openjdkjvmti/ti_class.cc:161-172,192,209-210`: "`void ClassPreDefine(const char* descriptor, …` `bool is_enabled = event_handler->IsEventEnabledAnywhere(ArtJvmtiEvent::kClassFileLoadHookRetransformable) || …` `def.InitFirstLoad(descriptor, class_loader, initial_dex_file);` … `Transformer::CallClassFileLoadHooksSingleClass< ArtJvmtiEvent::kClassFileLoadHookRetransformable>(event_handler, self, &def);`". Consequence (inferred from the code above): a class with the same name defined **later by any loader** (for example a second `okhttp3.OkHttpClient` in a feature or plugin `DexClassLoader`) is instrumented at definition. Its calls dispatch to the **same label**, so the same hooks run, even though those hooks were written against the first loader's types.
* **API 26–27:** CFLH is disabled globally and enabled for the current thread only while `RetransformClasses` runs (`app_inspection_service.cc:298-311`). Later definitions, including same-named copies in other loaders, are **not** instrumented. Copies that were already loaded in other loaders are never retransformed on any API level, because only the passed `Class` is retransformed.
* The Studio **profiler** agent handles late loads by name. Pre-P it uses ClassPrepare + targeted retransform; on P+ it uses always-on CFLH. `tools-base@11ff885:profiler/native/perfa/perfa.cc:73-88`: "`// ClassPrepare event callback to invoke transformation of selected classes. In pre-P, this saves expensive OnClassFileLoaded calls for other classes.` … `if (class_transforms->find(sig_mutf8) != class_transforms->end()) { … SetEventNotificationMode(JVMTI_ENABLE, JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, thread)); … RetransformClasses(1, &klass)`". `perfa.cc:176-182`: "`SetEventNotification(jvmti_env, filter_class_load_hook ? JVMTI_ENABLE : JVMTI_DISABLE, JVMTI_EVENT_CLASS_PREPARE);`".
* Other ways Studio finds a `Class` loaded by some loader:
  * `findInstances(Class.class)` returns all loaded classes; the test inspector uses it as a `forName` replacement (`tools-base@11ff885:app-inspection/tests/test-inspector/src/test/inspector/TestInspector.java:87-94` "`for (Class instance : environment.artTooling().findInstances(Class.class)) { if (instance != null && instance.getName().equals(className)) {`… `throw new IllegalStateException("Found multiple instances of " + className);`").
  * The deploy agent searches thread-context → Application → JNI → `GetLoadedClasses` and logs duplicates (`tools-base@11ff885:deploy/agent/native/class_finder.cc:91-101` "`The same class was found multiple times in the loaded classes list`").
* `androidx.inspection` documents the one-way nature. `androidx@fc135bf:inspection/inspection/src/main/java/androidx/inspection/ArtToolingImpl.java:46-48`: "`We don't have a way to undo bytecode manipulations, so to avoid duplicating doing the same transformations multiple times, this object lives forever.`"

---

## 3. Loading the inspector code

* **Class loader type and parent.** `DexClassLoader` over the pushed jar, with the parent set to the app loader. One loader is cached per `dexPath` for the process lifetime. `tools-base@11ff885:app-inspection/agent/src/main/java/com/android/tools/agent/app/inspection/InspectorContext.java:115-119,144-148`:
  ```java
  ClassLoader classLoader = sCachedClassLoaders.computeIfAbsent(dexPath, s -> createClassloader(dexPath, mClassLoader));
  ServiceLoader<InspectorFactory> loader = ServiceLoader.load(InspectorFactory.class, classLoader);
  …
  String optimizedDir = System.getProperty("java.io.tmpdir");
  String nativePath = prepareNativeLibraries(dexPath, Build.SUPPORTED_ABIS[0]);
  return new DexClassLoader(dexPath, optimizedDir, nativePath, classLoader);
  ```
  The cache is justified at `:94-104`: "`Having two DexClassloaders created from the same jars is a problem, because they start fighting over resources: second Classloader will fail to loadLibrary()… (b/187342510)`".
* **How the app loader is found (not via hidden APIs, not via Agent_OnAttach, not via GetThreadInfo).** `AppInspectionService.java:451-471`:
  ```java
  ArtToolingImpl artTooling = new ArtToolingImpl(mNativePtr, "inspector");
  List<Application> applications = artTooling.findInstances(Application.class);          // JVMTI heap walk
  ClassLoader looperClassLoader = Looper.getMainLooper().getThread().getContextClassLoader();
  if (applications.isEmpty()) { return looperClassLoader; }
  … ClassLoader classLoader = application.getClassLoader();
  return classLoader == null ? looperClassLoader : classLoader;
  ```
  * ART passes no class loader to `Agent_OnAttach`. The `(vm, options, reserved)` call uses `nullptr` for reserved (`art@18ea424:runtime/ti/agent.cc:96-98` "`*call_res = callback(Runtime::Current()->GetJavaVM(), copied_args.get(), nullptr);`"). The `class_loader` argument of `VMDebug.attachAgent` is used only to pick the linker namespace for `dlopen` (`agent.cc:115-129` "`JavaVMExt::GetLibrarySearchPath(env, class_loader)` … `android::OpenNativeLibrary(env, …, class_loader, …`").
  * The context-class-loader fallback is not always the app loader. `fwb@9231092:core/java/android/app/LoadedApk.java:1400-1409`: "`boolean sharable = (sharedUserIdSet || processNameNotDefault);` `ClassLoader contextClassLoader = (sharable) ? new WarningContextClassLoader() : mClassLoader;` `Thread.currentThread().setContextClassLoader(contextClassLoader);`". This is set inside `makeApplicationInner` (`:1636-1640`).
  * The deploy agent instead uses hidden APIs through JNI (`class_finder.cc:32-49`: "`CallStaticJniObjectMethod("currentApplication", …)` … `GetJniObjectField("mLoadedApk", "Landroid/app/LoadedApk;")` … `getClassLoader`"), wrapped in `HiddenAPISilencer` (`deploy/agent/native/agent.cc:234`).
* **Kotlin stdlib.** Bundled and renamed by jarjar; there is also an escape hatch for dependencies that must stay unrenamed.
  * `tools-base@11ff885:app-inspection/jarjar_rules.txt:1-2`: "`rule kotlin.** com.android.tools.idea.kotlin.@1`" / "`rule kotlinx.coroutines.** com.android.tools.idea.kotlinx.coroutines.@1`".
  * `tools-base@11ff885:app-inspection/app_inspection.bzl:72-78`: "`# bundle_srcs represents dependencies that need to be bundled with the inspector (via jarjar)… # nojarjar_deps contains dependencies that will be included without jarjaring. These deps will be able to interact directly with the classes in the app or library code (e.g. kotlin.*, kotlinx.coroutines.*) that are renamed in the inspector by jarjar.`"
  * Network inspector: `tools-base@11ff885:app-inspection/inspectors/network/BUILD:44-51`: "`bundle_srcs = [ …inspectors/common…, "@maven//:org.jetbrains.kotlin.kotlin-stdlib", "@maven//:org.jetbrains.kotlinx.kotlinx-coroutines-core-jvm", ],` `d8_flags = [ "--min-api 26",  # Network inspector is only supported on O+ devices.`". OkHttp/okio/grpc are only compile `deps` (`:18-30`), so at runtime they come from the app through the parent loader.
* **The agent's own Java code** is on the boot class path from `perfa.jar` (§1.5). Native code finds it with JNI `FindClass`. `tools-base@11ff885:app-inspection/native/src/commands/app_inspection_agent_command.cc:39-47`: "`jclass service_class = jni_env->FindClass("com/android/tools/agent/app/inspection/AppInspectionService");` … `"instance"`". The Java object is created by JNI (`app_inspection_java_jni.cc:299-310` "`env->GetMethodID(serviceClass, "<init>", "(J)V")`").
* **Inspector discovery** uses `ServiceLoader<InspectorFactory>` over the child loader and matches `getInspectorId()` (`InspectorContext.java:118-131`). `androidx.inspection.*` itself resolves from boot (perfa.jar) through parent-first delegation.

---

## 4. Attach flow from the host

### 4.1 Push to device (IDE, via ddmlib shell/push)
* Directory, config names and port. `studio-ide@0867bfe:android-transport/src/com/android/tools/idea/transport/TransportFileManager.java:130-134`: "`public static final String DEVICE_DIR = "/data/local/tmp/perfd/";` `private static final String CODE_CACHE_DIR = "code_cache";` … `AGENT_CONFIG_FILE = "agent.config";` `DEVICE_PORT = 12389;`"
* Per-ABI agent naming. `TransportFileManager.java:67-72`: "`new DeployableFile.Builder("libjvmtiagent.so")` … `.setOnDeviceAbiFileNameFormat("libjvmtiagent_%s.so") // e.g. libjvmtiagent_arm64.so`". `perfa.jar` is non-ABI (`:65`). The runtime attach picks the **process** ABI (`studio-ide@0867bfe:app-inspection/api/src/com/android/tools/idea/appinspection/internal/DefaultAppInspectionTarget.kt:105` "`.setAgentLibFileName("libjvmtiagent_${transport.process.abiCpuArch}.so")`"). That value comes from the DDM client ABI (`studio-ide@0867bfe:android-transport/src/com/android/tools/idea/transport/TransportServiceProxy.kt:381-389` "`Parse cpu arch from client abi info, for example, "arm64" from "64-bit (arm64)"`").
* ART does not support native-bridge agents. `art@18ea424:runtime/ti/agent.cc:138-146`: "`Native-bridge agents unsupported: %s`"; `runtime/ti/agent.h:99` "`Currently agents can only be the actual runtime ISA of the device.`".
* Pushed only on O+. `TransportFileManager.java:150-152`: "`if (isAtLeastO(myDevice)) { copyFileToDevice(HostFiles.PERFA); copyFileToDevice(HostFiles.JVMTI_AGENT);`"
* Push sequence and permissions, including the **API 34 read-only rule**. `TransportFileManager.java:340-378`:
  ```java
  myDevice.executeShellCommand("rm -f " + deviceFilePath + " " + deviceFilePath + "_hash", …);
  myDevice.executeShellCommand("mkdir -p -m 755 " + folder + "; chown shell:shell " + folder, …);
  myDevice.pushFile(localPath.toString(), deviceFilePath);
  myDevice.executeShellCommand("chown shell:shell " + deviceFilePath, …);
  if (executable) { String cmd = "chmod 755 " + deviceFilePath; … }
  else { /* Starting with API 34 there is an additional check that a dex cannot be writable (see dalvik_system_DexFile.cc). */
         if (fileName.endsWith(".jar")) { String cmd = "chmod 444 " + deviceFilePath; … } }
  ```
* The inspector jar is pushed with the same code and loaded **in place** from `/data/local/tmp/perfd/`. There is no copy into the sandbox. `DefaultAppInspectionTarget.kt:138-141`: "`val fileDevicePath = jarCopier.copyFileToDevice(params.inspectorJar).first()` … `CreateInspectorCommand.newBuilder().setDexPath(fileDevicePath)`"; the copier is `TransportFileManager` (`studio-ide@0867bfe:app-inspection/ide/src/com/android/tools/idea/appinspection/ide/AppInspectionDiscoveryService.kt:90-95`).
* ART's check that the jar must be read-only. `art@18ea424:runtime/native/dalvik_system_DexFile.cc:380-386`: "`if (isReadOnlyJavaDclChecked() && access(sourceName.c_str(), W_OK) == 0) { LOG(ERROR) << "Attempt to load writable dex file: "` … `"Writable dex file '%s' is not allowed."`".
  * It is gated on API > 33 plus compat change `218865702` (`:68`, `:316-341`).
  * Root/system/shell UIDs are exempt (`:357-364`).
  * The check lives only in `openDexFileNative`, not in `openInMemoryDexFilesNative` (`:253`).
  * A14 doc: "If your app targets Android 14 (API level 34) or higher and uses Dynamic Code Loading (DCL), all dynamically-loaded files must be marked as read-only. Otherwise, the system throws an exception." `AddToBootstrapClassLoaderSearch` does not go through this path (§1.5), so the `perfa.jar` copy in `code_cache` is fine.

### 4.2 Daemon start and host↔daemon link
* `studio-ide@0867bfe:android-transport/src/com/android/tools/idea/transport/TransportDeviceManager.java:335`: "`TransportFileManager.getTransportExecutablePath() + " -config_file=" + TransportFileManager.getDaemonConfigPath()`".
* `:465-470`: "`myDevice.createForward(localPort, DEVICE_SOCKET_NAME, IDevice.DeviceUnixSocketNamespace.ABSTRACT);`" (O+), with `DEVICE_SOCKET_NAME = "AndroidStudioTransport"` (`:88`).

### 4.3 Attach command (IDE → daemon → device)
* IDE skips the attach if an agent is already `ATTACHED` for this process instance. Otherwise it sends `ATTACH_AGENT` and waits for an `AGENT` event. `DefaultAppInspectionTarget.kt:90-92,98-108,122-124`: "`// Agent is already attached and connected, so there is no need to attach.`" … "`.setAgentConfigPath(TransportFileManager.getAgentConfigFile())` `.setPackageName(transport.process.packageName)`" … "`if (streamEventResult.agentData.status == UNATTACHABLE) { throw AppInspectionAgentUnattachableException() }`".
* The daemon (running as `shell`) copies files into the sandbox with `run-as`, deleting first. `tools-base@11ff885:transport/native/daemon/daemon.cc:62-68,85-107`:
  ```cpp
  const char* const kCodeCacheRelativeDir = "./code_cache/";
  const char* const kAgentJarFileName = "perfa.jar";
  // Remove old agent first to avoid attaching mismatched version of agent.
  DeleteFileFromPackageFolder(package_name, user, file_name);        // rm -f ./code_cache/<f>
  mkdir_args << "-p " << kCodeCacheRelativeDir;                     // mkdir -p ./code_cache/
  args << CurrentProcess::dir() << file_name << " " << kCodeCacheRelativeDir;   // cp <daemon dir>/<f> ./code_cache/
  ```
  `run-as` wrapping. `tools-base@11ff885:transport/native/utils/bash_command.android.cc:42-55`: "`oss << kSuExecutable << " root sh -c 'cd /data/user/" << user << "/" << package_name << " && ";`" (userdebug, P+) "`} else { oss << kRunAsExecutable << " " << package_name << " " << kRunAsUserFlag << " " << (user != "" ? user : "0") << " sh -c '"; }`". Here `kRunAsExecutable = "/system/bin/run-as"` and `kSafeNameChars = "abc…XYZ0123456789._-:"` (`bash_command.h:25-33`). The user comes from `/system/bin/am get-current-user` (`activity_manager.cc:37,182-184`). The data dir comes from `run-as … sh -c 'pwd'` (`package_manager.cc:53-56`).
* Sequence in `Daemon::TryAttachAppAgent`. `daemon.cc:268-277`: "`CopyFileToPackageFolder(package_name, user, kConnectorFileName);` … `if (!IsAppAgentAlive(app_pid, package_name, user)) { RunAgent(app_name, package_name, user, agent_config_path, agent_lib_file_name); }`". `RunAgent` copies `perfa.jar` and the agent `.so`, then runs the attach (`daemon.cc:168-179`).
* The exact attach command. `tools-base@11ff885:transport/native/utils/process_manager.android.cc:21,99-100`: "`const char* const kAttachAgentCmd = "cmd activity attach-agent";`" … "`attach_params << app_name << " " << data_path << "/code_cache/" << lib_file_name << "=" << config_path;`". `app_name` is `/proc/<pid>/cmdline` (`tools-base@11ff885:transport/native/daemon/commands/attach_agent.cc:34` "`string app_name = ProcessManager::GetCmdlineForPid(pid);`"). The daemon itself runs as the shell user because it is launched with `adb shell` (`TransportDeviceManager.java:335-337`). So:
  `cmd activity attach-agent <process-name> /data/user/<u>/<pkg>/code_cache/libjvmtiagent_<arch>.so=/data/local/tmp/perfd/agent.config`
* Apply Changes attaches by **PID**. `tools-base@11ff885:deploy/installer/command_cmd.cc:201-204`: "`parameters.emplace_back("attach-agent"); parameters.emplace_back(to_string(pid)); parameters.emplace_back(agent + "=" + args);`".
* Framework side: the target must be debuggable. The agent attaches on the app's main thread (H handler) using the app `LoadedApk` class loader if bound, and retries with a null loader on failure.
  * `fwb@9231092:services/core/java/com/android/server/am/ActivityManagerService.java:20071-20083`: "`public void attachAgent(String process, String path) {` … `findProcessLOSP(process, UserHandle.USER_SYSTEM, "attachAgent");` … `enforceDebuggable(proc);` `thread.attachAgent(path);`"
  * `:7003-7006` "`if (!Build.IS_DEBUGGABLE && !proc.isDebuggable()) { throw new SecurityException("Process not debuggable: "…`"
  * `fwb@9231092:core/java/android/app/ActivityThread.java:3024-3026`: "`case ATTACH_AGENT: { Application app = getApplication(); handleAttachAgent((String) msg.obj, app != null ? app.mLoadedApk : null);`"
  * `:5291-5298`: "`if (attemptAttachAgent(agent, classLoader)) { return; } if (classLoader != null) { attemptAttachAgent(agent, null); }`"
  * ART refuses the plugin for non-debuggable processes (`art@18ea424:runtime/runtime.cc:2312-2314` "`Process must be debuggable`").
  * ART TI doc: "An agent may only be attached to a running app that's marked as debuggable…", and "Use run-as to copy the agent into the app's data directory."

### 4.4 Launch-time ("from launch") attach and API branches
* **App Inspection has no launch-time attach.** A code search of JetBrains/android for `LaunchTaskContributor` / `getAmStartOptions` finds only profilers-android, debuggers (coroutine) and aswb (Blaze).
* **Profiler startup agent** (`am start --attach-agent`), API ≥ 27, not in "profileable" mode. `TransportFileManager.java:464-499`:
  ```java
  // Startup agent feature was introduced from android API level 27.
  if (myDevice.getVersion().getFeatureLevel() < AndroidVersion.VersionCodes.O_MR1) { return ""; }
  …String[] requiredAgentFiles = {agentName, HostFiles.PERFA.getFileName()};
  myDevice.executeShellCommand(buildRunAsCommand(packageName, String.format("rm -rf ./%s/%s", CODE_CACHE_DIR, agentFile)), …);
  myDevice.executeShellCommand(buildRunAsCommand(packageName, String.format("cp %s ./%s/", DEVICE_DIR + agentFile, CODE_CACHE_DIR)), …);
  // Example: --attach-agent /data/data/package_name/code_cache/libjvmtiagent_x86.so=/data/local/tmp/perfd/startupagent.config
  return String.format("--attach-agent %s/%s/%s=%s", packageDataPath, CODE_CACHE_DIR, agentName, DEVICE_DIR + configName);
  ```
  Here `buildRunAsCommand` = "`run-as %s sh -c '%s'`" (`:536-538`) and the data path = `run-as <pkg> sh -c 'pwd'` (`:509`). This path uses the **device's best ABI** (`:482` "`getBestAbi(HostFiles.JVMTI_AGENT)`"), not the app's. It is used only for profile launches (`studio-ide@0867bfe:profilers-android/src/com/android/tools/idea/profilers/AndroidProfilerLaunchTaskContributor.java:93-95,109` "`if (!isProfilerLaunch(executor)) { // Not a profile action return ""; }`" … "`fileManager.configureStartupAgent(applicationId, STARTUP_AGENT_CONFIG_NAME, executor.getId());`").
* **Coroutine debugger:** `--attach-agent <code_cache>/coroutine_debugger_agent.so`, Q+ only. Studio's comment reports problems on 27/28. `studio-ide@0867bfe:debuggers/src/com/android/tools/idea/debuggers/coroutine/CoroutineDebuggerLaunchTaskContributor.kt:43-48,61-62`: "`// On api 27 the agent .so is not found at startup time… // On api 28 the whole debugger hangs when attaching the agent.` `if (!device.version.isAtLeast(AndroidVersion.VersionCodes.Q)) { return "" }` … `return "--attach-agent ${appCodeCache}coroutine_debugger_agent.so"`". Deploy copies it with `run-as <pkg> cp -F …` (`tools-base@11ff885:deploy/installer/install_coroutine_agent.cc:65-70`).
* **Apply Changes startup agents** in `code_cache/startup_agents/`.
  * Sites: `tools-base@11ff885:deploy/sites/src/com/android/tools/SitesGenerator.java:59-64` "`"AppStartupAgent", "AppCodeCache(pkg) + \"startup_agents/\""`"; also `.studio` dirs `:47-52,79` "`AppCodeCache(pkg) + \".studio/\""`, "`/data/local/tmp/.studio/`".
  * Agents are versioned and stale ones are wiped. `tools-base@11ff885:deploy/installer/agent_interaction.cc:104-106,158-167`: "`return AppAgentAbsDir() + workspace_.GetVersion() + "-" + agent_filename;`" … "`// Clean up other agents from the startup_agent directory. Because agents are versioned (agent-<version#>) … we assume another agent is present and delete it.`"
  * The startup agent receives the data dir as its options. `tools-base@11ff885:deploy/agent/native/agent.cc:236-239`: "`// Startup agents are passed the path to the app data directory.` `if (input[0] == '/') { return HandleStartupAgent(jvmti, jni, input);`"
* **Framework support by API** (verified against AOSP tags):

  | API | Feature | Evidence |
  |---|---|---|
  | 26 (8.0) | `cmd activity attach-agent` only; no `am start --attach-agent` | `aosp-fwb@android-8.0.0_r1:services/core/java/com/android/server/am/ActivityManagerShellCommand.java:240` "`case "attach-agent":`"; `--attach-agent` absent (grep count 0) |
  | 27 (8.1) | `am start --attach-agent` ("before binding") | `aosp-fwb@android-8.1.0_r1:…/ActivityManagerShellCommand.java:297` "`} else if (opt.equals("--attach-agent")) {`", `:2681` "`--attach-agent <agent>: attach the given agent before binding`" |
  | 28 (9) | `--attach-agent-bind` ("during binding") | `aosp-fwb@android-9.0.0_r1:…/ActivityManagerShellCommand.java:333` "`} else if (opt.equals("--attach-agent-bind")) {`", `:2865` "`--attach-agent-bind <agent>: attach the given agent during binding`" |
  | 30 (11) | auto-attach every file in `code_cache/startup_agents` | `aosp-fwb@android-11.0.0_r1:core/java/android/app/ActivityThread.java:3952-3964` "`static void handleAttachStartupAgents(String dataDir) {` … `code_cache.resolve("startup_agents");` … `handleAttachAgent(p.toAbsolutePath().toString() + "=" + dataDir, null);`"; absent in android-10.0.0_r1 (grep count 0) |

  * Current framework: startup agents run for debuggable apps only, before `bindApplication`. `fwb@9231092:services/core/java/com/android/server/am/AppProfiler.java:2430-2437`: "`// If we were asked to attach an agent on startup, do so now, before we're binding application code.` `if (preBindAgent != null) { thread.attachAgent(preBindAgent); }` `if (app.isDebuggable()) { thread.attachStartupAgents(app.info.dataDir); }`". The call happens before `bindApplication` (`ActivityManagerService.java:5393` then `:5454`).
  * `--attach-agent-bind` attaches inside `handleBindApplication` with the `LoadedApk`, before the Application is created. `ActivityThread.java:8062-8063` "`if (data.initProfilerInfo.attachAgentDuringBind) { agent = data.initProfilerInfo.agent; }`" and `:8145-8146` "`if (agent != null) { handleAttachAgent(agent, data.info); }`".
  * Studio's own comments disagree with AOSP: `tools-base@11ff885:deploy/deployer/src/main/java/com/android/tools/deployer/Deployer.java:400` says "`--attach-agent was added on API 28`"; the AOSP 8.1 source shows it exists at API 27.
* Other API branches in Studio code:
  * O+ required for attach (`daemon.cc:251` "`assert(profiler::DeviceInfo::feature_level() >= profiler::DeviceInfo::O);`").
  * `su root` on userdebug P+ (§4.3).
  * CFLH strategy switches at P (§2).
  * `findInstances` switches at Q (§1.8).
  * Deploy "new pipeline" requires R (`Deployer.java:609-611` "`>= AndroidVersion.VersionCodes.R`").

---

## 5. Transport (context only)

1. The host (Studio) talks gRPC to an on-device daemon, `/data/local/tmp/perfd/transport`, started via `adb shell`. The link is `adb forward tcp:<p> localabstract:AndroidStudioTransport` (§4.2).
2. The in-app agent creates a listening abstract socket `@AndroidStudioTransportAgent<pid>` (`tools-base@11ff885:transport/native/utils/socket_utils.h:33` "`kAgentSocketName = "@AndroidStudioTransportAgent";`"; `agent.cc:358-362` "`// Creates and listens to socket at kAgentSocketName+pid.`").
3. The daemon does not connect to it directly. It runs its own binary copied into `code_cache` in *connector* mode via `run-as`. That process sends the agent an fd that is already connected to the daemon (`SCM_RIGHTS`, `socket_utils.cc:49-54`).
   * `daemon.cc:53-60`: "`// Connector is a program that inherits (since it is invoked by execl()) a client socket already connected to the daemon and passes the socket to the agent. This is technically an implementation detail of daemon due to Android's security restriction.`"
   * `daemon.cc:146-148`: "`execl(kRunAsExecutable, kRunAsExecutable, package_name.c_str(), kRunAsUserFlag, user.c_str(), kConnectorRelativePath, connect_arg.str().c_str(), …)`" with `--connect=<pid>:C:<fd>:<timeout>`. `H` is a heartbeat probe (`socket_utils.h:42-48`).
4. The agent then runs gRPC over that fd (`agent.cc:391-393` "`os << kGrpcUnixSocketAddrPrefix << "&" << fd; ConnectToDaemon(os.str());`"): a `RegisterAgent` command stream plus `SendEvent`.
5. App Inspection responses and events are `Event`s of kind `APP_INSPECTION_RESPONSE`, `APP_INSPECTION_EVENT` or `APP_INSPECTION_PAYLOAD` (`app_inspection_java_jni.cc:75,101,262`). Large payloads are chunked below gRPC's 4 MB limit (`InspectorContext.java:52-57` "`Since gRPC 1.22.0, the max size per gRPC message is 4 MB.`").
6. The connector retries for 5 minutes (`tools-base@11ff885:transport/native/daemon/connector.cc:26-30` "`try to connect for five minutes before giving up… an app can be stuck on waiting for debugger to attach.`").

---

## 6. Lifecycle

* **Double-attach guards.**
  1. The IDE checks for an `ATTACHED` agent event after the latest process-start event (§4.3).
  2. The daemon only runs `attach-agent` if a heartbeat to the agent socket fails. `daemon.cc:271-277`: "`// Only attach agent if one is not detected. Note that an agent can already exist if we have profiled the same app before, and either Studio/daemon has restarted and has lost any knowledge about such agent.` `if (!IsAppAgentAlive(app_pid, package_name, user)) {`". The probe is `run-as … ./code_cache/transport --connect=<pid>:H` (`daemon.cc:360-366`). If alive, the daemon only re-runs the connector to hand the agent a new daemon fd (`daemon.cc:279-297`; agent side `agent.cc:374-398` "`// A connect request - reconnect using the incoming fd.`").
  3. The in-agent singleton is replaced only if the config differs (`agent.cc:60-67` "`if (replace && !google::protobuf::util::MessageDifferencer::Equals(config, instance->agent_config())) { delete instance; instance = new Agent(config); }`").
  4. Inspector ids are unique unless `force` is set. `AppInspectionService.java:133-150`: "`"Inspector with the given id " + inspectorId + " already exists. It was launched by project: "` … `doDispose(inspectorId);`".
  5. The deploy agent uses a **breadcrumb class** in its boot jar to detect a previous load and a changed jar. `tools-base@11ff885:deploy/agent/native/instrumenter.cc:184-213`: "`// Check for the existence of a breadcrumb class, indicating a previous agent has already loaded instrumentation.` … `breadcrumb.CallStaticBooleanMethod("checkHash", "(Ljava/lang/String;)Z", jar_hash);` … `"The instrumentation jar at %s does not match the jar previously used to instrument. The application must be restarted."`". It also counts invocations (`deploy/agent/native/agent.cc:130` "`Prior agent invocations in this VM: %d`").
* **Agents cannot be unloaded.** `art@18ea424:runtime/ti/agent.h:96`: "`The agent's Agent_OnUnload function will be called during runtime shutdown.`". The native service is never freed (`tools-base@11ff885:app-inspection/native/include/app_inspection_service.h:52-54` "`java object AppInspectionService that keeps reference to this object is singleton, so no need to clean up`").
* **"Stop inspecting."** The IDE sends `DisposeInspectorCommand` (`studio-ide@0867bfe:app-inspection/api/src/com/android/tools/idea/appinspection/internal/AppInspectorConnection.kt:198-208`). The agent removes that inspector's handlers only; the bytecode stays. `AppInspectionService.java:244-251`: "`removeHooks(inspectorId, mEntryTransforms); removeHooks(inspectorId, mExitTransforms); InspectorBridge inspector = mInspectorBridges.remove(inspectorId); if (inspector != null) { sendDisposedEvent(inspectorId, errorMessage); inspector.disposeInspector(); }`".
  * Label lists stay, so re-creating the inspector does not retransform again (`computeIfAbsent`, §1.2).
  * With an empty list, `onExitInternal` returns the original value (§1.7).
  * The network inspector's own cleanup only undoes the OkHttp2 in-place list edit. `NetworkInspector.kt:358-361`: "`okHttp2Interceptors?.removeIf { it is OkHttp2Interceptor }` `scope.cancel("Network Inspector has been disposed.")`"
  * The test asserts hooks do not fire after dispose (`tools-base@11ff885:app-inspection/tests/app-inspection-test/testSrcs/com/android/tools/app/inspection/ArtToolingTest.java`, `entryAndExitHooksDisposed`: "`// hooks will throw if they are called but inspection is disposed`").
* **Daemon death** disposes all inspectors (`AppInspectionService.java:194-198` "`doDispose(inspectorId, "Deamon terminated");`"; hooked from `transport_agent.cc:99-105`). An **inspector crash** disposes it with an error event (`InspectorBridge.java:84-101` "`"Inspector " + inspectorId + " crashed due to: " + throwable.getMessage();`").
* **Re-attaching a new version** of the agent `.so` into a process that already has one: the daemon deletes and re-copies the file (`daemon.cc:85-90`) but skips `attach-agent` while the old agent answers heartbeats. **UNVERIFIED:** what bionic/ART do if the same path is attached again after the file was replaced. ART stores each attached agent (`runtime.cc:2342-2343` "`agents_.push_back(std::move(agent));`").

---

## 7. Failure handling

* **Method not found** (e.g. minified or renamed) → slicer `FindMethod` returns null → `InstrumentMethod` returns false → **logcat only**. `dexter@main:slicer/instrumentation.cc:763-771`: "`auto ir_method = builder.FindMethod(method_id); if (ir_method == nullptr) { // we couldn't find the specified method return false; }`". Abstract or native methods also fail (`:746-749` "`// can't instrument abstract methods`"). Studio logs "`Error instrumenting %s %s->%s%s`" (`app_inspection_transform.h:66-70`) and returns nothing to Java.
* **Class index missing** in the CFLH bytes → `Log::V` and the class is left unmodified (`app_inspection_service.cc:236-239`).
* **Retransform error** → `CheckJvmtiError` logs "`JVMTI error: %d(%s) %s`" and continues (`jvmti_helper.cc:47-59`; call at `app_inspection_service.cc:306`).
* **Bad method string** → `Log::E` and return (`app_inspection_java_jni.cc:344-350`).
* **The Java side still records the handler.** `registerExitHook` returns `void` and throws nothing, so an inspector cannot tell whether a hook took effect.
* **What reaches the IDE.**
  * Only a missing class, as `NoClassDefFoundError` caught by the inspector, becomes a boolean in `StartInspectionResponse` (`NetworkInspector.kt:119-130`; proto `tools-base@11ff885:app-inspection/inspectors/network/resources/proto/network-inspector.proto:295-300` "`// TRUE if agent was able to instrument OK HTTP code optional bool okhttpHooksRegistered = 4;`").
  * `registerJavaNetHooks()` always returns `true` (`NetworkInspector.kt:228-236`).
  * The IDE only surfaces `speedCollectionStarted` (`studio-ide@0867bfe:app-inspection/inspectors/network/view/src/com/android/tools/idea/appinspection/inspectors/network/view/NetworkInspectorTab.kt:128-133` "`if (!response.speedCollectionStarted) { services.ideServices.showNotification("Failed to collect speed data. See device Logcat for more information"`"). A JetBrains/android code search for `HooksRegistered` returned no hits.
* **Inspector creation errors are reported:**
  * "`Failed to find a file with path: `" (`AppInspectionService.java:157-160`)
  * "`Failed to find InspectorFactory with id `" (`InspectorContext.java:133`)
  * "`Failed during instantiating inspector with id `" (`:138`)
  * Version/proguard statuses (`AppInspectionService.java:271-293`). These apply to library inspectors only; the network inspector is a framework inspector (`studio-ide@0867bfe:app-inspection/inspectors/network/ide/src/com/android/tools/idea/appinspection/inspectors/network/ide/NetworkInspectorTabProvider.kt:51-57` "`FrameworkInspectorLaunchParams(AppInspectorJar("network-inspector.jar", …`"), so `doCheckVersion` returns `true` for a null library.
  * The IDE maps these statuses to exceptions (`DefaultAppInspectionTarget.kt:194-202`).
* **Agent attach failure.**
  * Studio's transport agent returns `JNI_ERR` on a bad config (`transport_agent.cc:70-81`). ART then throws `IOException` (`runtime.cc:2344-2347`) and the framework retries with a null loader (§4.3). Deploy avoids this: `deploy/agent/native/agent.cc:205-206` "`// We return JNI_OK even if anything failed, since returning JNI_ERR just causes ART to attempt to re-attach the agent with a null classloader.`"
  * The daemon's `RunAgent` does not propagate the `attach-agent` result (`daemon.cc:170-182`: `success |= attach.Run(...)` after `success` is already true). A failed attach surfaces as the IDE never seeing `ATTACHED`, while the connector retries for up to 5 minutes.
* **Slicer hard failures kill the process.** `dexter@main:slicer/common.cc:41-47`: "`SLICER_CHECK failed [` … `abort();`". Examples: ReturnAsObject on a primitive return (`instrumentation.cc:370-371` "`SLICER_CHECK(!return_as_object || (declared_return_type->GetCategory() == ir::Type::Category::Reference));`"), and `MethodId` constructed with a signature for the hook (`instrumentation.h:100` "`SLICER_CHECK_EQ(hook_method_id_.signature, nullptr);`").

---

## 8. Capabilities and API level

* **Studio requests every potential capability**, both in the transport env (`transport_agent.cc:75` "`SetAllCapabilities(jvmti_env);`") and in the app-inspection env (`app_inspection_service.cc:257`). `jvmti_helper.cc:61-67`: "`error = jvmti->GetPotentialCapabilities(&caps);` … `error = jvmti->AddCapabilities(&caps);`".
* **The deploy agent requests only** `can_redefine_classes` and `can_retransform_classes` (`tools-base@11ff885:deploy/agent/native/capabilities.h:35,63` "`.can_redefine_classes = 1,`" … "`.can_retransform_classes = 1,`").
* **ART potential capabilities** (debuggable): `can_tag_objects = 1`, `can_redefine_classes = 1`, `can_retransform_classes = 1`, `can_redefine_any_class = 0`, `can_retransform_any_class = 0`, `can_generate_all_class_hook_events = 0`, … (`art@18ea424:openjdkjvmti/art_jvmti.h:256-297`).
  * Unavailable when loaded without debuggable: `art_jvmti.h:298-311` "`These are capabilities that are disabled if we were loaded without being debuggable… can_retransform_classes… can_redefine_classes`".
  * `IsFullJvmtiAvailable` requires forced-interpret or `IsJavaDebuggableAtInit()` (`:75-79`).
  * `kArtTiVersion = JVMTI_VERSION_1_2 | 0x40000000` is the userdebug "debug-anything" version (`:66-72`).
* **Ordering requirement (JVMTI spec, can_retransform_classes):** "this capability must be set before the ClassFileLoadHook event is enabled for the first time in this environment." Studio adds capabilities first, then enables CFLH (`app_inspection_service.cc:257-274`).
* **Minimum API = 26 (O)**, enforced in several places:
  * IDE: `studio-ide@0867bfe:app-inspection/ide/src/com/android/tools/idea/appinspection/ide/ui/AppInspectionView.kt:86-90` "`a process is deemed inspectable if the device it's running on is O+ and if it's debuggable` … `return this.device.apiLevel.majorVersion >= AndroidVersion.VersionCodes.O`".
  * Daemon: `daemon.cc:251` (assert O+).
  * Files: `TransportFileManager.java:150` (agent and perfa pushed only O+).
  * Library: `androidx@fc135bf:inspection/inspection/build.gradle:50-51` "`// studio pipeline works only starting with Android O` `minSdk { version = release(26) }`".
  * Inspector dex: `inspectors/network/BUILD:49-51` (`--min-api 26`).
  * ART TI doc: "In Android 8.0 and higher, the ART Tooling Interface (ART TI) exposes certain runtime internals…".
  * Tests run on O, P and Q (`ArtToolingTest.java:47-52` "`// Enter/exit hook implementation slightly changes between O and P // findInstances has different implementation in Q`").

---

## 9. slicer API (`dexter@main:slicer/export/slicer/instrumentation.h`, `slicer/instrumentation.cc`)

| Class | Hook proto (auto-generated; the caller must pass `MethodId(class, name)` with **no signature**) | Where the call is inserted | Notes / constraints |
|---|---|---|---|
| `EntryHook(id, Tweak::None)` | `(<DeclaringType> this?, <params…>)V` | before the first bytecode | `invoke-static/range` over the method's own `ins` registers, so no scratch regs are needed (`instrumentation.cc:154-166`) |
| `EntryHook(id, Tweak::ThisAsObject)` | `(Object this?, <params…>)V` | same | `h:45-48` "`Expose the "this" argument of non-static methods as the "Object" type.`" |
| `EntryHook(id, Tweak::ArrayParams)` | `([Object)V` | same | array = `[label, this or null, boxed args…]` (`h:49-54`). Needs 3 scratch regs; grows `registers` and emits param-shift moves if needed (`cc:230-240,355-358`). Clears regs to `0xFEFEFEFE` (Studio's fork uses 0). |
| `ExitHook(id)` / `Tweak::None` | `(<Ret>)<Ret>` or `()V` | before **every** `return*` | `h:81-83` "`Insert a call to the "exit hook" method before every return… passed the original return value and it may return a new return value.`" |
| `ExitHook` + `ReturnAsObject` | `(Object)Object` + `check-cast <declared>` | same | `h:88-92` "`return value will be passed as "Object" type`". **Reference returns only** (a SLICER_CHECK aborts otherwise) |
| `ExitHook` + `PassMethodSignature` | `(String label, [ret])…` | same | `h:93-94` "`Pass method signature as the first parameter of the hook method.`". The label is `"<dotted.Class>-><name><desc>"` (`cc:98-101`). A register is taken over for the string: `reg-1`, or the value is shifted from v0 to v1; scratch is allocated with `AllocateScratchRegs(1, false)` if `registers < reg_count+1` (`cc:432-485`) |
| `DetourVirtualInvoke(orig, detour)` | `(<OrigRefClass> this, <params…>)<Ret>` | rewrites `invoke-virtual[/range]` call **sites inside the instrumented method** to `invoke-static[/range]` detour | `h:120-123` "`The detour is a static method which takes the same arguments as the original method plus an explicit "this" argument and returns the same type`". Matching is exact on (ref class, name, signature) (`dex_ir_builder.cc:24-28`) |
| `DetourInterfaceInvoke` | same | `invoke-interface[/range]` → `invoke-static[/range]` | `h:156-166` |
| `AllocateScratchRegs(n, allow_renumbering)` | — | renumbers (if < 16 regs) or grows regs and shifts params | `h:168-193`; `cc:707-742` |
| `MethodInstrumenter` | — | batches transformations for one method; builds the code IR once | `h:195-209`; `InstrumentMethod(MethodId)` returns false if the method is not found or has no code (`cc:744-772`) |

Generated exit-hook bytecode, from `ExitHook::Apply` (`cc:362-522`), for `ReturnAsObject|PassMethodSignature` and `return-object vR`:
```
[move-object/16 v(R+1), vR   ; only if R == 0, then label goes to v0]
const-string vS, "com.foo.Bar->m(...)Lx;"      ; S = R-1 (or 0)
invoke-static/range {vS .. vS+1}, Hook.onExit(Ljava/lang/String;Ljava/lang/Object;)Ljava/lang/Object;
move-result-object vR
check-cast vR, Lx;
return-object vR
```
* **Exceptional exits are not hooked**; only `return-void/return/return-object/return-wide` are instrumented (`cc:405-430`).
* **Constraints**, all inferred from the code:
  * The hook must be a **static** method (always `invoke-static/range`).
  * Its class must be resolvable from the instrumented class's defining loader.
  * Its parameter and return types must match the auto-generated proto exactly.
  * Public/package access rules are not checked by slicer. **UNVERIFIED:** the runtime access failure mode for non-public hooks; always use `public`.
* **Error policy:** `SLICER_CHECK` → `abort()` (`common.cc:41-47`). `slicer::set_logger` lets you route messages (`common.cc:36-38`); deploy does so (`deploy/agent/native/agent.cc:219` "`slicer::set_logger(SlicerLogger);`").

---

## Design implications for netinspect

The recommendations below apply the verified facts above; points that rest on inference are flagged.

### A. Hook mechanism
1. **Copy Studio's core pipeline:** a CFLH that receives dex, then slicer `Reader → FindClassIndex → CreateClassIr → MethodInstrumenter → Writer::CreateImage`, using a JVMTI-`Allocate` allocator (§1.4). All three targets return references (`networkInterceptors()Ljava/util/List;`, `eventListenerFactory()Lokhttp3/EventListener$Factory;`, `openConnection()Ljava/net/URLConnection;`). One trampoline entry point therefore suffices, via `ExitHook` with `ReturnAsObject | PassMethodSignature` → `static Object onExit(String, Object)`. Validate the return category before calling slicer, because it aborts otherwise (§7).
2. **Register by descriptor, not by `Class`.** Keep a table `{ "Lokhttp3/OkHttpClient;": [...], "Ljava/net/URL;": [...] }`. At attach, add `can_retransform_classes` *before* enabling CFLH (spec requirement, §8). Then call `GetLoadedClasses`, match signatures, and `RetransformClasses` **every** match (all loaders). Studio retransforms only the one `Class` it was handed (§2).
3. **API ≥ 28:** keep CFLH enabled globally, as Studio does, so later definitions (any loader) are instrumented (ART `ClassPreDefine`, §2). **API 26–27:** Studio would miss late loads. Use the profiler's pattern there (ClassPrepare → `RetransformClasses` with CFLH enabled on that thread), or accept always-on CFLH; Studio's comment says it has significant overhead pre-P.
4. **Idempotent transforms.** ART re-delivers the *original* dex on every retransform (§1.4), so the CFLH must apply the complete hook set for that class every time and never assume prior edits.
5. **Loader-aware dispatch (Studio has none).** The exit hook passes only `(label, value)`, with no `this`. For `networkInterceptors()` the value is a `java.util` list, so its loader tells you nothing. Options:
   * (a) Preferred, inferred: a small fork of `ExitHook` that emits a **per-loader label** (e.g. suffix `#<loaderId>`). The CFLH already knows `loader`, so keep a `loaderId → GlobalRef` map natively.
   * (b) Studio's gRPC pattern: `EntryHook(ThisAsObject)` + `ExitHook`, paired through a `ThreadLocal` (`NetworkInspector.kt:342-351`).

   Passing `this` at *exit* is not safe in general. **UNVERIFIED/inferred:** d8 may reuse the `p0` register after its last use, and slicer's `ExitHook` does not preserve it.
6. **OkHttp re-entrancy from `newBuilder()`.** In OkHttp 4.x/5.x, `Builder(OkHttpClient)` calls the hooked getters. `okhttp@40a3b87:okhttp/src/commonJvmAndroid/kotlin/okhttp3/OkHttpClient.kt:629-630`: "`this.networkInterceptors += okHttpClient.networkInterceptors`" / "`this.eventListenerFactory = okHttpClient.eventListenerFactory`". `javap` of okhttp-4.12.0 confirms `invokevirtual okhttp3/OkHttpClient.networkInterceptors:()Ljava/util/List;` and `…eventListenerFactory:()…` in `OkHttpClient$Builder.<init>(OkHttpClient)`. OkHttp 3.14.9 reads the fields directly (`getfield`). So a derived client would inherit our interceptor or wrapper and the hook would add another. **Dedupe:** skip if our interceptor class is already in the list, and unwrap our `EventListener.Factory` wrapper before re-wrapping. Studio's okhttp3 hook does *not* dedupe (`NetworkInspector.kt:273-278`), unlike its OkHttp2 hook (`:256`).
7. Hooks run on every call. OkHttp calls the getters per call: `RealCall.kt:77` "`client.eventListenerFactory.create(this)`" and `:219` "`interceptors += client.networkInterceptors`". Keep the trampoline fast path to a single volatile read.
8. Consider also hooking `URL.openConnection(Ljava/net/Proxy;)Ljava/net/URLConnection;`. Studio hooks only the no-arg overload (`NetworkInspector.kt:229-232`). This is a coverage gap, not verified further.

### B. Trampoline
1. Ship a **separate tiny boot dex**. Append it with `AddToBootstrapClassLoaderSearch` inside `Agent_OnAttach`; the live phase is fine and ART applies no read-only check there (§1.5). Do this **before** retransforming anything. It should contain only:
   * `public final class <ns>.Trampoline { public static Object onExit(String label, Object v) }`
   * a `public interface <ns>.ExitHandler { Object onExit(String label, Object v); }`
   * a `static volatile ExitHandler handler`

   Signatures should use only `java.lang.*`. This mirrors Studio: `AppInspectionService.onExit` and `androidx.inspection.ArtTooling.ExitHook` both live on the boot path in `perfa.jar` (§1.5). Follow the spec's advice to put nothing else on the boot path, and use a unique package so no app class is shadowed.
2. **Catch `Throwable` and return the original value.** Add a `ThreadLocal` re-entrancy guard, because the runtime's own I/O would re-enter the `URL.openConnection` hook. Studio does neither (§1.7).
3. **Version guard.** Before appending, try JNI `FindClass(<ns>/Trampoline)`. If it exists, check its version and reconnect instead of appending again; this is the deploy breadcrumb/`checkHash` pattern (§6). Optionally embed the version in the class name.
4. Trampoline or runtime `native` methods can bind to symbols exported by the agent `.so` with no `loadLibrary` (ART agent symbol lookup, §1.5).
5. A boot dex from an app path gets the hidden-API Platform domain (§1.5). The `HiddenApiSilencer` Studio uses during retransform is probably unnecessary for public targets; its purpose is **UNVERIFIED**.

### C. Class-loader strategy
1. **Parent = the defining loader of `okhttp3.OkHttpClient`**, obtained natively (CFLH `loader` param, or `GetClassLoader(klass)` on classes from `GetLoadedClasses`). This works at launch (pre-bind) and with multiple OkHttp copies.
   * Studio's `findInstances(Application)` + `getClassLoader()` approach is fine for runtime attach, but no Application exists before bind.
   * Avoid the main thread's context class loader. For `sharedUserId` or non-default-process-name apps it is a `WarningContextClassLoader`, and it is only set in `makeApplicationInner` (§3).
2. Create the runtime loader **lazily** on the first `onExit` from a given loader. Keep one runtime instance per OkHttp loader, as with Studio's per-`dexPath` cache (§3). For the `java.net` hook, any loader works because it needs only `java.*` and `android.*`.
3. **Loading the runtime dex under the API 34 rule.** Use `InMemoryDexClassLoader(ByteBuffer, parent)`: API 26+ per the reference ("Added in API level 26"), and not subject to ART's writable-file check (§4.1). Alternatively use a `DexClassLoader` on a file that is **not writable** by the app uid: either `chmod 444` in `/data/local/tmp/<dir>/` as Studio does, or `run-as cp` into `code_cache` followed by `chmod 444`.
4. Do not bundle `okhttp3`/`okio` (resolve through the parent, as Studio does). The runtime is Java 8, so no Kotlin stdlib is needed; if Kotlin is ever used, jarjar `kotlin.**` as Studio does.

### D. Launch attach ("capture from launch")
1. Per-API choice:
   * **API ≥ 30:** push the agent to `code_cache/startup_agents/` (debuggable apps). The framework attaches it pre-bind with `options = dataDir` on **every** process start, including secondary processes. **Delete it when the session ends.** Use versioned names, as deploy does.
   * **27–29:** `am start … --attach-agent <dataDir>/code_cache/<agent>.so=<opts>` (pre-bind). **28+:** alternatively `--attach-agent-bind` (during bind, loader available).
   * **26:** runtime attach only.
2. Pre-bind there is no app class loader. That is why A2/A3 (by-name transforms plus always-on CFLH) and C1/C2 (lazy runtime) matter. `java.net.URL` is already loaded, so retransform it at attach.
3. `Agent_OnAttach` runs on the app **main thread** (H handler, §4.3). Keep it short: set up JVMTI, append the boot dex, retransform. Do socket and connection setup on a separate thread (e.g. `RunAgentThread`, which the profiler uses for post-attach Java init: `perfa.cc:225-230`).
4. Attach by **PID** (`cmd activity attach-agent <pid> …`, as deploy does). Use the **process ABI** for the `.so`, because native-bridge agents are unsupported (§4.1). Studio's startup path uses the device's best ABI, which could mismatch 32-bit apps (inference).
5. **Transport.** Studio avoids connecting from shell to the app's socket and uses a `run-as` connector that passes an fd ("due to Android's security restriction", §5). **UNVERIFIED:** whether `adb forward … localabstract:<app socket>` works on all API levels. Prototype both, or reuse the `run-as` connector idea.

### E. Failure reporting
1. Report a structured **hook-status** record per target:
   * descriptor, method, and loaders seen;
   * `InstrumentMethod` result;
   * `RetransformClasses` error name (`GetErrorName`);
   * whether the class or method was missing (the likely "minified OkHttp" case);
   * first-hit timestamp and hit count.

   Studio logs these only to logcat and the IDE ignores them (§7). ART also exposes `com.android.art.misc.get_last_error_message` (`art@18ea424:openjdkjvmti/ti_extension.cc:287`) for richer diagnostics.
2. Always return `JNI_OK` from `Agent_OnAttach` and report errors over our channel (deploy's rationale, §7).
3. Surface `SecurityException("Writable dex file …")` from runtime loading distinctly.

### F. Lifecycle
1. **Stop** = set `handler = null` (bytecode stays; agents cannot be unloaded, §6), and remove interceptors or wrappers where we can. Optionally restore the original classes. The JVMTI spec says "Classes can be modified multiple times and can be returned to their original state", and ART retransforms from the original dex, so clearing our table and calling `RetransformClasses` should restore them. **UNVERIFIED on device.**
2. **Re-attach:** probe first (trampoline present? socket alive?), then reconnect; make `Agent_OnAttach` idempotent. **UNVERIFIED:** ART/bionic behavior when the same `.so` path is attached twice, or after the file is replaced.

### G. Risks and open questions
* **Name-based transforms are loader-agnostic** (P+). Every same-named class gets instrumented, and a single-loader dispatcher would hand it foreign types. Mitigation: A5/C2.
* **R8/minification:** renamed classes or methods mean no hook (report it). **UNVERIFIED:** whether R8 inlines the getters into `RealCall`, which would mean zero hits even though the method exists. Rely on hit counters, and fall back to library mode.
* **Slicer `abort()`** on unexpected input crashes the app. Pin the slicer revision and pre-validate targets.
* **Coexistence with Studio's own inspector or other agents.** CFLHs chain across envs (ART calls each retransformable env in turn, `ti_class.cc:194-210`). Our hooks may stack with Studio's `AppInspectionService.onExit`. Use distinct class names and tolerate pre-instrumented bytecode.
* **Always-on CFLH cost:** our callback runs for every class definition, so keep it to a lock-free hash lookup. Studio takes a mutex per event (`app_inspection_service.cc:229`).
* **Startup agents persist** until deleted. Every launch, and every process of the app, gets instrumented.
* **Debuggable only** (AMS `enforceDebuggable`, ART `EnsureJvmtiPlugin`). The userdebug `kArtTiVersion` path lacks retransform (§8).
* **API 34 DCL:** the runtime dex must not be app-writable (C3).
* Studio's `ArrayParamsEntryHook` fork aborts on static no-arg methods with fewer than 3 registers (inference, §1.4). Avoid entry hooks, or use upstream slicer.
* **Library-mode aside:** androidx's in-process `DefaultArtTooling` loads JVMTI by calling `Debug.attachJvmtiAgent("nonexistent.so", …)` (`androidx@fc135bf:inspection/inspection/src/androidTest/java/androidx/inspection/rules/JvmtiRule.kt:29-33` "`attachJvmtiAgent call is enough to make art to load JVMTI plugin`", API 28+). Its callback class `androidx.inspection.ArtToolingImpl` lives in the app loader (`art_tooling_transform.h:49-60`). *Inference:* hooking boot classes such as `java.net.URL` that way would reference a class the boot loader cannot resolve.

### UNVERIFIED items (what was tried)
* Purpose of `HiddenApiSilencer` in `AddTransform`: read `hidden_api_silencer.cc` and the deploy copy; there are no comments and no commit history in the squashed snapshot.
* ART/bionic behavior on re-attaching the same or a replaced agent path: not in the sources read (`runtime/ti/agent.cc` covers the dlopen path only).
* Whether `adb forward` to an app-owned abstract socket works on all API levels: only Studio's comment was found, and it avoids the pattern.
* R8 inlining of OkHttp getters: not examined.
* Behavior of JVMTI ext `com.android.art.classloader.add_to_dex_class_loader(_in_memory)` (`ti_extension.cc:338,358`) under the API 34 writable-dex check, and its availability by API level: not examined.
* Runtime error for non-public hook targets: slicer does no access checks, and runtime behavior was not tested.
* Whether `eventListenerFactory()` exists in OkHttp < 3.14: only 3.14.9 was checked (`javap` shows `public okhttp3.EventListener$Factory eventListenerFactory();`).
