#pragma once
// mini_art_elf：libart.so 符号解析器（LSPlant InitInfo.art_symbol_resolver 用）
//
// 为什么自研：官方 lsparself 是 git submodule（codeload 源码包不含），
// 且需求面很小——/proc/self/maps 定位内存基址 + 读磁盘 ELF 解 .dynsym/.symtab。
// 磁盘 libart（/apex/com.android.art/lib64/libart.so）uid 1000 可读，LSPosed 同款路径。

#include <cstdint>
#include <string>
#include <string_view>

class MiniArtElf {
public:
    /// 定位进程中已加载的 libart.so（maps 首条 offset=0 映射取基址+磁盘路径），
    /// mmap 磁盘文件并建立 .dynsym / .symtab 索引。失败返回 false（bridge 降级不 hook）。
    bool load();

    /// 精确符号解析：.dynsym 优先，.symtab 兜底。返回 运行地址 = 基址 + st_value。
    void *resolve(std::string_view name) const;

    /// 前缀符号解析（首个匹配，.dynsym 优先 .symtab 兜底）。
    void *resolve_prefix(std::string_view prefix) const;

private:
    struct SymTab {
        const uint8_t *sym = nullptr;   // Elf64_Sym 数组
        size_t count = 0;
        size_t entsize = 0;
        const char *str = nullptr;      // 关联字符串表
        size_t str_size = 0;
    };

    uintptr_t base_ = 0;
    std::string path_;
    const uint8_t *map_ = nullptr;      // mmap 只读视图
    size_t map_size_ = 0;
    SymTab dynsym_;
    SymTab symtab_;

    const void *find(const SymTab &t, std::string_view name, bool prefix) const;
};
