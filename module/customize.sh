#!/system/bin/sh
# Sundown 模块安装脚本

# 环境检查
$BOOTMODE || abort "- 错误: 请在 Magisk 或 KernelSU 环境中安装！"
[ "$API" -ge 30 ] || abort "- 错误: Sundown 仅支持 Android 11 (API 30) 及以上版本！"

ui_print "- 安卓版本 $API, 符合要求。"
ui_print "- 设备架构: $ARCH"

SUNDOWN_DIR="/data/adb/sundown"
DATA_DIR="$SUNDOWN_DIR/data"
PROP_BACKUP_FILE="$DATA_DIR/prop_backup.conf"
SYSTEM_PROP_FILE="$MODPATH/system.prop"
MI_PROP_FILE="$MODPATH/system.mi.prop"
OPLUS_PROP_FILE="$MODPATH/system.oplus.prop"
QTI_PROP_FILE="$MODPATH/system.qti.prop"

# 检测安装环境
if [ -n "$KSU" ]; then
    ui_print "- 检测到 KernelSU 环境 ✓"
else
    ui_print "- 检测到 Magisk 环境 ✓"
fi

# Zygisk 提供方检测（L1/L2 探针阶段需要；L0 骨架不阻断安装，仅提示）
if [ -d /data/adb/modules/rezygisk ]; then
    ui_print "- 检测到 ReZygisk ✓（L1 探针提供方）"
elif [ -d /data/adb/modules/zygisksu ] || [ -d /data/adb/modules/ZygiskNext ]; then
    ui_print "- 检测到 ZygiskNext ✓（L1 探针提供方）"
else
    ui_print "  ! 未检测到 ReZygisk/ZygiskNext"
    ui_print "  ! L0 骨架可正常安装；后续探针功能需要安装 ReZygisk"
fi

# 文件权限设置
ui_print "- 设置文件权限..."
set_perm_recursive "$MODPATH" 0 0 0755 0644
set_perm "$MODPATH/system/bin/sundownd" 0 0 0755
set_perm "$MODPATH/system/bin/sunctl" 0 0 0755
set_perm "$MODPATH/post-fs-data.sh" 0 0 0755
set_perm "$MODPATH/service.sh" 0 0 0755
set_perm "$MODPATH/uninstall.sh" 0 0 0755

append_prop_file() {
    src="$1"
    label="$2"

    if [ -f "$src" ]; then
        ui_print "  > 追加 $label 属性"
        printf '\n' >> "$SYSTEM_PROP_FILE"
        cat "$src" >> "$SYSTEM_PROP_FILE"
    fi
}

ui_print "- 生成设备适配属性..."
if find /proc -maxdepth 1 -name "oplus*" 2>/dev/null | grep -q . || [ -n "$(getprop ro.build.version.oplusrom.display)" ] || [ -n "$(getprop persist.sys.oplus.osense.version)" ]; then
    append_prop_file "$OPLUS_PROP_FILE" "OPLUS/ColorOS"
elif [ -n "$(getprop ro.miui.ui.version.name)" ]; then
    append_prop_file "$MI_PROP_FILE" "小米/HyperOS"
fi

if [ -d /sys/class/kgsl ]; then
    append_prop_file "$QTI_PROP_FILE" "Qualcomm QTI"
fi

mkdir -p "$DATA_DIR"
if [ ! -f "$PROP_BACKUP_FILE" ]; then
    ui_print "- 备份 persist.* 原始属性..."
    grep '^persist\.[A-Za-z0-9_.-]*=' "$SYSTEM_PROP_FILE" 2>/dev/null | cut -d= -f1 | sort -u | while read -r prop; do
        [ -z "$prop" ] && continue
        current_value=$(getprop "$prop")
        echo "$prop=$current_value" >> "$PROP_BACKUP_FILE"
    done
else
    ui_print "- 已存在属性备份，跳过。"
fi

# ========== L3 conf 模板首次部署（v0.4.16-l3 起） ==========
# 数据目录 conf/ 无任何 .toml/.json 配置时，从模块模板部署默认配置
# （policy.toml 观望模式 + action.toml 情景预设示例）；
# 已存在配置（含用户手工写入/旧版遗留）一律保留——用户配置优先是铁律。
CONF_DIR="$SUNDOWN_DIR/conf"
mkdir -p "$CONF_DIR"
if ! find "$CONF_DIR" -maxdepth 1 \( -name '*.toml' -o -name '*.json' \) 2>/dev/null | grep -q .; then
    ui_print "- 首次部署 L3 conf 模板（观望模式 + 情景预设示例）..."
    cp "$MODPATH/conf/"*.toml "$CONF_DIR/" 2>/dev/null
    chmod 0644 "$CONF_DIR/"*.toml 2>/dev/null
else
    ui_print "- conf 已存在配置，保留（用户配置优先）。"
fi

rm -f "$MI_PROP_FILE" "$OPLUS_PROP_FILE" "$QTI_PROP_FILE"

ui_print " "
ui_print "========================================="
ui_print "      🌇 Sundown 安装完成"
ui_print "========================================="
ui_print " "
ui_print "- 日落而息 · 墓碑调度"
ui_print "- 重启设备以激活所有功能"
ui_print "- 管理入口: KernelSU 管理器 → 模块 → Sundown → WebUI"
ui_print " "