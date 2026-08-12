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
//! 协议（B2 事件订阅注册表，v0.7-l3，订阅连接上行命令，只增不改）：
//!   subscribe [kinds=<a,b>] [packages=<x,y>] -> 声明事件兴趣（按需分发替代全量广播）；
//!                           未声明 = 全量收（旧 dex 兼容零风险）；无参 = 重置全量
//!   subscribe query        -> 返回当前过滤器 JSON
//!   subscribe clear        -> 等价无参（重置全量）
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

use crate::events::{EvAction, EvLevel};
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
        state.engine.lock().unwrap().events.push_system(
            EvLevel::Report,
            EvAction::System,
            Some("dex_handshake"),
            Some(&format!("version={}", arg)),
        );
        let resp = dex_hello_response(&state);
        writer.write_all(resp.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        // 登记订阅 + 读循环；EOF/错误注销（daemon 重启后 dex 侧自动重连重登记）
        let id = state.register_dex_client(writer.try_clone()?);
        logi!("dex 事件订阅已建立 (id={}, version={})", id, arg);
        let r = dex_subscription_loop(&mut reader, &mut writer, &state, id);
        state.unregister_dex_client(id);
        logi!("dex 事件订阅断开 (id={})", id);
        return r;
    }

    let resp = match cmd {
        "ping" => "{\"ok\":1,\"pong\":1}".to_string(),
        "status" => state.status_json(),
        "events" => {
            // L3.1 结构化事件缓冲（只读；管理面 + abstract 面均可查）
            // events [n]：最近 n 条（最旧→最新）；缺省/0 = 全部
            let limit = arg.parse::<usize>().unwrap_or(0);
            state.engine.lock().unwrap().events.to_json(limit)
        }
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
                state.engine.lock().unwrap().events.push_system(
                    EvLevel::Report,
                    EvAction::System,
                    Some("probe_handshake"),
                    Some(&format!("hash={}", arg)),
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
        // ---- B3 声明式规则引擎（v0.8-l3）：rules list | rules status ----
        "rules" => {
            if !mgmt {
                "{\"ok\":0,\"error\":\"rules is management-channel only\"}".to_string()
            } else {
                handle_rules(&state, arg)
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
/// EOF 返回由调用方注销。id = 订阅注册表登记 id（subscribe 命令按 id 更新过滤器）。
fn dex_subscription_loop(
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
    state: &DaemonState,
    id: u64,
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
                    state.engine.lock().unwrap().events.push_system(
                        EvLevel::Report,
                        EvAction::System,
                        Some("dex_reregister"),
                        Some(&format!("version={}", arg)),
                    );
                    dex_hello_response(state)
                }
            }
            // ---- B2 事件订阅注册表（v0.7-l3）：声明兴趣，按需分发 ----
            "subscribe" => handle_subscribe(state, id, arg),
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
                    state.engine.lock().unwrap().events.push_system(
                        EvLevel::Report,
                        EvAction::System,
                        Some("bridge_report"),
                        Some(&format!("hash={}", arg)),
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
                // L3.1 结构化事件：前台切换（open）；与 on_focus 同锁，避免二次加锁
                {
                    let mut eng = state.engine.lock().unwrap();
                    eng.events.push_app(
                        EvLevel::Event,
                        EvAction::Open,
                        &pkg,
                        Some("focus"),
                        None,
                    );
                    eng.on_focus(&pkg, fg, media);
                }
                "{\"ok\":1}".to_string()
            }
            None => "{\"ok\":0,\"error\":\"event focus requires pkg=\"}".to_string(),
        },
        "wakeup" => {
            state.bump_wakeup();
            // 广播风暴防护：只计数，每 32 条才落一条日志/事件
            let n = state.wakeup_events.load(std::sync::atomic::Ordering::Relaxed);
            if n % 32 == 1 {
                let reason = kv("reason").unwrap_or_else(|| "?".to_string());
                logi!(
                    "唤醒事件: pkg={} reason={}（累计 {} 条）",
                    kv("pkg").unwrap_or_else(|| "?".to_string()),
                    reason,
                    n
                );
                // L3.1 结构化事件：唤醒观测（open + reason；节流与日志同频）
                if let Some(pkg) = kv("pkg") {
                    state.engine.lock().unwrap().events.push_app(
                        EvLevel::Event,
                        EvAction::Open,
                        &pkg,
                        Some("wakeup"),
                        Some(&reason),
                    );
                }
            }
            // L3：冻结中 → 解冻 + 冷却（防唤醒失效）；v0.4.42-l3 携带唤醒源供节流判定，
            // v0.4.43-l3 携带广播 action 供 Receiver gate 门控（可选，缺省 "?"）；
            // v0.4.44-l3 观测增强：日志带 action（action 仅在 gate 命中时落盘事件，
            // 平时无观测点——日志补全运行时可见性）
            if let Some(pkg) = kv("pkg") {
                let src = kv("reason").unwrap_or_else(|| "?".to_string());
                let action = kv("action").unwrap_or_else(|| "?".to_string());
                if action != "?" {
                    logi!("唤醒事件(action): pkg={} src={} action={}", pkg, src, action);
                }
                state.engine.lock().unwrap().on_wakeup(&pkg, &src, &action);
            }
            "{\"ok\":1}".to_string()
        }
        "exempt" => match kv("pkg") {
            // L3：dex 豁免判定监视器上行（fg/media/loc，独立线程判定，2s 节拍；
            // v0.4.20-l3 起新增 loc=定位 AppOps 判定，旧 dex 不携带 → 缺省 false）
            Some(pkg) => {
                let fg = kv_bool("fg");
                let media = kv_bool("media");
                let loc = kv_bool("loc");
                logi!("豁免判定: {}（fg={} media={} loc={}）", pkg, fg, media, loc);
                state.engine.lock().unwrap().on_exempt(&pkg, fg, media, loc);
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

/// B2 事件订阅注册表（v0.7-l3）：subscribe 命令处理。
/// 语法（空格分隔 kv，只增不改）：
///   subscribe                  -> 重置全量（默认，旧 dex 兼容）
///   subscribe clear            -> 同无参
///   subscribe query            -> 返回当前过滤器 JSON
///   subscribe kinds=<a,b> packages=<x,y> -> 声明兴趣（缺省维度 = 全量）
/// 未知 key 忽略（前向兼容：未来新增维度旧 dex 不解析）。
fn handle_subscribe(state: &DaemonState, id: u64, arg: &str) -> String {
    if arg.is_empty() || arg == "clear" {
        state.update_dex_subscription(id, crate::state::Subscription::default());
        logi!("dex 订阅重置全量 (id={})", id);
        return "{\"ok\":1,\"subscription\":\"all\"}".to_string();
    }
    if arg == "query" {
        return match state.dex_subscription(id) {
            Some(s) => format!(
                "{{\"ok\":1,\"kinds\":[{}],\"packages\":[{}]}}",
                s.kinds
                    .iter()
                    .map(|k| format!("\"{}\"", k))
                    .collect::<Vec<_>>()
                    .join(","),
                s.packages
                    .iter()
                    .map(|p| format!("\"{}\"", p))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            None => "{\"ok\":0,\"error\":\"subscription not found\"}".to_string(),
        };
    }
    match parse_subscription(arg) {
        Ok(sub) => {
            let updated = state.update_dex_subscription(id, sub);
            if updated {
                let kinds = if state.dex_subscription(id).map(|s| s.kinds.is_empty()).unwrap_or(true) {
                    "*".to_string()
                } else {
                    state
                        .dex_subscription(id)
                        .map(|s| s.kinds.join(","))
                        .unwrap_or_default()
                };
                logi!("dex 订阅更新 (id={}): kinds={}", id, kinds);
                "{\"ok\":1,\"subscription\":\"updated\"}".to_string()
            } else {
                "{\"ok\":0,\"error\":\"subscription not found\"}".to_string()
            }
        }
        Err(e) => format!("{{\"ok\":0,\"error\":\"subscribe: {}\"}}", e),
    }
}

/// subscribe 参数解析（纯函数，可单测）：
/// `kinds=<a,b> packages=<x,y>` → Subscription；未知 key 忽略；空值段忽略。
/// 解析失败（无法识别的键值对）→ Err（格式错误提示，防静默吞错）。
fn parse_subscription(arg: &str) -> Result<crate::state::Subscription, String> {
    let mut kinds: Vec<String> = Vec::new();
    let mut packages: Vec<String> = Vec::new();
    let mut seen = false;
    for tok in arg.split_whitespace() {
        let Some((k, v)) = tok.split_once('=') else {
            return Err(format!("无法识别参数: {}", tok));
        };
        seen = true;
        let items: Vec<String> = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        match k {
            "kinds" => kinds = items,
            "packages" => packages = items,
            _ => {} // 未知 key：前向兼容忽略
        }
    }
    if !seen {
        return Err("缺少 kinds=/packages= 参数".to_string());
    }
    // 全空（kinds= packages=）→ 等价全量（默认语义）
    Ok(crate::state::Subscription { kinds, packages })
}

/// policy 管理命令（仅 root 管理面）：
///   policy status        -> 策略状态 JSON（enabled/revision/冻结表/grace/计数）
///   policy reload        -> 强制从磁盘重载（失败保留旧表）
///   policy preset list   -> 情景预设列表 + 当前生效
///   policy preset apply <name> -> 应用预设（内存覆盖 [general]，不动磁盘 policy.toml）
///   policy preset clear  -> 清除预设（重新加载磁盘 policy.toml 参数）
fn handle_policy(state: &DaemonState, arg: &str) -> String {
    let mut parts = arg.splitn(2, ' ');
    let sub = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    match sub {
        "preset" => handle_preset(state, rest),
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

/// policy preset 子命令（仅 root 管理面）：
///   preset list          -> {"ok":1,"presets":[{name,enabled,grace_seconds,...}],"active":"...","revision":N}
///   preset apply <name>  -> 应用预设（内存覆盖 [general]；未知预设返回 ok:0）
///   preset clear         -> 清除预设（回落磁盘 policy.toml 参数）
fn handle_preset(state: &DaemonState, arg: &str) -> String {
    let mut parts = arg.splitn(2, ' ');
    let sub = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("").trim();
    match sub {
        "list" => {
            let eng = state.engine.lock().unwrap();
            let active = eng.active_preset.clone().unwrap_or_default();
            // v0.4.17-l3 起携带参数摘要（WebUI 动态渲染；CLI 可读参数明细）
            let items: Vec<String> = eng
                .presets
                .names()
                .iter()
                .map(|n| {
                    let p = &eng.presets.presets[n];
                    format!(
                        concat!(
                            "{{",
                            "\"name\":\"{}\",",
                            "\"enabled\":{},",
                            "\"grace_seconds\":{},",
                            "\"cooldown_seconds\":{},",
                            "\"keep_fg_service\":{},",
                            "\"keep_media\":{}",
                            "}}"
                        ),
                        n,
                        p.enabled,
                        p.grace_seconds,
                        p.cooldown_seconds,
                        p.keep_fg_service,
                        p.keep_media
                    )
                })
                .collect();
            format!(
                "{{\"ok\":1,\"presets\":[{}],\"active\":\"{}\",\"revision\":{}}}",
                items.join(","),
                active,
                eng.presets.revision
            )
        }
        "apply" => {
            if name.is_empty() {
                return "{\"ok\":0,\"error\":\"preset apply 缺少预设名（用法: policy preset apply <name>）\"}"
                    .to_string();
            }
            let mut eng = state.engine.lock().unwrap();
            match eng.apply_preset(name) {
                Ok(()) => format!(
                    concat!(
                        "{{",
                        "\"ok\":1,",
                        "\"applied\":\"{}\",",
                        "\"enabled\":{},",
                        "\"grace_seconds\":{},",
                        "\"cooldown_seconds\":{},",
                        "\"keep_fg_service\":{},",
                        "\"keep_media\":{}",
                        "}}"
                    ),
                    name,
                    eng.policy.enabled,
                    eng.policy.grace_seconds,
                    eng.policy.cooldown_seconds,
                    eng.policy.keep_fg_service,
                    eng.policy.keep_media
                ),
                Err(e) => format!("{{\"ok\":0,\"error\":\"{}\"}}", e.replace('"', "'")),
            }
        }
        "clear" => {
            let mut eng = state.engine.lock().unwrap();
            eng.clear_preset();
            format!(
                "{{\"ok\":1,\"active\":\"{}\",\"enabled\":{},\"grace_seconds\":{}}}",
                eng.active_preset.clone().unwrap_or_default(),
                eng.policy.enabled,
                eng.policy.grace_seconds
            )
        }
        _ => "{\"ok\":0,\"error\":\"preset: 用法 list | apply <name> | clear\"}".to_string(),
    }
}

/// B3（v0.8-l3）rules 子命令（仅 root 管理面）：
///   rules list          -> {"ok":1,"count":N,"rules":["id1","id2",...]}（id 稳定排序）
///   rules status        -> {"ok":1,"count":N,"revision":R,"hits":H}
fn handle_rules(state: &DaemonState, arg: &str) -> String {
    let sub = arg.trim();
    let eng = state.engine.lock().unwrap();
    match sub {
        "list" => {
            let ids = eng.rules.ids();
            format!(
                "{{\"ok\":1,\"count\":{count},\"rules\":[{ids}]}}",
                count = ids.len(),
                ids = ids
                    .iter()
                    .map(|s| format!("\"{}\"", s))
                    .collect::<Vec<_>>()
                    .join(","),
            )
        }
        "status" => format!(
            "{{\"ok\":1,\"count\":{},\"revision\":{},\"hits\":{}}}",
            eng.rules.len(),
            eng.rules.revision,
            eng.rules.hits
        ),
        other => format!(
            "{{\"ok\":0,\"error\":\"rules: unknown subcommand: {}\"}}",
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
            let expected = state.expected_dex_hash();
            // v0.4.30-l3：字节源一致性熔断（2026-08-03 软重启事故根因之一）——
            // fetch-dex 读 root 侧字节源，若其 BuildInfo 版本 ≠ 模块期望（部署漏同步 root 侧 /
            // 软重启不跑 post-fs-data.sh），下发旧字节 → dex 换代后版本仍不匹配 →
            // 自愈死循环（实测每 6-7s 一次）→ 换代风暴引爆 lsplant SetClassStatus 空指针。
            // 不一致时拒绝下发（ok:0 + 详细 error），dex 侧保持旧代安全运行，杜绝风暴。
            let actual = extract_dex_version(&bytes);
            let consistent = match (&expected, &actual) {
                (Some(h), Some(a)) => *a == *h,
                (Some(_), None) => {
                    // v0.4.33-l3：解析失败打印诊断细节（dex 大小 + 头部 hex），
                    // 供排障区分"非 dex 文件 / 截断 / 构建信息格式异常"
                    let head: String = bytes
                        .iter()
                        .take(16)
                        .map(|b| format!("{:02x}", b))
                        .collect();
                    logw!(
                        "fetch-dex：dex 字节版本解析失败（放行，dex 侧防空转兜底）size={} head16={}",
                        bytes.len(),
                        head
                    );
                    true
                }
                (None, _) => true, // 模块无期望 hash（dev 场景）→ 放行
            };
            if !consistent {
                let resp = format!(
                    "{{\"ok\":0,\"error\":\"字节源版本与期望不一致 actual={} expected={}，请同步六位一体（含 root 侧）\"}}",
                    actual.as_deref().unwrap_or("<解析失败>"),
                    expected.as_deref().unwrap_or("<无>")
                );
                writer.write_all(resp.as_bytes())?;
                writer.write_all(b"\n")?;
                writer.flush()?;
                logw!(
                    "fetch-dex 熔断：root 侧字节源版本 {} ≠ 期望 {}（拒绝下发，防换代风暴）",
                    actual.as_deref().unwrap_or("<解析失败>"),
                    expected.as_deref().unwrap_or("<无>")
                );
                return Ok(());
            }
            let expected_json = match &expected {
                Some(h) => format!("\"{}\"", h),
                None => "null".to_string(),
            };
            let actual_json = match &actual {
                Some(a) => format!("\"{}\"", a),
                None => "null".to_string(),
            };
            let header = format!(
                "{{\"ok\":1,\"size\":{},\"expected_hash\":{},\"actual_hash\":{}}}",
                bytes.len(),
                expected_json,
                actual_json
            );
            writer.write_all(header.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.write_all(&bytes)?;
            writer.flush()?;
            logi!("fetch-dex: 已下发 {} 字节 (actual={})", bytes.len(), actual.as_deref().unwrap_or("?"));
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

/// v0.4.30-l3：从 dex 字节解析 BuildInfo 构建版本（CI 注入的 commit short sha，
/// 7 位小写 hex，与模块 probe.dex.hash 同语义同源）。
/// 轻量 DEX 格式解析（零依赖）：header.string_ids_size/off → 遍历 string_id →
/// 读 MUTF-8 字符串 → 匹配 7 位小写 hex 即返回。解析失败返回 None（调用方放行不误伤）。
pub(crate) fn extract_dex_version(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 0x70 || &bytes[0..4] != b"dex\n" {
        return None;
    }
    let u32_at = |off: usize| -> Option<u32> {
        if off + 4 > bytes.len() {
            return None;
        }
        Some(u32::from_le_bytes(bytes[off..off + 4].try_into().ok()?))
    };
    let string_ids_size = u32_at(0x38)? as usize;
    let string_ids_off = u32_at(0x3C)? as usize;
    for i in 0..string_ids_size {
        // v0.4.40-l3 修复：string_id_item 是 4 字节（string_data_off, uint），
        // 原 8 字节步长只遍历偶数索引——hash 落在奇数位时解析失败（2026-08-03 实机：
        // v0.4.36/v0.4.39 的 dex 自检误报"解析失败"，v0.4.38 的 hash 恰在偶数位才侥幸通过）。
        let id_off = string_ids_off.checked_add(i.checked_mul(4)?)?;
        let data_off = u32_at(id_off)? as usize;
        if data_off >= bytes.len() {
            continue;
        }
        // string_data_item: uleb128 utf16_size + MUTF-8 bytes + 0x00
        let mut p = data_off;
        let mut shift = 0u32;
        loop {
            if p >= bytes.len() {
                break;
            }
            let b = bytes[p];
            p += 1;
            shift += 7;
            if b & 0x80 == 0 || shift > 35 {
                break;
            }
        }
        let start = p;
        while p < bytes.len() && bytes[p] != 0 {
            p += 1;
        }
        if p > bytes.len() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(&bytes[start..p]) {
            let t = s.trim();
            // 7 位小写 hex（CI 注入格式）：数字 + 小写 a-f；排除大写（is_ascii_lowercase
            // 只匹配 'a'-'z'，数字返回 false，不能用于此判断）
            let is_short_sha = t.len() == 7
                && t.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
            if is_short_sha {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// push-dex（仅 root 管理面）：读 dex 文件 → 广播事件头行 + 字节帧给全部订阅者。
/// 无订阅者不算失败（notified=0：冷启动时桩会经 hello 应答拿到新 dex / dex 自愈）。
/// v0.4.30-l3：加字节源一致性熔断——广播的字节版本 ≠ 模块期望时拒绝推送
/// （防管理面换代也撞 SetClassStatus 竞态；与 fetch-dex 熔断同一事故根因）。
fn push_dex(state: &DaemonState, arg: &str) -> String {
    let path = if arg.is_empty() { paths::PROBE_DEX } else { arg };
    match std::fs::read(path) {
        Ok(bytes) => {
            let expected = state.expected_dex_hash();
            let actual = extract_dex_version(&bytes);
            let consistent = match (&expected, &actual) {
                (Some(h), Some(a)) => *a == *h,
                (Some(_), None) => true, // 解析失败放行（广播字节可能非 dex，管理面自查）
                (None, _) => true,
            };
            if !consistent {
                logw!(
                    "push-dex 熔断：字节源版本 {} ≠ 期望 {}（拒绝推送，防换代竞态）",
                    actual.as_deref().unwrap_or("<解析失败>"),
                    expected.as_deref().unwrap_or("<无>")
                );
                return format!(
                    "{{\"ok\":0,\"error\":\"字节源版本与期望不一致 actual={} expected={}，请同步六位一体（含 root 侧）\"}}",
                    actual.as_deref().unwrap_or("<解析失败>"),
                    expected.as_deref().unwrap_or("<无>")
                );
            }
            let expected_json = match &expected {
                Some(h) => format!("\"{}\"", h),
                None => "null".to_string(),
            };
            let actual_json = match &actual {
                Some(a) => format!("\"{}\"", a),
                None => "null".to_string(),
            };
            let header = format!(
                "{{\"event\":\"dex-push\",\"size\":{},\"expected_hash\":{},\"actual_hash\":{}}}\n",
                bytes.len(),
                expected_json,
                actual_json
            );
            let notified = state.broadcast_dex(header.as_bytes(), &bytes);
            logi!("push-dex: {} 字节 (actual={}) → 通知 {} 个订阅者", bytes.len(), actual.as_deref().unwrap_or("?"), notified);
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
    // v0.4.27-l3：应答携带 Sundown 冻结 uid 集（dex 初始化用；之后以 frozen-sync 增量更新）
    let frozen_uids_json = {
        let eng = state.engine.lock().unwrap();
        let uids = eng.sundown_frozen_uids();
        format!(
            "[{}]",
            uids.iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    // v0.4.48-l3：应答携带候选池 uid 集（dex 初始化用；之后以 candidate-sync 增量更新——
    // onSystemFreeze/HANS 冻结/杀进程拦截的判定依据）
    let candidate_uids_json = {
        let eng = state.engine.lock().unwrap();
        let uids = eng.sundown_candidate_uids();
        format!(
            "[{}]",
            uids.iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    format!(
        "{{\"ok\":1,\"dex_hash_match\":{},\"expected_dex_hash\":{},\"dex_path\":\"{}\",\"dex_present\":{},\"frozen_uids\":{},\"candidate_uids\":{}}}",
        hash_match,
        expected_json,
        paths::PROBE_DEX_MOUNT,
        dex_present as i32,
        frozen_uids_json,
        candidate_uids_json,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小 DEX：header + 1 个 string_id + 1 个字符串（CI 短 sha 格式）
    fn mini_dex_with_string(s: &[u8]) -> Vec<u8> {
        let mut dex = vec![0u8; 0x80 + s.len() + 1];
        dex[0..4].copy_from_slice(b"dex\n");
        dex[0x38..0x3C].copy_from_slice(&1u32.to_le_bytes()); // string_ids_size = 1
        dex[0x3C..0x40].copy_from_slice(&0x70u32.to_le_bytes()); // string_ids_off = 0x70
        dex[0x70..0x74].copy_from_slice(&0x78u32.to_le_bytes()); // data_off = 0x78
        dex[0x78] = s.len() as u8; // utf16_size（ASCII 单字节）
        dex[0x79..0x79 + s.len()].copy_from_slice(s);
        // 尾部自动为 0（终止符）
        dex
    }

    #[test]
    fn dex_version_extract_short_sha() {
        let dex = mini_dex_with_string(b"a672eff");
        assert_eq!(extract_dex_version(&dex).as_deref(), Some("a672eff"));
    }

    #[test]
    fn dex_version_reject_non_dex() {
        assert_eq!(extract_dex_version(b"not a dex file at all"), None);
        assert_eq!(extract_dex_version(&[]), None);
    }

    #[test]
    fn dex_version_reject_oversize_string() {
        let dex = mini_dex_with_string(b"abcdef1234567890"); // 非 7 位
        assert_eq!(extract_dex_version(&dex), None);
    }

    /// 构造 2 个 string_id 的最小 DEX（hash 放第二个，奇数索引）
    fn mini_dex_two_strings(first: &[u8], second: &[u8]) -> Vec<u8> {
        let off1 = 0x78 + 1 + first.len() + 1;
        let mut dex = vec![0u8; off1 + 1 + second.len() + 1];
        dex[0..4].copy_from_slice(b"dex\n");
        dex[0x38..0x3C].copy_from_slice(&2u32.to_le_bytes()); // string_ids_size = 2
        dex[0x3C..0x40].copy_from_slice(&0x70u32.to_le_bytes()); // string_ids_off = 0x70
        dex[0x70..0x74].copy_from_slice(&0x78u32.to_le_bytes()); // entry0 → 0x78
        dex[0x74..0x78].copy_from_slice(&(off1 as u32).to_le_bytes()); // entry1 → off1
        dex[0x78] = first.len() as u8; // uleb128 utf16_size（<128 单字节）
        dex[0x79..0x79 + first.len()].copy_from_slice(first);
        dex[0x79 + first.len()] = 0;
        dex[off1] = second.len() as u8;
        dex[off1 + 1..off1 + 1 + second.len()].copy_from_slice(second);
        dex
    }

    /// v0.4.40-l3 回归：hash 在第二个 string_id（奇数索引）也能解析——
    /// 旧实现 `i * 8` 步长只遍历偶数索引，此用例必然返回 None。
    #[test]
    fn dex_version_extract_second_string_id() {
        let dex = mini_dex_two_strings(b"com.example.Foo", b"a672eff");
        assert_eq!(extract_dex_version(&dex).as_deref(), Some("a672eff"));
    }

    #[test]
    fn dex_version_reject_uppercase_hex() {
        let dex = mini_dex_with_string(b"A672EFF"); // 大写 → 非 CI 注入格式
        assert_eq!(extract_dex_version(&dex), None);
    }

    // ---------------- B2 事件订阅注册表（v0.7-l3） ----------------

    use crate::state::Subscription;

    /// subscribe 参数解析：kinds + packages 双轴
    #[test]
    fn subscribe_parse_kinds_packages() {
        let s = parse_subscription("kinds=frozen-sync,candidate-sync packages=com.wechat,com.qq").unwrap();
        assert_eq!(s.kinds, vec!["frozen-sync", "candidate-sync"]);
        assert_eq!(s.packages, vec!["com.wechat", "com.qq"]);
    }

    /// subscribe 缺省维度 = 全量（kinds 空 / packages 空）
    #[test]
    fn subscribe_parse_partial_is_all() {
        let s = parse_subscription("kinds=dex-push").unwrap();
        assert_eq!(s.kinds, vec!["dex-push"]);
        assert!(s.packages.is_empty(), "缺省 packages = 全部包");
        let s2 = parse_subscription("packages=com.x").unwrap();
        assert!(s2.kinds.is_empty(), "缺省 kinds = 全部类型");
    }

    /// 未知 key 前向兼容忽略；空值段忽略；全空 = 全量语义
    #[test]
    fn subscribe_parse_unknown_key_and_empty() {
        let s = parse_subscription("kinds=frozen-sync level=2").unwrap();
        assert_eq!(s.kinds, vec!["frozen-sync"], "未知 key level= 前向兼容忽略");
        let s2 = parse_subscription("kinds=frozen-sync, packages=").unwrap();
        assert_eq!(s2.kinds, vec!["frozen-sync"]);
        assert!(s2.packages.is_empty());
        let s3 = parse_subscription("kinds= packages=").unwrap();
        assert!(s3.kinds.is_empty() && s3.packages.is_empty(), "全空 = 全量");
    }

    /// 格式错误（无 = 的 token）→ Err 防静默吞错
    #[test]
    fn subscribe_parse_rejects_bad_token() {
        assert!(parse_subscription("kinds").is_err());
        assert!(parse_subscription("kinds=frozen-sync garbage").is_err());
    }

    /// 匹配判定：kind 过滤
    #[test]
    fn subscription_match_kinds_filter() {
        let sub = Subscription {
            kinds: vec!["frozen-sync".to_string()],
            packages: Vec::new(),
        };
        assert!(sub.matches("frozen-sync", None));
        assert!(!sub.matches("candidate-sync", None));
        assert!(!sub.matches("dex-push", None));
        // 无 pkg= 事件不受包名过滤影响（frozen-sync 行只有 uid=）
        let sub2 = Subscription {
            kinds: Vec::new(),
            packages: vec!["com.wechat".to_string()],
        };
        assert!(sub2.matches("frozen-sync", None), "无 pkg 事件仅按 kinds 过滤");
    }

    /// 匹配判定：包名过滤（精确 + `.*` 前缀通配）
    #[test]
    fn subscription_match_packages_filter() {
        let sub = Subscription {
            kinds: Vec::new(),
            packages: vec!["com.wechat".to_string(), "com.tencent.*".to_string()],
        };
        assert!(sub.matches("any-kind", Some("com.wechat")));
        assert!(sub.matches("any-kind", Some("com.tencent.mm")));
        assert!(sub.matches("any-kind", Some("com.tencent.qq")));
        assert!(!sub.matches("any-kind", Some("com.other.app")));
        // 通配 `com.tencent.*` 不匹配裸前缀 `com.tencent`（须有子域名）
        assert!(!sub.matches("any-kind", Some("com.tencent")));
        // 默认全量：kinds/packages 都空 → 一切命中
        assert!(Subscription::default().matches("frozen-sync", Some("anything")));
    }

    /// pkg= 行内提取
    #[test]
    fn subscription_pkg_of_extract() {
        assert_eq!(Subscription::pkg_of("event frozen-sync uid=10001\n"), None);
        assert_eq!(
            Subscription::pkg_of("event wakeup pkg=com.wechat reason=broadcast\n"),
            Some("com.wechat")
        );
        assert_eq!(Subscription::pkg_of("event focus pkg=com.qq fg=1\n"), Some("com.qq"));
    }
}