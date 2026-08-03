package ren.sunset.sundown;

import android.util.Log;

import org.json.JSONObject;

import java.lang.reflect.Method;
import java.nio.ByteBuffer;
import dalvik.system.InMemoryDexClassLoader;
import ren.sunset.sundown.hook.HookEngine;
import ren.sunset.sundown.hook.LsPlantBridge;

/**
 * 单代 dex 的运行态：daemon 长连接事件循环 + Hook 编排 + 热切换。
 *
 * 代际模型：每次热切换产生新一代 Runtime（新 ClassLoader 加载，静态字段按
 * ClassLoader 隔离，两代互不可见）。新代建链验证成功后旧代 shutdown（断连、
 * 卸 hook、清静态引用 → 旧 ClassLoader 可被 GC 卸载）；任何失败旧代原样保留 = 回滚。
 *
 * dex 字节全程走 socket（daemon root 读文件下发），本层不触碰任何
 * /data/adb 路径——uid 1000 在 DAC 层不可达该目录（L1 已实证）。
 */
final class Runtime {

    private static final String TAG = "SundownDex";
    /** daemon 断线重连间隔（对齐 L1 桩握手重试哲学：2s 节拍，无限窗口） */
    private static final long RECONNECT_MS = 2000;

    /** 本代单例（每代 dex 的 ClassLoader 各持一份） */
    private static Runtime active;

    private final String socketName;
    private final String stubHash;
    private final String version = BuildInfo.DEX_BUILD_VERSION;
    private final HookEngine hooks;
    private final Thread eventThread;
    /** L2b：hook 事件缓冲（回调非阻塞投递，发送线程串行 drain） */
    private final EventQueue events = new EventQueue();
    private final Thread senderThread;
    /** L3：豁免判定监视器（独立线程，fg/media 上行） */
    private final ExemptMonitor exemptMonitor;

    private volatile boolean stopped;
    private volatile boolean swapping;   // 同代内切换去重（窗口期重复事件防护）
    private volatile boolean hooksInstalled;
    private volatile DaemonLink link;    // 当前订阅连接（shutdown 时关闭以打断读循环）

    private Runtime(String socketName, String stubHash) {
        this.socketName = socketName;
        this.stubHash = stubHash;
        // L3：豁免判定监视器（独立线程，需 dispatcher 上行）
        this.exemptMonitor = new ExemptMonitor(new LsPlantBridge.EventDispatcher() {
            @Override
            public void dispatch(String line) {
                events.offer(line);
            }
        });
        this.hooks = LsPlantBridge.create(new LsPlantBridge.EventDispatcher() {
            @Override
            public void dispatch(String line) {
                events.offer(line); // 非阻塞（hook 回调可能在 AMS 锁内线程）
            }
        }, this.exemptMonitor);
        // 注意：禁止 lambda/方法引用——javac -source 8 + -bootclasspath android.jar 时
        // lambda 的 invokedynamic 需在 bootclasspath 解析 LambdaMetafactory.metafactory，
        // 而 android.jar 无此符号（编译期 fatal）。匿名类由 d8 原样保留，无 desugar 依赖。
        this.eventThread = new Thread(new Runnable() {
            @Override
            public void run() {
                eventLoop();
            }
        }, "SundownDex-Events");
        this.eventThread.setDaemon(true);
        this.senderThread = new Thread(new Runnable() {
            @Override
            public void run() {
                senderLoop();
            }
        }, "SundownDex-Sender");
        this.senderThread.setDaemon(true);
    }

    /** 冷启动（L1 桩 → ProbeMain.init）：建运行态即返回，建链在事件线程内重试推进 */
    static synchronized void startCold(String socketName, String stubHash) {
        if (active != null) {
            Log.w(TAG, "重复 init，忽略（已有运行态 v" + active.version + "）");
            return;
        }
        Runtime r = new Runtime(socketName, stubHash);
        active = r;
        r.eventThread.start();
        r.senderThread.start();
        r.exemptMonitor.start();
        Log.i(TAG, "L2 dex 冷启动 (v" + r.version + ", stub=" + stubHash + ")");
    }

    /**
     * 热切换（旧代 → ProbeMain.hotSwap）：先向 daemon 验明正身（连得上 + hello 被接受），
     * 失败抛异常 = 旧代保留（回滚）；成功则新代上线，由旧代随后自杀。
     */
    static synchronized boolean startHot(String socketName, String stubHash, String prevVersion) throws Exception {
        if (active != null) {
            Log.i(TAG, "新代已在线 (v" + active.version + ")，hotSwap 幂等成功");
            return true;
        }
        Runtime r = new Runtime(socketName, stubHash);
        r.verifyAlive(); // 不可达/被拒 → 抛异常 → 回滚
        active = r;
        r.eventThread.start();
        r.senderThread.start();
        r.exemptMonitor.start();
        Log.i(TAG, "L2 dex 热切换上线 (v" + prevVersion + " → v" + r.version + ")");
        return true;
    }

    /** 切换前验证：新代能独立建链且 hello-dex 被 daemon 接受（一次性短连接） */
    private void verifyAlive() throws Exception {
        DaemonLink probe = new DaemonLink(socketName);
        try {
            probe.connect();
            JSONObject hello = probe.helloDex(version);
            if (hello.optInt("ok", 0) != 1) {
                throw new IllegalStateException("hello-dex 被 daemon 拒绝: " + hello);
            }
        } finally {
            probe.close();
        }
    }

    // ---------------- 事件循环（含断线重连） ----------------

    private void eventLoop() {
        while (!stopped) {
            DaemonLink l = null;
            try {
                l = new DaemonLink(socketName);
                l.connect();
                link = l;
                JSONObject hello = l.helloDex(version);
                // L2b 引导代 → 工作代自热切换（桩冷启 gen0 无 canonical NativeBridge 父链；
                // hop 后本代 shutdown，由新代重装全部机制——见 LsPlantBridge 类加载拓扑）
                if (LsPlantBridge.needsGenerationHop()) {
                    Log.i(TAG, "引导代检测（无 bridge 父链），自热切换到工作代 (v" + version + ")");
                    byte[] dex = DaemonLink.fetchDex(socketName);
                    if (dex != null) {
                        swapTo(dex, version);
                        if (stopped) return; // 新代接管成功，本代已 shutdown
                        Log.w(TAG, "引导代自热切换失败（回滚），以无 hook 降级继续运行");
                    } else {
                        Log.w(TAG, "引导代 fetch-dex 失败，以无 hook 降级继续运行");
                    }
                }
                onHello(hello);
                // L2b：上报伴生库 build hash（native 可用时；应答行由读循环静默吞掉）
                String bridgeHash = LsPlantBridge.bridgeBuildHash();
                if (bridgeHash != null) {
                    l.writeLine("report-bridge " + bridgeHash);
                }
                // 订阅事件流（阻塞读；EOF/异常 → 重连）
                String line;
                while (!stopped && (line = l.readLine()) != null) {
                    if (!line.isEmpty()) onEvent(l, line);
                }
            } catch (Throwable t) {
                if (!stopped) Log.w(TAG, "daemon 连接中断: " + t);
            } finally {
                link = null;
                if (l != null) l.close();
            }
            if (!stopped) {
                try { Thread.sleep(RECONNECT_MS); } catch (InterruptedException ignored) {}
            }
        }
        Log.i(TAG, "事件循环退出 (v" + version + ")");
    }

    /** 事件发送循环（L2b）：drain EventQueue → socket；断线期保序等待，不丢不乱序 */
    private void senderLoop() {
        String pending = null;
        while (!stopped) {
            try {
                if (pending == null) {
                    pending = events.poll();
                    if (pending == null) continue; // 队列空（500ms 节拍）
                }
                DaemonLink l = link;
                if (l == null) {
                    Thread.sleep(200); // 断线窗口：持回 pending 保序等待
                    continue;
                }
                l.writeLine(pending);
                pending = null;
            } catch (InterruptedException e) {
                break; // shutdown
            } catch (Throwable t) {
                Log.w(TAG, "事件发送失败（保序重试）: " + t);
                try {
                    Thread.sleep(500);
                } catch (InterruptedException e) {
                    break;
                }
            }
        }
        Log.i(TAG, "事件发送线程退出 (v" + version + ")");
    }

    /** hello-dex 应答处理：安装 hook（一次性）+ 版本落后则 fetch-dex 自愈 */
    private void onHello(JSONObject hello) {
        // L3：每次成功握手（含 daemon 重启后的重连）都重置豁免上报状态——
        // daemon 重启清空其 exempt 表，本侧判定值未变化时不会自发重发；
        // 重置 sent 后下一节拍全量重报，保证新 daemon 豁免表完整（防误冻）。
        exemptMonitor.reset();
        if (!hooksInstalled) {
            hooksInstalled = true; // 先置位防重入；失败下轮重连再试
            try {
                hooks.install();
            } catch (Throwable t) {
                Log.e(TAG, "hook 编排安装失败（骨架阶段不致命）: " + t, t);
            }
        }
        int match = hello.optInt("dex_hash_match", -1);
        String expected = emptyToNull(hello.optString("expected_dex_hash", null));
        Log.i(TAG, "hello-dex 完成: v" + version + " expected=" + expected + " match=" + match);
        // 自愈：daemon 期望版本与本地不一致 → 拉取最新 dex 字节并切换
        if (match == 0 && expected != null && !expected.equals(version)) {
            Log.i(TAG, "本地版本落后（v" + version + " → " + expected + "），fetch-dex 自愈");
            byte[] dex = DaemonLink.fetchDex(socketName);
            if (dex != null) swapTo(dex, expected);
        }
    }

    /** 订阅事件分发：dex-push（头行 + 字节帧）+ L2b 上行命令应答（静默吞掉） */
    private void onEvent(DaemonLink l, String line) {
        try {
            JSONObject ev = new JSONObject(line);
            if (!ev.has("event")) {
                // event/report-bridge 的上行应答行（{"ok":1} / {"ok":1,"bridge_hash_match":N}）；
                // ok=0 才留痕（daemon 拒收=协议异常，需取证）
                if (ev.optInt("ok", 0) != 1) {
                    Log.w(TAG, "上行命令被 daemon 拒绝: " + line);
                }
                return;
            }
            if (!"dex-push".equals(ev.optString("event"))) {
                Log.w(TAG, "未知事件，忽略: " + line);
                return;
            }
            int size = ev.getInt("size");
            String expected = emptyToNull(ev.optString("expected_hash", null));
            byte[] dex = l.readBytes(size); // 头行之后紧跟字节帧
            maybeSwap(dex, expected);
        } catch (Throwable t) {
            Log.e(TAG, "事件处理失败: " + t, t);
        }
    }

    // ---------------- 热切换（成功换代 / 失败回滚） ----------------

    /** 推送入口去重：目标版本与本地一致则忽略（已最新） */
    private void maybeSwap(byte[] newDex, String expectedHash) {
        if (expectedHash != null && expectedHash.equals(version)) {
            Log.i(TAG, "dex-push 版本与本地一致 (v" + version + ")，忽略");
            return;
        }
        swapTo(newDex, expectedHash);
    }

    private void swapTo(byte[] newDex, String expectedHash) {
        if (swapping || stopped) return;
        swapping = true;
        try {
            ByteBuffer buf = ByteBuffer.allocateDirect(newDex.length);
            buf.put(newDex);
            buf.rewind();
            // 父加载器 = bridgeLoader 单例（L2b 类加载拓扑：新一代看到 canonical
            // NativeBridge；bridge 未交付时退回 system CL，等价引导代降级）。
            // 两代命名空间完全隔离（兄弟 loader），唯 bridge.dex 经父链共享。
            ClassLoader loader = new InMemoryDexClassLoader(buf, LsPlantBridge.generationParent());
            Class<?> entry = loader.loadClass("ren.sunset.sundown.ProbeMain");
            // 跨 ClassLoader 只传 bootstrap 类型（String）；返回值经反射为 Boolean（bootstrap）
            Method m = entry.getMethod("hotSwap", String.class, String.class, String.class);
            Object ok = m.invoke(null, socketName, stubHash, version);
            if (Boolean.TRUE.equals(ok)) {
                Log.i(TAG, "新代接管成功，旧代退出 (v" + version + " → v" + expectedHash + ")");
                shutdown();
            } else {
                Log.e(TAG, "新代拒绝接管，旧代保留（回滚）");
            }
        } catch (Throwable t) {
            Log.e(TAG, "热切换失败，旧代保留（回滚）: " + t, t);
        } finally {
            swapping = false;
        }
    }

    /** 旧代自杀：断订阅连接（打断读循环）+ 卸 hook + 停全部线程 + 清静态引用（使旧 ClassLoader 可卸载）。
     *  v0.4.25-l3：必须停 ExemptMonitor/eventThread 并 join——否则旧代线程仍在已释放的
     *  dex 内存上 nterp 解释执行 → SIGSEGV 崩 system_server（2026-08-03 实机事故：热切换
     *  后旧代 SundownDex-Exempt 线程 new-array 崩溃，见报告文档）。 */
    synchronized void shutdown() {
        stopped = true;
        try {
            hooks.uninstall();
        } catch (Throwable t) {
            Log.w(TAG, "hook 卸载异常: " + t);
        }
        senderThread.interrupt();
        exemptMonitor.stop();
        eventThread.interrupt();
        DaemonLink l = link;
        link = null;
        if (l != null) l.close();
        // 等监视线程退出（binder 调用中 interrupt 不立即生效，3s 兜底足够）；
        // 线程全停后才允许清 active → 旧 ClassLoader/ByteBuffer 方可安全回收。
        try {
            exemptMonitor.join(3000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        synchronized (Runtime.class) {
            if (active == this) active = null;
        }
    }

    private static String emptyToNull(String s) {
        return (s == null || s.isEmpty()) ? null : s;
    }
}