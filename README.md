# 🌇 Sundown

**日落而息 · 墓碑调度** — by SunsetREN

面向 KernelSU 系的 Android 墓碑（应用冻结）调度模块。从 AStop/Cerberus 演进而来：
脱离 LSPosed 依赖，改用自有 Zygisk 探针 + KSU WebUI 管理，全链路支持热更新。

## 架构分层（热更新粒度）

| 层 | 资产 | 职责 | 更新方式 |
|---|---|---|---|
| L0 | `sundownd` | 调度大脑：冻结执行、策略决策、超时丢弃、socket 服务 | staged 更新，看门狗重启 |
| L1 | `libsunprobe.so` | Zygisk 探针桩：注入 system_server，socket 收发 + dex 加载 | 软重启 zygote |
| L2 | `probe.dex` | 探针逻辑：LSPlant Java hook（焦点/豁免/防御/唤醒/断网） | socket 推送，ClassLoader 热切换 |
| L3 | conf/*.toml | 策略与豁免配置（policy/action 双文件） | inotify 热加载，完全无感 |

## 目录结构

```
Sundown/
├── README.md               # 本文件
├── NAMING.md               # 命名规范（定稿，唯一权威副本）
├── docs/
│   ├── sunctl-spec.md      # sunctl CLI 命令规范与退出码契约
│   ├── l2-plan.md          # L2 推进计划（DAC 裁决 / 协议 / 热切换，权威副本）
│   ├── l2b-plan.md         # L2b 推进计划（LSPlant 集成 / hook 点 / 事件上行，权威副本）
│   └── l3-plan.md          # L3 推进计划（策略引擎 / 冻结执行 / 豁免决策，权威副本）
├── daemon/                 # sundownd Rust 源码（L0，仅依赖 libc）
│   ├── Cargo.toml          # 仅依赖 libc；release 体积优化
│   ├── README.md           # 构建/部署/staged 更新约定/冒烟测试
│   └── src/
│       ├── main.rs         # 入口：信号、ready 标记、主循环、单实例守护
│       ├── paths.rs        # 路径与版本常量（RELEASE_NO 只增不改）
│       ├── logging.rs      # 极简日志（sundownd.log + stdout，本地时区）
│       ├── state.rs        # 共享状态 + status JSON（契约只增不改）
│       ├── sock.rs         # Unix socket 控制面（行协议 + 事件订阅）
│       ├── config.rs       # inotify conf/ 热加载（L3 接入点）
│       ├── toml.rs         # 手写 TOML 子集解析器（零依赖）
│       ├── policy.rs       # Policy 模型 + 解析/校验/重建（失败保留旧表）
│       ├── preset.rs       # action.toml 情景预设（内存切换不动磁盘）
│       ├── freezer.rs      # cgroup 冻结执行 + SIGSTOP 兜底 + OOM 锁定 + 归属核验
│       ├── engine.rs       # 策略引擎：事件消费 + 进程/包表 + grace/冷却/节流/门控 + tick
│       ├── events.rs       # 结构化事件缓冲（环形 256）+ JSONL 审计落盘
│       └── network.rs      # 网络统计（netd eBPF map + xt_qtaguid + /proc 多源）
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

## 当前状态：L0 ✅ ｜ L1 ✅ ｜ L2 ✅ ｜ L2b ✅ ｜ L3 ✅（**v0.4.52-l3**：超时丢弃（Timeout Discard）落地——冻结超时/内存水位/开机回收三机制 + 安全护栏 + 观测面；AStop 差距矩阵 P0/P1/P2 全阶段完成；发布默认观望模式 `enabled=false`，冻结功能待用户确认后开启）
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
- [x] VPN 守护进程硬豁免 + 残留冻结三路兜底（v0.4.22-l3，修复实机冻死/全网断网）：
  - 根因链：VPN app 承载 tun 隧道被冻结 = 隧道断开 = 全网 app 断网；其主进程非 :push 类选择性冻结保不住、fg 软豁免依赖 dex 判定有失效窗口；白名单热更新只影响未来决策（已冻结包不追溯解冻）；daemon 崩溃/重启后 cgroup.freeze 内核态保留但 frozen 表清空 → 切前台不解冻 → "能打开但点击无响应" ANR 闪退
  - VPN 硬豁免：自动探测持有 tun 设备 fd 的进程所属 uid（`/proc/<pid>/fd` readlink 命中 `/dev/tun*`，60s 缓存，apps cgroup pid 枚举 + /proc 回退），decide_leave/tick/freeze_now 三处拦截（force 也拦截），事件留痕 `vpn_protected`；policy 新增 `[vpn] keep_vpn=true`（缺省开）+ `packages` 手动兜底列表
  - 策略热更新对账：reload 后 frozen 表重新评估，新增白名单/VPN/豁免的已冻结包立即解冻（reason=policy_reload）
  - 启动残留清理：daemon 启动即扫描 apps/uid_*/ 写 0 全量解冻（uid 层 + pid 子层）
  - on_focus 兜底：frozen 表无记录但 uid 实际冻结（残留/事件丢失）→ 仍解冻（reason=residual_thaw）
  - tick 低频对账（每 30 tick ≈9s）：实际冻结但表无记录 → 解冻，防僵尸状态
  - WebUI v2.3：策略页新增 VPN 守护进程保护开关；policy.toml 模板补 [vpn] 段文档
- [x] per-app 网络豁免位 + 冻结中网络唤醒（v0.4.23-l3，参考 AStopV1.7 研究落地）：
  - 背景：AStop 研究确认其网络解法 = `force_network_exemption`（网络活跃即豁免）+ `allow_network_wakeup`（冻结后网络事件唤醒）+ critical_apps.txt 强制名单；Sundown 已有 keep_high_network（高阈值 256KB/30s），缺"任何网络活动"档位的宽松豁免与冻结中唤醒
  - 网络豁免 `keep_network`（全局 `[whitelist]` + per-app `[apps."pkg"]`，缺省 true）：`NetSampler::is_active_any` 窗口内流量增量 >0 即活跃（内核侧统计——进程被 cgroup 冻结后 rx 仍计数）；decide_leave 豁免链新增 `network_exempt`、tick 到期二次校验新增 `tick_network_exempt`——VPN/推送/下载/通话类网络敏感 app 有流量在跑永不进 grace
  - 冻结中网络唤醒（对齐 AStop allow_network_wakeup）：tick 每 10 拍（≈3s）扫描 frozen 表中 keep_network 开启者，检测到网络活动 → 解冻（reason=network_wakeup，进冷却防抖）——防"冻死断流"（隧道心跳/外部发包仍被内核计数）
  - WebUI v2.4：策略页新增网络豁免开关；policy.toml 模板补 keep_network 文档（全局 + per-app）
- [x] P0 防御体系五件套（v0.4.24~v0.4.27-l3，对齐 AStop 防御链）：
  - v0.4.24：内置 critical 名单硬豁免 + L2 dex ANR 隐身（firstPids 过滤/SIGQUIT 豁免/errorState 过滤）+ 系统 freezer 防双冻结 + Activity 保护（destroyImmediately/releaseSomeActivities）
  - v0.4.25：热切换换代停旧代全部线程（ExemptMonitor stop/join + eventThread interrupt）——修实机热切换后旧代线程在已释放 dex 内存上解释执行致 system_server SIGSEGV
  - v0.4.26：ColorOS HANS/Freezer 防御适配——freezeAppAsync*LSP 防双冻结（实机校准 ColorOS 改名）+ HANS unfreeze 解冻防御 + isProxyed/GMS 限制禁用
  - v0.4.27：HANS 防御误伤修复——frozen-sync 冻结集归属协议（daemon 广播 Sundown 冻结 uid 集 + dex 双源判定）+ DefenseHooks public 可见性修复
- [x] keep_network 数据源修复（v0.4.28-l3）：netd map pin 文件解析源（open pin 文件非真 map fd 必然 EINVAL）+ dex 自愈换代延迟 3-10s（防部署触发 LSPlant SetClassStatus 崩溃）
- [x] 对账解冻归属修复 + 冻结集持久化 + 网络源启动自检（v0.4.29-l3）
- [x] 换代风暴熔断 + 自动自愈换代禁用（v0.4.30-l3，软重启事故修复）；state/ 目录补建 + dex 字节源启动自检（v0.4.31-l3）
- [x] SetClassStatus 概率性地雷根治（v0.4.32-l3）：LSPlant patch（class.cxx 四重载判空）+ dex waitForBootCompleted 双保险 + 版本三处同步——冷启动 4+ 分钟零崩溃，三个历史崩溃窗口全过
- [x] tombstone_15 zygisk 早期崩溃根治（v0.4.33-l3）：`-fno-threadsafe-statics` 入口零 PLT（zygote64 fork 后 4s guard PLT 崩溃），第 3 轮冷启动 59s+ 无崩溃
- [x] 日志观测性（v0.4.34/35-l3）：时间戳本地化（localtime_r）+ fetch-dex 解析失败诊断增强 + 双括号瑕疵修复
- [x] P1 收官系列（v0.4.36~v0.4.40-l3）：
  - v0.4.36：SIGSTOP 兜底冻结（cgroup 写失败降级，uid_pids 归属核验防 pid 复用）+ 幂等 SIGCONT 解冻
  - v0.4.37：OOM 保护——冻结期 oom_score_adj 锁 -1000（OOM_DISABLE 等效），解冻恢复原值
  - v0.4.38：事件审计持久化——JSONL 增量落盘 + 2MB 滚动保留 3 份 + seq 全局序号
  - v0.4.39：防御补全——OomAdjuster 防御（防系统重算覆盖 -1000）+ 耗电判定豁免 + Doze 白名单注入
  - v0.4.40：dex 版本解析步长 8→4 修复（DEX 规范 string_id_item=4B，hash 落奇数索引误报解析失败）
- [x] P2 系列（v0.4.41~v0.4.45-l3，对齐 AStop 策略模型）：
  - v0.4.41：IMPORTANT 档（AppMode::Important，grace=全局×2 + 唤醒解冻强制开启不可关）
  - v0.4.42：唤醒节流（wake_throttle_seconds，对齐 AStop Probe 60s 限流）
  - v0.4.43：Receiver gate 广播门控（白名单 action 才触发解冻，dex 透传 action）
  - v0.4.44：break_network（hook OplusDeepSleep 冻结集内 uid 断网，对齐 AStop OplusDeepSleepHooks）
  - v0.4.45：config export/import 本地配置压缩包迁移（用户决策不走云端）
- [x] 实机事故四轮修复 + 相机黑屏根治（v0.4.46~v0.4.50-l3）：
  - v0.4.46：选择性冻结路径补 OOM 保护 + tick 冻结集周期重锁（防系统 remove task 杀冻结 app）
  - v0.4.47：pid 级解冻彻底化（父子级全遍历）+ grace 期提前锁 adj + adj_keep 解冻保持
  - v0.4.48：candidate-sync 候选池广播（frozen+grace+adj_keep 并集）——系统冻结器/OOM/耗电防御对候选池全生效（dex 侧三处判定改候选池）
  - v0.4.49：系统组件保护——CRITICAL_PACKAGES 扩展 19 个 + 动态 system_apps 保护（pm 枚举 392 个，三处接入）
  - v0.4.50：系统链路 OOM 锁定（相机黑屏根治）——37 编译期组件 + android.process.media 恒锁 adj=-1000 + 防御性解冻残留；系统 AppFreezer 冻结媒体进程 → 相机 binder EPIPE(-32) → onDeviceError 黑屏，锁定后系统冻结逻辑跳过链路组件
- [x] Recents 任务保护实测补丁（v0.4.51-l3）：
  - 实测校准：ColorOS 滑卡/清 Recents 不走 ATMS.removeTask，实锤 o-stop(40)（OplusProcessManager）→ force-stop 直接杀进程
  - 根治机制：候选池 app adj=-1000（OOM 锁定）→ force-stop 视同 persistent 跳杀；killLocked 为候选池同步窗口期双保险（onTaskRemove ATMS+TaskSupervisor 双线 + onKillProcess CachedAppOptimizer + onKillLocked ProcessRecord#killLocked + reason 白名单）
  - 部署：sundownd release 56 + dex 6a72d93 六位一体闭环
- [x] 超时丢弃三机制（v0.4.52-l3，行为概念《超时丢弃》落地）：
  - 冻结超时丢弃（frozen_timeout_seconds 缺省 1800=30min，0=关闭）：冻结集条目超时且期间零活跃 → SIGKILL 整 uid 释放内存；"零活跃"口径 = 任何实际解冻重置超时，节流/门控拦截的唤醒不续期（防 FCM 风暴无限续期）
  - 内存水位丢弃（mem_watermark_mb 缺省 512，0=关闭）：MemAvailable 低于水位 → 按 LRU（最旧优先）+ RSS 占用排序丢弃直到恢复（每 6s 采样）
  - 开机缓存回收（boot_reclaim=true + delay 120s）：boot_completed 后延迟扫描"上次会话 Sundown 冻结集 ∪ 当前冻结集"中 oom_score_adj ≥ 900 的 cache 档包 → 丢弃（"开机高缓存"主动回收，只执行一次）
  - 安全护栏：只作用于 Sundown 冻结集；白名单/exempt/IMPORTANT/critical/系统组件/VPN/前台一律不参与（discard_ineligible 判定面）；丢弃前解冻写 0 防内核残留 + cgroup.procs 归属核验防 pid 复用；丢弃=终态不可撤销
  - 观测：事件 discard pkg=... reason=... + JSONL 审计 + status discard_ops/discarded_packages/discard_reasons/discard_timeout_s（契约只增不改）+ sunctl status 文本行
  - 测试 46/46（discard_parse_v052 / frozen_timeout_discard_v052 / mem_watermark_discard_v052 / boot_reclaim_v052）；sundownd release 57

---

# 行为概念：墓碑行为对齐之超时丢弃（Timeout Discard）

> 状态：✅ 已实施（v0.4.52-l3，sundownd release 57；46/46 测试通过，待真机 enable=true 验证）｜ 决策：SunsetREN
> 一句话：**墓碑该做的不是"永远冻着"，而是"冻住 → 过期 → 丢弃 → 释放内存"。**

## 0. 痛点（真实用户感受，本项目立项动机）

- **冻结 ≠ 释放内存**：cgroup freezer 只是暂停调度，app 的 RSS / 缓存页仍完整占用内存。
- **冻死 / 冻超时**：大量 app 被冻结后长期无人问津，系统可用内存被"冻结尸体"持续挤占——表现为"手机缓存拉得很高"、"用着难受"。
- **开机高缓存不清理**：开机时系统自动恢复/预加载一批 app（boot 恢复 + cache 进程预建），墓碑模块不主动回收，用户只能等系统自己慢慢杀（或手动清后台）。
- 现有流行墓碑（AStop/Cerberus）**没有完整闭环**（见 §1），上述体验缺陷长期存在。

## 1. 现状对照：主流墓碑做了什么、缺什么（AStop v1.6.0 反编译实证）

| 机制 | AStop 实现 | 与"超时丢弃"的关系 |
|---|---|---|
| PostCheck sync-kill | 冻结后核验，**残留进程** SIGKILL（kill_all 模式） | 只清理"冻结失败的残留"，不处理"冻结成功但超时"的尸体 |
| 孤儿进程清理 | `Frozen > 30m` 清扫**辅助进程**（保留主进程） | 方向最接近但**不动主体**：主进程仍占内存，且无"整包丢弃"语义 |
| 内存四件套 | MemoryGcMode / MemoryTrimMode / MemoryCompactionMode / MemoryPolicyMode | 是**主动整理**（GC/trim/compaction），不是**丢弃释放**；且依赖 LSPosed 内 API |
| 内存水位触发 | **无**（全源码无 MemAvailable / watermark / lowMemory 检索命中） | 完全缺失：内存告急时不会主动释放冻结集 |
| 开机缓存回收 | **无** | 完全缺失 |
| 冻结时长上限→整包丢弃 | **无**（>30m 只清辅助进程） | 完全缺失 |

**结论**："超时丢弃"（冻结超时 → 整包丢弃释放内存）在主流墓碑中不存在完整实现——这是 Sundown 的差异化行为概念。

## 2. Sundown 超时丢弃设计（三机制）

### 2.1 冻结超时丢弃（Frozen Timeout Discard）——主机制
- 冻结集条目带 `frozen_since` 时间戳；冻结时长超过 `[discard] frozen_timeout_seconds`（缺省 **1800s = 30min**，0=关闭）且期间无任何唤醒命中 → 升级为**丢弃**（SIGKILL 整 uid，cgroup.procs 归属核验防 pid 复用误杀）。
- 语义：**30 分钟没被唤醒的冻结 app ≈ 用户不需要** → 丢弃释放内存；"丢弃"成为冻结的终态之一（与"解冻"并列）。
- 与唤醒节流/门控联动：节流窗口内的唤醒不算"活跃"（防 FCM 风暴把超时无限续期）；Receiver 门控拦截的广播同样不续期。
- 丢弃前先写 `cgroup.freeze=0`（防内核态残留），随后 SIGKILL 主进程组。

### 2.2 内存水位丢弃（Memory Watermark Discard）——加速器
- daemon 周期采样 `/proc/meminfo` MemAvailable；低于 `[discard] mem_watermark_mb`（缺省 **512MB**，0=关闭）→ 按 **LRU（frozen_since 最旧优先）+ RSS 占用** 排序，丢弃冻结集直到水位恢复。
- 只作用于 Sundown 冻结集/候选池——白名单 / IMPORTANT / critical / 系统组件 / 前台豁免天然不参与。
- 语义：超时未到但内存告急 → 提前丢弃最不活跃的冻结 app，防"内存挤炸"。

### 2.3 开机缓存回收（Boot Cache Reclaim）——开机自愈
- boot_completed 后延迟 `[discard] boot_reclaim_delay_seconds`（缺省 **120s**，等系统恢复期结束），扫描 cache/空进程档（`/proc/*/oom_score_adj` 判定）与"开机恢复的冻结候选"，按策略丢弃——**"开机时的高缓存"由 Sundown 主动回收**，不再等系统慢慢杀。
- 只回收 cache/empty 档（adj ≥ 缓存档），绝不动前台/感知/服务进程。
- 观测：`boot_reclaim` 事件留痕 + status 计数（回收了谁、释放多少，全量可审计）。

### 2.4 状态机扩展（冻结终态）
```
冻结集条目
  ├─ 唤醒/前台命中 → 解冻（原有链路不变）
  ├─ frozen_timeout 到期（期间零活跃）→ 丢弃（SIGKILL，reason=frozen_timeout）
  ├─ 内存水位告急 → 按 LRU+RSS 丢弃（reason=mem_watermark）
  └─ 开机回收窗口 → cache 档候选丢弃（reason=boot_reclaim）
```

## 3. 安全护栏（铁律延续）

- **丢弃只作用于 Sundown 自己的冻结集/候选池**；白名单 / per-app exempt / IMPORTANT / critical 内置名单 / 动态 system_apps / VPN 硬豁免 / 前台 app 一律不参与（复用 v0.4.49 三处接入的判定面）。
- 丢弃前 **cgroup.procs 归属核验**（防 pid 复用误杀，v0.4.36 同款）；失败安全：核验不过 → 放弃丢弃 + 留痕。
- **丢弃 = 最终动作不可撤销**；用户切回时走冷启动（权衡文档化：冻结→丢弃→冷启动 vs 冻结→解冻→热恢复，丢弃只针对"30 分钟无活跃"的 app，冷启动代价可接受）。
- 事件留痕：`discard pkg=P uid=N reason=frozen_timeout|mem_watermark|boot_reclaim` + JSONL 审计 + status `discard_ops` / `discarded_packages` 观测（契约只增不改）。
- SIGKILL 权限面：daemon root 既有；SELinux 与 cgroup 写同域（v0.4.36 SIGSTOP 已有 kill 先例，无新面）。
- **发布默认**：`frozen_timeout=1800` / `mem_watermark_mb=512` / `boot_reclaim=true` 全部**默认开启**（直击用户痛点），但受 `[general] enabled` 总开关约束——观望模式零动作。

## 4. 实现落点（预估，v0.4.52-l3）

| 文件 | 改动 |
|---|---|
| `daemon/src/freezer.rs` | `discard_uid`（解冻写 0 → SIGKILL 进程组 → 清理记录） |
| `daemon/src/engine.rs` | tick 超时检查（frozen_since 水位）+ MemAvailable 采样 + boot 定时器（boot_completed 事件驱动） |
| `daemon/src/policy.rs` | `[discard]` 段解析（frozen_timeout_seconds / mem_watermark_mb / boot_reclaim_*，0=关闭，失败安全） |
| `daemon/src/state.rs` | status 追加 `discard_ops` / `discarded_packages` / `discard_reasons`（只增不改） |
| `daemon/src/events.rs` | 事件语义扩展（discard 三 reason） |
| `module/conf/policy.toml` | `[discard]` 段模板 + 注释文档 |
| `sunctl` | status 文本面 discard 行 |
| WebUI | 只读展示（后置） |
| 测试 | frozen_timeout_discard_v052 / mem_watermark_discard_v052 / boot_reclaim_v052（不依赖真实 cgroup，mock 水位与时钟） |

## 5. 明确不做（本刀留白）

- ❌ 丢弃非冻结集 app（那是"清后台"，不是墓碑职责——系统 LMK 管）
- ❌ 内存四件套（GC/trim/compaction 主动整理，AStop 路线，P3 独立评估）
- ❌ 云端/定时全盘清理（破坏 Android 应用生命周期预期）
- ❌ 桩（probe.cpp）任何改动；daemon 仍仅依赖 libc

---

## 待办（后置）
- [x] ~~超时丢弃实现（v0.4.52-l3）~~ ✅ 已落地：冻结超时丢弃（frozen_timeout_seconds 缺省 1800）+ 内存水位丢弃（mem_watermark_mb 缺省 512）+ 开机缓存回收（boot_reclaim）——设计见上文《行为概念：墓碑行为对齐之超时丢弃》
- [ ] v0.4.52-l3 刷入设备 + enable=true 实机验证（冻结/解冻正向链路 + frozen.state 持久化对账 + keep_network enable 场景 + 相机/Recents 保护实测 + 超时丢弃三机制观测）
- [ ] 冷启动压测 ≥10 轮无崩溃（当前 11 轮 ✅，续测）
- [ ] 定位活动豁免 dex 侧 AppOps 判定（ExemptMonitor.java 扩展 loc 字段上报，走 L2 热更新路线）
- [ ] 完整热更新闭环打磨（WebUI 检测新版 → 下载 → staged → 重启激活 → 回滚的端到端体验）
- [ ] WebUI 日志页交互打磨（v3 数据面就绪后的分组/时间线视图）
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
