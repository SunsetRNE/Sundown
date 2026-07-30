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
| `reload-probe` | — | 【L2 交付】通知 daemon 推送 probe.dex 热切换 | 3=当前阶段未实现 |
| `apply-update` | — | 【后续交付】激活 staged 守护进程更新 | 3=当前阶段未实现 |
| `logs` | `[行数=50]` | 输出 boot_watchdog.log 末尾 | 0 |
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
  "boot_completed": "1"
}
```

- `daemon_pid`：daemon 未运行时为 `null`
- `probe_stub_loaded`：L1 起为真实值（桩 hello-probe 上报后置 1）；socket 数据源专属，fs 降级时恒 0
- `probe_stub_build_hash`：L1 起由 socket 数据源追加（桩上报的 build hash；未上报为 `null`），
  软重启 hash 验证闭环见 probe/README.md
- `probe_dex_version`：L2 阶段填入真实值；当前恒为 `null`

## `status` 数据源与 socket 通道

`status` 优先经 daemon 控制 socket 取**真实运行态**；daemon 未连接时降级为文件探测：

| 优先级 | 数据源 | 通道 | 说明 |
|---|---|---|---|
| A | daemon socket | `nc -U /data/adb/sundown/sundownd.sock`（toybox nc，行协议） | 真实 uptime / 热加载计数 / 连接计数 |
| B | 文件探测 | pgrep + ready 标记 + getprop | daemon 未运行/未就绪时的降级视图 |

- socket 协议：一行一个命令，应答一行 JSON。命令面：
  `ping` / `status` / `reload-config` / `stop`（L0）；
  `hello-probe <hash>` / `probe-query`（L1，桩握手与查询）；
  `push-dex`（L2 预留）
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