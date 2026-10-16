/* Memory semantics programs rely on: fork gives the child a copy
   (copy-on-write underneath), MAP_SHARED memory is shared across fork,
   files mapped MAP_SHARED are shared between processes and with
   read/write, MAP_PRIVATE file mappings are copies, memfd_create with
   seals, shm_open, and mapping files much bigger than what's touched. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-56s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

/* Runs `child` in a forked process; returns its exit status. */
static int in_child(int (*child)(void *), void *argument) {
    pid_t pid = fork();
    if (pid == 0) _exit(child(argument));
    int status = 0;
    waitpid(pid, &status, 0);
    return WIFEXITED(status) ? WEXITSTATUS(status) : 255;
}

static int write_and_check(void *argument) {
    char *buffer = argument;
    if (strcmp(buffer, "parent's data") != 0) return 1;
    strcpy(buffer, "child's data");
    return strcmp(buffer, "child's data") == 0 ? 0 : 2;
}

static int write_shared(void *argument) {
    strcpy(argument, "written by the child");
    return 0;
}

static int write_file_through_mapping(void *argument) {
    int fd = open(argument, O_RDWR);
    if (fd < 0) return 1;
    char *map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) return 2;
    memcpy(map + 100, "from the other process", 22);
    munmap(map, 4096);
    close(fd);
    return 0;
}

/* Reads into a buffer over and over, so a system call is often writing to
   memory a fork has just made copy-on-write. */
static volatile int stop_reading = 0;
static char read_buffer[256 * 1024];

static void *keep_reading(void *argument) {
    (void)argument;
    int zero = open("/dev/zero", O_RDONLY);
    long rounds = 0;
    while (!stop_reading) {
        (void)!read(zero, read_buffer, sizeof read_buffer);
        rounds++;
    }
    close(zero);
    return (void *)rounds;
}

static sigjmp_buf recover;

static void on_segv(int signal) {
    (void)signal;
    siglongjmp(recover, 1);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("Memory test (pid %d)\n", getpid());

    /* fork: the child sees the parent's memory, and its writes stay its own. */
    char *heap = malloc(64 * 1024);
    strcpy(heap, "parent's data");
    char stack[64];
    strcpy(stack, "parent's data");
    check("a forked child reads and writes its copy of the heap", in_child(write_and_check, heap) == 0);
    check("the parent's heap is unchanged", strcmp(heap, "parent's data") == 0);
    check("... and its stack", in_child(write_and_check, stack) == 0 && strcmp(stack, "parent's data") == 0);
    strcpy(heap, "changed after fork");
    check("the parent can write after the child is gone", strcmp(heap, "changed after fork") == 0);

    /* Copy-on-write survives mprotect: a page made read-only and writable
       again is still the parent's own after a fork. */
    char *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    strcpy(page, "parent's data");
    pid_t pid = fork();
    if (pid == 0) {
        mprotect(page, 4096, PROT_READ);
        mprotect(page, 4096, PROT_READ | PROT_WRITE);
        strcpy(page, "child's data");
        _exit(0);
    }
    waitpid(pid, NULL, 0);
    check("mprotect in the child doesn't expose the parent's page", strcmp(page, "parent's data") == 0);

    /* MAP_SHARED | MAP_ANONYMOUS: shared with children. */
    char *shared = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    check("shared anonymous memory can be mapped", shared != MAP_FAILED);
    strcpy(shared + 4096, "parent");
    check("a child's writes to shared memory reach the parent",
          in_child(write_shared, shared + 4096) == 0 && strcmp(shared + 4096, "written by the child") == 0);

    /* A file mapped MAP_SHARED: shared with other processes and with read. */
    const char *path = "/tmp/memtest-file";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char block[8192];
    memset(block, 'a', sizeof block);
    (void)!write(fd, block, sizeof block);
    char *file = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check("a file can be mapped shared and writable", file != MAP_FAILED);
    check("the mapping shows the file's contents", file[0] == 'a' && file[8191] == 'a');
    memcpy(file + 10, "through memory", 14);
    char readback[14];
    (void)!pread(fd, readback, 14, 10);
    check("writes through the mapping are seen by read", memcmp(readback, "through memory", 14) == 0);
    (void)!pwrite(fd, "by write", 8, 4096);
    check("writes by write are seen through the mapping", memcmp(file + 4096, "by write", 8) == 0);
    check("another process's mapping of the file is shared",
          in_child(write_file_through_mapping, (void *)path) == 0 && memcmp(file + 100, "from the other process", 22) == 0);

    /* MAP_PRIVATE: a copy. */
    char *private = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    check("a private mapping shows the file", memcmp(private + 10, "through memory", 14) == 0);
    memcpy(private + 10, "only mine.....", 14);
    (void)!pread(fd, readback, 14, 10);
    check("writing a private mapping leaves the file alone", memcmp(readback, "through memory", 14) == 0);
    check("... and the shared mapping", memcmp(file + 10, "through memory", 14) == 0);
    munmap(private, 8192);
    munmap(file, 8192);
    close(fd);

    /* A big file, mapped whole and touched sparsely. */
    fd = open("/tmp/memtest-big", O_RDWR | O_CREAT | O_TRUNC, 0644);
    (void)!ftruncate(fd, 96 << 20);
    (void)!pwrite(fd, "end", 3, (96 << 20) - 3);
    char *big = mmap(NULL, 96 << 20, PROT_READ, MAP_PRIVATE, fd, 0);
    check("a 96 MiB file can be mapped", big != MAP_FAILED);
    check("its last bytes are there, and the rest is zero",
          big != MAP_FAILED && memcmp(big + (96 << 20) - 3, "end", 3) == 0 && big[50 << 20] == 0);
    if (big != MAP_FAILED) munmap(big, 96 << 20);
    close(fd);
    unlink("/tmp/memtest-big");
    unlink(path);

    /* memfd_create and seals. */
    int memfd = memfd_create("memtest", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    check("memfd_create makes a file", memfd >= 0);
    check("it can be sized", ftruncate(memfd, 16384) == 0);
    char *memory = mmap(NULL, 16384, PROT_READ | PROT_WRITE, MAP_SHARED, memfd, 0);
    strcpy(memory + 8192, "in a memfd");
    char back[11] = {0};
    (void)!pread(memfd, back, 10, 8192);
    check("its mapping and its contents are the same", strcmp(back, "in a memfd") == 0);
    check("it can be sealed against shrinking", fcntl(memfd, F_ADD_SEALS, F_SEAL_SHRINK) == 0);
    check("... which then fails", ftruncate(memfd, 4096) == -1 && errno == EPERM);
    check("... and the seals can be read", fcntl(memfd, F_GET_SEALS) == F_SEAL_SHRINK);
    check("growing is still allowed", ftruncate(memfd, 32768) == 0);
    int unsealable = memfd_create("plain", 0);
    check("without MFD_ALLOW_SEALING, sealing fails",
          fcntl(unsealable, F_ADD_SEALS, F_SEAL_SHRINK) == -1 && errno == EPERM);
    close(unsealable);
    munmap(memory, 16384);
    close(memfd);

    /* shm_open: files in /dev/shm. */
    int shm = shm_open("/memtest", O_RDWR | O_CREAT, 0600);
    check("shm_open works", shm >= 0 && ftruncate(shm, 4096) == 0);
    char *one = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, shm, 0);
    int again = shm_open("/memtest", O_RDWR, 0);
    char *two = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, again, 0);
    strcpy(one, "same pages");
    check("two mappings of a shm object are the same memory", strcmp(two, "same pages") == 0);
    shm_unlink("/memtest");
    close(shm);
    close(again);

    /* mremap: growing a mapping keeps its contents, moving it if needed. */
    char *small = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *blocker = mmap(small + 8192, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    strcpy(small, "kept");
    strcpy(small + 4096, "second page");
    char *grown = mremap(small, 8192, 1 << 20, MREMAP_MAYMOVE);
    check("mremap grows a mapping (moving it)", grown != MAP_FAILED && grown != small);
    check("... keeping its contents", grown != MAP_FAILED && strcmp(grown, "kept") == 0 && strcmp(grown + 4096, "second page") == 0);
    if (grown != MAP_FAILED) {
        grown[(1 << 20) - 1] = 1;
        check("... and the new part is usable", mremap(grown, 1 << 20, 4096, 0) == grown && strcmp(grown, "kept") == 0);
        munmap(grown, 4096);
    }
    munmap(blocker, 4096);

    /* Forking while another thread's system call writes to memory. */
    pthread_t reader;
    pthread_create(&reader, NULL, keep_reading, NULL);
    int forks_ok = 1;
    for (int i = 0; i < 100; i++) {
        pid_t child = fork();
        if (child == 0) _exit(read_buffer[0] == 0 ? 0 : 1);
        int status;
        waitpid(child, &status, 0);
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) forks_ok = 0;
    }
    stop_reading = 1;
    void *rounds;
    pthread_join(reader, &rounds);
    check("forking while another thread reads into memory", forks_ok && (long)rounds > 0);

    /* Unmapped memory faults. */
    char *gone = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    gone[0] = 1;
    munmap(gone, 4096);
    signal(SIGSEGV, on_segv);
    volatile int faulted = 0;
    if (sigsetjmp(recover, 1) == 0) {
        gone[0] = 2;
    } else {
        faulted = 1;
    }
    check("touching unmapped memory is a segmentation fault", faulted);

    printf("%s\n", failures ? "SOME TESTS FAILED" : "all memory tests passed");
    return failures ? 1 : 0;
}
