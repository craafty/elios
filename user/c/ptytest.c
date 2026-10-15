/* Pseudo-terminals, as terminal emulators use them: open the master,
   start a program on the slave side, type at it and read what it shows. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-56s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

/* Reads from the master, adding to the transcript in `seen`, until
   `expected` is in it (or a second passes). */
static int read_until(int master, const char *expected, char *seen, size_t size) {
    size_t length = strlen(seen);
    for (int tries = 0; tries < 100 && !strstr(seen, expected); tries++) {
        struct pollfd watch = {master, POLLIN, 0};
        if (poll(&watch, 1, 10) == 1) {
            ssize_t count = read(master, seen + length, size - length - 1);
            if (count <= 0) break;
            length += count;
            seen[length] = 0;
        }
    }
    return strstr(seen, expected) != NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("Pseudo-terminal test (pid %d)\n", getpid());
    int master = posix_openpt(O_RDWR | O_NOCTTY);
    check("posix_openpt opens /dev/ptmx", master >= 0);
    check("grantpt and unlockpt", grantpt(master) == 0 && unlockpt(master) == 0);
    char *name = ptsname(master);
    check("ptsname names /dev/pts/N", name && strncmp(name, "/dev/pts/", 9) == 0);

    struct winsize size = {.ws_row = 40, .ws_col = 100};
    check("the master sets the window size", ioctl(master, TIOCSWINSZ, &size) == 0);

    pid_t child = fork();
    if (child == 0) {
        setsid();
        int slave = open(name, O_RDWR);
        if (slave < 0) _exit(1);
        dup2(slave, 0);
        dup2(slave, 1);
        dup2(slave, 2);
        struct winsize seen;
        ioctl(0, TIOCGWINSZ, &seen);
        printf("size %dx%d\n", seen.ws_col, seen.ws_row);
        int tty = open("/dev/tty", O_WRONLY);
        if (tty >= 0) (void)!write(tty, "via /dev/tty\n", 13);
        char line[64];
        if (fgets(line, sizeof line, stdin)) printf("got [%.*s]\n", (int)strcspn(line, "\n"), line);
        fflush(stdout);
        _exit(0);
    }
    char seen[1024] = "";
    check("the program sees the window size", read_until(master, "size 100x40", seen, sizeof seen));
    check("newlines come out as CR LF", strstr(seen, "size 100x40\r\n") != NULL);
    check("/dev/tty is the program's own terminal", read_until(master, "via /dev/tty", seen, sizeof seen));
    (void)!write(master, "typed\r", 6);
    check("typing echoes, and Enter ends the line", read_until(master, "typed\r\n", seen, sizeof seen));
    check("the program reads the line", read_until(master, "got [typed]", seen, sizeof seen));
    int status;
    waitpid(child, &status, 0);
    check("the program exits", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    char byte;
    check("then reading the master fails with EIO", read(master, &byte, 1) == -1 && errno == EIO);
    close(master);

    /* Ctrl+C reaches the foreground program. */
    master = posix_openpt(O_RDWR | O_NOCTTY);
    unlockpt(master);
    name = ptsname(master);
    child = fork();
    if (child == 0) {
        setsid();
        int slave = open(name, O_RDWR);
        dup2(slave, 0);
        dup2(slave, 1);
        tcsetpgrp(0, getpid());
        printf("ready\n");
        fflush(stdout);
        pause();
        _exit(0);
    }
    seen[0] = 0;
    read_until(master, "ready", seen, sizeof seen);
    (void)!write(master, "\x03", 1);
    waitpid(child, &status, 0);
    check("Ctrl+C interrupts the program", WIFSIGNALED(status) && WTERMSIG(status) == SIGINT);
    close(master);

    printf("%s\n", failures ? "SOME TESTS FAILED" : "all pseudo-terminal tests passed");
    return failures ? 1 : 0;
}
