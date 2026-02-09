#ifndef STATS_H
#define STATS_H

#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>

typedef struct {
    _Atomic uint64_t recv_packets;
    _Atomic uint64_t recv_bytes;
    _Atomic uint64_t processed;
    _Atomic uint64_t vad_active;
    _Atomic uint64_t parse_errors;
    _Atomic uint64_t recv_errors;
    _Atomic uint64_t channel_drops;
} stats_t;

static inline void stats_init(stats_t *s) {
    atomic_store_explicit(&s->recv_packets,  0, memory_order_relaxed);
    atomic_store_explicit(&s->recv_bytes,    0, memory_order_relaxed);
    atomic_store_explicit(&s->processed,     0, memory_order_relaxed);
    atomic_store_explicit(&s->vad_active,    0, memory_order_relaxed);
    atomic_store_explicit(&s->parse_errors,  0, memory_order_relaxed);
    atomic_store_explicit(&s->recv_errors,   0, memory_order_relaxed);
    atomic_store_explicit(&s->channel_drops, 0, memory_order_relaxed);
}

static inline void stats_record_recv(stats_t *s, uint64_t bytes) {
    atomic_fetch_add_explicit(&s->recv_packets, 1, memory_order_relaxed);
    atomic_fetch_add_explicit(&s->recv_bytes, bytes, memory_order_relaxed);
}

static inline void stats_record_processed(stats_t *s, int is_active) {
    atomic_fetch_add_explicit(&s->processed, 1, memory_order_relaxed);
    if (is_active)
        atomic_fetch_add_explicit(&s->vad_active, 1, memory_order_relaxed);
}

static inline void stats_record_parse_error(stats_t *s) {
    atomic_fetch_add_explicit(&s->parse_errors, 1, memory_order_relaxed);
}

static inline void stats_record_recv_error(stats_t *s) {
    atomic_fetch_add_explicit(&s->recv_errors, 1, memory_order_relaxed);
}

static inline void stats_record_channel_drop(stats_t *s) {
    atomic_fetch_add_explicit(&s->channel_drops, 1, memory_order_relaxed);
}

static inline void stats_print_and_reset(stats_t *s, double elapsed_secs,
                                         const char *transport)
{
    if (elapsed_secs < 0.001) elapsed_secs = 0.001;

    uint64_t pkts    = atomic_exchange_explicit(&s->recv_packets,  0, memory_order_relaxed);
    uint64_t bytes   = atomic_exchange_explicit(&s->recv_bytes,    0, memory_order_relaxed);
    uint64_t proc    = atomic_exchange_explicit(&s->processed,     0, memory_order_relaxed);
    uint64_t active  = atomic_exchange_explicit(&s->vad_active,    0, memory_order_relaxed);
    uint64_t perr    = atomic_exchange_explicit(&s->parse_errors,  0, memory_order_relaxed);
    uint64_t rerr    = atomic_exchange_explicit(&s->recv_errors,   0, memory_order_relaxed);
    uint64_t drops   = atomic_exchange_explicit(&s->channel_drops, 0, memory_order_relaxed);

    printf("[STATS] %s: %.0f pps, %.2f Mbps | VAD: %.0f proc/s, %lu active | "
           "errors: parse=%lu recv=%lu drops=%lu\n",
           transport,
           pkts / elapsed_secs,
           (bytes * 8.0) / (elapsed_secs * 1e6),
           proc / elapsed_secs,
           (unsigned long)active,
           (unsigned long)perr,
           (unsigned long)rerr,
           (unsigned long)drops);
    fflush(stdout);
}

#endif /* STATS_H */
