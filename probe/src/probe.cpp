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
#include <string>
#include <unistd.h>
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

static constexpr const char *SOCKET_PATH = "/data/adb/sundown/sundownd.sock";
static constexpr const char *FALLBACK_DEX = "/data/adb/sundown/probe/probe.dex";
static constexpr const char *OAT_DIR = "/data/adb/sundown/probe/oat";
static constexpr const char *ENTRY_CLASS = "ren.sunset.sundown.ProbeMain";

// 与 daemon 控制面行协议通信：发一行命令，读一行 JSON 应答。失败返回空串。
static std::string sock_query(const char *cmd) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return "";

    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, SOCKET_PATH, sizeof(addr.sun_path) - 1);
    if (connect(fd, reinterpret_cast<sockaddr *>(&addr), sizeof(addr)) < 0) {
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
        run();
    }

private:
    Api *api = nullptr;
    JNIEnv *env = nullptr;
    bool do_unload = true;

    void run() {
        LOGI("L1 桩已注入 system_server (hash=%s)", PROBE_BUILD_HASH);

        // 1. hello-probe 握手：上报 build hash
        //    daemon 应答示例: {"ok":1,"hash_match":1,"expected_hash":"abc1234",
        //                      "dex_path":"/data/adb/sundown/probe/probe.dex","dex_present":0}
        std::string hello = std::string("hello-probe ") + PROBE_BUILD_HASH;
        std::string resp = sock_query(hello.c_str());
        if (resp.empty()) {
            LOGE("hello-probe 失败（daemon 未就绪？），桩驻留待命 (hash=%s)", PROBE_BUILD_HASH);
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
        load_dex(dex);
    }

    void load_dex(const std::string &dex_path) {
        jclass cl_cls = env->FindClass("dalvik/system/DexClassLoader");
        jclass class_cls = env->FindClass("java/lang/Class");
        if (!cl_cls || !class_cls) {
            if (env->ExceptionCheck()) env->ExceptionClear();
            LOGE("系统类查找失败（DexClassLoader/Class）");
            return;
        }

        jmethodID cl_ctor = env->GetMethodID(
                cl_cls, "<init>",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/ClassLoader;)V");
        jmethodID get_sys_cl = env->GetStaticMethodID(
                class_cls, "getSystemClassLoader", "()Ljava/lang/ClassLoader;");
        if (!cl_ctor || !get_sys_cl) {
            if (env->ExceptionCheck()) env->ExceptionClear();
            LOGE("DexClassLoader 方法签名解析失败");
            return;
        }
        jobject sys_cl = env->CallStaticObjectMethod(class_cls, get_sys_cl);

        jstring j_dex = env->NewStringUTF(dex_path.c_str());
        jstring j_oat = env->NewStringUTF(OAT_DIR);
        jobject loader = env->NewObject(cl_cls, cl_ctor, j_dex, j_oat, nullptr, sys_cl);
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
        jstring j_sock = env->NewStringUTF(SOCKET_PATH);
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

REGISTER_ZYGISK_MODULE(SunProbe)
