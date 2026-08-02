# 🌇 Sundown

**日落而息 · 墓碑调度** — by SunsetREN

面向 KernelSU 系的 Android 墓碑（应用冻结）调度模块。从 AStop/Cerberus 演进而来：
脱离 LSPosed 依赖，改用自有 Zygisk 探针 + KSU WebUI 管理，全链路支持热更新。

## 架构分层（热更新粒度）

| 层 | 资产 | 职责 | 更新方式 |
|---|---|---|---|
| L0 | `sundownd` | 调度大脑：冻结执行、策略决策、socket 服务 | staged 更新，看门狗重启 |
| L1 | `libsunprobe.so` | Zygisk 探针桩：注入 system_server，socket 收发 + dex 加载 | 软重启 zygote |
| L2 | `probe.dex` | 探针逻辑：LSPlant Java hook（焦点/豁免/Binder） | socket 推送，ClassLoader 热切换 |
| L3 | conf/*.toml | 策略与豁免配置 | inotify 热加载，完全无感 |

## 目录结构

```
Sundown/
├── README.md               # 本文件
├── NAMING.md               # 命名规范（定稿，唯一权威副本）
├── docs/
│   ├── sunctl-spec.md      # sunctl CLI 命令规范与退出码契约
│   ├── l2-plan.md          # L2 推进计划（DAC 裁决 / 协议 / 热切换，权威副本）
│   └── l2b-plan.md         # L2b 推进计划（LSPlant 集成 / hook 点 / 事件上行，权威副本）
├── daemon/                 # sundownd Rust 源码（L0 最小实现）
│   ├── Cargo.toml          # 仅依赖 libc；release 体积优化
│   ├── README.md           # 构建/部署/staged 更新约定/冒烟测试
│   └── src/
│       ├── main.rs         # 入口：信号、ready 标记、主循环
│       ├── paths.rs        # 路径与版本常量（RELEASE_NO 只增不改）
│       ├── logging.rs      # 极简日志（sundownd.log + stdout）
│       ├── state.rs        # 共享状态 + status JSON（兼容 sunctl-spec）
│       ├── sock.rs         # Unix socket 控制面（行协议）
│       └── config.rs       # inotify conf/ 热加载（L3 接入点）
├── probe/                  # libsunprobe.so C++ 源码（L1 探针桩）
│   ├── CMakeLists.txt      # NDK 构建（arm64-v8a，-DPROBE_BUILD_HASH 注入）
│   ├── README.md           # L1 定位铁律 / hash 闭环 / L2 契约 / SELinux 备忘
│   ├── include/zygisk.hpp  # Zygisk API v4 头文件（模板同款）
│   └── src/probe.cpp       # 桩：system_server 驻留 + hello-probe + dex 加载桥
├── dex/                    # probe.dex Java 源码（L2 探针逻辑层）
│   ├── README.md           # DAC 铁律 / 协议三命令 / 热切换时序 / 版本闭环 / 本地构建
│   └── src/ren/sunset/sundown/  # ProbeMain 入口 + DaemonLink + Runtime 代际 + hook 组
├── bridge/                 # libsundownhook.so C++ 源码（L2b native 伴生库）
│   ├── CMakeLists.txt      # NDK 构建（prefab LSPlant/Dobby 由 CI 注入 + sha256 校验）
│   ├── README.md           # 机制面定位 / LGPL 合规 / 运行时链路 / 修改红线
│   └── src/                # bridge.cpp（五机制出口）+ mini_art_elf（art 符号解析）
└── module/                 # KSU 模块（目录内容即模块 zip 内容）
    ├── module.prop         # id=sundown, author=SunsetREN
    ├── customize.sh        # 安装脚本（环境/ABI 检查、设备适配属性、ReZygisk 检测）
    ├── post-fs-data.sh     # 早期初始化 + Cerberus 旧资产迁移
    ├── service.sh          # sundownd 启动 / staged 更新激活 / 看门狗
    ├── uninstall.sh        # 卸载清理（杀 daemon、恢复属性、删数据目录）
    ├── sepolicy.rule       # system_server ↔ root 域 socket 规则（L1 探针需要）
    ├── system.prop         # LMKD 保后台参数
    ├── system/bin/
    │   ├── sundownd        # 【占位】守护进程二进制，CI 打包时注入真实产物
    │   └── sunctl          # 控制 CLI（L0 shell 实现，命令面已定稿）
    ├── zygisk/             # 【CI 生成】arm64-v8a.so（libsunprobe）+ probe.hash
    ├── probe/              # 【CI 生成】probe.dex + probe.dex.hash（root 字节源，post-fs-data 同步到 /data/adb）
    ├── hook/               # 【CI 生成】hook.hash（bridge 期望 build hash）
    ├── system/etc/sundown/probe.dex  # 【CI 生成】magic-mount 冷启动兜底（uid 1000 可读）
    ├── system/etc/sundown/bridge.dex # 【CI 生成】canonical NativeBridge（L2b 类加载父链）
    ├── system/lib64/       # 【CI 生成】libsundownhook.so + liblsplant.so（L2b 伴生库）
    └── webroot/
        └── index.html      # KSU WebUI 仪表盘（状态展示 + daemon/运行时/探针热更新控制）
```

## 版本号策略（升级检查清单）

模块版本三处同步 + 一处自动，**漏改任何一处都会造成版本漂移**（L1 期间已踩过一次）：

| 位置 | 字段 | 规则 |
|---|---|---|
| `module/module.prop` | `version` / `versionCode` | version 语义化带阶段后缀（如 `v0.2.0-l1`）；versionCode **单调 +1**（KSU 更新感知与未来 updateJson 在线更新的硬要求） |
| `daemon/src/paths.rs` | `VERSION_NAME` / `RELEASE_NO` | VERSION_NAME 与 module.prop version 同步（去 `v` 前缀）；RELEASE_NO 在 **daemon 二进制任何变更时 +1**（只加不改，staged readiness 校验依据） |
| `module/system/bin/sunctl` | `VERSION` | 与 module.prop version 同步（去 `v` 前缀） |
| zip 文件名 | `sundown-<version>.zip` | CI 从 module.prop 读取，自动跟随，无需手改 |

- 分层语义：L0→`v0.1.0-l0`，L1→`v0.2.0-l1`，L2→`v0.3.0-l2`，L3→`v0.4.0-l3`；正式版从 `v1.0.0` 起去阶段后缀
- 阶段内修复/迭代走 **patch 位**（如 L1 握手修复 `v0.2.0-l1`→`v0.2.1-l1`），versionCode 照常 +1；跨阶段才动次版本号
- CI 打包 job 内置防呆校验：三处版本不一致则构建失败
- Nightly 渠道 asset 名随版本变化，CI 自动清理旧 assets，页面永远只有最新一个 zip

## 当前状态：L0 ✅ ｜ L1 ✅ ｜ L2 ✅ ｜ L2b ✅ ｜ L3 ✅（v0.4.21-l3：修复 eBPF bpf() syscall 缓冲区缺陷——Android16 实机校验发现并定位）
- [x] 命名规范定稿（NAMING.md）
- [x] 模块骨架改名（AStop/Cerberus → Sundown 全套脚本）
- [x] Cerberus 旧资产迁移逻辑（post-fs-data.sh）
- [x] `sunctl` L0 实现 + 命令规范（docs/sunctl-spec.md）
- [x] WebUI L0 仪表盘（状态展示、daemon 重启、软重启按钮二次确认）
- [x] `sundownd` Rust 最小实现（daemon/：ready 标记 + socket 控制面 + inotify 热加载）
- [x] Git 仓库初始化 + GitHub Actions CI（docs/git-and-ci.md：push 即编译出模块 zip）
- [x] 推送远端仓库 github.com/SunsetRNE/Sundown + CI 首跑成功（模块 zip artifact 已产出）
- [x] CI 升级 Node 24 actions + Nightly 滚动 Release（单层模块 zip 主下载渠道）
- [x] sunctl status 切换 socket 数据源（nc -U 行协议，失败降级文件探测）
- [x] daemon 真机冒烟（2026-07-31：Nightly 单层 zip 刷入，sunctl status / socket 行协议
  ping/status/probe-query 全部通过，daemon 长稳运行 uptime 7.8h+ 无异常）
- [x] L1 桩工程化：`libsunprobe.so`（probe/：hello-probe hash 握手 + dex 加载桥，
  daemon 协议面 hello-probe/probe-query，CI build-probe 注入 zygisk/arm64-v8a.so）
- [x] L1 真机验证（2026-07-31，ReZygisk v1.0.0 提供方）：
  桩注入 system_server 实证（/proc/<pid>/maps 含 arm64-v8a.so r-xp 可执行映射）；
  probe_stub_loaded=1 + hash_match=1，hash ce2f36b 四位一体闭环
  （桩上报 = 模块 probe.hash = CI 构建 commit = git HEAD）；
  v0.2.2-l1 abstract socket 启动同秒握手成功，对照 v0.2.1-l1 文件 socket 全程无握手，
  根治 /data/adb DAC 层 EACCES 实证
- [x] L2：`probe.dex` 工程化 + 热切换闭环（v0.3.0-l2）：
  dex/ Java 工程（ProbeMain 契约入口 + DaemonLink 帧纪律 + Runtime 代际模型 + LSPlant 降级桥）；
  daemon 协议面 hello-dex（订阅长连接）/ fetch-dex（字节帧）/ push-dex（root 管理面广播）；
  dex 字节全程走 abstract socket（InMemoryDexClassLoader），冷启动兜底 magic-mount
  `/system/etc/sundown/probe.dex`——绕开 /data/adb DAC 铁律；
  post-fs-data dex 同步（hash 比对防无谓 dexopt）、CI build-dex job、
  WebUI/sunctl 热更新入口、status 新增 probe_dex_version/probe_dex_hash_match；
  本地 javac+d8 编译链与 cargo check 零错误。LSPlant 真实 hook 留 L2b
- [x] L2b：LSPlant native 集成 + 焦点/唤醒感知真实 hook（v0.3.2-l2，工程闭环）：
  bridge/ C++ 工程（lsplant::Init/Hook/UnHook/MakeDexFileTrusted 五机制出口 +
  自研 mini_art_elf 符号解析，LGPL 动态链接 prefab liblsplant.so，Dobby 静态链入）；
  canonical NativeBridge 类加载拓扑（bridge.dex 单例父链 + 引导代自热切换，
  桩零触碰）；hook 组 FocusHooks（updateActivityUsageStats/addPidLocked/
  removePidLocked/forceStopPackage）+ WakeupHooks（broadcastIntentLocked/
  realStartServiceLocked/sendInner）全观测模式；EventQueue 非阻塞上行 +
  report-bridge/event 协议扩展；daemon status 新增 probe_hook_bridge_hash/
  focus_pkg/wakeup_events；CI build-bridge job（pinned AAR + sha256 校验）。
  计划与裁决见 docs/l2b-plan.md，hook 点经 AStop v1.6.0 dex 静态扫描实证萃取
- [x] L2b 真机回归（2026-08-02，v0.4.13-l3 版本闭环 c84b14c 四位一体匹配：
  stub/dex/bridge hash 全绿，焦点/唤醒/豁免事件流实测工作）
- [x] L3 策略引擎（v0.4.8-l3 起渐进交付）：
- [x] L3 per-app 策略分级（v0.4.8-l3：`[apps."pkg"]` mode=exempt|standard|strict + grace/豁免开关覆盖，daemon 侧 + 单元测试；设计见 docs/l3-plan.md §0.6）
- [x] L3 结构化事件缓冲（v0.4.9-l3：`daemon/src/events.rs`，EvLevel 7 档 × EvAction 8 种 × subject app/system，环形 256 覆盖最旧 + total 单调计数，零依赖手写 JSON；sock `events [n]` 命令；status 追加 events_count/events_total）
- [x] L3 冻结链路实测（v0.4.11-l3 沉淀：strict 5s / standard 30s / exempt / force / 前台解冻 / 唤醒解冻 / 冷却 7 链路全验证，真实场景抖音 30s 自动冻结 + 点开自动解冻，全程无 ANR；结论固化在 module/conf/policy.toml 注释）
- [x] L3 freeze 事件 reason 语义化（v0.4.12-l3：freeze_now reason 参数区分 `grace_expired`/`force`）
- [x] WebUI 日志页 v3（v0.4.13-l3：数据源切换 sunctl events 结构化 JSON，parseEvent 推导卡片，analyzeLogs 双模式 + 老 daemon 文本降级，焦点停留时长改 ts 秒差）
- [x] 焦点去抖（v0.4.14-l3：hook focus 降级为线索，ExemptMonitor 权威 topActivity 2s 节拍为唯一决策源 + 10s 失效自动恢复 hook 兜底——根治 OPPO ROM resume 残留导致的 force 抖动解冻）
- [x] L3 情景预设（v0.4.15-l3：conf/action.toml `[presets."name"]` 参数组，`policy preset apply <name>`/`clear`/`list` 内存切换不动磁盘 policy.toml；预设只覆盖 [general] 五参数，白名单/force/per-app 始终以磁盘为准；action.toml 缺失/解析失败降级空表不致命；reload 时预设表随热加载刷新、生效中预设重放覆盖、已删除回落磁盘）
- [x] L3 conf 模板首次部署（v0.4.16-l3：customize.sh 数据目录 conf 无 .toml/.json 时部署模块模板 policy.toml+action.toml，已存在配置一律保留——用户配置优先；实机验证预设三链路：apply 内存覆盖不动磁盘 / 热加载重放保持 / 解析失败回落磁盘）
- [x] WebUI 情景预设（v0.4.17-l3：策略页预设区块动态化——preset list 携带参数摘要，WebUI 从 action.toml 实时渲染按钮 + 当前生效高亮 + 清除预设；点击即 `policy preset apply` 内存切换，废弃旧硬编码写磁盘实现）
- [x] 情景预设启动加载修复（v0.4.18-l3：init_engine 启动即加载 action.toml——此前预设表仅 reload 时刷新，daemon 重启后为空；实机校验发现并修复）
- [x] Cerberus 豁免维度扩展 + 子进程管理（v0.4.19-l3）：
  - 高网络负载豁免 `keep_high_network`（全局 + per-app）：daemon 侧 /proc/uid_stat + xt_qtaguid 双源流量采样，30s 窗口增量 ≥256KB 判定活跃 → decide_leave/tick 双重豁免（数据源不可用降级不致命）
  - 交互/FCM 唤醒开关 `keep_wakeup`（per-app）：false = 唤醒不解冻（防 FCM/唤醒风暴反复解冻），事件留痕 `wakeup_ignored`
  - 定时解冻窗口 `unfreeze_window = "HH:MM-HH:MM"`（per-app）：窗口内退后台不冻结（libc localtime_r 零依赖，不支持跨零点）
  - 子进程管理 `push_policy`/`push_mode = keep|kill`（全局 + per-app）：keep = pid 级选择性冻结（:push/:MSF/:channel/:pull 子进程保留运行，推送通道不断，失败回落 uid 级整冻）；kill = 冻结时连带 SIGKILL :push 类子进程（cgroup.procs 归属核验防 pid 复用误杀）
  - WebUI 策略页新增高网络豁免 + 推送子进程保留开关（v2.1），policy.toml 模板文档化
- [x] eBPF 流量数据源 + 定位活动豁免 + 热更新 staged 命令（v0.4.20-l3）：
  - 高网络数据源第三源：AOSP 标准 eBPF map `/sys/fs/bpf/netd_shared/map_netd_app_uid_stats_map`（bpf() syscall 遍历，2s 缓存 TTL；修复 Android16 上 /proc/uid_stat 与 xt_qtaguid 均缺失导致 keep_high_network 失效——OPPO 定制内核实测发现）
  - 定位活动豁免 `keep_location`（全局 + per-app，缺省 true）：daemon 侧消费 dex 上行 `event exempt loc=1` 的 AppOps 判定，decide_leave/tick 双重豁免链扩展为 fg/media/loc 三重（dex 侧判定实现走 L2 热更新后续交付）
  - 热更新命令实装：`sunctl apply-update [zip|URL]` 下载 Nightly 模块包 → 提取 sundownd → 运行 `--version` 解析版本 → SHA256 + installed.json.new + pending.json（staged_boot_id）写入 pending 四件套（防降级：release_no 只增）；`sunctl apply-update --activate` 立即激活（备份 → 替换 → 重启 daemon → 20s+10s readiness 校验 → 失败自动回滚，与 service.sh 同规）
  - installed.json 初始化（daemon 启动仅缺失时写入 version_name/release_no，staged 激活仍由 service.sh 原子替换）
  - WebUI v2.2：策略页新增定位活动豁免开关 + 更多页「检查并升级（Nightly）」一键暂存按钮（下载后确认重启激活）
- [x] eBPF bpf() syscall 缓冲区缺陷修复（v0.4.21-l3）：
  - 实机校验发现 v0.4.20-l3 的 eBPF 源仍失效（日志"网络统计源不可用"），map 文件本身 root 可读但 BPF_MAP_GET_NEXT_KEY/LOOKUP_ELEM/OBJ_GET_INFO_BY_FD 全失败
  - 根因 1：内核 `bpf_check_uarg_tail_zero` 要求用户缓冲区 `[attr_size, sizeof(union bpf_attr))` 区间全零（否则 E2BIG）——原实现传栈上小数组（16/24/32 字节）尾部随机垃圾 → 必然失败
  - 根因 2：`struct bpf_map_info` 在 6.x 内核约 88+ 字节，原 info buffer 仅 64 字节 + info_len=64 → OBJ_GET_INFO_BY_FD 返回 EINVAL，快照函数第一步即失败
  - 修复：`bpf_cmd` 统一改用 256B 清零缓冲承载（`[0u8; 256]` + copy_from_slice 填充前 N 字节，attr_size 传实际字段长度，256 ≥ sizeof(union bpf_attr) 约 144B）；info buffer/info_len 均扩为 256

## 待办（后置）
- [ ] 定位活动豁免 dex 侧 AppOps 判定（ExemptMonitor.java 扩展 loc 字段上报，走 L2 热更新路线）
- [ ] 完整热更新闭环打磨（WebUI 检测新版 → 下载 → staged → 重启激活 → 回滚的端到端体验）
- [ ] WebUI 日志页交互打磨（v3 数据面就绪后的分组/时间线视图）

## 依赖

- KernelSU 系 root（原版 / Next / SukiSU 等分支均兼容）
- ReZygisk（L1 探针提供方，L0 阶段可选；推荐 v1.0.0+）
- Android 11 (API 30)+

## 参考资产

工作区 `zygisk-research/` 内有完整调研包：Zygisk API v4 头文件、
5ec1cff/PShocker 模块模板、Magisk 原生加载链源码、ReZygisk 源码（含 webroot 参考实现）。
旧实现参考：`AStopV1.7/`（Cerberus daemon + 全套模块脚本，本骨架由其改名演进）。
