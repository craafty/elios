/* Timers through glibc: alarm, setitimer, POSIX timers (signal and thread
   notification), and waiting for signals with sigwaitinfo and
   sigtimedwait. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int failures = 0;
static volatile sig_atomic_t alarms = 0;
static volatile int timer_value = -1;
static volatile int thread_calls = 0;

static void check(const char *what, int ok) {
    printf("  %-52s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

static void on_alarm(int signal) { (void)signal; alarms++; }

static void on_timer(int signal, siginfo_t *info, void *context) {
    (void)signal;
    (void)context;
    if (info->si_code == SI_TIMER) timer_value = info->si_value.sival_int;
}

static void on_thread(union sigval value) { thread_calls += value.sival_int; }

static long elapsed_ms(struct timespec *start) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (now.tv_sec - start->tv_sec) * 1000 + (now.tv_nsec - start->tv_nsec) / 1000000;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("Timer test (pid %d)\n", getpid());
    struct timespec start;

    signal(SIGALRM, on_alarm);
    clock_gettime(CLOCK_MONOTONIC, &start);
    alarm(1);
    while (alarms == 0) pause();
    long waited = elapsed_ms(&start);
    check("alarm(1) rings after about a second", waited >= 900 && waited < 2000);
    check("alarm(0) reports what was left", (alarm(5), alarm(0)) >= 4);

    struct itimerval every = {{0, 50000}, {0, 50000}};
    alarms = 0;
    setitimer(ITIMER_REAL, &every, NULL);
    while (alarms < 5) pause();
    struct itimerval current;
    getitimer(ITIMER_REAL, &current);
    check("setitimer repeats every 50 ms", current.it_interval.tv_usec == 50000);
    struct itimerval off = {{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &off, NULL);

    struct sigaction action = {0};
    action.sa_sigaction = on_timer;
    action.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &action, NULL);
    struct sigevent event = {0};
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_int = 42;
    timer_t timer;
    check("timer_create with a signal", timer_create(CLOCK_MONOTONIC, &event, &timer) == 0);
    struct itimerspec once = {{0, 0}, {0, 100000000}};
    timer_settime(timer, 0, &once, NULL);
    struct itimerspec left;
    timer_gettime(timer, &left);
    check("timer_gettime shows it counting down", left.it_value.tv_nsec > 0 && left.it_value.tv_nsec <= 100000000);
    while (timer_value < 0) pause();
    check("the signal carries the timer's value", timer_value == 42);
    timer_delete(timer);

    event.sigev_notify = SIGEV_THREAD;
    event.sigev_notify_function = on_thread;
    event.sigev_value.sival_int = 1;
    check("timer_create with a thread", timer_create(CLOCK_MONOTONIC, &event, &timer) == 0);
    struct itimerspec repeating = {{0, 20000000}, {0, 20000000}};
    timer_settime(timer, 0, &repeating, NULL);
    clock_gettime(CLOCK_MONOTONIC, &start);
    while (thread_calls < 3 && elapsed_ms(&start) < 3000) usleep(10000);
    check("its function runs in a thread, repeatedly", thread_calls >= 3);
    timer_delete(timer);

    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    sigprocmask(SIG_BLOCK, &set, NULL);
    kill(getpid(), SIGUSR2);
    siginfo_t info;
    check("sigwaitinfo takes a blocked, pending signal", sigwaitinfo(&set, &info) == SIGUSR2 && info.si_signo == SIGUSR2);
    struct timespec brief = {0, 100000000};
    clock_gettime(CLOCK_MONOTONIC, &start);
    int result = sigtimedwait(&set, &info, &brief);
    check("sigtimedwait gives up after its timeout", result == -1 && errno == EAGAIN && elapsed_ms(&start) >= 90);

    printf(failures ? "%d FAILED\n" : "all passed\n", failures);
    return failures != 0;
}
