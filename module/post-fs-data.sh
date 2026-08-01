#!/system/bin/sh
# Sundown post-fs-data.sh
# post-fs-data 阶段是阻塞的（最多 10 秒），仅执行必要操作

MODDIR=${0%/*}
SUNDOWN_DIR="/data/adb/sundown"
LEGACY_DIR="/data/adb/cerberus"
CONF_DIR="$SUNDOWN_DIR/conf"
DATA_DIR="$SUNDOWN_DIR/data"
LOG_DIR="$SUNDOWN_DIR/logs"
PROP_FILE="$MODDIR/system.prop"

mkdir -p "$CONF_DIR" "$DATA_DIR" "$LOG_DIR"

# ========== 旧资产迁移：Cerberus/AStop → Sundown ==========
# 目录映射表：
#   /data/adb/cerberus/conf/*.json|toml|txt  → /data/adb/sundown/conf/
#   /data/adb/cerberus/data/*                → /data/adb/sundown/data/
#   /data/adb/cerberus/logs/*                → /data/adb/sundown/logs/
#   /data/adb/cerberus/update/pending|backup → /data/adb/sundown/update/  （pending 更新不跨品牌迁移，直接废弃）
# 迁移完成后旧目录改名保留为 cerberus.legacy 一个启动周期，确认稳定后可手动删除。
if [ -d "$LEGACY_DIR" ] && [ ! -f "$SUNDOWN_DIR/data/.migrated_from_cerberus" ]; then
    # 先迁移 conf/data/logs 三个标准子目录（若存在）
    for sub in conf data logs; do
        if [ -d "$LEGACY_DIR/$sub" ]; then
            cp -a "$LEGACY_DIR/$sub/." "$SUNDOWN_DIR/$sub/" 2>/dev/null
        fi
    done
    # 旧版扁平布局兼容：散落在根目录的配置/数据文件
    move_if_missing() {
        src="$1"
        dst_dir="$2"
        if [ -f "$src" ]; then
            dst="$dst_dir/$(basename "$src")"
            [ ! -e "$dst" ] && mv "$src" "$dst"
        fi
    }
    for f in "$LEGACY_DIR"/*.json "$LEGACY_DIR"/*.toml "$LEGACY_DIR"/*.txt; do
        move_if_missing "$f" "$CONF_DIR"
    done
    for f in "$LEGACY_DIR"/logs.db "$LEGACY_DIR"/logs.db-wal "$LEGACY_DIR"/logs.db-shm \
             "$LEGACY_DIR"/session.dat "$LEGACY_DIR"/license.dat "$LEGACY_DIR"/soft_device_id; do
        move_if_missing "$f" "$DATA_DIR"
    done
    [ -f "$LEGACY_DIR/boot_watchdog.log" ] && move_if_missing "$LEGACY_DIR/boot_watchdog.log" "$LOG_DIR"
    [ -f "$LEGACY_DIR/index_dump.log" ] && move_if_missing "$LEGACY_DIR/index_dump.log" "$LOG_DIR"

    # pending 更新不跨品牌迁移（二进制不兼容 sundownd 命名），显式废弃
    rm -rf "$LEGACY_DIR/update/pending" 2>/dev/null

    touch "$SUNDOWN_DIR/data/.migrated_from_cerberus"
    mv "$LEGACY_DIR" "${LEGACY_DIR}.legacy" 2>/dev/null
fi

# ========== Sundown 内部旧版本兼容：文件自动迁移逻辑 ==========
move_if_missing() {
    src="$1"
    dst_dir="$2"
    if [ -f "$src" ]; then
        dst="$dst_dir/$(basename "$src")"
        [ ! -e "$dst" ] && mv "$src" "$dst"
    fi
}

for f in "$SUNDOWN_DIR"/*.json "$SUNDOWN_DIR"/*.toml "$SUNDOWN_DIR"/*.txt; do
    move_if_missing "$f" "$CONF_DIR"
done

for f in "$SUNDOWN_DIR"/logs.db "$SUNDOWN_DIR"/logs.db-wal "$SUNDOWN_DIR"/logs.db-shm \
         "$SUNDOWN_DIR"/session.dat "$SUNDOWN_DIR"/license.dat "$SUNDOWN_DIR"/soft_device_id; do
    move_if_missing "$f" "$DATA_DIR"
done

[ -f "$SUNDOWN_DIR/boot_watchdog.log" ] && move_if_missing "$SUNDOWN_DIR/boot_watchdog.log" "$LOG_DIR"
[ -f "$SUNDOWN_DIR/index_dump.log" ] && move_if_missing "$SUNDOWN_DIR/index_dump.log" "$LOG_DIR"
# =================================================

# ========== L2 探针 dex 同步：模块资产 → root 侧字节源 ==========
# daemon(root) 从 /data/adb/sundown/probe/probe.dex 读字节，经 abstract socket 下发给
# system_server 内的 dex 层；桩/dex 层（uid 1000）不直接读该路径（/data/adb 为
# drwx------ root，DAC 层 EACCES——L1 真机实证）。
# 按 hash 比对避免无谓拷贝：dex 文件 mtime 变化会触发运行期 dexopt/oat 失效。
PROBE_SRC="$MODDIR/probe/probe.dex"
PROBE_HASH_SRC="$MODDIR/probe/probe.dex.hash"
PROBE_DST_DIR="$SUNDOWN_DIR/probe"
PROBE_DST="$PROBE_DST_DIR/probe.dex"
DEPLOYED_MARK="$PROBE_DST_DIR/.deployed_dex_hash"

if [ -f "$PROBE_SRC" ]; then
    mkdir -p "$PROBE_DST_DIR"
    new_hash="$(cat "$PROBE_HASH_SRC" 2>/dev/null)"
    old_hash="$(cat "$DEPLOYED_MARK" 2>/dev/null)"
    if [ "$new_hash" != "$old_hash" ] || [ ! -f "$PROBE_DST" ]; then
        cp "$PROBE_SRC" "$PROBE_DST"
        chmod 0600 "$PROBE_DST"
        # dex 变更后旧 oat 缓存失效，一并清理
        rm -rf "$PROBE_DST_DIR/oat" 2>/dev/null
        echo "$new_hash" > "$DEPLOYED_MARK"
    fi
fi
# =================================================

# ========== L2b 排障辅助：Sundown/LSPlant 日志常驻记录器 ==========
# 完整重启时由本脚本启动（KernelSU 进程树管理，常驻跨软重启存活）；
# 只记录 Sundown 三 tag + LSPlant，避免被系统日志/其他应用刷屏淹没
# （logcat main buffer 仅 256KB，启动早期日志约 2 分钟即被冲掉——真机实证）。
# 文件：$LOG_DIR/boot-logcat.log（启动时清空重建；软重启由常驻进程继续追加，
# 时间戳可区分；post-fs-data 阶段 logd 已就绪，且早于 zygote/system_server 注入窗口）。
if [ -d "$LOG_DIR" ]; then
    : > "$LOG_DIR/boot-logcat.log" 2>/dev/null
    nohup logcat -b all -v threadtime -s SundownHook:I SundownDex:I SundownProbe:I LSPlant:I \
        >> "$LOG_DIR/boot-logcat.log" 2>&1 &
fi
# =================================================

# 注意：属性设置已移至 system.prop，由 KernelSU/Magisk 自动应用。
# Magisk fallback 只兜底 persist.*，避免在 post-fs-data 阶段强制覆盖 ro.* / sys.*。
if [ -z "$KSU" ] && [ -f "$PROP_FILE" ]; then
    while IFS='=' read -r prop value; do
        case "$prop" in
            ""|\#*) continue ;;
        esac
        case "$prop" in
            *[!A-Za-z0-9_.-]*) continue ;;
        esac
        case "$prop" in
            persist.*) ;;
            *) continue ;;
        esac
        resetprop -n "$prop" "$value"
    done < "$PROP_FILE"
fi