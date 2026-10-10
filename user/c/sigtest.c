/* Signal handling: handlers with siginfo, blocking, FPU state across a
   handler, and recovering from a segmentation fault with siglongjmp. */
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int failures = 0;
static volatile sig_atomic_t got_signal = 0;
static volatile int info_signo = 0;
static sigjmp_buf recovery;

static void check(const char *what, int ok) {
    printf("  %-44s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

static void on_signal(int signal, siginfo_t *info, void *context) {
    (void)context;
    got_signal = signal;
    info_signo = info->si_signo;
    /* Floating point in a handler mustn't disturb the interrupted code. */
    volatile double scratch = 1.0;
    for (int i = 0; i < 10; i++) scratch = scratch * 3.7 + 0.1;
}

static void on_segfault(int signal) {
    (void)signal;
    siglongjmp(recovery, 1);
}

int main(void) {
    printf("Signal test (pid %d)\n", getpid());

    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_sigaction = on_signal;
    action.sa_flags = SA_SIGINFO;
    sigemptyset(&action.sa_mask);
    sigaction(SIGUSR1, &action, NULL);

    volatile double value = 2.5;
    raise(SIGUSR1);
    check("handler runs with siginfo", got_signal == SIGUSR1 && info_signo == SIGUSR1);
    check("FPU state survives a handler", value * 2 == 5.0);

    got_signal = 0;
    sigaction(SIGUSR2, &action, NULL);
    sigset_t block;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR2);
    sigprocmask(SIG_BLOCK, &block, NULL);
    kill(getpid(), SIGUSR2);
    check("blocked signals wait", got_signal == 0);
    sigprocmask(SIG_UNBLOCK, &block, NULL);
    check("unblocking delivers them", got_signal == SIGUSR2);

    signal(SIGSEGV, on_segfault);
    if (sigsetjmp(recovery, 1) == 0) {
        volatile int *null = NULL;
        *null = 42;
        check("recover from a segfault", 0);
    } else {
        check("recover from a segfault", 1);
    }

    signal(SIGINT, SIG_IGN);
    raise(SIGINT);
    check("ignored signals do nothing", 1);

    printf("%s: %d failure%s\n", failures ? "FAILED" : "PASSED", failures, failures == 1 ? "" : "s");
    return failures;
}
