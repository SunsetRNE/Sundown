package ren.sunset.sundown.hook;

import android.util.Log;

import java.io.File;
import java.util.ArrayList;
import java.util.List;

import dalvik.system.DexClassLoader;
import ren.sunset.sundown.ExemptMonitor;

/**
 * LSPlant 引擎装配（L2b）：bridge.dex 父链 + 伴生库加载 + hook 组编排。
 *
 * 类加载拓扑（docs/l2b-plan.md §0.2 补充裁决）：
 * <pre>
 *   system CL（桩冷启父链）
 *     └─ bridgeLoader（DexClassLoader@/system/etc/sundown/bridge.dex，单例）
 *          ├─ canonical NativeBridge（native 唯一绑定点）
 *          ├─ probe.dex gen1..N（InMemoryDexClassLoader，父=bridgeLoader）✅ 工作代
 *     └─ probe.dex gen0（InMemoryDexClassLoader，父=system CL）⚠️ 引导代：
 *        桩创建，看不到 canonical NativeBridge → 自热切换到工作代（Runtime 发起）
 * </pre>
 * bridgeLoader 单例跨代共享：System.getProperties() 是进程内全 loader 可见的
 * 存活全局表，用作跨 ClassLoader 单例寄存（进程局部、无外部暴露面）。
 */
public final class LsPlantBridge {

    private static final String TAG = "SundownDex";
    /** bridge.dex 落点：magic-mount，uid 1000 可读（与 probe.dex 冷启动路径同哲学） */
    private static final String BRIDGE_DEX = "/system/etc/sundown/bridge.dex";
    /** bridgeLoader 单例在 System.getProperties() 的寄存键 */
    private static final String LOADER_KEY = "sundown.l2.bridge.loader";

    /** native 状态（每代 dex 各持一份静态字段；ensureLoaded 进程内幂等） */
    private static volatile boolean nativeReady = false;
    private static volatile String bridgeHash = null;

    private LsPlantBridge() {}

    /** 伴生库 build hash（native 不可用时 null；Runtime 据此决定是否 report-bridge） */
    public static String bridgeBuildHash() {
        return bridgeHash;
    }

    /** 事件派发口：hook 命中 → 一行协议文本（"event focus pkg=..."），绝不阻塞 */
    public interface EventDispatcher {
        void dispatch(String line);
    }

    /** bridgeLoader 单例；bridge.dex 缺失返回 null（dev/旧模块场景，hook 整体降级） */
    public static ClassLoader acquireBridgeLoader() {
        Object cached = System.getProperties().get(LOADER_KEY);
        if (cached instanceof ClassLoader) return (ClassLoader) cached;
        synchronized (LsPlantBridge.class) {
            cached = System.getProperties().get(LOADER_KEY);
            if (cached instanceof ClassLoader) return (ClassLoader) cached;
            if (!new File(BRIDGE_DEX).canRead()) {
                Log.i(TAG, "bridge.dex 不可读（未交付），hook 层空转");
                return null;
            }
            try {
                ClassLoader loader = new DexClassLoader(
                        BRIDGE_DEX, null, null, ClassLoader.getSystemClassLoader());
                // 预热 canonical 类（触发加载校验，失败在此暴露而非后续 NoClassDefFoundError）
                Class.forName("ren.sunset.sundown.hook.NativeBridge", true, loader);
                System.getProperties().put(LOADER_KEY, loader);
                Log.i(TAG, "bridge.dex 已加载（canonical NativeBridge 就位）");
                return loader;
            } catch (Throwable t) {
                Log.w(TAG, "bridge.dex 加载失败: " + t);
                return null;
            }
        }
    }

    /** probe.dex 新一代的父加载器：bridgeLoader（缺失时退回 system CL，等价引导代） */
    public static ClassLoader generationParent() {
        ClassLoader bp = acquireBridgeLoader();
        return (bp != null) ? bp : ClassLoader.getSystemClassLoader();
    }

    /**
     * 当前代是否需要「引导代 → 工作代」自热切换：
     * bridgeLoader 可用，但本代的 NativeBridge 不是 canonical 副本
     * （桩冷启 gen0 父=system CL，解析到私有死代码副本，绝无 native 能力）。
     */
    public static boolean needsGenerationHop() {
        ClassLoader bp = acquireBridgeLoader();
        if (bp == null) return false; // bridge 未交付：hop 无意义（防循环）
        return NativeBridge.class.getClassLoader() != bp;
    }

    /** 装配引擎：伴生库不可用 → no-op 降级（dev/旧模块/引导代场景不阻塞闭环） */
    public static HookEngine create(EventDispatcher dispatcher, ExemptMonitor monitor) {
        if (!loadNative()) {
            return new NoopEngine();
        }
        List<HookEngine> engines = new ArrayList<>();
        engines.add(new FocusHooks(dispatcher, monitor));
        engines.add(new WakeupHooks(dispatcher));
        // v0.4.24-l3 P0：防御 hook 组（ANR 隐身 / 系统 freezer 防双冻结 / Activity 保护）
        engines.add(new DefenseHooks());
        return new CompositeEngine(engines);
    }

    private static boolean loadNative() {
        if (nativeReady) return true;
        synchronized (LsPlantBridge.class) {
            if (nativeReady) return true;
            ClassLoader bp = acquireBridgeLoader();
            if (bp == null || NativeBridge.class.getClassLoader() != bp) {
                Log.i(TAG, "canonical NativeBridge 未就位（引导代/未交付），hook 层空转");
                return false;
            }
            boolean ok;
            try {
                ok = NativeBridge.ensureLoaded();
            } catch (Throwable t) {
                Log.w(TAG, "伴生库加载异常: " + t);
                return false;
            }
            if (!ok) return false;
            try {
                bridgeHash = NativeBridge.nativeGetBuildHash();
            } catch (Throwable t) {
                bridgeHash = null;
            }
            try {
                boolean trusted =
                        NativeBridge.nativeMakeOwnDexTrusted(LsPlantBridge.class.getClassLoader());
                Log.i(TAG, "hidden API 解封: " + trusted
                        + (trusted ? "" : "（反射隐藏类可能受限，hook 解析降级）"));
            } catch (Throwable t) {
                Log.w(TAG, "hidden API 解封异常: " + t);
            }
            nativeReady = true;
            Log.i(TAG, "LSPlant 机制就绪 (bridge=" + bridgeHash + ")");
            return true;
        }
    }

    /** 组合引擎：分组 install/uninstall，单组失败不拖垮其他组 */
    private static final class CompositeEngine implements HookEngine {
        private final List<HookEngine> engines;

        CompositeEngine(List<HookEngine> engines) {
            this.engines = engines;
        }

        @Override
        public void install() {
            for (HookEngine e : engines) {
                try {
                    e.install();
                } catch (Throwable t) {
                    Log.e(TAG, "hook 组安装失败（跳过不致命）: " + e.getClass().getSimpleName()
                            + " -> " + t, t);
                }
            }
        }

        @Override
        public void uninstall() {
            for (int i = engines.size() - 1; i >= 0; i--) {
                try {
                    engines.get(i).uninstall();
                } catch (Throwable t) {
                    Log.w(TAG, "hook 组卸载异常: " + engines.get(i).getClass().getSimpleName()
                            + " -> " + t);
                }
            }
        }
    }

    /** 空转引擎：占位生命周期，保证无 native 环境全链路（安装/卸载/换代）可演练 */
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