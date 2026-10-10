/* Users and permissions through glibc: ids, file permissions, umask,
   ownership, the sticky bit, signals between users, and setuid programs.
   Must start as root; it drops to an ordinary user in a child. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define USER 1000
#define GROUP 1000

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-52s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

/* Run as a setuid-root copy of this program by an ordinary user. */
static int report_setuid(void) {
    int ok = getuid() == USER && geteuid() == 0 && getauxval(AT_SECURE) == 1;
    /* Drop to the user for a while, then take root back. */
    ok = ok && seteuid(USER) == 0 && geteuid() == USER;
    ok = ok && open("/tmp/permtest/secret", O_RDONLY) == -1 && errno == EACCES;
    ok = ok && seteuid(0) == 0 && geteuid() == 0;
    ok = ok && open("/tmp/permtest/secret", O_RDONLY) >= 0;
    return ok ? 0 : 1;
}

static void as_user(const char *self) {
    check("dropping to an ordinary user",
          setgroups(0, NULL) == 0 && setgid(GROUP) == 0 && setuid(USER) == 0 && getuid() == USER &&
              geteuid() == USER && getgid() == GROUP);
    check("and there's no way back to root", setuid(0) == -1 && errno == EPERM);
    check("a private directory can't be searched",
          open("/tmp/permtest/secret", O_RDONLY) == -1 && errno == EACCES);
    check("access() agrees", access("/tmp/permtest", R_OK) == -1 && access("/tmp/permtest", F_OK) == 0);
    check("system files can't be written", open("/etc/passwd", O_WRONLY) == -1 && errno == EACCES);
    check("but can be read", open("/etc/passwd", O_RDONLY) >= 0);
    check("root's password hashes can't be read", open("/etc/shadow", O_RDONLY) == -1 && errno == EACCES);

    umask(022);
    int fd = open("/tmp/permtest-mine", O_CREAT | O_WRONLY | O_TRUNC, 0666);
    struct stat info;
    check("new files belong to their creator, less the umask",
          fd >= 0 && fstat(fd, &info) == 0 && info.st_uid == USER && info.st_gid == GROUP &&
              (info.st_mode & 0777) == 0644);
    close(fd);
    check("owners can chmod their files",
          chmod("/tmp/permtest-mine", 0600) == 0 && stat("/tmp/permtest-mine", &info) == 0 &&
              (info.st_mode & 0777) == 0600);
    check("but not give them away", chown("/tmp/permtest-mine", 0, 0) == -1 && errno == EPERM);
    check("or chmod someone else's", chmod("/tmp/permtest-root", 0777) == -1 && errno == EPERM);
    check("the sticky bit protects others' files in /tmp",
          unlink("/tmp/permtest-root") == -1 && errno == EPERM);
    check("while their own can go", unlink("/tmp/permtest-mine") == 0);
    check("other users' processes can't be signalled", kill(getppid(), 0) == -1 && errno == EPERM);
    check("files without execute permission can't run",
          execl("/tmp/permtest-root", "x", NULL) == -1 && errno == EACCES);

    pid_t child = fork();
    if (child == 0) {
        execl("/tmp/permtest-suid", "permtest", "--setuid", NULL);
        _exit(127);
    }
    int status = 0;
    waitpid(child, &status, 0);
    check("a setuid program runs as its owner, and can switch",
          WIFEXITED(status) && WEXITSTATUS(status) == 0);
    (void)self;
}

/* Copies a file (to make the setuid copy of this program). */
static int copy(const char *from, const char *to) {
    int in = open(from, O_RDONLY), out = open(to, O_CREAT | O_WRONLY | O_TRUNC, 0755);
    if (in < 0 || out < 0) return -1;
    char buffer[65536];
    ssize_t count;
    while ((count = read(in, buffer, sizeof buffer)) > 0)
        if (write(out, buffer, count) != count) return -1;
    close(in);
    close(out);
    return 0;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "--setuid") == 0) return report_setuid();
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("Permission test (pid %d)\n", getpid());
    if (getuid() != 0) {
        printf("  must start as root\n");
        return 1;
    }
    check("starting as root", geteuid() == 0 && getgid() == 0);

    mkdir("/tmp/permtest", 0700);
    int fd = open("/tmp/permtest/secret", O_CREAT | O_WRONLY | O_TRUNC, 0600);
    check("root can make private files", fd >= 0 && write(fd, "secret", 6) == 6);
    close(fd);
    fd = open("/tmp/permtest-root", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    close(fd);
    check("a copy of this program, setuid root",
          copy("/proc/self/exe", "/tmp/permtest-suid") == 0 && chmod("/tmp/permtest-suid", 04755) == 0);
    struct stat info;
    check("giving a setuid file away clears the bit",
          copy("/proc/self/exe", "/tmp/permtest-give") == 0 && chmod("/tmp/permtest-give", 04755) == 0 &&
              chown("/tmp/permtest-give", USER, GROUP) == 0 && stat("/tmp/permtest-give", &info) == 0 &&
              (info.st_mode & 07777) == 0755 && info.st_uid == USER);

    pid_t child = fork();
    if (child == 0) {
        as_user(argv[0]);
        _exit(failures);
    }
    int status = 0;
    waitpid(child, &status, 0);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) failures++;

    unlink("/tmp/permtest/secret");
    rmdir("/tmp/permtest");
    unlink("/tmp/permtest-root");
    unlink("/tmp/permtest-suid");
    unlink("/tmp/permtest-give");
    printf(failures ? "%d FAILED\n" : "all passed\n", failures);
    return failures != 0;
}
