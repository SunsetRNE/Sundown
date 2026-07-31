# L2 推进计划：probe.dex 工程化 + 热切换闭环（第一阶段）

> 状态：✅ 已完成（v0.3.0-l2，工程闭环；LSPlant 真实 hook 留 L2b）｜ 目标版本：v0.3.0-l2 ｜ 决策：SunsetREN
> 真机回归：v0.3.0-l2 首验发现桩 dex 加载桥断裂（GetStaticMethodID 拿错归属类
> `java/lang/Class`→应为 `java/lang/ClassLoader`；optimizedDirectory 显式指向 DAC 死路径
> 改传 nullptr；FALLBACK_DEX 对齐 magic-mount）→ **v0.3.1-l2 patch 修复**（桩三处 +
> sunctl/WebUI L2 状态行文案按「桩一次性加载」如实化；RELEASE_NO 保持 4，daemon 逻辑未变）。
> 详见 probe/README.md「dex 加载桥 JNI 备忘」。
> 前置：L0 ✅ L1 ✅（真机验证通过）。本计划为 L2 第一刀：先立「dex 工程闭环 + 热切换」，
> LSPlant hook 逻辑作为接入骨架留下一刀（类比 L1「先桩工程化、再真机验证」的节奏）。

## 0. 关键设计裁决：dex 下发通道（DAC 坑前置排除）

L1 真机已实证：`/data/adb` 为 `drwx------ root root`，system_server(uid 1000) 在 **DAC 层**
即 EACCES（无 avc）。由此推出 **L2 的铁律**：

- ❌ 桩/dex 层（uid 1000）**不可能直接读** `/data/adb/sundown/probe/probe.dex`
  （L1 桩当前的文件路径加载桥在真机上必然失败——L1 验证时 dex 缺失所以没踩到）
- ✅ 定稿通道 = NAMING.md 既定方案：**daemon（root）读文件 → abstract socket 下发字节 →
  dex 层 `InMemoryDexClassLoader` 加载**。字节走 socket，全程无文件系统耦合，无 DAC/SELinux/oat 问题
- 冷启动兜底：daemon hello-probe 应答的 `dex_path` 指向**模块 magic-mount 路径**
  `/system/etc/sundown/probe.dex`（zip 内置于 `module/system/etc/sundown/`，随挂载出现，
  全局可读、SELinux `system_file` 无争议）。桩文件加载桥保留为冷启动路径，不动桩（遵守 L1 红线）
- 热切换路径 100% 走 socket 字节（`InMemoryDexClassLoader(ByteBuffer)`），与文件完全解耦

## 1. socket 协议扩展（行协议，向后兼容只增不改）

| 命令 | 方向 | 说明 |
|---|---|---|
| `hello-dex <version>` | abstract 面 | dex 上报构建版本（=CI git short sha）；应答一行 JSON 后**连接保持**为事件订阅通道；daemon 记录 → status `probe_dex_version` 填真实值 |
| `fetch-dex` | abstract 面 | 拉取 dex 字节：应答头行 `{"ok":1,"size":N,"expected_hash":"..."}` **紧跟 N 字节原始 dex**；独立短连接，用完即关（不占订阅通道） |
| `push-dex [path]` | **仅 root 管理面**（文件 socket） | 读文件 → 向所有 hello-dex 订阅连接写事件头行 `{"event":"dex-push","size":N,"expected_hash":"..."}` + N 字节；应答 `{"ok":1,"notified":N,...}`；无订阅者 `notified:0`（不算失败，冷启动自愈） |

- hello-dex 应答：`{"ok":1,"dex_hash_match":1|0|-1,"expected_dex_hash":"...","dex_path":"..."}`
  - `dex_hash_match=0`（本地版本 ≠ 模块期望）→ dex 主动 `fetch-dex` **自愈热切换**
- 订阅连接上 dex 侧只收事件；再发命令仅支持 `ping` / 重复 `hello-dex`（重登记）
- dex 事件循环遇到 EOF（daemon 重启/崩溃）→ 每 2s 重连重握手（对齐桩的重试哲学）

## 2. dex/ 工程（Java，无 Gradle，CI javac + d8）

```
dex/
├── README.md                    # 本层定位 / 协议 / 热切换时序 / 构建 / LSPlant 接入点
└── src/ren/sunset/sundown/
    ├── BuildInfo.java           # DEX_BUILD_VERSION = "@DEX_BUILD_VERSION@"（CI sed 注入 git sha）
    ├── ProbeMain.java           # L2 契约入口：
    │                            #   init(String socketName, String stubBuildHash)     ← L1 桩冷启动调用（签名不变）
    │                            #   hotSwap(String socketName, String stubHash, String prevVersion) ← 旧代 dex 反射调用
    ├── DaemonLink.java          # abstract LocalSocket 客户端：hello-dex / 事件循环 / fetch-dex / 断线重连
    ├── Runtime.java             # 单代运行态：DaemonLink + HookEngine + 热切换编排（成功换代 / 失败回滚）
    └── hook/
        ├── HookEngine.java      # hook 编排接口：install() / uninstall()
        └── LsPlantBridge.java   # LSPlant 软接入点：反射探测 org.lsposed.lsplant.LSPlant，
                                 #   缺失降级 no-op（本阶段 hook 层空转，L2b 填充真实 hook）
```

热切换时序（失败回滚铁律）：
1. 旧代收到 `dex-push` 字节（或 hello 应答 mismatch 后 fetch）→ 校验 `expected_hash` ≠ 自身版本
2. `InMemoryDexClassLoader(directByteBuffer, systemClassLoader)` 加载新 `ProbeMain`
3. 反射调新代 `hotSwap(...)`（**跨 ClassLoader 只传 bootstrap 类型 String**）
4. 新代自建 daemon 连接 + 事件线程 + hook 安装，全部成功 → 返回 true → **旧代 shutdown**
   （断连/卸 hook/清静态引用 → 旧 ClassLoader 可被 GC 卸载）
5. 任一步失败 → 抛异常返回 false → **旧代原样保留（回滚）**，logcat 取证

## 3. daemon 改动（Rust，RELEASE_NO 3→4）

- `paths.rs`：`VERSION_NAME="0.3.0-l2"`；新增 `PROBE_EXPECTED_DEX_HASH_FILE=/data/adb/modules/sundown/probe/probe.dex.hash`
- `state.rs`：`DexReport`（version+上报时刻）、`expected_dex_hash`、订阅者注册表
  `dex_clients: Mutex<Vec<(u64, UnixStream)>>`（id 分配/注销/写失败剔除）；
  `status_json`：`probe_dex_version` 填真实值 + 新增 `probe_dex_hash_match`（三态，契约只增不改）
- `sock.rs`：`handle_conn` 增加 `mgmt: bool`（文件 socket=true / abstract=false）；
  `hello-dex` 应答后转订阅读循环（EOF 注销）；`fetch-dex` 头行+字节帧；
  `push-dex` 仅 mgmt 面可用（管理动作收敛单一入口）
- `config.rs`：reload 时同步重读期望 dex hash

## 4. 模块与脚本

- `module.prop`：`version=v0.3.0-l2`、`versionCode=5`、description 换 L2 文案
- `module/system/bin/sunctl`：`VERSION` 同步；**`reload-probe` 落地**（发 `push-dex`，解析
  ok/notified/error，退出码 0/1，告别 3）；status 文本面 L2 行显示真实版本与 hash 闭环状态
- `module/system/etc/sundown/probe.dex`：**CI 注入**（magic-mount 冷启动兜底路径，见 §0）
- `module/probe/probe.dex` + `probe.dex.hash`：**CI 注入**（期望版本闭环 + post-fs-data 同步源）
- `post-fs-data.sh`：新增 dex 同步块：模块 `probe/probe.dex` → `/data/adb/sundown/probe/probe.dex`
  （按 `probe.dex.hash` vs `.deployed_dex_hash` 比对，不一致才覆盖，省无谓 dexopt）
- `webroot/index.html`：L2 状态行文案真实化 + 「🔁 热更新探针」按钮（走 `sunctl reload-probe`）
- `.gitignore`：+ `dex/build/`、`module/probe/`（CI 注入物不入库，同 zygisk/ 惯例）

## 5. CI（build.yml）

- 新增 `build-dex` job：定位 runner 自带 SDK（platform 取最高版本兜底）→ sed 注入
  `DEX_BUILD_VERSION=${GITHUB_SHA::7}` → `javac -source 8 -target 8 -cp android.jar` →
  `jar` → `d8 --min-api 30` → 产出 `probe.dex` + `probe.dex.hash`（=commit sha，与桩 hash 同源）
- `package-module`：`needs` 加 build-dex；下载 artifact 布置 `module/probe/` +
  `module/system/etc/sundown/probe.dex`；三处版本防呆校验不变

## 6. 文档同步

- 本文件（docs/l2-plan.md）为 L2 阶段权威计划
- `docs/sunctl-spec.md`：reload-probe 退出码 0/1、socket 命令面 +hello-dex/fetch-dex/push-dex、
  status JSON 新字段说明
- `probe/README.md`：L2 契约补 hotSwap + dex 字节下发通道（DAC 裁决写明）
- 主 `README.md`：目录结构 +dex/、L2 状态勾选（第一阶段闭环，真机验证待办）
- `dex/README.md`：新建

## 7. 验证与推送

- 本地：`cargo check`（daemon 全量）+ javac/d8 全链路本地编译 probe.dex（SDK 已在手）
- CI：push 后三 job 绿 + Nightly zip 含 probe.dex
- 版本闭环预期（真机下一步验证）：dex 上报版本 = 模块 probe.dex.hash = CI commit = git HEAD
- 推送：`git push origin main`（SSH 走 ssh.github.com:443，密钥已就位，全程不读密钥内容）

## 8. 明确不做（留下一刀）

- ❌ LSPlant native 集成与真实 hook（AMS 焦点/Binder 豁免）——L2b，需真机验证 SELinux execmod
- ❌ 冻结策略执行——L3 策略引擎之后
- ❌ 桩 probe.cpp 任何改动——本阶段零触碰 L1
