package ren.sunset.sundown.hook;

import android.util.Log;

import java.io.File;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
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
        // 1. ANR 流程上游阻断（直击"点击无响应→闪退"）
        hookAllOverloads(findClass(ANR_HELPER), "appNotResponding", callback("onAppNotResponding"));

        // 2. ANR stack 转储 firstPids 剔除冻结 pid（A13+ ProcessList，AMS 兜底）
        Class<?> pl = (findMethod(findClass(PROCESS_LIST), "dumpStackTraces") != null)
                ? findClass(PROCESS_LIST) : findClass(AMS);
        hookAllOverloads(pl, "dumpStackTraces", callback("onDumpStackTraces"));

        // 3. Service 超时豁免（冻结 app 不判 ANR）
        hookAllOverloads(findClass(ACTIVE_SERVICES), "serviceTimeout", callback("onServiceTimeout"));
        hookAllOverloads(findClass(ACTIVE_SERVICES), "serviceForegroundTimeout",
                callback("onServiceForegroundTimeout"));

        // 4. ProcessErrorStateRecord 二次拦截（方法名探测：appNotResponding / setNotResponding，
        //    找不到降级——AnrHelper 上游已兜底）
        Class<?> pesr = findClass(PESR);
        Method pesrTarget = findMethod(pesr, "appNotResponding");
        if (pesrTarget == null) {
            pesrTarget = findMethod(pesr, "setNotResponding");
        }
        if (pesrTarget != null && pesr != null) {
            hookMethod(pesrTarget, callback("onPesrAnr"));
        } else {
            Log.w(TAG, "ProcessErrorStateRecord 二次拦截不可用（方法未找到，上游已兜底）");
        }

        // 5. 系统 freezer 防双冻结（CachedAppOptimizer 不冻被管 app）
        hookAllOverloads(findClass(CACHED_APP_OPTIMIZER), "freezeApp", callback("onFreezeApp"));

        // 6. 冻结期 Activity 保护（不回收，防解冻后黑屏/重建）
        hookAllOverloads(findClass(ACTIVITY_RECORD), "destroyImmediately", callback("onDestroyImmediately"));

        // 7. v0.4.26-l3 ColorOS HANS 防御（实机校准：PJD110/Android16 无 CachedAppOptimizer#freezeApp，
        //    系统 freezer 入口为 freezeAppAsync*LSP 家族；HANS 用 unfreezeForKernel 解冻——真机元凶）
        // 7a. 系统 freezer 防双冻结：ColorOS 改名后的 freeze 入口（锁内方法，全重载）
        hookAllOverloads(findClass(CACHED_APP_OPTIMIZER), "freezeAppAsyncLSP", callback("onSystemFreeze"));
        hookAllOverloads(findClass(CACHED_APP_OPTIMIZER), "freezeAppAsyncInternalLSP", callback("onSystemFreeze"));
        hookAllOverloads(findClass(CACHED_APP_OPTIMIZER), "freezeAppAsyncAtEarliestLSP", callback("onSystemFreeze"));
        hookAllOverloads(findClass(CACHED_APP_OPTIMIZER), "freezeAppAsyncImmediateLSP", callback("onSystemFreeze"));
        hookAllOverloads(findClass(CACHED_APP_OPTIMIZER), "freezeBinder", callback("onSystemFreeze"));
        hookAllOverloads(findClass(CACHED_APP_OPTIMIZER), "freezeBinderAndPackageCgroup", callback("onSystemFreeze"));
        hookAllOverloads(findClass(CACHED_APP_OPTIMIZER), "freezePackageCgroup", callback("onSystemFreeze"));
        // 7b. HANS 主动冻结入口（冻结集内 uid 阻止二次冻结）
        hookAllOverloads(findClass(HANS_MANAGER), "freezeAppForPreload", callback("onSystemFreeze"));
        hookAllOverloads(findClass(HANS_MANAGER), "freezeAllProcess", callback("onSystemFreeze"));
        hookAllOverloads(findClass(HANS_MANAGER), "freezeCgroupUid", callback("onSystemFreeze"));
        // 7c. HANS 解冻防御（防 HANS 解掉 Sundown 冻结态——墓碑失效/假活）
        hookAllOverloads(findClass(HANS_MANAGER), "unfreezeForKernel", callback("onHansUnfreeze"));
        hookAllOverloads(findClass(HANS_MANAGER), "unfreezeForKernelTargetPid", callback("onHansUnfreeze"));
        hookAllOverloads(findClass(HANS_MANAGER), "unfreezeAppforHansMinSystem", callback("onHansUnfreeze"));
        // 7d. HANS Proxy 防御（冻结集内 uid 不被 HANS 代理干预）
        hookAllOverloads(findClass(HANS_PROXY), "isProxyed", callback("onHansIsProxyed"));
        // 7e. ColorOS GMS 限制禁用（对齐 AStop DO_NOTHING：registerGmsRestrictObserver/updateGmsRestrict）
        hookAllOverloads(findClass(BG_SCENE), "registerGmsRestrictObserver", callback("onNoop"));
        hookAllOverloads(findClass(BG_SCENE), "updateGmsRestrict", callback("onNoop"));
        hookAllOverloads(findClass(STARTUP_STRATEGY), "isGoogleRestricInfoOn", callback("onReturnFalse"));

        // 8. v0.4.39-l3 P1⑨ OomAdjuster 防御：系统重算 adj 不得覆盖 daemon OOM 保护（-1000）
        //    —— daemon 侧 protect_oom 写 /proc/<pid>/oom_score_adj 锁 -1000（OOM_DISABLE 等效），
        //    但 OomAdjuster 周期重算会写回 cached≈900 覆盖保护（解冻即"白冻"）。
        //    双保险：applyOomAdj*（计算入口，冻结集内跳过）+ setOomAdj（写入入口，adj 改写 -1000）。
        hookAllOverloads(findClass(OOM_ADJUSTER), "applyOomAdjLSP", callback("onOomAdjCompute"));
        hookAllOverloads(findClass(OOM_ADJUSTER), "applyOomAdj", callback("onOomAdjCompute"));
        hookAllOverloads(findClass(PROCESS_LIST), "setOomAdj", callback("onOomAdjApply"));

        // 9. v0.4.39-l3 P1⑨ 禁用耗电判定（冻结集内豁免——AStop 无条件禁用 checkExcessivePowerUsageLPr，
        //    Sundown 裁剪为冻结集防御：非冻结 app 保持系统耗电判定，防"耗电异常"杀冻结 app）
        hookAllOverloads(findClass(AMS), "checkExcessivePowerUsageLPr", callback("onExcessivePower"));

        // 10. v0.4.39-l3 P1⑨ Doze 白名单注入：冻结 uid 注入白名单数组（appId = uid，无需 pkg 映射），
        //     防 Doze 维护期把冻结 app 当非白名单清理（对齐 AStop DeviceIdleWhitelistHooks）
        hookAllOverloads(findClass(DEVICE_IDLE), "getAppIdWhitelistInternal", callback("onAppIdWhitelist"));
        hookAllOverloads(findClass(DEVICE_IDLE), "getAppIdUserWhitelistInternal", callback("onAppIdWhitelist"));

        // 11. v0.4.44-l3 P2⑮ break_network：ColorOS Oplus 深度睡眠 uid 断网判定——
        //     冻结集内 uid → true（系统对其断网），其余保持系统默认。
        //     对齐 AStop OplusDeepSleepHooks（无条件断网），裁剪为冻结集防御：
        //     非冻结 app 不受影响；断网随冻结生命周期自动开/关（冻结→断网，解冻→恢复）。
        //     实机校准：PJD110/Android16 若方法名/类名不符 → hook 失败留痕不崩溃（失败安全）。
        hookAllOverloads(findClass(OPLUS_DEEPSLEEP), "isNeedUidDisconnectNetwork",
                callback("onNeedUidDisconnect"));

        // 启动冻结集扫描线程（install 一次；uninstall 停）
        if (!scannerThread.isAlive()) {
            scannerThread.start();
        }
        Log.i(TAG, "DefenseHooks 安装完成（hook 数: " + hookers.size() + "）");
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

    private Method callback(String name) {
        try {
            return DefenseHooks.class.getDeclaredMethod(name, MethodCallback.class);
        } catch (Throwable t) {
            Log.e(TAG, "回调方法缺失: " + name + " -> " + t);
            return null;
        }
    }

    private static Class<?> findClass(String name) {
        return ServerClasses.find(name);
    }

    private static Method findMethod(Class<?> cls, String name) {
        if (cls == null) return null;
        for (Method m : cls.getDeclaredMethods()) {
            if (m.getName().equals(name)) return m;
        }
        return null;
    }

    private void hookMethod(Method target, Method callback) {
        if (target == null || callback == null) {
            Log.w(TAG, "hook 跳过（方法或回调缺失）: " + target);
            return;
        }
        if (Modifier.isAbstract(target.getModifiers()) || target.isBridge() || target.isSynthetic()) {
            return;
        }
        Hooker h = NativeBridge.hook(target, callback, this);
        if (h != null) {
            hookers.add(h);
            Log.i(TAG, "已 hook " + target.getDeclaringClass().getSimpleName() + "#" + target.getName());
        } else {
            Log.w(TAG, "方法未 hook 到: " + target.getDeclaringClass().getName() + "#" + target.getName());
        }
    }

    private int hookAllOverloads(Class<?> cls, String name, Method callback) {
        if (cls == null || callback == null) {
            Log.w(TAG, "hook 跳过（类或回调缺失）: " + name);
            return 0;
        }
        int ok = 0;
        for (Method m : cls.getDeclaredMethods()) {
            if (!m.getName().equals(name)) continue;
            if (Modifier.isAbstract(m.getModifiers()) || m.isBridge() || m.isSynthetic()) continue;
            Hooker h = NativeBridge.hook(m, callback, this);
            if (h != null) {
                hookers.add(h);
                ok++;
            }
        }
        if (ok == 0) {
            Log.w(TAG, "方法未 hook 到: " + cls.getName() + "#" + name);
        } else {
            Log.i(TAG, "已 hook " + cls.getSimpleName() + "#" + name + "（重载数: " + ok + "）");
        }
        return ok;
    }
}
