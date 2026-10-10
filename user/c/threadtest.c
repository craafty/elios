/* Threads: creating and joining, mutexes, condition variables, semaphores,
   thread-local storage, per-thread signals, and how exit and execve treat
   the other threads. Everything here runs on futexes and clone. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-44s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

static void *square(void *argument) {
    long value = (long)argument;
    return (void *)(value * value);
}

static pthread_mutex_t counter_lock = PTHREAD_MUTEX_INITIALIZER;
static long counter = 0;

static void *count_up(void *argument) {
    (void)argument;
    for (int i = 0; i < 20000; i++) {
        pthread_mutex_lock(&counter_lock);
        counter++;
        pthread_mutex_unlock(&counter_lock);
    }
    return NULL;
}

/* A one-slot queue between a producer and a consumer. */
static pthread_mutex_t queue_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t queue_changed = PTHREAD_COND_INITIALIZER;
static int slot = 0, slot_full = 0;

static void *produce(void *argument) {
    (void)argument;
    for (int i = 1; i <= 100; i++) {
        pthread_mutex_lock(&queue_lock);
        while (slot_full) pthread_cond_wait(&queue_changed, &queue_lock);
        slot = i;
        slot_full = 1;
        pthread_cond_broadcast(&queue_changed);
        pthread_mutex_unlock(&queue_lock);
    }
    return NULL;
}

static __thread int thread_local_value = 1;

static void *use_thread_local(void *argument) {
    thread_local_value = (int)(long)argument;
    usleep(10000);
    return (void *)(long)thread_local_value;
}

static void *report_ids(void *argument) {
    pid_t *ids = argument;
    ids[0] = getpid();
    ids[1] = gettid();
    return NULL;
}

static volatile pid_t handled_by = 0;

static void on_usr1(int signal) {
    (void)signal;
    handled_by = gettid();
}

static pid_t waiter_tid = 0;
static sem_t started;

static void *wait_for_signal(void *argument) {
    (void)argument;
    waiter_tid = gettid();
    sem_post(&started);
    while (!handled_by) usleep(1000);
    return NULL;
}

static sem_t tokens;

static void *take_tokens(void *argument) {
    (void)argument;
    for (int i = 0; i < 50; i++) sem_wait(&tokens);
    return NULL;
}

static void *block_forever(void *argument) {
    (void)argument;
    for (;;) pause();
    return NULL;
}

int main(void) {
    /* Children exit with exit(), which would flush a copy of the buffer. */
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("Thread test (pid %d)\n", getpid());

    pthread_t threads[8];
    void *result = NULL;
    pthread_create(&threads[0], NULL, square, (void *)7L);
    pthread_join(threads[0], &result);
    check("a thread runs and returns a value", (long)result == 49);

    for (int i = 0; i < 8; i++) pthread_create(&threads[i], NULL, count_up, NULL);
    for (int i = 0; i < 8; i++) pthread_join(threads[i], NULL);
    check("a mutex protects a shared counter", counter == 8 * 20000);

    pthread_create(&threads[0], NULL, produce, NULL);
    int sum = 0, received = 0;
    while (received < 100) {
        pthread_mutex_lock(&queue_lock);
        while (!slot_full) pthread_cond_wait(&queue_changed, &queue_lock);
        sum += slot;
        slot_full = 0;
        received++;
        pthread_cond_broadcast(&queue_changed);
        pthread_mutex_unlock(&queue_lock);
    }
    pthread_join(threads[0], NULL);
    check("condition variables pass values", sum == 5050);

    for (int i = 0; i < 4; i++) pthread_create(&threads[i], NULL, use_thread_local, (void *)(long)(i + 10));
    int all_own = 1;
    for (int i = 0; i < 4; i++) {
        pthread_join(threads[i], &result);
        all_own &= (long)result == i + 10;
    }
    check("each thread has its own thread-locals", all_own && thread_local_value == 1);

    pid_t ids[2];
    pthread_create(&threads[0], NULL, report_ids, ids);
    pthread_join(threads[0], NULL);
    check("threads share the pid, not the tid", ids[0] == getpid() && ids[1] != gettid() && ids[1] > 0);

    signal(SIGUSR1, on_usr1);
    sem_init(&started, 0, 0);
    pthread_create(&threads[0], NULL, wait_for_signal, NULL);
    sem_wait(&started);
    pthread_kill(threads[0], SIGUSR1);
    pthread_join(threads[0], NULL);
    check("pthread_kill signals that thread", handled_by == waiter_tid);

    sem_init(&tokens, 0, 0);
    pthread_create(&threads[0], NULL, take_tokens, NULL);
    for (int i = 0; i < 50; i++) sem_post(&tokens);
    pthread_join(threads[0], NULL);
    int left = -1;
    sem_getvalue(&tokens, &left);
    check("semaphores count", left == 0 && sem_trywait(&tokens) == -1 && errno == EAGAIN);

    pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t never = PTHREAD_COND_INITIALIZER;
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_nsec += 50 * 1000 * 1000;
    if (deadline.tv_nsec >= 1000000000) {
        deadline.tv_sec++;
        deadline.tv_nsec -= 1000000000;
    }
    pthread_mutex_lock(&lock);
    int timed_out = pthread_cond_timedwait(&never, &lock, &deadline);
    pthread_mutex_unlock(&lock);
    check("timed waits time out", timed_out == ETIMEDOUT);

    pid_t child = fork();
    if (child == 0) {
        pthread_t blocker;
        pthread_create(&blocker, NULL, block_forever, NULL);
        usleep(10000);
        exit(7);
    }
    int status = 0;
    waitpid(child, &status, 0);
    check("exit ends every thread", WIFEXITED(status) && WEXITSTATUS(status) == 7);

    child = fork();
    if (child == 0) {
        pthread_t blocker;
        pthread_create(&blocker, NULL, block_forever, NULL);
        usleep(10000);
        execl("/bin/true", "true", (char *)NULL);
        _exit(99);
    }
    waitpid(child, &status, 0);
    check("execve replaces every thread", WIFEXITED(status) && WEXITSTATUS(status) == 0);

    child = fork();
    if (child == 0) {
        pthread_t blocker;
        pthread_create(&blocker, NULL, block_forever, NULL);
        usleep(10000);
        pthread_kill(blocker, SIGTERM);
        for (;;) pause();
    }
    waitpid(child, &status, 0);
    check("a fatal signal ends every thread", WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM);

    printf(failures ? "%d FAILED\n" : "all passed\n", failures);
    return failures != 0;
}
