//! 极简日志：写 logs/<VERSION_NAME>/<YYYY-MM-DD>/sundownd.log（v0.4.53-l3 起按版本×日期归档），
//! 带时间戳。L0 阶段够用；后续可替换为环形缓冲/tracing。

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

use crate::paths;

static LOG_LOCK: Mutex<()> = Mutex::new(());

/// 本地时区分解（v0.4.33-l3 弃用简易 UTC 分解——排障需 +8 换算易错；
/// 复用 engine.rs 已验证的 libc::localtime_r 零依赖方案，时区随系统 TZ）
fn local_tm() -> Option<libc::tm> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return None;
        }
        Some(tm)
    }
}

fn now_stamp() -> String {
    match local_tm() {
        Some(tm) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        None => {
            // 失败兜底：epoch 秒（不带括号，外层 log() 统一加）
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_else(|_| "?".to_string())
        }
    }
}

/// 本地日期 YYYY-MM-DD（v0.4.53-l3：日志按天归档的日期键；每天 0:00 自动切换）
pub fn local_date() -> String {
    match local_tm() {
        Some(tm) => format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday),
        None => "1970-01-01".to_string(), // 失败兜底（时间不可用，归档键稳定即可）
    }
}

/// 惰性创建日志目录（logs/<version>/<date>/，跨天自动新建）——每天 0:00 后第一条日志落新目录
fn ensure_log_parent(path: &str) {
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
}

pub fn log(level: &str, msg: &str) {
    let line = format!("[{}] [{}] {}\n", now_stamp(), level, msg);
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // v0.4.53-l3：路径动态解析（运行中 daemon 自身版本 + 本地日期——版本归属天然正确）
    let path = paths::current_log_file();
    ensure_log_parent(&path);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
    // 同时输出到 stdout：service.sh 已重定向到 boot_watchdog.log
    print!("{}", line);
}

/// v0.4.53-l3：实际启动生效时间记录（开机校验）——
/// 写 logs/<VERSION_NAME>/effective-since（epoch + 人类可读）；目录缺失自动补建。
/// install-time（刷入时间）缺失时一并补写（手动替换二进制场景），保证版本文件夹完整。
pub fn write_effective_since() {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let content = format!("{} {}\n", ts, now_stamp());
    let ver_dir = paths::version_log_dir();
    let _ = std::fs::create_dir_all(&ver_dir);
    let _ = std::fs::write(paths::effective_since_file(), &content);
    if !std::path::Path::new(&paths::install_time_file()).exists() {
        let _ = std::fs::write(paths::install_time_file(), &content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.4.53-l3：local_date 严格 YYYY-MM-DD（日志按天归档的目录键）
    #[test]
    fn local_date_format_v053() {
        let d = local_date();
        let parts: Vec<&str> = d.split('-').collect();
        assert_eq!(parts.len(), 3, "格式应为 YYYY-MM-DD: {}", d);
        assert_eq!(parts[0].len(), 4, "年 4 位: {}", d);
        assert_eq!(parts[1].len(), 2, "月 2 位: {}", d);
        assert_eq!(parts[2].len(), 2, "日 2 位: {}", d);
        assert!(parts[0].chars().all(|c| c.is_ascii_digit()));
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()));
        assert!(parts[2].chars().all(|c| c.is_ascii_digit()));
    }

    /// v0.4.53-l3：日志路径层级 = logs/<version>/<date>/sundownd.log
    #[test]
    fn log_path_hierarchy_v053() {
        let p = paths::current_log_file();
        assert!(p.starts_with(&format!("{}/", paths::LOG_DIR)), "应以 logs/ 开头: {}", p);
        assert!(p.contains(&format!("/{}/", paths::VERSION_NAME)), "应含版本目录: {}", p);
        assert!(p.ends_with("/sundownd.log"), "应以 sundownd.log 结尾: {}", p);
        // 日期段 = local_date（与归档键一致）
        let mid = p.trim_start_matches(&format!("{}/{}/", paths::LOG_DIR, paths::VERSION_NAME));
        let day = mid.trim_end_matches("/sundownd.log");
        assert_eq!(day, local_date(), "日期段应等于本地日期: {}", p);

        let e = paths::current_event_file();
        assert!(e.ends_with("/events.jsonl"));
        let v = paths::version_log_dir();
        assert_eq!(v, format!("{}/{}", paths::LOG_DIR, paths::VERSION_NAME));
        assert!(paths::install_time_file().ends_with("/install-time"));
        assert!(paths::effective_since_file().ends_with("/effective-since"));
    }
}

#[macro_export]
macro_rules! logi {
    ($($arg:tt)*) => { $crate::logging::log("INFO", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! logw {
    ($($arg:tt)*) => { $crate::logging::log("WARN", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! loge {
    ($($arg:tt)*) => { $crate::logging::log("ERROR", &format!($($arg)*)) };
}