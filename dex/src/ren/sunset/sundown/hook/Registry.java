package ren.sunset.sundown.hook;

import android.util.Log;

import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.List;

import ren.sunset.sundown.hook.NativeBridge.Hooker;
import ren.sunset.sundown.hook.NativeBridge.MethodCallback;

/**
 * Hook 注册表（v0.6-l3，缺口补入清单 B1——统一注册层，docs/l2b-plan.md 演进）。
 *
 * 背景（B1 设计裁决）：hook 点此前在各 HookEngine.install() 硬编码（findClass +
 * hookAllOverloads 逐行写），存在三个缺口：
 *   ① 无注册条目视图（hook 点/能力依赖不可观测、不可枚举）；
 *   ② 安装失败仅"跳过"（部分成功部分失败时状态未知，无法整体回滚）；
 *   ③ 公共工具（findClass/callback/hookAllOverloads）在每组重复实现。
 *
 * 本类收敛公共机制，条目化描述 hook 点，并引入 critical 回滚语义：
 *   - 普通条目失败 → 跳过继续（现状不变，零风险）；
 *   - critical 条目失败 → 整组回滚（unhook 已装条目 + 抛异常由 Runtime 记录）——
 *     防御类 hook（ANR 隐身/防双冻结）缺一角 = 半装误导，宁可不装。
 *
 * 铁律（对齐工程红线）：
 *   - 禁 lambda / 方法引用（android.jar 无 LambdaMetafactory，javac -source 8 fatal）；
 *   - 类解析一律经 ServerClasses（services.jar 不在 BOOTCLASSPATH，v0.3.4-l2 修复）；
 *   - 不做服务注册中心 / 不做动态发现（防过度抽象，注册表只描述不调度）。
 */
public final class Registry {

    private static final String TAG = "SundownDex";

    /** 单条 hook 注册描述 */
    public static final class Entry {
        /** 唯一 id（status/env-check 观测面，如 "focus.switch"） */
        public final String id;
        /** 宿主类名（经 ServerClasses 解析） */
        public final String host;
        /** 宿主兜底类名（版本迁移差异，如 AMS→ATMS；可 null） */
        public final String fallbackHost;
        /** 目标方法名（hook 全部同名重载） */
        public final String method;
        /** 回调方法名（宿主 engine 内的实例方法，签名 Object xxx(MethodCallback)） */
        public final String callback;
        /** true = 安装失败整组回滚；false = 失败仅跳过（默认，保持既有行为） */
        public final boolean critical;
        /** 能力依赖说明（文档化，供 env-check 导出） */
        public final String capability;

        Entry(String id, String host, String fallbackHost, String method,
              String callback, boolean critical, String capability) {
            this.id = id;
            this.host = host;
            this.fallbackHost = fallbackHost;
            this.method = method;
            this.callback = callback;
            this.critical = critical;
            this.capability = capability;
        }
    }

    /** 构建条目（critical=false 缺省；capability 缺省 ""） */
    public static Entry entry(String id, String host, String fallbackHost,
                              String method, String callback) {
        return new Entry(id, host, fallbackHost, method, callback, false, "");
    }

    /** 构建条目（含 critical / capability） */
    public static Entry entry(String id, String host, String fallbackHost,
                              String method, String callback, boolean critical,
                              String capability) {
        return new Entry(id, host, fallbackHost, method, callback, critical, capability);
    }

    /** 注册表描述视图（status/env-check 用）："id host#method→callback [critical]" */
    public static List<String> describe(List<Entry> entries) {
        List<String> out = new ArrayList<>();
        for (Entry e : entries) {
            StringBuilder sb = new StringBuilder();
            sb.append(e.id).append(' ').append(e.host).append('#').append(e.method)
                    .append("→").append(e.callback);
            if (e.fallbackHost != null) sb.append(" (fallback ").append(e.fallbackHost).append(')');
            if (e.critical) sb.append(" [critical]");
            if (!e.capability.isEmpty()) sb.append(" — ").append(e.capability);
            out.add(sb.toString());
        }
        return out;
    }

    /**
     * 安装一组注册条目（v0.6-l3 B1 核心）。
     *
     * 语义：
     *   - 类解析失败（ServerClasses 找不到）→ 尝试 fallbackHost；双失败 = 跳过（logw）
     *     ——critical 条目则整组回滚并抛 IllegalStateException（由 Runtime 记录）；
     *   - 回调方法缺失 → 同上（跳过或回滚）；
     *   - 方法未 hook 到（重载全跳过/无匹配）→ 同上；
     *   - 成功 hook 的 Hooker 追加到 out（调用方 uninstall 用）。
     *
     * @return 成功 hook 的重载总数
     * @throws IllegalStateException critical 条目失败时（已回滚本组全部已装 hook）
     */
    public static int installGroup(Object engine, List<Entry> entries, List<Hooker> out) {
        int total = 0;
        for (Entry e : entries) {
            Class<?> cls = findClass(e.host);
            if (cls == null && e.fallbackHost != null) {
                cls = findClass(e.fallbackHost);
            }
            if (cls == null) {
                if (rollbackOrThrow(engine, e, "类未找到: " + e.host
                        + (e.fallbackHost != null ? " / " + e.fallbackHost : ""), out)) {
                    return -1;
                }
                continue;
            }
            Method cb = callbackOf(engine, e.callback);
            if (cb == null) {
                if (rollbackOrThrow(engine, e, "回调缺失: " + e.callback, out)) {
                    return -1;
                }
                continue;
            }
            int n = hookAllOverloads(cls, e.method, cb, engine, out);
            total += n;
            if (n == 0) {
                if (rollbackOrThrow(engine, e, "方法未 hook 到: "
                        + cls.getName() + "#" + e.method, out)) {
                    return -1;
                }
            }
        }
        return total;
    }

    /** critical 条目失败 → 回滚已装 + 抛异常；普通条目 → logw 跳过。返回 true=已回滚 */
    private static boolean rollbackOrThrow(Object engine, Entry e, String reason,
                                           List<Hooker> out) {
        if (e.critical) {
            Log.w(TAG, "注册表 critical 条目失败，整组回滚: " + e.id + "（" + reason + "）");
            uninstallAll(out);
            throw new IllegalStateException("hook critical 回滚: " + e.id + " " + reason);
        }
        Log.w(TAG, "注册表条目跳过: " + e.id + "（" + reason + "）");
        return false;
    }

    /** 卸载全部 Hooker（幂等；单点失败不拖垮其余） */
    public static void uninstallAll(List<Hooker> hookers) {
        for (Hooker h : hookers) {
            try {
                h.unhook();
            } catch (Throwable t) {
                Log.w(TAG, "unhook 异常: " + t);
            }
        }
        hookers.clear();
    }

    /** 解析宿主 engine 的回调方法（签名 Object xxx(MethodCallback)） */
    public static Method callbackOf(Object engine, String name) {
        try {
            return engine.getClass().getDeclaredMethod(name, MethodCallback.class);
        } catch (Throwable t) {
            Log.e(TAG, "回调方法缺失: " + name + " -> " + t);
            return null;
        }
    }

    /** hook 指定类的全部同名重载（抽象/桥接跳过）；成功 Hooker 追加到 out */
    public static int hookAllOverloads(Class<?> cls, String name, Method callback,
                                       Object engine, List<Hooker> out) {
        if (cls == null || callback == null) {
            Log.w(TAG, "hook 跳过（类或回调缺失）: " + name);
            return 0;
        }
        int ok = 0;
        for (Method m : cls.getDeclaredMethods()) {
            if (!m.getName().equals(name)) continue;
            if (Modifier.isAbstract(m.getModifiers()) || m.isBridge() || m.isSynthetic()) continue;
            Hooker h = NativeBridge.hook(m, callback, engine);
            if (h != null) {
                out.add(h);
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

    /** services.jar 不在 BOOTCLASSPATH（SYSTEMSERVERCLASSPATH 专属 loader 加载），
     *  注入 dex 的 loader 链不可见——必须经 ServerClasses 解析（v0.3.4-l2 修复） */
    public static Class<?> findClass(String name) {
        return ServerClasses.find(name);
    }

    private Registry() {
    }
}
