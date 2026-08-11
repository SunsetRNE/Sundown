package ren.sunset.sundown.hook;

import android.content.ComponentName;
import android.util.Log;

import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.List;

import ren.sunset.sundown.ExemptMonitor;
import ren.sunset.sundown.hook.NativeBridge.Hooker;
import ren.sunset.sundown.hook.NativeBridge.MethodCallback;

/**
 * 焦点 / 进程生命周期 hook 组（L2b 第一组，观测模式）。
 *
 * hook 点（AStop v1.6.0 dex 静态扫描实证，docs/l2b-plan.md §1）：
 *   AMS#updateActivityUsageStats —— activity 切换（resume/pause 必经之路）
 *   AMS#addPidLocked / removePidLocked —— 进程生死
 *   AMS#forceStopPackage —— 强杀信号
 *
 * 回调纪律：
 *   - 第一行起全 try-catch，任何异常不得泄漏进 system_server；
 *   - 先 invokeOriginalOrDefault 放行原语义，再提取信号；
 *   - dispatch 非阻塞（EventQueue 满则丢弃计数），AMS 锁内线程零阻塞。
 */
final class FocusHooks implements HookEngine {

    private static final String TAG = "SundownDex";
    private static final String AMS = "com.android.server.am.ActivityManagerService";
    private static final String ATMS = "com.android.server.wm.ActivityTaskManagerService";

    private final LsPlantBridge.EventDispatcher dispatcher;
    /** L3 豁免判定监视器（独立线程；focus 回调仅登记，零锁内开销） */
    private final ExemptMonitor monitor;
    private final List<Hooker> hookers = new ArrayList<>();

    FocusHooks(LsPlantBridge.EventDispatcher dispatcher, ExemptMonitor monitor) {
        this.dispatcher = dispatcher;
        this.monitor = monitor;
    }

    @Override
    public void install() {
        // v0.6-l3（缺口补入清单 B1）：注册表驱动——hook 点条目化，公共机制收敛至 Registry。
        // critical=false（观测组，失败仅跳过，保持既有行为）；注册表描述可经 env-check 导出。
        int n = Registry.installGroup(this, entries(), hookers);
        Log.i(TAG, "FocusHooks 安装完成（hook 数: " + n + "）");
    }

    /** B1 注册条目（id / 宿主 / 兜底宿主 / 方法 / 回调 / critical / 能力说明） */
    private static List<Registry.Entry> entries() {
        List<Registry.Entry> list = new ArrayList<>();
        // 焦点切换：AMS 优先，ATMS 兜底（版本迁移差异）
        list.add(Registry.entry("focus.switch", AMS, ATMS, "updateActivityUsageStats",
                "onActivitySwitch", false, "activity 切换信号（resume 权威源线索）"));
        list.add(Registry.entry("focus.pid-add", AMS, null, "addPidLocked",
                "onPidAdd", false, "进程诞生（proc-add 上行）"));
        list.add(Registry.entry("focus.pid-remove", AMS, null, "removePidLocked",
                "onPidRemove", false, "进程消亡（proc-remove 上行）"));
        list.add(Registry.entry("focus.force-stop", AMS, null, "forceStopPackage",
                "onForceStop", false, "强杀信号（force-stop 上行）"));
        return list;
    }

    @Override
    public void uninstall() {
        // v0.6-l3（B1）：卸载收敛至 Registry（幂等，单点失败不拖垮其余）
        Registry.uninstallAll(hookers);
        Log.i(TAG, "FocusHooks 已卸载");
    }

    // ---------------- hook 回调（实例方法，签名 Object xxx(MethodCallback)） ----------------

    /**
     * activity 切换信号：扫参数中的 ComponentName 取包名（签名跨版本鲁棒）。
     *
     * 噪声过滤（2026-08-02 真机实证）：updateActivityUsageStats 的 event 参数
     * 在 resume(1)/pause(2)/stopped(3) 都会触发——pause/stopped 时参数仍是
     * 正在离开的 app（OPPO ROM 实证：抖音 splash 启动瞬间 launcher 的 PAUSED
     * 事件被当作"焦点切到 launcher"上报 → 引擎误判退后台 → 误冻前台 app）。
     * 因此只把 event==ACTIVITY_RESUMED(1) 视为焦点切换候选，其余忽略。
     * event 定位：参数中最后一个值∈{1,2,3} 的 Integer（userId 在前，event 在后）；
     * 找不到（签名漂移）→ 保守照旧上报（宁多不漏）。
     *
     * 焦点去抖（v0.4.14-l3）：resume 事件在 OPPO ROM 仍存在退后台瞬间的乱序/
     * 残留（回桌面后 launcher/calculator 交替上报 → daemon last_focus 被污染 →
     * force 立即冻结被抖动解冻，真机实证）。因此 hook focus **降级为线索**：
     * 仅登记 ExemptMonitor（observe），由权威 topActivity（2s 节拍，
     * ActivityTaskManager.getTasks(1)）作为唯一焦点决策源——daemon 的
     * last_focus/decide_leave 只消费权威事件。权威源失效（连续 10s 无成功
     * 判定）时自动恢复 hook 直报兜底（宁多不漏，与降级哲学一致）。
     */
    public Object onActivitySwitch(MethodCallback cb) {
        Object res = cb.invokeOriginalOrDefault();
        try {
            String pkg = null;
            Integer event = null;
            for (Object a : cb.args) {
                if (a instanceof ComponentName) {
                    pkg = ((ComponentName) a).getPackageName();
                } else if (a instanceof Integer) {
                    int v = ((Integer) a).intValue();
                    if (v >= 1 && v <= 3) {
                        event = Integer.valueOf(v);
                    }
                }
            }
            if (pkg != null && (event == null || event.intValue() == 1)) {
                // L3：登记最近焦点（ExemptMonitor 独立线程做 fg/media 豁免判定）
                if (monitor != null) {
                    monitor.observe(pkg);
                }
                // 权威焦点源不活跃时（启动初期 / getTasks 持续失败）恢复直报兜底
                if (monitor == null || !monitor.authActive()) {
                    dispatcher.dispatch("event focus pkg=" + pkg);
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "焦点事件提取失败: " + t);
        }
        return res;
    }

    public Object onPidAdd(MethodCallback cb) {
        Object res = cb.invokeOriginalOrDefault();
        reportPid(cb, "proc-add");
        return res;
    }

    public Object onPidRemove(MethodCallback cb) {
        Object res = cb.invokeOriginalOrDefault();
        reportPid(cb, "proc-remove");
        return res;
    }

    public Object onForceStop(MethodCallback cb) {
        Object res = cb.invokeOriginalOrDefault();
        try {
            for (Object a : cb.args) {
                if (a instanceof String) {
                    dispatcher.dispatch("event force-stop pkg=" + a);
                    break;
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "force-stop 事件提取失败: " + t);
        }
        return res;
    }

    // ---------------- 工具 ----------------

    /** 从实参提取 pid：Integer 直取，否则反射 ProcessRecord.pid（低频事件，逐次反射可接受）；
     *  proc-add 附加 pkg/uid（ProcessRecord.processName/uid 纯字段读，AMS 锁内安全），
     *  缺失时 daemon 从 /proc/<pid>/status 兜底 */
    private void reportPid(MethodCallback cb, String kind) {
        try {
            for (Object a : cb.args) {
                Integer pid = extractPid(a);
                if (pid != null) {
                    String line = "event " + kind + " pid=" + pid;
                    if ("proc-add".equals(kind)) {
                        String pkg = extractProcessName(a);
                        Integer uid = extractUid(a);
                        if (pkg != null) {
                            line += " pkg=" + pkg;
                        }
                        if (uid != null) {
                            line += " uid=" + uid;
                        }
                    }
                    dispatcher.dispatch(line);
                    return;
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, kind + " 事件提取失败: " + t);
        }
    }

    private static Integer extractPid(Object arg) {
        if (arg == null) return null;
        if (arg instanceof Integer) return (Integer) arg;
        try {
            Field f = arg.getClass().getDeclaredField("pid");
            f.setAccessible(true);
            return f.getInt(arg);
        } catch (Throwable ignored) {
            return null;
        }
    }

    /** ProcessRecord.processName → 主包名（截冒号前缀；无冒号原样返回） */
    private static String extractProcessName(Object arg) {
        if (arg == null) return null;
        try {
            Field f = arg.getClass().getDeclaredField("processName");
            f.setAccessible(true);
            Object v = f.get(arg);
            if (v instanceof String) {
                String s = (String) v;
                int c = s.indexOf(':');
                return (c > 0) ? s.substring(0, c) : s;
            }
        } catch (Throwable ignored) {
            // 字段缺失/类型漂移 → 交给 daemon 兜底
        }
        return null;
    }

    /** ProcessRecord.uid（有效 uid）；无效值返回 null */
    private static Integer extractUid(Object arg) {
        if (arg == null) return null;
        try {
            Field f = arg.getClass().getDeclaredField("uid");
            f.setAccessible(true);
            int v = f.getInt(arg);
            return (v >= 0) ? Integer.valueOf(v) : null;
        } catch (Throwable ignored) {
            return null;
        }
    }
}