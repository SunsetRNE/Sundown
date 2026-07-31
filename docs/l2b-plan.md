# L2b 推进计划：LSPlant native 集成 + 焦点/唤醒感知真实 hook

> 状态：🚧 进行中 ｜ 目标版本：v0.3.2-l2（L2 阶段内 patch 位，L3 仍为 v0.4.0-l3）｜ 决策：SunsetREN
> 前置：L0 ✅ L1 ✅ L2 ✅（v0.3.1-l2 真机回归通过：四位一体闭环锚定 git HEAD f5eb163，
> reload-probe 管理面实测 notified=1 + dex 同版本拒换代，全链路数据自洽）
> 本刀定位：**机制先行**——LSPlant 真实 hook 机制落地 + 焦点/唤醒入口感知 + dex→daemon
> 事件上行协议。所有 hook 一律**观测模式**（logcat 留痕 + event 上报 daemon），
> 冻结执行、豁免动作、拦截策略、厂商适配矩阵一律留 L3/L3+。

---

## 0. 三个关键裁决（调研实证，证据见 §附）

### 0.1 LSPlant 集成形态：LGPL 动态链接 + vendor Java 桥

- **LSPlant 6.4**（Maven `org.lsposed.lsplant`，latest 定格 2024-04-18），许可证 **LGPL-3.0**
- 官方 AAR（`lsplant` 与 `lsplant-standalone` 两个 artifact 均同）**classes.jar 为空**——
  官方只发布 C++ API（`lsplant.hpp`：`Init/Hook/UnHook/IsHooked/Deoptimize/GetNativeFunction/
  MakeDexFileTrusted/MakeClassInheritable`）；Java 桥（`Hooker.java`）在官方 test 模块，
  **设计上即要求 vendor 进使用方源码**（LSPatch 同款做法）
- **合规路径 = 动态链接**：模块 zip 内置独立 `liblsplant.so`（prefab 原样搬运，CI sha256 校验），
  我们的 bridge 是另一个独立 `.so` 链接它；LSPlant 源码获取方式写入 NOTICE
- **inline hooker = Dobby**（官方 test 同款，Apache-2.0，CI pinned commit 源码静态链入 bridge）
  ——LSPlant `InitInfo` 要求调用方自带 inline hook/unhook（Init 时要 hook 若干 libart 函数）
- **art 符号解析 = 自研 mini resolver**（~150 行）：
  `/proc/self/maps` 取 libart.so 内存基址 + 读磁盘 `/apex/com.android.art/lib64/libart.so`
  解析 `.dynsym`+`.symtab`（`InitInfo` 明确要求两者都要能解）。
  不用官方 lsparself（git submodule，codeload 源码包不含）；磁盘 libart uid 1000 可读，
  LSPosed 同款路径，无新风险面

### 0.2 不动 L1 桩（铁律延续）：L2 native 伴生库 `libsundownhook.so`

- **桩零触碰**。bridge 由 **dex 层 `System.load` 加载**，不经过 Zygisk 桩：
  - 原理：从 `InMemoryDexClassLoader` 的类里调 `System.load`，`JNI_OnLoad` 上下文的
    `FindClass` 解析到**调用方 ClassLoader** → bridge 可直接对本 dex 的
    `NativeBridge` 类 `RegisterNatives`（真机验证项 V1）
- bridge **只做机制不做策略**（与桩同哲学，机制面一次设计到位、此后不再更新）：
  `lsplant::Init` + `Hook/UnHook/IsHooked` + `MakeDexFileTrusted` 五个出口 + `BUILD_HASH` 上报
- 落位 `module/system/lib64/`（magic-mount → `/system/lib64/`，uid 1000 可读，
  与 probe.dex 冷启动路径同哲学）。**不新增层号**——bridge 是 L2 的 native 伴生部分，
  NAMING.md 不动；更新成本 = 软重启（与 L1 同级）
- 加载顺序：dex 显式先 `System.load("/system/lib64/liblsplant.so")`
  再 `System.load("/system/lib64/libsundownhook.so")`（显式双 load，不依赖 DT_NEEDED 解析序）

### 0.3 hidden API 与 execmod（两大真机验证点）

- 本 dex（`InMemoryDexClassLoader`）= **untrusted**，反射 `com.android.server.*` 必被
  hidden API 拦截 → 照 LSPosed 做法接线 `MakeDexFileTrusted`：
  官方警告 `DexFile.mCookie` 字段本身是 hidden API，**jfieldID 必须在 JNI_OnLoad 预取**；
  dex 侧经 `BaseDexClassLoader.pathList → dexElements[] → dexFile` 反射拿出自己的
  DexFile 对象 → `nativeMakeDexFilesTrusted(dexFile)` → native 取 cookie 调
  `lsplant::MakeDexFileTrusted(env, cookie)`
- execmod 风险面（按序验证，avc 取证后备 sepolicy）：
  1. `System.load` `/system/lib64/*.so`（system_file，预期无争议）
  2. Dobby `mprotect` libart.so text 段为 rwx（apex 库，LSPosed 在 system_server 日常操作，
     预期可过；若见 avc → dmesg 取证 → sepolicy.rule 补规则，**此补规则属 L1 级变更**）
  3. LSPlant 运行时生成的 stub dex（`LSPHooker_`）trampoline 执行（匿名 exec 内存）

## 1. hook 点清单（AStop v1.6.0 实证萃取）

萃取方法：自研流式 dex 扫描器（`.backup/dex_scan.py`，仅解析 string/type/method/
class_data 表 + 指令长度表，内存线性，规避 jadx 全量反编译 OOM），对
`com.zeaolv.astop.lsp.*` 全部方法提取 const-string 序列并做「类名→方法名」配对，
命中均带 AStop 运行时日志串实证（如 `已 Hook AMS#addPidLocked`）。

### 第一组：焦点/进程生命周期感知（本刀装真实 hook）

| 目标类 | 方法 | 信号含义 | AStop 证据 |
|---|---|---|---|
| `ActivityManagerService` | `updateActivityUsageStats` | activity 切换（resume/pause 必经之路） | `已 Hook ActivityManagerService#updateActivityUsageStats` |
| `ActivityManagerService` | `addPidLocked` / `removePidLocked` | 进程生死 | `已 Hook AMS#addPidLocked/removePidLocked` |
| `ActivityManagerService` | `forceStopPackage` | 强杀（冻结表清理信号） | `已 Hook AMS#forceStopPackage` |
| `com.android.server.wm.Task` | `getTopNonFinishingActivity` / `getTopMostActivity` / `topRunningActivity` | 栈顶查询（焦点确认手段，按版本降级枚举） | ActivityProtectionHooksKt / TaskHooksKt |

- 焦点事件流：`updateActivityUsageStats` 触发 → 查栈顶 `ActivityRecord`
  （反射其 `packageName`/`processName`/`app.pid` 字段，字段名按版本枚举降级）
  → `event focus` 上行
- `ActivityTaskManagerService` 构造 hook（AStop `hookAtmsInit`）本刀**不抄**：
  updateActivityUsageStats 自带 AMS 实例上下文，ATMS 实例非必需

### 第二组：唤醒入口（Binder/组件豁免前置，本刀装真实 hook，观测上报）

| 目标类 | 方法 | 信号含义 |
|---|---|---|
| `BroadcastController`（A14+）/ `ActivityManagerService`（<14，降级枚举） | `broadcastIntentLocked` | 广播投递（FCM 唤醒主路径） |
| `ActiveServices` | `realStartServiceLocked` | 服务启动 |
| `PendingIntentRecord` | `sendInner` | PendingIntent 触发 |
| `ProcessReceiverRecord` | `addCurReceiver` / `removeCurReceiver` | 正在收广播（active receiver 门禁数据源） |

- 事件流：hook callback 提取目标 pkg（BroadcastRecord/ServiceRecord/PendingIntentRecord
  字段反射，字段名多版本枚举）→ `event wakeup` 上行
- **本刀不做任何放行/解冻动作**（无自冻对象），纯感知建链路

### 第三组：防御（本刀只留接入骨架 no-op，L3 启用）

- `CachedAppOptimizer`：`useFreezer`（禁系统 freezer）/ `killProcess`（拦截冻结器终止）
  ——L3 自冻上线前启用，本阶段不 hook（避免无谓改变系统行为）
- ANR 防护链：`AnrHelper.appNotResponding` / `ProcessErrorStateRecord.appNotResponding` /
  ActiveServices service 超时系列 / `StackTracesDumpHelper` dump 系列 ——随 L3 启用
- 厂商 binder 唤醒上报：MIUI `GreezeManagerService.reportBinderTrans` /
  OPPO `OplusHansManager.unfreezeForKernel` / Vivo `com.vivo.services.freezer.FreezeRecord`
  ——L3+ 厂商适配阶段（AStop 全套 ROM 矩阵已萃取存档，见 §附）

## 2. dex 改动（Java；语法红线延续：**禁 lambda/方法引用**，一律匿名内部类）

```
hook/
├── NativeBridge.java     # 【新增】vendor 官方 Hooker 模式：
│                         #   native doHook(Member, Method)→Method(backup)
│                         #   native doUnhook(Member)；nativeInit()；nativeGetBuildHash()
│                         #   nativeMakeDexFilesTrusted(Object dexFile)
│                         #   public static Hooker hook(Member target, Method replacement, Object owner)
│                         #   MethodCallback{backup, args} public static（replacement 签名用）
├── LsPlantBridge.java    # 【升级】探测 /system/lib64/libsundownhook.so：
│                         #   双 load + nativeInit + makeOwnDexTrusted 全过 → 真实模式
│                         #   任一失败 → 维持 no-op 降级（dev/未刷全场景不阻塞闭环）
├── FocusHooks.java       # 【新增】第一组四连 hook 安装/卸载 + 焦点事件上报
├── WakeupHooks.java      # 【新增】第二组四连 hook 安装/卸载 + 唤醒事件上报
└── HookEngine.java       # 接口不变；编排改为按组 install，单组失败不拖垮其他组（logcat 取证）
```

- 所有 hook callback 第一行为 try-catch 全包——**hook 内任何异常不得泄漏进 system_server**
- 字段反射（ActivityRecord/Task/BroadcastRecord 等）按 Android 11~15 版本枚举降级，
  单个枚举失败 → 该信号缺失降级（不致命，logcat 记录缺失项）
- 事件上报走既有 hello-dex 订阅长连接（§3 协议扩展），logcat 同步留痕（tag SundownDex）

## 3. daemon 改动（Rust，RELEASE_NO 4→5）

协议扩展（**只增不改**；abstract 面，hello-dex 订阅连接上新增上行命令）：

| 命令 | 说明 | 应答 |
|---|---|---|
| `event focus pkg=<pkg> pid=<n> uid=<n>` | 前台焦点切换 | `{"ok":1}` |
| `event wakeup pkg=<pkg> reason=<broadcast\|service\|pendingintent> pid=<n>` | 唤醒入口命中 | `{"ok":1}` |
| `event proc-add pid=<n> uid=<n> pkg=<pkg>` / `event proc-remove pid=<n>` / `event force-stop pkg=<pkg>` | 进程生命周期 | `{"ok":1}` |
| `report-bridge <hash>` | bridge BUILD_HASH 上报（hello-dex 后紧随） | `{"ok":1,"bridge_hash_match":1\|0\|-1}` |

- 未知 event 子类型容错 `{"ok":1,"ignored":1}`（新旧版本滚动不炸）
- `paths.rs`：新增 `PROBE_EXPECTED_HOOK_HASH_FILE=/data/adb/modules/sundown/hook/hook.hash`
  （magic-mount 只挂 system/ 子树，`hook/` 不入 /system，与 zygisk/、probe/ 同惯例）
- `state.rs`：`hook_bridge`（hash+上报时刻）、`last_focus_pkg`、`focus_changes`、
  `wakeup_events` 计数；`config.rs` reload 同步重读期望 hook hash
- status JSON 新增（契约只增不改）：`probe_hook_bridge_hash` /
  `probe_hook_bridge_hash_match`（三态）/ `focus_pkg` / `focus_changes` / `wakeup_events`

## 4. 模块 / 脚本 / CI

- `module/system/lib64/`：**CI 注入** `liblsplant.so`（prefab 原样）+ `libsundownhook.so`；
  `module/hook/hook.hash`：CI 写入（= commit short sha，四位一体同源）
- `.gitignore`：+ `module/system/lib64/`、`module/hook/`（CI 注入物不入库，同 zygisk/ 惯例）
- `sunctl`：status 文本面 L2b 行（bridge hash 闭环 + 焦点包名 + 唤醒计数）；VERSION 同步
- `webroot/index.html`：L2b 状态行真实化
- CI `build-bridge` job：NDK + CMake；
  拉取 Dobby pinned commit 源码 + `lsplant-6.4.aar`（sha256 校验后取 prefab）；
  `-static-libstdc++`、`MinSizeRel`、strip；产物 + `hook.hash` 入 artifact
- `package-module`：`needs` 加 build-bridge；三处版本防呆校验不变
- `sepolicy.rule`：**本阶段不加**；execmod 验证出 avc 再补（补规则 = L1 级变更，需明示）
- 版本：`module.prop` `v0.3.2-l2` / `versionCode=7` / description 换 L2b 文案；
  daemon `RELEASE_NO=5`

## 5. 真机验证清单（按序执行）

- V1 bridge `System.load` 成功 + `nativeGetBuildHash` 上报（logcat / status `probe_hook_bridge_hash`）
- V2 `lsplant::Init` 成功（Dobby mprotect libart text → `dmesg | grep avc` 取证）
- V3 hidden API：dex 反射 AMS 方法成功（MakeDexFileTrusted 生效；失败先见 NoSuchMethod/反射异常）
- V4 焦点 hook：切换前台 App → logcat + `status focus_pkg` 跟随变化
- V5 唤醒 hook：`am broadcast` / `am startservice` 测试包 → wakeup 事件上行（daemon 日志 + 计数）
- V6 版本闭环：bridge hash = 模块 hook.hash = CI commit = git HEAD
- V7 热切换兼容：`reload-probe` 换代 → 新代重装 hook 成功；旧代 `uninstall()` 全部
  `UnHook`（不卸则旧 ClassLoader 泄漏 + 旧逻辑残留，必须验证）
- V8 长稳：daemon + hook 运行 ≥2h 无 system_server 崩溃/ANR 弹窗/logcat 异常风暴

## 6. 明确不做（留刀清单）

- ❌ 冻结执行与豁免动作（L3 策略引擎）
- ❌ `CachedAppOptimizer` 拦截 / ANR 防护链启用（L3 前置，本阶段骨架都不装，只存档点位）
- ❌ 厂商 ROM 适配矩阵（MIUI Greeze / OPPO Hans / Vivo Freezer——L3+）
- ❌ L1 桩（probe.cpp）任何改动
- ❌ LSPlant 静态链接 / 源码编译（LGPL 合规复杂度 + C++23 modules 对 CI 工具链要求过高）
- ❌ WebUI 交互重构（仅状态行追加）

---

## §附：调研证据存档

| 证据 | 位置 |
|---|---|
| AStop hook 点萃取原始数据（全量 JSON） | `/tmp/astop_x/hooks.json`（真机临时，建议归档 `.backup/`） |
| dex 流式扫描器 | `.backup/dex_scan.py` |
| LSPlant 6.4 AAR（prefab 源） | `.backup/lsplant-6.4.aar` / `lsplant-standalone-6.4.aar` |
| LSPlant 源码包（Hooker.java vendor 源 + lsplant.hpp） | `.backup/LSPlant-master.zip` |
| LSPlant maven 版本元数据 | `.backup/lsplant-maven-metadata.xml` |
| AStop xposed 入口 | `assets/xposed_init` = `com.zeaolv.astop.lsp.ProbeHook` |
