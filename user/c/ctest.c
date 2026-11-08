/* Exercises the C library on eliOS: stdio, malloc, strings, files,
   directories, time, processes and the environment. Every check prints
   "ok" or "FAILED"; the exit code is the number of failures. */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-40s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

static int compare(const void *a, const void *b) {
    return *(const int *)a - *(const int *)b;
}

int main(int argc, char **argv) {
    printf("C library test, running as %s (pid %d)\n", argv[0], getpid());

    /* Memory: lots of small blocks, then one big one. */
    char **blocks = malloc(1000 * sizeof *blocks);
    int memory_ok = blocks != NULL;
    for (int i = 0; i < 1000 && memory_ok; i++) {
        blocks[i] = malloc(i + 1);
        memory_ok = blocks[i] != NULL;
        if (memory_ok) memset(blocks[i], i & 0xff, i + 1);
    }
    for (int i = 0; i < 1000 && memory_ok; i++) {
        memory_ok = (unsigned char)blocks[i][i] == (i & 0xff);
        free(blocks[i]);
    }
    free(blocks);
    char *big = malloc(16 << 20);
    memory_ok = memory_ok && big != NULL;
    if (big) { memset(big, 1, 16 << 20); free(big); }
    check("malloc and free", memory_ok);

    int numbers[] = {5, 3, 9, 1, 7};
    qsort(numbers, 5, sizeof *numbers, compare);
    check("qsort", numbers[0] == 1 && numbers[4] == 9);

    char buffer[64];
    snprintf(buffer, sizeof buffer, "%d-%s-%.2f", 42, "x", 3.14159);
    check("snprintf with floats", strcmp(buffer, "42-x-3.14") == 0);

    /* Files. */
    FILE *file = fopen("/tmp/ctest.txt", "w");
    int file_ok = file != NULL && fprintf(file, "line one\nline two\n") > 0 && fclose(file) == 0;
    file = fopen("/tmp/ctest.txt", "r");
    char line[64] = {0};
    file_ok = file_ok && file && fgets(line, sizeof line, file) && strcmp(line, "line one\n") == 0;
    if (file) fclose(file);
    struct stat info;
    file_ok = file_ok && stat("/tmp/ctest.txt", &info) == 0 && info.st_size == 18;
    check("fopen, fprintf, fgets, stat", file_ok);
    check("unlink", unlink("/tmp/ctest.txt") == 0 && access("/tmp/ctest.txt", F_OK) != 0);
    check("errno for a missing file", open("/nonexistent", O_RDONLY) < 0 && errno == ENOENT);
    int lock_fd = open("/tmp", O_RDONLY);
    check("flock", flock(lock_fd, LOCK_EX | LOCK_NB) == 0 && flock(lock_fd, LOCK_UN) == 0 &&
                       flock(lock_fd, 0) < 0 && errno == EINVAL && flock(-1, LOCK_SH) < 0 && errno == EBADF);
    close(lock_fd);

    /* Directories. */
    DIR *dir = opendir("/bin");
    int entries = 0;
    struct dirent *entry;
    while (dir && (entry = readdir(dir)) != NULL) entries++;
    if (dir) closedir(dir);
    check("opendir and readdir", entries > 10);

    /* Time. */
    time_t now = time(NULL);
    struct tm *utc = gmtime(&now);
    check("time and gmtime", now > 1700000000 && utc && utc->tm_year >= 124);
    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    usleep(20000);
    clock_gettime(CLOCK_MONOTONIC, &end);
    long elapsed_ms = (end.tv_sec - start.tv_sec) * 1000 + (end.tv_nsec - start.tv_nsec) / 1000000;
    check("usleep and the monotonic clock", elapsed_ms >= 20 && elapsed_ms < 500);

    /* Processes. */
    pid_t child = fork();
    if (child == 0) _exit(7);
    int status = 0;
    check("fork, _exit and waitpid", child > 0 && waitpid(child, &status, 0) == child &&
          WIFEXITED(status) && WEXITSTATUS(status) == 7);

    /* waitid: WNOWAIT leaves the child to collect again; a killed child
       reports CLD_KILLED and the signal; WNOHANG with nothing clears si_pid. */
    child = fork();
    if (child == 0) _exit(3);
    siginfo_t child_info = {0};
    int waitid_ok = waitid(P_PID, child, &child_info, WEXITED | WNOWAIT) == 0 && child_info.si_pid == child &&
                    child_info.si_code == CLD_EXITED && child_info.si_status == 3;
    waitid_ok = waitid_ok && waitid(P_ALL, 0, &child_info, WEXITED) == 0 && child_info.si_pid == child;
    child = fork();
    if (child == 0) {
        pause();
        _exit(0);
    }
    child_info.si_pid = 1;
    waitid_ok = waitid_ok && waitid(P_PID, child, &child_info, WEXITED | WNOHANG) == 0 && child_info.si_pid == 0;
    kill(child, SIGKILL);
    waitid_ok = waitid_ok && waitid(P_PID, child, &child_info, WEXITED) == 0 && child_info.si_code == CLD_KILLED &&
                child_info.si_status == SIGKILL && child_info.si_signo == SIGCHLD;
    check("waitid", waitid_ok);

    child = fork();
    if (child == 0) {
        char *args[] = {"echo", "  (printed by echo, started with execv)", NULL};
        execv("/bin/echo", args);
        _exit(127);
    }
    check("fork and execv", waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);

    int pipe_fds[2];
    int pipe_ok = pipe(pipe_fds) == 0 && write(pipe_fds[1], "ping", 4) == 4;
    char pong[5] = {0};
    pipe_ok = pipe_ok && read(pipe_fds[0], pong, 4) == 4 && strcmp(pong, "ping") == 0;
    check("pipe", pipe_ok);

    /* The environment and the system. */
    setenv("ELIOS_TEST", "yes", 1);
    check("getenv and setenv", getenv("PATH") && strcmp(getenv("ELIOS_TEST"), "yes") == 0);
    struct utsname name;
    check("uname", uname(&name) == 0 && strcmp(name.sysname, "eliOS") == 0);

    printf("%s: %d failure%s\n", failures ? "FAILED" : "PASSED", failures, failures == 1 ? "" : "s");
    return failures;
}
