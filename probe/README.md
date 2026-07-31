# probe/ —— libsunprobe.so（L1 Zygisk 探针桩）

## 定位与铁律

**本桩"几乎不允许更新"**：`libsunprobe.so` 由 Zygisk 提供方（ReZygisk/ZygiskNext）
在 zygote 启动时加载，任何变更必须**软重启 zygote** 才生效（`sunctl restart-runtime --yes`）。
因此桩内只保留三件稳定职责，一切可变逻辑下沉 L2 `probe.dex`：

1. **识别 system_server 并驻留**（`preAppSpecialize` 对普通 app 立即 `DLCLOSE`，零侵入）
2. **hello-probe 握手**：连接 daemon 上报编译期 build hash（带重试，见下节）
3. **加载 probe.dex 并调用入口**（L2 契约，见下），随后控制权完全移交 dex 层

## 桩 ↔ daemon 通道：abstract namespace socket（踩坑定稿）

桩运行在 system_server(uid 1000) 内，**不能**用文件 socket 连 daemon：
`/data/adb` 是 `drwx------ root root`，DAC 层（传统文件权限）直接 EACCES——
不走 SELinux，所以 **avc 日志里看不到任何痕迹**，极易误判为策略缺失。

定稿通道：**abstract namespace socket `sundown_probe`**（无文件系统路径）。
daemon 双监听：文件 socket 服务 root 管理面（sunctl/WebUI），abstract 服务桩/dex。
SELinux 侧 `allow system_server ksu unix_stream_socket connectto`（sepolicy.rule）
已覆盖该通道；Java 侧（L2 dex）用 `LocalSocketAddress(..., Namespace.ABSTRACT)` 连接。

## 开机时序与握手重试（踩坑定稿）

```
开机早期：zygote fork system_server → 桩 postServerSpecialize
          → 此时 sundownd.sock 还不存在（daemon 未拉起）
boot completed 后：service.sh 才启动 sundownd → socket 开始监听
```

**一次性握手在开机时必然失败**（ENOENT 层面失败，不触发 SELinux，无 avc 痕迹）。
因此握手整体挪入**后台线程**（不阻塞 system_server 启动），每 2s 重试、
窗口 120s 覆盖 daemon 拉起延迟；dex 加载前 `AttachCurrentThread` 获取
本线程 JNIEnv（后台线程不能复用 specialize 线程的 env）。

## build hash 验证闭环

```
CI 构建：git short sha ──(-DPROBE_BUILD_HASH)──► libsunprobe.so 内嵌
                 └────────(写入)────────► module/zygisk/probe.hash（期望值）

运行时：zygote 加载桩 ──hello-probe <hash>──► sundownd
        daemon 比对期望值 → status JSON: probe_stub_loaded / probe_stub_build_hash
        sunctl status / WebUI 可见 → 软重启后 hash 匹配 = 桩激活成功
```

- daemon 应答：`{"ok":1,"hash_match":1|0|-1,"expected_hash":"...","dex_path":"...","dex_present":0|1}`
  （`hash_match=-1` 表示模块内无 probe.hash，本地 dev 构建场景）
- **L2 起 `dex_path` 一律指向 magic-mount 的 `/system/etc/sundown/probe.dex`**
  （uid 1000 可读、SELinux `system_file` 无争议），不再指向 `/data/adb` 下任何路径——
  该目录 `drwx------ root`，桩/dex 层在 DAC 层不可达（同上方踩坑定稿）
- `probe-query` 命令提供相同应答但无记录副作用（L2 dex 层轮询用）

## L2 契约（dex 层入口）

```java
package ren.sunset.sundown;
public final class ProbeMain {
    /** 由 L1 桩在 system_server 中经 DexClassLoader 调用（冷启动，dex 走 dex_path） */
    public static void init(String socketPath, String stubBuildHash) { ... }
    /** 热切换：旧代 ClassLoader 反射调用新代（跨 ClassLoader 只传 bootstrap 类型） */
    public static boolean hotSwap(String socketPath, String stubBuildHash) { ... }
}
```

dex 层自此接管：连 daemon 订阅事件、热切换（`InMemoryDexClassLoader` 重载自身，
失败回滚铁律）、Hook 编排（LSPlant 骨架）。桩不再参与，这是"桩不更新"的关键。
协议三命令（`hello-dex`/`fetch-dex`/`push-dex`）、热切换时序、版本闭环详见
[dex/README.md](../dex/README.md)。

## 构建

CI 自动完成（`.github/workflows/build.yml` 的 `build-probe` job）：
runner 自带 NDK + CMake + Ninja，ABI 仅 **arm64-v8a**
（system_server 是纯 64 位进程；32 位 zygote 只跑 app，需要时再补 armeabi-v7a）。
产物布置到 `module/zygisk/arm64-v8a.so` + `module/zygisk/probe.hash`。

本地构建（需 Android NDK）：

```sh
cd probe
cmake -B build -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$ANDROID_NDK_HOME/build/cmake/android.toolchain.cmake" \
  -DANDROID_ABI=arm64-v8a -DANDROID_PLATFORM=android-30 \
  -DCMAKE_BUILD_TYPE=MinSizeRel -DPROBE_BUILD_HASH="dev"
cmake --build build   # 产物 build/libsunprobe.so（已 strip）
```

## SELinux 备忘

`module/sepolicy.rule` 已放行 `system_server → su/magisk/kernelsu/ksu` 域的
unix_stream_socket connectto（KSU 的 su 域覆盖桩连 daemon 的场景）。
若真机仍见 `avc: denied`：`dmesg | grep avc` 取证后按需补
`adb_data_file sock_file` 类规则。**修改本文件即 L1 级变更，需软重启。**

## 修改红线

改 `src/probe.cpp` 任何一行 = L1 级变更 = 用户必须软重启。
新需求先问：能否放进 L2 probe.dex？（答案几乎总是"能"）
