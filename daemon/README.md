# sundownd（Rust）

Sundown 守护进程的 L0 最小实现。依赖仅 `libc`（JSON 手写、inotify 直调 syscall），
交叉编译链最简单，产物体积小（release 约数百 KB）。

## L0 已实现职责

| 职责 | 实现 | 对应文件 |
|---|---|---|
| ready 标记 | 启动后写 `update/daemon.ready`（含 `release_no`），退出时删除；供 service.sh staged 更新 readiness 校验 | `main.rs` |
| Unix socket 控制面 | `/data/adb/sundown/sundownd.sock`（0660 root:root），行协议：`ping` / `status` / `reload-config` / `stop` | `sock.rs` |
| L3 配置热加载 | inotify 监听 `conf/`，`.toml/.json` 变更计数 + 日志（策略解析 TODO 接入点已留） | `config.rs` |
| status 契约 | 输出字段兼容 `docs/sunctl-spec.md`（只增不改），追加 `uptime_s` / `config_reloads` / `connections_served` | `state.rs` |
| 优雅退出 | SIGTERM/SIGINT + socket `stop` 命令；SIGPIPE 忽略 | `main.rs` |

## 构建（Android 目标）

```bash
# 一次性准备
rustup target add aarch64-linux-android
# 需要 Android NDK，并配置 linker（二选一）：
#   A) cargo-ndk:  cargo install cargo-ndk
#   B) 手动在 ~/.cargo/config.toml 配置 [target.aarch64-linux-android] linker

# 构建
cd daemon
cargo ndk -t arm64-v8a --platform 30 build --release   # 方式 A（推荐，API 30 对齐模块最低要求）
# 注意：cargo-ndk v4 起 -p 表示 cargo 包选择，API 级别必须用 --platform

# 产物拷入模块
cp target/aarch64-linux-android/release/sundownd ../module/system/bin/sundownd
chmod 755 ../module/system/bin/sundownd
```

> 注意：拷入真实二进制后，`module/system/bin/sundownd` 占位文件即被覆盖。
> 32 位设备支持（可选）：`rustup target add armv7-linux-androideabi`，
> 模块需同时携带对应 ABI 的二进制并按需启动（L0 暂不覆盖）。

## 版本与 staged 更新的约定

- `paths.rs` 中 `RELEASE_NO` **单调递增，只加不改**：service.sh 用它比对
  `installed.json.release_no` 与 `daemon.ready.release_no` 判定新版本是否就绪。
- 发新版流程（沿用 Cerberus staged 通道）：
  1. 新二进制 + `installed.json.new`（含 `version_name` / `release_no`）+
     `pending.sha256` + `pending.json`（含 `staged_boot_id`）放入
     `/data/adb/sundown/update/pending/`
  2. 重启后 service.sh 校验 SHA-256 → 激活 → 启动 → 校验 ready 标记的
     `release_no` 一致 → 通过；失败自动回滚 `backup/sundownd.previous`

## 手动冒烟测试（设备上，root）

```sh
# 直接运行（不经过 service.sh）
/data/adb/modules/sundown/system/bin/sundownd &

# socket 协议验证
echo ping   | nc -U /data/adb/sundown/sundownd.sock   # {"ok":1,"pong":1}
echo status | nc -U /data/adb/sundown/sundownd.sock   # status JSON
echo reload-config | nc -U /data/adb/sundown/sundownd.sock
echo stop   | nc -U /data/adb/sundown/sundownd.sock   # 优雅退出

# 热加载验证：修改 conf/ 下任意 .toml，观察 logs/sundownd.log
```

（设备无 `nc` 时可用 `toybox nc` 或后续把 `sunctl status` 切换到 socket 数据源。）

## L1/L2 扩展点（已在代码中预留）

- `sock.rs`：`hello-probe`（探针握手 + build hash 上报，软重启验证闭环）、
  `push-dex`（probe.dex 字节流推送）——行分隔协议新增命令即可
- `state.rs`：status JSON 的 `probe_stub_loaded` / `probe_dex_version` 占位字段
- `config.rs`：`request_reload()` 与 inotify 回调共用入口，TOML 解析在此接入