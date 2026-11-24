/*
 * corsia-test: checks the C client against a running `corsia serve`.
 *
 *   cc -O2 -Wall -Wextra -o corsia-test corsia-test.c
 *   ./corsia-test SOCKET ORIGINAL [BATCH]
 *
 * Reads every 4 MiB range of the served model once, in a scrambled order, BATCH ranges
 * per request (default 4), with odd offsets and lengths to exercise alignment. Each range
 * is compared byte for byte with the same bytes read from ORIGINAL. Read-only.
 */
#define _GNU_SOURCE
#include "corsia-client.h"

#include <fcntl.h>
#include <time.h>

#define BLOCK (4u << 20)
#define SLOT (BLOCK + 8192u)

int main(int argc, char **argv) {
    char err[256];
    if (argc < 3) {
        fprintf(stderr, "usage: %s SOCKET ORIGINAL [BATCH]\n", argv[0]);
        return 2;
    }
    size_t batch = argc > 3 ? (size_t)atoi(argv[3]) : 4;
    if (batch < 1 || batch > 64) batch = 4;
    corsia_client *c = corsia_connect(argv[1], SLOT * batch, err, sizeof err);
    if (!c) {
        fprintf(stderr, "corsia-test: %s\n", err);
        return 1;
    }
    int fd = open(argv[2], O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        perror("corsia-test: open original");
        return 1;
    }
    uint64_t size = corsia_model_bytes(c);
    uint64_t blocks = (size + BLOCK - 1) / BLOCK;
    uint8_t *check = malloc(BLOCK);
    corsia_range *r = calloc(batch, sizeof *r);
    corsia_landed *l = calloc(batch, sizeof *l);
    if (!check || !r || !l) return 1;

    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    uint64_t done = 0, bytes = 0, bad = 0, from_copy = 0;
    /* A fixed odd stride visits every block once in a scrambled order. */
    uint64_t stride = 2654435761u % blocks;
    while (blocks % stride == 0 && stride > 1) stride--;
    if (stride == 0) stride = 1;
    uint64_t at = 0;
    while (done < blocks) {
        size_t k = 0;
        for (; k < batch && done < blocks; k++, done++) {
            uint64_t b = at;
            at = (at + stride) % blocks;
            uint64_t off = b * BLOCK, len = size - off < BLOCK ? size - off : BLOCK;
            uint64_t skew = (b * 7919) % 4096;
            if (skew >= len) skew = len - 1;
            off += skew;
            len -= skew;
            uint64_t trim = (b * 104729) % 8192;
            len = len > trim ? len - trim : 1;
            r[k].offset = off;
            r[k].len = (uint32_t)len;
            r[k].arena_off = (uint64_t)k * SLOT;
        }
        if (corsia_read(c, r, k, l) != 0) {
            fprintf(stderr, "corsia-test: read failed: %s\n", strerror(errno));
            return 1;
        }
        for (size_t i = 0; i < k; i++) {
            ssize_t n = pread(fd, check, r[i].len, (off_t)r[i].offset);
            if (n != (ssize_t)l[i].got || memcmp(corsia_arena(c) + l[i].data_off, check, (size_t)n) != 0) bad++;
            bytes += l[i].got;
            from_copy += l[i].source != 0;
        }
    }
    clock_gettime(CLOCK_MONOTONIC, &t1);
    double secs = (double)(t1.tv_sec - t0.tv_sec) + (double)(t1.tv_nsec - t0.tv_nsec) / 1e9;
    printf("%llu ranges, %llu MiB in %.1f s (%.0f MB/s with checking), %llu started on a copy, %llu mismatches\n",
           (unsigned long long)blocks, (unsigned long long)(bytes >> 20), secs, (double)bytes / secs / 1e6,
           (unsigned long long)from_copy, (unsigned long long)bad);
    corsia_close(c);
    close(fd);
    free(check);
    free(r);
    free(l);
    return bad == 0 ? 0 : 1;
}
