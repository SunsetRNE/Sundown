/*
 * madvise_test.c —— A1 解冻预热链实机探针（缺口补入清单 A1 配套）
 *
 * 与 daemon/src/freezer.rs probe_madvise_willneed() 同参数纪律：
 *   1. pidfd_open(pid, 0)          syscall 434
 *   2. 读 /proc/<pid>/maps 收集可读映射（perms 含 'r'，跳过 [内核特殊段）
 *   3. clamp：段数 ≤64、单段 ≤8MB（防 IO 风暴）
 *   4. process_madvise(pidfd, iovs, vlen, MADV_WILLNEED, 0)  syscall 440
 *
 * 用途：真机验证 ColorOS/AOSP 内核 process_madvise 可用性，输出 errno 判读，
 * 用于区分「内核不支持 syscall」vs「权限/参数」问题，指导解冻预热是否可启用。
 *
 * 编译（Android 设备/交叉）：
 *   aarch64-linux-gnu-gcc -static -O2 -o madvise_test madvise_test.c
 * 或设备端 termux：gcc -O2 -o madvise_test madvise_test.c
 *
 * 运行：
 *   ./madvise_test [pid]        # 缺省 pid=1（system_server 代表，权限最严格场景）
 * 退出码：0=成功；1=失败（含不支持）；2=参数错误
 */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#ifndef __NR_pidfd_open
#define __NR_pidfd_open 434 /* arm64/x86_64/arm EABI 一致 */
#endif
#ifndef __NR_process_madvise
#define __NR_process_madvise 440
#endif
#ifndef MADV_WILLNEED
#define MADV_WILLNEED 3
#endif

#define MAX_IOVS 64
#define MAX_SEG (8 << 20) /* 单段 clamp 8MB，与 freezer.rs 一致 */

static const char *errno_interpret(int e)
{
    switch (e) {
    case ENOSYS:
        return "内核不支持 process_madvise syscall（旧内核 <5.10 或无 CONFIG_PROCESS_MADVISE）→ 解冻预热永久降级";
    case EINVAL:
        return "MADV_WILLNEED 不支持 或 iov 参数非法（低版本内核 MADV_WILLNEED 不支持跨进程）";
    case EPERM:
        return "权限不足（Android SELinux 限制跨进程 madvise）";
    case ESRCH:
        return "目标进程不存在（已退出）";
    case EBADF:
        return "pidfd 无效";
    case EFAULT:
        return "iov 指针/地址非法（理论上不会，进程已退出竞态）";
    case EIO:
        return "内核内部 IO 错误";
    default:
        return "未知错误（查 errno 表）";
    }
}

int main(int argc, char **argv)
{
    long pid = 1; /* 缺省 system_server */
    if (argc > 2) {
        fprintf(stderr, "用法: %s [pid]（缺省 1=system_server）\n", argv[0]);
        return 2;
    }
    if (argc == 2) {
        char *end = NULL;
        pid = strtol(argv[1], &end, 10);
        if (!end || *end != '\0' || pid <= 0) {
            fprintf(stderr, "pid 非法: %s\n", argv[1]);
            return 2;
        }
    }

    printf("=== madvise_test: pid=%ld ===\n", pid);
    printf("syscall pidfd_open=%d process_madvise=%d MADV_WILLNEED=%d\n",
           __NR_pidfd_open, __NR_process_madvise, MADV_WILLNEED);

    /* 1. pidfd_open（目标进程无需合作） */
    int pidfd = (int)syscall(__NR_pidfd_open, pid, 0);
    if (pidfd < 0) {
        printf("[FAIL] pidfd_open(%ld) errno=%d (%s): %s\n",
               pid, errno, strerror(errno), errno_interpret(errno));
        if (errno == ENOSYS)
            printf("[判读] pidfd_open 都不可用 → 内核太旧，解冻预热不可用\n");
        return 1;
    }
    printf("[ OK ] pidfd_open(%ld) -> fd=%d\n", pid, pidfd);

    /* 2. 收集可读映射（同 freezer.rs 纪律：'r' 权限、跳过 [段、clamp 64 段/8MB） */
    char path[64];
    snprintf(path, sizeof(path), "/proc/%ld/maps", pid);
    FILE *fp = fopen(path, "r");
    if (!fp) {
        printf("[FAIL] fopen(%s) errno=%d (%s)\n", path, errno, strerror(errno));
        close(pidfd);
        return 1;
    }
    struct iovec iovs[MAX_IOVS];
    int n = 0;
    char line[512];
    while (n < MAX_IOVS && fgets(line, sizeof(line), fp)) {
        char perms[8] = {0}, pathname[256] = {0};
        unsigned long long start = 0, end = 0;
        int cnt = sscanf(line, "%llx-%llx %7s %*s %*s %*s %255s",
                         &start, &end, perms, pathname);
        if (cnt < 3 || !strchr(perms, 'r') || end <= start)
            continue;
        if (pathname[0] == '[') /* [vvar]/[vdso]/[vsyscall] 内核特殊段 */
            continue;
        unsigned long long len = end - start;
        if (len > MAX_SEG)
            len = MAX_SEG; /* clamp 单段 8MB */
        iovs[n].iov_base = (void *)(uintptr_t)start;
        iovs[n].iov_len = (size_t)len;
        n++;
    }
    fclose(fp);
    printf("[INFO] 收集可读映射段: %d 段（clamp 上限 %d，单段 ≤%dMB）\n",
           n, MAX_IOVS, MAX_SEG >> 20);
    if (n == 0) {
        printf("[FAIL] 无可用映射段（fopen ok 但无 'r' 段？）\n");
        close(pidfd);
        return 1;
    }

    /* 3. process_madvise(MADV_WILLNEED) */
    errno = 0;
    ssize_t ret = syscall(__NR_process_madvise, pidfd, iovs, (size_t)n,
                          MADV_WILLNEED, 0);
    int e = errno;
    printf("[INFO] raw syscall ret=%ld errno=%d\n", (long)ret, e);
    printf("[INFO] iovs[0].iov_base=%p iov_len=%zu（对比 ret 是否=iov_base 残留）\n",
           iovs[0].iov_base, iovs[0].iov_len);
    if (ret == 0) {
        printf("[ OK ] process_madvise(MADV_WILLNEED) 成功：%d 段预热下发\n", n);
        printf("[判读] 内核支持跨进程 WILLNEED → 解冻预热链可用\n");
        close(pidfd);
        return 0;
    }
    /* 内核直接返回负 errno 而 glibc 未映射（seccomp/私有约定）时 ret 可能为 -E 原值 */
    if (ret < 0 && ret > -4096) {
        int raw_e = (int)(-ret);
        printf("[判读] 内核返回负 errno 原值: -%d (%s)\n", raw_e, strerror(raw_e));
        printf("[判读] %s\n", errno_interpret(raw_e));
        close(pidfd);
        return 1;
    }
    /* 实机实证（ColorOS/Android16，2026-08-12）：440 返回正数 0x74F000/0xF0B000 且
     * errno=0，非任何标准 errno —— 疑似 OEM 内核私有 syscall 占用 440 号或 process_madvise
     * 未实现且 syscall 表偏移。与 daemon（bionic）capability 矩阵 madvise_willneed=false
     * 一致：解冻预热在该内核不可用，自动降级失败安全（不阻塞解冻）。 */
    if (ret > 0) {
        printf("[判读] 440 返回正数 %ld 且 errno=0 → OEM 内核私有 syscall 占用/未实现标准 process_madvise\n", (long)ret);
        printf("[判读] 解冻预热不可用（与 daemon madvise_willneed=false 一致），自动降级失败安全\n");
        close(pidfd);
        return 1;
    }
    printf("[FAIL] process_madvise errno=%d (%s)\n", e, strerror(e));
    printf("[判读] %s\n", errno_interpret(e));
    if (e == EINVAL) {
        printf("[建议] 试 flags=0 已用；若仍 EINVAL 大概率 MADV_WILLNEED 跨进程不支持\n");
    }
    close(pidfd);
    return 1;
}
