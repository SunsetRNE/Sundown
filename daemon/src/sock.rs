//! Unix socket 控制面：/data/adb/sundown/sundownd.sock
//!
//! 协议（L0）：一行一个命令（UTF-8，\n 结尾），应答为一行 JSON。
//!   ping           -> {"ok":1,"pong":1}
//!   status         -> 见 state::DaemonState::status_json()
//!   reload-config  -> 触发一次 conf/ 重载（与 inotify 自动热加载等价）
//!   stop           -> 优雅退出（service.sh 看门狗会按策略重启）
//!
//! L1/L2 扩展点：握手命令 hello-probe（build hash 上报）、push-dex（probe.dex 推送），
//! 协议保持行分隔，新增命令即可，不破坏兼容。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::state::DaemonState;
use crate::{loge, logi, logw, paths};

pub fn serve(state: Arc<DaemonState>, shutdown: Arc<AtomicBool>) -> std::io::Result<()> {
    // 清理陈旧 socket 文件
    let _ = std::fs::remove_file(paths::SOCKET_PATH);

    let listener = UnixListener::bind(paths::SOCKET_PATH)?;
    // root:root 0660 —— sunctl（root）与未来 L1 探针（system_server，走 sepolicy 放行）可连
    unsafe {
        let c_path = std::ffi::CString::new(paths::SOCKET_PATH).unwrap();
        libc::chmod(c_path.as_ptr(), 0o660);
    }
    // 非阻塞 + 轮询超时，便于响应 shutdown 标志
    listener.set_nonblocking(true)?;
    logi!("控制 socket 已监听: {}", paths::SOCKET_PATH);

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                state.bump_connections();
                let st = Arc::clone(&state);
                std::thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, st) {
                        logw!("连接处理异常: {}", e);
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => {
                loge!("accept 失败: {}", e);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
    Ok(())
}

fn handle_conn(stream: UnixStream, state: Arc<DaemonState>) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(());
    }
    let cmd = line.trim();

    let resp = match cmd {
        "ping" => "{\"ok\":1,\"pong\":1}".to_string(),
        "status" => state.status_json(),
        "reload-config" => {
            crate::config::request_reload(&state);
            "{\"ok\":1,\"reloaded\":1}".to_string()
        }
        "stop" => {
            logi!("收到 stop 命令，优雅退出");
            writer.write_all(b"{\"ok\":1,\"stopping\":1}\n")?;
            crate::request_shutdown();
            return Ok(());
        }
        other => {
            logw!("未知命令: {}", other);
            format!("{{\"ok\":0,\"error\":\"unknown command: {}\"}}", other)
        }
    };

    writer.write_all(resp.as_bytes())?;
    writer.write_all(b"\n")?;
    Ok(())
}