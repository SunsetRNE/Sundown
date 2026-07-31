package ren.sunset.sundown.hook;

import android.util.Log;

import java.lang.reflect.Member;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

/**
 * L2 native 伴生库（libsundownhook.so）的 Java 契约面。
 *
 * ⚠️ 类加载纪律（docs/l2b-plan.md §0.2 补充裁决）：
 * 本类被编译进 probe.dex 与 bridge.dex 两个产物，但**运行期只允许
 * bridge.dex 的 canonical 副本生效**（ClassLoader 父委托保证）：
 *   - bridge.dex 经 magic-mount `/system/etc/sundown/bridge.dex` 由
 *     {@link LsPlantBridge#acquireBridgeLoader()} 以单例 loader 加载一次，
 *     native 库只与 canonical NativeBridge 绑定（System.load 同路径
 *     不允许被第二个 ClassLoader 再加载）；
 *   - probe.dex 各代以该单例 loader 为父 → 父委托解析到 canonical 副本；
 *   - 桩冷启引导代（父=system CL）会解析到自己的私有副本——
 *     LsPlantBridge 据此判定为引导代并自热切换（绝不 System.load，
 *     否则独占 native 绑定导致后续所有代不可用）。
 * 因此：probe.dex 中本类的副本是**死代码**，禁止在其上调用任何 native 方法。
 *
 * 模式 vendor 自 LSPlant 官方 test（Hooker.java）：hooker_object + callback_method。
 * 本类（含成员）的改动 = bridge.dex 变化 = 需软重启生效——与 bridge.cpp 同红线。
 */
public final class NativeBridge {

    private static final String TAG = "SundownDex";
    private static final String LIB_LSPLANT = "/system/lib64/liblsplant.so";
    private static final String LIB_BRIDGE = "/system/lib64/libsundownhook.so";

    private static boolean loadAttempted = false;
    private static boolean ready = false;

    private NativeBridge() {}

    // ---- 机制出口（native 实现，JNI_OnLoad 时对 canonical 类 RegisterNatives） ----

    public static native boolean nativeInit();

    public static native String nativeGetBuildHash();

    /** 将指定 ClassLoader 持有的全部 DexFile 标记为 trusted（解除 hidden API 反射限制） */
    public static native boolean nativeMakeOwnDexTrusted(ClassLoader cl);

    /**
     * 加载伴生库并完成 lsplant::Init（只许在 canonical 副本上调用——
     * LsPlantBridge 以 ClassLoader 身份一致性判定保证）。
     * 必须在 bridge.dex 自己的 ClassLoader 上下文执行 System.load，
     * JNI_OnLoad 的 FindClass 才能解析到 canonical NativeBridge。
     */
    public static synchronized boolean ensureLoaded() {
        if (loadAttempted) return ready;
        loadAttempted = true;
        try {
            System.load(LIB_LSPLANT); // LGPL 动态库，先加载（DT_NEEDED 依赖）
        } catch (Throwable t) {
            Log.w(TAG, "liblsplant.so 加载失败: " + t);
            return false;
        }
        try {
            System.load(LIB_BRIDGE);
        } catch (Throwable t) {
            Log.w(TAG, "libsundownhook.so 加载失败: " + t);
            return false;
        }
        try {
            ready = nativeInit();
        } catch (Throwable t) {
            Log.w(TAG, "nativeInit 调用失败（RegisterNatives 未生效）: " + t);
            return false;
        }
        if (!ready) {
            Log.e(TAG, "lsplant::Init 失败（logcat tag SundownHook 取证）");
        }
        return ready;
    }

    // ---- Hooker（LSPlant 官方 test 模式 vendor） ----

    /** LSPlant 回调上下文：backup（原方法存根）+ 实参 + 目标方法 */
    public static final class MethodCallback {
        public final Method backup;
        public final Object[] args;
        public final Member target;

        MethodCallback(Method backup, Object[] args, Member target) {
            this.backup = backup;
            this.args = (args != null) ? args : new Object[0];
            this.target = target;
        }

        /**
         * 调用原方法。约定（lsplant.hpp）：非静态方法 args[0] = thisObject；
         * 静态方法无 this 占位。
         */
        public Object invokeOriginal() throws Exception {
            if (backup == null) return null;
            if (Modifier.isStatic(target.getModifiers())) {
                return backup.invoke(null, args);
            }
            Object thisObj = args.length > 0 ? args[0] : null;
            Object[] rest = new Object[args.length - 1];
            System.arraycopy(args, 1, rest, 0, rest.length);
            return backup.invoke(thisObj, rest);
        }

        /**
         * 观测 hook 语义保底：调原方法，失败返回目标返回类型的安全默认值，
         * 避免把异常/空值泄漏进 system_server（原始返回为 int 时 null 会 unbox 崩溃）。
         */
        public Object invokeOriginalOrDefault() {
            try {
                return invokeOriginal();
            } catch (Throwable t) {
                Log.e(TAG, "原方法调用失败 " + target + " -> " + t);
                return defaultReturn();
            }
        }

        /** 目标返回类型的安全默认值（void→null；原始类型→零值；引用→null） */
        public Object defaultReturn() {
            if (!(target instanceof Method)) return null;
            Class<?> rt = ((Method) target).getReturnType();
            if (rt == void.class || rt == Void.class) return null;
            if (rt == boolean.class) return Boolean.FALSE;
            if (rt == byte.class) return (byte) 0;
            if (rt == short.class) return (short) 0;
            if (rt == int.class) return 0;
            if (rt == long.class) return 0L;
            if (rt == float.class) return 0f;
            if (rt == double.class) return 0d;
            if (rt == char.class) return (char) 0;
            return null;
        }
    }

    /** 一次 hook 的句柄：backup 存根 + unhook。实例属 canonical 类（跨代共享语义） */
    public static final class Hooker {
        /** LSPlant 生成的原方法存根（doHook 成功后回填；volatile 保证回调线程可见） */
        public volatile Method backup;
        private Member target;
        private Method replacement;
        private Object owner;

        private Hooker() {}

        private native Method doHook(Member original, Method callback);

        private native boolean doUnhook(Member target);

        /** LSPlant 生成 stub 的回调签名：Object callback(Object[] args)（必须 public） */
        public Object callback(Object[] args) throws Exception {
            return replacement.invoke(owner, new MethodCallback(backup, args, target));
        }

        public boolean unhook() {
            try {
                return doUnhook(target);
            } catch (Throwable t) {
                Log.w(TAG, "unhook 失败 " + target + " -> " + t);
                return false;
            }
        }
    }

    /**
     * hook 一个方法/构造器。
     * @param target     目标 Member（反射取得，调用方已 setAccessible）
     * @param replacement 回调方法，签名 Object xxx(MethodCallback)；
     *                    实例方法则随 owner 调用，静态方法 owner 传 null
     * @param owner      replacement 的接收者（静态方法为 null）
     * @return Hooker 句柄；失败 null（调用方按单点失败降级处理）
     */
    public static Hooker hook(Member target, Method replacement, Object owner) {
        if (target == null || replacement == null) return null;
        Hooker h = new Hooker();
        h.target = target;
        h.replacement = replacement;
        h.owner = owner;
        try {
            replacement.setAccessible(true);
            Method cb = Hooker.class.getDeclaredMethod("callback", Object[].class);
            Method backup = h.doHook(target, cb);
            if (backup == null) return null;
            h.backup = backup;
            return h;
        } catch (Throwable t) {
            Log.e(TAG, "hook 失败 " + target + " -> " + t);
            return null;
        }
    }
}