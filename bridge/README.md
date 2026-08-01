# bridge/ —— libsundownhook.so（L2 native 伴生库）

L2 的 native 机制层（docs/l2b-plan.md §0.2 裁决）。由 dex 层 `System.load` 加载
（不经过 L1 桩，桩零触碰），向 `ren.sunset.sundown.hook.NativeBridge` 暴露五个机制出口：
`lsplant::Init` / `Hook` / `UnHook` / `MakeDexFileTrusted` + `BUILD_HASH` 上报。

**只做机制不做策略**：hook 哪些方法、命中后做什么，全部在 L2 dex（Java）侧。

## 依赖（CI 注入，sha256 校验）

| 依赖 | 来源 | 许可证 | 链接方式 |
|---|---|---|---|
| LSPlant master | GitHub `LSPosed/LSPlant` pinned `84256d4`（CI 源码构建，Android 15/16/17 适配链；Maven 官方 6.4 仅适配到 Android 14，见下方说明） | **LGPL-3.0** | 动态链接 `liblsplant.so`（随模块独立分发） |
| Dobby 1.2 | Maven `io.github.vvb2060.ndk:dobby:1.2`（LSPlant 官方 test 同款坐标） | Apache-2.0 | 静态链入 bridge |

> **为什么不用 Maven `lsplant:6.4`**（2024-04，官方最新 release）：
> Android 16 (API 36) 的 `libart.so` 相对 Android 14 发生大面积符号变更——Instrumentation
> （`InitializeMethodsCode`→`GetOptimizedCodeFor`/`ReinitializeMethodsCode`）、JIT、DexFile、
> GC 等签名全部变化，且新 NDK 工具链的 LOCAL 符号带 `.__uniq` 后缀导致 6.4 的精确解析失败，
> `lsplant::Init` 在 system_server 内直接返回 false（实机 V2 卡点，见 Sundown-实机巡检报告）。
> LSPlant master 已适配 API 36/37（含 Baklava/Cinnamon Bun 常量、Android 15+ jit 修复、
> memfd/SDK 37 ashmem 限制准备），故 CI 改为 pinned commit 源码构建。
> 构建产物静态链入 libc++（`-static-libstdc++`），**不引入 `libc++_shared.so` 依赖**
> （Android 16 实机 /system/lib64 无此库，缺失会直接导致 `System.load` 失败——历史教训）。

- LSPlant 官方 AAR 只发布 C++ API（`lsplant.hpp`）；Java 桥（`NativeBridge`/`Hooker`）
  按官方 test 模式 vendor 在 `dex/src/ren/sunset/sundown/hook/NativeBridge.java`
- LGPL-3.0 合规：本模块**动态链接**且不修改 LSPlant；LSPlant 源码：
  https://github.com/LSPosed/LSPlant （用户可自行重新构建替换 `liblsplant.so`）
- art 符号解析为自研 `mini_art_elf`（官方 lsparself 为 submodule 不分发；
  需求面 = maps 基址 + 磁盘 ELF `.dynsym`/`.symtab`）

## 构建（CI `build-bridge` job；本地需 NDK）

```sh
cmake -B build -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$ANDROID_NDK_HOME/build/cmake/android.toolchain.cmake" \
  -DANDROID_ABI=arm64-v8a -DANDROID_PLATFORM=android-30 \
  -DCMAKE_BUILD_TYPE=MinSizeRel \
  -DLSPLANT_PREFAB=/path/to/lsplant/prefab/modules/lsplant \
  -DDOBBY_PREFAB=/path/to/dobby/prefab/modules/dobby \
  -DBRIDGE_BUILD_HASH="dev"
cmake --build build   # 产物 build/libsundownhook.so
```

模块 zip 布局（CI 布置）：`system/lib64/libsundownhook.so` +
`system/lib64/liblsplant.so`（magic-mount，uid 1000 可读）+ `hook/hook.hash`
（期望 build hash = CI commit，daemon 比对 `report-bridge` 上报值，四位一体闭环）。

## 运行时链路

```
dex LsPlantBridge.loadNative():
  System.load("/system/lib64/liblsplant.so")     # LGPL 动态库，先加载
  System.load("/system/lib64/libsundownhook.so") # JNI_OnLoad:
    ├─ 预取 DexFile.mCookie jfieldID（hidden API 警告字段）
    ├─ mini_art_elf 定位 libart.so → lsplant::Init（dobby inline hook libart）
    └─ RegisterNatives(NativeBridge / NativeBridge$Hooker)
  NativeBridge.nativeInit() == true              # Init 结果
  NativeBridge.nativeMakeOwnDexTrusted(cl)       # 解除本 dex hidden API 限制
  → 之后 hook 组（FocusHooks/WakeupHooks）经 NativeBridge.hook() 安装
```

## 修改红线

改 `src/bridge.cpp` 任何一行 = 需**软重启 zygote** 才生效（与 L1 同级更新成本）。
机制面一次设计到位：新需求先问「能否只在 dex 侧做？」（答案几乎总是「能」）。