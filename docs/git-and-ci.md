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
     → Nightly 滚动 Release（单层 zip，主下载渠道）
     → Artifact 留档（套娃 zip，仅 CI 调试用）/ 手动建 Release 时自动附加 zip
```

**本机零工具链要求**：不需要 NDK、不需要 Rust，push 即编译。

### Action 版本基线（2026-07-30 核实）

GitHub 已弃用 Node 20 运行时（声明 Node 20 的 action 会被强制按 Node 24
执行并在 Annotations 告警）。workflow 已统一升到声明 Node 24 的大版本：

| Action | 版本 | 备注 |
|---|---|---|
| actions/checkout | `@v7`（v7.0.1） | |
| actions/upload-artifact | `@v7`（v7.0.1） | |
| actions/download-artifact | `@v8`（v8.0.1） | |
| softprops/action-gh-release | `@v3`（v3.0.2） | 官方 Node 24 pin |
| dtolnay/rust-toolchain | `@stable` | 无运行时告警 |
| taiki-e/install-action | `@v2` | 无运行时告警 |

升级原则：只跟随 major tag，不锁 patch；若 Actions 页面再出现运行时弃用
告警，先查对应 action 的 latest release 再 bump，不盲目升级。

### 首次推送步骤（Termux 执行）

```sh
# 1. 在 GitHub 网页创建空仓库 Sundown（不勾 README/LICENSE，避免首次 push 冲突）
#    或在 Termux 装 gh:  pkg install gh && gh auth login && gh repo create Sundown --private

# 2. 工作区仓库路径（Termux 可直接访问 Android 共享存储以外的应用沙箱时需要 root；
#    建议先把仓库复制到 Termux 私有目录再推送）
cp -r /data/user/0/com.ai.assistance.operit/files/workspace/48e38deb-6e79-44a7-9543-0f0f7760d88d/Sundown ~/Sundown
cd ~/Sundown

# 3. 绑定远端并推送（ssh config 已配 443，无需额外处理）
git remote add origin git@github.com:SunsetRNE/Sundown.git
git push -u origin main

# 4. 推送后约 1~3 分钟，Actions 产出:
#    - Artifact: sundownd-aarch64（裸二进制）
#    - Artifact: sundown-module（可直接刷入的模块 zip）
```

### 发布版本

日常构建无需任何操作：push main 后 CI 自动滚动更新 **Nightly 预发布**
（`releases/tag/nightly`，asset 为单层模块 zip，KSU 直接刷入）。

```sh
# 正式发版（方式一，推荐）：网页上 Draft Release → 选 tag（如 v0.1.0-l0）→ 发布
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
| 真机刷入 | Nightly Release 下载单层模块 zip → KSU 管理器本地安装 |

### Artifact 产物形态（为什么"压缩包里还有压缩包"）

GitHub Artifacts 的硬性行为：**任何 artifact 下载到本地都是一层 zip 容器**。
因此两个 artifact 的实际结构是：

```
sundown-module.zip          ← artifact 容器（下载所得）
└── sundown-vX.Y.Z.zip      ← 真正的 KSU 模块包（刷入用这个）

sundownd-aarch64.zip        ← artifact 容器
└── sundownd                ← 裸二进制
```

边界：
- 套娃由 artifact 机制决定，与 upload-artifact v4/v7 无关，无法关闭
- **KSU 管理器本地安装时必须选内层的 `sundown-vX.Y.Z.zip`**
- **日常下载走 Nightly 滚动 Release**（每次 push main 自动更新，asset 为单层 zip）：
  `https://github.com/SunsetRNE/Sundown/releases/tag/nightly`
- 正式发版手动建 Release（tag 如 v0.1.0-l0），同样单层；Artifacts 仅作 CI 调试留档

## 边界

- `daemon/target/`、模块 zip 不入库（.gitignore 已定）
- `module/system/bin/sundownd` 文本占位文件**保持入库**：CI 只在工作区覆盖为二进制，
  不提交该变更（保证任何时刻 clone 下来的模块脚本可运行、CI 注入点明确）
- 远端仓库名建议与模块 id 一致：`Sundown`（显示名），git remote 名用 `origin`
