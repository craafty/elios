/* seccomp: filters that fail, trap and kill system calls, stacking,
   inheritance by threads and children, TSYNC, and strict mode. Each case
   runs in a child, since filters can't be removed. */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-56s %s\n", what, ok ? "ok" : "FAILED");
    fflush(stdout);
    if (!ok) failures++;
}

/* A filter returning `action` for system call `nr` and allowing the rest. */
static int install(int nr, unsigned action, unsigned flags) {
    struct sock_filter program[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, nr, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, action),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog fprog = {sizeof program / sizeof program[0], program};
    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, flags, &fprog);
}

/* Runs `body` in a child; returns its wait status. */
static int in_child(int (*body)(void)) {
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) _exit(body());
    int status = 0;
    waitpid(pid, &status, 0);
    return status;
}

static int errno_filter(void) {
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) return 10;
    if (install(SYS_getppid, SECCOMP_RET_ERRNO | 42, 0) != 0) return 11;
    if (syscall(SYS_getppid) != -1 || errno != 42) return 12;
    if (syscall(SYS_getpid) <= 0) return 13;
    if (prctl(PR_GET_SECCOMP) != 2) return 14;
    /* A newer filter with the same action wins with its data. */
    if (install(SYS_getppid, SECCOMP_RET_ERRNO | 5, 0) != 0) return 15;
    if (syscall(SYS_getppid) != -1 || errno != 5) return 16;
    /* An older, stricter one still wins over a newer, laxer one. */
    if (install(SYS_getppid, SECCOMP_RET_ALLOW, 0) != 0) return 17;
    if (syscall(SYS_getppid) != -1 || errno != 5) return 18;
    /* Children inherit the filters. */
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) _exit(syscall(SYS_getppid) == -1 && errno == 5 ? 0 : 1);
    int status;
    waitpid(pid, &status, 0);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return 19;
    return 0;
}

static volatile int trap_ok = 0;

static void on_sigsys(int signal, siginfo_t *info, void *context) {
    ucontext_t *uc = context;
    trap_ok = signal == SIGSYS && info->si_code == 1 /* SYS_SECCOMP */ && info->si_errno == 7 &&
              info->si_syscall == SYS_getppid && info->si_arch == AUDIT_ARCH_X86_64 &&
              info->si_call_addr == (void *)uc->uc_mcontext.gregs[REG_RIP] &&
              uc->uc_mcontext.gregs[REG_RAX] == SYS_getppid;
    /* Emulate the call, as sandboxes do. */
    uc->uc_mcontext.gregs[REG_RAX] = 1234;
}

static int trap_filter(void) {
    struct sigaction action = {0};
    action.sa_sigaction = on_sigsys;
    action.sa_flags = SA_SIGINFO;
    sigaction(SIGSYS, &action, NULL);
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) return 10;
    if (install(SYS_getppid, SECCOMP_RET_TRAP | 7, 0) != 0) return 11;
    long result = syscall(SYS_getppid);
    if (result != 1234) return 12;
    if (!trap_ok) return 13;
    return 0;
}

static int trap_blocked(void) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGSYS);
    sigprocmask(SIG_BLOCK, &set, NULL);
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    install(SYS_getppid, SECCOMP_RET_TRAP | 7, 0);
    syscall(SYS_getppid);
    return 1;
}

static int kill_filter(void) {
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    install(SYS_getppid, SECCOMP_RET_KILL_PROCESS, 0);
    syscall(SYS_getppid);
    return 1;
}

static void *thread_getppid(void *unused) {
    (void)unused;
    long result = syscall(SYS_getppid);
    return (void *)(long)(result == -1 && errno == 9);
}

static void *thread_wait(void *gate) {
    while (!*(volatile int *)gate) usleep(1000);
    return thread_getppid(NULL);
}

static int tsync_filter(void) {
    static int gate = 0;
    pthread_t waiting, later;
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    pthread_create(&waiting, NULL, thread_wait, &gate);
    /* TSYNC puts the filter on the thread that already exists too. */
    if (install(SYS_getppid, SECCOMP_RET_ERRNO | 9, SECCOMP_FILTER_FLAG_TSYNC) != 0) return 10;
    gate = 1;
    void *ok;
    pthread_join(waiting, &ok);
    if (!ok) return 11;
    /* New threads inherit it. */
    pthread_create(&later, NULL, thread_getppid, NULL);
    pthread_join(later, &ok);
    if (!ok) return 12;
    return 0;
}

static int strict_mode(void) {
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT) != 0) return 10;
    if (syscall(SYS_write, 1, "", 0) != 0) return 11;
    syscall(SYS_getpid);
    syscall(SYS_exit, 12); /* plain exit is allowed; exit_group isn't */
    return 13;
}

int main(void) {
    /* Chromium's (and so Firefox's) check that seccomp-bpf exists: a NULL
       filter must fail with EFAULT, not EINVAL or ENOSYS. */
    check("prctl(PR_SET_SECCOMP, 2, NULL) fails with EFAULT",
          prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, NULL) == -1 && errno == EFAULT);
    check("seccomp(FILTER, TSYNC, NULL) fails with EFAULT",
          syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, NULL) == -1 && errno == EFAULT);
    unsigned action = SECCOMP_RET_TRAP;
    check("SECCOMP_RET_TRAP is available", syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, &action) == 0);
    action = 0x12340000;
    check("an unknown action is not",
          syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, &action) == -1 && errno == EOPNOTSUPP);
    struct sock_filter bad[] = {BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 2), BPF_STMT(BPF_RET | BPF_K, 0)};
    struct sock_fprog bad_program = {2, bad};
    check("a misaligned load is refused",
          syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &bad_program) == -1 && errno == EINVAL);
    check("prctl(PR_GET_SECCOMP) is 0 without filters", prctl(PR_GET_SECCOMP) == 0);

    int status = in_child(errno_filter);
    check("ERRNO filters stack and are inherited", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    if (!WIFEXITED(status) || WEXITSTATUS(status)) printf("    status %#x\n", status);
    status = in_child(trap_filter);
    check("TRAP raises SIGSYS with the call's details", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    if (!WIFEXITED(status) || WEXITSTATUS(status)) printf("    status %#x\n", status);
    status = in_child(trap_blocked);
    check("TRAP with SIGSYS blocked ends the process", WIFSIGNALED(status) && WTERMSIG(status) == SIGSYS);
    status = in_child(kill_filter);
    check("KILL_PROCESS ends it with SIGSYS", WIFSIGNALED(status) && WTERMSIG(status) == SIGSYS);
    status = in_child(tsync_filter);
    check("TSYNC filters every thread", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    if (!WIFEXITED(status) || WEXITSTATUS(status)) printf("    status %#x\n", status);
    status = in_child(strict_mode);
    check("strict mode allows write and kills on getpid", WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);

    printf(failures ? "%d FAILED\n" : "all passed\n", failures);
    return failures != 0;
}
