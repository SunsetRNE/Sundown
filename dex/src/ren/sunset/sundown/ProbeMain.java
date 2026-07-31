package ren.sunset.sundown;

import android.util.Log;

/**
 * L2 契约入口（与 L1 桩 probe.cpp 的硬编码签名一一对应，禁止改动签名）。
 *
 * 冷启动：桩在 system_server 内经 DexClassLoader 反射调用
 *   {@code init(String socketName, String stubBuildHash)}
 * 热切换：运行中的旧代 dex 经 InMemoryDexClassLoader 加载新代后反射调用
 *   {@code hotSwap(String socketName, String stubBuildHash, String previousVersion)}
 *
 * 跨 ClassLoader 交接只使用 bootstrap 类型（String / boolean 反射返回值），
 * 两代 dex 的类命名空间完全隔离，互不引用对方类型。
 */
public final class ProbeMain {

    private static final String TAG = "SundownDex";

    /**
     * 冷启动入口（L1 桩调用）。任何异常都不允许外抛——桩侧只有 log，没有重试。
     *
     * @param socketName    abstract namespace socket 名（当前为 "sundown_probe"）
     * @param stubBuildHash 桩的编译期 build hash（仅记录，便于日志定位桩代际）
     */
    public static void init(String socketName, String stubBuildHash) {
        try {
            Runtime.startCold(socketName, stubBuildHash);
        } catch (Throwable t) {
            // 冷启动失败不致命：桩保持驻留，后续可经 daemon 重启 / 软重启再来
            Log.e(TAG, "init 失败（桩驻留待命）: " + t, t);
        }
    }

    /**
     * 热切换入口（旧代 dex 调用）。新代在此完成全部建链与 hook 安装；
     * 成功返回 true（旧代随后自杀），失败返回 false 或抛异常（旧代原样保留 = 回滚）。
     */
    public static boolean hotSwap(String socketName, String stubBuildHash, String previousVersion) {
        try {
            return Runtime.startHot(socketName, stubBuildHash, previousVersion);
        } catch (Throwable t) {
            Log.e(TAG, "hotSwap 失败（旧代保留，回滚）: " + t, t);
            return false;
        }
    }

    private ProbeMain() {}
}
