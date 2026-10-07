#include <jni.h>
#include "jvmti.h"

#include <android/log.h>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iterator>
#include <limits>
#include <mutex>
#include <pthread.h>
#include <string>
#include <vector>

#include "slicer/instrumentation.h"
#include "slicer/reader.h"
#include "slicer/writer.h"

namespace {

constexpr char kLogTag[] = "TrafficPoliceAgent";
// Trampoline.VERSION in the boot dex this agent is built with
constexpr char kVersion[] = "0.2.0";
constexpr char kTrampolineClass[] = "io/trafficpolice/boot/Trampoline";
constexpr char kNativeBridgeClass[] = "io/trafficpolice/boot/NativeBridge";
constexpr char kTrampolineDescriptor[] = "Lio/trafficpolice/boot/Trampoline;";
constexpr char kOkHttpDescriptor[] = "Lokhttp3/OkHttpClient;";
constexpr char kUrlDescriptor[] = "Ljava/net/URL;";
constexpr char kGrpcBuilderDescriptor[] = "Lio/grpc/internal/ManagedChannelImplBuilder;";
constexpr char kGrpcOldBuilderDescriptor[] = "Lio/grpc/internal/AbstractManagedChannelImplBuilder;";
constexpr char kGrpcStubDescriptor[] = "Lio/grpc/stub/AbstractStub;";

void Log(int priority, const std::string& message) {
  __android_log_write(priority, kLogTag, message.c_str());
}

// A hook as the host sees it: its id, what it hooks, and how that went.
struct Hook {
  const char* id;
  std::string target;
  std::string status;
  std::string detail;
};

// One method a hook instruments. A hook can have several, of which one exists in a given version
// (gRPC renamed the method it needs twice).
struct Site {
  const char* hook;
  const char* klass;
  const char* method;
  const char* signature;
  // the class can exist without the method, and that is no failure (gRPC 1.34 to 1.58 keep
  // AbstractManagedChannelImplBuilder as a forwarding shim)
  bool optional;
};

std::vector<Hook> g_hooks = {
    {"url_open_connection", "java.net.URL#openConnection()Ljava/net/URLConnection;", "pending", ""},
    {"url_open_connection_proxy", "java.net.URL#openConnection(Ljava/net/Proxy;)Ljava/net/URLConnection;",
     "pending", ""},
    {"okhttp_network_interceptors", "okhttp3.OkHttpClient#networkInterceptors()Ljava/util/List;", "pending", ""},
    {"okhttp_event_listener_factory",
     "okhttp3.OkHttpClient#eventListenerFactory()Lokhttp3/EventListener$Factory;", "pending", ""},
    {"okhttp_new_websocket",
     "okhttp3.OkHttpClient#newWebSocket(Lokhttp3/Request;Lokhttp3/WebSocketListener;)Lokhttp3/WebSocket;",
     "pending", ""},
    {"grpc_channel_interceptors", "io.grpc.internal.ManagedChannelImplBuilder#getEffectiveInterceptors",
     "pending", ""},
    {"grpc_stub_channel", "io.grpc.stub.AbstractStub#getChannel()Lio/grpc/Channel;", "pending", ""},
};

const std::vector<Site> g_sites = {
    {"url_open_connection", kUrlDescriptor, "openConnection", "()Ljava/net/URLConnection;", false},
    {"url_open_connection_proxy", kUrlDescriptor, "openConnection", "(Ljava/net/Proxy;)Ljava/net/URLConnection;",
     false},
    {"okhttp_network_interceptors", kOkHttpDescriptor, "networkInterceptors", "()Ljava/util/List;", false},
    {"okhttp_event_listener_factory", kOkHttpDescriptor, "eventListenerFactory",
     "()Lokhttp3/EventListener$Factory;", false},
    // WebSockets: OkHttp sends the handshake past interceptors and listeners, so the socket the
    // app gets is wrapped (its messages, and its listener for what arrives)
    {"okhttp_new_websocket", kOkHttpDescriptor, "newWebSocket",
     "(Lokhttp3/Request;Lokhttp3/WebSocketListener;)Lokhttp3/WebSocket;", false},
    // gRPC: the interceptors of every channel built from now on (ARCHITECTURE.md §4.9)
    {"grpc_channel_interceptors", kGrpcBuilderDescriptor, "getEffectiveInterceptors",
     "(Ljava/lang/String;)Ljava/util/List;", false},                                       // 1.64 and newer
    {"grpc_channel_interceptors", kGrpcBuilderDescriptor, "getEffectiveInterceptors", "()Ljava/util/List;",
     false},                                                                              // 1.33 to 1.63
    {"grpc_channel_interceptors", kGrpcOldBuilderDescriptor, "getEffectiveInterceptors", "()Ljava/util/List;",
     true},                                                                               // 1.10 to 1.32
    // and the channel of every call a generated stub makes, which reaches channels built before
    {"grpc_stub_channel", kGrpcStubDescriptor, "getChannel", "()Lio/grpc/Channel;", false},
};

JavaVM* g_vm = nullptr;
jvmtiEnv* g_jvmti = nullptr;
jclass g_trampoline = nullptr;
jmethodID g_on_okhttp_loader = nullptr;
jmethodID g_on_grpc_loader = nullptr;
jmethodID g_on_hook = nullptr;
jmethodID g_on_diag = nullptr;
std::mutex g_hooks_mutex;
std::mutex g_loader_mutex;
jobject g_okhttp_loader = nullptr;
jobject g_grpc_loader = nullptr;
std::string g_runtime_dir;
std::string g_package_name;
bool g_startup_agent = false;
int g_api_level = 0;
std::atomic<bool> g_initialized{false};
// Whether classes are instrumented: from the moment the load hook is set up, not only once the
// runtime runs. Until 0.3.1 it waited for the runtime, and an OkHttpClient defined while the
// runtime dex was still loading stayed unhooked until the retransform pass that follows, so a
// slow device lost the first requests of an app that loads OkHttp just after an attach (the
// nightly API 36 failures, 2026-10-02 to 10-06). The trampoline passes every value through
// until the runtime installs its handler, so an early hook changes nothing.
std::atomic<bool> g_runtime_active{false};
std::atomic<bool> g_other_loader_reported{false};
std::atomic<bool> g_other_grpc_loader_reported{false};

// The Java callbacks catch their own errors, so an exception that still arrives is a bug: log it.
void ClearCallbackException(JNIEnv* env, const char* callback) {
  if (!env->ExceptionCheck()) return;
  env->ExceptionDescribe();
  env->ExceptionClear();
  Log(ANDROID_LOG_WARN, std::string("Java callback failed: ") + callback);
}

Hook* FindHook(const char* id) {
  for (Hook& hook : g_hooks) {
    if (std::strcmp(hook.id, id) == 0) return &hook;
  }
  return nullptr;
}

std::vector<const Site*> SitesForClass(const std::string& descriptor) {
  std::vector<const Site*> result;
  for (const Site& site : g_sites) {
    if (descriptor == site.klass) result.push_back(&site);
  }
  return result;
}

// "Lio/grpc/stub/AbstractStub;" and getChannel ()Lio/grpc/Channel; ->
// "io.grpc.stub.AbstractStub#getChannel()Lio/grpc/Channel;"
std::string SiteTarget(const Site& site) {
  std::string klass(site.klass);
  if (klass.size() >= 2 && klass.front() == 'L' && klass.back() == ';') klass = klass.substr(1, klass.size() - 2);
  for (char& c : klass) {
    if (c == '/') c = '.';
  }
  return klass + "#" + site.method + site.signature;
}

void NotifyHook(JNIEnv* env, const Hook& hook) {
  if (g_trampoline == nullptr || g_on_hook == nullptr) return;
  jstring id = env->NewStringUTF(hook.id);
  jstring target = env->NewStringUTF(hook.target.c_str());
  jstring status = env->NewStringUTF(hook.status.c_str());
  jstring detail = hook.detail.empty() ? nullptr : env->NewStringUTF(hook.detail.c_str());
  env->CallStaticVoidMethod(g_trampoline, g_on_hook, id, target, status, detail);
  ClearCallbackException(env, "Trampoline.onHook");
  if (detail != nullptr) env->DeleteLocalRef(detail);
  env->DeleteLocalRef(status);
  env->DeleteLocalRef(target);
  env->DeleteLocalRef(id);
}

// Sets a hook's status. A failure never replaces "installed": another of the hook's methods
// (another class, or another signature) is in place.
void SetStatus(JNIEnv* env, const char* id, const char* status, const char* detail = nullptr,
               const std::string* target = nullptr, bool force = false) {
  Hook copy{};
  bool changed = false;
  {
    std::lock_guard<std::mutex> lock(g_hooks_mutex);
    Hook* hook = FindHook(id);
    if (hook == nullptr) return;
    const std::string next_status(status != nullptr ? status : "failed");
    const std::string next_detail(detail != nullptr ? detail : "");
    if (hook->status == "installed" && next_status != "installed" && !force) return;
    if (hook->status != next_status || hook->detail != next_detail ||
        (target != nullptr && hook->target != *target)) {
      hook->status = next_status;
      hook->detail = next_detail;
      if (target != nullptr) hook->target = *target;
      copy = *hook;
      changed = true;
    }
  }
  if (changed) NotifyHook(env, copy);
}

class JvmtiAllocator final : public dex::Writer::Allocator {
 public:
  explicit JvmtiAllocator(jvmtiEnv* env) : env_(env) {}

  void* Allocate(size_t size) override {
    unsigned char* data = nullptr;
    if (env_->Allocate(static_cast<jlong>(size), &data) != JVMTI_ERROR_NONE) return nullptr;
    return data;
  }

  void Free(void* pointer) override {
    if (pointer != nullptr) env_->Deallocate(static_cast<unsigned char*>(pointer));
  }

 private:
  jvmtiEnv* env_;
};

bool SameObject(JNIEnv* env, jobject a, jobject b) {
  if (a == nullptr || b == nullptr) return a == b;
  return env->IsSameObject(a, b) == JNI_TRUE;
}

void SelectOkHttpLoader(JNIEnv* env, jobject loader) {
  if (loader == nullptr) return;
  bool selected = false;
  {
    std::lock_guard<std::mutex> lock(g_loader_mutex);
    if (g_okhttp_loader == nullptr) {
      g_okhttp_loader = env->NewGlobalRef(loader);
      selected = g_okhttp_loader != nullptr;
    } else if (!SameObject(env, g_okhttp_loader, loader)) {
      return;
    }
  }
  if (selected && g_trampoline != nullptr && g_on_okhttp_loader != nullptr) {
    env->CallStaticVoidMethod(g_trampoline, g_on_okhttp_loader, loader);
    ClearCallbackException(env, "Trampoline.onOkHttpLoader");
  }
}

bool IsSelectedOkHttpLoader(JNIEnv* env, jobject loader) {
  std::lock_guard<std::mutex> lock(g_loader_mutex);
  return g_okhttp_loader == nullptr || SameObject(env, g_okhttp_loader, loader);
}

void ReportOtherLoader(JNIEnv* env, std::atomic<bool>* reported, const char* message) {
  if (reported->exchange(true, std::memory_order_acq_rel)) return;
  Log(ANDROID_LOG_WARN, message);
  if (g_trampoline != nullptr && g_on_diag != nullptr) {
    jstring level = env->NewStringUTF("warn");
    jstring code = env->NewStringUTF("hook_other_loader");
    jstring text = env->NewStringUTF(message);
    env->CallStaticVoidMethod(g_trampoline, g_on_diag, level, code, text);
    ClearCallbackException(env, "Trampoline.onDiag");
    env->DeleteLocalRef(text);
    env->DeleteLocalRef(code);
    env->DeleteLocalRef(level);
  }
}

// One gRPC copy per process, as for OkHttp: the first loader that defines a hooked gRPC class.
bool AcceptGrpcLoader(JNIEnv* env, jobject loader) {
  if (loader == nullptr) return true;
  bool selected = false;
  bool other = false;
  {
    std::lock_guard<std::mutex> lock(g_loader_mutex);
    if (g_grpc_loader == nullptr) {
      g_grpc_loader = env->NewGlobalRef(loader);
      selected = g_grpc_loader != nullptr;
    } else {
      other = !SameObject(env, g_grpc_loader, loader);
    }
  }
  // Java is called with no lock held: what it loads comes back through the load hook
  if (other) {
    ReportOtherLoader(env, &g_other_grpc_loader_reported,
                      "another class loader also defines gRPC (io.grpc); that copy was left alone");
    return false;
  }
  if (selected && g_trampoline != nullptr && g_on_grpc_loader != nullptr) {
    env->CallStaticVoidMethod(g_trampoline, g_on_grpc_loader, loader);
    ClearCallbackException(env, "Trampoline.onGrpcLoader");
  }
  return true;
}

// One OkHttp copy per process: the first loader's copy is hooked, and a copy in another loader is
// left alone and reported once as a diagnostic. The hook statuses keep describing the first copy.
bool AcceptOkHttpLoader(JNIEnv* env, jobject loader) {
  SelectOkHttpLoader(env, loader);
  if (IsSelectedOkHttpLoader(env, loader)) return true;
  ReportOtherLoader(env, &g_other_loader_reported,
                    "another class loader also defines okhttp3.OkHttpClient; that copy was left alone");
  return false;
}

// Whether to hook this definition of a class: OkHttp's and gRPC's only in their first loader.
bool AcceptLoader(JNIEnv* env, const std::string& descriptor, jobject loader) {
  if (descriptor == kOkHttpDescriptor) return AcceptOkHttpLoader(env, loader);
  if (descriptor.rfind("Lio/grpc/", 0) == 0) return AcceptGrpcLoader(env, loader);
  return true;
}

bool InstrumentClass(JNIEnv* env, const char* class_name, jobject loader, jint data_len,
                     const unsigned char* data, jint* new_data_len,
                     unsigned char** new_data) {
  if (!g_runtime_active.load(std::memory_order_acquire) || class_name == nullptr || data == nullptr) {
    return false;
  }
  // the class names to hook are few, and every class loads through here: a cheap check first
  const char first = class_name[0];
  if (first != 'j' && first != 'o' && first != 'i') return false;
  const std::string descriptor = std::string("L") + class_name + ";";
  std::vector<const Site*> sites = SitesForClass(descriptor);
  if (sites.empty()) return false;

  if (!AcceptLoader(env, descriptor, loader)) return false;

  dex::Reader reader(data, static_cast<size_t>(data_len));
  const dex::u4 class_index = reader.FindClassIndex(descriptor.c_str());
  if (class_index == dex::kNoIndex) {
    for (const Site* site : sites) {
      if (!site->optional) SetStatus(env, site->hook, "failed", "class data did not contain the target definition");
    }
    return false;
  }

  reader.CreateClassIr(class_index);
  std::shared_ptr<ir::DexFile> dex_ir = reader.GetIr();
  bool transformed = false;
  std::vector<const Site*> installed;
  for (const Site* site : sites) {
    slicer::MethodInstrumenter instrumenter(dex_ir);
    instrumenter.AddTransformation<slicer::ExitHook>(
        ir::MethodId(kTrampolineDescriptor, "onExit"),
        slicer::ExitHook::Tweak::ReturnAsObject |
            slicer::ExitHook::Tweak::PassMethodSignature);
    if (instrumenter.InstrumentMethod(ir::MethodId(site->klass, site->method, site->signature))) {
      installed.push_back(site);
      transformed = true;
    }
  }
  // per hook: installed when one of its methods in this class was; else a failure, unless every
  // method it lacks here is optional
  for (const Site* site : sites) {
    bool hook_installed = false;
    for (const Site* done : installed) {
      if (std::strcmp(done->hook, site->hook) == 0) {
        if (!hook_installed) {
          const std::string target = SiteTarget(*done);
          SetStatus(env, done->hook, "installed", nullptr, &target);
        }
        hook_installed = true;
      }
    }
    if (!hook_installed && !site->optional) {
      SetStatus(env, site->hook, "method_not_found", "target method is absent or not instrumentable");
    }
  }
  if (!transformed) return false;

  dex::Writer writer(dex_ir);
  JvmtiAllocator allocator(g_jvmti);
  size_t output_size = 0;
  dex::u1* output = writer.CreateImage(&allocator, &output_size);
  if (output == nullptr || output_size > static_cast<size_t>(std::numeric_limits<jint>::max())) {
    if (output != nullptr) allocator.Free(output);
    // nothing of this class is hooked after all
    for (const Site* site : installed) {
      SetStatus(env, site->hook, "failed", "Slicer could not produce a valid transformed dex image", nullptr, true);
    }
    return false;
  }
  *new_data_len = static_cast<jint>(output_size);
  *new_data = output;
  return true;
}

void JNICALL OnClassFileLoadHook(jvmtiEnv*, JNIEnv* env, jclass, jobject loader,
                                 const char* name, jobject, jint data_len,
                                 const unsigned char* data, jint* new_data_len,
                                 unsigned char** new_data) {
  try {
    InstrumentClass(env, name, loader, data_len, data, new_data_len, new_data);
  } catch (const std::exception& error) {
    Log(ANDROID_LOG_ERROR, std::string("class transform failed: ") + error.what());
  } catch (...) {
    Log(ANDROID_LOG_ERROR, "class transform failed with an unknown native error");
  }
}

bool ClassDescriptor(JNIEnv* env, jclass klass, std::string* descriptor, jobject* loader) {
  char* signature = nullptr;
  char* generic = nullptr;
  if (g_jvmti->GetClassSignature(klass, &signature, &generic) != JVMTI_ERROR_NONE || signature == nullptr) {
    if (signature != nullptr) g_jvmti->Deallocate(reinterpret_cast<unsigned char*>(signature));
    if (generic != nullptr) g_jvmti->Deallocate(reinterpret_cast<unsigned char*>(generic));
    return false;
  }
  *descriptor = signature;
  g_jvmti->Deallocate(reinterpret_cast<unsigned char*>(signature));
  if (generic != nullptr) g_jvmti->Deallocate(reinterpret_cast<unsigned char*>(generic));
  if (loader != nullptr) {
    *loader = nullptr;
    if (g_jvmti->GetClassLoader(klass, loader) != JVMTI_ERROR_NONE) *loader = nullptr;
  }
  return true;
}

void Retransform(JNIEnv* env, jclass klass) {
  // API 26-27 keep the load hook off; it is on only for this thread, only for this retransform, so
  // two threads retransforming at once cannot switch it off under each other
  jthread thread = nullptr;
  if (g_api_level < 28) {
    if (g_jvmti->GetCurrentThread(&thread) != JVMTI_ERROR_NONE) thread = nullptr;
    g_jvmti->SetEventNotificationMode(JVMTI_ENABLE, JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, thread);
  }
  const jvmtiError error = g_jvmti->RetransformClasses(1, &klass);
  if (g_api_level < 28) {
    g_jvmti->SetEventNotificationMode(JVMTI_DISABLE, JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, thread);
    if (thread != nullptr) env->DeleteLocalRef(thread);
  }
  if (error != JVMTI_ERROR_NONE) {
    char* error_name = nullptr;
    std::string error_label = std::to_string(error);
    if (g_jvmti->GetErrorName(error, &error_name) == JVMTI_ERROR_NONE && error_name != nullptr) {
      error_label = error_name;
      g_jvmti->Deallocate(reinterpret_cast<unsigned char*>(error_name));
    }
    std::string message = "RetransformClasses failed: " + error_label;
    std::string descriptor;
    jobject loader = nullptr;
    if (ClassDescriptor(env, klass, &descriptor, &loader)) {
      for (const Site* site : SitesForClass(descriptor)) {
        if (!site->optional) SetStatus(env, site->hook, "failed", message.c_str(), nullptr, true);
      }
    }
    if (loader != nullptr) env->DeleteLocalRef(loader);
  }
}

void JNICALL OnClassPrepare(jvmtiEnv*, JNIEnv* env, jthread, jclass klass) {
  if (g_api_level >= 28 || !g_runtime_active.load(std::memory_order_acquire)) return;
  std::string descriptor;
  jobject loader = nullptr;
  if (!ClassDescriptor(env, klass, &descriptor, &loader)) return;
  if (!SitesForClass(descriptor).empty() && AcceptLoader(env, descriptor, loader)) {
    Retransform(env, klass);
  }
  if (loader != nullptr) env->DeleteLocalRef(loader);
}

jobjectArray JNICALL NativeSnapshot(JNIEnv* env, jclass) {
  std::vector<Hook> snapshot;
  {
    std::lock_guard<std::mutex> lock(g_hooks_mutex);
    snapshot = g_hooks;
  }
  jclass string_class = env->FindClass("java/lang/String");
  jclass string_array_class = env->FindClass("[Ljava/lang/String;");
  if (string_class == nullptr || string_array_class == nullptr) return nullptr;
  jobjectArray rows = env->NewObjectArray(static_cast<jsize>(snapshot.size()), string_array_class, nullptr);
  if (rows == nullptr) return nullptr;
  for (jsize i = 0; i < static_cast<jsize>(snapshot.size()); ++i) {
    const Hook& hook = snapshot[static_cast<size_t>(i)];
    jobjectArray row = env->NewObjectArray(4, string_class, nullptr);
    if (row == nullptr) return nullptr;
    env->SetObjectArrayElement(row, 0, env->NewStringUTF(hook.id));
    env->SetObjectArrayElement(row, 1, env->NewStringUTF(hook.target.c_str()));
    env->SetObjectArrayElement(row, 2, env->NewStringUTF(hook.status.c_str()));
    env->SetObjectArrayElement(row, 3,
                               hook.detail.empty() ? nullptr : env->NewStringUTF(hook.detail.c_str()));
    env->SetObjectArrayElement(rows, i, row);
    env->DeleteLocalRef(row);
  }
  return rows;
}

void JNICALL NativeSetActive(JNIEnv*, jclass, jboolean active) {
  g_runtime_active.store(active == JNI_TRUE, std::memory_order_release);
}

int ApiLevel(JNIEnv* env) {
  jclass version = env->FindClass("android/os/Build$VERSION");
  if (version == nullptr) {
    if (env->ExceptionCheck()) env->ExceptionClear();
    return 26;
  }
  jfieldID sdk_int = env->GetStaticFieldID(version, "SDK_INT", "I");
  if (sdk_int == nullptr) {
    if (env->ExceptionCheck()) env->ExceptionClear();
    env->DeleteLocalRef(version);
    return 26;
  }
  const jint result = env->GetStaticIntField(version, sdk_int);
  env->DeleteLocalRef(version);
  return result;
}

void ReportAll(JNIEnv* env) {
  std::vector<Hook> snapshot;
  {
    std::lock_guard<std::mutex> lock(g_hooks_mutex);
    snapshot = g_hooks;
  }
  for (const Hook& hook : snapshot) NotifyHook(env, hook);
}

void RetransformLoadedClasses(JNIEnv* env) {
  jint count = 0;
  jclass* classes = nullptr;
  if (g_jvmti->GetLoadedClasses(&count, &classes) != JVMTI_ERROR_NONE || classes == nullptr) return;
  for (jint i = 0; i < count; ++i) {
    std::string descriptor;
    jobject loader = nullptr;
    if (ClassDescriptor(env, classes[i], &descriptor, &loader) && !SitesForClass(descriptor).empty() &&
        AcceptLoader(env, descriptor, loader)) {
      Retransform(env, classes[i]);
    }
    if (loader != nullptr) env->DeleteLocalRef(loader);
    env->DeleteLocalRef(classes[i]);
  }
  g_jvmti->Deallocate(reinterpret_cast<unsigned char*>(classes));
}

bool ReadAgentConfig(const char* key, std::string* value) {
  std::ifstream config(g_runtime_dir + "/agent.conf");
  if (!config.is_open()) return false;
  std::string line;
  const std::string prefix = std::string(key) + "=";
  while (std::getline(config, line)) {
    if (line.rfind(prefix, 0) == 0) {
      *value = line.substr(prefix.size());
      if (!value->empty() && value->back() == '\r') value->pop_back();
      return true;
    }
  }
  return false;
}

bool StartupAgentIsCurrent() {
  std::string expiry_text;
  if (!ReadAgentConfig("expires_at_ms", &expiry_text) || expiry_text.empty()) return false;
  errno = 0;
  char* end = nullptr;
  const long long expires_at = std::strtoll(expiry_text.c_str(), &end, 10);
  if (errno != 0 || end == expiry_text.c_str() || *end != '\0') return false;
  const auto now = std::chrono::duration_cast<std::chrono::milliseconds>(
                       std::chrono::system_clock::now().time_since_epoch())
                       .count();
  if (expires_at <= now) return false;
  if (g_package_name.empty()) ReadAgentConfig("package", &g_package_name);
  return true;
}

void LogJavaFailure(JNIEnv* env, const char* stage) {
  if (!env->ExceptionCheck()) return;
  env->ExceptionDescribe();
  env->ExceptionClear();
  Log(ANDROID_LOG_ERROR, std::string("runtime loading failed during ") + stage);
}

void* InitializeOnAgentThread(void*) {
  JNIEnv* env = nullptr;
  if (g_vm->AttachCurrentThread(&env, nullptr) != JNI_OK || env == nullptr) {
    Log(ANDROID_LOG_ERROR, "could not attach the initialization thread to ART");
    return nullptr;
  }

  const std::string dex_path = g_runtime_dir + "/traffic-police-runtime.dex";
  std::ifstream dex_file(dex_path, std::ios::binary | std::ios::ate);
  if (!dex_file.is_open()) {
    Log(ANDROID_LOG_ERROR, "cannot open runtime dex: " + dex_path);
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  const std::streamoff dex_size = dex_file.tellg();
  if (dex_size <= 0 || dex_size > std::numeric_limits<jsize>::max()) {
    Log(ANDROID_LOG_ERROR, "runtime dex has an invalid size");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  dex_file.seekg(0, std::ios::beg);
  std::vector<jbyte> dex_bytes(static_cast<size_t>(dex_size));
  if (!dex_file.read(reinterpret_cast<char*>(dex_bytes.data()),
                      static_cast<std::streamsize>(dex_size))) {
    Log(ANDROID_LOG_ERROR, "could not read the complete runtime dex");
    g_vm->DetachCurrentThread();
    return nullptr;
  }

  jbyteArray dex_array = env->NewByteArray(static_cast<jsize>(dex_bytes.size()));
  if (dex_array == nullptr) {
    LogJavaFailure(env, "allocating the runtime dex byte array");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  env->SetByteArrayRegion(dex_array, 0, static_cast<jsize>(dex_bytes.size()), dex_bytes.data());
  if (env->ExceptionCheck()) {
    LogJavaFailure(env, "copying the runtime dex");
    g_vm->DetachCurrentThread();
    return nullptr;
  }

  jclass byte_buffer_class = env->FindClass("java/nio/ByteBuffer");
  if (byte_buffer_class == nullptr) {
    LogJavaFailure(env, "finding java.nio.ByteBuffer");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jmethodID wrap = env->GetStaticMethodID(byte_buffer_class, "wrap", "([B)Ljava/nio/ByteBuffer;");
  if (wrap == nullptr) {
    LogJavaFailure(env, "resolving ByteBuffer.wrap");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jclass dex_loader_class = env->FindClass("dalvik/system/InMemoryDexClassLoader");
  if (dex_loader_class == nullptr) {
    LogJavaFailure(env, "finding InMemoryDexClassLoader");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jmethodID dex_loader_constructor = env->GetMethodID(
      dex_loader_class, "<init>", "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V");
  if (dex_loader_constructor == nullptr) {
    LogJavaFailure(env, "resolving InMemoryDexClassLoader constructor");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jobject byte_buffer = env->CallStaticObjectMethod(byte_buffer_class, wrap, dex_array);
  if (env->ExceptionCheck() || byte_buffer == nullptr) {
    LogJavaFailure(env, "wrapping the runtime dex bytes");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  // With a null parent, the Java-side ClassLoader of older releases finds no boot classes at all
  // (API 26 cannot resolve java.lang.Object), so the runtime's parent is the boot class loader.
  jclass class_class = env->FindClass("java/lang/Class");
  if (class_class == nullptr) {
    LogJavaFailure(env, "finding java.lang.Class");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jmethodID get_class_loader =
      env->GetMethodID(class_class, "getClassLoader", "()Ljava/lang/ClassLoader;");
  if (get_class_loader == nullptr) {
    LogJavaFailure(env, "resolving Class.getClassLoader");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jclass object_class = env->FindClass("java/lang/Object");
  if (object_class == nullptr) {
    LogJavaFailure(env, "finding java.lang.Object");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jobject boot_loader = env->CallObjectMethod(object_class, get_class_loader);
  if (env->ExceptionCheck() || boot_loader == nullptr) {
    LogJavaFailure(env, "finding the boot class loader");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jobject runtime_loader =
      env->NewObject(dex_loader_class, dex_loader_constructor, byte_buffer, boot_loader);
  if (env->ExceptionCheck() || runtime_loader == nullptr) {
    LogJavaFailure(env, "creating the runtime class loader");
    g_vm->DetachCurrentThread();
    return nullptr;
  }

  jmethodID for_name = env->GetStaticMethodID(
      class_class, "forName",
      "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;");
  if (for_name == nullptr) {
    LogJavaFailure(env, "resolving Class.forName");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jstring entry_name = env->NewStringUTF("io.trafficpolice.internal.AttachEntry");
  if (entry_name == nullptr) {
    LogJavaFailure(env, "resolving the runtime entry point");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jclass entry_class = static_cast<jclass>(
      env->CallStaticObjectMethod(class_class, for_name, entry_name, JNI_TRUE, runtime_loader));
  if (env->ExceptionCheck() || entry_class == nullptr) {
    LogJavaFailure(env, "loading the runtime entry point");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jmethodID start = env->GetStaticMethodID(
      entry_class, "start",
      "(Ljava/lang/String;Ljava/lang/String;[B)Lio/trafficpolice/capture/attach/ExitHandler;");
  if (start == nullptr) {
    LogJavaFailure(env, "resolving AttachEntry.start");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  jstring directory = env->NewStringUTF(g_runtime_dir.c_str());
  jstring package_name = g_package_name.empty() ? nullptr : env->NewStringUTF(g_package_name.c_str());
  jobject handler = env->CallStaticObjectMethod(entry_class, start, directory, package_name, dex_array);
  if (env->ExceptionCheck() || handler == nullptr) {
    LogJavaFailure(env, "starting AttachEntry");
    g_vm->DetachCurrentThread();
    return nullptr;
  }

  jmethodID install = env->GetStaticMethodID(
      g_trampoline, "installHandler", "(Lio/trafficpolice/capture/attach/ExitHandler;)V");
  if (install == nullptr) {
    LogJavaFailure(env, "resolving the bootstrap handler installer");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  env->CallStaticVoidMethod(g_trampoline, install, handler);
  if (env->ExceptionCheck()) {
    LogJavaFailure(env, "installing the capture handler");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  if (g_runtime_active.load(std::memory_order_acquire)) {
    RetransformLoadedClasses(env);
    ReportAll(env);
  }
  // only now the socket: a host that connects finds every hook as it will stay (installed for
  // the classes loaded so far, pending for the rest), and the app's requests are captured from
  // the moment it can see them
  jmethodID listen = env->GetStaticMethodID(entry_class, "listen", "()V");
  if (listen == nullptr) {
    LogJavaFailure(env, "resolving AttachEntry.listen");
    g_vm->DetachCurrentThread();
    return nullptr;
  }
  env->CallStaticVoidMethod(entry_class, listen);
  if (env->ExceptionCheck()) LogJavaFailure(env, "opening the capture socket");
  g_vm->DetachCurrentThread();
  return nullptr;
}

std::string NormalizeOptions(const char* options, std::string* package_name,
                             bool* startup_agent) {
  std::string raw = options != nullptr ? options : "";
  std::string directory;
  bool explicit_directory = false;
  size_t begin = 0;
  while (begin <= raw.size()) {
    const size_t end = raw.find(';', begin);
    const std::string item = raw.substr(begin, end == std::string::npos ? end : end - begin);
    const size_t equal = item.find('=');
    if (equal != std::string::npos) {
      const std::string key = item.substr(0, equal);
      const std::string value = item.substr(equal + 1);
      if (key == "dir") {
        directory = value;
        explicit_directory = true;
      } else if (key == "package") {
        *package_name = value;
      }
    }
    if (end == std::string::npos) break;
    begin = end + 1;
  }
  if (!explicit_directory) directory = raw;
  constexpr char kSuffix[] = "/code_cache/traffic-police";
  if (explicit_directory || directory.find(kSuffix) != std::string::npos) {
    *startup_agent = false;
    return directory;
  }
  *startup_agent = true;
  while (!directory.empty() && directory.back() == '/') directory.pop_back();
  return directory + kSuffix;
}

jint Attach(JavaVM* vm, char* options) {
  if (g_initialized.exchange(true, std::memory_order_acq_rel)) {
    Log(ANDROID_LOG_WARN, "attached again; the agent that loaded first stays in charge until the app restarts");
    return JNI_OK;
  }
  g_vm = vm;
  g_runtime_dir = NormalizeOptions(options, &g_package_name, &g_startup_agent);
  // A startup agent stays in code_cache/startup_agents until it is deleted (a host that crashed
  // cannot), so a stale one must leave the process untouched: no boot dex, no load hook.
  if (g_startup_agent && !StartupAgentIsCurrent()) {
    g_initialized.store(false, std::memory_order_release);
    Log(ANDROID_LOG_INFO, "stale or incomplete startup-agent config; staying inert");
    return JNI_OK;
  }
  void* jvmti_raw = nullptr;
  if (vm->GetEnv(&jvmti_raw, JVMTI_VERSION_1_2) != JNI_OK || jvmti_raw == nullptr) {
    g_initialized.store(false, std::memory_order_release);
    Log(ANDROID_LOG_ERROR, "ART did not provide JVMTI 1.2");
    return JNI_OK;
  }
  g_jvmti = static_cast<jvmtiEnv*>(jvmti_raw);
  JNIEnv* env = nullptr;
  if (vm->GetEnv(reinterpret_cast<void**>(&env), JNI_VERSION_1_6) != JNI_OK || env == nullptr) {
    g_initialized.store(false, std::memory_order_release);
    Log(ANDROID_LOG_ERROR, "ART did not provide JNI 1.6 on the attach thread");
    return JNI_OK;
  }

  // Another copy of the agent (another path: a startup agent, then a runtime attach; or another
  // traffic-police version) already put the boot dex in place and started a runtime. Boot classes
  // cannot be replaced, so this copy leaves the process to it.
  jclass attached = env->FindClass(kTrampolineClass);
  if (attached != nullptr) {
    std::string version = "unknown";
    jfieldID field = env->GetStaticFieldID(attached, "VERSION", "Ljava/lang/String;");
    jstring text = field != nullptr ? static_cast<jstring>(env->GetStaticObjectField(attached, field)) : nullptr;
    if (text != nullptr) {
      const char* chars = env->GetStringUTFChars(text, nullptr);
      if (chars != nullptr) {
        version = chars;
        env->ReleaseStringUTFChars(text, chars);
      }
      env->DeleteLocalRef(text);
    }
    if (env->ExceptionCheck()) env->ExceptionClear();
    env->DeleteLocalRef(attached);
    Log(ANDROID_LOG_WARN, version == kVersion
                              ? "already attached in this process; this copy stays inactive"
                              : "agent " + version + " is already attached in this process; this " +
                                    kVersion + " copy stays inactive (restart the app to use it)");
    return JNI_OK;
  }
  if (env->ExceptionCheck()) env->ExceptionClear();

  g_api_level = ApiLevel(env);
  const std::string boot_dex = g_runtime_dir + "/traffic-police-boot.dex";
  const jvmtiError append_error = g_jvmti->AddToBootstrapClassLoaderSearch(boot_dex.c_str());
  if (append_error != JVMTI_ERROR_NONE) {
    Log(ANDROID_LOG_ERROR, "AddToBootstrapClassLoaderSearch failed with JVMTI error " +
                               std::to_string(append_error));
    return JNI_OK;
  }

  jclass trampoline_local = env->FindClass(kTrampolineClass);
  if (trampoline_local == nullptr) {
    if (env->ExceptionCheck()) env->ExceptionClear();
    Log(ANDROID_LOG_ERROR, "could not load the bootstrap trampoline dex");
    return JNI_OK;
  }
  g_trampoline = static_cast<jclass>(env->NewGlobalRef(trampoline_local));
  g_on_okhttp_loader = env->GetStaticMethodID(trampoline_local, "onOkHttpLoader",
                                              "(Ljava/lang/ClassLoader;)V");
  g_on_grpc_loader = env->GetStaticMethodID(trampoline_local, "onGrpcLoader", "(Ljava/lang/ClassLoader;)V");
  g_on_hook = env->GetStaticMethodID(trampoline_local, "onHook",
                                     "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V");
  g_on_diag = env->GetStaticMethodID(trampoline_local, "onDiag",
                                     "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V");
  env->DeleteLocalRef(trampoline_local);
  if (g_on_okhttp_loader == nullptr || g_on_grpc_loader == nullptr || g_on_hook == nullptr ||
      g_on_diag == nullptr) {
    if (env->ExceptionCheck()) env->ExceptionClear();
    Log(ANDROID_LOG_ERROR, "bootstrap trampoline methods are missing");
    return JNI_OK;
  }

  jclass bridge = env->FindClass(kNativeBridgeClass);
  if (bridge == nullptr) {
    if (env->ExceptionCheck()) env->ExceptionClear();
    Log(ANDROID_LOG_ERROR, "could not load the bootstrap native bridge");
    return JNI_OK;
  }
  JNINativeMethod methods[] = {
      {const_cast<char*>("snapshot"), const_cast<char*>("()[[Ljava/lang/String;"),
       reinterpret_cast<void*>(NativeSnapshot)},
      {const_cast<char*>("setActive"), const_cast<char*>("(Z)V"),
       reinterpret_cast<void*>(NativeSetActive)},
  };
  const jint registration = env->RegisterNatives(bridge, methods, 2);
  env->DeleteLocalRef(bridge);
  if (registration != JNI_OK) {
    if (env->ExceptionCheck()) env->ExceptionClear();
    Log(ANDROID_LOG_ERROR, "could not register bootstrap native methods");
    return JNI_OK;
  }

  jvmtiCapabilities capabilities{};
  capabilities.can_retransform_classes = 1;
  const jvmtiError capability_error = g_jvmti->AddCapabilities(&capabilities);
  if (capability_error != JVMTI_ERROR_NONE) {
    Log(ANDROID_LOG_ERROR, "AddCapabilities failed with JVMTI error " +
                               std::to_string(capability_error));
    for (Hook& hook : g_hooks) {
      hook.status = "failed";
      hook.detail = "ART denied retransformation capability";
    }
    ReportAll(env);
    return JNI_OK;
  }

  g_runtime_active.store(true, std::memory_order_release);
  jvmtiEventCallbacks callbacks{};
  callbacks.ClassFileLoadHook = OnClassFileLoadHook;
  callbacks.ClassPrepare = OnClassPrepare;
  if (g_jvmti->SetEventCallbacks(&callbacks, sizeof(callbacks)) != JVMTI_ERROR_NONE) {
    Log(ANDROID_LOG_ERROR, "SetEventCallbacks failed");
    return JNI_OK;
  }
  if (g_jvmti->SetEventNotificationMode(
          g_api_level >= 28 ? JVMTI_ENABLE : JVMTI_DISABLE,
          JVMTI_EVENT_CLASS_FILE_LOAD_HOOK, nullptr) != JVMTI_ERROR_NONE) {
    Log(ANDROID_LOG_ERROR, "could not configure the class-file load hook");
    return JNI_OK;
  }
  if (g_api_level < 28 &&
      g_jvmti->SetEventNotificationMode(JVMTI_ENABLE, JVMTI_EVENT_CLASS_PREPARE, nullptr) !=
          JVMTI_ERROR_NONE) {
    Log(ANDROID_LOG_ERROR, "could not enable ClassPrepare for API 26-27");
    return JNI_OK;
  }

  pthread_t thread;
  if (pthread_create(&thread, nullptr, InitializeOnAgentThread, nullptr) != 0) {
    Log(ANDROID_LOG_ERROR, "could not start agent initialization thread");
    return JNI_OK;
  }
  pthread_detach(thread);
  Log(ANDROID_LOG_INFO, "agent attached; initialization continues on a private thread");
  return JNI_OK;
}

}  // namespace

extern "C" JNIEXPORT jint JNICALL Agent_OnAttach(JavaVM* vm, char* options, void*) {
  return Attach(vm, options);
}

extern "C" JNIEXPORT jint JNICALL Agent_OnLoad(JavaVM* vm, char* options, void*) {
  return Attach(vm, options);
}

extern "C" JNIEXPORT void JNICALL Agent_OnUnload(JavaVM*) {}
