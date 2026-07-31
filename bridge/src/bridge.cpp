// libsundownhook —— L2 native 伴生库（bridge）
//
// 定位（docs/l2b-plan.md §0.2）：只做机制不做策略。五个出口：
//   lsplant::Init / Hook / UnHook / MakeDexFileTrusted + BUILD_HASH 上报。
// 由 dex 层（system_server 内 InMemoryDexClassLoader）System.load 加载；
// JNI_OnLoad 上下文的 FindClass 解析到调用方 ClassLoader，
// 因此对 ren.sunset.sundown.hook.NativeBridge（及其内部类 Hooker）RegisterNatives。
// L1 桩（libsunprobe.so）零触碰。

#include <jni.h>

#include <android/log.h>
#include <sys/mman.h>

#include <dobby.h>
#include <lsplant.hpp>

#include "mini_art_elf.h"

#ifndef BRIDGE_BUILD_HASH
#define BRIDGE_BUILD_HASH "dev"
#endif

#define LOG_TAG "SundownHook"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, LOG_TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, LOG_TAG, __VA_ARGS__)

namespace {

bool g_init_result = false;
jfieldID g_cookie_fid = nullptr; // dalvik.system.DexFile#mCookie（Ljava/lang/Object;）

// ---- inline hooker（LSPlant 官方 test 同款接线：先置 rwx 再 DobbyHook） ----

uintptr_t page_floor(uintptr_t p) { return p & ~(uintptr_t)0xFFF; }

void make_rwx(void *p, size_t n) {
    uintptr_t start = page_floor((uintptr_t)p);
    uintptr_t end = page_floor((uintptr_t)p + n + 0xFFF);
    ::mprotect((void *)start, end - start, PROT_READ | PROT_WRITE | PROT_EXEC);
}

void *inline_hooker(void *target, void *hooker) {
    make_rwx(target, 0x1000);
    void *origin_call = nullptr;
    if (DobbyHook(target, hooker, &origin_call) == RS_SUCCESS) {
        return origin_call;
    }
    return nullptr;
}

bool inline_unhooker(void *func) { return DobbyDestroy(func) == RT_SUCCESS; }

// ---- NativeBridge 静态出口 ----

jboolean native_init(JNIEnv *, jclass) { return g_init_result ? JNI_TRUE : JNI_FALSE; }

jstring native_build_hash(JNIEnv *env, jclass) { return env->NewStringUTF(BRIDGE_BUILD_HASH); }

/// 将指定 ClassLoader 持有的全部 DexFile 标记为 trusted（解除 hidden API 反射限制）。
/// 全程 JNI 字段访问（native 侧不受 hidden API 管控）：
/// BaseDexClassLoader.pathList → DexPathList.dexElements[] → Element.dexFile → DexFile.mCookie
jboolean native_make_own_dex_trusted(JNIEnv *env, jclass, jobject class_loader) {
    if (!g_init_result || g_cookie_fid == nullptr || class_loader == nullptr) {
        return JNI_FALSE;
    }
    jclass bcl = env->FindClass("dalvik/system/BaseDexClassLoader");
    if (bcl == nullptr) return JNI_FALSE;
    jfieldID path_list_fid =
            env->GetFieldID(bcl, "pathList", "Ldalvik/system/DexPathList;");
    jobject path_list = path_list_fid ? env->GetObjectField(class_loader, path_list_fid)
                                      : nullptr;
    if (path_list == nullptr) {
        env->ExceptionClear();
        LOGE("pathList 字段解析失败");
        return JNI_FALSE;
    }
    jclass dpl = env->FindClass("dalvik/system/DexPathList");
    jfieldID elements_fid =
            dpl ? env->GetFieldID(dpl, "dexElements", "[Ldalvik/system/DexPathList$Element;")
                : nullptr;
    jobjectArray elements =
            elements_fid ? (jobjectArray)env->GetObjectField(path_list, elements_fid) : nullptr;
    if (elements == nullptr) {
        env->ExceptionClear();
        LOGE("dexElements 字段解析失败");
        return JNI_FALSE;
    }
    jclass elem_cls = env->FindClass("dalvik/system/DexPathList$Element");
    jfieldID dex_file_fid =
            elem_cls ? env->GetFieldID(elem_cls, "dexFile", "Ldalvik/system/DexFile;") : nullptr;
    if (dex_file_fid == nullptr) {
        env->ExceptionClear();
        LOGE("Element.dexFile 字段解析失败");
        return JNI_FALSE;
    }
    jsize n = env->GetArrayLength(elements);
    bool all_ok = true;
    int trusted = 0;
    for (jsize i = 0; i < n; i++) {
        jobject elem = env->GetObjectArrayElement(elements, i);
        if (elem == nullptr) continue;
        jobject dex_file = env->GetObjectField(elem, dex_file_fid);
        if (dex_file != nullptr) {
            jobject cookie = env->GetObjectField(dex_file, g_cookie_fid);
            if (cookie != nullptr) {
                if (lsplant::MakeDexFileTrusted(env, cookie)) {
                    trusted++;
                } else {
                    all_ok = false;
                }
            }
            env->DeleteLocalRef(dex_file);
        }
        env->DeleteLocalRef(elem);
    }
    LOGI("MakeDexFileTrusted: %d/%d 个 DexFile 已 trusted", trusted, (int)n);
    return (all_ok && trusted > 0) ? JNI_TRUE : JNI_FALSE;
}

// ---- NativeBridge$Hooker 实例出口 ----

jobject do_hook(JNIEnv *env, jobject thiz, jobject target, jobject callback) {
    if (!g_init_result) return nullptr;
    return lsplant::Hook(env, target, thiz, callback);
}

jboolean do_unhook(JNIEnv *env, jobject, jobject target) {
    if (!g_init_result) return JNI_FALSE;
    return lsplant::UnHook(env, target) ? JNI_TRUE : JNI_FALSE;
}

const JNINativeMethod kBridgeMethods[] = {
        {"nativeInit", "()Z", (void *)native_init},
        {"nativeGetBuildHash", "()Ljava/lang/String;", (void *)native_build_hash},
        {"nativeMakeOwnDexTrusted", "(Ljava/lang/ClassLoader;)Z",
         (void *)native_make_own_dex_trusted},
};

const JNINativeMethod kHookerMethods[] = {
        {"doHook", "(Ljava/lang/reflect/Member;Ljava/lang/reflect/Method;)"
                   "Ljava/lang/reflect/Method;", (void *)do_hook},
        {"doUnhook", "(Ljava/lang/reflect/Member;)Z", (void *)do_unhook},
};

void register_natives(JNIEnv *env, const char *cls_name, const JNINativeMethod *methods,
                      jint count) {
    jclass cls = env->FindClass(cls_name);
    if (cls == nullptr) {
        env->ExceptionClear();
        LOGE("类不可见（ClassLoader 上下文异常）: %s", cls_name);
        return;
    }
    if (env->RegisterNatives(cls, methods, count) != JNI_OK) {
        env->ExceptionClear();
        LOGE("RegisterNatives 失败: %s", cls_name);
    }
}

} // namespace

JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM *vm, void *) {
    JNIEnv *env = nullptr;
    if (vm->GetEnv((void **)&env, JNI_VERSION_1_6) != JNI_OK) {
        return JNI_ERR;
    }

    // 1) 预取 DexFile.mCookie 字段 id（官方警告：cookie 本身是 hidden API，
    //    GetFieldID 必须在 JNI_OnLoad 完成，运行期再取会被 hidden API 拦截）
    jclass dex_file_cls = env->FindClass("dalvik/system/DexFile");
    if (dex_file_cls != nullptr) {
        g_cookie_fid = env->GetFieldID(dex_file_cls, "mCookie", "Ljava/lang/Object;");
        if (g_cookie_fid == nullptr) {
            env->ExceptionClear();
            LOGE("DexFile.mCookie 解析失败，MakeDexFileTrusted 不可用");
        }
    }

    // 2) LSPlant 初始化（dobby + 自研 art elf resolver；execmod 真机验证点 V2）
    MiniArtElf art;
    if (!art.load()) {
        LOGE("libart.so 定位/解析失败，LSPlant 初始化跳过（bridge 降级不可用）");
    } else {
        lsplant::InitInfo info{
                .inline_hooker = inline_hooker,
                .inline_unhooker = inline_unhooker,
                .art_symbol_resolver =
                        [&art](std::string_view sym) { return art.resolve(sym); },
                .art_symbol_prefix_resolver =
                        [&art](std::string_view prefix) { return art.resolve_prefix(prefix); },
        };
        g_init_result = lsplant::Init(env, info);
        LOGI("lsplant::Init = %d (bridge build=%s)", g_init_result ? 1 : 0,
             BRIDGE_BUILD_HASH);
    }

    // 3) RegisterNatives（无论 Init 成败都注册：nativeInit()/nativeGetBuildHash 需可用）
    register_natives(env, "ren/sunset/sundown/hook/NativeBridge", kBridgeMethods, 3);
    register_natives(env, "ren/sunset/sundown/hook/NativeBridge$Hooker", kHookerMethods, 2);

    return JNI_VERSION_1_6;
}