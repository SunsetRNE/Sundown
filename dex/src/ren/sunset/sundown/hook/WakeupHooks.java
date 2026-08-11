package ren.sunset.sundown.hook;

import android.content.ComponentName;
import android.content.Intent;
import android.util.Log;

import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
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
        // 广播投递：A14+ BroadcastController，旧版 AMS 兜底
        Class<?> bcHost = (findMethod(findClass(BC), "broadcastIntentLocked") != null)
                ? findClass(BC) : findClass(AMS);
        hookAllOverloads(bcHost, "broadcastIntentLocked", callback("onBroadcast"));

        hookAllOverloads(findClass(AS), "realStartServiceLocked", callback("onServiceStart"));
        hookAllOverloads(findClass(PIR), "sendInner", callback("onPendingSend"));

        Log.i(TAG, "WakeupHooks 安装完成（hook 数: " + hookers.size() + "）");
    }

    @Override
    public void uninstall() {
        for (Hooker h : hookers) {
            try {
                h.unhook();
            } catch (Throwable t) {
                Log.w(TAG, "unhook 异常: " + t);
            }
        }
        hookers.clear();
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

    private Method callback(String name) {
        try {
            return WakeupHooks.class.getDeclaredMethod(name, MethodCallback.class);
        } catch (Throwable t) {
            Log.e(TAG, "回调方法缺失: " + name + " -> " + t);
            return null;
        }
    }

    private static Class<?> findClass(String name) {
        // services.jar 不在 BOOTCLASSPATH（SYSTEMSERVERCLASSPATH 专属 loader 加载），
        // 注入 dex 的 loader 链不可见——必须经 ServerClasses 解析（v0.3.4-l2 修复）
        return ServerClasses.find(name);
    }

    private static Method findMethod(Class<?> cls, String name) {
        if (cls == null) return null;
        for (Method m : cls.getDeclaredMethods()) {
            if (m.getName().equals(name)) return m;
        }
        return null;
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