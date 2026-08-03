//! 极简日志：写 /data/adb/sundown/logs/sundownd.log，带时间戳。
//! L0 阶段够用；后续可替换为环形缓冲/tracing。

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

use crate::paths;

static LOG_LOCK: Mutex<()> = Mutex::new(());

fn now_stamp() -> String {
    // 本地时区时间戳（v0.4.33-l3：弃用简易 UTC 分解——排障需 +8 换算易错；
    // 复用 engine.rs 已验证的 libc::localtime_r 零依赖方案，时区随系统 TZ）
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return now.to_string(); // 失败兜底：epoch 秒（不带括号，外层 log() 统一加）
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

pub fn log(level: &str, msg: &str) {
    let line = format!("[{}] [{}] {}\n", now_stamp(), level, msg);
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(paths::LOG_FILE) {
        let _ = f.write_all(line.as_bytes());
    }
    // 同时输出到 stdout：service.sh 已重定向到 boot_watchdog.log
    print!("{}", line);
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