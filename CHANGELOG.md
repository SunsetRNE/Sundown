# Sundown 版本记录（CHANGELOG）
> 版本策略见 `README.md`「版本号策略」：L0→v0.1.0-l0 … L3→v0.4.0-l3；阶段内修复/迭代走 patch 位。
> 每个版本在 GitHub Release 留档（正式版，非 prerelease）；本文件为仓库内版本档案。
> 格式：`## vX.Y.Z-lN (YYYY-MM-DD)` + 变更要点；补入路线代号（v0.6 行为层 / v0.7 架构层 / v0.8 学习层）附注。
---
## v0.4.62-l3 (2026-08-12)
**dex 侧收尾三件套（B1/B2/B4 配套 · v0.9-l3）**
### B 档 · P1 架构演进（dex 侧配套）
- **B1 DefenseHooks 迁移注册表**：13 组硬编码 hook 点（39 条）全部条目化（`def.*` id + capability 描述字段），经 `Registry.installGroup` 统一安装（类解析/回调查找/重载 hook 收敛注册表，status/env-check 可枚举）；PESR 双方法探测（appNotResponding/setNotResponding）与 ProcessList→AMS 兜底等价迁移为双条目（同回调无副作用）；删除组内私有 findClass/callback/hookAllOverloads/hookMethod 重复实现；全部条目 critical=false 保持既有"失败跳过"语义（ColorOS 专有条目在 AOSP 设备必然失败——零行为变化）
- **B2 DaemonLink 接入 subscribe 声明**：dex 建链 hello 后声明 `kinds=frozen-sync,candidate-sync,dex-push`（按需分发替代全量广播；旧 daemon 不支持 subscribe → ok=0 仅告警降级全量，零风险兼容）
- **B4 CapabilityProbe（dex 侧 ROM 能力探测）**：新增 `CapabilityProbe.java`——system_server 内类/方法存在性矩阵（19 项：AOSP 基座 13 + ColorOS 专有 6，findClass 经 ServerClasses，与注册表条目同判据）；上报通道 `capability-probe` 命令（daemon 原样存 `state.dex_capability`，事件留痕 capability_probe_dex）；`capability status` 导出 `dex_probe` 字段；sunctl `capability dex-probe` 子命令 + `env-check` ROM 探测面（类命中率摘要，daemon 离线仅提示不置失败）
### 验证
- cargo test 90/90（+0：纯协议/观测面收尾，无新决策分支）
- 版本号 v0.4.61-l3→v0.4.62-l3（release 66→67，versionCode 71→72）三处同步：paths.rs / module.prop / sunctl
- **发布流程修正（对照 GitHub 官方最新文档 2026-03-10）**：核验 repo 仅 7 个正式 Release（无 nightly tag/draft 残留，本地旧 nightly tag 引用已清）；build.yml 发布步骤确认已用 action-gh-release@v3（v3.0.2，Node 24，官方当前版本）+ contents: write 权限 + 正式 Release 语义（tag=版本号、非 prerelease、overwrite 软发布）；补充官方约束注释（commit 修改 workflow 文件时 GITHUB_TOKEN 无权建 Release 的边界）；清理 README/docs/WebUI 全部 "Nightly" 渠道残留描述（5+5+1+1+3 处），统一为正式 Release 渠道（sunctl apply-update 实际行为 v0.4.57-l3 起已查 releases/latest，文档同步）
- **发布渠道重建（2026-08-12，有意清空重发）**：清空全部 Release/tag 后重新走正式发布流程——7 个版本 tag（v0.4.56-l3~v0.4.62-l3）从本地 commit 重建推送恢复；v0.4.62-l3 Release 经 CI push 重建（tag 已存在 → action-gh-release 创建 release 并上传 asset）
- **A1 解冻预热链实机验证（2026-08-12）**：补上缺失的 `_verify/madvise_test.c` 探针（与 freezer.rs 同参数纪律：pidfd_open 434 + process_madvise 440 + 可读映射收集 + clamp 64 段/8MB + errno 判读表）；实机执行三场景（system_server/自身进程/sundownd）：**pidfd_open(434) 正常，process_madvise(440) 返回正数 0x74F000/0xF0B000 且 errno=0**（非任何标准 errno）→ 实证 ColorOS 内核 440 号被 OEM 私有 syscall 占用或未实现标准 process_madvise；与 daemon（bionic）capability 矩阵 `madvise_willneed=false` 双源一致 → **解冻预热在该内核不可用，自动降级失败安全（不阻塞解冻），行为正确**；探针判读表新增 OEM 正数返回值分支（对未来 ROM 变体验证有参考价值）
- **实机验证（2026-08-12）**：daemon 热替换 release 67 + root/模块双侧 probe.dex 同步（64512→67760→67888B，hash=c0fc580→fac8460→2cfa8c8）+ reload-probe 换代成功（dex 握手 version 三态闭环）；**B2 subscribe 声明生效**（daemon 日志 "dex 订阅更新 (id=3): kinds=frozen-sync,candidate-sync,dex-push"，重连后仍正确声明）；**B4 capability-probe 上报生效**（daemon 日志 "dex ROM 能力探测上报" + `sunctl capability dex-probe` 20 项矩阵在线 + `capability status` 内嵌 dex_probe + env-check "类命中 17/20"导出面）；**实机校准**：def.stack-dump 拆双条目（ProcessList/AMS 类均在但 dumpStackTraces 方法均不存在——ColorOS 实证 firstPids 剔除与原始代码行为等价走失败跳过，双条目对未来 ROM 变体更完整，软发布覆盖 asset）；ROM 矩阵实证 ColorOS 差异：PESR 类不存在、freezeAppAsyncLSP=true、OplusHansProxyManager.isProxyed=true、OplusStartupStrategy.isGoogleRestricInfoOn=true、OplusBgSceneManager GMS 限制=true

---
## v0.4.61-l3 (2026-08-12)

**C2 分析建议（C 档行为学习收尾刀 · 只建议不执行）**

### C 档 · P2 行为学习
- **C2 分析建议**（新增 `daemon/src/analyze.rs`，纯函数：画像快照 → 建议列表）：
  - **五类建议**：wakeup_storm（"疯狂唤醒者"识别：占比过半或 ≥10 次 + 每小时速率）、source_pattern（单源 >70% 聚类：广播风暴/服务拉起/任务闹钟）、jitter（冻结-唤醒抖动：唤醒 ≥ 冻结×3）、exempt（频繁使用却被冻结 → 豁免建议）、throttle（中频唤醒适度节流）
  - **数据充分性门槛**：<3 app 或 <10 次唤醒 → 明确提示继续采集（不输出噪音建议）
  - **铁律**：只建议不自动执行（每条建议附 rules.toml / policy.toml 手工动作指引，daemon 绝不自动改配置）；本地分析不走云端；阈值用相对量（占比/速率）保守可解释
- **导出面**：sock.rs `analyze` 命令（JSON 建议列表）；sunctl `analyze` 子命令 + usage；WebUI 画像页消费
- C1 画像 + C2 建议 = 行为学习闭环（数据→洞察→手工动作指引）

### 验证
- cargo test 90/90（原 84 + C2 新增 6 项：数据不足提示 / 风暴识别+动作指引 / 源模式聚类 / 抖动+豁免 / 中频节流 / 速率时间窗）
- 版本号 v0.4.60-l3→v0.4.61-l3（release 65→66，versionCode 70→71）三处同步：paths.rs / module.prop / sunctl
- **实机验证（2026-08-12）**：daemon 热替换 release 66；首测数据不足门槛正确拦截（1 app/23 唤醒 → 提示继续采集）；积累 12 app/108 唤醒后 analyze 输出 6 条建议——source_pattern ×4（"?" 与 systemui broadcast 100%、微信 broadcast 90%、coloros.weather service 100% 带 service_gate 指引）+ throttle ×2（systemui 8 次 / weather 6 次），JSON 转义正常，只建议未动配置（失败安全哲学实测）

---

**C1 使用画像采集（C 档行为学习第一刀 · per-app 聚合）**

### C 档 · P2 行为学习
- **C1 使用画像采集**（新增 `daemon/src/profile.rs`，零新增采集——引擎事件入口旁路聚合）：
  - per-app 画像：前台次数/前台累计时长（焦点进入→离开计时）、冻结/解冻/丢弃计数、唤醒总数 + 源分布（broadcast/service/pendingintent）、时间轴（first/last seen、last focus/freeze）
  - **铁律**：纯内存聚合（不落盘——画像为诊断输入，审计走 events.jsonl 既有通道）；失败安全无 IO 面；未知包名照常建画像
- **engine.rs 七处挂载点**：on_focus（进入 + 旧前台离开计时 + 两处解冻）、on_wakeup（入口聚合 + 解冻）、freeze_now 成功、discard_pkg 成功
- **导出面**：sock.rs `profile` 命令（`top [n]` 唤醒 TOP / `summary` 总览 / `get <pkg>` 单 app 明细）；sunctl `profile top|summary|get` 子命令 + usage
- C1 数据 → C2 分析建议输入（唤醒模式聚类 / "疯狂唤醒者"识别 / 节流与豁免建议——只建议不执行）

### 验证
- cargo test 84/84（原 79 + C1 新增 5 项：前台时长累计 / 唤醒计数+源分布 / 冻结解冻丢弃 / TOP 排序 / 事件丢失容忍）
- 版本号 v0.4.59-l3→v0.4.60-l3（release 64→65，versionCode 69→70）三处同步：paths.rs / module.prop / sunctl
- **实机验证（2026-08-12）**：daemon 热替换 release 65；`profile summary` 3 app/5 唤醒实时在线；`profile top` 排序正确（"?" 3 次 > heytap.htms / tencent.mm 各 1）；`profile get com.tencent.mm` 触发焦点切换后 focus_count=1 / focus_ms=2003ms 真实累计，且暴露真实洞察——微信切前台伴随 26 次唤醒（broadcast 21 / service 4 / pendingintent 1），C1 数据作为 C2 "疯狂唤醒者"识别依据的价值实证

---

**B4 设备能力探测矩阵（缺口补入 B 档架构层 · 观测面）**

### B 档 · P1 架构演进
- **B4 设备能力探测矩阵**（新增 `daemon/src/capability.rs`，对齐 network.rs `probe_source` 启动自检 + 缓存哲学）：
  - **freezer 层级判定**：v2 uid 级（apps/uid_<uid>/cgroup.freeze）> v2 pid 级 > v1 freezer 控制器 > none（SIGSTOP 兜底）；判定为纯函数 `classify`（可单测），探测只读零副作用（探测用 uid=10000）
  - **process_madvise(MADV_WILLNEED) 支持**：复用 freezer.rs syscall 封装（pidfd_open 434 / process_madvise 440）对自身进程实测——解冻预热前置可用性
  - **网络数据源**：engine.net.probe_source() 结果纳入矩阵（keep_network 豁免数据可用性基线）
  - **唤醒源统计基线**：EngineState 新增 `wakeup_sources`（on_wakeup 入口按 source 聚合，含 keep_wakeup=false 忽略事件——"到达 daemon 的全部唤醒"完整视图，写规则的依据面）
- **接入面**：main.rs 启动探测一次（日志 + 事件留痕 capability_probed）；sock.rs `capability status|reprobe` 命令（mgmt 面）；sunctl `capability status|reprobe` 子命令 + usage
- 铁律保持：只读探测失败安全（缺失 = None，不 panic 不阻塞）；探测结果仅观测面消费，不参与决策

### 验证
- cargo test 79/79（原 76 + B4 新增 3 项：classify 优先级判定 / FreezerLevel 序列化 / probe 失败安全契约）
- 版本号 v0.4.58-l3→v0.4.59-l3（release 63→64，versionCode 68→69）三处同步：paths.rs / module.prop / sunctl
- **实机验证 + 修复（2026-08-12）**：首测暴露**固定 uid_10000 探测缺陷**（ColorOS uid 从 10066 起，10000-10065 无目录 → freezer=none 误报）→ 修复为枚举 apps/ 下最小 uid_* 目录（143feff + d76b537，含一次编译遗留变量修复）；修复后实机矩阵：**freezer=uid_v2**（uid_10066/cgroup.freeze + pid 级路径命中）、net=pinfile、madvise_willneed=false（ColorOS 内核不支持 process_madvise，解冻预热自动降级，失败安全）、唤醒源基线统计中（broadcast/service/pendingintent）
- reprobe 实机验证：重新探测刷新矩阵 + 日志/事件留痕（capability_probed ×2）

---

**B3 声明式规则引擎 rules.toml（缺口补入 B 档架构层 · 快速应对层）**

### B 档 · P1 架构演进
- **B3 声明式规则引擎**（App 设计缺陷 → 写规则热加载，**不重新编译 dex**）：
  - 新增 `daemon/src/rules.rs`：`RuleCondition` 四态（Always/Leave/Wakeup/Focus）+ `RuleAction` 四类（Suppress/Exempt/Freeze/Discard）+ `Rule`（id/priority/applies_to/condition/source/throttle/after_seconds/expires_at）+ `RuleTable`（find_index 按优先级降序 + 同优先级按定义顺序；peek 只读观测 / hit 命中更新节流；`load()` 缺失/解析失败保留旧表——失败安全与 policy 同构）
  - action **必填**：未声明或未知值整条剔除（保底 Suppress 不可静默生效，action_set 内部标记区分）
  - 配置：`module/conf/rules.toml`（发布默认空表 + 全键注释模板）；路径 `/data/adb/sundown/conf/rules.toml`（paths.rs RULES_FILE）
  - **engine.rs 五处插入点**：① reload_policy 热加载（成功替换 + 事件留痕 rules_reloaded，缺失保留旧表）；② should_never_freeze 规则 exempt 判定（peek 只读，热加载后已冻结/grace 中立即解冻）；③ decide_leave 规则 exempt/freeze（冷却之后、force 之前）；④ on_wakeup 规则 suppress（三源门控之前，不解冻不取消 grace + 事件留痕 rule_suppressed）；⑤ tick 规则 discard（每 10 tick ≈3s，after_seconds 缺省 = 全局 frozen_timeout，安全护栏复用 discard_ineligible）
  - 优先级链：critical > 系统组件 > 白名单/VPN > 豁免链 > 冷却 > **规则引擎** > force > grace
- **管理面**：sock.rs `rules` 命令（`rules list` 规则 id 稳定排序 / `rules status` 规则数 + revision + 累计命中 hits）；sunctl `rules list|status` 子命令 + usage 补录
- **配置迁移同步**：sunctl config export/import 白名单与 META 校验纳入 rules.toml（换机迁移覆盖规则文件）

### 验证
- cargo test 76/76（原 65 + B3 新增 11 项：解析 2 / 缺 action 1 / 匹配 3 / priority 1 / throttle 1 / expires 1 / date 1 / 通配 1——修复 3 项测试自身断言/数据问题：未知 action 剔除语义、同优先级定义顺序、throttle 通配隔离）
- 版本号 v0.4.57-l3→v0.4.58-l3（release 62→63，versionCode 67→68）三处同步：paths.rs / module.prop / sunctl
- **实机热更新上线（2026-08-12）**：daemon 热替换 release 62→63（PID 16741，六位一体闭环保持：dex 上报 c0fc580 / hook a001f80 均 match=1）+ 设备端 sunctl 同步 0.4.58-l3（rules 子命令上线）
- **规则引擎端到端验证（实机）**：`sunctl rules status/list` 正常；写入测试规则（expires_at 过期 + 不存在包，零风险）→ 8s 内自动热加载 count=1 / revision=mtime / events 留痕 rules_reloaded ×3；恢复默认空表回落 count=0（热加载 + 失败安全链路实测通过）

---

**B2 事件订阅注册表（缺口补入 B 档架构层）+ CI 正式发布流程改造（v0.4.56-l3 起不做 Nightly 测试版）**

### B 档 · P1 架构演进
- **B2 事件订阅注册表**（替代全量广播，按需分发）：
  - `daemon` 新增 `Subscription` 过滤器：事件类型（kinds）+ 包名（packages）双轴
  - **默认全量**（Default = 收所有事件），旧 dex 不声明 subscribe 行为不变（零风险兼容）
  - 包名精确 + `pkg.*` 前缀通配；无 `pkg=` 事件（frozen-sync/candidate-sync）仅按 kind 过滤
  - 协议扩展（只增不改）：订阅连接新增 `subscribe` 命令（`kinds=<a,b> packages=<x,y>` 声明兴趣 / `query` 查询 / `clear` 重置全量）；未知 key 前向兼容忽略；格式错误 Err 防静默吞错
  - `state.rs` dex_clients 升级为 (id, stream, Subscription) 三元组，`broadcast_line`/`broadcast_dex` 按过滤器分发（不感兴趣连接保留但不通知）；main.rs 广播携带 kind

### CI 正式发布流程（v0.4.56-l3 起）
- 删除 nightly 滚动三步（tag 移动 / 旧 assets 清理 / 滚动 Release）
- 新增 **CHANGELOG 版本条目防呆**：每个版本必须留档，缺失则构建失败
- 新增 Release notes 提取（awk 抽取当前版本条目为 body）
- 发布正式 Release：**tag = 版本号**，非 prerelease，覆盖 asset 软发布
- 新增 CHANGELOG.md 版本档案（v0.1.0-l0 → 当前全版本留档）

### 验证
- cargo test 65/65（原 58 + 新增 B2 订阅 7 项：解析 4 / 匹配 2 / pkg= 提取 1）
- 正式 Release 流程全链路验证：v0.4.56-l3（55c65d0）/ v0.4.57-l3（c0fc580，312b710 软更新）自动创建成功（tag=版本号，非 prerelease，asset=sundown-*.zip）
- **实机热更新上线（2026-08-12）**：daemon 热替换 release 61→62（PID 12311）+ probe.dex 热切换（hello-dex c0fc580 match=1）+ 设备端 sunctl 同步修复版（magic mount 实时生效，无参更新改查 releases/latest）+ 六位一体闭环（dex 上报 c0fc580 = 模块 probe.dex.hash = root 侧字节源）
- fix(sunctl)（312b710）：无参 apply-update 原查已删除的 nightly tag，改为 releases/latest（最新正式 Release）
- nightly tag/Release 已废弃清理（正式发布流程上线后）

---

## v0.4.56-l3 (2026-08-12)

**缺口补入 A 档 + B1（决策：不重写，三档增量补入，见 `Sundown-缺口补入清单.md`）**

### A 档 · P0 行为层
- **A1 解冻预热链**（回应"解冻后无响应"体验问题，v0.4.22 遗留）：
  - `freezer.rs` 新增 `madvise_willneed`（pidfd_open=434 + process_madvise=440，读 /proc//maps 收集可读映射，clamp ≤64 段/单段 ≤8MB，跳过内核特殊段）+ `madvise_warm_uid`（日志纪律：仅成功一条 logi）
  - 接入 `unfreeze_uid` 与 `unfreeze_uid_keep_oom` 两条解冻路径（放行前预热，失败安全不阻塞；discard 路径自动继承）
  - `_verify/madvise_test.c` 实机探针（与实现同参数纪律，errno 判读表，待真机执行）
- **A2 立即墓碑档位**（grace 语义扩展）：
  - `grace_seconds = 0` = 离开前台即入冻结队列（tick 300ms 落冻，完整豁免链 + 二次校验不减判，防抖由 cooldown 承担）
  - `action.toml` 新增 `instant` 预设（grace=0 / cooldown=120 / keep_fg_service / keep_media）；`preset.rs` 测试覆盖（0 不被 max(0) 钳掉）
- **A3 压制维度扩展**（service/pendingintent 门控，同构复制 receiver_gate）：
  - `policy.rs` 新增 `service_gate` / `pendingintent_gate`（缺省空 = 全放行零风险，`*` 通配）
  - `engine.rs` on_wakeup 三源统一门控（receiver_gated / service_gated / pendingintent_gated 留痕不解冻；IMPORTANT 档绕过；门控优先于节流）
  - `WakeupHooks.java` service/pendingintent 上报组件名匹配键

### B 档 · P1 架构演进
- **B1 HookRegistry**（hook 点统一注册层）：
  - `dex/hook/Registry.java`：hook 点注册条目化（id/宿主/兜底宿主/方法/回调/critical/能力说明）+ installGroup（critical 失败整组回滚）/ uninstallAll / describe（env-check 导出面）
  - FocusHooks / WakeupHooks 迁移为注册表驱动（死代码清理）；广播宿主 BC(A14+)/AMS(<14) 经 fallbackHost 处理

### 修复
- **fix(sunctl)**: sed 版本/URL 提取 `\u0001` → `\1`（GNU 扩展在 Android toybox sed 不可用，导致 apply-update 无法自动解析 nightly URL 与版本号）

### 验证
- cargo test 58/58；javac dex 全量编译通过；CI 四轮全绿（A档 / B1 / 版本号 / sunctl 修复）
- 实机热更新上线：daemon 热替换 release 60→61 + probe.dex 热切换（hello-dex e803e89 match=1）

---

## v0.4.55-l3 (2026-08-11)

- 系统组件保护强化：CRITICAL_PACKAGES 37→46（拨号/设置存储/电话存储/NFC/WiFi 对话框/ColorOS 私有组件）
- `pm list packages -f` 分区路径判定（/system /vendor /product /odm /system_ext /apex）+ 厂商包名域兜底
- discard 落刀前终检：exempt 表实时判定（fg_service/media/location）与 tick 冻结路径同构
- 联通 ANR 事件实机印证：焦点抖动致 last_focus 失真 → 持有前台服务/媒体/定位的冻结 app 不得 SIGKILL
- B09：daemon 版本显示与防降级基线改以 daemon.ready 优先

## v0.4.54-l3 (2026-08-09)

- 日志按「版本 × 日期」归档（logs/<VERSION_NAME>/<YYYY-MM-DD>/）+ 旧平铺日志一次性迁移
- write_freeze ENOENT 降噪（系统 uid 不在 apps cgroup 树 / 应用未运行时 uid 目录不存在 → 静默）
- 热更新冲突修复：apply-update 激活加维护窗口（.updating 通知 service.sh 看门狗暂停自动重启）

## v0.4.53-l3 (2026-08-08)

- （日志归档机制前置准备；迭代记录见 v0.4.54）

## v0.4.52-l3 (2026-08-07)

- 超时丢弃（冻结超时 SIGKILL 释放内存）：frozen_timeout_seconds / mem_watermark_mb / boot_reclaim
- 安全护栏三机制：只作用于 Sundown 冻结集/候选池；归属核验防 pid 复用误杀；丢弃前 exempt 终检

## v0.4.51-l3 (2026-08-06)

- ProcessRecord#killLocked 拦截（ColorOS 滑卡 o-stop 杀进程根治，reason 白名单保护候选池）

## v0.4.50-l3 (2026-08-05)

- （迭代记录，详见 v0.4.49 系列事故报告）

## v0.4.49-l3 (2026-08-05)

- 动态系统 app 保护（pm 枚举）：系统组件冻结 = 隐式 Intent/凭据/安装/文件选择链路黑屏（相机事故根治）

## v0.4.47-l3 (2026-08-05)

- 保持 OOM 保护（adj=-1000）候选池：防系统 AppFreezer 抢先 pid 级冻结（拉锯黑屏根治）
- cgroup v2 父子级 freeze 独立：解冻必须遍历 pid_* 子目录写 0（pid 级残留冻结根治）

## v0.4.43-l3 (2026-08-04)

- receiver_gate 广播门控白名单（对齐 AStop Receiver gate 裁剪版）

## v0.4.42-l3 (2026-08-03)

- wake_throttle_seconds 唤醒节流（对齐 AStop Probe 60s 限流，防 FCM/广播风暴抖动）

## v0.4.32-l3 (2026-08-03)

- SetClassStatus 概率性地雷根治（lsplant patch + 六位一体刷写闭环）
- 12 轮冷启动压测通过；v0.4.33~v0.4.45 逐版本迭代起点

## v0.4.29-l3 (2026-08-03)

- 冻结集持久化 + 开机归属对账（"上次会话 Sundown 冻结集"权威源）

## v0.4.28-l3 (2026-08-03)

- bpf 路径 EINVAL 实锤 + PinFile 网络探测链修复 + 对账归属缺陷

## v0.4.24-l3 (2026-08-02)

- 热切换旧代线程撞已释放 dex 内存崩溃修复（热切换线程屏障 join(3000)）
- DefenseHooks 第一刀 + critical 内置清单实测

## v0.4.22-l3 (2026-08-02)

- VPN 守护进程保护（tun 隧道持有者永不冻结）+ 残留对账清理 20+ uid
- （同日记录"解冻后能打开但无响应"——A1 解冻预热链的前身线索）

## v0.4.0-l3 (2026-07-30)

- L3 策略引擎落地：per-app 策略分级 + 结构化事件缓冲 + TOML 热加载 + cgroup freezer 冻结 + 豁免判定
- 发布默认观望模式（enabled=false）

## v0.3.x-l2 (2026-07-2x)

- L2 dex 探针：focus/wakeup hook 观测组 + libc++_shared 缺失根因修复 + ServerClasses 解析（v0.3.4-l2）
- KernelSU 暂存-合并机制 + WebUI v2.x 演进

## v0.2.x-l1 (2026-07-1x)

- L1 libsunprobe.so Zygisk 探针桩

## v0.1.0-l0 (2026-07-0x)

- L0 sundownd 守护进程骨架（Unix socket 控制面 + 版本闭环起点）

---

*早期版本（v0.4.1~v0.4.31 之间未列出的 patch 迭代）详见 `Sundown-全量设计挖掘-完整文档.md` 与各版本验证/事故报告。*
