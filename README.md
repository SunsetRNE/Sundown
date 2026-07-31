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
│   └── sunctl-spec.md      # sunctl CLI 命令规范与退出码契约
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
    └── webroot/
        └── index.html      # KSU WebUI 仪表盘（L0 只读 + daemon/运行时控制）
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

## 当前状态：L0 ✅ ｜ L1 ✅（真机验证通过）
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
- [ ] L2：`probe.dex` + LSPlant 集成 + 热切换
- [ ] L3：策略引擎与情景预设

## 依赖

- KernelSU 系 root（原版 / Next / SukiSU 等分支均兼容）
- ReZygisk（L1 探针提供方，L0 阶段可选；推荐 v1.0.0+）
- Android 11 (API 30)+

## 参考资产

工作区 `zygisk-research/` 内有完整调研包：Zygisk API v4 头文件、
5ec1cff/PShocker 模块模板、Magisk 原生加载链源码、ReZygisk 源码（含 webroot 参考实现）。
旧实现参考：`AStopV1.7/`（Cerberus daemon + 全套模块脚本，本骨架由其改名演进）。
