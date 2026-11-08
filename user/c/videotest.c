/* Cameras through V4L2, the way Firefox's WebRTC uses them: lists formats,
   sizes and rates, sets a format, maps buffers and streams. Under
   `cargo xtask run`/`test` the camera is xtask's pretend one (camera.rs),
   whose pictures are known exactly: luma (x + y + 2n) & 255 in frame n,
   and eight chroma bars; every frame is checked against that. Without a
   camera it says so and passes. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/videodev2.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("  %-56s %s\n", what, ok ? "ok" : "FAILED");
    if (!ok) failures++;
}

static const uint8_t BARS[8][2] = {{128, 128}, {16, 146}, {166, 16}, {54, 34},
                                   {202, 222}, {90, 240}, {240, 110}, {128, 128}};

/* Whether a frame is the test picture; *number gets its frame number
   (mod 128). */
static int picture_ok(const uint8_t *frame, unsigned width, unsigned height, unsigned *number) {
    unsigned start = frame[0];
    *number = start / 2;
    for (unsigned y = 0; y < height; y++) {
        for (unsigned x = 0; x < width; x += 2) {
            const uint8_t *p = frame + (y * width + x) * 2;
            const uint8_t *bar = BARS[x * 8 / width];
            if (p[0] != ((x + y + start) & 0xFF) || p[2] != ((x + 1 + y + start) & 0xFF) || p[1] != bar[0] ||
                p[3] != bar[1]) {
                printf("    pixel (%u, %u): %u %u %u %u\n", x, y, p[0], p[1], p[2], p[3]);
                return 0;
            }
        }
    }
    return 1;
}

/* Streams `frames` frames at width x height and checks them. */
static void stream(int fd, unsigned width, unsigned height, int frames) {
    printf("%ux%u:\n", width, height);
    struct v4l2_format format = {.type = V4L2_BUF_TYPE_VIDEO_CAPTURE};
    format.fmt.pix.width = width;
    format.fmt.pix.height = height;
    format.fmt.pix.pixelformat = V4L2_PIX_FMT_YUYV;
    check("S_FMT takes the size", ioctl(fd, VIDIOC_S_FMT, &format) == 0 && format.fmt.pix.width == width &&
                                      format.fmt.pix.height == height &&
                                      format.fmt.pix.bytesperline == width * 2 &&
                                      format.fmt.pix.sizeimage == width * height * 2);
    struct v4l2_streamparm parameters = {.type = V4L2_BUF_TYPE_VIDEO_CAPTURE};
    parameters.parm.capture.timeperframe.numerator = 1;
    parameters.parm.capture.timeperframe.denominator = 30;
    check("S_PARM sets 30 frames a second", ioctl(fd, VIDIOC_S_PARM, &parameters) == 0 &&
                                                parameters.parm.capture.timeperframe.denominator == 30);

    struct v4l2_requestbuffers request = {.count = 4, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE, .memory = V4L2_MEMORY_MMAP};
    check("REQBUFS makes 4 buffers", ioctl(fd, VIDIOC_REQBUFS, &request) == 0 && request.count == 4);
    void *mapped[4] = {0};
    size_t lengths[4] = {0};
    int ok = 1;
    for (unsigned i = 0; i < request.count && i < 4; i++) {
        struct v4l2_buffer buffer = {.index = i, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE, .memory = V4L2_MEMORY_MMAP};
        if (ioctl(fd, VIDIOC_QUERYBUF, &buffer) != 0 || buffer.length < width * height * 2) {
            ok = 0;
            continue;
        }
        lengths[i] = buffer.length;
        mapped[i] = mmap(NULL, buffer.length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, buffer.m.offset);
        if (mapped[i] == MAP_FAILED) {
            printf("    mmap: %s\n", strerror(errno));
            mapped[i] = NULL;
            ok = 0;
        }
        if (ioctl(fd, VIDIOC_QBUF, &buffer) != 0) ok = 0;
    }
    check("QUERYBUF, mmap and QBUF each", ok);

    int other = open("/dev/video0", O_RDWR);
    struct v4l2_requestbuffers stolen = {.count = 2, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE, .memory = V4L2_MEMORY_MMAP};
    check("another open file can't take the buffers", other >= 0 && ioctl(other, VIDIOC_REQBUFS, &stolen) == -1 &&
                                                           errno == EBUSY);
    close(other);

    enum v4l2_buf_type type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    check("STREAMON", ioctl(fd, VIDIOC_STREAMON, &type) == 0);
    int good = 0, ordered = 1, timely = 1;
    unsigned last_sequence = 0, last_number = 0;
    uint64_t last_time = 0;
    for (int i = 0; i < frames; i++) {
        struct pollfd waiting = {.fd = fd, .events = POLLIN};
        if (poll(&waiting, 1, 3000) != 1 || !(waiting.revents & POLLIN)) {
            printf("    no frame in 3 seconds (revents %#x)\n", waiting.revents);
            timely = 0;
            break;
        }
        struct v4l2_buffer buffer = {.type = V4L2_BUF_TYPE_VIDEO_CAPTURE, .memory = V4L2_MEMORY_MMAP};
        if (ioctl(fd, VIDIOC_DQBUF, &buffer) != 0 || buffer.index >= 4 || !mapped[buffer.index]) {
            printf("    DQBUF: %s\n", strerror(errno));
            break;
        }
        unsigned number;
        if (buffer.bytesused == width * height * 2 && picture_ok(mapped[buffer.index], width, height, &number)) {
            good++;
        } else {
            printf("    frame %d: %u bytes\n", i, buffer.bytesused);
        }
        uint64_t time = (uint64_t)buffer.timestamp.tv_sec * 1000000 + buffer.timestamp.tv_usec;
        if (i > 0 && (buffer.sequence <= last_sequence || time <= last_time || number == last_number)) ordered = 0;
        last_sequence = buffer.sequence;
        last_time = time;
        last_number = number;
        ioctl(fd, VIDIOC_QBUF, &buffer);
    }
    check("frames arrive", timely);
    check("every frame is the whole test picture", good == frames);
    check("sequence numbers and timestamps go forward", ordered);
    check("STREAMOFF", ioctl(fd, VIDIOC_STREAMOFF, &type) == 0);
    for (int i = 0; i < 4; i++)
        if (mapped[i]) munmap(mapped[i], lengths[i]);
    request.count = 0;
    check("REQBUFS 0 frees the buffers", ioctl(fd, VIDIOC_REQBUFS, &request) == 0);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("V4L2 camera test (pid %d)\n", getpid());
    int fd = open("/dev/video0", O_RDWR);
    if (fd < 0 && errno == ENOENT) {
        printf("no camera; skipped\n");
        return 0;
    }
    check("/dev/video0 opens", fd >= 0);
    if (fd < 0) return 1;

    struct v4l2_capability capability;
    check("QUERYCAP: a streaming capture device",
          ioctl(fd, VIDIOC_QUERYCAP, &capability) == 0 && (capability.device_caps & V4L2_CAP_VIDEO_CAPTURE) &&
              (capability.device_caps & V4L2_CAP_STREAMING));
    printf("    %s: %s (%s)\n", capability.driver, capability.card, capability.bus_info);
    if (strcmp((char *)capability.card, "Test camera") != 0) {
        printf("not the test camera; only the listing is checked\n");
    }

    struct v4l2_fmtdesc description = {.index = 0, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE};
    check("ENUM_FMT: YUYV first", ioctl(fd, VIDIOC_ENUM_FMT, &description) == 0 &&
                                      description.pixelformat == V4L2_PIX_FMT_YUYV);
    description.index = 1;
    check("and nothing after it", ioctl(fd, VIDIOC_ENUM_FMT, &description) == -1 && errno == EINVAL);

    int sizes = 0;
    for (;; sizes++) {
        struct v4l2_frmsizeenum size = {.index = sizes, .pixel_format = V4L2_PIX_FMT_YUYV};
        if (ioctl(fd, VIDIOC_ENUM_FRAMESIZES, &size) != 0) break;
        printf("    %ux%u:", size.discrete.width, size.discrete.height);
        for (unsigned j = 0;; j++) {
            struct v4l2_frmivalenum interval = {.index = j, .pixel_format = V4L2_PIX_FMT_YUYV,
                                                .width = size.discrete.width, .height = size.discrete.height};
            if (ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &interval) != 0) break;
            printf(" %u/%u", interval.discrete.numerator, interval.discrete.denominator);
        }
        printf("\n");
    }
    check("ENUM_FRAMESIZES: 3 sizes", sizes == 3);
    struct v4l2_frmivalenum interval = {.index = 1, .pixel_format = V4L2_PIX_FMT_YUYV, .width = 320, .height = 240};
    check("ENUM_FRAMEINTERVALS: 1/15 second second", ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &interval) == 0 &&
                                                       interval.discrete.numerator == 1 &&
                                                       interval.discrete.denominator == 15);

    struct v4l2_format format = {.type = V4L2_BUF_TYPE_VIDEO_CAPTURE};
    format.fmt.pix.width = 300;
    format.fmt.pix.height = 200;
    format.fmt.pix.pixelformat = V4L2_PIX_FMT_YUYV;
    check("TRY_FMT picks the nearest size", ioctl(fd, VIDIOC_TRY_FMT, &format) == 0 && format.fmt.pix.width == 320 &&
                                                format.fmt.pix.height == 240);

    if (strcmp((char *)capability.card, "Test camera") == 0) {
        stream(fd, 320, 240, 10);
        stream(fd, 640, 480, 5);
    }
    close(fd);
    printf(failures ? "%d FAILED\n" : "all passed\n", failures);
    return failures != 0;
}
