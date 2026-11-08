/* Sockets through glibc: TCP and UDP over loopback, nonblocking connect,
   poll and select, shutdown, MSG_PEEK, timeouts, and refused connections. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-48s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

static struct sockaddr_in loopback(int port) {
    struct sockaddr_in address;
    memset(&address, 0, sizeof address);
    address.sin_family = AF_INET;
    address.sin_port = htons(port);
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return address;
}

/* A listening socket on a free loopback port; returns the port. */
static int listen_any(int *port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in address = loopback(0);
    if (bind(fd, (struct sockaddr *)&address, sizeof address) < 0 || listen(fd, 4) < 0) return -1;
    socklen_t length = sizeof address;
    getsockname(fd, (struct sockaddr *)&address, &length);
    *port = ntohs(address.sin_port);
    return fd;
}

#define BULK (200 * 1024)

/* Echoes everything back until the client shuts down its side. */
static void *echo_server(void *argument) {
    int listener = *(int *)argument;
    int fd = accept(listener, NULL, NULL);
    char buffer[8192];
    ssize_t count;
    while ((count = read(fd, buffer, sizeof buffer)) > 0) {
        for (ssize_t done = 0; done < count;) {
            ssize_t written = write(fd, buffer + done, count - done);
            if (written <= 0) return NULL;
            done += written;
        }
    }
    close(fd);
    return NULL;
}

static void *bulk_writer(void *argument) {
    int fd = *(int *)argument;
    unsigned char *data = malloc(BULK);
    for (int i = 0; i < BULK; i++) data[i] = (unsigned char)(i * 31 + 7);
    for (int done = 0; done < BULK;) {
        ssize_t written = write(fd, data + done, BULK - done);
        if (written <= 0) break;
        done += written;
    }
    shutdown(fd, SHUT_WR);
    free(data);
    return NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("Network test (pid %d)\n", getpid());

    int port = 0;
    int listener = listen_any(&port);
    check("bind to port 0 picks a port, and listen", listener >= 0 && port > 0);

    pthread_t server;
    pthread_create(&server, NULL, echo_server, &listener);
    int client = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in address = loopback(port);
    check("connect over loopback", connect(client, (struct sockaddr *)&address, sizeof address) == 0);
    struct sockaddr_in peer;
    socklen_t peer_length = sizeof peer;
    getpeername(client, (struct sockaddr *)&peer, &peer_length);
    check("getpeername says who we're connected to", ntohs(peer.sin_port) == port);

    /* 200 KiB through the echo server, written by one thread and read by
       this one, so both directions' windows fill and drain. */
    pthread_t writer;
    pthread_create(&writer, NULL, bulk_writer, &client);
    unsigned char *received = malloc(BULK);
    int total = 0;
    ssize_t count;
    while (total < BULK && (count = read(client, received + total, BULK - total)) > 0) total += count;
    int intact = total == BULK;
    for (int i = 0; intact && i < BULK; i++) intact = received[i] == (unsigned char)(i * 31 + 7);
    check("200 KiB echoed back intact", intact);
    check("the server's close reads as end of file", read(client, received, 1) == 0);
    pthread_join(writer, NULL);
    pthread_join(server, NULL);
    close(client);
    free(received);

    /* MSG_PEEK and FIONREAD-style readiness. */
    pthread_create(&server, NULL, echo_server, &listener);
    client = socket(AF_INET, SOCK_STREAM, 0);
    connect(client, (struct sockaddr *)&address, sizeof address);
    check("a small write goes through", write(client, "peekaboo", 8) == 8);
    struct pollfd entry = {client, POLLIN, 0};
    check("poll sees the echo arrive", poll(&entry, 1, 2000) == 1 && (entry.revents & POLLIN));
    char text[16] = {0};
    recv(client, text, 4, MSG_PEEK);
    char again[16] = {0};
    recv(client, again, 8, MSG_WAITALL);
    check("MSG_PEEK leaves data to read again", memcmp(text, "peek", 4) == 0 && memcmp(again, "peekaboo", 8) == 0);
    shutdown(client, SHUT_WR);
    check("shutdown lets the other side finish", read(client, text, 1) == 0);
    pthread_join(server, NULL);
    close(client);

    /* Nonblocking connect, then poll for writability and check SO_ERROR. */
    pthread_create(&server, NULL, echo_server, &listener);
    client = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    int result = connect(client, (struct sockaddr *)&address, sizeof address);
    check("nonblocking connect is in progress", result == -1 && errno == EINPROGRESS);
    entry = (struct pollfd){client, POLLOUT, 0};
    poll(&entry, 1, 2000);
    int error = -1;
    socklen_t error_length = sizeof error;
    getsockopt(client, SOL_SOCKET, SO_ERROR, &error, &error_length);
    check("then becomes writable, with no error", (entry.revents & POLLOUT) && error == 0);
    check("nonblocking reads of nothing fail with EAGAIN", read(client, text, 1) == -1 && errno == EAGAIN);
    close(client);
    pthread_join(server, NULL);
    close(listener);

    client = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in closed = loopback(1);
    result = connect(client, (struct sockaddr *)&closed, sizeof closed);
    check("connecting to a closed port is refused", result == -1 && errno == ECONNREFUSED);
    close(client);

    /* UDP. */
    int a = socket(AF_INET, SOCK_DGRAM, 0);
    int b = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a_address = loopback(0);
    bind(a, (struct sockaddr *)&a_address, sizeof a_address);
    socklen_t length = sizeof a_address;
    getsockname(a, (struct sockaddr *)&a_address, &length);
    sendto(b, "datagram", 8, 0, (struct sockaddr *)&a_address, sizeof a_address);
    struct sockaddr_in from;
    length = sizeof from;
    memset(text, 0, sizeof text);
    count = recvfrom(a, text, sizeof text, 0, (struct sockaddr *)&from, &length);
    check("UDP datagrams arrive whole, with the sender", count == 8 && memcmp(text, "datagram", 8) == 0 && from.sin_port != 0);
    sendto(a, "reply", 5, 0, (struct sockaddr *)&from, length);
    fd_set readable;
    FD_ZERO(&readable);
    FD_SET(b, &readable);
    struct timeval wait = {2, 0};
    check("select sees the reply", select(b + 1, &readable, NULL, NULL, &wait) == 1 && FD_ISSET(b, &readable));
    memset(text, 0, sizeof text);
    check("and it reads", recv(b, text, sizeof text, 0) == 5 && strcmp(text, "reply") == 0);

    struct timeval timeout = {0, 200000};
    setsockopt(a, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout);
    check("receive timeouts give EAGAIN", recv(a, text, sizeof text, 0) == -1 && (errno == EAGAIN || errno == EWOULDBLOCK));
    close(a);
    close(b);

    /* Interfaces, as WebRTC finds them: getifaddrs (route netlink), names
       through if_indextoname (an ioctl on an AF_UNIX socket), then UDP
       between two sockets bound to the machine's own address. */
    struct ifaddrs *interfaces = NULL;
    struct sockaddr_in own = {0};
    char own_name[IF_NAMESIZE] = "";
    if (getifaddrs(&interfaces) == 0) {
        for (struct ifaddrs *i = interfaces; i; i = i->ifa_next) {
            if (i->ifa_addr && i->ifa_addr->sa_family == AF_INET && !(i->ifa_flags & IFF_LOOPBACK)) {
                own = *(struct sockaddr_in *)i->ifa_addr;
                snprintf(own_name, sizeof own_name, "%s", i->ifa_name);
            }
        }
        freeifaddrs(interfaces);
    }
    check("getifaddrs finds a named IPv4 interface", own.sin_addr.s_addr != 0 && own_name[0]);
    char named[IF_NAMESIZE] = "";
    unsigned index = if_nametoindex(own_name);
    check("if_nametoindex and if_indextoname agree",
          index > 0 && if_indextoname(index, named) && strcmp(named, own_name) == 0);
    a = socket(AF_INET, SOCK_DGRAM, 0);
    b = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in own_a = own, own_b = own;
    own_a.sin_port = 0;
    own_b.sin_port = 0;
    bind(a, (struct sockaddr *)&own_a, sizeof own_a);
    bind(b, (struct sockaddr *)&own_b, sizeof own_b);
    length = sizeof own_b;
    getsockname(b, (struct sockaddr *)&own_b, &length);
    sendto(a, "own", 4, 0, (struct sockaddr *)&own_b, sizeof own_b);
    entry = (struct pollfd){.fd = b, .events = POLLIN};
    length = sizeof from;
    check("UDP to our own address arrives",
          poll(&entry, 1, 2000) == 1 && recvfrom(b, text, sizeof text, 0, (struct sockaddr *)&from, &length) == 4 &&
              from.sin_addr.s_addr == own.sin_addr.s_addr);
    close(a);
    close(b);

    printf(failures ? "%d FAILED\n" : "all passed\n", failures);
    return failures != 0;
}
