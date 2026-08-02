# L3 推进计划：策略引擎（TOML 策略 + 冻结执行 + 豁免决策）

> 状态：📋 计划定稿（v0.4.0-l3 实施）｜ 决策：SunsetREN
> 前置：L0 ✅ L1 ✅ L2 ✅ L2b ✅（v0.3.4-l2 实机回归通过：hook 类可见性修复后
> FocusHooks 6 + WakeupHooks 4 = 10 hook 全量生效，焦点/唤醒事件真实上行）
> 本刀定位：**策略落地**——把 L2b 的"纯感知"升级为"感知 + 决策 + 执行"：
> TOML 策略热加载、退后台冻结（cgroup freezer）、豁免动作、唤醒解冻、防御 hook 组骨架。
>
> 铁律延续：**桩（probe.cpp）零触碰**；daemon 保持**仅依赖 libc**（TOML/JSON 手写）；
> dex 改动走 L2 热更新（push-dex 无感）；失败安全（坏配置保留旧表、冻结失败不拖垮 daemon）。

---

## 0. 关键裁决（真机取证，证据见 §附）

### 0.1 冻结执行通道：cgroup v2 freezer（uid 级）

真机（PJD110 / Android 16 / cgroup v2）取证：

| 路径 | 语义 | 写入 |
|---|---|---|
| `/sys/fs/cgroup/apps/uid_<uid>/cgroup.freeze` | **uid 级冻结**（整 app 全部进程） | `1` 冻结 / `0` 解冻 |
| `/sys/fs/cgroup/apps/uid_<uid>/pid_<pid>/cgroup.freeze` | pid 级冻结（单进程，备选） | 同上 |
| `/sys/fs/cgroup/system/uid_<uid>/...` | system 域（uid<10000 系统服务）——**不冻结** | — |

- **裁决**：默认按 **uid 级**冻结（墓碑语义 = 冻结整个 app；pid 级仅诊断/调试用）
- root 写权限已验证（daemon 为 root，直接写文件，无 SELinux 新面——cgroup 写归属
  root 域既有权限，真机实证 `echo 0 > uid_*/cgroup.freeze` 成功）
- **uid 来源**：`/data/system/packages.list`（root 可读，`pkg uid ...` 每行）——
  **pkg→uid 全量映射**（含未运行包），策略按 pkg 匹配后经此查 uid；进程表
  （proc-add 事件）作 pid→pkg 补充索引（唤醒解冻定位、冻结效果核验）

### 0.2 策略文件：conf/policy.toml（inotify 热加载，失败保留旧表）

- 沿用 config.rs 既有 inotify 监听（IN_MODIFY/CLOSE_WRITE/MOVED_TO/CREATE/DELETE），
  reload 回调接入 policy::rebuild()；**解析失败 → 保留旧策略表 + logcat/sundownd.log 留痕**（铁律）
- TOML 手写子集（零依赖，与 JSON 手写同哲学）：表头 `[a.b]`、`key = value`、
  字符串（双引号 + `\"` `\\` `\n` `\t` 转义）、布尔、整数、字符串数组、`#` 注释；
  未知键/段 → 警告不致命（前向兼容）

### 0.3 决策状态机：退后台 grace → 冻结；唤醒/前台 → 解冻

```
focus pkg=P（前台切换）
  ├─ P 在冻结表 → 解冻（用户切回）＋ 冷却窗口
  └─ 旧前台 Q 离开前台
       ├─ Q ∈ whitelist.packages → 豁免（不启动计时）
       ├─ Q ∈ freeze.force → 立即冻结
       └─ 否则 → 启动 grace 计时（general.grace_seconds）
            ├─ grace 到期仍非前台 → 冻结 Q（uid 级）
            └─ grace 内 focus 回来 / wakeup 命中 → 取消计时
wakeup pkg=P reason=...
  ├─ P ∈ 冻结表 → 解冻（防唤醒失效）＋ 冷却窗口（cooldown_seconds 内不再冻）
  └─ 记录唤醒统计（观测）
proc-add/remove → 维护 pkg→pid 索引（冻结执行后核验、force-stop 清理）
force-stop pkg=P → 清冻结记录 + 取消计时
```

- **冷却窗口**：解冻后 cooldown_seconds 内不重新冻结（防"解冻-立即再冻"抖动）
- **前台豁免**：last_focus_pkg 恒不冻结（引擎内部约束，无需配置）
- 定时驱动：daemon 主循环 tick（300ms）→ engine::tick() 检查 grace/冷却到期

### 0.4 豁免动作（首刀实现 keep_fg_service / keep_media 判定）

- 判定数据源（dex 侧，L2 热更新随本刀升级）：
  - `keep_fg_service`：AMS `ActiveServices` 前台服务状态——hook `realStartServiceLocked`
    已能感知服务启动；新增 `updateServiceConnectionLocked`/`bringDownServiceLocked`
    观测或经 `ActivityManager.getRunningServices`（hidden，dex 已 trusted 可反射）——
    **首刀用反射 `ActivityManager.getRunningServices` 周期性快照**（engine tick 内
    daemon 侧不行——dex 在 system_server 内才可查）→ **裁决：豁免判定放 dex 侧**，
    事件上行新增 `event fg-service pkg=P` / `event media pkg=P`（MediaSession 查询）
- 简化边界（首刀）：`keep_fg_service` / `keep_media` 由 dex 侧在 focus 离开时
  随 `event focus` 的判定字段上报（`event focus pkg=P fg=1 media=0`），daemon 决策
  时直接消费；dex 判定失败（反射异常）→ 字段缺省 0（宁可多冻，观测纠正）

### 0.5 防御 hook 组（L3 只装骨架，默认关闭）

- l2b-plan §1 第三组点位（CachedAppOptimizer.useFreezer / ANR 防护链）**本刀只解析
  `[defense]` 配置 + status 展示**，不装 hook——自冻语义稳定后再启用（L3.1）
- 真机执行冻结与系统 freezer 的相互作用：Android 16 LMKD 的 freezer 与我们的
  cgroup.freeze 同通道，冻结 app 后 LMKD 不再杀它（墓碑的省电 + 保活双收益）；
  **副作用观察项**：冻结 app 的 binder 调用会阻塞调用方（最多 500ms binder 超时）——
  观测 wakeup 解冻的响应延迟，必要时缩短 grace/提前解冻

## 1. daemon 改动（Rust，RELEASE_NO 6→7）

```
daemon/src/
├── toml.rs        # 【新增】极简 TOML 子集解析器（零依赖）
├── policy.rs      # 【新增】Policy 模型 + 解析/校验 + 重建（失败保留旧表）
├── freezer.rs     # 【新增】cgroup 冻结执行（pkg→uid 经 packages.list，写 cgroup.freeze）
├── engine.rs      # 【新增】策略引擎：事件消费 + 进程/包表 + grace/冷却 + 冻结表 + tick
├── config.rs      # 【改】reload 回调 → policy::rebuild + engine 通知；inotify 增量重载
├── sock.rs        # 【改】event 处理接入 engine（focus/wakeup/proc-*/force-stop）
├── state.rs       # 【改】status JSON 追加 L3 字段（契约只增不改）
└── paths.rs       # 【改】POLICY_FILE / PACKAGES_LIST 常量；VERSION 0.4.0-l3
```

协议扩展（只增不改，abstract 面事件行）：
| 命令 | 说明 | 应答 |
|---|---|---|
| `event focus pkg=P [fg=0\|1] [media=0\|1]` | 焦点切换（新增豁免判定字段，旧格式兼容） | `{"ok":1}` |
| `event wakeup pkg=P reason=...` | 唤醒命中（触发解冻决策） | `{"ok":1}` |
| `event proc-add pid=N pkg=P [uid=N]` / `proc-remove pid=N` / `force-stop pkg=P` | 进程生命周期 | `{"ok":1}` |
| `policy status` / `policy reload` | 【root 管理面】策略状态 / 强制重载 | JSON |

status JSON 新增（只增不改）：`policy_enabled` / `policy_revision`（策略文件 mtime）/
`frozen_packages`（数组）/ `freeze_ops` / `unfreeze_ops` / `grace_pending`

## 2. dex 改动（Java，L2 热更新随本刀）

- `FocusHooks`：focus 事件追加 `fg=`/`media=` 豁免判定字段（反射 AMS 前台服务 +
  MediaSession 快照；判定失败缺省 0）；proc-add 追加 uid（ProcessRecord.uid 反射，
  缺失时 daemon 从 /proc/<pid>/status 兜底）
- `WakeupHooks`：不变（事件已含 pkg/reason）
- 语法红线延续：禁 lambda/方法引用；hook 回调第一行 try-catch 全包

## 3. 模块 / 脚本 / WebUI

- `module/conf/policy.toml`：**默认策略文件**（安装时放置到 /data/adb/sundown/conf/，
  customize.sh 逻辑：存在则不覆盖——用户配置优先）
- `sunctl`：status 文本面 L3 行（策略开关/冻结列表/计数）；`policy status` 子命令
- `webroot/index.html`：L3 面板（策略开关、冻结列表、grace 倒计时）——本刀最小化：
  状态行 + 冻结列表只读展示，交互后置
- `sepolicy.rule`：**本刀不加**（cgroup 写 root 域既有权限，实证无 avc）
- 版本：`module.prop` `v0.4.0-l3` / `versionCode=10` / description 换 L3 文案；
  daemon `RELEASE_NO=7`；CI 三处防呆校验不变

## 4. 真机验证清单（按序执行）

- T1 TOML 解析：合法/非法/未知键/转义用例（本地 + 设备 policy status 回显）
- T2 热加载：改 policy.toml → 策略重建（revision 变更）；写入坏文件 → 保留旧表 + 留痕
- T3 pkg→uid：packages.list 解析 + 缓存失效重读
- T4 退后台冻结：启动测试 app → 回桌面 → grace 到期 → `cgroup.freeze=1`（读回实证）
  + `frozen_packages` 出现
- T5 白名单豁免：whitelist.packages 内 app 退后台 → 不冻结
- T6 前台豁免/解冻：冻结中 app 被切回 → 解冻（`cgroup.freeze=0` + 冷却窗口内不重冻）
- T7 唤醒解冻：`am broadcast` 测试包 → wakeup 命中冻结 app → 解冻 + 计数
- T8 force-stop：清理冻结记录 + 取消 grace
- T9 长稳：策略引擎 + 冻结/解冻循环 ≥2h，system_server 无 ANR/崩溃，binder 阻塞无异常
- T10 版本闭环：v0.4.0-l3 三处同步 + 四位一体 hash

## 5. 明确不做（留刀清单）

- ❌ 厂商 ROM 适配矩阵（MIUI Greeze / OPPO Hans / Vivo Freezer——L3+）
- ❌ 防御 hook 组启用（CachedAppOptimizer / ANR 防护链——L3.1，本刀仅配置+状态面）
- ❌ ProcessReceiverRecord 门禁（active receiver 数据源——L3.1 随防御组）
- ❌ WebUI 策略编辑交互（仅只读面板；编辑走手动改 conf/policy.toml）
- ❌ 桩（probe.cpp）任何改动
- ❌ daemon 引入第三方 crate（TOML/JSON 一律手写，仅依赖 libc）
- ❌ 32 位设备 / 多用户（user 0 单用户模型，包表按 user 0）

---

## 6. 实测事故与修订（2026-08-02 首刀真机，闪退事故复盘）

### 6.1 事故：L3 引擎误冻高频 app → binder 阻塞 ANR/闪退

- **现象**：daemon 0.4.0-l3 部署后约 15 分钟，用户反馈 app 闪退并重启手机。
- **铁证（sundownd.log）**：`com.ai.assistance.operit`（AI 助手自身）被冻结 6 次
  （冻结累计 11→16），每次冻结后立即"冻结记录清理（进程已退出）"；期间
  `unfreeze_ops=0`（从未解冻）。
- **根因链**（三个缺陷叠加）：
  1. **进程核验依赖 proc-add 事件索引（pkg_pids）**——当前 dex 未升级 proc-add
     上报，索引恒空 → 冻结记录写入瞬间被 tick 误判"进程已退出"清理 →
     **cgroup.freeze=1 实际生效但引擎无记录** → 用户切回 app 时无解冻路径 →
     binder 调用阻塞（>500ms 超时）→ ANR/进程被杀 = 闪退感
  2. **grace=10s 过短**：用户切走高频 app 专注其他 10s+ 即触发冻结
  3. **豁免字段缺失**：dex 侧 fg/media 未实现（缺省 0），无前台服务/媒体豁免

### 6.2 修复（daemon 侧，已真机部署验证）

| # | 修复 | 文件 |
|---|---|---|
| 1 | **冻结前核验 uid 进程**（cgroup.procs 内核级）：无进程不冻结 | freezer.rs `uid_has_procs` + engine.rs `freeze_now` |
| 2 | **冻结记录核验改 cgroup.procs**：废弃不可靠的 pkg_pids 索引 | engine.rs `tick` |
| 3 | **grace 离开即重置计时**：防"切回再离开"沿用旧时刻刚离开就到期；活跃用户频繁切换不误冻 | engine.rs `decide_leave` |
| 4 | **默认策略保守化**：enabled 缺省 false（发布默认观望）、grace 缺省 30s、cooldown 缺省 60s | policy.rs |
| 5 | **默认白名单补 `com.android.launcher`**（OPPO 桌面真名，非 launcher3）+ 开发环境 app（operit/resukisu） | module/conf/policy.toml |
| 6 | **单实例守护**：bind 前 abstract 探测，已有实例让位退出（修复 watchdog 与 restart-daemon 竞争双实例互踩 socket 路径） | sock.rs + main.rs |
| 7 | **TOML 多行数组 + 尾逗号容忍**（默认策略即多行格式） | toml.rs |

### 6.3 决策变更（相对 §0）

- **发布默认 enabled=false**（观望模式）：冻结是强副作用操作，用户确认无异常后再
  手动开启（policy.toml 注释已写明事故教训）
- **grace 默认 10s→30s**；**cooldown 默认 30s→60s**
- 引擎只管理自己的冻结记录（厂商 ColorOS 的 cgroup 冻结不干预、不覆盖）

### 6.4 遗留（正向前向链路待实证）

- T4/T6/T7 正向链路（退后台→冻结→读回→切回解冻→冷却）在用户活跃场景下
  grace 持续重置无法自然触发；需**用户安静 ≥grace 时长**或配合 am start 测试
- dex 侧 fg/media 豁免字段（防误冻关键，L2 热更新随下刀）仍为待办
- 冻结副作用观察项：冻结 app 的 binder 调用阻塞（最长 500ms 超时）——长稳观察中

---

## §附：真机取证存档（2026-08-02，PJD110 / Android 16）

| 证据 | 结果 |
|---|---|
| cgroup 树 | `/sys/fs/cgroup/{apps,system}`；`/proc/self/cgroup` = `0::/` |
| uid 级冻结文件 | `/sys/fs/cgroup/apps/uid_<uid>/cgroup.freeze`（实测存在，root 可写） |
| pid 级冻结文件 | `/sys/fs/cgroup/apps/uid_<uid>/pid_<pid>/cgroup.freeze`（实测存在） |
| 控制器 | freezer 经 cgroup.freeze 伪文件暴露（v2），无独立 freezer 控制器挂载 |
| 写权限 | `echo 0 > uid_10066/cgroup.freeze` 成功（root，无 avc） |
| 包表 | `/data/system/packages.list`（root 可读，`pkg uid` 行式） |
| 进程 cgroup 归属 | `com.oplus.statistics` → `/apps/uid_10094/pid_5915`（pid 级目录含 cgroup.procs） |
