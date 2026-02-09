#ifndef STATS_H
#define STATS_H

#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <time.h>

typedef struct {
    _Atomic uint64_t udp_packets_received;
    _Atomic uint64_t udp_bytes_received;
    _Atomic uint64_t mqtt_messages_published;
    _Atomic uint64_t parse_errors;
    _Atomic uint64_t mqtt_publish_errors;
    _Atomic uint64_t channel_drops;
} stats_t;

static inline void stats_init(stats_t *s) {
    atomic_store_explicit(&s->udp_packets_received,   0, memory_order_relaxed);
    atomic_store_explicit(&s->udp_bytes_received,     0, memory_order_relaxed);
    atomic_store_explicit(&s->mqtt_messages_published, 0, memory_order_relaxed);
    atomic_store_explicit(&s->parse_errors,           0, memory_order_relaxed);
    atomic_store_explicit(&s->mqtt_publish_errors,    0, memory_order_relaxed);
    atomic_store_explicit(&s->channel_drops,          0, memory_order_relaxed);
}

static inline void stats_record_udp_recv(stats_t *s, uint64_t bytes) {
    atomic_fetch_add_explicit(&s->udp_packets_received, 1, memory_order_relaxed);
    atomic_fetch_add_explicit(&s->udp_bytes_received, bytes, memory_order_relaxed);
}

static inline void stats_record_mqtt_publish(stats_t *s) {
    atomic_fetch_add_explicit(&s->mqtt_messages_published, 1, memory_order_relaxed);
}

static inline void stats_record_parse_error(stats_t *s) {
    atomic_fetch_add_explicit(&s->parse_errors, 1, memory_order_relaxed);
}

static inline void stats_record_mqtt_error(stats_t *s) {
    atomic_fetch_add_explicit(&s->mqtt_publish_errors, 1, memory_order_relaxed);
}

static inline void stats_record_channel_drop(stats_t *s) {
    atomic_fetch_add_explicit(&s->channel_drops, 1, memory_order_relaxed);
}

static inline void stats_print_and_reset(stats_t *s, double elapsed_secs) {
    if (elapsed_secs < 0.001) elapsed_secs = 0.001;

    uint64_t pkts  = atomic_exchange_explicit(&s->udp_packets_received,   0, memory_order_relaxed);
    uint64_t bytes = atomic_exchange_explicit(&s->udp_bytes_received,     0, memory_order_relaxed);
    uint64_t mqtt  = atomic_exchange_explicit(&s->mqtt_messages_published, 0, memory_order_relaxed);
    uint64_t perr  = atomic_exchange_explicit(&s->parse_errors,           0, memory_order_relaxed);
    uint64_t merr  = atomic_exchange_explicit(&s->mqtt_publish_errors,    0, memory_order_relaxed);
    uint64_t drops = atomic_exchange_explicit(&s->channel_drops,          0, memory_order_relaxed);

    printf("[STATS] UDP: %.0f pps, %.2f Mbps | MQTT: %.0f msg/s | "
           "errors: parse=%lu mqtt=%lu drops=%lu\n",
           pkts / elapsed_secs,
           (bytes * 8.0) / (elapsed_secs * 1e6),
           mqtt / elapsed_secs,
           (unsigned long)perr,
           (unsigned long)merr,
           (unsigned long)drops);
    fflush(stdout);
}

#endif /* STATS_H */
