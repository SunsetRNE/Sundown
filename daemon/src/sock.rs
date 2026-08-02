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
//! 协议（L2 新增，行协议不变，只增不改）：
//!   hello-dex <version>  -> dex 层上报构建版本；应答后连接保持为事件订阅通道
//!                           （EOF/写失败注销；通道上仅支持 ping / 重复 hello-dex 重登记）
//!   fetch-dex            -> 拉取 dex 字节：应答头行 {"ok":1,"size":N,"expected_hash":...}
//!                           紧跟 N 字节原始 dex（独立短连接，用完即关）
//!   push-dex [path]      -> 【仅 root 管理面】读 dex 文件并向全部订阅连接推送
//!                           {"event":"dex-push","size":N,...} + N 字节（热切换触发）
//!
//! 协议（L2b 新增，订阅连接上的 dex→daemon 上行命令，只增不改）：
//!   report-bridge <hash> -> bridge（libsundownhook）上报 build hash；
//!                           应答 {"ok":1,"bridge_hash_match":1|0|-1}
//!   event focus pkg=<pkg>            -> 前台焦点切换（观测模式）
//!   event wakeup pkg=<pkg> reason=<broadcast|service|pendingintent> -> 唤醒入口命中
//!   event proc-add pid=<n> / proc-remove pid=<n> / force-stop pkg=<pkg> -> 进程生命周期
//!                           应答 {"ok":1}；未知 event 子类型容错 {"ok":1,"ignored":1}
//!
//! 二进制帧纪律：头行声明 size，紧随其后恰为 size 字节，无额外分隔符；
//! 客户端必须按字节精确读取（dex 侧 DaemonLink 自行分行，不用 BufferedReader）。

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

/// 尝试连接 abstract socket：成功 = 已有活跃 daemon 实例（单实例守护探测）。
/// abstract 是 daemon 的存活标志（文件 socket 可能因异常退出残留死文件）。
fn abstract_connect(name: &str) -> std::io::Result<()> {
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
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr() as *const libc::c_char,
            addr.sun_path.as_mut_ptr().add(1),
            bytes.len(),
        );
        let len = (std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len()) as libc::socklen_t;
        let r = libc::connect(fd, &addr as *const _ as *const libc::sockaddr, len);
        libc::close(fd);
        if r < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub fn serve(state: Arc<DaemonState>, shutdown: Arc<AtomicBool>) -> std::io::Result<()> {
    // 单实例守护：已有活跃 daemon（abstract 探针 socket 被监听）→ 本进程让位退出，
    // 绝不 remove/bind 抢占其 socket 路径（修复双实例互踩：watchdog 兜底拉起与
    // restart-daemon 竞争时，后启动者必须退出而非破坏活跃实例）
    if abstract_connect(paths::PROBE_ABSTRACT_SOCK).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "已有 sundownd 实例在运行（abstract 探测命中），本实例让位退出",
        ));
    }

    // 到这里才允许清理陈旧 socket 文件（探测失败 = 无活跃实例，残留必为死文件）
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
    let probe_listener = match bind_abstract(paths::PROBE_ABSTRACT_SOCK) {
        Ok(l) => l,
        Err(e) => {
            // 竞态窗口内 abstract 被抢占（极小概率）→ 让位：清理自己刚 bind 的文件
            // socket 路径后退出（活跃实例随后会重新 bind 文件 socket 路径）
            let _ = std::fs::remove_file(paths::SOCKET_PATH);
            return Err(e);
        }
    };
    probe_listener.set_nonblocking(true)?;
    logi!("探针 socket 已监听（abstract）: @{}", paths::PROBE_ABSTRACT_SOCK);

    while !shutdown.load(Ordering::Relaxed) {
        let mut idle = true;
        // 文件 socket = root 管理面（mgmt=true）；abstract = 桩/dex 通道（mgmt=false）
        for (l, is_mgmt) in [(&listener, true), (&probe_listener, false)] {
            match l.accept() {
                Ok((stream, _)) => {
                    idle = false;
                    state.bump_connections();
                    let st = Arc::clone(&state);
                    std::thread::spawn(move || {
                        if let Err(e) = handle_conn(stream, st, is_mgmt) {
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

fn handle_conn(stream: UnixStream, state: Arc<DaemonState>, mgmt: bool) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(());
    }
    // 命令与参数：首个空格分隔（hello-probe <hash> / hello-dex <version> / push-dex <path>）
    let mut parts = line.trim().splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();

    // hello-dex 特殊路径：应答后保持连接，转为事件订阅通道（push-dex 推送对象）
    if cmd == "hello-dex" {
        if arg.is_empty() {
            writer.write_all(b"{\"ok\":0,\"error\":\"hello-dex requires <build_version>\"}\n")?;
            return Ok(());
        }
        state.record_dex(arg);
        logi!(
            "探针 dex 握手: version={}（期望: {}）",
            arg,
            state.expected_dex_hash().as_deref().unwrap_or("<无 probe.dex.hash>")
        );
        let resp = dex_hello_response(&state);
        writer.write_all(resp.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        // 登记订阅 + 读循环；EOF/错误注销（daemon 重启后 dex 侧自动重连重登记）
        let id = state.register_dex_client(writer.try_clone()?);
        logi!("dex 事件订阅已建立 (id={}, version={})", id, arg);
        let r = dex_subscription_loop(&mut reader, &mut writer, &state);
        state.unregister_dex_client(id);
        logi!("dex 事件订阅断开 (id={})", id);
        return r;
    }

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
        "fetch-dex" => {
            // 头行 + 原始字节帧（无行尾分隔）；写完即关连接
            return serve_dex_bytes(&mut writer, &state);
        }
        "push-dex" => {
            if !mgmt {
                // 管理动作收敛 root 管理面（单一可审计入口），abstract 面仅桩/dex 消费
                "{\"ok\":0,\"error\":\"push-dex is management-channel only\"}".to_string()
            } else {
                push_dex(&state, arg)
            }
        }
        "policy" => {
            // L3 策略管理（仅 root 管理面；abstract 面仅桩/dex 消费）
            if !mgmt {
                "{\"ok\":0,\"error\":\"policy is management-channel only\"}".to_string()
            } else {
                handle_policy(&state, arg)
            }
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

/// hello-dex 订阅连接的读循环：dex 侧只收事件 + L2b 起上行命令；
/// EOF 返回由调用方注销。
fn dex_subscription_loop(
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
    state: &DaemonState,
) -> std::io::Result<()> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // EOF：dex 侧主动断开（热切换换代 / 进程退出）
        }
        let mut parts = line.trim().splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        let resp = match cmd {
            "ping" => "{\"ok\":1,\"pong\":1}".to_string(),
            "hello-dex" => {
                if arg.is_empty() {
                    "{\"ok\":0,\"error\":\"hello-dex requires <build_version>\"}".to_string()
                } else {
                    state.record_dex(arg);
                    logi!("dex 重登记: version={}", arg);
                    dex_hello_response(state)
                }
            }
            // ---- L2b 上行命令（观测模式事件面） ----
            "report-bridge" => {
                if arg.is_empty() {
                    "{\"ok\":0,\"error\":\"report-bridge requires <build_hash>\"}".to_string()
                } else {
                    state.record_bridge(arg);
                    logi!(
                        "hook bridge 上报: hash={}（期望: {}）",
                        arg,
                        state.expected_hook_hash().as_deref().unwrap_or("<无 hook.hash>")
                    );
                    bridge_response(state)
                }
            }
            "event" => handle_event(state, arg),
            other => format!(
                "{{\"ok\":0,\"error\":\"subscription connection: unsupported command: {}\"}}",
                other
            ),
        };
        writer.write_all(resp.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
}

/// L2b/L3 事件分发：观测记录 + L3 策略引擎消费。
/// 未知子类型容错 {"ok":1,"ignored":1}——新旧版本滚动期间协议单向兼容。
fn handle_event(state: &DaemonState, arg: &str) -> String {
    let mut parts = arg.split_whitespace();
    let kind = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let kv = |key: &str| -> Option<String> {
        let prefix = format!("{}=", key);
        rest.iter()
            .find_map(|t| t.strip_prefix(prefix.as_str()).map(|v| v.to_string()))
    };
    let kv_u32 = |key: &str| -> Option<u32> {
        kv(key).and_then(|v| v.parse::<u32>().ok())
    };
    let kv_bool = |key: &str| -> bool {
        kv(key).map(|v| v == "1").unwrap_or(false)
    };
    match kind {
        "focus" => match kv("pkg") {
            Some(pkg) => {
                state.record_focus(&pkg);
                let fg = kv_bool("fg");
                let media = kv_bool("media");
                logi!(
                    "焦点切换: {}（fg={} media={}，累计 {} 次）",
                    pkg,
                    fg,
                    media,
                    state.focus_changes.load(std::sync::atomic::Ordering::Relaxed)
                );
                // L3：决策状态机消费
                state.engine.lock().unwrap().on_focus(&pkg, fg, media);
                "{\"ok\":1}".to_string()
            }
            None => "{\"ok\":0,\"error\":\"event focus requires pkg=\"}".to_string(),
        },
        "wakeup" => {
            state.bump_wakeup();
            // 广播风暴防护：只计数，每 32 条才落一条日志
            let n = state.wakeup_events.load(std::sync::atomic::Ordering::Relaxed);
            if n % 32 == 1 {
                logi!(
                    "唤醒事件: pkg={} reason={}（累计 {} 条）",
                    kv("pkg").unwrap_or_else(|| "?".to_string()),
                    kv("reason").unwrap_or_else(|| "?".to_string()),
                    n
                );
            }
            // L3：冻结中 → 解冻 + 冷却（防唤醒失效）
            if let Some(pkg) = kv("pkg") {
                state.engine.lock().unwrap().on_wakeup(&pkg);
            }
            "{\"ok\":1}".to_string()
        }
        "exempt" => match kv("pkg") {
            // L3：dex 豁免判定监视器上行（fg/media，独立线程判定，2s 节拍）
            Some(pkg) => {
                let fg = kv_bool("fg");
                let media = kv_bool("media");
                logi!("豁免判定: {}（fg={} media={}）", pkg, fg, media);
                state.engine.lock().unwrap().on_exempt(&pkg, fg, media);
                "{\"ok\":1}".to_string()
            }
            None => "{\"ok\":0,\"error\":\"event exempt requires pkg=\"}".to_string(),
        },
        "proc-add" => {
            // L3 进程表：pid→pkg 索引（uid 缺失时经 /proc/<pid>/status 兜底）
            match (kv_u32("pid"), kv("pkg")) {
                (Some(pid), Some(pkg)) => {
                    let uid = kv_u32("uid").or_else(|| crate::freezer::pid_uid(pid));
                    state.engine.lock().unwrap().on_proc_add(pid, &pkg, uid);
                    "{\"ok\":1}".to_string()
                }
                _ => "{\"ok\":0,\"error\":\"event proc-add requires pid= and pkg=\"}".to_string(),
            }
        }
        "proc-remove" => match kv_u32("pid") {
            Some(pid) => {
                state.engine.lock().unwrap().on_proc_remove(pid);
                "{\"ok\":1}".to_string()
            }
            None => "{\"ok\":0,\"error\":\"event proc-remove requires pid=\"}".to_string(),
        },
        "force-stop" => match kv("pkg") {
            Some(pkg) => {
                state.engine.lock().unwrap().on_force_stop(&pkg);
                "{\"ok\":1}".to_string()
            }
            None => "{\"ok\":0,\"error\":\"event force-stop requires pkg=\"}".to_string(),
        },
        _ => format!("{{\"ok\":1,\"ignored\":1,\"kind\":\"{}\"}}", kind),
    }
}

/// policy 管理命令（仅 root 管理面）：
///   policy status  -> 策略状态 JSON（enabled/revision/冻结表/grace/计数）
///   policy reload  -> 强制从磁盘重载（失败保留旧表）
fn handle_policy(state: &DaemonState, arg: &str) -> String {
    match arg {
        "status" => {
            let eng = state.engine.lock().unwrap();
            let frozen = eng.frozen_packages();
            let grace = eng.grace_pending();
            let frozen_json = format!(
                "[{}]",
                frozen.iter().map(|s| format!("\"{}\"", s)).collect::<Vec<_>>().join(",")
            );
            let grace_json = format!(
                "[{}]",
                grace.iter().map(|s| format!("\"{}\"", s)).collect::<Vec<_>>().join(",")
            );
            format!(
                concat!(
                    "{{",
                    "\"ok\":1,",
                    "\"enabled\":{enabled},",
                    "\"revision\":{rev},",
                    "\"grace_seconds\":{grace_s},",
                    "\"cooldown_seconds\":{cool_s},",
                    "\"whitelist\":{wl},",
                    "\"force\":{force},",
                    "\"frozen_packages\":{frozen},",
                    "\"grace_pending\":{grace},",
                    "\"freeze_ops\":{freeze_ops},",
                    "\"unfreeze_ops\":{unfreeze_ops},",
                    "\"wakeup_thaws\":{thaws},",
                    "\"last_focus\":{focus}",
                    "}}"
                ),
                enabled = eng.policy.enabled,
                rev = eng.policy.revision,
                grace_s = eng.policy.grace_seconds,
                cool_s = eng.policy.cooldown_seconds,
                wl = format!(
                    "[{}]",
                    eng.policy
                        .whitelist
                        .iter()
                        .map(|s| format!("\"{}\"", s))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                force = format!(
                    "[{}]",
                    eng.policy
                        .force
                        .iter()
                        .map(|s| format!("\"{}\"", s))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                frozen = frozen_json,
                grace = grace_json,
                freeze_ops = eng.freeze_ops,
                unfreeze_ops = eng.unfreeze_ops,
                thaws = eng.wakeup_thaws,
                focus = eng
                    .last_focus
                    .as_ref()
                    .map(|p| format!("\"{}\"", p))
                    .unwrap_or_else(|| "null".to_string()),
            )
        }
        "reload" => {
            let mut eng = state.engine.lock().unwrap();
            eng.reload_policy();
            format!(
                "{{\"ok\":1,\"enabled\":{},\"revision\":{}}}",
                eng.policy.enabled, eng.policy.revision
            )
        }
        other => format!(
            "{{\"ok\":0,\"error\":\"policy: unknown subcommand: {}\"}}",
            other
        ),
    }
}

/// report-bridge 应答：期望 hash 比对三态（1=匹配，0=不匹配，-1=无期望值可比）
fn bridge_response(state: &DaemonState) -> String {
    let expected = state.expected_hook_hash();
    let reported = state
        .hook_bridge
        .lock()
        .unwrap()
        .as_ref()
        .map(|b| b.build_hash.clone());
    let hash_match = match (&expected, &reported) {
        (Some(e), Some(r)) if e == r => 1,
        (Some(_), Some(_)) => 0,
        _ => -1,
    };
    format!("{{\"ok\":1,\"bridge_hash_match\":{}}}", hash_match)
}

/// fetch-dex：读 canonical 字节源（root 专属路径），头行 + 原始字节帧应答。
/// 客户端（桩冷启动自愈 / dex 层版本落后自愈）按 size 精确读取后自行关闭连接。
fn serve_dex_bytes(writer: &mut UnixStream, state: &DaemonState) -> std::io::Result<()> {
    match std::fs::read(paths::PROBE_DEX) {
        Ok(bytes) => {
            let expected_json = match state.expected_dex_hash() {
                Some(h) => format!("\"{}\"", h),
                None => "null".to_string(),
            };
            let header = format!(
                "{{\"ok\":1,\"size\":{},\"expected_hash\":{}}}",
                bytes.len(),
                expected_json
            );
            writer.write_all(header.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.write_all(&bytes)?;
            writer.flush()?;
            logi!("fetch-dex: 已下发 {} 字节", bytes.len());
        }
        Err(e) => {
            let resp = format!(
                "{{\"ok\":0,\"error\":\"read {} failed: {}\"}}",
                paths::PROBE_DEX,
                e
            );
            writer.write_all(resp.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
    Ok(())
}

/// push-dex（仅 root 管理面）：读 dex 文件 → 广播事件头行 + 字节帧给全部订阅者。
/// 无订阅者不算失败（notified=0：冷启动时桩会经 hello 应答拿到新 dex / dex 自愈）。
fn push_dex(state: &DaemonState, arg: &str) -> String {
    let path = if arg.is_empty() { paths::PROBE_DEX } else { arg };
    match std::fs::read(path) {
        Ok(bytes) => {
            let expected_json = match state.expected_dex_hash() {
                Some(h) => format!("\"{}\"", h),
                None => "null".to_string(),
            };
            let header = format!(
                "{{\"event\":\"dex-push\",\"size\":{},\"expected_hash\":{}}}\n",
                bytes.len(),
                expected_json
            );
            let notified = state.broadcast_dex(header.as_bytes(), &bytes);
            logi!("push-dex: {} 字节 → 通知 {} 个订阅者", bytes.len(), notified);
            format!(
                "{{\"ok\":1,\"notified\":{},\"size\":{},\"dex_path\":\"{}\"}}",
                notified,
                bytes.len(),
                path
            )
        }
        Err(e) => format!("{{\"ok\":0,\"error\":\"read {} failed: {}\"}}", path, e),
    }
}

/// hello-dex 应答：期望版本比对结果 + 冷启动兜底 dex 路径（magic-mount，uid 1000 可读）
fn dex_hello_response(state: &DaemonState) -> String {
    let expected = state.expected_dex_hash();
    let reported = state.dex.lock().unwrap().as_ref().map(|d| d.build_version.clone());
    // dex_hash_match 三态：1=匹配，0=不匹配（dex 侧据此 fetch-dex 自愈），-1=无期望值可比
    let hash_match = match (&expected, &reported) {
        (Some(e), Some(r)) if e == r => 1,
        (Some(_), Some(_)) => 0,
        _ => -1,
    };
    let expected_json = match &expected {
        Some(h) => format!("\"{}\"", h),
        None => "null".to_string(),
    };
    let dex_present = std::path::Path::new(paths::PROBE_DEX_MOUNT).exists();
    format!(
        "{{\"ok\":1,\"dex_hash_match\":{},\"expected_dex_hash\":{},\"dex_path\":\"{}\",\"dex_present\":{}}}",
        hash_match,
        expected_json,
        paths::PROBE_DEX_MOUNT,
        dex_present as i32,
    )
}

/// hello-probe / probe-query 的统一应答：
/// 期望 hash 比对结果 + probe.dex 路径与存在性（L1 桩据此决定是否加载 dex）。
/// 注意：dex_path 必须指向 uid 1000 可读的 magic-mount 路径（PROBE_DEX_MOUNT），
/// 绝不指向 /data/adb 下的 canonical 字节源（DAC 层 EACCES，L1 真机已实证）；
/// canonical 路径仅供 root 侧 fetch-dex / push-dex 读字节经 socket 下发。
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
    let dex_present = std::path::Path::new(paths::PROBE_DEX_MOUNT).exists();
    format!(
        "{{\"ok\":1,\"hash_match\":{},\"expected_hash\":{},\"dex_path\":\"{}\",\"dex_present\":{}}}",
        hash_match,
        expected_json,
        paths::PROBE_DEX_MOUNT,
        dex_present as i32,
    )
}