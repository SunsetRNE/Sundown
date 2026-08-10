# Sundown 故障排查指南：第三方 App 卡死 / ANR

> 面向：模块分发用户遇到「刷了 Sundown 后某个 App 卡死 / ANR / 闪退」的归因排查。
> 目标：用证据快速区分「App 自身问题」与「模块干预问题」，避免误判与无效反馈。
> 维护：2026-08-11（联通 ANR 实机案例沉淀，v0.4.55-l3）。

## 0. 为什么需要这份文档

Sundown 是系统级调度模块，用户刷入后如果恰好某个 App 出现 ANR/卡死，**时间上容易产生归因联想**（"装了你模块才坏的"）。但 ANR 的根因绝大多数在 App 自身（启动过载/版本回归/广告 SDK），模块只是"恰好在场"。

本指南给出**证据化的三步判断法**，任何一方（作者 / 用户 / 反馈渠道）都能用 5 分钟得出结论。

## 1. 核心原理：Sundown 的干预边界（先建立认知）

| 事实 | 说明 |
|---|---|
| **不注入第三方 App 进程** | L1/L2 探针（Zygisk + LSPlant）只注入 **system_server**（焦点/豁免/防御 hook）；第三方 App 进程内部**没有任何 Sundown 代码**，模块无法直接卡住 App 主线程 |
| **冻结操作仅作用于显式冻结集** | 只有进入 `frozen` 表的包才会被 cgroup 冻结（uid 级）；白名单 / exempt / IMPORTANT / critical / 系统组件 / VPN 硬豁免一律不参与 |
| **一切操作可审计** | 每次冻结 / 解冻 / 豁免 / 丢弃 / force-stop 清理都留痕（日志 + events.jsonl），可精确到包名与时间 |
| **默认观望模式** | 发布默认 `enabled=false`——不主动冻结任何 App，零干预 |

## 2. 快速判断三步法

### 第一步：时间线对照（30 秒）

```sh
# App 最近更新时间 vs 首次出问题时间
dumpsys package <包名> | grep -E 'firstInstallTime|lastUpdateTime'

# Sundown 模块安装/生效时间
cat /data/adb/sundown/logs/*/install-time
cat /data/adb/sundown/logs/*/effective-since
```

- App **更新后**才开始 ANR、且更新前（模块已装）无问题 → 指向 **App 版本回归**（模块在更新前就装着，若模块是元凶更新前就该出问题）
- App 从未更新、模块安装后立刻出问题 → 才需要进入第二步细查

### 第二步：Sundown 审计（1 分钟）

```sh
# 该 App 在 Sundown 侧的全部操作记录（空 = 模块对该 App 零干预）
grep <包名> /data/adb/sundown/logs/<版本>/<日期>/sundownd.log

# 结构化事件（freeze / unfreeze / discard / force-stop 清理）
sunctl events 50

# 当前冻结集（该 App 不在 = 未冻结）
sunctl status | grep -iE '冻结|frozen'
```

- 关键判据：**该 App 没有任何 `freeze` / `discard` 记录** = 模块从未冻结/丢弃它，卡死与模块无直接因果
- 注意：日志中的 `L3 force-stop 清理` 是**响应**系统 force-stop 事件（清理自身状态），**不是**模块主动杀 App——模块没有主动 force-stop 的能力（只有 `discard` = SIGKILL 且受白名单/exempt 终检护栏约束，日志 reason 可区分）

### 第三步：App 自身证据（2 分钟，root）

```sh
# ANR trace（最近一次卡死的直接证据）
ls -lt /data/anr/ | head -5
head -3 /data/anr/anr_* | grep -E 'Subject|Rss'    # 原因 + 峰值内存

# 卡死时 App 的线程数与 RSS（线程爆炸/内存过载是 App 侧特征）
grep -c '^sysTid' /data/anr/anr_*                   # 线程数（正常 App < 80，爆炸可到 300+）
grep RssHwmKb /data/anr/anr_*                       # 峰值 RSS（>1GB 属异常膨胀）

# 冷启动耗时实测
monkey -p <包名> -c android.intent.category.LAUNCHER 1 && sleep 10 && pgrep -f <包名>
```

**App 侧特征（与模块无关的铁证）**：
- `Subject: Input dispatching timed out` + 进程 uptime < 30s → **冷启动主线程过载**（App 自身初始化重）
- 线程数 200+ / RSS > 1GB → 启动架构爆炸（广告 SDK / 多框架初始化）
- 内置广告 SDK 高频唤醒（如 `com.bytedance.openadsdk` 广播）→ 内存与唤醒大户

### 排除法（最终定论）

KSU 管理器 → 模块列表 → 停用 Sundown → 重启 → 冷启动该 App：
- **仍 ANR** = 与模块无关实锤（可放心向 App 厂商反馈）
- 停用后正常 = 再按第二步审计确认模块具体干预了什么（理论上不应有任何干预记录）

## 3. 典型案例：中国联通 App（2026-08-11，v0.4.55-l3）

> 完整案例档案，可作为排查示范与"模块背锅"反例。

| 证据 | 数值/内容 | 判读 |
|---|---|---|
| ANR Subject | `Input dispatching timed out`（触摸/焦点 5s 无响应） | App 主线程无响应 |
| 进程 uptime | 14s（冷启动即 ANR） | 冷启动初始化过载 |
| RSS 峰值 | 1.24GB（匿名内存 758MB） | 启动内存爆炸 |
| 线程数 | 300+（正常 < 80） | 启动架构爆炸 |
| 磁盘数据 | 仅 66MB（cache 206K） | 非缓存膨胀，清缓存无用 |
| Sundown 审计 | 0 冻结 / 0 丢弃（白名单 07:05 生效） | 模块零干预 |
| 时间线 | App 07-25 更新 12.14.1 → 首次 ANR 08-11；模块 08-08 安装，07-25~08-11 间无 ANR | 指向 App 版本回归 |
| 广告 SDK | `com.bytedance.openadsdk`（穿山甲）高频广播唤醒 | 内存/唤醒大户 |

**结论**：App 自身冷启动过载（12.14.1 版本回归疑似），与 Sundown 无直接因果。缓解：隐藏 ANR 弹窗（`settings put global hide_error_dialogs 1`）、白名单保进程常驻、或降级 App 版本。

## 4. 责任边界与建议

- **Sundown 侧**：白名单/豁免机制已保证"被管 App 不受冻结干预"；若确认模块 bug（有冻结/丢弃记录但本应豁免）→ 提交 issue 附 `sunctl events` 输出与日志
- **App 侧**：反馈时附 ANR trace 路径（`/data/anr/anr_*`）+ Subject 行 + RSS/线程数——厂商可据此定位启动链路
- **用户侧**：先按本指南三步自查，避免误伤模块作者，也让 App 厂商拿到有效证据

## 5. 证据采集速查卡（复制即用）

```sh
PKG=<包名>
echo "== 时间线 =="
dumpsys package $PKG | grep -E 'firstInstallTime|lastUpdateTime'
cat /data/adb/sundown/logs/*/install-time
echo "== Sundown 审计 =="
grep $PKG /data/adb/sundown/logs/*/*/sundownd.log | tail -20
echo "== ANR 证据 =="
ls -lt /data/anr/ | head -5
head -3 /data/anr/anr_* | grep -E 'Subject|RssHwm|RssKb'
grep -c '^sysTid' /data/anr/anr_*
```
