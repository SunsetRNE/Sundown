# sunctl 命令规范（L0 定稿）

`sunctl` 是 Sundown 的**唯一管理入口**。WebUI（ksu.exec）、脚本、用户 shell 全部经由此 CLI 操作，保证单一可审计入口。

- 路径（模块挂载后）：`/data/adb/modules/sundown/system/bin/sunctl`，同时位于系统 PATH 的 `/system/bin/sunctl`
- 运行身份：root（KSU WebUI 的 `ksu.exec` 自动以 root 执行）
- L0 实现为 shell 脚本；后续可由 `sundownd` 内建子命令替代，**命令面与退出码必须保持兼容**

## 命令面

| 命令 | 参数 | 说明 | 退出码 |
|---|---|---|---|
| `status` | `[--json]` | 模块/守护进程/Zygisk 提供方状态（socket 优先，fs 降级） | 0=daemon 运行中；1=daemon 未运行 |
| `env-check` | — | 环境自检（KSU、Zygisk 提供方、文件完整性、迁移标记） | 0=通过；1=存在缺失项 |
| `restart-daemon` | — | 重启 sundownd（杀旧→启动→验证 PID） | 0=成功；1=失败 |
| `restart-runtime` | `--yes`（必需） | 软重启 zygote（`ctl.restart`）。无 `--yes` 时仅打印警告 | 0=已触发；2=未确认拒绝执行 |
| `reload-probe` | — | 【L2 已交付】经 daemon 管理面推送 probe.dex，运行中 dex 层 ClassLoader 热切换 | 0=成功（含 notified:0 的静默落地）；1=daemon 未连接/推送失败 |
| `apply-update` | — | 【后续交付】激活 staged 守护进程更新 | 3=当前阶段未实现 |
| `logs` | `[行数=50]` | 输出 boot_watchdog.log 末尾 | 0 |
| `logs` | `--engine [行数=100]` | 输出 sundownd.log 末尾（引擎事件时间轴：焦点/唤醒/豁免/冻结） | 0 |
| `events` | `[行数=50]` | 【L3.1】结构化事件时间线（JSON 数组，最旧→最新；字段见 §事件契约） | 0=成功；1=daemon 未连接；2=参数错误 |
| `policy` | `status` | 【L3】查询冻结策略状态（JSON，经 daemon 管理面 socket） | 0=成功；1=daemon 未连接；2=参数错误 |
| `policy` | `reload` | 【L3】强制从磁盘重载策略（失败保留旧表） | 同上 |
| `policy` | `preset list` | 【L3】情景预设列表 + 当前生效（JSON，action.toml） | 同上 |
| `policy` | `preset apply <name>` | 【L3】应用预设（内存覆盖 [general]，不动磁盘 policy.toml） | 同上 |
| `policy` | `preset clear` | 【L3】清除预设（回落磁盘 policy.toml 参数） | 同上 |
| `version` | — | 模块与 daemon 版本 | 0 |
| （无参数/未知） | — | 用法说明 | 2 |

## 退出码约定（全局）

| 码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 执行失败 / 状态异常（status 专指 daemon 未运行） |
| 2 | 参数错误或缺少必要确认 |
| 3 | 功能在当前交付阶段未实现（预留命令面） |

## `status --json` 输出契约

WebUI 仪表盘依赖以下字段，**任何实现变更必须向后兼容**（只增不改）：

```json
{
  "module": "sundown",
  "version": "0.1.0-l0",
  "daemon_running": 1,
  "daemon_pid": 1234,
  "daemon_ready": 1,
  "zygisk_provider": "rezygisk | zygisknext | magisk-zygisk | none",
  "probe_stub_loaded": 0,
  "probe_dex_version": null,
  "probe_dex_hash_match": null,
  "boot_completed": "1"
}
```

- `daemon_pid`：daemon 未运行时为 `null`
- `probe_stub_loaded`：L1 起为真实值（桩 hello-probe 上报后置 1）；socket 数据源专属，fs 降级时恒 0
- `probe_stub_build_hash`：L1 起由 socket 数据源追加（桩上报的 build hash；未上报为 `null`），
  软重启 hash 验证闭环见 probe/README.md
- `probe_dex_version`：L2 起为真实值（dex 层 hello-dex 上报的构建版本 = CI 构建 commit short sha；
  未上报为 `null`），四位一体闭环见 dex/README.md
- `probe_dex_hash_match`：L2 起由 socket 数据源追加。三态：`1`=与模块内 probe.dex.hash 匹配；
  `0`=不匹配（可 `reload-probe` 热更新自愈）；`-1`=无期望值可比（dev 场景，模块内无 probe.dex.hash）。
  fs 降级时为 `null`
- `probe_hook_bridge_hash`：L2b 起由 socket 数据源追加（bridge 经 report-bridge 上报的 build hash；
  未上报为 `null`）
- `probe_hook_bridge_hash_match`：L2b 起由 socket 数据源追加。三态语义同上（期望值 = 模块内
  hook/hook.hash；不匹配时刷入一致版本后**软重启**生效，bridge 不走 socket 热更新）
- `focus_pkg` / `focus_changes` / `wakeup_events`：L2b 起由 socket 数据源追加（观测模式事件面：
  最近焦点包名 / 焦点切换累计 / 唤醒入口命中累计；无数据时 `focus_pkg` 为 `null`，计数为 0）

## `status` 数据源与 socket 通道

`status` 优先经 daemon 控制 socket 取**真实运行态**；daemon 未连接时降级为文件探测：

| 优先级 | 数据源 | 通道 | 说明 |
|---|---|---|---|
| A | daemon socket | `nc -U /data/adb/sundown/sundownd.sock`（toybox nc，行协议） | 真实 uptime / 热加载计数 / 连接计数 |
| B | 文件探测 | pgrep + ready 标记 + getprop | daemon 未运行/未就绪时的降级视图 |

- socket 协议：一行一个命令，应答一行 JSON。命令面：
  `ping` / `status` / `reload-config` / `stop`（L0）；
  `hello-probe <hash>` / `probe-query`（L1，桩握手与查询）；
  `hello-dex <version>` / `fetch-dex` / `push-dex`（L2，dex 握手订阅/字节拉取/管理面推送，
  协议细节与热切换时序见 dex/README.md 与 docs/l2-plan.md）；
  `report-bridge <hash>` / `event <type> k=v...`（L2b，hello-dex 订阅连接上的
  dex→daemon 上行命令：bridge hash 上报与焦点/唤醒/进程事件上行，见 dex/README.md）；
  `events [n]`（L3.1，结构化事件缓冲查询：最近 n 条，最旧→最新，JSON 数组；
  n 缺省/0 = 全部；管理面与 abstract 面均可读）

## `events` 事件契约（L3.1，只增不改）

daemon 内存环形缓冲（容量 256，覆盖最旧），事件模型参考 AStop/Cerberus 事件时间线：

```json
[
  {"ts":1722600000,"level":"success","action":"freeze","subject":"app","pkg":"com.tencent.mm","reason":"grace_expired"},
  {"ts":1722600010,"level":"event","action":"unfreeze","subject":"app","pkg":"com.tencent.mm","reason":"wakeup"},
  {"ts":1722600020,"level":"report","action":"system","subject":"system","reason":"daemon_start","msg":"v0.4.9-l3 (release 16)"}
]
```

| 字段 | 取值 | 说明 |
|---|---|---|
| `ts` | 整数 | epoch 秒（排序键） |
| `level` | `info`/`event`/`success`/`warn`/`error`/`timer`/`report` | 事件性质（参考 Cerberus log_level_*） |
| `action` | `open`/`close`/`freeze`/`unfreeze`/`delay`/`exempt`/`policy`/`system` | 动作（参考 Cerberus log_level_action_*） |
| `subject` | `app`/`system` | 主体（参考 Cerberus log_subject_*） |
| `pkg` | 字符串（可选） | 应用包名；subject=app 时必有 |
| `reason` | 字符串（可选） | 触发原因：`foreground`/`wakeup`/`grace`/`grace_expired`/`force`（force 列表立即冻结，v0.4.12-l3 起与 grace_expired 区分）/`force_stop`/`per_app_exempt`/`exempt_action`/`tick_exempt`/`no_procs`/`freeze_failed`/`policy_disabled`/`reloaded`/`reload_failed`/`preset`（情景预设 apply/clear，v0.4.15-l3 起；msg 携带 `apply=<name>`/`clear=<name>`）/`probe_handshake`/`dex_handshake`/`dex_reregister`/`bridge_report`/`daemon_start`/`daemon_stop` 等 |
| `msg` | 字符串（可选） | 人类可读补充 |

- 可选字段（pkg/reason/msg）缺省时**省略**（非 null）
- 唤醒事件按既有节流（每 32 条落一条）入缓冲，防广播风暴刷屏
- `status --json` 同时追加 `events_count`（缓冲内条数）/ `events_total`（累计产生，单调递增）
- **双通道**（同一套行协议，daemon 同时监听）：
  - 文件 socket `/data/adb/sundown/sundownd.sock` —— root 管理面（sunctl/WebUI）。
    注意 `/data/adb` 为 `drwx------ root root`，**system_server(uid 1000) 在 DAC 层
    即被 EACCES**（无 avc，纯文件权限拒绝），此通道只服务 root 客户端；
  - abstract socket `sundown_probe`（abstract namespace，无文件路径）—— L1 桩 /
    L2 dex 层通道。无 DAC 路径穿越问题，SELinux `connectto ksu` 已由 sepolicy.rule
    放行。Java 侧连接方式：`LocalSocketAddress("sundown_probe", Namespace.ABSTRACT)`
- socket 应答比 CLI 契约**多** `release_no` / `uptime_s` / `config_reloads` / `connections_served`；
  `zygisk_provider` / `boot_completed` daemon 不掌握，由 sunctl 本地探测**补全**后输出
- 文本输出标注当前数据源（`daemon socket` / `文件探测`）；`--json` 字段契约与退出码约定**不变**（只增不改）

## 与分层热更新的对应

| 命令 | 作用层 |
|---|---|
| `restart-daemon` | L0（daemon 自身，无感） |
| `reload-probe` | L2（probe.dex，无感热更新） |
| `restart-runtime` | L1（libsunprobe.so，需软重启） |
| `apply-update` | L0 staged 通道（配合 service.sh 在重启/软重启后激活） |