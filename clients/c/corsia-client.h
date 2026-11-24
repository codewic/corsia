/*
 * corsia-client.h: read a model through a Corsia daemon (hinted mode), straight into
 * shared memory. Header only, C99, Linux. MIT OR Apache-2.0.
 *
 * Start the daemon with `corsia serve MODEL ...`; it prints the socket, usually
 * $XDG_RUNTIME_DIR/corsia/<model>.sock. Then:
 *
 *     char err[256];
 *     corsia_client *c = corsia_connect(socket_path, 256u << 20, err, sizeof err);
 *     corsia_range r = { .offset = off, .len = n, .arena_off = 0 };
 *     corsia_landed l;
 *     if (corsia_read(c, &r, 1, &l) == 0)
 *         use(corsia_arena(c) + l.data_off, l.got);
 *     corsia_close(c);
 *
 * Put several ranges in one corsia_read: Corsia spreads all of them over its drives at
 * once. corsia_prefetch does the same into the page cache of Corsia's mounted file, for
 * engines that memory-map the model from the mount. Each range's arena_off must be a multiple of 4096 with len + 8192 bytes of room.
 * Protocol v1 is described in corsia/src/hint.rs. Assumes a little-endian CPU.
 */
#ifndef CORSIA_CLIENT_H
#define CORSIA_CLIENT_H

/* syscall() and ftruncate() need this in strict C modes; include this header first, or
 * build with -std=gnu11 (C++ compilers define it already). */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif

#define CORSIA_MAGIC 0x53524f43u /* "CORS" */
#define CORSIA_VERSION 1u
#define CORSIA_ALIGN 4096u
#define CORSIA_MAX_RANGES 2048u
#define CORSIA_HEAD 8u
#define CORSIA_MAX_MSG (CORSIA_HEAD + 16u + CORSIA_MAX_RANGES * 24u)

typedef struct {
    uint64_t offset;    /* byte offset in the model file */
    uint32_t len;       /* bytes wanted */
    uint64_t arena_off; /* where to land it: multiple of 4096, len + 8192 bytes free */
} corsia_range;

typedef struct {
    uint64_t data_off; /* the range's first byte is at corsia_arena(c) + data_off */
    uint32_t got;      /* bytes delivered (short only at end of file) */
    uint8_t source;    /* 0 = the original file, 1+ = a copy on another drive */
} corsia_landed;

typedef struct corsia_client {
    int fd;
    uint8_t *arena;
    size_t arena_bytes;
    uint64_t model_bytes;
    uint64_t batch;
    uint8_t *msg;
    int arena_fd; /* kept so a second connection can share the arena */
} corsia_client;

static inline void corsia__put16(uint8_t *p, uint16_t v) { memcpy(p, &v, 2); }
static inline void corsia__put32(uint8_t *p, uint32_t v) { memcpy(p, &v, 4); }
static inline void corsia__put64(uint8_t *p, uint64_t v) { memcpy(p, &v, 8); }
static inline uint16_t corsia__get16(const uint8_t *p) { uint16_t v; memcpy(&v, p, 2); return v; }
static inline uint32_t corsia__get32(const uint8_t *p) { uint32_t v; memcpy(&v, p, 4); return v; }
static inline uint64_t corsia__get64(const uint8_t *p) { uint64_t v; memcpy(&v, p, 8); return v; }

static inline void corsia__header(uint8_t *p, uint16_t type) {
    corsia__put32(p, CORSIA_MAGIC);
    corsia__put16(p + 4, CORSIA_VERSION);
    corsia__put16(p + 6, type);
}

/* Returns the message type, or -1 if this is not a Corsia v1 message. */
static inline int corsia__type(const uint8_t *p, ssize_t n) {
    if (n < (ssize_t)CORSIA_HEAD || corsia__get32(p) != CORSIA_MAGIC || corsia__get16(p + 4) != CORSIA_VERSION) return -1;
    return corsia__get16(p + 6);
}

static inline int corsia__send(int fd, const void *data, size_t len, int pass_fd) {
    struct iovec iov;
    struct msghdr msg;
    union { struct cmsghdr align; char buf[CMSG_SPACE(sizeof(int))]; } ctl;
    memset(&msg, 0, sizeof msg);
    iov.iov_base = (void *)data;
    iov.iov_len = len;
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    if (pass_fd >= 0) {
        struct cmsghdr *c;
        memset(&ctl, 0, sizeof ctl);
        msg.msg_control = ctl.buf;
        msg.msg_controllen = sizeof ctl.buf;
        c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &pass_fd, sizeof(int));
    }
    for (;;) {
        ssize_t n = sendmsg(fd, &msg, MSG_NOSIGNAL);
        if (n >= 0) return 0;
        if (errno != EINTR) return -1;
    }
}

static inline ssize_t corsia__recv(int fd, uint8_t *buf, size_t cap) {
    for (;;) {
        ssize_t n = recv(fd, buf, cap, 0);
        if (n >= 0 || errno != EINTR) return n;
    }
}

static inline void corsia__err(char *err, size_t errlen, const char *what) {
    if (err && errlen) snprintf(err, errlen, "%s: %s", what, strerror(errno));
}

/* Connects using mfd (an arena of arena_bytes the client then owns). */
static inline corsia_client *corsia__connect_fd(const char *socket_path, int mfd, size_t arena_bytes, char *err, size_t errlen) {
    struct sockaddr_un addr;
    corsia_client *c;
    uint8_t hello[CORSIA_HEAD + 8];
    ssize_t n;

    if (strlen(socket_path) >= sizeof addr.sun_path) {
        errno = ENAMETOOLONG;
        corsia__err(err, errlen, "socket path");
        close(mfd);
        return NULL;
    }
    c = (corsia_client *)calloc(1, sizeof *c);
    if (!c) { close(mfd); return NULL; }
    c->fd = -1;
    c->arena_fd = mfd;
    c->arena = (uint8_t *)MAP_FAILED;
    c->msg = (uint8_t *)malloc(CORSIA_MAX_MSG);
    if (!c->msg) goto fail;
    c->fd = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    if (c->fd < 0) { corsia__err(err, errlen, "socket"); goto fail; }
    memset(&addr, 0, sizeof addr);
    addr.sun_family = AF_UNIX;
    memcpy(addr.sun_path, socket_path, strlen(socket_path));
    if (connect(c->fd, (struct sockaddr *)&addr, sizeof addr) != 0) { corsia__err(err, errlen, "connect"); goto fail; }

    c->arena = (uint8_t *)mmap(NULL, arena_bytes, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if (c->arena == (uint8_t *)MAP_FAILED) { corsia__err(err, errlen, "mmap"); goto fail; }
    c->arena_bytes = arena_bytes;

    corsia__header(hello, 0x00);
    corsia__put64(hello + CORSIA_HEAD, (uint64_t)arena_bytes);
    if (corsia__send(c->fd, hello, sizeof hello, mfd) != 0) { corsia__err(err, errlen, "send HELLO"); goto fail; }
    n = corsia__recv(c->fd, c->msg, CORSIA_MAX_MSG);
    if (corsia__type(c->msg, n) != 0x80 || n < (ssize_t)(CORSIA_HEAD + 8)) {
        if (err && errlen) {
            if (corsia__type(c->msg, n) == 0xFF && n > (ssize_t)(CORSIA_HEAD + 4))
                snprintf(err, errlen, "Corsia refused: %.*s", (int)(n - CORSIA_HEAD - 4), (const char *)c->msg + CORSIA_HEAD + 4);
            else
                snprintf(err, errlen, "no valid answer from Corsia");
        }
        goto fail;
    }
    c->model_bytes = corsia__get64(c->msg + CORSIA_HEAD);
    return c;

fail:
    if (c->arena != (uint8_t *)MAP_FAILED) munmap(c->arena, arena_bytes);
    if (c->fd >= 0) close(c->fd);
    if (c->arena_fd >= 0) close(c->arena_fd);
    free(c->msg);
    free(c);
    return NULL;
}

/* Connects and shares an arena of arena_bytes. NULL on failure, with a reason in err. */
static inline corsia_client *corsia_connect(const char *socket_path, size_t arena_bytes, char *err, size_t errlen) {
    int mfd = (int)syscall(SYS_memfd_create, "corsia-arena", MFD_CLOEXEC);
    if (mfd < 0) { corsia__err(err, errlen, "memfd_create"); return NULL; }
    if (ftruncate(mfd, (off_t)arena_bytes) != 0) { corsia__err(err, errlen, "ftruncate"); close(mfd); return NULL; }
    return corsia__connect_fd(socket_path, mfd, arena_bytes, err, errlen);
}

/* A second connection on the same arena as first (the same memory, mapped again), for
 * reads from another thread: data_off in its results are offsets into that same arena. */
static inline corsia_client *corsia_connect_shared(const char *socket_path, const corsia_client *first, char *err, size_t errlen) {
    int mfd = dup(first->arena_fd);
    if (mfd < 0) { corsia__err(err, errlen, "dup"); return NULL; }
    return corsia__connect_fd(socket_path, mfd, first->arena_bytes, err, errlen);
}

static inline uint64_t corsia_model_bytes(const corsia_client *c) { return c->model_bytes; }
static inline uint8_t *corsia_arena(const corsia_client *c) { return c->arena; }
static inline size_t corsia_arena_bytes(const corsia_client *c) { return c->arena_bytes; }

/* Reads n ranges at once. 0 on success; -1 with errno set on failure. */
static inline int corsia_read(corsia_client *c, const corsia_range *r, size_t n, corsia_landed *out) {
    uint8_t *m = c->msg;
    size_t i, len;
    ssize_t got;
    int32_t status;

    if (n == 0 || n > CORSIA_MAX_RANGES) { errno = EINVAL; return -1; }
    c->batch++;
    corsia__header(m, 0x01);
    corsia__put64(m + CORSIA_HEAD, c->batch);
    corsia__put32(m + CORSIA_HEAD + 8, (uint32_t)n);
    corsia__put32(m + CORSIA_HEAD + 12, 0);
    for (i = 0; i < n; i++) {
        uint8_t *e = m + CORSIA_HEAD + 16 + i * 24;
        corsia__put64(e, r[i].offset);
        corsia__put32(e + 8, r[i].len);
        corsia__put32(e + 12, 0);
        corsia__put64(e + 16, r[i].arena_off);
    }
    len = CORSIA_HEAD + 16 + n * 24;
    if (corsia__send(c->fd, m, len, -1) != 0) return -1;
    got = corsia__recv(c->fd, m, CORSIA_MAX_MSG);
    if (corsia__type(m, got) != 0x81 || got < (ssize_t)(CORSIA_HEAD + 16)) { errno = EPROTO; return -1; }
    memcpy(&status, m + CORSIA_HEAD + 8, 4);
    if (status != 0) { errno = status; return -1; }
    if (corsia__get32(m + CORSIA_HEAD + 12) != n || got < (ssize_t)(CORSIA_HEAD + 16 + n * 16)) { errno = EPROTO; return -1; }
    for (i = 0; i < n; i++) {
        const uint8_t *e = m + CORSIA_HEAD + 16 + i * 16;
        out[i].data_off = corsia__get64(e);
        out[i].got = corsia__get32(e + 8);
        out[i].source = e[12];
        if (out[i].data_off + out[i].got > c->arena_bytes) { errno = EPROTO; return -1; }
    }
    return 0;
}

/* For engines that memory-map the model from Corsia's mount: Corsia reads the n ranges
 * (offset and len; arena_off is ignored) from its drives and stores them straight into
 * the kernel's page cache for the mounted file, then returns. 0 on success, -1 with
 * errno set on failure. If bytes is not NULL it receives the bytes stored. */
static inline int corsia__send_prefetch(corsia_client *c, const corsia_range *r, size_t n, uint32_t flags) {
    uint8_t *m = c->msg;
    size_t i;
    if (n == 0 || n > CORSIA_MAX_RANGES) { errno = EINVAL; return -1; }
    c->batch++;
    corsia__header(m, 0x02);
    corsia__put64(m + CORSIA_HEAD, c->batch);
    corsia__put32(m + CORSIA_HEAD + 8, (uint32_t)n);
    corsia__put32(m + CORSIA_HEAD + 12, flags);
    for (i = 0; i < n; i++) {
        uint8_t *e = m + CORSIA_HEAD + 16 + i * 16;
        corsia__put64(e, r[i].offset);
        corsia__put32(e + 8, r[i].len);
        corsia__put32(e + 12, 0);
    }
    return corsia__send(c->fd, m, CORSIA_HEAD + 16 + n * 16, -1);
}

static inline int corsia_prefetch(corsia_client *c, const corsia_range *r, size_t n, uint64_t *bytes) {
    uint8_t *m = c->msg;
    ssize_t got;
    int32_t status;

    if (corsia__send_prefetch(c, r, n, 0) != 0) return -1;
    got = corsia__recv(c->fd, m, CORSIA_MAX_MSG);
    if (corsia__type(m, got) != 0x82 || got < (ssize_t)(CORSIA_HEAD + 24)) { errno = EPROTO; return -1; }
    memcpy(&status, m + CORSIA_HEAD + 8, 4);
    if (status != 0) { errno = status; return -1; }
    if (bytes) *bytes = corsia__get64(m + CORSIA_HEAD + 16);
    return 0;
}

/* Like corsia_prefetch but returns at once: Corsia works in the background and sends
 * no reply. For guesses (the experts a later layer will probably need). If guesses pile
 * up faster than the drives serve them, Corsia drops the oldest. */
static inline int corsia_prefetch_async(corsia_client *c, const corsia_range *r, size_t n) {
    return corsia__send_prefetch(c, r, n, 1);
}

static inline void corsia_close(corsia_client *c) {
    if (!c) return;
    if (c->arena != (uint8_t *)MAP_FAILED) munmap(c->arena, c->arena_bytes);
    if (c->fd >= 0) close(c->fd);
    if (c->arena_fd >= 0) close(c->arena_fd);
    free(c->msg);
    free(c);
}

#endif /* CORSIA_CLIENT_H */
