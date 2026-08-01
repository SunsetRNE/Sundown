package ren.sunset.sundown.hook;

import android.os.IBinder;
import android.util.Log;

import java.lang.ref.WeakReference;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;

/**
 * system_server 专属类解析器（v0.3.4-l2 修复：L2b hook 类可见性）。
 *
 * 背景（真机实证，PJD110 / Android 16）：
 *   services.jar / oplus-services.jar 不在 BOOTCLASSPATH，而是由 system_server
 *   专属 PathClassLoader（SYSTEMSERVERCLASSPATH 环境变量）加载。注入 dex 的
 *   loader 链（bridgeLoader → system CL → BootClassLoader）看不到这些类——
 *   单参 Class.forName 必败（v0.3.3-l2 实机：FocusHooks/WakeupHooks 8 个 hook
 *   点全部 "类未找到"，hook 数 0，focus/wakeup 观测空转）。
 *
 * 解法：从 Binder 服务反推 services loader——ServiceManager.getService("activity")
 * 返回的 Binder 实现类（AOSP ActivityManagerService / 厂商 Oplus 子类）定义于
 * SYSTEMSERVERCLASSPATH，其 getClass().getClassLoader() 即加载 services.jar 的
 * PathClassLoader。ART 按 (loader, name) 唯一定位类，由此 Class.forName 得到的
 * Class 即运行时实际调用份，LSPlant hook 有效。命中后 WeakReference 缓存。
 *
 * 候选顺序：缓存 → Binder 服务反推 → 当前线程 context → "main" 线程 context
 * → system CL → 兜底单参。纪律：与项目一致禁 lambda；任何候选失败不抛异常。
 */
final class ServerClasses {

    private static final String TAG = "SundownDex";

    /** 命中缓存的 loader（弱引用；热切换新代复用，旧 loader 可被 GC） */
    private static volatile WeakReference<ClassLoader> cached;

    private ServerClasses() {
    }

    /** 解析目标类；全部候选不可见 → null（调用方记录 "类未找到"） */
    static Class<?> find(String name) {
        for (ClassLoader cl : candidates()) {
            try {
                return Class.forName(name, false, cl);
            } catch (Throwable ignored) {
                // 该候选不可见，继续下一 loader
            }
        }
        try {
            return Class.forName(name); // 兜底：调用者 loader（dev/非 system_server 场景）
        } catch (Throwable t) {
            Log.w(TAG, "类未找到: " + name + "（全部 loader 候选不可见）");
            return null;
        }
    }

    private static List<ClassLoader> candidates() {
        List<ClassLoader> list = new ArrayList<>();

        WeakReference<ClassLoader> c = cached;
        ClassLoader cachedCl = (c != null) ? c.get() : null;
        if (cachedCl != null) list.add(cachedCl);

        ClassLoader binderCl = servicesLoaderFromBinder();
        if (binderCl != null && !list.contains(binderCl)) list.add(binderCl);

        ClassLoader ctx = Thread.currentThread().getContextClassLoader();
        if (ctx != null && !list.contains(ctx)) list.add(ctx);

        ClassLoader mainCl = mainThreadContextClassLoader();
        if (mainCl != null && !list.contains(mainCl)) list.add(mainCl);

        ClassLoader sys = ClassLoader.getSystemClassLoader();
        if (sys != null && !list.contains(sys)) list.add(sys);

        if (!list.isEmpty()) cached = new WeakReference<>(list.get(0));
        return list;
    }

    /**
     * 从 Binder 服务反推 services loader（首选，真机验证可靠）：
     * ServiceManager.getService("activity") → AMS 的 Binder 实现（system_server
     * 内为本地对象），getClass() 即 services.jar/oplus-services.jar 中的类，
     * 其 ClassLoader 就是加载 SYSTEMSERVERCLASSPATH 的 PathClassLoader。
     * getService 为 @SystemApi（android.jar 不含），走反射；dex 已 MakeDexFileTrusted
     * （hidden API 解封），运行期调用无拦。
     */
    private static ClassLoader servicesLoaderFromBinder() {
        try {
            Class<?> sm = Class.forName("android.os.ServiceManager");
            Method m = sm.getMethod("getService", String.class);
            IBinder b = (IBinder) m.invoke(null, "activity");
            if (b != null) {
                ClassLoader cl = b.getClass().getClassLoader();
                if (cl != null) {
                    Log.i(TAG, "services loader 获取成功（binder 实现: "
                            + b.getClass().getName() + "）");
                    return cl;
                }
            }
        } catch (Throwable ignored) {
        }
        return null;
    }

    /**
     * system_server 主线程（Java 名 "main"）的 contextClassLoader 后备方案。
     * ZygoteInit.handleSystemServerProcess：systemServerClasspath
     * （=SYSTEMSERVERCLASSPATH）→ createPathClassLoader →
     * Thread.currentThread().setContextClassLoader(cl)。注：部分厂商 ROM 主线程
     * pthread comm 为 "system_server"（非 "main"），Java 名仍为 "main"；本方案仅后备。
     */
    private static ClassLoader mainThreadContextClassLoader() {
        try {
            ThreadGroup root = Thread.currentThread().getThreadGroup();
            while (root.getParent() != null) {
                root = root.getParent();
            }
            int n = root.activeCount();
            Thread[] threads = new Thread[n * 2 + 8];
            int m = root.enumerate(threads);
            for (int i = 0; i < m; i++) {
                Thread t = threads[i];
                if (t != null && "main".equals(t.getName())) {
                    return t.getContextClassLoader();
                }
            }
        } catch (Throwable ignored) {
        }
        return null;
    }
}
