// Read bandwidth probe: how fast can this machine stream memory, with 1, 2 and 4 threads,
// on 4 KiB pages and on transparent huge pages. Decode streams the weights once per token,
// so this is the ceiling the kernels should approach.
// Build: cc -O2 -mavx -pthread membw.c -o membw
// Run: membw [GiB]   membw map GIB   membw scan GIB   membw hold fast|slow GIB
#define _GNU_SOURCE
#include <immintrin.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

typedef struct { const uint8_t * p; size_t n; double sink; } job;

static void * reader(void * arg) {
    job * j = arg;
    __m256 acc0 = _mm256_setzero_ps(), acc1 = _mm256_setzero_ps(), acc2 = _mm256_setzero_ps(), acc3 = _mm256_setzero_ps();
    for (size_t i = 0; i + 128 <= j->n; i += 128) {
        acc0 = _mm256_add_ps(acc0, _mm256_load_ps((const float *) (j->p + i)));
        acc1 = _mm256_add_ps(acc1, _mm256_load_ps((const float *) (j->p + i + 32)));
        acc2 = _mm256_add_ps(acc2, _mm256_load_ps((const float *) (j->p + i + 64)));
        acc3 = _mm256_add_ps(acc3, _mm256_load_ps((const float *) (j->p + i + 96)));
    }
    float out[8];
    _mm256_storeu_ps(out, _mm256_add_ps(_mm256_add_ps(acc0, acc1), _mm256_add_ps(acc2, acc3)));
    j->sink = out[0];
    return NULL;
}

static double run(const uint8_t * buf, size_t size, int nt) {
    pthread_t th[16];
    job jobs[16];
    double best = 0;
    for (int rep = 0; rep < 5; rep++) {
        const double t0 = now();
        for (int t = 0; t < nt; t++) {
            jobs[t].p = buf + size / nt * t;
            jobs[t].n = size / nt;
            pthread_create(&th[t], NULL, reader, &jobs[t]);
        }
        for (int t = 0; t < nt; t++) pthread_join(th[t], NULL);
        const double gbs = size / (now() - t0) / 1e9;
        if (gbs > best) best = gbs;
    }
    return best;
}

// scan mode: fill N GiB and time each 1 GiB slice with 4 threads. Mismatched modules run in
// flex mode (only the matched part dual-channel), which shows as two speeds across slices.
static int scan(size_t gib) {
    const size_t slice = 1ull << 30;
    uint8_t * buf = NULL;
    if (posix_memalign((void **) &buf, 2 << 20, gib * slice) != 0) return 1;
    madvise(buf, gib * slice, MADV_HUGEPAGE);
    memset(buf, 1, gib * slice);
    for (size_t i = 0; i < gib; i++) printf("slice %2zu: %.1f GB/s\n", i, run(buf + i * slice, slice, 4));
    free(buf);
    return 0;
}

static int cmp_d(const void * a, const void * b) {
    const double x = *(const double *) a, y = *(const double *) b;
    return x < y ? -1 : x > y;
}

// hold mode: take N GiB in 64 MiB chunks, time each with 4 threads, give back the fast chunks
// (hold slow) or the slow ones (hold fast) and keep the rest until killed. What the kernel
// hands out next (a model's page cache, a pool) then lands in the kind that was given back.
static int hold(const char * keep, size_t gib) {
    const size_t chunk = 64ull << 20, n = gib * (1ull << 30) / chunk;
    uint8_t ** bufs = calloc(n, sizeof *bufs);
    double * speed = calloc(n, sizeof *speed), * sorted = calloc(n, sizeof *sorted);
    for (size_t i = 0; i < n; i++) {
        bufs[i] = mmap(NULL, chunk, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (bufs[i] == MAP_FAILED) return 1;
        madvise(bufs[i], chunk, MADV_HUGEPAGE);
        memset(bufs[i], 1, chunk);
    }
    for (size_t i = 0; i < n; i++) {
        sorted[i] = speed[i] = run(bufs[i], chunk, 4);
    }
    qsort(sorted, n, sizeof *sorted, cmp_d);
    const double thr = (sorted[n / 10] + sorted[n - 1 - n / 10]) / 2;
    printf("chunks %zu: p10 %.1f, median %.1f, p90 %.1f GB/s; split at %.1f\n", n, sorted[n / 10], sorted[n / 2], sorted[n - 1 - n / 10], thr);
    for (int b = 8; b <= 34; b += 2) {
        size_t c = 0;
        for (size_t i = 0; i < n; i++) c += speed[i] >= b && speed[i] < b + 2;
        if (c) printf("  %2d to %2d GB/s: %zu chunks (%.1f GiB)\n", b, b + 2, c, c / 16.0);
    }
    const int keep_fast = strcmp(keep, "fast") == 0;
    size_t kept = 0;
    double sum = 0;
    for (size_t i = 0; i < n; i++) {
        if ((speed[i] >= thr) == keep_fast) {
            kept++;
            sum += speed[i];
        } else {
            munmap(bufs[i], chunk);
        }
    }
    printf("holding %zu %s chunks (%.1f GiB, mean %.1f GB/s), gave back %.1f GiB\n", kept, keep, kept / 16.0, kept ? sum / kept : 0, (n - kept) / 16.0);
    fflush(stdout);
    for (;;) pause();
}

static size_t mem_free_mib(void) {
    FILE * f = fopen("/proc/meminfo", "r");
    char line[128];
    size_t kb = 0;
    while (f && fgets(line, sizeof line, f)) {
        if (sscanf(line, "MemFree: %zu kB", &kb) == 1) break;
    }
    if (f) fclose(f);
    return kb >> 10;
}

static const double * guard_speed;
static int cmp_by_speed_desc(const void * a, const void * b) {
    const double x = guard_speed[*(const size_t *) a], y = guard_speed[*(const size_t *) b];
    return x > y ? -1 : x < y;
}

// guard mode, for a model load on a machine whose memory is uneven: takes N GiB, times it,
// gives back the fast chunks at once and keeps the slow ones, so what loads next lands in the
// fast memory. While it holds anything it watches the free memory: under 1 GiB free it lets go
// of 1 GiB, fastest first, so the load is never squeezed and the hottest data, loaded first,
// gets the fastest memory. Ends when it holds nothing; SIGTERM ends it at once.
static int guard(size_t gib) {
    const size_t chunk = 64ull << 20, n = gib * (1ull << 30) / chunk;
    uint8_t ** bufs = calloc(n, sizeof *bufs);
    double * speed = calloc(n, sizeof *speed), * sorted = calloc(n, sizeof *sorted);
    size_t * held = calloc(n, sizeof *held);
    if (n < 10) return 1;
    for (size_t i = 0; i < n; i++) {
        bufs[i] = mmap(NULL, chunk, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (bufs[i] == MAP_FAILED) return 1;
        madvise(bufs[i], chunk, MADV_HUGEPAGE);
        memset(bufs[i], 1, chunk);
    }
    for (size_t i = 0; i < n; i++) sorted[i] = speed[i] = run(bufs[i], chunk, 4);
    qsort(sorted, n, sizeof *sorted, cmp_d);
    const double lo = sorted[n / 10], hi = sorted[n - 1 - n / 10], thr = (lo + hi) / 2;
    size_t nh = 0;
    double sf = 0, ss = 0;
    for (size_t i = 0; i < n; i++) {
        if (hi / lo < 1.25 || speed[i] >= thr) { // even memory: nothing worth holding
            sf += speed[i];
            munmap(bufs[i], chunk);
        } else {
            ss += speed[i];
            held[nh++] = i;
        }
    }
    guard_speed = speed;
    qsort(held, nh, sizeof *held, cmp_by_speed_desc);
    printf("guard: holding %.1f GiB of slow memory (%.1f GB/s), gave back %.1f GiB of fast (%.1f GB/s)\n",
        nh / 16.0, nh ? ss / nh : 0, (n - nh) / 16.0, n > nh ? sf / (n - nh) : 0);
    fflush(stdout);
    size_t next = 0;
    while (next < nh) {
        usleep(100 * 1000);
        if (mem_free_mib() < 1024) {
            for (size_t k = 0; k < 16 && next < nh; k++) munmap(bufs[held[next++]], chunk);
        }
    }
    printf("guard: all released\n");
    return 0;
}

// map mode: time N GiB in 64 MiB chunks and say how much of it is fast and how much slow.
// One line for scripts: "map: GIB fast FAST_GIB FAST_GBS slow SLOW_GIB SLOW_GBS ratio R".
// A ratio under 1.25 means the memory is even.
static int map(size_t gib) {
    const size_t chunk = 64ull << 20, n = gib * (1ull << 30) / chunk;
    double * speed = calloc(n, sizeof *speed), * sorted = calloc(n, sizeof *sorted);
    uint8_t * buf = mmap(NULL, n * chunk, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buf == MAP_FAILED || n < 10) return 1;
    madvise(buf, n * chunk, MADV_HUGEPAGE);
    memset(buf, 1, n * chunk);
    for (size_t i = 0; i < n; i++) sorted[i] = speed[i] = run(buf + i * chunk, chunk, 4);
    qsort(sorted, n, sizeof *sorted, cmp_d);
    const double lo = sorted[n / 10], hi = sorted[n - 1 - n / 10], thr = (lo + hi) / 2;
    size_t nf = 0;
    double sf = 0, ss = 0;
    for (size_t i = 0; i < n; i++) {
        if (speed[i] >= thr) { nf++; sf += speed[i]; } else { ss += speed[i]; }
    }
    printf("map: %zu fast %.1f %.1f slow %.1f %.1f ratio %.2f\n", gib, nf / 16.0, nf ? sf / nf : 0,
        (n - nf) / 16.0, n > nf ? ss / (n - nf) : 0, lo > 0 ? hi / lo : 0);
    return 0;
}

int main(int argc, char ** argv) {
    if (argc > 2 && strcmp(argv[1], "map") == 0) return map((size_t) atoi(argv[2]));
    if (argc > 2 && strcmp(argv[1], "guard") == 0) return guard((size_t) atoi(argv[2]));
    if (argc > 2 && strcmp(argv[1], "scan") == 0) return scan((size_t) atoi(argv[2]));
    if (argc > 3 && strcmp(argv[1], "hold") == 0) return hold(argv[2], (size_t) atoi(argv[3]));
    const size_t size = (size_t) (argc > 1 ? atof(argv[1]) : 2.0) * (1ull << 30);
    for (int huge = 0; huge <= 1; huge++) {
        uint8_t * buf = NULL;
        if (posix_memalign((void **) &buf, 2 << 20, size) != 0) return 1;
        madvise(buf, size, huge ? MADV_HUGEPAGE : MADV_NOHUGEPAGE);
        memset(buf, 1, size);
        printf("%s pages:", huge ? "2 MiB" : "4 KiB");
        for (int nt = 1; nt <= 4; nt *= 2) printf("  %d thread%s %.1f GB/s", nt, nt > 1 ? "s" : "", run(buf, size, nt));
        printf("\n");
        free(buf);
    }
    return 0;
}
