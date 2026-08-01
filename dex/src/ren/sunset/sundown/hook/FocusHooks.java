package ren.sunset.sundown.hook;

import android.content.ComponentName;
import android.util.Log;

import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.List;

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
    private final List<Hooker> hookers = new ArrayList<>();

    FocusHooks(LsPlantBridge.EventDispatcher dispatcher) {
        this.dispatcher = dispatcher;
    }

    @Override
    public void install() {
        Class<?> ams = findClass(AMS);

        // 焦点切换：AMS 优先，ATMS 兜底（版本迁移差异）
        Class<?> switchHost = (findMethod(ams, "updateActivityUsageStats") != null)
                ? ams : findClass(ATMS);
        hookAllOverloads(switchHost, "updateActivityUsageStats",
                callback("onActivitySwitch"));

        // 进程生命周期
        hookAllOverloads(ams, "addPidLocked", callback("onPidAdd"));
        hookAllOverloads(ams, "removePidLocked", callback("onPidRemove"));
        hookAllOverloads(ams, "forceStopPackage", callback("onForceStop"));

        Log.i(TAG, "FocusHooks 安装完成（hook 数: " + hookers.size() + "）");
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
        Log.i(TAG, "FocusHooks 已卸载");
    }

    // ---------------- hook 回调（实例方法，签名 Object xxx(MethodCallback)） ----------------

    /** activity 切换信号：扫参数中的 ComponentName 取包名（签名跨版本鲁棒） */
    public Object onActivitySwitch(MethodCallback cb) {
        Object res = cb.invokeOriginalOrDefault();
        try {
            String pkg = null;
            for (Object a : cb.args) {
                if (a instanceof ComponentName) {
                    pkg = ((ComponentName) a).getPackageName();
                    break;
                }
            }
            if (pkg != null) {
                dispatcher.dispatch("event focus pkg=" + pkg);
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

    /** 从实参提取 pid：Integer 直取，否则反射 ProcessRecord.pid（低频事件，逐次反射可接受） */
    private void reportPid(MethodCallback cb, String kind) {
        try {
            for (Object a : cb.args) {
                Integer pid = extractPid(a);
                if (pid != null) {
                    dispatcher.dispatch("event " + kind + " pid=" + pid);
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

    private Method callback(String name) {
        try {
            return FocusHooks.class.getDeclaredMethod(name, MethodCallback.class);
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

    /** hook 指定类的全部同名重载（抽象/桥接跳过）；单点失败不拖垮其余重载 */
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