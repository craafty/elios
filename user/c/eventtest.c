/* Event loops' building blocks: eventfd, timerfd, and epoll over them,
   pipes and other epoll sets, as GLib, libwayland and Firefox use them. */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-56s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

static long now_ms(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return now.tv_sec * 1000 + now.tv_nsec / 1000000;
}

static int wake_fd;

static void *wake_later(void *argument) {
    (void)argument;
    usleep(50000);
    uint64_t one = 1;
    (void)!write(wake_fd, &one, 8);
    return NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("Event test (pid %d)\n", getpid());
    uint64_t value = 0;

    /* eventfd */
    int event = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    check("eventfd makes a descriptor", event >= 0);
    check("reading an empty one fails with EAGAIN", read(event, &value, 8) == -1 && errno == EAGAIN);
    value = 3;
    (void)!write(event, &value, 8);
    value = 2;
    (void)!write(event, &value, 8);
    check("writes add up and a read takes the total", read(event, &value, 8) == 8 && value == 5);
    int semaphore = eventfd(2, EFD_SEMAPHORE | EFD_NONBLOCK);
    check("a semaphore eventfd counts down by one",
          read(semaphore, &value, 8) == 8 && value == 1 && read(semaphore, &value, 8) == 8 &&
              read(semaphore, &value, 8) == -1);
    close(semaphore);

    /* timerfd */
    int timer = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
    struct itimerspec once = {{0, 0}, {0, 50000000}};
    long start = now_ms();
    timerfd_settime(timer, 0, &once, NULL);
    check("a read waits for the timer and counts one expiry", read(timer, &value, 8) == 8 && value == 1);
    long waited = now_ms() - start;
    check("... after about 50 ms", waited >= 45 && waited < 500);
    struct itimerspec every = {{0, 20000000}, {0, 20000000}};
    timerfd_settime(timer, 0, &every, NULL);
    usleep(110000);
    check("a repeating timer counts the periods that passed", read(timer, &value, 8) == 8 && value >= 4 && value <= 6);
    struct itimerspec current;
    timerfd_gettime(timer, &current);
    check("timerfd_gettime reports the interval", current.it_interval.tv_nsec == 20000000);
    struct itimerspec off = {{0, 0}, {0, 0}};
    timerfd_settime(timer, 0, &off, NULL);
    struct pollfd watch = {timer, POLLIN, 0};
    check("a disarmed timer isn't readable", poll(&watch, 1, 30) == 0);
    timerfd_settime(timer, 0, &once, NULL);
    start = now_ms();
    check("poll wakes when the timer expires", poll(&watch, 1, 2000) == 1 && now_ms() - start < 500);
    (void)!read(timer, &value, 8);

    /* epoll */
    int pipes[2];
    (void)!pipe(pipes);
    int epoll = epoll_create1(EPOLL_CLOEXEC);
    check("epoll_create1 makes a descriptor", epoll >= 0);
    struct epoll_event add = {.events = EPOLLIN, .data.u64 = 0x1111};
    check("a pipe can be added", epoll_ctl(epoll, EPOLL_CTL_ADD, pipes[0], &add) == 0);
    check("adding it twice fails", epoll_ctl(epoll, EPOLL_CTL_ADD, pipes[0], &add) == -1 && errno == EEXIST);
    add.data.u64 = 0x2222;
    epoll_ctl(epoll, EPOLL_CTL_ADD, event, &add);
    struct epoll_event got[4];
    check("nothing is ready yet", epoll_wait(epoll, got, 4, 0) == 0);
    (void)!write(pipes[1], "x", 1);
    int count = epoll_wait(epoll, got, 4, 1000);
    check("a written pipe is reported with its data", count == 1 && got[0].data.u64 == 0x1111 && got[0].events == EPOLLIN);
    check("level-triggered: reported again until read", epoll_wait(epoll, got, 4, 0) == 1);
    char byte;
    (void)!read(pipes[0], &byte, 1);
    check("... and not after", epoll_wait(epoll, got, 4, 0) == 0);

    /* Waking a waiter from another thread. */
    wake_fd = event;
    pthread_t waker;
    pthread_create(&waker, NULL, wake_later, NULL);
    start = now_ms();
    count = epoll_wait(epoll, got, 4, 2000);
    pthread_join(waker, NULL);
    check("epoll_wait wakes when another thread writes an eventfd", count == 1 && got[0].data.u64 == 0x2222);
    (void)!read(event, &value, 8);

    /* Edge-triggered and one-shot. */
    struct epoll_event edge = {.events = EPOLLIN | EPOLLET, .data.u64 = 0x3333};
    epoll_ctl(epoll, EPOLL_CTL_MOD, pipes[0], &edge);
    (void)!write(pipes[1], "y", 1);
    check("edge-triggered: reported once", epoll_wait(epoll, got, 4, 1000) == 1 && got[0].data.u64 == 0x3333);
    check("... not again while unread", epoll_wait(epoll, got, 4, 50) == 0);
    (void)!write(pipes[1], "z", 1);
    check("... and again after more data arrives", epoll_wait(epoll, got, 4, 1000) == 1);
    char drain[8];
    (void)!read(pipes[0], drain, sizeof drain);
    struct epoll_event oneshot = {.events = EPOLLIN | EPOLLONESHOT, .data.u64 = 0x4444};
    epoll_ctl(epoll, EPOLL_CTL_MOD, pipes[0], &oneshot);
    (void)!write(pipes[1], "w", 1);
    check("one-shot: reported once", epoll_wait(epoll, got, 4, 1000) == 1);
    check("... then quiet until re-armed", epoll_wait(epoll, got, 4, 0) == 0);
    epoll_ctl(epoll, EPOLL_CTL_MOD, pipes[0], &oneshot);
    check("... and reported after EPOLL_CTL_MOD", epoll_wait(epoll, got, 4, 0) == 1);
    (void)!read(pipes[0], drain, sizeof drain);

    /* A timerfd in an epoll set, and epoll sets inside epoll sets. */
    struct epoll_event timed = {.events = EPOLLIN, .data.u64 = 0x5555};
    epoll_ctl(epoll, EPOLL_CTL_ADD, timer, &timed);
    timerfd_settime(timer, 0, &once, NULL);
    start = now_ms();
    count = epoll_wait(epoll, got, 4, 2000);
    check("epoll_wait wakes for a timerfd", count == 1 && got[0].data.u64 == 0x5555 && now_ms() - start < 500);
    int outer = epoll_create1(0);
    struct epoll_event nested = {.events = EPOLLIN, .data.u64 = 0x6666};
    epoll_ctl(outer, EPOLL_CTL_ADD, epoll, &nested);
    check("an epoll set with something ready is readable in another",
          epoll_wait(outer, got, 4, 0) == 1 && got[0].data.u64 == 0x6666);
    check("an epoll set can't watch itself", epoll_ctl(epoll, EPOLL_CTL_ADD, epoll, &nested) == -1 && errno == EINVAL);

    /* Closing a file takes it out of the set. */
    (void)!read(timer, &value, 8);
    close(timer);
    (void)!write(pipes[1], "v", 1);
    close(pipes[0]);
    check("closed files drop out of the set", epoll_wait(epoll, got, 4, 0) == 0);
    check("EPOLL_CTL_DEL removes an entry", epoll_ctl(epoll, EPOLL_CTL_DEL, event, NULL) == 0 &&
                                                epoll_ctl(epoll, EPOLL_CTL_DEL, event, NULL) == -1);

    printf("%s\n", failures ? "SOME TESTS FAILED" : "all event tests passed");
    return failures ? 1 : 0;
}
