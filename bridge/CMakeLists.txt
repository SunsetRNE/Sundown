cmake_minimum_required(VERSION 3.22.1)
project(sundownhook CXX)

# libsundownhook —— L2 native 伴生库（LSPlant bridge，docs/l2b-plan.md §0.2）
# 依赖（CI 注入 prefab 路径，sha256 在 CI 下载时校验）：
#   -DLSPLANT_PREFAB=<lsplant-6.4.aar 解包>/prefab/modules/lsplant     （LGPL-3.0，动态链接）
#   -DDOBBY_PREFAB=<dobby-1.2.aar 解包>/prefab/modules/dobby           （Apache-2.0，静态）
#   -DBRIDGE_BUILD_HASH=<git short sha>                                （四位一体版本闭环）

set(CMAKE_CXX_STANDARD 20)        # lsplant.hpp + 指定初始化器
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_library(lsplant SHARED IMPORTED GLOBAL)
set_target_properties(lsplant PROPERTIES
        IMPORTED_LOCATION "${LSPLANT_PREFAB}/libs/android.arm64-v8a/liblsplant.so"
        INTERFACE_INCLUDE_DIRECTORIES "${LSPLANT_PREFAB}/include")

add_library(dobby STATIC IMPORTED GLOBAL)
set_target_properties(dobby PROPERTIES
        IMPORTED_LOCATION "${DOBBY_PREFAB}/libs/android.arm64-v8a/libdobby.a"
        INTERFACE_INCLUDE_DIRECTORIES "${DOBBY_PREFAB}/include")

add_library(sundownhook SHARED
        src/bridge.cpp
        src/mini_art_elf.cpp)

target_compile_definitions(sundownhook PRIVATE BRIDGE_BUILD_HASH="${BRIDGE_BUILD_HASH}")
target_compile_options(sundownhook PRIVATE
        -fvisibility=hidden -ffunction-sections -fdata-sections)
target_link_libraries(sundownhook PRIVATE lsplant dobby log)
target_link_options(sundownhook PRIVATE -Wl,--gc-sections -Wl,--exclude-libs,ALL)