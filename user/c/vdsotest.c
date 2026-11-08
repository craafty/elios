/* The vDSO: finds it the way C libraries do (AT_SYSINFO_EHDR, then the
   dynamic section and its SysV hash table, as musl reads it), calls its
   functions directly, and checks them against the system calls. */
#define _GNU_SOURCE
#include <elf.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-56s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

static void *vdso_symbol(const unsigned char *base, const char *name) {
    const Elf64_Ehdr *header = (const Elf64_Ehdr *)base;
    const Elf64_Phdr *headers = (const Elf64_Phdr *)(base + header->e_phoff);
    uintptr_t load = (uintptr_t)-1;
    const Elf64_Dyn *dynamic = NULL;
    for (int i = 0; i < header->e_phnum; i++) {
        if (headers[i].p_type == PT_LOAD && load == (uintptr_t)-1) load = (uintptr_t)base + headers[i].p_offset - headers[i].p_vaddr;
        if (headers[i].p_type == PT_DYNAMIC) dynamic = (const Elf64_Dyn *)(base + headers[i].p_offset);
    }
    if (!dynamic || load == (uintptr_t)-1) return NULL;
    const char *strings = NULL;
    const Elf64_Sym *symbols = NULL;
    const uint32_t *hash = NULL;
    for (; dynamic->d_tag != DT_NULL; dynamic++) {
        if (dynamic->d_tag == DT_STRTAB) strings = (const char *)(load + dynamic->d_un.d_ptr);
        if (dynamic->d_tag == DT_SYMTAB) symbols = (const Elf64_Sym *)(load + dynamic->d_un.d_ptr);
        if (dynamic->d_tag == DT_HASH) hash = (const uint32_t *)(load + dynamic->d_un.d_ptr);
    }
    if (!strings || !symbols || !hash) return NULL;
    for (uint32_t i = 0; i < hash[1]; i++) {
        if (ELF64_ST_TYPE(symbols[i].st_info) == STT_FUNC && symbols[i].st_shndx != SHN_UNDEF &&
            strcmp(strings + symbols[i].st_name, name) == 0)
            return (void *)(load + symbols[i].st_value);
    }
    return NULL;
}

static int64_t nanos(const struct timespec *t) { return (int64_t)t->tv_sec * 1000000000 + t->tv_nsec; }

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("vDSO test (pid %d)\n", getpid());
    const unsigned char *base = (const unsigned char *)getauxval(AT_SYSINFO_EHDR);
    check("AT_SYSINFO_EHDR points at an ELF image", base && memcmp(base, ELFMAG, SELFMAG) == 0);
    if (!base) return 1;

    long (*vdso_clock_gettime)(long, struct timespec *) = vdso_symbol(base, "__vdso_clock_gettime");
    long (*vdso_gettimeofday)(struct timeval *, void *) = vdso_symbol(base, "__vdso_gettimeofday");
    long (*vdso_time)(time_t *) = vdso_symbol(base, "__vdso_time");
    check("it exports clock_gettime, gettimeofday and time", vdso_clock_gettime && vdso_gettimeofday && vdso_time);
    if (!vdso_clock_gettime || !vdso_gettimeofday || !vdso_time) return 1;

    const long clocks[] = {CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_REALTIME_COARSE,
                           CLOCK_MONOTONIC_COARSE, CLOCK_BOOTTIME};
    int ordered = 1;
    for (unsigned i = 0; i < sizeof clocks / sizeof clocks[0]; i++) {
        for (int round = 0; round < 1000; round++) {
            struct timespec before, during, after;
            syscall(SYS_clock_gettime, clocks[i], &before);
            long result = vdso_clock_gettime(clocks[i], &during);
            syscall(SYS_clock_gettime, clocks[i], &after);
            /* The same formula on both sides; allow a little for the TSC
               being read out of order. */
            if (result != 0 || during.tv_nsec >= 1000000000 || nanos(&during) + 1000 < nanos(&before) ||
                nanos(&during) > nanos(&after) + 1000) {
                printf("    clock %ld: %ld.%09ld <= %ld.%09ld <= %ld.%09ld? (result %ld)\n", clocks[i],
                       before.tv_sec, before.tv_nsec, during.tv_sec, during.tv_nsec, after.tv_sec,
                       after.tv_nsec, result);
                ordered = 0;
                break;
            }
        }
    }
    check("its clocks fall between the system call's readings", ordered);

    struct timespec spent;
    check("CPU-time clocks fall back to the system call",
          vdso_clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &spent) == 0 &&
              vdso_clock_gettime(CLOCK_THREAD_CPUTIME_ID, &spent) == 0);
    check("an unknown clock gives -EINVAL", vdso_clock_gettime(99, &spent) == -22 &&
                                                vdso_clock_gettime(-1, &spent) == -22);

    struct timeval now;
    struct timespec real;
    syscall(SYS_clock_gettime, CLOCK_REALTIME, &real);
    long result = vdso_gettimeofday(&now, NULL);
    int64_t difference = (int64_t)now.tv_sec * 1000000 + now.tv_usec - nanos(&real) / 1000;
    check("gettimeofday agrees with CLOCK_REALTIME",
          result == 0 && now.tv_usec < 1000000 && difference >= 0 && difference < 100000);
    struct timezone zone = {123, 456};
    check("gettimeofday fills in a time zone of UTC",
          vdso_gettimeofday(NULL, &zone) == 0 && zone.tz_minuteswest == 0 && zone.tz_dsttime == 0);

    time_t stored = 0;
    time_t seconds = vdso_time(&stored);
    long from_kernel = syscall(SYS_time, NULL);
    check("time agrees with the system call", seconds == stored && from_kernel - seconds >= 0 && from_kernel - seconds <= 1);

    /* glibc's own clock_gettime goes through the vDSO now. */
    struct timespec previous, current;
    clock_gettime(CLOCK_MONOTONIC, &previous);
    int forward = 1;
    for (int i = 0; i < 100000; i++) {
        clock_gettime(CLOCK_MONOTONIC, &current);
        if (nanos(&current) < nanos(&previous)) forward = 0;
        previous = current;
    }
    check("100000 clock_gettime calls never go backward", forward);

    printf(failures ? "%d FAILED\n" : "all passed\n", failures);
    return failures != 0;
}
