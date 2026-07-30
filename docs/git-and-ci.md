# Git 与 CI 工作流（定稿）

> 环境事实（已核实）：
> - Termux 已配置 git 用户 `SunsetREN <z100o190zgxc@163.com>`、ed25519 SSH key
> - `~/.ssh/config` 已走 `ssh.github.com:443`（绕过 22 端口封锁），`ssh -T git@github.com` 已验证过
> - Termux **未安装** Rust / gh CLI
> - 本仓库 git 身份与 Termux 一致（local config）

## 主路线：GitHub Actions 云端编译 ✅（已配置）

仓库内 `.github/workflows/build.yml` 已实现完整链路：

```
push → build-daemon (rustup + cargo-ndk 交叉编译 aarch64, API 30)
     → package-module (注入二进制 → 修权限 → zip 打包)
     → Artifact 下载 / 手动建 Release 时自动附加 zip
```

**本机零工具链要求**：不需要 NDK、不需要 Rust，push 即编译。

### 首次推送步骤（Termux 执行）

```sh
# 1. 在 GitHub 网页创建空仓库 Sundown（不勾 README/LICENSE，避免首次 push 冲突）
#    或在 Termux 装 gh:  pkg install gh && gh auth login && gh repo create Sundown --private

# 2. 工作区仓库路径（Termux 可直接访问 Android 共享存储以外的应用沙箱时需要 root；
#    建议先把仓库复制到 Termux 私有目录再推送）
cp -r /data/user/0/com.ai.assistance.operit/files/workspace/48e38deb-6e79-44a7-9543-0f0f7760d88d/Sundown ~/Sundown
cd ~/Sundown

# 3. 绑定远端并推送（ssh config 已配 443，无需额外处理）
git remote add origin git@github.com:SunsetREN/Sundown.git
git push -u origin main

# 4. 推送后约 1~3 分钟，Actions 产出:
#    - Artifact: sundownd-aarch64（裸二进制）
#    - Artifact: sundown-module（可直接刷入的模块 zip）
```

### 发布版本

```sh
# 方式一（推荐）：网页上 Draft Release → 选 tag（如 v0.1.0-l0）→ 发布
#   workflow 的 release 事件触发，自动把模块 zip 附加到 Release
# 方式二：命令行（需 gh）
gh release create v0.1.0-l0 --title "Sundown v0.1.0-l0" --notes "L0 首个构建"
```

## 备选路线：Termux 本机编译（无网络/离线路线）

Termux 的 Rust 工具链**原生目标就是 aarch64-linux-android**（bionic），
不需要交叉配置，适合应急验证：

```sh
pkg install rust
cd ~/Sundown/daemon
cargo build --release
cp target/release/sundownd ../module/system/bin/sundownd
```

注意：
- Termux 产物动态链接 bionic，在 root shell 直接运行没问题
- 但 **release 分发仍以 CI 产物为准**（NDK API 30 对齐、strip 体积优化、可复现）
- Termux cargo 首次构建会拉 crates.io 依赖（本项目仅 libc，体积小）

## 日常开发循环

| 动作 | 位置 |
|---|---|
| 改代码/文档 | 工作区 `Sundown/`（Operit 附着工作区直接编辑） |
| 提交 | 工作区仓库（已 init，身份已配）或同步到 Termux 后提交 |
| 编译验证 | push 到 GitHub → Actions（主力）；或 Termux cargo build（应急） |
| 真机刷入 | 下载 sundown-module artifact → KSU 管理器本地安装 |

## 边界

- `daemon/target/`、模块 zip 不入库（.gitignore 已定）
- `module/system/bin/sundownd` 文本占位文件**保持入库**：CI 只在工作区覆盖为二进制，
  不提交该变更（保证任何时刻 clone 下来的模块脚本可运行、CI 注入点明确）
- 远端仓库名建议与模块 id 一致：`Sundown`（显示名），git remote 名用 `origin`
