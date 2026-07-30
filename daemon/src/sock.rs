//! Unix socket 控制面（双通道，同一套行协议）
//!   1. 文件 socket：/data/adb/sundown/sundownd.sock —— root 管理面（sunctl/WebUI）
//!   2. abstract socket：sundown_probe —— L1 桩 / L2 dex 层通道
//!      （/data/adb 为 drwx------ root，system_server 在 DAC 层不可达文件路径；
//!        abstract namespace 无文件系统路径，SELinux connectto ksu 已由 sepolicy 放行）
//!
//! 协议（L0/L1）：一行一个命令（UTF-8，\n 结尾），应答为一行 JSON。
//!   ping                 -> {"ok":1,"pong":1}
//!   status               -> 见 state::DaemonState::status_json()
//!   reload-config        -> 触发一次 conf/ 重载（与 inotify 自动热加载等价）
//!   stop                 -> 优雅退出（service.sh 看门狗会按策略重启）
//!   hello-probe <hash>   -> L1 桩上报 build hash（见 probe_response，含记录副作用）
//!   probe-query          -> 同 hello-probe 的应答，但只查询不记录（L2 dex 层轮询用）
//!
//! L2 扩展点：push-dex（probe.dex 推送），协议保持行分隔，新增命令即可，不破坏兼容。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::state::DaemonState;
use crate::{loge, logi, logw, paths};

/// 绑定 Linux abstract namespace socket（sun_path[0] = '\0' + name）。
/// std 的高级 abstract API 平台归属（std::os::linux vs android）易踩坑，
/// 直接用 libc 手工构造，glibc 主机与 bionic 均可编译。
fn bind_abstract(name: &str) -> std::io::Result<UnixListener> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = name.as_bytes();
        if bytes.len() > addr.sun_path.len() - 1 {
            libc::close(fd);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "abstract socket 名过长",
            ));
        }
        // sun_path[0] 保持 0（abstract 标志），名字从 [1] 开始
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr() as *const libc::c_char,
            addr.sun_path.as_mut_ptr().add(1),
            bytes.len(),
        );
        let len = (std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len()) as libc::socklen_t;
        if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, len) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        if libc::listen(fd, 16) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(UnixListener::from_raw_fd(fd))
    }
}

pub fn serve(state: Arc<DaemonState>, shutdown: Arc<AtomicBool>) -> std::io::Result<()> {
    // 清理陈旧 socket 文件
    let _ = std::fs::remove_file(paths::SOCKET_PATH);

    let listener = UnixListener::bind(paths::SOCKET_PATH)?;
    // root:root 0660 —— sunctl（root）管理面；桩不走这里（DAC 不可达），见 abstract 通道
    unsafe {
        let c_path = std::ffi::CString::new(paths::SOCKET_PATH).unwrap();
        libc::chmod(c_path.as_ptr(), 0o660);
    }
    // 非阻塞 + 轮询超时，便于响应 shutdown 标志
    listener.set_nonblocking(true)?;
    logi!("控制 socket 已监听: {}", paths::SOCKET_PATH);

    // L1 桩 / L2 dex 通道：abstract namespace socket（无文件路径，system_server 可直连）
    let probe_listener = bind_abstract(paths::PROBE_ABSTRACT_SOCK)?;
    probe_listener.set_nonblocking(true)?;
    logi!("探针 socket 已监听（abstract）: @{}", paths::PROBE_ABSTRACT_SOCK);

    while !shutdown.load(Ordering::Relaxed) {
        let mut idle = true;
        for l in [&listener, &probe_listener] {
            match l.accept() {
                Ok((stream, _)) => {
                    idle = false;
                    state.bump_connections();
                    let st = Arc::clone(&state);
                    std::thread::spawn(move || {
                        if let Err(e) = handle_conn(stream, st) {
                            logw!("连接处理异常: {}", e);
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    loge!("accept 失败: {}", e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
        if idle {
            std::thread::sleep(std::time::Duration::from_millis(100));
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
    // 命令与参数：首个空格分隔（hello-probe <hash>）
    let mut parts = line.trim().splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();

    let resp = match cmd {
        "ping" => "{\"ok\":1,\"pong\":1}".to_string(),
        "status" => state.status_json(),
        "reload-config" => {
            crate::config::request_reload(&state);
            "{\"ok\":1,\"reloaded\":1}".to_string()
        }
        "hello-probe" => {
            if arg.is_empty() {
                "{\"ok\":0,\"error\":\"hello-probe requires <build_hash>\"}".to_string()
            } else {
                state.record_probe(arg);
                logi!(
                    "探针桩握手: hash={}（期望: {}）",
                    arg,
                    state.expected_hash().as_deref().unwrap_or("<无 probe.hash>")
                );
                probe_response(&state)
            }
        }
        "probe-query" => probe_response(&state),
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

/// hello-probe / probe-query 的统一应答：
/// 期望 hash 比对结果 + probe.dex 路径与存在性（L1 桩据此决定是否加载 dex）
fn probe_response(state: &DaemonState) -> String {
    let expected = state.expected_hash();
    let reported = state.probe.lock().unwrap().as_ref().map(|p| p.build_hash.clone());
    // hash_match 三态：1=匹配，0=不匹配，-1=无期望值可比（dev 场景）
    let hash_match = match (&expected, &reported) {
        (Some(e), Some(r)) if e == r => 1,
        (Some(_), Some(_)) => 0,
        _ => -1,
    };
    let expected_json = match &expected {
        Some(h) => format!("\"{}\"", h),
        None => "null".to_string(),
    };
    let dex_present = std::path::Path::new(paths::PROBE_DEX).exists();
    format!(
        "{{\"ok\":1,\"hash_match\":{},\"expected_hash\":{},\"dex_path\":\"{}\",\"dex_present\":{}}}",
        hash_match,
        expected_json,
        paths::PROBE_DEX,
        dex_present as i32,
    )
}