#!/system/bin/sh
# Sundown service.sh
# late_start 服务模式 - 非阻塞，与启动过程并行执行
# L0 职责：sundownd 守护进程启动 / staged 更新激活 / 看门狗

MODDIR=${0%/*}
SUNDOWN_DIR="/data/adb/sundown"
LOG_DIR="$SUNDOWN_DIR/logs"
PROP_FILE="$MODDIR/module.prop"
DAEMON_PATH="$MODDIR/system/bin/sundownd"
UPDATE_DIR="$SUNDOWN_DIR/update"
PENDING_DIR="$UPDATE_DIR/pending"
BACKUP_DIR="$UPDATE_DIR/backup"
INSTALLED_META="$UPDATE_DIR/installed.json"
READY_MARKER="$UPDATE_DIR/daemon.ready"
UPDATE_APPLIED=0

# v0.4.53-l3：日志按「版本 × 日期」归档——boot_watchdog 落 logs/<version>/<今天>/
VER_NAME="$(grep '^version=' "$PROP_FILE" 2>/dev/null | cut -d= -f2 | tr -d 'v')"
TODAY="$(date +%F 2>/dev/null)"
LOG_DAY_DIR="$LOG_DIR/$VER_NAME/$TODAY"
BOOT_LOG="$LOG_DAY_DIR/boot_watchdog.log"

mkdir -p "$LOG_DIR" "$PENDING_DIR" "$BACKUP_DIR"
[ -n "$VER_NAME" ] && [ -n "$TODAY" ] && mkdir -p "$LOG_DAY_DIR"

exec >> "$BOOT_LOG" 2>&1
echo "----------------------------------------"
echo "[$(date)] Sundown service.sh starting..."

update_description() {
    description="$1"

    if command -v ksud >/dev/null 2>&1; then
        ksud module config set override.description "$description" >/dev/null 2>&1 && return
    fi
    sed -i "s#^description=.*#description=$description#" "$PROP_FILE"
}

json_string() {
    key="$1"
    file="$2"
    sed -n "s/.*\"$key\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$file" 2>/dev/null | head -n 1
}

json_number() {
    key="$1"
    file="$2"
    sed -n "s/.*\"$key\"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p" "$file" 2>/dev/null | head -n 1
}

daemon_version() {
    version="$(json_string version_name "$INSTALLED_META")"
    [ -n "$version" ] && echo "$version" || echo "0.1.0-l0"
}

clear_pending_update() {
    rm -f \
        "$PENDING_DIR/sundownd.new" \
        "$PENDING_DIR/sundownd.download" \
        "$PENDING_DIR/installed.json.new" \
        "$PENDING_DIR/pending.sha256" \
        "$PENDING_DIR/pending.json"
}

apply_pending_update() {
    pending_bin="$PENDING_DIR/sundownd.new"
    pending_meta="$PENDING_DIR/installed.json.new"
    pending_sha="$PENDING_DIR/pending.sha256"
    pending_marker="$PENDING_DIR/pending.json"
    [ -f "$pending_marker" ] || return 0

    staged_boot_id="$(json_string staged_boot_id "$pending_marker")"
    current_boot_id="$(tr -d '[:space:]' < /proc/sys/kernel/random/boot_id 2>/dev/null)"
    if [ -z "$staged_boot_id" ] || [ -z "$current_boot_id" ]; then
        echo "[$(date)] Pending update is missing boot identity; leaving it untouched."
        return 0
    fi
    if [ "$staged_boot_id" = "$current_boot_id" ]; then
        echo "[$(date)] Pending update was staged in this boot; waiting for a real reboot."
        return 0
    fi

    if [ ! -f "$pending_bin" ] || [ ! -f "$pending_meta" ] || [ ! -f "$pending_sha" ]; then
        echo "[$(date)] Pending update is incomplete; discarding it."
        clear_pending_update
        return 0
    fi

    expected_sha="$(tr -d '[:space:]' < "$pending_sha")"
    actual_sha="$(sha256sum "$pending_bin" 2>/dev/null | awk '{print $1}')"
    case "$expected_sha" in
        *[!0-9a-f]*|"")
            echo "[$(date)] Pending update SHA-256 metadata is invalid."
            clear_pending_update
            return 0
            ;;
    esac
    if [ "${#expected_sha}" -ne 64 ] || [ "$actual_sha" != "$expected_sha" ]; then
        echo "[$(date)] Pending update SHA-256 mismatch; refusing activation."
        clear_pending_update
        return 0
    fi

    rm -f "$BACKUP_DIR/sundownd.previous" "$BACKUP_DIR/installed.json.previous"
    if ! cp -p "$DAEMON_PATH" "$BACKUP_DIR/sundownd.previous"; then
        echo "[$(date)] Could not back up current daemon; update remains pending."
        return 0
    fi
    [ -f "$INSTALLED_META" ] && cp -p "$INSTALLED_META" "$BACKUP_DIR/installed.json.previous"

    rm -f "$READY_MARKER"
    if ! mv -f "$pending_bin" "$DAEMON_PATH"; then
        echo "[$(date)] Could not activate staged daemon."
        cp -p "$BACKUP_DIR/sundownd.previous" "$DAEMON_PATH"
        return 0
    fi
    chmod 0755 "$DAEMON_PATH"
    if ! mv -f "$pending_meta" "$INSTALLED_META"; then
        echo "[$(date)] Could not activate daemon version metadata; rolling back files."
        cp -p "$BACKUP_DIR/sundownd.previous" "$DAEMON_PATH"
        [ -f "$BACKUP_DIR/installed.json.previous" ] &&
            cp -p "$BACKUP_DIR/installed.json.previous" "$INSTALLED_META"
        clear_pending_update
        return 0
    fi
    UPDATE_APPLIED=1
    echo "[$(date)] Activated staged daemon $(daemon_version); verifying startup."
}

rollback_pending_update() {
    echo "[$(date)] New daemon failed readiness check; rolling back."
    pgrep -f "$DAEMON_PATH" | while read -r failed_pid; do
        kill "$failed_pid" 2>/dev/null
    done
    sleep 1
    if [ -f "$BACKUP_DIR/sundownd.previous" ]; then
        cp -p "$BACKUP_DIR/sundownd.previous" "$DAEMON_PATH"
        chmod 0755 "$DAEMON_PATH"
    fi
    if [ -f "$BACKUP_DIR/installed.json.previous" ]; then
        cp -p "$BACKUP_DIR/installed.json.previous" "$INSTALLED_META"
    else
        rm -f "$INSTALLED_META"
    fi
    clear_pending_update
    rm -f "$READY_MARKER"
    UPDATE_APPLIED=0
}

# 等待系统启动完成
while [ "$(getprop sys.boot_completed)" != "1" ]; do
    sleep 5
done
echo "[$(date)] Boot completed."

# 时间未同步防护归位（v0.4.53-l3 实机修复）：post-fs-data 阶段若系统时钟未就绪，
# boot-logcat.log 落在 logs/<ver>/pending-boot/ 占位目录；本阶段（boot completed，
# 时间已同步）归位到真实日期目录，保证「版本×日期」归档体系无 1970 脏目录。
# 注：logcat 常驻记录器可能仍持有旧 inode（fd 悬空），移动后新行不再落盘——
# 属尽力归位，下次重启后 post-fs-data 在正确日期目录重建。
PENDING_BOOT_DIR="$LOG_DIR/$VER_NAME/pending-boot"
if [ -d "$PENDING_BOOT_DIR" ]; then
    TODAY="$(date +%F 2>/dev/null)"
    case "$TODAY" in
        1970-*) TODAY="" ;;
    esac
    if [ -n "$TODAY" ]; then
        REAL_DAY_DIR="$LOG_DIR/$VER_NAME/$TODAY"
        mkdir -p "$REAL_DAY_DIR"
        mv "$PENDING_BOOT_DIR"/* "$REAL_DAY_DIR/" 2>/dev/null
        rmdir "$PENDING_BOOT_DIR" 2>/dev/null
        echo "[$(date)] boot-logcat 占位目录已归位: pending-boot/ → $TODAY/"
    fi
fi

# 清理旧日志
find "$LOG_DIR" -type f -name "*.log" -mtime +7 -delete 2>/dev/null

# 仅在重启后的 daemon 启动前切换已校验的待更新文件。
apply_pending_update

# 启动守护进程
start_daemon() {
    if [ -x "$DAEMON_PATH" ]; then
        echo "[$(date)] Starting sundownd daemon..."
        nohup "$DAEMON_PATH" > /dev/null 2>&1 &
        sleep 3

        DAEMON_PID=$(pgrep -f "$DAEMON_PATH" | head -n 1)
        if [ -n "$DAEMON_PID" ]; then
            echo "[$(date)] Daemon started successfully. PID: $DAEMON_PID"
            update_description "🌇 Sundown 运行中/$(daemon_version) [PID: $DAEMON_PID]"
            return 0
        else
            echo "[$(date)] ERROR: Daemon failed to start!"
            update_description "🌇 Sundown ❌ 启动失败，请检查日志"
            return 1
        fi
    else
        echo "[$(date)] FATAL: Daemon not found at $DAEMON_PATH"
        update_description "🌇 Sundown ❌ 错误: 守护进程文件丢失（L0 骨架尚未内置 sundownd 二进制）"
        return 1
    fi
}

# 首次启动
if ! pgrep -f "$DAEMON_PATH" > /dev/null; then
    if start_daemon; then
        if [ "$UPDATE_APPLIED" = "1" ]; then
            ready_wait=0
            while [ "$ready_wait" -lt 20 ] && [ ! -f "$READY_MARKER" ]; do
                sleep 1
                ready_wait=$((ready_wait + 1))
            done
            stable_wait=0
            while [ "$stable_wait" -lt 10 ] && pgrep -f "$DAEMON_PATH" > /dev/null; do
                sleep 1
                stable_wait=$((stable_wait + 1))
            done
            expected_release="$(json_number release_no "$INSTALLED_META")"
            ready_release="$(json_number release_no "$READY_MARKER")"
            if [ ! -f "$READY_MARKER" ] ||
                ! pgrep -f "$DAEMON_PATH" > /dev/null ||
                [ -z "$expected_release" ] ||
                [ "$ready_release" != "$expected_release" ]; then
                rollback_pending_update
                start_daemon
            else
                echo "[$(date)] New daemon passed readiness check."
                clear_pending_update
                rm -f "$BACKUP_DIR/sundownd.previous" "$BACKUP_DIR/installed.json.previous"
            fi
        fi
    elif [ "$UPDATE_APPLIED" = "1" ]; then
        rollback_pending_update
        start_daemon
    fi
fi

# 看门狗循环（v0.4.54-l3：维护窗口判定——外部管理面 sunctl hotswap / 
# apply-update --activate / restart-daemon 会 touch $UPDATE_DIR/.updating，
# 标记存在时跳过本轮自动重启，防替换窗口竞争启动半成品/旧二进制）
while true; do
    sleep 300
    [ -f "$UPDATE_DIR/.updating" ] && continue
    if ! pgrep -f "$DAEMON_PATH" > /dev/null; then
        echo "[$(date)] Watchdog: Daemon not running, restarting..."
        start_daemon
    fi
done