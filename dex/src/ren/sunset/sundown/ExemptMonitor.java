package ren.sunset.sundown;

import android.app.ActivityManager;
import android.app.ActivityManager.RunningServiceInfo;
import android.app.ActivityManager.RunningTaskInfo;
import android.content.ComponentName;
import android.os.IBinder;
import android.util.Log;

import java.lang.reflect.Method;
import java.util.List;

import ren.sunset.sundown.hook.LsPlantBridge.EventDispatcher;

/**
 * 豁免判定监视器（L3：keep_fg_service / keep_media，docs/l3-plan.md §0.4）。
 *
 * 独立守护线程，2s 节拍对"最近焦点包"做前台服务 / 媒体播放判定；判定变化时经
 * EventQueue 上行 `event exempt pkg=P fg=.. media=..`（daemon 决策直接消费）。
 *
 * 为什么独立线程：hook 回调可能运行在持有 AMS 锁的线程上（updateActivityUsageStats /
 * addPidLocked 均在锁内路径实证），锁内反射 getServices / playback 配置会经 binder
 * 重入 AMS → 死等 = ANR（真机教训，docs/l3-plan.md §6）。独立线程无锁上下文，
 * 判定慢几百 ms 无妨（豁免语义是"最近判定"，2s 节拍足够）。
 *
 * 全反射纪律：getService / getServices / getPackageUid / IAudioService 等为
 * @SystemApi / hidden（android.jar 无编译期符号或随版本漂移），一律反射；
 * RunningServiceInfo（public 嵌套类）与 ServiceManager / IBinder（public API）
 * 直接类型引用。system_server 内自调走 fast-path binder，无 hidden API 限制。
 *
 * 判定失败（反射异常）→ 本轮不上行（daemon 保留旧值；无值缺省 0——宁可多冻）。
 */
public final class ExemptMonitor {

    private static final String TAG = "SundownDex";
    private static final long INTERVAL_MS = 2000;
    /** RunningServiceInfo.flags 的前台服务标志（START_FLAG_FOREGROUND） */
    private static final int START_FLAG_FOREGROUND = 1;
    /** 同时判定的最近活跃包上限（LRU：退后台但仍有服务/媒体的 app 持续获得豁免） */
    private static final int MAX_TRACKED = 4;
    /** 权威 topActivity 判定失效阈值：超过该时长无成功判定 → hook focus 恢复直报兜底 */
    private static final long AUTH_STALE_MS = 10000;

    private final EventDispatcher dispatcher;

    /** 最近一次权威 topActivity 判定成功时刻（elapsedRealtime，单调时钟） */
    private volatile long lastAuthOkMs = 0L;

    /** 权威焦点源是否活跃（最近 AUTH_STALE_MS 内 topActivity 判定成功）。
     *  FocusHooks 据此裁决：权威活跃 → hook focus 仅登记线索（去抖）；
     *  权威失效（如 getTasks 反射连续失败）→ hook focus 恢复直报兜底（宁多不漏）。 */
    public boolean authActive() {
        return (android.os.SystemClock.elapsedRealtime() - lastAuthOkMs) < AUTH_STALE_MS;
    }

    /** 观察中的包 → 上次判定（fg/media/sent），lock 保护 */
    private final java.util.Map<String, Flags> states = new java.util.HashMap<>();
    /** 观察顺序（队首最新，LRU 淘汰），lock 保护 */
    private final java.util.List<String> order = new java.util.ArrayList<>();
    private final Object lock = new Object();

    private static final class Flags {
        boolean fg;
        boolean media;
        boolean sent;
    }

    ExemptMonitor(EventDispatcher dispatcher) {
        this.dispatcher = dispatcher;
    }

    /** hook 回调侧（锁内）调用：登记最近焦点包，零阻塞 */
    public void observe(String pkg) {
        synchronized (lock) {
            order.remove(pkg);
            order.add(0, pkg);
            while (order.size() > MAX_TRACKED) {
                order.remove(order.size() - 1);
            }
            if (!states.containsKey(pkg)) {
                states.put(pkg, new Flags());
            }
        }
    }

    /**
     * daemon 重连/换代后调用：重置所有包的 sent 标志，下一节拍全量重报豁免判定。
     * （daemon 重启会清空其 exempt 表；若判定值未变化，本侧不会自发重发——
     * 导致新 daemon 的豁免表缺失，退后台即有服务/媒体的 app 被误计时/误冻。
     * 2026-08-02 真机实证：daemon 重启后微信 fg=true 不再上报，进入 grace。）
     */
    public void reset() {
        synchronized (lock) {
            for (Flags f : states.values()) {
                f.sent = false;
            }
        }
    }

    void start() {
        Thread t = new Thread(new Runnable() {
            @Override
            public void run() {
                loop();
            }
        }, "SundownDex-Exempt");
        t.setDaemon(true);
        t.start();
    }

    private void loop() {
        while (true) {
            try {
                Thread.sleep(INTERVAL_MS);
            } catch (InterruptedException e) {
                return;
            }
            try {
                // 权威前台校正（每节拍）：hook focus 事件在 OPPO ROM 存在抖动噪声
                // （回桌面后残留 resume 事件反复上报，pause 过滤挡不住 event=1 的
                // 乱序/残留）。以 ActivityTaskManager.getTasks(1) 的真实 top activity
                // 为准，变化时补发权威 focus 事件——daemon 的 last_focus/decide_leave
                // 以权威源为准，hook focus 降级为 observe 线索（2026-08-02 真机实证）。
                String top = topActivityPkg();
                if (top != null) {
                    // 权威判定成功：刷新活性（FocusHooks 据此去抖/兜底裁决）
                    lastAuthOkMs = android.os.SystemClock.elapsedRealtime();
                    if (!top.equals(lastTop)) {
                        lastTop = top;
                        dispatcher.dispatch("event focus pkg=" + top);
                        observe(top);
                    }
                }
                java.util.List<String> pkgs;
                synchronized (lock) {
                    pkgs = new java.util.ArrayList<>(order); // 快照，锁外判定
                }
                for (String pkg : pkgs) {
                    boolean fg = hasForegroundService(pkg);
                    boolean media = hasActiveMedia(pkg);
                    Flags st;
                    synchronized (lock) {
                        st = states.get(pkg);
                        if (st == null) {
                            st = new Flags();
                            states.put(pkg, st);
                        }
                    }
                    boolean changed = !st.sent || fg != st.fg || media != st.media;
                    st.fg = fg;
                    st.media = media;
                    st.sent = true;
                    if (changed) {
                        dispatcher.dispatch(
                                "event exempt pkg=" + pkg
                                        + " fg=" + (fg ? 1 : 0)
                                        + " media=" + (media ? 1 : 0));
                    }
                }
            } catch (Throwable t) {
                Log.w(TAG, "豁免判定异常（跳过本轮）: " + t);
            }
        }
    }

    /** 真实前台包名（ActivityTaskManager.getTasks(1)，@hide 全反射；system_server 内
     *  自调用 fast-path binder，2s 节拍开销可接受）。失败返回 null（降级 hook focus）。 */
    private String lastTop;

    private static String topActivityPkg() {
        try {
            Class<?> atmCls = Class.forName("android.app.ActivityTaskManager");
            Method getInstance = atmCls.getMethod("getInstance");
            Object atm = getInstance.invoke(null);
            if (atm == null) {
                return null;
            }
            Method getTasks = atm.getClass().getMethod("getTasks", int.class);
            Object ret = getTasks.invoke(atm, 1);
            if (!(ret instanceof List)) {
                return null;
            }
            List<?> tasks = (List<?>) ret;
            if (tasks.isEmpty()) {
                return null;
            }
            Object info = tasks.get(0);
            if (info == null) {
                return null;
            }
            java.lang.reflect.Field f = info.getClass().getField("topActivity");
            Object cn = f.get(info);
            if (!(cn instanceof ComponentName)) {
                return null;
            }
            return ((ComponentName) cn).getPackageName();
        } catch (Throwable t) {
            Log.w(TAG, "topActivity 判定失败（降级 hook focus）: " + t);
            return null;
        }
    }

    /** 前台服务判定：IActivityManager.getServices → RunningServiceInfo.flags */
    private static boolean hasForegroundService(String pkg) {
        try {
            Object iam = activityManagerService();
            if (iam == null) {
                return false;
            }
            Method getServices = iam.getClass().getMethod("getServices", int.class, int.class);
            Object list = getServices.invoke(iam, 300, 0);
            if (!(list instanceof List)) {
                return false;
            }
            for (Object item : (List<?>) list) {
                if (!(item instanceof RunningServiceInfo)) {
                    continue;
                }
                RunningServiceInfo info = (RunningServiceInfo) item;
                if (info.service != null
                        && pkg.equals(info.service.getPackageName())
                        && (info.flags & START_FLAG_FOREGROUND) != 0) {
                    return true;
                }
            }
            return false;
        } catch (Throwable t) {
            Log.w(TAG, "前台服务判定失败（缺省 false）: " + t);
            return false;
        }
    }

    /** 媒体播放判定：IAudioService.getActivePlaybackConfigurations → getClientUid 匹配 pkg uid */
    private static boolean hasActiveMedia(String pkg) {
        try {
            Integer uid = packageUid(pkg);
            if (uid == null || uid <= 0) {
                return false;
            }
            IBinder audio = getServiceBinder("audio");
            if (audio == null) {
                return false;
            }
            Class<?> stub = Class.forName("android.media.IAudioService$Stub");
            Method asInterface = stub.getMethod("asInterface", IBinder.class);
            Object ias = asInterface.invoke(null, audio);
            Method gac = ias.getClass().getMethod("getActivePlaybackConfigurations");
            Object list = gac.invoke(ias);
            if (!(list instanceof List)) {
                return false;
            }
            for (Object item : (List<?>) list) {
                if (item == null) {
                    continue;
                }
                try {
                    Method getUid = item.getClass().getMethod("getClientUid");
                    Object v = getUid.invoke(item);
                    if (v instanceof Integer && ((Integer) v).intValue() == uid.intValue()) {
                        return true;
                    }
                } catch (Throwable ignored) {
                    // 单个配置项反射失败，跳过继续
                }
            }
            return false;
        } catch (Throwable t) {
            Log.w(TAG, "媒体判定失败（缺省 false）: " + t);
            return false;
        }
    }

    /** IActivityManager：ServiceManager.getService("activity") + IActivityManager$Stub.asInterface
     *  （标准 AIDL 桥。ActivityManager.getService() 在 Android16 返回实现类，其
     *   getPackageUid 等签名随版本漂移（真机实证 NoSuchMethodException），
     *   proxy 类稳定暴露接口方法） */
    private static Object activityManagerService() {
        try {
            IBinder b = getServiceBinder("activity");
            if (b == null) {
                return null;
            }
            Class<?> stub = Class.forName("android.app.IActivityManager$Stub");
            Method asInterface = stub.getMethod("asInterface", IBinder.class);
            return asInterface.invoke(null, b);
        } catch (Throwable t) {
            Log.w(TAG, "IActivityManager 获取失败: " + t);
            return null;
        }
    }

    /** ServiceManager.getService（@hide，全反射；android.jar 无此符号） */
    private static IBinder getServiceBinder(String name) {
        try {
            Class<?> sm = Class.forName("android.os.ServiceManager");
            Method m = sm.getMethod("getService", String.class);
            Object v = m.invoke(null, name);
            return (v instanceof IBinder) ? (IBinder) v : null;
        } catch (Throwable t) {
            Log.w(TAG, "ServiceManager.getService 反射失败 (" + name + "): " + t);
            return null;
        }
    }

    /** pkg → uid（user 0）：IActivityManager.getPackageUid（遍历签名适配——AMS 各版本
     *  2 参/3 参/私有化漂移，真机实证 getMethod 按签名查找不可靠） */
    private static Integer packageUid(String pkg) {
        try {
            Object iam = activityManagerService();
            if (iam == null) {
                return null;
            }
            Method target = null;
            for (Method m : iam.getClass().getMethods()) {
                if (!m.getName().equals("getPackageUid")) {
                    continue;
                }
                int pc = m.getParameterTypes().length;
                if (pc == 2 || pc == 3) {
                    target = m;
                    break;
                }
            }
            if (target == null) {
                return null;
            }
            Object v = (target.getParameterTypes().length == 2)
                    ? target.invoke(iam, pkg, 0)
                    : target.invoke(iam, pkg, 0, 0);
            return (v instanceof Integer) ? (Integer) v : null;
        } catch (Throwable t) {
            Log.w(TAG, "getPackageUid 反射失败: " + t);
            return null;
        }
    }
}
