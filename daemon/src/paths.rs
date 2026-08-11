//! Sundown 路径常量（与 NAMING.md / sunctl-spec.md 定稿一致）

pub const SUNDOWN_DIR: &str = "/data/adb/sundown";
pub const CONF_DIR: &str = "/data/adb/sundown/conf";
pub const DATA_DIR: &str = "/data/adb/sundown/data";
pub const LOG_DIR: &str = "/data/adb/sundown/logs";
pub const UPDATE_DIR: &str = "/data/adb/sundown/update";
pub const READY_MARKER: &str = "/data/adb/sundown/update/daemon.ready";
pub const INSTALLED_META: &str = "/data/adb/sundown/update/installed.json";
pub const SOCKET_PATH: &str = "/data/adb/sundown/sundownd.sock";

// ---- v0.4.53-l3 日志按「版本 × 日期」归档 ----
// 目录结构（logs/ 下）：
//   <VERSION_NAME>/             版本文件夹（customize.sh 刷入即建）
//     install-time              实际刷入时间（customize.sh 写入；手动替换二进制时由 daemon 补写）
//     effective-since           实际启动生效时间（daemon 启动即校验写入——开机后旧版本仍运行时
//                               旧 daemon 写旧版本文件夹，直到新版本 daemon 真正启动才切换）
//     <YYYY-MM-DD>/             日期子文件夹（每天 0:00 后第一条日志惰性新建，本地时区）
//       sundownd.log            引擎日志（原 logs/sundownd.log 平铺 → 归档）
//       events.jsonl(+.1/.2/.3) 事件审计 JSONL（滚动保留 3 份不变，在日期文件夹内滚动）
// 判定规则：写日志时按「运行中 daemon 自身版本 + 本地日期」解析路径——版本归属天然正确，
// 无需全局状态；刷入但未重启（旧版本仍在跑）→ 日志继续写旧版本文件夹（用户需求原话）。

/// 运行中 daemon 的版本日志根：logs/<VERSION_NAME>/
pub fn version_log_dir() -> String {
    format!("{}/{}", LOG_DIR, VERSION_NAME)
}

/// 版本内日期子文件夹：logs/<VERSION_NAME>/<YYYY-MM-DD>/（本地时区，每天 0:00 切换）
pub fn day_log_dir(date: &str) -> String {
    format!("{}/{}", version_log_dir(), date)
}

/// 当前引擎日志路径：logs/<VERSION_NAME>/<本地日期>/sundownd.log
pub fn current_log_file() -> String {
    format!("{}/sundownd.log", day_log_dir(&crate::logging::local_date()))
}

/// 当前事件审计 JSONL 路径：logs/<VERSION_NAME>/<本地日期>/events.jsonl
pub fn current_event_file() -> String {
    format!("{}/events.jsonl", day_log_dir(&crate::logging::local_date()))
}

/// 实际刷入时间记录文件（customize.sh 写入；daemon 启动时缺失则补写）
pub fn install_time_file() -> String {
    format!("{}/install-time", version_log_dir())
}

/// 实际启动生效时间记录文件（daemon 启动写入——开机校验"哪个版本真正生效"）
pub fn effective_since_file() -> String {
    format!("{}/effective-since", version_log_dir())
}

// ---- L1 探针桩相关 ----
/// probe.dex / oat 目录（root 侧字节源落点；桩/dex 层不直接读——DAC 不可达，见下）
pub const PROBE_DIR: &str = "/data/adb/sundown/probe";
/// canonical dex 字节源（root 专属）：fetch-dex / push-dex 从这里读字节经 socket 下发
pub const PROBE_DEX: &str = "/data/adb/sundown/probe/probe.dex";
pub const PROBE_OAT_DIR: &str = "/data/adb/sundown/probe/oat";
/// 期望的桩 build hash（CI 打包时写入模块，daemon 据此比对 hello-probe 上报值）
pub const PROBE_EXPECTED_HASH_FILE: &str = "/data/adb/modules/sundown/zygisk/probe.hash";

// ---- L2 探针 dex 相关 ----
/// 期望的 dex 构建版本（CI 打包写入模块；= 构建 commit short sha，与桩 hash 同源闭环：
/// dex 上报版本 = 模块 probe.dex.hash = CI 构建 commit = git HEAD）
pub const PROBE_EXPECTED_DEX_HASH_FILE: &str = "/data/adb/modules/sundown/probe/probe.dex.hash";

// ---- L2b native 伴生库（libsundownhook.so）相关 ----
/// 期望的 bridge build hash（CI 打包写入模块 hook/hook.hash；与桩/dex hash 同源闭环）。
/// magic-mount 只挂 system/ 子树，hook/ 不入 /system（与 zygisk/、probe/ 同惯例）。
pub const PROBE_EXPECTED_HOOK_HASH_FILE: &str = "/data/adb/modules/sundown/hook/hook.hash";
/// 冷启动兜底 dex 路径：模块 magic-mount（module/system/etc/sundown/probe.dex → /system/...）。
/// 全局可读、SELinux 无争议，uid 1000 的桩文件加载桥可直达；
/// hello-probe / hello-dex 应答的 dex_path 一律指向这里（不再指向 /data/adb 下任何路径，
/// 该目录 drwx------ root，uid 1000 在 DAC 层 EACCES——L1 真机已实证）。
pub const PROBE_DEX_MOUNT: &str = "/system/etc/sundown/probe.dex";

/// 探针桩 / L2 dex 专用通道：abstract namespace socket（无文件路径）。
/// 为什么不用文件 socket 给桩用：/data/adb 是 drwx------ root root，
/// system_server(uid 1000) 在 DAC 层就被拒（无 avc，纯 EACCES）；
/// abstract socket 无路径穿越问题，SELinux 侧 connectto ksu 已放行。
/// 注：root 管理面（sunctl/WebUI）仍走 SOCKET_PATH 文件 socket，双通道并存。
pub const PROBE_ABSTRACT_SOCK: &str = "sundown_probe";

// ---- L3 策略引擎相关 ----
/// 策略文件（TOML；inotify 热加载，失败保留旧表）
pub const POLICY_FILE: &str = "/data/adb/sundown/conf/policy.toml";
/// 情景预设文件（action.toml；[presets."name"] 参数组，policy preset apply 内存切换）
pub const ACTION_FILE: &str = "/data/adb/sundown/conf/action.toml";
/// 包→uid 映射表（root 可读，`pkg uid ...` 行式；冻结执行 uid 定位用）
pub const PACKAGES_LIST: &str = "/data/system/packages.list";
/// 冻结集持久化目录（v0.4.30-l3 补建：v0.4.29 起 persist 写盘一直失败——
/// 该目录未在 ensure_dirs 中创建，实测 daemon 日志 "冻结集持久化写盘失败"）
pub const STATE_DIR: &str = "/data/adb/sundown/state";
/// 冻结集持久化（v0.4.29-l3）：daemon 冻结表落盘（行式 `pkg:uid`），
/// 启动归属对账的"上次会话 Sundown 冻结集"权威源——区分 HANS/系统冻结（无归属证据不碰）
pub const STATE_FROZEN_FILE: &str = "/data/adb/sundown/state/frozen.state";
/// 事件审计 JSONL（P1⑩，对齐 AStop firewall_events 时间线）：结构化事件追加落盘，
/// 一行一 JSON；超阈值滚动保留最近 3 份（events.jsonl.1/2/3）。
/// v0.4.53-l3：落盘路径改为 logs/<VERSION_NAME>/<日期>/events.jsonl（按版本×日期归档），
/// 由 paths::current_event_file() 动态解析。

/// 守护进程版本（与 module.prop version 同步，策略见主 README「版本号策略」）
pub const VERSION_NAME: &str = "0.4.56-l3";
/// 单调递增的发布号：service.sh readiness 校验依据（installed.json vs daemon.ready）
/// daemon 二进制任何变更必须 +1（只加不改）
pub const RELEASE_NO: u32 = 61;