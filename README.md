# 🌇 Sundown

**日落而息 · 墓碑调度** — by SunsetREN

> **当前阶段：`v0.4.52-l3`（sundownd release 57，versionCode 62）**
> L0 ✅ ｜ L1 ✅ ｜ L2 ✅ ｜ L2b ✅ ｜ L3 ✅ ｜ 单元测试 **46/46** ✅ ｜ AStop 差距矩阵 **P0/P1/P2 全阶段完成** ✅
> 发布默认观望模式（`[general] enabled=false`），冻结与超时丢弃功能待真机确认后开启。

面向 KernelSU 系的 Android 墓碑（应用冻结）调度模块。从 AStop/Cerberus 演进而来：
脱离 LSPosed 依赖，改用自有 Zygisk 探针 + KSU WebUI 管理，全链路支持热更新。

## 快速上手

```sh
# 构建 L0 daemon（仅依赖 libc，release 体积优化）
cd daemon && cargo build --release && cargo test    # 46/46

# 真机安装：KSU 管理器刷入 CI Nightly 产出的 sundown-<version>.zip（唯一分发渠道）

# 常用控制命令（sunctl，完整规范见 docs/sunctl-spec.md）
sunctl status                 # 状态总览（含超时丢弃四行观测）
sunctl events [n]             # 结构化事件（环形 256，JSON 行）
sunctl policy preset list     # 情景预设（内存切换不动磁盘）
sunctl apply-update [zip|URL] # Nightly 热更新暂存 → --activate 激活（失败自动回滚）
sunctl restart-daemon         # daemon 重启（L0 生效路径，无需软重启 zygote）
```

## 架构分层（热更新粒度）

| 层 | 资产 | 职责 | 更新方式 |
|---|---|---|---|
| L0 | `sundownd` | 调度大脑：冻结执行、策略决策、超时丢弃、socket 服务 | staged 更新，看门狗重启 |
| L1 | `libsunprobe.so` | Zygisk 探针桩：注入 system_server，socket 收发 + dex 加载 | 软重启 zygote |
| L2 | `probe.dex` | 探针逻辑：LSPlant Java hook（焦点/豁免/防御/唤醒/断网） | socket 推送，ClassLoader 热切换 |
| L2b | `libsundownhook.so` | native 伴生库：LSPlant Init/Hook 五机制出口 + mini_art_elf 符号解析 | socket 推送，引导代自热切换 |
| L3 | conf/*.toml | 策略与豁免配置（policy/action 双文件） | inotify 热加载，完全无感 |

## 核心行为全景

1. **冻结主线**：per-app 策略分级（exempt / standard / strict / IMPORTANT / force）→ grace 宽限 → cgroup v2 freezer 冻结（SIGSTOP 兜底 + OOM 锁 adj=-1000）→ 唤醒/前台解冻 → 冷却防抖。
2. **豁免链**：前台（ExemptMonitor 2s 节拍权威源）/ 媒体 / 定位 / 高网络负载（256KB/30s）/ 任何网络活动（内核侧流量） / 交互·FCM 唤醒开关 / 定时解冻窗口 / 推送子进程保留（pid 级选择性冻结）。
3. **防御体系**（对齐 AStop 防御链）：内置 critical 19 项 + 动态 system_apps 保护 / HANS·Freezer·OomAdjuster 防双冻结与防重算 / ANR 隐身 / Doze 白名单 / 候选池广播（frozen+grace+adj_keep 并集）/ Recents 任务保护（adj 锁定 + killLocked 双保险）。
4. **超时丢弃**（v0.4.52 新增，差异化行为）：冻结超时 → 整包 SIGKILL 释放内存；内存水位告急 → LRU+RSS 排序丢弃；开机缓存 → 主动回收。详见下节。
5. **观测面**：结构化事件（EvLevel 7 档 × EvAction 9 种）+ JSONL 审计落盘（2MB 滚动 ×3）+ status JSON（契约只增不改）+ WebUI 仪表盘。

## 当前版本：v0.4.52-l3 · 超时丢弃（Timeout Discard）

> 行为概念定稿于 v0.4.51-l3（README 文档），**v0.4.52-l3 已完整落地**（sundownd release 57，46/46 测试）。
> 一句话：**墓碑该做的不是"永远冻着"，而是"冻住 → 过期 → 丢弃 → 释放内存"。**

### 痛点（立项动机）

- **冻结 ≠ 释放内存**：cgroup freezer 只是暂停调度，app 的 RSS / 缓存页仍完整占用内存。
- **冻死 / 冻超时**：大量 app 被冻结后长期无人问津，可用内存被"冻结尸体"持续挤占。
- **开机高缓存不清理**：开机自动恢复/预加载的 app 无人回收，只能等系统慢慢杀。
- 主流墓碑（AStop v1.6.0 反编译实证）无完整闭环：PostCheck sync-kill 只清冻结失败残留；`Frozen>30m` 只清辅助进程不动主体；内存四件套是主动整理非丢弃；**内存水位触发与开机缓存回收完全缺失**——这是 Sundown 的差异化空白。

### 三机制（实现落点）

| 机制 | 配置（缺省） | 实现 | 落点 |
|---|---|---|---|
| ① 冻结超时丢弃 | `frozen_timeout_seconds=1800`（0=关闭） | 冻结集条目带 frozen_since；每 3s 检查，超时且期间零活跃 → SIGKILL 整 uid；"零活跃"口径 = 任何实际解冻重置超时，**节流/门控拦截的唤醒不续期**（防 FCM 风暴无限续期）；丢弃前写 `cgroup.freeze=0` 防内核残留 | `engine.rs` expired_discard_candidates + `freezer.rs` discard_uid |
| ② 内存水位丢弃 | `mem_watermark_mb=512`（0=关闭） | 每 6s 采样 `/proc/meminfo` MemAvailable；低于水位按 **LRU 最旧优先 + RSS 大优先** 排序逐个丢弃直到恢复 | `engine.rs` sort_discard_candidates + uid_rss_kb |
| ③ 开机缓存回收 | `boot_reclaim=true` + `boot_reclaim_delay_seconds=120` | 轮询 `sys.boot_completed`，延迟后**一次执行**；候选 = 上次会话冻结集 ∪ 当前冻结集中 `oom_score_adj ≥ 900` 的 cache 档包（启动时 persisted 快照注入候选池） | `engine.rs` boot_reclaim_execute + `main.rs` 快照注入 |

### 安全护栏（铁律延续）

- **丢弃只作用于 Sundown 自己的冻结集/候选池**；白名单 / per-app exempt / IMPORTANT / critical 内置 / 动态 system_apps / VPN 硬豁免 / 当前前台一律不参与（`discard_ineligible` 复用 should_never_freeze 判定面）。
- 丢弃前 `unfreeze_uid`（cgroup 写 0 + SIGCONT + 还原 OOM adj）+ `uid_pids` cgroup.procs **归属核验**防 pid 复用误杀；核验不过 → 放弃丢弃 + 留痕（失败安全）。
- **丢弃 = 终态不可撤销**；用户切回走冷启动（仅针对 30min 无活跃的 app，代价可接受）。
- 包表未知 / 无进程 / 竞态 → 清理记录不计数不 panic；全部默认开启但受 `[general] enabled` 总开关约束（观望模式零动作）。

### 观测面（契约只增不改）

- 事件：`discard pkg=P uid=N reason=frozen_timeout|mem_watermark|boot_reclaim`（EvAction::Discard）+ JSONL 审计落盘
- status 新增：`discard_ops` / `discard_reasons`（嵌套三 reason 计数）/ `discarded_packages`（最近 20）/ `discard_timeout_s`
- sunctl status 文本面新增超时丢弃观测行

### 版本同步

module.prop `v0.4.52-l3`/versionCode=62 ＝ paths.rs `0.4.52-l3`/RELEASE_NO=57 ＝ sunctl `VERSION="0.4.52-l3"`；CI 防呆校验三处一致才打包。

## 版本时间线

| 阶段 | 版本 | 关键里程碑 |
|---|---|---|
| L0 | v0.1.0-l0 → v0.1.1-l0 | 命名规范定稿；模块骨架改名（AStop/Cerberus → Sundown）；sunctl + WebUI L0；sundownd Rust 最小实现；CI + Nightly 滚动 Release；daemon 真机冒烟（uptime 7.8h+） |
| L1 | v0.2.0-l1 → v0.2.2-l1 | libsunprobe.so 桩工程化（hello-probe hash 握手 + dex 加载桥）；真机注入实证 + hash 四位一体闭环；abstract socket 同秒握手根治 /data/adb DAC EACCES |
| L2 | v0.3.0-l2 → v0.3.2-l2 | probe.dex 工程化（ProbeMain 契约 + DaemonLink 帧纪律 + Runtime 代际 + LSPlant 降级桥）；hello-dex/fetch-dex/push-dex 协议 + magic-mount 冷启动兜底；L2b bridge C++ 五机制出口 + FocusHooks/WakeupHooks 真实 hook + EventQueue 非阻塞上行 |
| L3 策略底座 | v0.4.0-l3 → v0.4.7-l3 | 策略引擎渐进交付；cgroup freezer 冻结执行链；L2b 真机回归（四位一体 hash 全绿） |
| L3 策略能力 | v0.4.8-l3 → v0.4.23-l3 | per-app 分级；结构化事件缓冲；冻结链路 7 链路实测；焦点去抖（ExemptMonitor 权威源）；情景预设（action.toml）；conf 模板首次部署；keep_high_network / keep_wakeup / unfreeze_window / push_policy 子进程管理；eBPF 流量第三源 + keep_location；staged 热更新命令；eBPF 缓冲区缺陷修复（256B 清零缓冲）；VPN 硬豁免 + 残留冻结三路兜底；keep_network 任何网络活动豁免 + 冻结中网络唤醒 |
| L3 P0 防御 | v0.4.24-l3 → v0.4.27-l3 | 防御五件套：critical 名单 + ANR 隐身 + 系统 freezer 防双冻结 + Activity 保护；热切换换代停旧代线程（修 SIGSEGV）；ColorOS HANS/Freezer 适配；frozen-sync 冻结集归属协议 |
| L3 稳定性 | v0.4.28-l3 → v0.4.35-l3 | netd pin 文件解析源修复；对账解冻归属 + 冻结集持久化 + 网络源启动自检；换代风暴熔断 + 自愈换代禁用；SetClassStatus 概率性地雷根治（class.cxx 四重载判空 + waitForBootCompleted 双保险）；tombstone_15 zygisk 早期崩溃根治（-fno-threadsafe-statics 零 PLT）；日志本地化 |
| L3 P1 收官 | v0.4.36-l3 → v0.4.40-l3 | SIGSTOP 兜底冻结 + 归属核验；OOM 保护（adj 锁 -1000）；JSONL 事件审计；OomAdjuster 防御 + 耗电豁免 + Doze 白名单；dex 版本解析步长 8→4 修复 |
| L3 P2 对齐 | v0.4.41-l3 → v0.4.45-l3 | IMPORTANT 档；唤醒节流（对齐 AStop 60s）；Receiver gate 广播门控；break_network（OplusDeepSleep 断网）；config export/import |
| L3 实机加固 | v0.4.46-l3 → v0.4.51-l3 | 选择性冻结补 OOM 保护 + tick 周期重锁；pid 级解冻彻底化 + adj_keep；candidate-sync 候选池广播；CRITICAL_PACKAGES 19 项 + 动态 system_apps；系统链路 OOM 锁定（相机黑屏根治：37 组件 + android.process.media 恒锁）；Recents 任务保护实测补丁（o-stop(40) 实锤 + killLocked 双保险，release 56） |
| **L3 超时丢弃** | **v0.4.52-l3** | **超时丢弃三机制落地（见上节）；46/46 测试；release 57** |

## 安全铁律（贯穿全生命周期）

1. **白名单/豁免优先**：任何机制不得触碰白名单、exempt、IMPORTANT、critical、系统组件、VPN、前台。
2. **失败安全**：配置解析失败保留旧表；热切换失败自动回滚；staged 激活失败回滚（20s+10s readiness 校验）。
3. **契约只增不改**：status JSON、事件语义、sunctl 命令面一旦发布只增不改（旧客户端降级兼容）。
4. **归属核验**：一切 pid/uid 操作先经 cgroup.procs 归属核验，防 pid 复用误杀。
5. **版本同步**：module.prop / paths.rs / sunctl 三处同步 + CI 防呆校验，漏改即构建失败。
6. **release_no 只增不改**：staged 更新防降级的硬依据。

## 目录结构

```
Sundown/
├── README.md               # 本文件（阶段状态唯一权威）
├── NAMING.md               # 命名规范（定稿，唯一权威副本）
├── docs/                   # sunctl-spec / l2-plan / l2b-plan / l3-plan（权威副本）
├── daemon/                 # sundownd Rust 源码（L0，仅依赖 libc）
│   └── src/                # main/paths/logging/state/sock/config/toml/policy/preset/freezer/engine/events/network
├── probe/                  # libsunprobe.so C++ 源码（L1 探针桩，NDK arm64-v8a）
├── dex/                    # probe.dex Java 源码（L2 探针逻辑层）
├── bridge/                 # libsundownhook.so C++ 源码（L2b native 伴生库）
└── module/                 # KSU 模块（目录内容即模块 zip 内容）
    ├── module.prop         # id=sundown, version=v0.4.52-l3, versionCode=62
    ├── customize.sh        # 安装脚本（环境/ABI 检查、conf 模板首次部署、ReZygisk 检测）
    ├── post-fs-data.sh     # 早期初始化 + Cerberus 旧资产迁移 + dex 同步
    ├── service.sh          # sundownd 启动 / staged 更新激活 / 看门狗
    ├── uninstall.sh        # 卸载清理
    ├── sepolicy.rule       # system_server ↔ root 域 socket 规则
    ├── system.prop         # LMKD 保后台参数
    ├── system/bin/         # sundownd（CI 注入占位）+ sunctl
    ├── conf/               # policy.toml + action.toml（L3，含 [discard] 段模板）
    ├── zygisk/ probe/ hook/ system/etc/sundown/ system/lib64/   # 【CI 生成】各层二进制与 hash
    └── webroot/            # KSU WebUI 仪表盘（index.html）
```

## 版本号策略（升级检查清单）

| 位置 | 字段 | 规则 |
|---|---|---|
| `module/module.prop` | `version` / `versionCode` | version 语义化带阶段后缀；versionCode **单调 +1**（KSU 更新感知硬要求） |
| `daemon/src/paths.rs` | `VERSION_NAME` / `RELEASE_NO` | 与 module.prop 同步（去 `v`）；RELEASE_NO 在 daemon 二进制任何变更时 +1（只增不改） |
| `module/system/bin/sunctl` | `VERSION` | 与 module.prop 同步（去 `v`） |
| zip 文件名 | `sundown-<version>.zip` | CI 从 module.prop 读取，自动跟随 |

- 分层语义：L0→`v0.1.0-l0`，L1→`v0.2.0-l1`，L2→`v0.3.0-l2`，L3→`v0.4.0-l3`；正式版从 `v1.0.0` 起去阶段后缀
- 阶段内修复/迭代走 patch 位；跨阶段才动次版本号
- CI 防呆校验：三处版本不一致则构建失败；Nightly 渠道永远只保留最新一个 zip

## 构建与测试

```sh
cd daemon
cargo test          # 46/46（42 基线 + discard_parse/frozen_timeout/mem_watermark/boot_reclaim 4 项）
cargo build --release   # sundownd <version> (release_no N)，529KB，仅依赖 libc
```

- CI（GitHub Actions）：push 即构建全部层 → 打包模块 zip → Nightly 滚动 Release 自动发布 `sundown-<version>.zip`
- 本地 `daemon/target/release/sundownd` 为本地构建产物；模块内 `system/bin/sundownd` 为占位符，CI 打包时注入真实产物

## 待办（后置）

- [ ] v0.4.52-l3 刷入设备 + `enable=true` 实机验证：冻结/解冻正向链路 + frozen.state 持久化对账 + keep_network 场景 + 相机/Recents 保护实测 + **超时丢弃三机制观测**（sunctl status 四行指标 + 事件 JSONL 核对）
- [ ] 冷启动压测 ≥10 轮无崩溃（当前 11 轮 ✅，续测）
- [ ] 定位活动豁免 dex 侧 AppOps 判定（ExemptMonitor.java 扩展 loc 字段上报，走 L2 热更新路线）
- [ ] 完整热更新闭环打磨（WebUI 检测新版 → 下载 → staged → 重启激活 → 回滚的端到端体验）
- [ ] WebUI 日志页交互打磨（v3 数据面就绪后的分组/时间线视图）+ 超时丢弃只读展示（后置）
- [ ] AStop 小米 8 类名清单 → 预置探测清单；能力探测清单 + hook 命中矩阵导出
- [ ] 日志子系统优化（唤醒事件聚合 / pkg=? 降噪 / SIGTERM 优雅解冻）
- [ ] P3 立项评估（暂缓）：自启动管控子系统 / 内存四件套 / 自带 eBPF 程序（多用户已定不做）

## 依赖

- KernelSU 系 root（原版 / Next / SukiSU 等分支均兼容）
- ReZygisk（L1 探针提供方，L0 阶段可选；推荐 v1.0.0+）
- Android 11 (API 30)+

## 参考资产

工作区 `zygisk-research/` 内有完整调研包：Zygisk API v4 头文件、
5ec1cff/PShocker 模块模板、Magisk 原生加载链源码、ReZygisk 源码（含 webroot 参考实现）。
旧实现参考：`AStopV1.7/`（Cerberus daemon + 全套模块脚本，本骨架由其改名演进）。
工作区另有各版本实机验证报告、AStop 差距矩阵/防御体系/网络机制分析、HANS 误伤事故与冻结集归属设计等报告（`.md` 根目录）。
