# Sundown 命名规范（定稿）

> 决策者：SunsetREN
> 状态：**已定稿，模块 id 一经发布不再变更**
> 本文档为 Sundown 项目源码库内的唯一权威副本（原工作区根目录副本已迁移至此）

---

## 一、核心命名

| 项 | 值 | 说明 |
|---|---|---|
| 显示名 | **Sundown** | 副标题：日落而息 · 墓碑调度 |
| 模块 id | `sundown` | 全小写，对应 `/data/adb/modules/sundown/` |
| 作者署名 | `SunsetREN` | module.prop `author` 字段、WebUI 关于页、daemon 日志头统一露出 |
| 守护进程 | `sundownd` | 沿用 Unix daemon 命名习惯（cerberusd → sundownd） |
| 控制 CLI | `sunctl` | WebUI 唯一后端入口：`sunctl status / reload-probe / restart-daemon / restart-runtime / apply-update` |
| 数据目录 | `/data/adb/sundown/` | conf/ data/ logs/ update/ 子目录结构沿用现有 Cerberus 布局 |
| Zygisk 探针桩 (L1) | `libsunprobe.so` | 稳定 ABI，只做 socket 收发 + dex 加载 |
| 探针逻辑 (L2) | `probe.dex` | daemon 经 socket 推送，InMemoryDexClassLoader 加载，支持热切换 |
| 包名风格 | `ren.sunset.sundown` | 需要 Java/Android 包名时使用 |

## 二、命名隐喻（功能命名可据此延展）

- **Sundown** = 日落而息：应用退至后台即"太阳落山"，进程安眠（冻结）
- 可延展的 feature 命名：
  - **黄昏模式 (Dusk Mode)** —— 严格冻结时段/策略
  - **黎明预热 (Dawn Preheat)** —— 解冻时预热关键工作集
  - **余晖 (Afterglow)** —— 冻结后保留的快速恢复能力

## 三、边界与硬约束

### 1. 模块 id 边界（最高优先级）

- id `sundown` 必须匹配 `^[a-zA-Z][a-zA-Z0-9._-]+$`，保持全小写
- **发布后永不更改**：KSU 按 id 识别同一模块，改名 = 老用户设备上变成两个模块
- 安装/升级前必须检测 `/data/adb/modules/` 下无同名冲突（KSU 同名会覆盖安装）
- 显示名（module.prop `name`、WebUI 标题）不受此限，随时可调

### 2. 旧资产切割边界（AStop/Cerberus → Sundown）

- 保留迁移逻辑：`post-fs-data.sh` 检测旧目录 `/data/adb/cerberus/`（及 AStop 遗留）自动迁移配置/数据/日志到 `/data/adb/sundown/`
  - 现有脚本已有"旧版本文件自动迁移"成熟写法（move_if_missing 模式），照搬即可
- 二进制更名映射：`cerberusd` → `sundownd`，`cerberusctl` → `sunctl`
- staged 更新机制（pending/boot_id/SHA-256 校验/回滚）整体沿用，仅路径改名
- 迁移完成后旧目录保留只读备份一轮启动周期，确认无回滚需求再清理

### 3. 依赖边界

- Sundown 与 Zygisk 提供方（ReZygisk）是**两个独立模块**，互不捆绑
- WebUI 环境自检页需检测：ReZygisk 模块存在且 monitor 存活，缺失时明确提示而非静默失败
- 兼容 KSU 各分支（KernelSU / KernelSU Next / SukiSU 等），不针对单一分支写死逻辑

### 4. 热更新分层边界（命名与层级绑定）

| 层 | 资产 | 更新方式 | 重启要求 |
|---|---|---|---|
| L3 | 策略/配置（TOML） | daemon inotify 热加载 | 无感 |
| L2 | `probe.dex` | socket 推送 + ClassLoader 热切换，失败回滚上一版 | 无感 |
| L1 | `libsunprobe.so` | 软重启 Zygote（`setprop ctl.restart zygote`），hash 校验闭环 | 热重启（按钮触发） |
| L0 | `sundownd` | staged 更新 + 看门狗重启（沿用 service.sh 机制） | 无感 |

- L1 桩设计约束：**几乎不允许更新**，所有逻辑下沉 L2；L1 更新失败回退 staged 通道，下次开机挂载前落盘

### 5. WebUI 边界

- WebUI = 纯静态 `webroot/`，只通过 `ksu.exec` 调 `sunctl`，**不持有任何直接文件/进程操作能力**
- WebUI 不可用时调度功能必须完全不受影响（解耦底线）
- 软重启按钮必须二次确认，文案写清"将结束所有应用进程（状态由系统保存，不丢数据）"

---

## 附：命名检查清单（发布前过一遍）

- [ ] `module.prop`：id=sundown，author=SunsetREN，name/description 定稿
- [ ] 全仓库 grep 无 `cerberus` / `astop` 残留（迁移逻辑除外）
- [ ] `/data/adb/sundown/` 目录结构与旧布局映射表已写入迁移脚本注释
- [ ] WebUI 关于页：Sundown + SunsetREN + 版本号 + LICENSE
- [ ] 主流模块仓库检索无同名 `sundown` 模块
