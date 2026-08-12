package ren.sunset.sundown.hook;

import android.util.Log;

/**
 * B4 dex 侧能力探测（v0.9-l3）：system_server 内 ROM 类/方法存在性矩阵。
 *
 * 与 daemon 侧 capability.rs（cgroup freezer 层级 / process_madvise / 网络源 /
 * 唤醒源基线，纯磁盘探测）互补：本类回答"注入环境里哪些 hook 目标真实存在"——
 *   - 类解析经 ServerClasses（services.jar / oplus-services.jar 不在 BOOTCLASSPATH，
 *     v0.3.4-l2 修复：Binder 服务反推 SYSTEMSERVERCLASSPATH loader）；
 *   - 方法存在性 = getDeclaredMethods 同名扫描（与 Registry.hookAllOverloads 同判据，
 *     探测结果即"该条目能 hook 到几条重载"的预测值）。
 *
 * 铁律（对齐工程红线）：
 *   - 只读探测（不 hook、不实例化、无副作用），任何异常仅记 false；
 *   - 失败安全：单项探测失败不影响其余项，整体失败返回 null 由调用方静默降级；
 *   - 上报通道 = daemon 管理面 socket 的 capability-probe 命令（L2b 上行面），
 *     daemon 原样存 state.dex_capability，经 capability status / sunctl env-check 导出；
 *   - 禁 lambda（javac -source 8 + android.jar bootclasspath 无 LambdaMetafactory）。
 */
public final class CapabilityProbe {

    private static final String TAG = "SundownDex";

    /** 探测项：{类名, 关键方法名}；方法为空 = 仅探测类存在性。
     *  清单对齐 DefenseHooks 注册表条目（def.*）的宿主类与方法名，AOSP 基座 +
     *  ColorOS 专有（HANS / Oplus 深度睡眠 / freezeAppAsyncLSP 家族）全覆盖。 */
    private static final String[][] CLASS_PROBES = {
            // ---- AOSP 基座（Android 16 标准类） ----
            {"com.android.server.am.AnrHelper", "appNotResponding"},
            {"com.android.server.am.ProcessList", "dumpStackTraces"},
            {"com.android.server.am.ActivityManagerService", "dumpStackTraces"},
            {"com.android.server.am.ActivityManagerService", "checkExcessivePowerUsageLPr"},
            {"com.android.server.am.ActiveServices", "serviceTimeout"},
            {"com.android.server.am.ProcessRecord$ProcessErrorStateRecord", "appNotResponding"},
            {"com.android.server.am.CachedAppOptimizer", "freezeApp"},
            {"com.android.server.wm.ActivityRecord", "destroyImmediately"},
            {"com.android.server.am.OomAdjuster", "applyOomAdj"},
            {"com.android.server.deviceidle.DeviceIdleController", "getAppIdWhitelistInternal"},
            {"com.android.server.wm.ActivityTaskManagerService", "removeTask"},
            {"com.android.server.wm.ActivityTaskSupervisor", "removeTask"},
            {"com.android.server.wm.TaskPersister", "removeTask"},
            {"com.android.server.am.ProcessRecord", "killLocked"},
            // ---- ColorOS 专有（HANS / Oplus 深度睡眠 / freezer 改名家族） ----
            {"com.android.server.am.CachedAppOptimizer", "freezeAppAsyncLSP"},
            {"com.android.server.am.OplusHansManager", "freezeAppForPreload"},
            {"com.android.server.hans.OplusHansProxyManager", "isProxyed"},
            {"com.android.server.hans.scene.OplusBgSceneManager", "registerGmsRestrictObserver"},
            {"com.android.server.am.OplusAppStartupManager$OplusStartupStrategy",
                    "isGoogleRestricInfoOn"},
            {"com.oplus.deepsleep.ControllerCenter", "isNeedUidDisconnectNetwork"},
    };

    private CapabilityProbe() {
    }

    /**
     * 探测全部项并序列化为 JSON（失败安全：单项异常仅记 false，不中断整体）：
     * {"probed_at":<unix秒>,"classes":[{"name":"<类>","class":true,"method":"<方法>",
     *  "method_found":false}, ...]}
     * 整体异常返回 null（调用方静默降级，不阻塞 dex 生命周期）。
     */
    public static String probeJson() {
        try {
            StringBuilder sb = new StringBuilder(512);
            sb.append("{\"probed_at\":").append(System.currentTimeMillis() / 1000)
                    .append(",\"classes\":[");
            for (int i = 0; i < CLASS_PROBES.length; i++) {
                String cls = CLASS_PROBES[i][0];
                String method = CLASS_PROBES[i][1];
                Class<?> c = ServerClasses.find(cls);
                boolean classFound = c != null;
                boolean methodFound = classFound && !method.isEmpty() && hasMethod(c, method);
                if (i > 0) sb.append(',');
                sb.append("{\"name\":\"").append(escape(cls))
                        .append("\",\"class\":").append(classFound)
                        .append(",\"method\":\"").append(escape(method))
                        .append("\",\"method_found\":").append(methodFound)
                        .append('}');
            }
            sb.append("]}");
            return sb.toString();
        } catch (Throwable t) {
            Log.w(TAG, "capability-probe 序列化失败（返回 null，调用方降级）: " + t);
            return null;
        }
    }

    /** 同名方法存在性（与 Registry.hookAllOverloads 同判据：getDeclaredMethods 扫描） */
    private static boolean hasMethod(Class<?> cls, String name) {
        try {
            for (java.lang.reflect.Method m : cls.getDeclaredMethods()) {
                if (m.getName().equals(name)) return true;
            }
        } catch (Throwable ignored) {
        }
        return false;
    }

    /** JSON 字符串转义（类名/方法名均为常量，仅防御性处理） */
    private static String escape(String s) {
        if (s == null) return "";
        return s.replace("\\", "\\\\").replace("\"", "\\\"");
    }
}
