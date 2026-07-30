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