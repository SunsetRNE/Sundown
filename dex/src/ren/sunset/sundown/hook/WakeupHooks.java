package ren.sunset.sundown.hook;

import android.content.ComponentName;
import android.content.Intent;
import android.util.Log;

import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.List;

import ren.sunset.sundown.hook.NativeBridge.Hooker;
import ren.sunset.sundown.hook.NativeBridge.MethodCallback;

/**
 * 唤醒入口 hook 组（L2b 第二组，观测模式）。
 *
 * hook 点（AStop v1.6.0 实证，docs/l2b-plan.md §1）：
 *   BroadcastController(A14+) / ActivityManagerService(<14) #broadcastIntentLocked
 *   ActiveServices#realStartServiceLocked
 *   PendingIntentRecord#sendInner
 *
 * 本刀只感知不上动作（无自冻对象）：命中 → event wakeup 上行，daemon 计数观测。
 * TODO-L3：ProcessReceiverRecord addCurReceiver/removeCurReceiver（active receiver
 *          门禁数据源，协议消费面随 L3 策略引擎一起上）。
 */
final class WakeupHooks implements HookEngine {

    private static final String TAG = "SundownDex";
    private static final String AMS = "com.android.server.am.ActivityManagerService";
    private static final String BC = "com.android.server.am.BroadcastController";
    private static final String AS = "com.android.server.am.ActiveServices";
    private static final String PIR = "com.android.server.am.PendingIntentRecord";

    private final LsPlantBridge.EventDispatcher dispatcher;
    private final List<Hooker> hookers = new ArrayList<>();

    WakeupHooks(LsPlantBridge.EventDispatcher dispatcher) {
        this.dispatcher = dispatcher;
    }

    @Override
    public void install() {
        // v0.6-l3（缺口补入清单 B1）：注册表驱动——hook 点条目化，公共机制收敛至 Registry。
        // 广播宿主 BC(A14+)/AMS(<14) 版本迁移经 Registry fallbackHost 处理；
        // critical=false（观测组，失败仅跳过）；注册表描述可经 env-check 导出。
        int n = Registry.installGroup(this, entries(), hookers);
        Log.i(TAG, "WakeupHooks 安装完成（hook 数: " + n + "）");
    }

    /** B1 注册条目 */
    private static List<Registry.Entry> entries() {
        List<Registry.Entry> list = new ArrayList<>();
        // 广播投递：A14+ BroadcastController，旧版 AMS 兜底
        list.add(Registry.entry("wakeup.broadcast", BC, AMS, "broadcastIntentLocked",
                "onBroadcast", false, "广播投递（含 action，Receiver gate 数据源）"));
        list.add(Registry.entry("wakeup.service", AS, null, "realStartServiceLocked",
                "onServiceStart", false, "服务启动（含组件名，Service gate 数据源）"));
        list.add(Registry.entry("wakeup.pendingintent", PIR, null, "sendInner",
                "onPendingSend", false, "PendingIntent 触发（含组件名，PendingIntent gate 数据源）"));
        return list;
    }

    @Override
    public void uninstall() {
        // v0.6-l3（B1）：卸载收敛至 Registry（幂等，单点失败不拖垮其余）
        Registry.uninstallAll(hookers);
        Log.i(TAG, "WakeupHooks 已卸载");
    }

    // ---------------- hook 回调 ----------------

    /** 广播投递：参数中找 Intent（直参或 BroadcastRecord.intent 字段）；v0.4.43-l3 携带 action 供 Receiver gate 门控 */
    public Object onBroadcast(MethodCallback cb) {
        Object res = cb.invokeOriginalOrDefault();
        try {
            Intent intent = null;
            for (Object a : cb.args) {
                intent = extractIntent(a);
                if (intent != null) break;
            }
            if (intent != null) {
                String action = intent.getAction();
                reportWakeup(pkgOf(intent), "broadcast",
                        (action != null) ? action : "?");
            }
        } catch (Throwable t) {
            Log.w(TAG, "广播事件提取失败: " + t);
        }
        return res;
    }

    /** 服务启动：ServiceRecord 实参 → packageName/name/appInfo 字段枚举；v0.6-l3 携带组件名
     *  （ServiceRecord.name ComponentName flatten，Service gate 门控匹配键） */
    public Object onServiceStart(MethodCallback cb) {
        Object res = cb.invokeOriginalOrDefault();
        try {
            for (Object a : cb.args) {
                String pkg = extractPkgFromRecord(a);
                if (pkg != null) {
                    reportWakeup(pkg, "service", componentFromRecord(a));
                    break;
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "服务事件提取失败: " + t);
        }
        return res;
    }

    /** PendingIntent 触发：this（args[0]）→ key.packageName；v0.6-l3 携带组件名
     *  （key.intent component flatten，PendingIntent gate 门控匹配键） */
    public Object onPendingSend(MethodCallback cb) {
        Object res = cb.invokeOriginalOrDefault();
        try {
            String pkg = null;
            Object self = cb.args.length > 0 ? cb.args[0] : null;
            Object key = readField(self, "key");
            if (key != null) {
                pkg = readStringField(key, "packageName");
            }
            if (pkg != null) {
                reportWakeup(pkg, "pendingintent", componentFromKey(key));
            }
        } catch (Throwable t) {
            Log.w(TAG, "PendingIntent 事件提取失败: " + t);
        }
        return res;
    }

    // ---------------- 工具 ----------------

    private void reportWakeup(String pkg, String reason) {
        dispatcher.dispatch("event wakeup pkg=" + pkg + " reason=" + reason);
    }

    /** v0.4.43-l3：带广播 action（Receiver gate 门控数据源；仅 broadcast 源携带）
     *  v0.6-l3：service/pendingintent 源复用此入口，第三参 = 组件 flatten（缺省 "?"） */
    private void reportWakeup(String pkg, String reason, String key) {
        dispatcher.dispatch("event wakeup pkg=" + pkg + " reason=" + reason + " action=" + key);
    }

    /** ServiceRecord → 组件名（匹配键）：name(ComponentName) flatten 优先，intent.getComponent 兜底 */
    private static String componentFromRecord(Object rec) {
        if (rec == null) return "?";
        Object name = readField(rec, "name");
        if (name instanceof ComponentName) return ((ComponentName) name).flattenToString();
        Object intent = readField(rec, "intent");
        if (intent instanceof Intent) {
            ComponentName c = ((Intent) intent).getComponent();
            if (c != null) return c.flattenToString();
        }
        return "?";
    }

    /** PendingIntentRecord.Key → 组件名（匹配键）：key.intent component flatten */
    private static String componentFromKey(Object key) {
        if (key == null) return "?";
        Object intent = readField(key, "intent");
        if (intent instanceof Intent) {
            ComponentName c = ((Intent) intent).getComponent();
            if (c != null) return c.flattenToString();
        }
        return "?";
    }

    private static String pkgOf(Intent intent) {
        String pkg = intent.getPackage();
        if (pkg == null) {
            ComponentName c = intent.getComponent();
            if (c != null) pkg = c.getPackageName();
        }
        return (pkg != null) ? pkg : "?";
    }

    private static Intent extractIntent(Object arg) {
        if (arg instanceof Intent) return (Intent) arg;
        Object v = readField(arg, "intent");
        return (v instanceof Intent) ? (Intent) v : null;
    }

    /** ServiceRecord 等多版本字段枚举：packageName(String) → name(ComponentName) → appInfo.packageName */
    private static String extractPkgFromRecord(Object rec) {
        if (rec == null) return null;
        String pkg = readStringField(rec, "packageName");
        if (pkg != null) return pkg;
        Object name = readField(rec, "name");
        if (name instanceof ComponentName) return ((ComponentName) name).getPackageName();
        Object appInfo = readField(rec, "appInfo");
        if (appInfo != null) return readStringField(appInfo, "packageName");
        return null;
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

    private static String readStringField(Object obj, String name) {
        Object v = readField(obj, name);
        return (v instanceof String) ? (String) v : null;
    }
}