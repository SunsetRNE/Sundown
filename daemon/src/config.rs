//! L3 配置热加载：inotify 监听 /data/adb/sundown/conf/。
//!
//! L0 行为：文件变更 -> 记录日志 + config_reloads 计数 +1。
//! 真正的 TOML 策略解析在策略引擎阶段接入本模块的 reload 回调。

use std::sync::Arc;

use crate::state::DaemonState;
use crate::{loge, logi, logw, paths};

/// 由 socket 的 reload-config 命令调用：与 inotify 路径共用同一个重载入口。
pub fn request_reload(state: &Arc<DaemonState>) {
    state.bump_config_reloads();
    logi!(
        "配置重载触发（手动），累计 {} 次",
        state.config_reloads.load(std::sync::atomic::Ordering::Relaxed)
    );
    // TODO(L3): 解析 conf/*.toml，重建策略表，失败保留旧表
}

/// inotify 监听线程入口（阻塞循环，由 main 起线程运行）。
pub fn watch_conf(state: Arc<DaemonState>) {
    unsafe {
        let fd = libc::inotify_init1(libc::IN_CLOEXEC);
        if fd < 0 {
            loge!("inotify_init1 失败，配置热加载不可用");
            return;
        }
        let mask = libc::IN_MODIFY | libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO
            | libc::IN_CREATE | libc::IN_DELETE;
        let c_conf = std::ffi::CString::new(paths::CONF_DIR).unwrap();
        let wd = libc::inotify_add_watch(fd, c_conf.as_ptr(), mask);
        if wd < 0 {
            loge!("inotify_add_watch({}) 失败", paths::CONF_DIR);
            libc::close(fd);
            return;
        }
        logi!("inotify 已监听 {}", paths::CONF_DIR);

        let mut buf = [0u8; 4096];
        loop {
            let len = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            if len <= 0 {
                // EINTR 等：继续；fd 异常则退出
                let err = *libc::__errno();
                if err == libc::EINTR {
                    continue;
                }
                loge!("inotify read 异常 (errno={})，监听线程退出", err);
                break;
            }
            // 解析事件，提取文件名用于日志
            let mut offset = 0usize;
            while offset < len as usize {
                let ev = &*(buf.as_ptr().add(offset) as *const libc::inotify_event);
                if ev.len > 0 {
                    // libc crate 的 inotify_event 未建模柔性数组成员 name[]，
                    // 需手动从结构体末尾偏移取 C 字符串
                    let name_ptr = buf
                        .as_ptr()
                        .add(offset + std::mem::size_of::<libc::inotify_event>())
                        as *const libc::c_char;
                    let name = std::ffi::CStr::from_ptr(name_ptr).to_string_lossy();
                    // 只关心策略文件，忽略临时文件噪音
                    if name.ends_with(".toml") || name.ends_with(".json") {
                        state.bump_config_reloads();
                        logi!(
                            "配置变更: {} (事件掩码 0x{:x})，热加载 #{}",
                            name,
                            ev.mask,
                            state.config_reloads.load(std::sync::atomic::Ordering::Relaxed)
                        );
                        // TODO(L3): 增量重载该文件
                    } else {
                        logw!("忽略非策略文件变更: {}", name);
                    }
                }
                offset += std::mem::size_of::<libc::inotify_event>() + ev.len as usize;
            }
        }
        libc::close(fd);
    }
}