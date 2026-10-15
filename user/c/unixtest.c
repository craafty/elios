/* Unix domain sockets: socketpair, listening on a path and an abstract
   name, message boundaries, passing file descriptors, peer credentials,
   end of file and broken pipes, and a big transfer between processes. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-56s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

/* Sends one byte and a descriptor. */
static int send_fd(int socket, int fd, const char *data, size_t length) {
    struct iovec vector = {(void *)data, length};
    char control[CMSG_SPACE(sizeof(int))];
    memset(control, 0, sizeof control);
    struct msghdr message = {0};
    message.msg_iov = &vector;
    message.msg_iovlen = 1;
    message.msg_control = control;
    message.msg_controllen = sizeof control;
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &fd, sizeof(int));
    return sendmsg(socket, &message, 0);
}

/* Receives data and a descriptor (-1 if none came). */
static int receive_fd(int socket, char *data, size_t length, ssize_t *received) {
    struct iovec vector = {data, length};
    char control[CMSG_SPACE(sizeof(int) * 4)];
    struct msghdr message = {0};
    message.msg_iov = &vector;
    message.msg_iovlen = 1;
    message.msg_control = control;
    message.msg_controllen = sizeof control;
    *received = recvmsg(socket, &message, MSG_CMSG_CLOEXEC);
    for (struct cmsghdr *header = CMSG_FIRSTHDR(&message); header; header = CMSG_NXTHDR(&message, header)) {
        if (header->cmsg_level == SOL_SOCKET && header->cmsg_type == SCM_RIGHTS) {
            int fd;
            memcpy(&fd, CMSG_DATA(header), sizeof fd);
            return fd;
        }
    }
    return -1;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("Unix socket test (pid %d)\n", getpid());
    signal(SIGPIPE, SIG_IGN);
    char buffer[256];

    /* socketpair */
    int pair[2];
    check("socketpair makes a connected stream pair", socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) == 0);
    (void)!write(pair[0], "hello", 5);
    check("data goes one way", read(pair[1], buffer, sizeof buffer) == 5 && memcmp(buffer, "hello", 5) == 0);
    (void)!write(pair[1], "back", 4);
    check("... and the other", read(pair[0], buffer, sizeof buffer) == 4);
    (void)!write(pair[0], "ab", 2);
    (void)!write(pair[0], "cd", 2);
    check("a stream joins writes", read(pair[1], buffer, sizeof buffer) == 4 && memcmp(buffer, "abcd", 4) == 0);
    close(pair[0]);
    check("closing one end gives the other end of file", read(pair[1], buffer, sizeof buffer) == 0);
    check("writing to a closed peer fails with EPIPE", write(pair[1], "x", 1) == -1 && errno == EPIPE);
    close(pair[1]);

    /* Message boundaries. */
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, pair);
    (void)!write(pair[0], "first", 5);
    (void)!write(pair[0], "second", 6);
    check("sequenced packets keep their boundaries",
          read(pair[1], buffer, sizeof buffer) == 5 && read(pair[1], buffer, sizeof buffer) == 6);
    close(pair[0]);
    close(pair[1]);
    socketpair(AF_UNIX, SOCK_DGRAM, 0, pair);
    (void)!write(pair[0], "a long datagram", 15);
    check("a datagram read short is truncated", recv(pair[1], buffer, 6, 0) == 6 && memcmp(buffer, "a long", 6) == 0);
    struct pollfd watch = {pair[1], POLLIN, 0};
    check("... and the rest is gone", poll(&watch, 1, 0) == 0);
    close(pair[0]);
    close(pair[1]);

    /* A server on a path. */
    const char *path = "/tmp/unixtest.sock";
    unlink(path);
    int server = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    strcpy(address.sun_path, path);
    check("bind to a path", bind(server, (struct sockaddr *)&address, sizeof address) == 0);
    struct stat info;
    check("... makes a socket file", stat(path, &info) == 0 && S_ISSOCK(info.st_mode));
    check("binding it again fails", bind(socket(AF_UNIX, SOCK_STREAM, 0), (struct sockaddr *)&address, sizeof address) == -1 &&
                                         errno == EADDRINUSE);
    check("listen", listen(server, 8) == 0);
    pid_t child = fork();
    if (child == 0) {
        int client = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(client, (struct sockaddr *)&address, sizeof address) != 0) _exit(1);
        (void)!write(client, "from the client", 15);
        char reply[16];
        if (read(client, reply, sizeof reply) != 5 || memcmp(reply, "reply", 5) != 0) _exit(2);
        _exit(0);
    }
    watch = (struct pollfd){server, POLLIN, 0};
    check("the listener becomes readable when a client connects", poll(&watch, 1, 2000) == 1);
    struct sockaddr_un peer;
    socklen_t length = sizeof peer;
    int connection = accept4(server, (struct sockaddr *)&peer, &length, SOCK_CLOEXEC);
    check("accept4 takes the connection", connection >= 0);
    check("the message arrives", read(connection, buffer, sizeof buffer) == 15);
    struct ucred credentials;
    socklen_t size = sizeof credentials;
    check("SO_PEERCRED says who connected",
          getsockopt(connection, SOL_SOCKET, SO_PEERCRED, &credentials, &size) == 0 && credentials.pid == child &&
              credentials.uid == getuid());
    length = sizeof peer;
    check("getsockname gives the path", getsockname(connection, (struct sockaddr *)&peer, &length) == 0 &&
                                            strcmp(peer.sun_path, path) == 0);
    (void)!write(connection, "reply", 5);
    int status;
    waitpid(child, &status, 0);
    check("the client saw the reply", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    close(connection);
    int nonblocking = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    check("connecting to nothing is refused",
          connect(nonblocking, (struct sockaddr *)&(struct sockaddr_un){.sun_family = AF_UNIX, .sun_path = "/tmp/none"},
                  sizeof address) == -1 &&
              errno == ENOENT);
    close(nonblocking);
    close(server);
    check("a closed server's path refuses connections", ({
              int late = socket(AF_UNIX, SOCK_STREAM, 0);
              int result = connect(late, (struct sockaddr *)&address, sizeof address);
              close(late);
              result == -1 && errno == ECONNREFUSED;
          }));
    unlink(path);

    /* An abstract name. */
    server = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    struct sockaddr_un abstract = {.sun_family = AF_UNIX};
    memcpy(abstract.sun_path, "\0unixtest", 9);
    socklen_t abstract_length = offsetof(struct sockaddr_un, sun_path) + 9;
    check("bind to an abstract name", bind(server, (struct sockaddr *)&abstract, abstract_length) == 0 &&
                                          listen(server, 1) == 0);
    int client = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    check("connect to it", connect(client, (struct sockaddr *)&abstract, abstract_length) == 0);
    connection = accept(server, NULL, NULL);
    check("... and accept", connection >= 0);
    close(client);
    close(connection);
    close(server);

    /* Passing a descriptor: a pipe's write end goes over the socket, and
       a write through the received copy comes out of the pipe. */
    socketpair(AF_UNIX, SOCK_STREAM, 0, pair);
    int pipes[2];
    (void)!pipe(pipes);
    (void)!write(pair[0], "before", 6);
    check("sendmsg with SCM_RIGHTS", send_fd(pair[0], pipes[1], "F", 1) == 1);
    close(pipes[1]);
    ssize_t received;
    int fd = receive_fd(pair[1], buffer, sizeof buffer, &received);
    check("data sent before the file comes alone", fd == -1 && received == 6);
    fd = receive_fd(pair[1], buffer, sizeof buffer, &received);
    check("the file arrives with its byte", fd >= 0 && received == 1 && buffer[0] == 'F');
    check("... close-on-exec, as MSG_CMSG_CLOEXEC asked", fcntl(fd, F_GETFD) == FD_CLOEXEC);
    (void)!write(fd, "through", 7);
    check("writing to the received file reaches the pipe", read(pipes[0], buffer, sizeof buffer) == 7);
    close(fd);
    check("with every write end closed, the pipe ends", read(pipes[0], buffer, sizeof buffer) == 0);
    close(pipes[0]);

    /* A megabyte through a socket: the sender waits while the receiver's
       queue is full. */
    child = fork();
    if (child == 0) {
        char block[4096];
        memset(block, 'z', sizeof block);
        for (int i = 0; i < 256; i++) {
            if (write(pair[0], block, sizeof block) != sizeof block) _exit(1);
        }
        _exit(0);
    }
    close(pair[0]);
    long total = 0;
    ssize_t count;
    char block[8192];
    while (total < 256 * 4096 && (count = read(pair[1], block, sizeof block)) > 0) total += count;
    waitpid(child, &status, 0);
    check("a megabyte crosses between processes", total == 256 * 4096 && WEXITSTATUS(status) == 0);
    close(pair[1]);

    printf("%s\n", failures ? "SOME TESTS FAILED" : "all Unix socket tests passed");
    return failures ? 1 : 0;
}
