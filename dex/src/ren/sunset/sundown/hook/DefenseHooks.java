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
     *  —— 冻结集内 uid（或 pid 参数）→ 阻断（不 invoke 原方法，防双冻结/冻结态错乱） */
    public Object onSystemFreeze(MethodCallback cb) {
        try {
            for (Object a : cb.args) {
                if (a == null) continue;
                if (a instanceof Integer) {
                    int v = ((Integer) a).intValue();
                    if (v > 0) {
                        // uid 语义（>=10000 普通 app）或 pid 语义（TargetPid/进程）任一命中即拦
                        if (v >= 10000 && sundownFrozen(v)) {
                            Log.i(TAG, "系统 freeze 拦截（冻结中 uid=" + v + "）: " + cb.target.getName());
                            return cb.defaultReturn();
                        }
                        if (pidFrozenSundown(v)) {
                            Log.i(TAG, "系统 freeze 拦截（冻结中 pid=" + v + "）: " + cb.target.getName());
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

    /** DO_NOTHING：禁 ColorOS GMS 限制（registerGmsRestrictObserver/updateGmsRestrict，void → null） */
    public Object onNoop(MethodCallback cb) {
        return cb.defaultReturn();
    }

    /** returnConstant(FALSE)：OplusStartupStrategy#isGoogleRestricInfoOn → false（GMS 限制信息关闭） */
    public Object onReturnFalse(MethodCallback cb) {
        return Boolean.FALSE;
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
