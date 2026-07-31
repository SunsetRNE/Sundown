#include "mini_art_elf.h"

#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

namespace {

// ELF64 最小常量集（避免依赖 <elf.h> 的平台差异；数值为 ELF 规范定值）
constexpr uint8_t ELFCLASS64 = 2;
constexpr uint16_t ET_DYN = 3;
constexpr uint16_t EM_AARCH64 = 183;
constexpr uint32_t SHT_SYMTAB = 2;
constexpr uint32_t SHT_DYNSYM = 11;

#pragma pack(push, 1)
struct Elf64Ehdr {
    uint8_t ident[16];
    uint16_t type, machine;
    uint32_t version;
    uint64_t entry, phoff, shoff;
    uint32_t flags;
    uint16_t ehsize, phentsize, phnum, shentsize, shnum, shstrndx;
};
struct Elf64Shdr {
    uint32_t name, type;
    uint64_t flags, addr, offset, size;
    uint32_t link, info;
    uint64_t addralign, entsize;
};
struct Elf64Sym {
    uint32_t name;
    uint8_t info, other;
    uint16_t shndx;
    uint64_t value, size;
};
#pragma pack(pop)

bool ends_with(const std::string &s, const char *suffix) {
    size_t n = std::strlen(suffix);
    return s.size() >= n && s.compare(s.size() - n, n, suffix) == 0;
}

} // namespace

bool MiniArtElf::load() {
    // ---- 1) /proc/self/maps 定位 libart.so：取首条 offset=0 的可读映射 ----
    FILE *maps = std::fopen("/proc/self/maps", "r");
    if (!maps) return false;
    char line[512];
    uintptr_t base = 0;
    std::string path;
    while (std::fgets(line, sizeof(line), maps)) {
        if (!std::strstr(line, "libart.so")) continue;
        // 格式：start-end perms offset dev inode path
        unsigned long long start = 0, offset = 0;
        char perms[8] = {0}, map_path[384] = {0};
        int matched = std::sscanf(line, "%llx-%*x %7s %llx %*s %*s %383[^\n]",
                                  &start, perms, &offset, map_path);
        if (matched < 4 || offset != 0 || map_path[0] != '/') continue;
        if (!ends_with(map_path, "libart.so")) continue;
        base = (uintptr_t)start;
        path = map_path;
        break;
    }
    std::fclose(maps);
    if (base == 0 || path.empty()) return false;

    // ---- 2) mmap 磁盘文件（section header/symtab 不加载进内存，必须读文件） ----
    int fd = ::open(path.c_str(), O_RDONLY | O_CLOEXEC);
    if (fd < 0) return false;
    struct stat st {};
    if (::fstat(fd, &st) < 0 || st.st_size < (off_t)sizeof(Elf64Ehdr)) {
        ::close(fd);
        return false;
    }
    const uint8_t *map = (const uint8_t *)::mmap(nullptr, st.st_size, PROT_READ,
                                                 MAP_PRIVATE, fd, 0);
    ::close(fd);
    if (map == MAP_FAILED) return false;

    // ---- 3) ELF 头校验 + 节表索引 ----
    const auto *eh = (const Elf64Ehdr *)map;
    if (std::memcmp(eh->ident, "\x7f""ELF", 4) != 0 || eh->ident[4] != ELFCLASS64 ||
        eh->type != ET_DYN || eh->machine != EM_AARCH64 ||
        eh->shoff == 0 || eh->shnum == 0 || eh->shentsize != sizeof(Elf64Shdr)) {
        ::munmap((void *)map, st.st_size);
        return false;
    }
    if (eh->shoff + (uint64_t)eh->shnum * sizeof(Elf64Shdr) > (uint64_t)st.st_size ||
        eh->shstrndx >= eh->shnum) {
        ::munmap((void *)map, st.st_size);
        return false;
    }
    const auto *shdrs = (const Elf64Shdr *)(map + eh->shoff);
    const auto *shstr = (const char *)(map + shdrs[eh->shstrndx].offset);

    SymTab dynsym{}, symtab{};
    for (uint16_t i = 0; i < eh->shnum; i++) {
        const Elf64Shdr &s = shdrs[i];
        if (s.type != SHT_DYNSYM && s.type != SHT_SYMTAB) continue;
        if (s.offset + s.size > (uint64_t)st.st_size || s.entsize < sizeof(Elf64Sym) ||
            s.link >= eh->shnum) continue;
        const Elf64Shdr &strsec = shdrs[s.link];
        if (strsec.offset + strsec.size > (uint64_t)st.st_size) continue;
        SymTab t{map + s.offset, s.size / s.entsize, s.entsize,
                 (const char *)(map + strsec.offset), strsec.size};
        const char *sec_name = shstr + s.name;
        if (s.type == SHT_DYNSYM && std::strcmp(sec_name, ".dynsym") == 0) dynsym = t;
        if (s.type == SHT_SYMTAB && std::strcmp(sec_name, ".symtab") == 0) symtab = t;
    }
    if (dynsym.sym == nullptr) { // .dynsym 必有，否则判定异常文件
        ::munmap((void *)map, st.st_size);
        return false;
    }

    base_ = base;
    path_ = path;
    map_ = map;
    map_size_ = (size_t)st.st_size;
    dynsym_ = dynsym;
    symtab_ = symtab;
    return true;
}

const void *MiniArtElf::find(const SymTab &t, std::string_view name, bool prefix) const {
    if (t.sym == nullptr || t.str == nullptr) return nullptr;
    for (size_t i = 0; i < t.count; i++) {
        const auto *sym = (const Elf64Sym *)(t.sym + i * t.entsize);
        if (sym->name >= t.str_size || sym->value == 0) continue;
        const char *n = t.str + sym->name;
        bool hit = prefix ? (std::strncmp(n, name.data(), name.size()) == 0)
                          : (name == std::string_view(n));
        if (hit) return (const void *)(base_ + (uintptr_t)sym->value);
    }
    return nullptr;
}

void *MiniArtElf::resolve(std::string_view name) const {
    if (base_ == 0) return nullptr;
    if (const void *p = find(dynsym_, name, false)) return (void *)p;
    return (void *)find(symtab_, name, false);
}

void *MiniArtElf::resolve_prefix(std::string_view prefix) const {
    if (base_ == 0) return nullptr;
    if (const void *p = find(dynsym_, prefix, true)) return (void *)p;
    return (void *)find(symtab_, prefix, true);
}