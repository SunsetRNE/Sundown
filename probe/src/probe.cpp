// libsunprobe.so —— Sundown L1 Zygisk 探针桩
//
// 设计约束（NAMING.md / 分层热更新定稿）：本桩"几乎不允许更新"，
// 任何变更需要软重启 zygote 生效。因此所有可变逻辑必须下沉 L2 probe.dex，
// 桩只做三件稳定的事：
//   1. 识别 system_server 并驻留（app 进程立即 DLCLOSE 自身）
//   2. hello-probe 握手：向 sundownd 上报编译期 build hash
//      （daemon 据此置 status 的 probe_stub_loaded / probe_stub_build_hash）
//   3. 按 daemon 应答加载 probe.dex 并调用入口 ProbeMain.init（L2 契约）
//      dex 缺失 / 类缺失不致命：桩保持驻留，等待 L2 推送后经 socket 通知再来
//
// ABI：仅 arm64-v8a（system_server 是纯 64 位进程；32 位 zygote 只跑 app，
// 若未来需要注入 32 位 app 进程再补 armeabi-v7a 构建）。

#include <cstring>
#include <cstddef>
#include <string>
#include <unistd.h>
#include <pthread.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <android/log.h>

#include "zygisk.hpp"

using zygisk::Api;

#define LOG_TAG "SundownProbe"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, LOG_TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, LOG_TAG, __VA_ARGS__)

#ifndef PROBE_BUILD_HASH
#define PROBE_BUILD_HASH "dev"
#endif

static constexpr const char *ABSTRACT_SOCK = "sundown_probe"; // 与 daemon paths.rs PROBE_ABSTRACT_SOCK 对齐
// 冷启动兜底 dex 路径：模块 magic-mount（全局可读、SELinux system_file 无争议）。
// 严禁指向 /data/adb 下任何路径：该目录 drwx------ root root，uid 1000 在 DAC 层
// 即 EACCES（L1 真机实证）；daemon hello-probe 应答的 dex_path 也一律指向此路径。
static constexpr const char *FALLBACK_DEX = "/system/etc/sundown/probe.dex";
static constexpr const char *ENTRY_CLASS = "ren.sunset.sundown.ProbeMain";

// 与 daemon 控制面行协议通信：发一行命令，读一行 JSON 应答。失败返回空串。
// 通道用 abstract namespace socket：/data/adb 为 drwx------ root root，
// system_server(uid 1000) 在 DAC 层即被 EACCES（无 avc），文件 socket 不可达；
// abstract socket 无文件路径，SELinux connectto ksu 已由 sepolicy.rule 放行。
static std::string sock_query(const char *cmd) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return "";

    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    // abstract namespace：sun_path 首字节 '\0' + 名字（不带路径）
    strncpy(addr.sun_path + 1, ABSTRACT_SOCK, sizeof(addr.sun_path) - 2);
    socklen_t len = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + 1 + strlen(ABSTRACT_SOCK));
    if (connect(fd, reinterpret_cast<sockaddr *>(&addr), len) < 0) {
        close(fd);
        return "";
    }

    std::string out;
    if (write(fd, cmd, strlen(cmd)) < 0 || write(fd, "\n", 1) < 0) {
        close(fd);
        return "";
    }
    char buf[2048];
    ssize_t n;
    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        out.append(buf, static_cast<size_t>(n));
        if (out.find('\n') != std::string::npos) break;
        if (out.size() > 65536) break; // 防爆：应答约定为一行短 JSON
    }
    close(fd);
    while (!out.empty() && (out.back() == '\n' || out.back() == '\r')) out.pop_back();
    return out;
}

// 从一行 JSON 提取字符串字段（极简解析：值不含转义引号的路径/哈希场景够用）
static std::string json_str(const std::string &j, const char *key) {
    std::string pat = std::string("\"") + key + "\":\"";
    auto p = j.find(pat);
    if (p == std::string::npos) return "";
    p += pat.size();
    auto e = j.find('"', p);
    if (e == std::string::npos) return "";
    return j.substr(p, e - p);
}

class SunProbe : public zygisk::ModuleBase {
public:
    void onLoad(Api *api, JNIEnv *env) override {
        this->api = api;
        this->env = env;
        env->GetJavaVM(&this->vm); // JavaVM 进程级共享：后台线程 attach 取自己的 JNIEnv
    }

    void preAppSpecialize(zygisk::AppSpecializeArgs *) override {
        // L1 只驻留 system_server；普通 app 进程立即卸载桩（零侵入）
        api->setOption(zygisk::Option::DLCLOSE_MODULE_LIBRARY);
    }

    void preServerSpecialize(zygisk::ServerSpecializeArgs *) override {
        do_unload = false; // system_server：保持加载，等 post 阶段握手
    }

    void postServerSpecialize(const zygisk::ServerSpecializeArgs *) override {
        if (do_unload) return;
        // 握手必须挪后台线程：开机时序上 daemon 要等 boot completed 后才由
        // service.sh 拉起（远晚于 system_server specialize），握手需要重试窗口，
        // 同步执行会阻塞 system_server 启动流程。
        // 安全性：preServerSpecialize 未设 DLCLOSE，本进程内桩常驻（不 unmap），
        // 后台线程持有 this 安全。
        pthread_t tid;
        if (pthread_create(&tid, nullptr, &SunProbe::run_entry, this) == 0) {
            pthread_detach(tid);
        } else {
            run(); // 线程创建失败兜底：退回同步执行（保留旧行为）
        }
    }

private:
    Api *api = nullptr;
    JNIEnv *env = nullptr;
    JavaVM *vm = nullptr;
    bool do_unload = true;

    static void *run_entry(void *self) {
        static_cast<SunProbe *>(self)->run();
        return nullptr;
    }

    void run() {
        LOGI("L1 桩已注入 system_server (hash=%s)", PROBE_BUILD_HASH);

        // 1. hello-probe 握手：上报 build hash（带重试）
        //    开机时序：system_server specialize 远早于 daemon 就绪（service.sh
        //    在 boot completed 后才拉起 sundownd），一次性握手开机时必然失败。
        //    每 2s 重试一次，窗口 120s 覆盖 daemon 拉起延迟。
        //    daemon 应答示例: {"ok":1,"hash_match":1,"expected_hash":"abc1234",
        //                      "dex_path":"/data/adb/sundown/probe/probe.dex","dex_present":0}
        std::string hello = std::string("hello-probe ") + PROBE_BUILD_HASH;
        std::string resp;
        for (int i = 1; i <= 60; ++i) {
            resp = sock_query(hello.c_str());
            if (!resp.empty()) break;
            if (i == 1) LOGI("daemon 未就绪，握手重试中（每 2s，至多 120s）");
            sleep(2);
        }
        if (resp.empty()) {
            LOGE("hello-probe 重试耗尽（120s），桩驻留待命 (hash=%s)", PROBE_BUILD_HASH);
            return;
        }
        LOGI("hello-probe 应答: %s", resp.c_str());

        // 2. 确定 dex 路径（daemon 应答优先，fallback 默认路径）
        std::string dex = json_str(resp, "dex_path");
        if (dex.empty()) dex = FALLBACK_DEX;

        if (access(dex.c_str(), R_OK) != 0) {
            LOGI("probe.dex 不存在（L2 未交付），桩驻留待命: %s", dex.c_str());
            return;
        }

        // 3. 加载 dex 并移交控制权（L2 契约；任何缺失仅告警，桩不崩）
        //    后台线程没有自己的 JNIEnv，必须 attach 获取本线程 env
        JNIEnv *t_env = nullptr;
        if (vm == nullptr || vm->AttachCurrentThread(&t_env, nullptr) != JNI_OK || t_env == nullptr) {
            LOGE("后台线程 attach JVM 失败，放弃 dex 加载");
            return;
        }
        load_dex(t_env, dex);
        vm->DetachCurrentThread();
    }

    void load_dex(JNIEnv *env, const std::string &dex_path) {
        jclass cl_cls = env->FindClass("dalvik/system/DexClassLoader");
        jclass loader_cls = env->FindClass("java/lang/ClassLoader");
        if (!cl_cls || !loader_cls) {
            if (env->ExceptionCheck()) env->ExceptionClear();
            LOGE("系统类查找失败（DexClassLoader/ClassLoader）");
            return;
        }

        jmethodID cl_ctor = env->GetMethodID(
                cl_cls, "<init>",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/ClassLoader;)V");
        // getSystemClassLoader 是 java.lang.ClassLoader 的静态方法
        // （不是 java.lang.Class 的——v0.3.0-l2 真机实证拿错类导致签名解析失败，
        //   dex 冷启动链断裂，桩静默驻留）
        jmethodID get_sys_cl = env->GetStaticMethodID(
                loader_cls, "getSystemClassLoader", "()Ljava/lang/ClassLoader;");
        if (!cl_ctor || !get_sys_cl) {
            if (env->ExceptionCheck()) env->ExceptionClear();
            LOGE("DexClassLoader 方法签名解析失败");
            return;
        }
        jobject sys_cl = env->CallStaticObjectMethod(loader_cls, get_sys_cl);

        jstring j_dex = env->NewStringUTF(dex_path.c_str());
        // optimizedDirectory 传 nullptr：由 ART 自选 oat 落点（dalvik-cache，
        // system_server 可写）。严禁显式指向 /data/adb 下目录——uid 1000 DAC 不可达，
        // 真机会导致 dexopt 失败/降级（v0.3.0-l2 真机排障结论）。
        jobject loader = env->NewObject(cl_cls, cl_ctor, j_dex, nullptr, nullptr, sys_cl);
        if (env->ExceptionCheck() || !loader) {
            if (env->ExceptionCheck()) { env->ExceptionDescribe(); env->ExceptionClear(); }
            LOGE("DexClassLoader 创建失败: %s", dex_path.c_str());
            return;
        }

        jmethodID load_class = env->GetMethodID(
                cl_cls, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;");
        jstring j_name = env->NewStringUTF(ENTRY_CLASS);
        jobject entry = env->CallObjectMethod(loader, load_class, j_name);
        if (env->ExceptionCheck() || !entry) {
            if (env->ExceptionCheck()) env->ExceptionClear();
            LOGI("入口类 %s 不存在（L2 未交付），桩驻留待命", ENTRY_CLASS);
            return;
        }

        // L2 契约：ProbeMain.init(String socketPath, String stubBuildHash)
        // dex 层自此接管（连 daemon 订阅事件、热切换、Hook 编排），桩不再参与
        auto *entry_cls = static_cast<jclass>(entry);
        jmethodID init = env->GetStaticMethodID(
                entry_cls, "init", "(Ljava/lang/String;Ljava/lang/String;)V");
        if (!init) {
            if (env->ExceptionCheck()) env->ExceptionClear();
            LOGE("ProbeMain.init 签名不匹配，期望 (String, String)V");
            return;
        }
        // L2 契约第一参为 abstract socket 名（Java 侧用 LocalSocketAddress ABSTRACT 连接）
        jstring j_sock = env->NewStringUTF(ABSTRACT_SOCK);
        jstring j_hash = env->NewStringUTF(PROBE_BUILD_HASH);
        env->CallStaticVoidMethod(entry_cls, init, j_sock, j_hash);
        if (env->ExceptionCheck()) {
            env->ExceptionDescribe();
            env->ExceptionClear();
            LOGE("ProbeMain.init 执行异常");
            return;
        }
        LOGI("probe.dex 已接管（L2 逻辑上线）");
    }
};

// ---- tombstone_15 根治：自定义 zygisk_module_entry（2026-08-03）----
// 事故：zygote64 启动 4s SIGSEGV（pc=0x4ab00 = .plt 基址偏移，未加映射基址）。
// 根因：rezygisk 在 zygote fork 早期的加载路径跳过 .init_array 且 JUMP_SLOT GOT
//       未完成基址修正（lazy 占位 = plt0 偏移）。原 REGISTER_ZYGISK_MODULE 宏展开的
//       entry_impl<clazz>() 内含三个 static 局部变量（Api/T/module_abi），其 guard
//       初始化触发 `bl __cxa_guard_acquire@plt` —— 首次 PLT 调用即跳到未加基址的
//       plt0 → SIGSEGV。运行期（后台线程）GOT 已解析，故仅入口早期窗口会炸。
// 修复：入口零 PLT 依赖——全局对象（bss 零初始化，无 .init_array/无 guard）、
//       abi 栈上构造（构造函数体为成员赋值 + 无捕获 lambda，零外部符号）、
//       仅间接调用（api_table 函数指针表 + 虚函数，均经 RELRO RELATIVE 重定位）。
// 双保险：CMakeLists 增加 -fno-threadsafe-statics（未来 static 局部也不生成 guard）。
namespace {
SunProbe g_probe; // 静态存储期：零初始化先于一切（NSDMI 与零值语义等价，见下注释）
// do_unload 注意：NSDMI=true 依赖构造执行；若 .init_array 未执行，零初始化=false。
// 语义校验：preAppSpecialize 无条件设 DLCLOSE（app 卸载路径不依赖 do_unload）；
// postServerSpecialize 仅 system_server 走，do_unload=false 恰为所需值。安全。
}

extern "C" __attribute__((visibility("default")))
void zygisk_module_entry(zygisk::internal::api_table *table, JNIEnv *env) {
    // 复刻 zygisk::internal::entry_impl 语义，但去掉全部 static 局部（零 guard PLT）
    Api api;               // 平凡构造（无成员初始化代码）
    api.tbl = table;
    internal::module_abi abi(&g_probe); // 栈上构造：成员赋值 + 无捕获 lambda，零 PLT
    if (!table->registerModule(table, &abi)) return;
    g_probe.onLoad(&api, env);
}
