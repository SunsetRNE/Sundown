# dex/ — L2 探针 dex（probe.dex）

L2 是 Sundown 四层热更新架构的**策略执行层前身**：运行在 `system_server` 进程内，
由 L1 探针桩（libsunprobe.so）加载，承载 LSPlant hook 逻辑（本阶段为接入骨架）。
设计裁决与完整计划见 [docs/l2-plan.md](../docs/l2-plan.md)。

## 目录结构

```
dex/
└── src/ren/sunset/sundown/
    ├── BuildInfo.java        编译期构建信息（@DEX_BUILD_VERSION@ 占位符，CI 注入 commit sha）
    ├── ProbeMain.java        L2 契约入口：init() / hotSwap()（与 L1 桩的加载桥约定）
    ├── DaemonLink.java       daemon 通道客户端（abstract LocalSocket + 行协议/字节帧纪律）
    ├── Runtime.java          代际模型（Generation）+ 热切换编排 + 断线 2s 重连自愈
    ├── EventQueue.java       【L2b】hook 事件有界缓冲（回调非阻塞，发送线程串行 drain）
    └── hook/
        ├── NativeBridge.java 【L2b】伴生库 Java 契约面（canonical 类加载纪律见类注释）
        ├── LsPlantBridge.java LSPlant 引擎装配（bridge.dex 父链 + 伴生库加载 + 降级 no-op）
        ├── FocusHooks.java   【L2b】焦点/进程生命周期 hook 组（观测模式）
        ├── WakeupHooks.java  【L2b】唤醒入口 hook 组（观测模式）
        └── HookEngine.java   hook 编排接口（install/uninstall 生命周期）
```

构建产物（`dex/build/`，git 忽略）：
- `probe.dex`（全量源码，含 NativeBridge 死代码副本）+ `probe.dex.hash`
- `bridge.dex`（仅 NativeBridge，canonical 副本）——L2b 类加载拓扑的 native 唯一绑定点

> **语法红线：本工程禁用 lambda / 方法引用。**
> 编译走 `javac -source 8 -target 8 -bootclasspath android.jar`，lambda 的
> invokedynamic 需要在 bootclasspath 解析 `LambdaMetafactory.metafactory`，
> 而 android.jar 无此符号（javac 直接 fatal，CI #18 已踩坑）。一律写匿名内部类。

## DAC 铁律与字节通道（本层存在的理由）

L1 真机实证：`/data/adb` 为 `drwx------ root root`，`system_server`（uid 1000）
**在 DAC 层即 EACCES**（无 avc，不走 SELinux）。因此 dex 层**不能**从
`/data/adb/sundown/probe/probe.dex` 读文件，热更新字节通道为：

```
模块资产 module/probe/probe.dex
  → post-fs-data.sh 同步 → /data/adb/sundown/probe/probe.dex（0600, root 专属）
  → daemon(root) 读字节 → abstract socket「sundown_probe」下发
  → dex 层 InMemoryDexClassLoader(directByteBuffer, systemClassLoader) 加载
```

冷启动/断线自愈兜底路径 = 模块 magic-mount 的 **`/system/etc/sundown/probe.dex`**
（uid 1000 可读、SELinux `system_file` 无争议）；hello 应答的 `dex_path` 一律指向它。

## socket 协议（L2 新增三命令，abstract 通道行协议）

| 命令 | 方向 | 说明 |
|---|---|---|
| `hello-dex <version>` | dex → daemon | 上报构建版本；应答含 `dex_hash_match`/`expected_dex_hash`/`dex_path`/`dex_present`。应答后连接**转长连接订阅通道**（daemon 推送 `dex-push` 帧），EOF 注销 |
| `fetch-dex` | dex → daemon | 短连接拉取字节：应答头行 JSON（`size`/`expected_dex_hash`）+ 紧跟 `size` 字节原始 dex |
| `push-dex` | sunctl → daemon | **仅 root 管理面**（文件 socket）。daemon 读磁盘字节广播给全部订阅者；无订阅者 `notified:0` 不算失败（dex 下次上线自愈） |

`dex_hash_match` 三态：`1`=匹配；`0`=不匹配（dex 侧据此主动 `fetch-dex` 自愈）；`-1`=无期望值。

## 热切换时序（失败回滚铁律）

1. 旧代收到 `dex-push` 字节（或 hello 应答 `hash_match=0` 主动 `fetch-dex`）
2. 校验 `expected_hash ≠ 自身版本`（相同则忽略，避免无意义换代）
3. `InMemoryDexClassLoader` 加载新 `ProbeMain`，反射调 `hotSwap`
   （**跨 ClassLoader 只传 bootstrap 类型 `String`**，禁传自定义类）
4. 新代自建连接 + 事件线程 + hook 安装，**全部成功**返回 `true`
5. 旧代 `shutdown`：断连 / 卸 hook / 清静态引用（旧 ClassLoader 可被 GC 卸载）
6. **任一步失败 → 旧代原样保留 = 回滚**

daemon 重启/断线：dex 侧每 2s 重连重握手（对齐 L1 桩哲学）。

## 上行协议（L2b 新增，hello-dex 订阅连接上 dex→daemon，只增不改）

| 命令 | 说明 | 应答 |
|---|---|---|
| `report-bridge <hash>` | 伴生库（libsundownhook）build hash 上报（hello-dex 后紧随） | `{"ok":1,"bridge_hash_match":1\|0\|-1}` |
| `event focus pkg=<pkg>` | 前台焦点切换（AMS#updateActivityUsageStats 实证点位） | `{"ok":1}` |
| `event exempt pkg=<pkg> fg=0\|1 media=0\|1` | 【L3】豁免判定监视器上行（独立线程 2s 节拍：前台服务 / 媒体播放，判定变化才发；daemon 决策消费） | `{"ok":1}` |
| `event wakeup pkg=<pkg> reason=<broadcast\|service\|pendingintent>` | 唤醒入口命中 | `{"ok":1}` |
| `event proc-add pid=<n> pkg=<pkg> [uid=<n>] / proc-remove pid=<n> / force-stop pkg=<pkg>` | 进程生命周期（L3 进程表接入点；proc-add 附 pkg/uid，缺失时 daemon 从 /proc/<pid>/status 兜底） | `{"ok":1}` |

- 未知 event 子类型容错 `{"ok":1,"ignored":1}`（新旧版本滚动不炸）
- 事件由 hook 回调经 `EventQueue` 非阻塞投递（回调可能持有 AMS 锁，绝不阻塞），
  Runtime 发送线程串行 drain；daemon 侧只记录/计数（观测模式，无动作）

## L2b 类加载拓扑（canonical NativeBridge 裁决）

`System.load` 同一路径不允许被第二个 ClassLoader 加载，而热切换每代都是新
ClassLoader——因此 native 绑定点必须收敛到**唯一 canonical 类**：

```
system CL（L1 桩冷启父链）
  └─ bridgeLoader（DexClassLoader @ /system/etc/sundown/bridge.dex，单例，
  │    寄存于 System.getProperties()——进程内全 loader 可见的存活全局表）
  │    └─ canonical NativeBridge（libsundownhook 只与这个副本绑定）
  ├─ probe.dex gen1..N（父=bridgeLoader → 父委托看到 canonical 副本）✅ 工作代
  └─ probe.dex gen0（父=system CL，桩创建）⚠️ 引导代：
       只能解析到自己的私有死代码副本 → LsPlantBridge.needsGenerationHop()
       判定后由 Runtime 自热切换到工作代（gen0 绝不 System.load）
```

- `NativeBridge.ensureLoaded()` 只许在 canonical 副本上执行（身份一致性由
  ClassLoader 比较保证）；probe.dex 中的 NativeBridge 副本是死代码
- 伴生库本体（`system/lib64/libsundownhook.so` + `liblsplant.so`）随 magic-mount
  出现，uid 1000 可读；更新 = 软重启（与 L1 同级成本，机制面因此不做策略）
- bridge build hash 闭环：`report-bridge` 上报 = 模块 `hook/hook.hash` =
  CI commit = git HEAD（status 观测 `probe_hook_bridge_hash*`）

## 版本闭环（四位一体）

```
dex 上报版本（BuildInfo.DEX_BUILD_VERSION）
  = 模块内 probe.dex.hash（daemon 期望值）
  = CI 构建 commit short sha（${GITHUB_SHA::7}，build-dex job sed 注入）
  = git HEAD
```

观测面：`sunctl status` 的 `probe_dex_version` / `probe_dex_hash_match`，
WebUI「L2 探针 dex」状态行，logcat tag `SundownDex`。

## 本地构建验证

```bash
# 依赖：JDK 17 + Android SDK（platforms/android-3x + build-tools/d8）
sed "s/@DEX_BUILD_VERSION@/localdev/" dex/src/ren/sunset/sundown/BuildInfo.java > /tmp/BuildInfo.java
mkdir -p dex/build/classes
javac -source 8 -target 8 \
  -bootclasspath "$ANDROID_HOME/platforms/android-36/android.jar" \
  -d dex/build/classes $(find dex/src -name '*.java')
mkdir -p dex/build/out   # d8 要求 --output 为已存在目录
find dex/build/classes -name '*.class' | xargs \
  "$ANDROID_HOME/build-tools/34.0.0/d8" --min-api 30 --output dex/build/out
mv dex/build/out/classes.dex dex/build/probe.dex
```

CI 对应 job：`.github/workflows/build.yml` 的 `build-dex`（产物进模块 zip 的
`module/probe/` 与 `module/system/etc/sundown/probe.dex`）。

## 本阶段不做（L2b / 后续）

- LSPlant native 集成与真实 hook（AMS 焦点 / Binder 豁免）——需真机验证 SELinux `execmod`
- 冻结策略执行（L3 策略引擎）
- 桩 probe.cpp 任何改动（本阶段零触碰 L1）
