// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// Memory allocation and access benchmark for evaluating THP effectiveness.
//
// Workloads:
//   fill       - Sequential memset (baseline throughput)
//   fault      - First-touch page faults via stride-1-page sequential write
//   random     - Random 8-byte read-modify-write across the region
//   stride     - Strided read-modify-write simulating working set sweeps
//
// Usage: alloc_bench <block_size_bytes> <iterations> <workload> [hugepage]
// Output: one line per iteration with elapsed nanoseconds

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#define HPAGE_SIZE (2UL * 1024 * 1024)

static inline uint64_t xorshift64(uint64_t *state) {
    uint64_t x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    return x;
}

static inline long long elapsed_ns(struct timespec *start, struct timespec *end) {
    return (long long)(end->tv_sec - start->tv_sec) * 1000000000LL +
           (end->tv_nsec - start->tv_nsec);
}

// Sequential memset — measures raw write bandwidth.
static void bench_fill(volatile char *p, size_t size, int iterations) {
    struct timespec start, end;
    for (int i = 0; i < iterations; i++) {
        clock_gettime(CLOCK_MONOTONIC, &start);
        memset((void *)p, i & 0xff, size);
        __asm__ __volatile__("" : : "r"(p) : "memory");
        clock_gettime(CLOCK_MONOTONIC, &end);
        printf("%lld\n", elapsed_ns(&start, &end));
    }
}

// First-touch fault benchmark — mmap fresh each iteration, touch every page once.
// Measures page fault + allocation cost (most affected by THP).
static void bench_fault(size_t size, int iterations, int use_hugepage) {
    size_t page_size = (size_t)sysconf(_SC_PAGESIZE);
    struct timespec start, end;

    for (int i = 0; i < iterations; i++) {
        void *p = mmap(NULL, size, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) {
            perror("mmap");
            exit(1);
        }
        if (use_hugepage)
            madvise(p, size, MADV_HUGEPAGE);

        clock_gettime(CLOCK_MONOTONIC, &start);
        // Touch one byte per page to trigger faults.
        for (size_t off = 0; off < size; off += page_size)
            ((volatile char *)p)[off] = (char)i;
        __asm__ __volatile__("" : : "r"(p) : "memory");
        clock_gettime(CLOCK_MONOTONIC, &end);

        printf("%lld\n", elapsed_ns(&start, &end));
        munmap(p, size);
    }
}

// Random read-modify-write — measures TLB pressure and cache behavior.
static void bench_random(volatile char *p, size_t size, int iterations) {
    size_t num_words = size / sizeof(uint64_t);
    // Enough ops to touch ~25% of pages per iteration.
    size_t ops = size / (4 * sizeof(uint64_t));
    volatile uint64_t *words = (volatile uint64_t *)p;
    struct timespec start, end;

    for (int i = 0; i < iterations; i++) {
        uint64_t rng = 0xdeadbeefcafe0000ULL ^ (uint64_t)i;
        clock_gettime(CLOCK_MONOTONIC, &start);
        for (size_t op = 0; op < ops; op++) {
            size_t idx = xorshift64(&rng) % num_words;
            words[idx] ^= rng;
        }
        __asm__ __volatile__("" : : "r"(p) : "memory");
        clock_gettime(CLOCK_MONOTONIC, &end);
        printf("%lld\n", elapsed_ns(&start, &end));
    }
}

// Strided read-modify-write — simulates a working set sweep with a large stride,
// stressing TLB with one access per huge-page-sized chunk.
static void bench_stride(volatile char *p, size_t size, int iterations) {
    size_t stride = HPAGE_SIZE; // one access per 2MB chunk
    volatile uint64_t *words = (volatile uint64_t *)p;
    size_t num_steps = size / stride;
    struct timespec start, end;

    for (int i = 0; i < iterations; i++) {
        clock_gettime(CLOCK_MONOTONIC, &start);
        for (size_t s = 0; s < num_steps; s++) {
            // Note: I'm not always reading the first bytes of each page, to avoid measuring depending too much
            // on the exact alignments of the page in memory
            size_t idx = (s * stride) / sizeof(uint64_t) + s;
            words[idx] += (uint64_t)i;
        }
        __asm__ __volatile__("" : : "r"(p) : "memory");
        clock_gettime(CLOCK_MONOTONIC, &end);
        printf("%lld\n", elapsed_ns(&start, &end));
    }
}

int main(int argc, char **argv) {
    if (argc < 4 || argc > 5) {
        fprintf(stderr,
                "Usage: %s <block_size_bytes> <iterations> <workload> [hugepage]\n"
                "Workloads: fill, fault, random, stride\n",
                argv[0]);
        return 1;
    }

    size_t block_size = (size_t)strtoull(argv[1], NULL, 10);
    int iterations = atoi(argv[2]);
    const char *workload = argv[3];
    int use_hugepage = 0;

    if (argc == 5) {
        if (strcmp(argv[4], "hugepage") != 0) {
            fprintf(stderr, "Unknown option: %s\n", argv[4]);
            return 1;
        }
        use_hugepage = 1;
    }

    // The fault workload manages its own mmap per iteration.
    if (strcmp(workload, "fault") == 0) {
        bench_fault(block_size, iterations, use_hugepage);
        return 0;
    }

    void *p = mmap(NULL, block_size, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        perror("mmap");
        return 1;
    }

    if (use_hugepage)
        madvise(p, block_size, MADV_HUGEPAGE);

    // Pre-fault all pages so non-fault workloads measure steady-state.
    memset(p, 0, block_size);

    if (strcmp(workload, "fill") == 0) {
        bench_fill(p, block_size, iterations);
    } else if (strcmp(workload, "random") == 0) {
        bench_random(p, block_size, iterations);
    } else if (strcmp(workload, "stride") == 0) {
        bench_stride(p, block_size, iterations);
    } else {
        fprintf(stderr, "Unknown workload: %s\n", workload);
        munmap(p, block_size);
        return 1;
    }

    munmap(p, block_size);
    return 0;
}
