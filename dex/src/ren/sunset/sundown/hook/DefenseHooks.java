package ren.sunset.sundown.hook;

import android.util.Log;

import java.io.File;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

import ren.sunset.sundown.hook.NativeBridge.Hooker;
import ren.sunset.sundown.hook.NativeBridge.MethodCallback;

/**
 * P0 防御 hook 组（v0.4.24-l3，对齐 AStop TimeoutAndAnrHooks / SystemDefenseHooks /
 * ActivityProtectionHooks 的 Sundown 裁剪版）。
 *
 * 目标：冻结 app 在系统视角"活但慢"——永不判 ANR、系统 freezer 不二次冻结、
 * 冻结期 activity 不被回收。直击 v0.4.22-l3 实机"点击无响应→闪退"痛点。
 *
 * 冻结集数据源：**直接扫描 /sys/fs/cgroup/apps 下 uid_* 目录的 cgroup.freeze==1**
 * （PJD110/Android16 实机实证：cgroup2 目录 system:system rwxr-xr-x，system_server
 * uid=1000 可读可遍历，114 个 uid 目录；免 daemon↔dex 协议同步——dex 侧与 daemon
 * 冻结状态天然一致，daemon 崩溃/重启也不失真）。
 * 独立线程 2s 节拍刷新（对齐 ExemptMonitor 节拍纪律），快照原子替换（hook 线程零锁）。
 *
 * hook 点（AStop smali 静态扫描实证 + Android16 AOSP 签名）：
 *   AnrHelper#appNotResponding            —— ANR 流程上游阻断（冻结 uid → 直接 return）
 *   ProcessList#dumpStackTraces           —— firstPids 剔除冻结 pid（A13+ 迁移至 ProcessList，AMS 兜底）
 *   ActiveServices#serviceTimeout         —— 冻结 app 的 service 超时不判 ANR
 *   ActiveServices#serviceForegroundTimeout —— 冻结 app 的前台 service 超时豁免
 *   ProcessRecord$ProcessErrorStateRecord —— ANR 二次拦截（探测方法名，找不到降级）
 *   CachedAppOptimizer#freezeApp          —— 系统 freezer 不冻被管 app（防双冻结竞争）
 *   ActivityRecord#destroyImmediately     —— 冻结期 activity 不回收（防解冻后黑屏/重建）
 *
 * 回调纪律（与 FocusHooks/WakeupHooks 一致）：
 *   - 第一行起全 try-catch，任何异常不得泄漏进 system_server；
 *   - 阻断场景：命中冻结 → 不 invokeOriginal，直接 return 安全默认值（阻断 ANR/超时/冻结）；
 *   - 过滤场景：修改实参（List 引用）后 invokeOriginalOrDefault 放行原语义；
 *   - hook 单点失败仅 logw（hookAllOverloads 纪律），不拖垮其他 hook。
 */
public final class DefenseHooks implements HookEngine {

    private static final String TAG = "SundownDex";
    private static final String CGROUP_APPS = "/sys/fs/cgroup/apps";
    private static final long SCAN_INTERVAL_MS = 2000;

    private static final String ANR_HELPER = "com.android.server.am.AnrHelper";
    private static final String PROCESS_LIST = "com.android.server.am.ProcessList";
    private static final String AMS = "com.android.server.am.ActivityManagerService";
    private static final String ACTIVE_SERVICES = "com.android.server.am.ActiveServices";
    private static final String PESR = "com.android.server.am.ProcessRecord$ProcessErrorStateRecord";
    private static final String CACHED_APP_OPTIMIZER = "com.android.server.am.CachedAppOptimizer";
    private static final String ACTIVITY_RECORD = "com.android.server.wm.ActivityRecord";
    // v0.4.51-l3：Recents 任务保护（对齐 AStop TaskHooks）——removeTask 移除拦截
    // 实测校准（2026-08-05）：AStop hook 目标是 ActivityTaskManagerService#removeTask
    // （Recents 滑卡链路 AMS.removeTask(binder) → ATMS.removeTask → 内部清理）；
    // 仅 hook ActivityTaskSupervisor 在 ColorOS 上匹配不到重载（hookAllOverloads=0）。
    private static final String ACTIVITY_TASK_MANAGER = "com.android.server.wm.ActivityTaskManagerService";
    private static final String TASK_SUPERVISOR = "com.android.server.wm.ActivityTaskSupervisor";
    // 2026-08-05 定位：activity_task binder 分发器（AIDL Stub.onTransact 观测事务 code）
    private static final String ACTIVITY_TASK_STUB = "android.app.IActivityTaskManager$Stub";
    // 2026-08-05 定位：activity（AMS）binder 分发器 + TaskPersister（Recents 持久化清理）
    private static final String ACTIVITY_STUB = "android.app.IActivityManager$Stub";
    private static final String TASK_PERSISTER = "com.android.server.wm.TaskPersister";
    // v0.4.51-l3 实测补丁：ColorOS 滑卡/清 Recents 走 o-stop(40) 直接杀进程（非 removeTask）——
    // 任务移除是杀进程后果；对齐 AStop hookProcessRecordKill（ProcessRecord#killLocked）
    private static final String PROCESS_RECORD = "com.android.server.am.ProcessRecord";
    // v0.4.26-l3：ColorOS HANS 防御（对齐 AStop RomCompatHooks，语义裁剪为冻结集防御）
    private static final String HANS_MANAGER = "com.android.server.am.OplusHansManager";
    private static final String HANS_PROXY = "com.android.server.hans.OplusHansProxyManager";
    private static final String BG_SCENE = "com.android.server.hans.scene.OplusBgSceneManager";
    private static final String STARTUP_STRATEGY = "com.android.server.am.OplusAppStartupManager$OplusStartupStrategy";
    // v0.4.39-l3：P1⑨ 防御补全（对齐 AStop OomAdjusterHooks / SystemDefenseHooks / DeviceIdleWhitelistHooks）
    private static final String OOM_ADJUSTER = "com.android.server.am.OomAdjuster";
    private static final String DEVICE_IDLE = "com.android.server.deviceidle.DeviceIdleController";
    // v0.4.44-l3：P2⑮ break_network（对齐 AStop OplusDeepSleepHooks——ColorOS 深度睡眠 uid 断网）
    private static final String OPLUS_DEEPSLEEP = "com.oplus.deepsleep.ControllerCenter";

    /** 冻结 uid 快照（扫描线程原子替换，hook 线程只读，零锁） */
    private static final class FrozenSet {
        private volatile Set<Integer> uids = new HashSet<Integer>();

        boolean contains(int uid) {
            return uids.contains(Integer.valueOf(uid));
        }

        void replace(Set<Integer> fresh) {
            uids = fresh;
        }
    }

    /** v0.4.27-l3 双源冻结集：
     *  - sundownFrozen：daemon frozen-sync 推送的权威集（Sundown 自己冻的）——
     *    HANS 解冻/冻结拦截只信这个（防误伤：HANS 解冻它自己冻的 app 必须放行，
     *    2026-08-03 误伤事故：误拦 HANS 解冻微信致卡冻结）
     *  - cgroupFrozen：cgroup 扫描兜底（daemon 断连时；ANR/Activity 保护用并集——
     *    cgroup 冻着 = 系统视角冻结，ANR 判断应豁免，无论谁冻的）
     * 本代实例经 INSTANCE 暴露给 Runtime（frozen-sync 事件/hello 应答更新）。 */
    public static volatile DefenseHooks INSTANCE;

    private final Object sundownLock = new Object();
    private volatile Set<Integer> sundownFrozen = new HashSet<Integer>();
    // v0.4.48-l3：候选池（frozen + grace + adj_keep 并集，daemon candidate-sync 下行）——
    // 对齐 AStop hookSystemFreezer"被管 app"语义：系统冻结器/HANS/杀进程/OOM 防御
    // 对候选池全部生效（防系统在 grace 期抢冻候选 app → freeze_binder 挂起 → 黑屏）
    private final Object candidateLock = new Object();
    private volatile Set<Integer> candidateSet = new HashSet<Integer>();
    private final FrozenSet frozen = new FrozenSet();
    private final List<Hooker> hookers = new ArrayList<>();
    private final Thread scannerThread;
    private volatile boolean stopped;

    DefenseHooks() {
        INSTANCE = this; // 本代实例注册（Runtime 经此更新 frozen-sync 权威集）
        scannerThread = new Thread(new Runnable() {
            @Override
            public void run() {
                scanLoop();
            }
        }, "SundownDex-FrozenScan");
        scannerThread.setDaemon(true);
    }

    /** v0.4.27-l3：daemon frozen-sync 权威集更新（Runtime 事件线程调用；原子替换零锁） */
    public void updateSundownSet(Set<Integer> fresh) {
        synchronized (sundownLock) {
            sundownFrozen = (fresh == null) ? new HashSet<Integer>() : fresh;
        }
    }

    /** v0.4.48-l3：daemon candidate-sync 候选池更新（Runtime 事件线程调用；原子替换零锁） */
    public void updateCandidateSet(Set<Integer> fresh) {
        synchronized (candidateLock) {
            candidateSet = (fresh == null) ? new HashSet<Integer>() : fresh;
        }
    }

    /** Sundown 自己冻结的 uid（权威集；HANS 解冻/冻结拦截专用判定） */
    private boolean sundownFrozen(int uid) {
        return sundownFrozen.contains(Integer.valueOf(uid));
    }

    /** 并集判定（cgroup 兜底 ∪ daemon 权威）：系统视角冻结即命中（ANR/Activity 保护用） */
    private boolean anyFrozen(int uid) {
        return frozen.contains(uid) || sundownFrozen(uid);
    }

    @Override
    public void install() {
        // B1 迁移（v0.9-l3）：hook 点全部条目化（Registry.installGroup）——类解析/
        // 回调查找/重载 hook 收敛到注册表统一机制（status/env-check 可枚举、可观测）。
        // 全部条目 critical=false：保持既有"失败跳过"语义（ColorOS 专有条目在 AOSP
        // 设备必然失败——freezeAppAsyncLSP 家族/HANS——绝不能整组回滚；critical
        // 回滚语义留给未来显式评估，本版零行为变化）。
        int n = Registry.installGroup(this, entries(), hookers);

        // 启动冻结集扫描线程（install 一次；uninstall 停）
        if (!scannerThread.isAlive()) {
            scannerThread.start();
        }
        Log.i(TAG, "DefenseHooks 安装完成（注册条目 hook 重载数: " + n + "）");
    }

    /** B1 注册表条目（v0.9-l3）：hook 点全量描述（对齐 CapabilityProbe 探测清单）。
     *  id 语义：def.<域>-<方法>；capability 字段供 env-check 注册表导出面展示。 */
    private static List<Registry.Entry> entries() {
        List<Registry.Entry> list = new ArrayList<Registry.Entry>();
        // 1. ANR 流程上游阻断（直击"点击无响应→闪退"）
        list.add(Registry.entry("def.anr", ANR_HELPER, null, "appNotResponding",
                "onAppNotResponding", false, "冻结 uid 阻断 ANR 判定"));
        // 2. ANR stack 转储 firstPids 剔除冻结 pid（A13+ ProcessList，AMS 兜底）——
        //    双条目（非 critical）：原代码按方法名探测二选一，注册表 fallbackHost 只
        //    覆盖"类找不到"，故拆两条目（v0.4.62-l3 实机校准：ColorOS ProcessList 类在
        //    但 dumpStackTraces 方法不存在 → def.stack-dump-pl 跳过，AMS 条目兜底）
        list.add(Registry.entry("def.stack-dump-pl", PROCESS_LIST, null, "dumpStackTraces",
                "onDumpStackTraces", false, "firstPids 剔除冻结 pid"));
        list.add(Registry.entry("def.stack-dump-ams", AMS, null, "dumpStackTraces",
                "onDumpStackTraces", false, "firstPids 剔除冻结 pid（AMS 兜底）"));
        // 3. Service 超时豁免（冻结 app 不判 ANR）
        list.add(Registry.entry("def.service-timeout", ACTIVE_SERVICES, null, "serviceTimeout",
                "onServiceTimeout", false, "冻结 app 的 service 超时不判 ANR"));
        list.add(Registry.entry("def.service-fg-timeout", ACTIVE_SERVICES, null,
                "serviceForegroundTimeout", "onServiceForegroundTimeout", false,
                "冻结 app 前台 service 超时豁免"));
        // 4. ProcessErrorStateRecord 二次拦截（双方法条目：appNotResponding 优先 +
        //    setNotResponding 兜底——原"方法名探测二选一"等价迁移，同回调无副作用）
        list.add(Registry.entry("def.pesr-anr", PESR, null, "appNotResponding",
                "onPesrAnr", false, "ANR 二次拦截"));
        list.add(Registry.entry("def.pesr-set-anr", PESR, null, "setNotResponding",
                "onPesrAnr", false, "ANR 二次拦截（方法名兜底）"));
        // 5. 系统 freezer 防双冻结（AOSP freezeApp + ColorOS 改名家族 + HANS 入口）
        list.add(Registry.entry("def.freeze-app", CACHED_APP_OPTIMIZER, null, "freezeApp",
                "onFreezeApp", false, "系统 freezer 不冻被管 app"));
        // 7a. ColorOS 改名 freeze 入口（锁内方法，全重载）
        list.add(Registry.entry("def.sysfreeze-lsp", CACHED_APP_OPTIMIZER, null,
                "freezeAppAsyncLSP", "onSystemFreeze", false, "ColorOS 系统冻结入口"));
        list.add(Registry.entry("def.sysfreeze-lsp-internal", CACHED_APP_OPTIMIZER, null,
                "freezeAppAsyncInternalLSP", "onSystemFreeze", false, "ColorOS 系统冻结入口"));
        list.add(Registry.entry("def.sysfreeze-lsp-earliest", CACHED_APP_OPTIMIZER, null,
                "freezeAppAsyncAtEarliestLSP", "onSystemFreeze", false, "ColorOS 系统冻结入口"));
        list.add(Registry.entry("def.sysfreeze-lsp-immediate", CACHED_APP_OPTIMIZER, null,
                "freezeAppAsyncImmediateLSP", "onSystemFreeze", false, "ColorOS 系统冻结入口"));
        list.add(Registry.entry("def.sysfreeze-binder", CACHED_APP_OPTIMIZER, null,
                "freezeBinder", "onSystemFreeze", false, "系统冻结入口"));
        list.add(Registry.entry("def.sysfreeze-binder-pkg", CACHED_APP_OPTIMIZER, null,
                "freezeBinderAndPackageCgroup", "onSystemFreeze", false, "系统冻结入口"));
        list.add(Registry.entry("def.sysfreeze-pkg", CACHED_APP_OPTIMIZER, null,
                "freezePackageCgroup", "onSystemFreeze", false, "系统冻结入口"));
        // 7b. HANS 主动冻结入口（冻结集内 uid 阻止二次冻结）
        list.add(Registry.entry("def.hans-freeze-preload", HANS_MANAGER, null,
                "freezeAppForPreload", "onSystemFreeze", false, "HANS 冻结入口"));
        list.add(Registry.entry("def.hans-freeze-all", HANS_MANAGER, null,
                "freezeAllProcess", "onSystemFreeze", false, "HANS 冻结入口"));
        list.add(Registry.entry("def.hans-freeze-cgroup", HANS_MANAGER, null,
                "freezeCgroupUid", "onSystemFreeze", false, "HANS 冻结入口"));
        // 7c. HANS 解冻防御（防 HANS 解掉 Sundown 冻结态——墓碑失效/假活）
        list.add(Registry.entry("def.hans-unfreeze-kernel", HANS_MANAGER, null,
                "unfreezeForKernel", "onHansUnfreeze", false, "防 HANS 解掉 Sundown 冻结态"));
        list.add(Registry.entry("def.hans-unfreeze-pid", HANS_MANAGER, null,
                "unfreezeForKernelTargetPid", "onHansUnfreeze", false, "防 HANS 解掉 Sundown 冻结态"));
        list.add(Registry.entry("def.hans-unfreeze-min", HANS_MANAGER, null,
                "unfreezeAppforHansMinSystem", "onHansUnfreeze", false, "防 HANS 解掉 Sundown 冻结态"));
        // 7d. HANS Proxy 防御（冻结集内 uid 不被 HANS 代理干预）
        list.add(Registry.entry("def.hans-proxyed", HANS_PROXY, null, "isProxyed",
                "onHansIsProxyed", false, "冻结 uid 不被 HANS 代理干预"));
        // 7e. ColorOS GMS 限制禁用（对齐 AStop DO_NOTHING）
        list.add(Registry.entry("def.gms-restrict-observer", BG_SCENE, null,
                "registerGmsRestrictObserver", "onNoop", false, "禁用 GMS 限制"));
        list.add(Registry.entry("def.gms-restrict-update", BG_SCENE, null,
                "updateGmsRestrict", "onNoop", false, "禁用 GMS 限制"));
        list.add(Registry.entry("def.gms-startup-info", STARTUP_STRATEGY, null,
                "isGoogleRestricInfoOn", "onReturnFalse", false, "GMS 限制信息关闭"));
        // 8. OomAdjuster 防御（候选池跳过 adj 重算 + 写入锁 -1000）
        list.add(Registry.entry("def.oom-compute-lsp", OOM_ADJUSTER, null, "applyOomAdjLSP",
                "onOomAdjCompute", false, "候选池跳过 adj 重算"));
        list.add(Registry.entry("def.oom-compute", OOM_ADJUSTER, null, "applyOomAdj",
                "onOomAdjCompute", false, "候选池跳过 adj 重算"));
        list.add(Registry.entry("def.oom-apply", PROCESS_LIST, null, "setOomAdj",
                "onOomAdjApply", false, "候选池 pid adj 锁 -1000"));
        // 9. 耗电判定豁免（冻结集内豁免，防"耗电异常"杀冻结 app）
        list.add(Registry.entry("def.excessive-power", AMS, null, "checkExcessivePowerUsageLPr",
                "onExcessivePower", false, "冻结 uid 豁免耗电判定"));
        // 10. Doze 白名单注入（冻结 uid 注入白名单数组）
        list.add(Registry.entry("def.doze-whitelist", DEVICE_IDLE, null,
                "getAppIdWhitelistInternal", "onAppIdWhitelist", false, "冻结 uid 注入白名单"));
        list.add(Registry.entry("def.doze-user-whitelist", DEVICE_IDLE, null,
                "getAppIdUserWhitelistInternal", "onAppIdWhitelist", false, "冻结 uid 注入白名单"));
        // 11. break_network（ColorOS Oplus 深度睡眠 uid 断网判定）
        list.add(Registry.entry("def.deepsleep-net", OPLUS_DEEPSLEEP, null,
                "isNeedUidDisconnectNetwork", "onNeedUidDisconnect", false, "冻结 uid 断网"));
        // 12. Recents 任务保护（候选池 app 的 Task 不被移除/清空）
        list.add(Registry.entry("def.task-remove-atms", ACTIVITY_TASK_MANAGER, null,
                "removeTask", "onTaskRemove", false, "候选池任务不被移除"));
        list.add(Registry.entry("def.task-remove-supervisor", TASK_SUPERVISOR, null,
                "removeTask", "onTaskRemove", false, "候选池任务不被移除"));
        list.add(Registry.entry("def.task-removeall-atms", ACTIVITY_TASK_MANAGER, null,
                "removeAllVisibleRecentTasks", "onRemoveAllVisible", false, "候选池任务不被清空"));
        list.add(Registry.entry("def.task-removeall-supervisor", TASK_SUPERVISOR, null,
                "removeAllVisibleRecentTasks", "onRemoveAllVisible", false, "候选池任务不被清空"));
        list.add(Registry.entry("def.task-persister-remove", TASK_PERSISTER, null,
                "removeTask", "onTaskPersisterRemove", false, "Recents 持久化移除观测"));
        // 13. killProcess 拦截（OOM/内存压力杀进程路径）
        list.add(Registry.entry("def.kill-process", CACHED_APP_OPTIMIZER, null,
                "killProcess", "onKillProcess", false, "候选池进程不被 OOM 杀"));
        list.add(Registry.entry("def.kill-locked", PROCESS_RECORD, null,
                "killLocked", "onKillLocked", false, "候选池进程不被 killLocked 杀"));
        return list;
    }

    @Override
    public void uninstall() {
        stopped = true;
        for (Hooker h : hookers) {
            try {
                h.unhook();
            } catch (Throwable t) {
                Log.w(TAG, "unhook 异常: " + t);
            }
        }
        hookers.clear();
        Log.i(TAG, "DefenseHooks 已卸载");
    }

    // ---------------- 冻结集扫描 ----------------

    private void scanLoop() {
        while (!stopped) {
            try {
                Set<Integer> fresh = new HashSet<Integer>();
                File apps = new File(CGROUP_APPS);
                File[] dirs = apps.listFiles();
                if (dirs != null) {
                    for (File d : dirs) {
                        String name = d.getName();
                        if (!name.startsWith("uid_")) continue;
                        int uid = parseUid(name);
                        if (uid < 0) continue;
                        if (isFrozen(d)) {
                            fresh.add(Integer.valueOf(uid));
                        }
                    }
                }
                frozen.replace(fresh);
            } catch (Throwable t) {
                Log.w(TAG, "冻结集扫描失败（保留旧快照）: " + t);
            }
            try {
                Thread.sleep(SCAN_INTERVAL_MS);
            } catch (InterruptedException ignored) {
                return;
            }
        }
    }

    private static int parseUid(String dirName) {
        String s = dirName.substring(4);
        if (s.isEmpty()) return -1;
        for (int i = 0; i < s.length(); i++) {
            if (!Character.isDigit(s.charAt(i))) return -1;
        }
        try {
            return Integer.parseInt(s);
        } catch (Throwable ignored) {
            return -1;
        }
    }

    private static boolean isFrozen(File uidDir) {
        try {
            File f = new File(uidDir, "cgroup.freeze");
            String v = readTrimmed(f);
            return "1".equals(v);
        } catch (Throwable ignored) {
            return false;
        }
    }

    private static String readTrimmed(File f) {
        try {
            java.io.FileInputStream in = new java.io.FileInputStream(f);
            try {
                byte[] buf = new byte[16];
                int n = in.read(buf);
                if (n <= 0) return "";
                return new String(buf, 0, n, "UTF-8").trim();
            } finally {
                in.close();
            }
        } catch (Throwable t) {
            return "";
        }
    }

    // ---------------- hook 回调 ----------------

    /** ANR 上游阻断：ProcessRecord 参数 uid 冻结中 → 不触发 ANR 流程（return null） */
    public Object onAppNotResponding(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                Integer uid = extractUid(a);
                if (uid != null && anyFrozen(uid.intValue())) {
                    Log.i(TAG, "ANR 阻断（冻结中 uid=" + uid + "）: " + cb.target.getName());
                    return cb.defaultReturn(); // void → null：阻断 ANR 判定
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "ANR 阻断判定异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** firstPids 过滤：剔除冻结 pid（pid→uid 经 /proc/<pid>/status，修改实参后放行） */
    public Object onDumpStackTraces(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                if (a instanceof List) {
                    List<?> raw = (List<?>) a;
                    if (!raw.isEmpty() && raw.get(0) instanceof Integer) {
                        @SuppressWarnings("unchecked")
                        List<Integer> list = (List<Integer>) raw; // 元素已确认 Integer（仅移除操作）
                        int removed = 0;
                        List<Integer> keep = new ArrayList<Integer>();
                        for (Integer pid : list) {
                            if (pidFrozenAny(pid.intValue())) {
                                removed++;
                            } else {
                                keep.add(pid);
                            }
                        }
                        if (removed > 0) {
                            list.clear();
                            list.addAll(keep);
                            Log.i(TAG, "dumpStackTraces firstPids 剔除冻结 pid " + removed + " 个");
                        }
                        break;
                    }
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "firstPids 过滤异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** Service 超时豁免：冻结 app 的 serviceTimeout 不判 ANR */
    public Object onServiceTimeout(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                Integer uid = extractUid(a);
                if (uid != null && anyFrozen(uid.intValue())) {
                    Log.i(TAG, "serviceTimeout 豁免（冻结中 uid=" + uid + "）");
                    return cb.defaultReturn();
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "serviceTimeout 豁免异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** 前台 Service 超时豁免：ServiceRecord.app.uid 冻结中 → 阻断 */
    public Object onServiceForegroundTimeout(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                Integer uid = serviceRecordUid(a);
                if (uid != null && anyFrozen(uid.intValue())) {
                    Log.i(TAG, "serviceForegroundTimeout 豁免（冻结中 uid=" + uid + "）");
                    return cb.defaultReturn();
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "serviceForegroundTimeout 豁免异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** ProcessErrorStateRecord 二次拦截：this 的进程 uid 冻结中 → 阻断 */
    public Object onPesrAnr(MethodCallback cb) {
        try {
            Object self = cb.args.length > 0 ? cb.args[0] : null;
            Integer uid = extractUid(self);
            if (uid != null && anyFrozen(uid.intValue())) {
                Log.i(TAG, "ProcessErrorStateRecord 二次拦截（冻结中 uid=" + uid + "）");
                return cb.defaultReturn();
            }
        } catch (Throwable t) {
            Log.w(TAG, "PESR 拦截异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** 系统 freezer 防双冻结：CachedAppOptimizer#freezeApp 目标是冻结中 uid → 阻止（false） */
    public Object onFreezeApp(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                Integer uid = extractUid(a);
                if (uid != null && anyFrozen(uid.intValue())) {
                    Log.i(TAG, "CachedAppOptimizer.freezeApp 拦截（冻结中 uid=" + uid + "）");
                    return Boolean.FALSE; // 返回 false = 未冻结（阻断系统二次冻结）
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "freezeApp 拦截异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** 冻结期 Activity 不回收：属主 uid 冻结中 → 阻止 destroy（false） */
    public Object onDestroyImmediately(MethodCallback cb) {
        try {
            Object self = cb.args.length > 0 ? cb.args[0] : null;
            Integer uid = activityUid(self);
            if (uid != null && anyFrozen(uid.intValue())) {
                Log.i(TAG, "ActivityRecord.destroyImmediately 拦截（冻结中 uid=" + uid + "）");
                return Boolean.FALSE;
            }
        } catch (Throwable t) {
            Log.w(TAG, "destroyImmediately 拦截异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    // ---------------- v0.4.26-l3 ColorOS HANS / 系统 freezer 防御回调 ----------------

    /** 系统二次冻结拦截：freezeAppAsync*LSP / freezeBinder / freezePackageCgroup / HANS 冻结入口
     *  —— 候选池内 uid（或 pid 参数）→ 阻断（不 invoke 原方法，防双冻结/冻结态错乱）
     *  v0.4.48-l3：判定从"冻结集"扩为"候选池"——对齐 AStop hookSystemFreezer 语义，
     *  系统冻结器不得冻结任何 Sundown 管理的 app（含 grace 期候选，防抢占黑屏） */
    public Object onSystemFreeze(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                if (a == null) continue;
                if (a instanceof Integer) {
                    int v = ((Integer) a).intValue();
                    if (v > 0) {
                        // uid 语义（>=10000 普通 app）或 pid 语义（TargetPid/进程）任一命中即拦
                        if (v >= 10000 && candidate(v)) {
                            Log.i(TAG, "系统 freeze 拦截（候选池 uid=" + v + "）: " + cb.target.getName());
                            return cb.defaultReturn();
                        }
                        if (pidCandidate(v)) {
                            Log.i(TAG, "系统 freeze 拦截（候选池 pid=" + v + "）: " + cb.target.getName());
                            return cb.defaultReturn();
                        }
                    }
                } else {
                    Integer uid = extractUid(a);
                    if (uid != null && uid.intValue() >= 10000 && sundownFrozen(uid.intValue())) {
                        Log.i(TAG, "系统 freeze 拦截（冻结中 uid=" + uid + "）: " + cb.target.getName());
                        return cb.defaultReturn();
                    }
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "系统 freeze 拦截异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** HANS 解冻防御：unfreezeForKernel / unfreezeForKernelTargetPid / unfreezeAppforHansMinSystem
     *  —— 冻结集内 uid/pid → 阻断（防 HANS 解掉 Sundown 冻结态，墓碑失效/假活） */
    public Object onHansUnfreeze(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                if (a == null) continue;
                if (a instanceof Integer) {
                    int v = ((Integer) a).intValue();
                    if (v > 0) {
                        if (sundownFrozen(v)) {
                            Log.i(TAG, "HANS unfreeze 拦截（冻结中 uid=" + v + "）");
                            return cb.defaultReturn();
                        }
                        if (pidFrozenSundown(v)) {
                            Log.i(TAG, "HANS unfreeze 拦截（冻结中 pid=" + v + "）");
                            return cb.defaultReturn();
                        }
                    }
                } else {
                    Integer uid = extractUid(a);
                    if (uid != null && sundownFrozen(uid.intValue())) {
                        Log.i(TAG, "HANS unfreeze 拦截（冻结中 uid=" + uid + "）");
                        return cb.defaultReturn();
                    }
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "HANS unfreeze 拦截异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** HANS Proxy 防御：isProxyed 冻结集内 uid → false（阻止 HANS 代理干预冻结 app） */
    public Object onHansIsProxyed(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                Integer uid = extractUid(a);
                if (uid != null && uid.intValue() >= 10000 && sundownFrozen(uid.intValue())) {
                    Log.i(TAG, "HANS isProxyed 拦截（冻结中 uid=" + uid + "）");
                    return Boolean.FALSE;
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "HANS isProxyed 拦截异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** v0.4.44-l3 P2⑮ break_network：ColorOS Oplus 深度睡眠 uid 断网判定——
     * 冻结集内 uid → true（系统对其断网，墓碑彻底休眠）；其余保持系统默认。
     * 对齐 AStop OplusDeepSleepHooks（无条件断网）裁剪为冻结集防御：
     * 非冻结 app 不受影响；断网随冻结生命周期自动开/关（冻结→断网，解冻→恢复）。 */
    public Object onNeedUidDisconnect(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                Integer uid = extractUid(a);
                if (uid != null && uid.intValue() >= 10000 && sundownFrozen(uid.intValue())) {
                    Log.i(TAG, "break_network 命中（冻结中 uid=" + uid + "）");
                    return Boolean.TRUE;
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "break_network 判定异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** DO_NOTHING：禁 ColorOS GMS 限制（registerGmsRestrictObserver/updateGmsRestrict，void → null） */
    public Object onNoop(MethodCallback cb) {
        return cb.defaultReturn();
    }

    /** returnConstant(FALSE)：OplusStartupStrategy#isGoogleRestricInfoOn → false（GMS 限制信息关闭） */
    public Object onReturnFalse(MethodCallback cb) {
        return Boolean.FALSE;
    }

    // ---------------- v0.4.39-l3 P1⑨ 防御补全回调 ----------------

    /** OomAdjuster 计算入口防御：applyOomAdj* 目标是候选池内 uid → 跳过（内核保持 -1000，
     *  防系统把 adj 重算回 cached≈900 覆盖 daemon OOM 保护；v0.4.48-l3 冻结集→候选池） */
    public Object onOomAdjCompute(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                Integer uid = extractUid(a);
                if (uid != null && uid.intValue() >= 10000 && candidate(uid.intValue())) {
                    Log.i(TAG, "OomAdjuster 计算跳过（候选池 uid=" + uid + "）: " + cb.target.getName());
                    return cb.defaultReturn();
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "OomAdjuster 计算跳过异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** OomAdjuster 写入入口防御：setOomAdj(pid, adj, ...) 候选池内 pid → adj 改写 -1000
     *  （保留系统流程完整，写入值强制为保护值；pid 判定走 /proc 归属核验） */
    public Object onOomAdjApply(MethodCallback cb) {
        try {
            Integer pid = null;
            Integer adjIdx = null;
            int intCount = 0;
            for (int i = 0; i < cb.args.length; i++) {
                if (cb.args[i] instanceof Integer) {
                    intCount++;
                    if (intCount == 1) {
                        pid = (Integer) cb.args[i];
                    } else if (intCount == 2) {
                        adjIdx = Integer.valueOf(i);
                    }
                }
            }
            if (pid != null && adjIdx != null && pid.intValue() > 0
                    && pidCandidate(pid.intValue())) {
                Log.i(TAG, "OomAdjuster 写入保护（候选池 pid=" + pid + "，adj 锁 -1000）");
                cb.args[adjIdx.intValue()] = Integer.valueOf(-1000);
            }
        } catch (Throwable t) {
            Log.w(TAG, "OomAdjuster 写入保护异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** 耗电判定豁免：checkExcessivePowerUsageLPr 冻结集内 uid → false（不耗电，
     *  防"耗电异常"杀冻结 app；非冻结 app 保持系统判定） */
    public Object onExcessivePower(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                Integer uid = extractUid(a);
                if (uid != null && uid.intValue() >= 10000 && sundownFrozen(uid.intValue())) {
                    Log.i(TAG, "耗电判定豁免（冻结中 uid=" + uid + "）");
                    return Boolean.FALSE;
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "耗电判定豁免异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** Doze 白名单注入：getAppId*WhitelistInternal 返回数组追加冻结 uid（appId = uid，
     *  无需 pkg 映射；防 Doze 维护期清理冻结 app） */
    public Object onAppIdWhitelist(MethodCallback cb) {
        try {
            Object r = cb.invokeOriginalOrDefault();
            if (!(r instanceof int[])) return r;
            int[] orig = (int[]) r;
            List<Integer> frozen = new ArrayList<Integer>();
            for (Integer uid : sundownFrozen) {
                if (uid.intValue() >= 10000 && !intArrayContains(orig, uid.intValue())) {
                    frozen.add(uid);
                }
            }
            if (frozen.isEmpty()) return orig;
            int[] merged = new int[orig.length + frozen.size()];
            System.arraycopy(orig, 0, merged, 0, orig.length);
            for (int i = 0; i < frozen.size(); i++) {
                merged[orig.length + i] = frozen.get(i).intValue();
            }
            Log.i(TAG, "Doze 白名单注入 " + frozen.size() + " 个冻结 uid");
            return merged;
        } catch (Throwable t) {
            Log.w(TAG, "Doze 白名单注入异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    // ---------------- v0.4.51-l3 P2 收尾：Recents 任务保护 + killProcess 拦截 ----------------

    /** Recents 任务保护：ActivityTaskSupervisor#removeTask(taskId) 目标是候选池 app 的任务 → 拦截。
     *  对齐 AStop TaskHooks hookRecentsTaskDefense（hiddenTaskProtectedPackages 语义）：
     *  候选池 app 的 Task 不被移除（防"清后台/系统自动清任务"杀被管 app，
     *  与事故① remove task 杀冻结 app 同源）；用户滑掉任务同样保护（墓碑语义）。
     *  taskId → uid 判定走 taskUid（getTaskById → getTopActivity → getUid），失败放行。 */
    public Object onTaskRemove(MethodCallback cb) {
        try {
            // 调试日志（2026-08-05 定位）：打印触发与判定链每步结果，锁定 ColorOS 真实路径
            StringBuilder dbg = new StringBuilder("removeTask 触发: " + cb.target.getName());
            for (Object a : cb.args) {
                if (a == null) { dbg.append(" [null]"); continue; }
                if (a instanceof Number) { dbg.append(" [num=").append(a).append(']'); continue; }
                dbg.append(" [obj=").append(a.getClass().getSimpleName()).append(']');
            }
            Log.i(TAG, dbg.toString());
            for (Object a : cb.args) {
                if (a instanceof Number) {
                    int taskId = ((Number) a).intValue();
                    if (taskId <= 0) continue;
                    // 非静态方法 args[0] = thisObject（lsplant 约定）→ ActivityTaskManagerService 实例
                    Object supervisor = cb.args.length > 0 ? cb.args[0] : null;
                    Object task = null;
                    try {
                        task = callMethod(supervisor, "getTaskById", Integer.valueOf(taskId));
                    } catch (Throwable ignored) {
                    }
                    if (task == null && supervisor != null) {
                        Object ts = readField(supervisor, "mTaskSupervisor");
                        if (ts != null) {
                            try {
                                task = callMethod(ts, "getTaskById", Integer.valueOf(taskId));
                            } catch (Throwable ignored) {
                            }
                        }
                    }
                    Integer uid = (task != null) ? taskObjUid(task) : null;
                    Log.i(TAG, "removeTask 判定: taskId=" + taskId + " task=" + (task != null ? "ok" : "null")
                            + " uid=" + uid);
                    if (uid != null && uid.intValue() >= 10000 && candidate(uid.intValue())) {
                        Log.i(TAG, "removeTask 拦截（候选池任务 taskId=" + taskId + " uid=" + uid + "）");
                        return cb.defaultReturn();
                    }
                } else {
                    // removeTask(Task) 等对象重载：Task.getTopActivity → ActivityRecord.getUid
                    Integer uid = taskObjUid(a);
                    Log.i(TAG, "removeTask 对象判定: " + a.getClass().getSimpleName() + " uid=" + uid);
                    if (uid != null && uid.intValue() >= 10000 && candidate(uid.intValue())) {
                        Log.i(TAG, "removeTask 拦截（候选池任务对象 uid=" + uid + "）: " + cb.target.getName());
                        return cb.defaultReturn();
                    }
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "removeTask 拦截异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** 清除全部观测（2026-08-05 定位）：removeAllVisibleRecentTasks——先确认 ColorOS 清除全部是否走此入口 */
    public Object onRemoveAllVisible(MethodCallback cb) {
        Log.i(TAG, "removeAllVisibleRecentTasks 触发: " + cb.target.getName());
        return cb.invokeOriginalOrDefault();
    }

    // 2026-08-05 定位：ATMS binder 事务 code 采样（不拦截；同 code 每 2s 一条，异 code 即时）
    private static int lastTransactCode = -1;
    private static long lastTransactTs = 0;

    public Object onAtmsTransact(MethodCallback cb) {
        try {
            if (cb.args.length > 1 && cb.args[1] instanceof Number) {
                Log.i(TAG, "activity_task binder code=" + ((Number) cb.args[1]).intValue());
            }
        } catch (Throwable ignored) {
        }
        return cb.invokeOriginalOrDefault();
    }

    /** 2026-08-05 定位：AMS binder code 全量（定位窗口用，操作后即撤） */
    public Object onAmsTransact(MethodCallback cb) {
        try {
            if (cb.args.length > 1 && cb.args[1] instanceof Number) {
                Log.i(TAG, "activity binder code=" + ((Number) cb.args[1]).intValue());
            }
        } catch (Throwable ignored) {
        }
        return cb.invokeOriginalOrDefault();
    }

    /** 2026-08-05 定位：TaskPersister.removeTask 观测（Recents 记录清理持久化层） */
    public Object onTaskPersisterRemove(MethodCallback cb) {
        Log.i(TAG, "TaskPersister.removeTask 触发: " + cb.target.getName());
        return cb.invokeOriginalOrDefault();
    }

    /** v0.4.51-l3 实测补丁：ProcessRecord#killLocked 拦截——候选池 app 且 reason 命中保护名单 → 不杀。
     *  2026-08-05 实锤：ColorOS 滑卡/清 Recents = o-stop(40) 杀进程（非 removeTask，任务移除是
     *  杀进程后果）；对齐 AStop hookProcessRecordKill（killLocked + reason 白名单）。
     *  uid 判定：ProcessRecord.uid 直字段 → getPid → /proc 兜底。 */
    public Object onKillLocked(MethodCallback cb) {
        try {
            Object self = cb.args.length > 0 ? cb.args[0] : null;
            Integer uid = extractUid(self);
            if (uid == null && self != null) {
                Object pid = callMethod(self, "getPid");
                if (pid instanceof Number) uid = pidUid(((Number) pid).intValue());
            }
            String reason = null;
            for (Object a : cb.args) {
                if (a instanceof String) { reason = (String) a; break; }
            }
            if (uid != null && uid.intValue() >= 10000 && candidate(uid.intValue())
                    && isProtectedKillReason(reason)) {
                Log.i(TAG, "killLocked 拦截（候选池 uid=" + uid + " reason=" + reason + "）");
                return cb.defaultReturn(); // void → null：不执行杀进程
            }
        } catch (Throwable t) {
            Log.w(TAG, "killLocked 拦截异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    /** killLocked reason 保护名单（对齐 AStop hookProcessRecordKill 语义 + ColorOS o-stop） */
    private static boolean isProtectedKillReason(String reason) {
        if (reason == null) return false;
        String r = reason.toLowerCase();
        return r.contains("remove task") || r.contains("o-stop") || r.contains("ostop")
                || r.contains("graceful kill") || r.contains("stop user")
                || r.contains("force stop") || r.contains("remove user");
    }

    /** killProcess 拦截：CachedAppOptimizer#killProcess（OOM/内存压力杀进程路径）目标是候选池内
     *  pid/ProcessRecord → 拦截不杀。对齐 AStop hookBinderFreezeDefense：
     *  OOM 保护 -1000 的双保险（防系统把进程杀了再清 Task，墓碑失效）。
     *  用户强停走 forceStop 不常经此路径，被管 app 墓碑语义下拦截可接受。 */
    public Object onKillProcess(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                if (a == null) continue;
                if (a instanceof Number) {
                    int v = ((Number) a).intValue();
                    if (v > 0 && pidCandidate(v)) {
                        Log.i(TAG, "killProcess 拦截（候选池 pid=" + v + "）");
                        return cb.defaultReturn();
                    }
                } else {
                    Integer pid = processPid(a);
                    if (pid != null && pid.intValue() > 0 && pidCandidate(pid.intValue())) {
                        Log.i(TAG, "killProcess 拦截（候选池 ProcessRecord pid=" + pid + "）");
                        return cb.defaultReturn();
                    }
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "killProcess 拦截异常（放行）: " + t);
        }
        return cb.invokeOriginalOrDefault();
    }

    private static boolean intArrayContains(int[] arr, int v) {
        for (int x : arr) {
            if (x == v) return true;
        }
        return false;
    }

    // ---------------- 工具 ----------------

    /** pid → uid → 并集判定（ANR firstPids 过滤等：cgroup 冻着即豁免，无论谁冻的） */
    private boolean pidFrozenAny(int pid) {
        Integer uid = pidUid(pid);
        return uid != null && anyFrozen(uid.intValue());
    }

    /** pid → uid → Sundown 权威集判定（HANS 解冻/冻结拦截：只认 Sundown 自己冻的） */
    private boolean pidFrozenSundown(int pid) {
        Integer uid = pidUid(pid);
        return uid != null && sundownFrozen(uid.intValue());
    }

    /** v0.4.48-l3：候选池判定（系统冻结器/杀进程/OOM/耗电防御统一用——对齐 AStop"被管 app"） */
    private boolean candidate(int uid) {
        return candidateSet.contains(Integer.valueOf(uid));
    }

    /** v0.4.48-l3：pid → uid → 候选池判定（setOomAdj/killProcess 等 pid 参数场景） */
    private boolean pidCandidate(int pid) {
        Integer uid = pidUid(pid);
        return uid != null && candidate(uid.intValue());
    }

    /** /proc/<pid>/status 的 Uid 首字段（真实 uid） */
    private static Integer pidUid(int pid) {
        try {
            java.io.BufferedReader r = new java.io.BufferedReader(
                    new java.io.InputStreamReader(
                            new java.io.FileInputStream("/proc/" + pid + "/status"), "UTF-8"));
            try {
                String line;
                while ((line = r.readLine()) != null) {
                    if (line.startsWith("Uid:")) {
                        String[] f = line.split("\\s+");
                        if (f.length >= 2) return Integer.valueOf(Integer.parseInt(f[1]));
                        return null;
                    }
                }
            } finally {
                r.close();
            }
        } catch (Throwable ignored) {
        }
        return null;
    }

    /** 从实参提取 uid：ProcessRecord.uid 字段 / 直接 Integer（uid 语义）——低频事件逐次反射可接受 */
    private static Integer extractUid(Object arg) {
        if (arg == null) return null;
        if (arg instanceof Integer) return (Integer) arg;
        try {
            Field f = arg.getClass().getDeclaredField("uid");
            f.setAccessible(true);
            int v = f.getInt(arg);
            return (v >= 0) ? Integer.valueOf(v) : null;
        } catch (Throwable ignored) {
        }
        // ProcessRecord.mState（A14+ 拆出的状态对象）也可能携带 uid？保守：只认直字段
        return null;
    }

    /** ServiceRecord.app.uid */
    private static Integer serviceRecordUid(Object rec) {
        if (rec == null) return null;
        Object app = readField(rec, "app");
        return extractUid(app);
    }

    /** ActivityRecord.app.uid */
    private static Integer activityUid(Object rec) {
        if (rec == null) return null;
        Object app = readField(rec, "app");
        return extractUid(app);
    }

    private static Object readField(Object obj, String name) {
        if (obj == null) return null;
        try {
            Field f = obj.getClass().getDeclaredField(name);
            f.setAccessible(true);
            return f.get(obj);
        } catch (Throwable ignored) {
            return null;
        }
    }

    /** 反射调用方法（低频事件逐次反射可接受；异常返回 null）——先 public getMethod，再声明方法兜底 */
    private static Object callMethod(Object obj, String name, Object... args) {
        if (obj == null) return null;
        try {
            Class<?>[] pts = new Class<?>[args.length];
            for (int i = 0; i < args.length; i++) pts[i] = args[i].getClass();
            Method m = obj.getClass().getMethod(name, pts);
            return m.invoke(obj, args);
        } catch (Throwable ignored) {
        }
        try {
            for (Method m : obj.getClass().getDeclaredMethods()) {
                if (!m.getName().equals(name) || m.getParameterTypes().length != args.length) continue;
                m.setAccessible(true);
                return m.invoke(obj, args);
            }
        } catch (Throwable ignored) {
        }
        return null;
    }

    /** v0.4.51-l3：taskId → 属主 uid——ActivityTaskManagerService.getTaskById → Task.getTopActivity()
     *  → ActivityRecord.getUid()（对齐 AStop TaskHooks 的 task→package 判定链；失败返回 null 放行）
     *  回落链：ATMS.getTaskById → mTaskSupervisor.getTaskById（ColorOS 方法名/位置差异兜底） */
    private static Integer taskUid(Object supervisor, int taskId) {
        try {
            if (supervisor == null) return null;
            Object task = callMethod(supervisor, "getTaskById", Integer.valueOf(taskId));
            if (task == null) {
                Object ts = readField(supervisor, "mTaskSupervisor");
                if (ts != null) task = callMethod(ts, "getTaskById", Integer.valueOf(taskId));
            }
            if (task == null) return null;
            return taskObjUid(task);
        } catch (Throwable ignored) {
        }
        return null;
    }

    /** v0.4.51-l3：Task 对象 → 属主 uid（Task.getTopActivity → ActivityRecord.getUid；
     *  覆盖 removeTask(Task) 等对象重载；失败返回 null 放行） */
    private static Integer taskObjUid(Object task) {
        try {
            if (task == null) return null;
            Object top = callMethod(task, "getTopActivity");
            if (top == null) return null;
            Object uid = callMethod(top, "getUid");
            if (uid instanceof Number) return Integer.valueOf(((Number) uid).intValue());
        } catch (Throwable ignored) {
        }
        return null;
    }

    /** v0.4.51-l3：ProcessRecord 提取 pid——getPid() 方法 → mPid 字段 → pid 字段
     *  （对齐 AStop killHook 提取链；失败返回 null 放行） */
    private static Integer processPid(Object rec) {
        if (rec == null) return null;
        try {
            Object v = callMethod(rec, "getPid");
            if (v instanceof Number) return Integer.valueOf(((Number) v).intValue());
        } catch (Throwable ignored) {
        }
        Object f = readField(rec, "mPid");
        if (f instanceof Number) return Integer.valueOf(((Number) f).intValue());
        f = readField(rec, "pid");
        if (f instanceof Number) return Integer.valueOf(((Number) f).intValue());
        return null;
    }
}
