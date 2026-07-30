#!/system/bin/sh
# Sundown uninstall.sh
# 模块卸载时执行

# 终止守护进程
killall sundownd 2>/dev/null

# 恢复系统属性
SUNDOWN_DIR="/data/adb/sundown"
PROP_BACKUP_FILE="$SUNDOWN_DIR/data/prop_backup.conf"
SYSTEM_PROP_FILE="${0%/*}/system.prop"

if [ -f "$PROP_BACKUP_FILE" ]; then
    echo "Restoring system properties from backup..." >&2
    while IFS='=' read -r prop_name original_value; do
        # 跳过空行和注释
        [ -z "$prop_name" ] && continue
        case "$prop_name" in \#*) continue ;; esac

        if [ -z "$original_value" ]; then
            # 原始值为空，删除属性
            resetprop -p --delete "$prop_name" 2>/dev/null
        else
            # 恢复到原始值
            resetprop -p -n "$prop_name" "$original_value" 2>/dev/null
        fi
    done < "$PROP_BACKUP_FILE"
    echo "Properties restored." >&2
else
    # 备份文件不存在，删除所有已知属性
    echo "Backup file not found. Deleting known properties..." >&2
    if [ -f "$SYSTEM_PROP_FILE" ]; then
        grep '^[A-Za-z0-9_.-][A-Za-z0-9_.-]*=' "$SYSTEM_PROP_FILE" | cut -d= -f1 | while read -r prop; do
            [ -z "$prop" ] && continue
            resetprop -p --delete "$prop" 2>/dev/null
        done
    fi
fi

# 清理 KernelSU 模块配置
if command -v ksud >/dev/null 2>&1; then
    ksud module config clear 2>/dev/null
fi

# 清理模块数据目录
rm -rf "$SUNDOWN_DIR"

echo "Sundown uninstalled successfully." >&2