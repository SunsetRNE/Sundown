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
    └── hook/
        ├── HookEngine.java   hook 编排接口（install/uninstall 生命周期）
        └── LsPlantBridge.java LSPlant 软接入降级桥（native 未接入时 noop，不阻塞闭环）
```

构建产物（`dex/build/`，git 忽略）：`probe.dex`（约 12K）+ `probe.dex.hash`。

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
