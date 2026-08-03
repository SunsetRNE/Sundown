cmake_minimum_required(VERSION 3.22.1)
project(sunprobe CXX)

# libsunprobe.so —— Sundown L1 Zygisk 探针桩
# 设计约束：本桩"几乎不允许更新"（更新需软重启 zygote），
# 因此编译配置求稳：无异常、无 RTTI、符号隐藏、MinSizeRel。

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

set(CXX_FLAGS "-fno-exceptions -fno-rtti -fvisibility=hidden -fvisibility-inlines-hidden -fno-threadsafe-statics")
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} ${CXX_FLAGS}")

if(NOT DEFINED PROBE_BUILD_HASH)
    set(PROBE_BUILD_HASH "dev")
endif()
message(STATUS "PROBE_BUILD_HASH = ${PROBE_BUILD_HASH}")

add_library(sunprobe SHARED src/probe.cpp)
target_include_directories(sunprobe PRIVATE include)
target_compile_definitions(sunprobe PRIVATE PROBE_BUILD_HASH="${PROBE_BUILD_HASH}")
target_link_libraries(sunprobe PRIVATE log)

# 产物 strip；未 strip 版本留在构建目录供排障
add_custom_command(TARGET sunprobe POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy $<TARGET_FILE:sunprobe> ${CMAKE_BINARY_DIR}/libsunprobe.unstripped.so
    COMMAND ${CMAKE_STRIP} --strip-all $<TARGET_FILE:sunprobe>
)
