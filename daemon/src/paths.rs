//! Sundown 路径常量（与 NAMING.md / sunctl-spec.md 定稿一致）

pub const SUNDOWN_DIR: &str = "/data/adb/sundown";
pub const CONF_DIR: &str = "/data/adb/sundown/conf";
pub const DATA_DIR: &str = "/data/adb/sundown/data";
pub const LOG_DIR: &str = "/data/adb/sundown/logs";
pub const UPDATE_DIR: &str = "/data/adb/sundown/update";
pub const READY_MARKER: &str = "/data/adb/sundown/update/daemon.ready";
pub const INSTALLED_META: &str = "/data/adb/sundown/update/installed.json";
pub const SOCKET_PATH: &str = "/data/adb/sundown/sundownd.sock";
pub const LOG_FILE: &str = "/data/adb/sundown/logs/sundownd.log";

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
/// 包→uid 映射表（root 可读，`pkg uid ...` 行式；冻结执行 uid 定位用）
pub const PACKAGES_LIST: &str = "/data/system/packages.list";

/// 守护进程版本（与 module.prop version 同步，策略见主 README「版本号策略」）
pub const VERSION_NAME: &str = "0.4.1-l3";
/// 单调递增的发布号：service.sh readiness 校验依据（installed.json vs daemon.ready）
/// daemon 二进制任何变更必须 +1（只加不改）
pub const RELEASE_NO: u32 = 8;