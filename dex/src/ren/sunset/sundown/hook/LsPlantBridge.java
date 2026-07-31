package ren.sunset.sundown.hook;

import android.util.Log;

/**
 * LSPlant 软接入点（L2b 集成前的探测与降级桥）。
 *
 * 设计意图：LSPlant 的 Java API（org.lsposed.lsplant.LSPlant）与 native 库
 * 在 L2b 才随 dex / 模块交付。本桥通过反射探测其存在性：
 *   - 缺失（当前常态）→ 返回 no-op 引擎，hook 层空转，dex 其余功能不受影响
 *   - 存在（L2b 交付后）→ 走反射初始化 LSPlant 并注册真实 hook（TODO-L2b）
 *
 * 全程反射、编译期零依赖：dex 在有无 LSPlant 的环境都能跑。
 */
public final class LsPlantBridge {

    private static final String TAG = "SundownDex";
    private static final String LSPLANT_CLASS = "org.lsposed.lsplant.LSPlant";

    private LsPlantBridge() {}

    /** LSPlant 是否在当前 ClassLoader 可见（仅探测，不初始化） */
    public static boolean isAvailable() {
        try {
            Class.forName(LSPLANT_CLASS);
            return true;
        } catch (Throwable t) {
            return false;
        }
    }

    /** 创建 hook 引擎：LSPlant 缺失时降级 no-op（本阶段恒走此分支） */
    public static HookEngine create() {
        if (!isAvailable()) {
            Log.i(TAG, "LSPlant 未集成（L2b 交付），hook 层空转");
            return new NoopEngine();
        }
        // TODO-L2b: 反射调用 LSPlant.init() → 注册 AMS 焦点 / Binder 豁免 hook。
        //  注意 native 库装载：liblsplant.so 需随 dex 经 daemon socket 下发落盘到
        //  system_server 可执行映射的位置（SELinux execmod 需真机验证，见 dex/README.md）。
        Log.w(TAG, "检测到 LSPlant 类但 L2b 未交付，暂按空转处理");
        return new NoopEngine();
    }

    /** 空转引擎：占位生命周期，保证 L2a 全链路（安装/卸载/换代）可演练 */
    private static final class NoopEngine implements HookEngine {
        @Override
        public void install() {
            Log.i(TAG, "HookEngine(noop) install");
        }

        @Override
        public void uninstall() {
            Log.i(TAG, "HookEngine(noop) uninstall");
        }
    }
}