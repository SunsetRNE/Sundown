# Sundown 缺陷与修复日志（Bug & Fix Log）

> 记录实机校验 / 开发过程中发现的缺陷与修复闭环：现象 → 根因 → 修复 → 验证。
> 约定：编号 B 系列递增；严重度分级 S1（数据损坏/进程崩溃）/ S2（功能路径必败）/ S3（行为偏差）/ S4（日志噪音/体验）；
> 每条含 commit 定位与验证方式，可回溯。
>
> 本日志由 v0.4.53-l3 实机校验阶段起维护（2026-08-09 起）。

## 汇总表

| 编号 | 日期 | 引入版本 | 修复版本 | 严重度 | 标题 | commit |
|---|---|---|---|---|---|---|
| B01 | 08-09 | v0.4.53-l3 | v0.4.53-l3 | S3 | sunctl `logs --list/--times` 空输出 | `ec8921d` |
| B02 | 08-09 | v0.4.53-l3 | v0.4.53-l3 | S3 | 开机早期时钟未同步产生 1970 脏日期目录 | `ec8921d` |
| B03 | 08-09 | v0.4.50-l3 | v0.4.54-l3 | S4 | cgroup.freeze ENOENT WARN 刷屏（14445 条/天，占日志 74%） | `2daea99` |
| B04 | 08-09 | v0.4.46-l3 | v0.4.54-l3 | S4 | OOM 保护成功锁定日志刷屏（948 条/天） | `2daea99` |
| B05 | 08-09 | v0.4.20-l3 | v0.4.54-l3 | S2 | 运行中二进制替换 `Text file busy`（staged 激活必败） | `f5a4a3f` |
| B06 | 08-09 | v0.4.20-l3 | v0.4.54-l3 | S2 | 看门狗与外部管理操作竞争（kill 后抢启旧/半成品二进制） | `f5a4a3f` |
| B07 | 08-09 | v0.4.20-l3 | v0.4.54-l3 | S1 | `BACKUP_DIR` 未定义（`rm -f "$BACKUP_DIR/..."` 解析到根路径） | `f5a4a3f` |
| B08 | 08-09 | v0.4.20-l3 | v0.4.54-l3 | S2 | `json_number` 函数未定义（staged 激活 / hotswap / apply-update 必败） | `3addbdf` |
| B09 | 08-09 | v0.4.20-l3 | v0.4.54-l3 | S3 | `daemon_version` 与防降级基线读 installed.json 过时（实机 0.4.25-l3/32 vs 实际 0.4.54-l3/59） | `7c4d01d` |

## 明细

### B01 sunctl `logs --list` / `--times` 空输出

- **引入**：v0.4.53-l3（sunctl 日志归档命令）
- **现象**：`sunctl logs --list` / `--times` 只打印标题，无任何目录/时间记录输出
- **根因**：sunctl 版本目录 glob 为 `"$LOG_DIR"/v[0-9]*/`（要求 **v 前缀**），但 customize.sh 创建的是 `logs/0.4.53-l3/`（`tr -d 'v'` 去前缀）→ 模式不匹配 → 匹配不到任何目录
- **修复**：glob 改 `[0-9]*` 为主 + `v[0-9]*` 兼容历史目录（`current_log_dir` / `--list` / `--times` 三处）；`current_log_dir` 日期判定改名称字典序（`sort | tail`）而非 mtime——1970 脏目录当天仍有写入时会误选
- **验证**：本地模拟目录结构 + 设备热更新后复测（`0.4.53-l3/ 2026-08-08/ 464K` 正常输出）

### B02 开机早期时钟未同步产生 1970 脏日期目录

- **引入**：v0.4.53-l3（boot-logcat 按「版本×日期」归档）
- **现象**：`logs/0.4.53-l3/1970-01-02/boot-logcat.log`——boot-logcat 落错日期目录（实测 2026-08-09 校验时发现）
- **根因**：post-fs-data 阶段**系统时钟未就绪**（RTC 未同步，`date +%F` 返回 1970-01-02）→ 建出错误日期目录；而 logcat 行时间戳是写入瞬间（时间已同步后），目录名与内容时间戳不一致
- **修复**：post-fs-data 的 TODAY 加 `1970-*` 防护 → 回退 `pending-boot/` 占位目录；service.sh（boot completed 后时间可信）把 pending-boot 归位到真实日期目录；时间仍异常时安全跳过
- **验证**：本地模拟三阶段（1970→占位→归位→边界跳过）+ 设备脏目录手动归位清理

### B03 cgroup.freeze ENOENT WARN 刷屏

- **引入**：v0.4.50-l3（系统链路 OOM 锁定 `lock_system_chain_oom` 每 5 tick 对 CRITICAL_PACKAGES 全部 uid 无条件写 0）
- **现象**：sundownd.log 08-08 共 20609 行，WARN 15393 条（74.7%）；其中 `cgroup.freeze 写入失败` 14445 条，**100% 为 os error 2（ENOENT）**；08-09 一上午膨胀至 38MB，淹没真实诊断信息
- **根因**：系统 uid（1000/1001/2000）不在 apps cgroup 树 / 应用未运行时 uid 目录不存在 → 写 cgroup.freeze 必然 ENOENT 失败；`write_freeze` 对每次失败都 logw（无降噪）
- **修复**：`write_freeze` 对 ENOENT（目标 uid 未运行 = 正常状态）**完全静默**（返回 false 语义不变，SIGSTOP 兜底/幂等解冻不受影响）；其他真实故障（权限/IO）WARN + 60s/路径节流；新增 `err_log_cooled()` 纯函数 + 2 测试
- **验证**：50/50 测试全过；设备热更新 release 59 后运行 1 分钟 73 行日志 **WARN 0 条**

### B04 OOM 保护成功锁定日志刷屏

- **引入**：v0.4.46-l3（OOM 保护 tick 周期重锁）
- **现象**：`OOM 保护: uid=xxx 锁定 N 个进程 adj=-1000` 每天 948 条——成功锁定是**正常重锁动作**（系统 OomAdjuster 每 1.5s 覆盖 adj，tick 重锁即命中），却以 WARN 级别每次打印
- **修复**：同一 uid 60s 只留痕一次（保留"系统与 Sundown 拉锯"可观测性，同时控制日志量）
- **验证**：随 B03 一并热更新，运行 1 分钟 OOM 保护日志 0 条

### B05 运行中二进制替换 `Text file busy`

- **引入**：v0.4.20-l3（staged 更新激活路径 `cp -p "$pending_bin" "$DAEMON_PATH"`）
- **现象**：daemon 运行时执行 `apply-update --activate` → `cp: Text file busy`（ETXTBSY）→ 激活必败；手动热更新 cp 同样踩坑（2026-08-09 实录）
- **根因**：Linux 禁止覆盖正在执行的可执行文件；`cp` 覆盖运行中二进制必然失败
- **修复**：所有替换/回滚路径改 **`mv -f` 原子 rename**（运行中 daemon 占用旧 inode 不受影响，kill 后自然释放）——`activate_pending_update` 替换+回滚、新增 `hotswap` 替换+回滚
- **验证**：hotswap 实机全流程通过（daemon 运行中完成替换，pid 更新、ready 校验通过）；失败回滚路径实测（mv 还原 + 重启，daemon 恢复正常）

### B06 看门狗与外部管理操作竞争

- **引入**：v0.4.20-l3（service.sh 看门狗 while 循环）
- **现象**：kill daemon 后，看门狗（300s 周期）会用**当前路径二进制**抢启——替换窗口内可能启动半成品/旧二进制；多轮操作出现多实例并存与 ready 标记混乱（2026-08-09 热更新实录）
- **修复**：**维护窗口标记 `$UPDATE_DIR/.updating`**——watchdog 每轮检查，标记存在则跳过自动重启；`hotswap` / `apply-update --activate` / `restart-daemon` 操作期间 touch，完成与失败路径均 rm
- **验证**：本地模拟三态（无标记重启 / 有标记跳过 / rm 后恢复）+ hotswap 实测 `.updating` 正常清理

### B07 `BACKUP_DIR` 未定义

- **引入**：v0.4.20-l3（staged 更新激活）
- **现象**：`activate_pending_update` 备份步骤 `rm -f "$BACKUP_DIR/sundownd.previous"`——变量未定义 → 解析为**根路径** `/sundownd.previous`；备份 `cp -p` 目标同样无效 → 备份必败
- **根因**：sunctl 顶部只定义了 `UPDATE_DIR`/`PENDING_DIR`，漏 `BACKUP_DIR`
- **修复**：补 `BACKUP_DIR="$UPDATE_DIR/backup"` + 备份前 `mkdir -p`
- **验证**：hotswap 备份步骤实机通过（backup 目录正确创建）

### B08 `json_number` 函数未定义

- **引入**：v0.4.20-l3（staged 更新激活）
- **现象**：`json_number: inaccessible or not found`——stage_update / activate_pending_update / cmd_apply_update / cmd_hotswap 均依赖，但函数从未定义（只有 `json_string` / `json_str_in` / `json_int_in`）
- **根因**：激活路径从未在运行态完整跑过（先后被 B07、B05 拦截），缺陷被掩盖
- **修复**：工具函数区补 `json_number()`（文件 JSON 数字字段提取）
- **验证**：hotswap 重测全流程通过（版本解析、防降级、readiness 比对全部依赖该函数）

### B09 `daemon_version` 与防降级基线读 installed.json 过时

- **引入**：v0.4.20-l3（sunctl `daemon_version()` / 防降级基线以 `$INSTALLED_META` 为准）
- **现象**：`sunctl --version` 显示 `daemon: 0.4.25-l3`，实际运行 0.4.54-l3/59（2026-08-09 v0.4.54-l3 完整刷入+重启后校验发现）
- **根因**：installed.json 仅 **staged 激活路径**（`apply-update --activate`）更新；zip 刷入（customize.sh）与 hotswap 均不更新它 → 长期过时（实机遗留 08-03 的 0.4.25-l3/32）；`daemon_version()` 与三处防降级基线（stage_update / cmd_hotswap / cmd_apply_update）均以它为唯一数据源 → 版本误报误导诊断 + **降级拦截基线失效**（基线 32 远低于实际 59，hotswap 旧包不会被拦截）
- **修复**：新增 `current_release_no()` 统一基线；`daemon_version()` 与三处防降级基线改 **daemon.ready 优先**（daemon 每次启动自写的运行时权威状态，含 version_name/release_no/pid）→ installed.json → `$VERSION`/0 兜底
- **验证**：本地模拟三场景（ready+installed 并存 / 仅 installed / 全无）fallback 链正确；设备热更新后 `--version` 正确显示 `daemon: 0.4.54-l3`

## 经验沉淀（防再犯）

1. **未运行态验证的路径 = 隐患区**：B05/B07/B08 全部位于"从未在运行态完整跑过"的 staged 激活路径——新功能必须端到端实测（hotswap 正是为此而生的受控验证入口）
2. **二进制替换必须原子**：运行中可执行文件只能 rename（mv），不能 write（cp）——所有替换路径统一 mv + 维护窗口协调
3. **日志噪音是缺陷**：高频必然失败路径（ENOENT）不得每次打 WARN——正常状态静默、真实故障保留 + 节流
4. **shell 全局变量用前必查**：`BACKUP_DIR`/`json_number` 类缺失应在语法/静态检查或首次端到端测试中暴露
5. **运行时状态源与安装元数据分离**：installed.json（staged 安装记录）≠ 当前运行版本（daemon.ready）——版本展示与防降级判定一律以**运行时状态**为准，安装元数据仅作兜底（B09）