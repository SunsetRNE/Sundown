//! sundownd - Sundown 守护进程 (L0)
//!
//! 职责（与 module/system/bin/sundownd 占位注释、Sundown/README.md 一致）：
//!   1. 启动后写 update/daemon.ready（含 release_no），供 service.sh readiness 校验
//!   2. Unix socket /data/adb/sundown/sundownd.sock 控制面（ping/status/reload-config/stop）
//!   3. inotify 监听 conf/ 实现 L3 配置热加载（L0 为计数+日志，策略解析后续接入）
//!
//! 用法：
//!   sundownd            前台运行（由 service.sh nohup 拉起）
//!   sundownd --version  打印版本（含 release_no，供 staged 更新元数据生成）

mod config;
mod engine;
mod events;
mod freezer;
mod logging;
mod network;
mod paths;
mod policy;
mod preset;
mod sock;
mod state;
mod toml;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use state::DaemonState;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// 供 sock::handle_conn 的 stop 命令触发全局退出
pub(crate) fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as usize);
        libc::signal(libc::SIGINT, on_signal as usize);
        // 忽略 SIGPIPE：socket 客户端断开不应杀死 daemon
        libc::signal(libc::SIGPIPE, libc::SIG_IGN as usize);
    }
}

fn ensure_dirs() -> std::io::Result<()> {
    for d in [
        paths::SUNDOWN_DIR,
        paths::CONF_DIR,
        paths::DATA_DIR,
        paths::LOG_DIR,
        paths::UPDATE_DIR,
        paths::PROBE_DIR,
        paths::PROBE_OAT_DIR,
    ] {
        std::fs::create_dir_all(d)?;
    }
    Ok(())
}

/// 写 update/daemon.ready：service.sh 比对 release_no 与 installed.json 是否一致
fn write_ready_marker() -> std::io::Result<()> {
    let content = format!(
        "{{\"version_name\":\"{}\",\"release_no\":{},\"pid\":{}}}",
        paths::VERSION_NAME,
        paths::RELEASE_NO,
        std::process::id()
    );
    std::fs::write(paths::READY_MARKER, content)
}

/// 初始化 update/installed.json（仅缺失时，v0.4.20-l3）：
/// 记录当前 daemon 版本，供 service.sh readiness 校验与 sunctl daemon_version 查询。
/// staged 更新激活时由 service.sh 原子替换该文件——daemon 绝不覆盖既有文件，
/// 避免破坏「installed.json(期望) vs daemon.ready(实际)」的回滚判定语义。
fn write_installed_meta() -> std::io::Result<()> {
    if std::path::Path::new(paths::INSTALLED_META).exists() {
        return Ok(());
    }
    let content = format!(
        "{{\"version_name\":\"{}\",\"release_no\":{}}}",
        paths::VERSION_NAME, paths::RELEASE_NO
    );
    std::fs::write(paths::INSTALLED_META, content)
}

fn remove_ready_marker() {
    let _ = std::fs::remove_file(paths::READY_MARKER);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("sundownd {} (release_no {}) by SunsetREN", paths::VERSION_NAME, paths::RELEASE_NO);
        return;
    }

    if let Err(e) = ensure_dirs() {
        eprintln!("FATAL: 创建数据目录失败: {}", e);
        std::process::exit(1);
    }
    // v0.4.20-l3：installed.json 初始化（仅缺失时写入；staged 激活由 service.sh 管理）
    if let Err(e) = write_installed_meta() {
        logw!("installed.json 初始化失败: {}（daemon_version 回落 sunctl VERSION）", e);
    }

    logi!("========================================");
    logi!("🌇 Sundown daemon v{} (release {}) starting, pid={}",
        paths::VERSION_NAME, paths::RELEASE_NO, std::process::id());
    logi!("日落而息 · 墓碑调度 — by SunsetREN");

    install_signal_handlers();

    let state = Arc::new(DaemonState::new());
    let shutdown = Arc::new(AtomicBool::new(false));

    // v0.4.22-l3：启动残留冻结清理——daemon 崩溃/重启后 cgroup.freeze 状态保留
    // （内核态），frozen 表却已清空；不清则残留冻结 app 切前台也不会被解冻
    // （"能打开但点击无响应"→ ANR 闪退，实机反馈）。启动即全量解冻，重新开始决策。
    {
        use crate::events::{EvAction, EvLevel};
        let n = freezer::thaw_all_residual();
        if n > 0 {
            logw!("启动残留冻结清理：解冻 {} 个 uid（上次退出异常遗留）", n);
            state.engine.lock().unwrap().events.push_system(
                EvLevel::Warn,
                EvAction::Unfreeze,
                Some("startup_thaw"),
                Some(&format!("{} uids", n)),
            );
        }
    }

    // L3.1 结构化事件：daemon 启动
    {
        use crate::events::{EvAction, EvLevel};
        state.engine.lock().unwrap().events.push_system(
            EvLevel::Report,
            EvAction::System,
            Some("daemon_start"),
            Some(&format!("v{} (release {})", paths::VERSION_NAME, paths::RELEASE_NO)),
        );
    }

    // L3 配置热加载监听线程
    {
        let st = Arc::clone(&state);
        std::thread::spawn(move || config::watch_conf(st));
    }

    // 控制 socket 服务线程
    let sock_handle = {
        let st = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            if let Err(e) = sock::serve(st, sd) {
                loge!("socket 服务异常退出: {}", e);
                request_shutdown();
            }
        })
    };

    // 等待 socket 就绪（最多 2 秒）后写 ready 标记
    // 单实例守护：若 serve 让位/失败（SHUTDOWN 已置位）→ 不写 ready，也不动活跃实例的标记
    let mut waited = 0;
    while waited < 20 && !std::path::Path::new(paths::SOCKET_PATH).exists() {
        std::thread::sleep(std::time::Duration::from_millis(100));
        waited += 1;
    }
    if SHUTDOWN.load(Ordering::Relaxed) {
        logw!("socket 服务未就绪（已有实例/异常），跳过 ready 标记");
    } else {
        match write_ready_marker() {
            Ok(_) => logi!("ready 标记已写入: {}", paths::READY_MARKER),
            Err(e) => loge!("ready 标记写入失败: {}（service.sh readiness 校验将失败并回滚）", e),
        }
    }

    // 主循环：响应退出标志（信号 / socket stop 命令）+ L3 策略引擎定时推进
    // v0.4.27-l3：每 tick 比较 Sundown 冻结集签名，变化即广播 frozen-sync 给 dex 订阅者
    // （dex 侧冻结集归属判定的权威源；区分"HANS 自己冻的"与"Sundown 冻的"）
    let mut last_frozen_sig = String::new();
    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            break;
        }
        // L3：grace 到期冻结 / 冷却清理 / 策略关闭全量解冻（300ms 节拍）
        let mut sig = String::new();
        {
            let mut eng = state.engine.lock().unwrap();
            eng.tick();
            let uids = eng.sundown_frozen_uids();
            if !uids.is_empty() {
                sig = uids
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
            }
        }
        if sig != last_frozen_sig {
            last_frozen_sig = sig.clone();
            let line = format!("event frozen-sync uid={}\n", sig);
            let n = state.broadcast_line(&line);
            logi!("frozen-sync 广播: [{}] → {} 订阅者", sig, n);
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    logi!("正在退出...");
    // L3.1 结构化事件：daemon 停止
    {
        use crate::events::{EvAction, EvLevel};
        state.engine.lock().unwrap().events.push_system(
            EvLevel::Report,
            EvAction::System,
            Some("daemon_stop"),
            None,
        );
    }
    shutdown.store(true, Ordering::Relaxed);
    remove_ready_marker();
    let _ = sock_handle.join();
    logi!("sundownd 已退出");
}