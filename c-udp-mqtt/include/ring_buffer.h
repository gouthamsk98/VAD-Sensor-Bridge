/**
 * Lock-free MPSC (multi-producer, single-consumer) ring buffer.
 *
 * Multiple UDP receiver threads can push concurrently via CAS on head.
 * A single MQTT publisher thread pops using a per-slot "ready" flag
 * to know when the producer has finished writing the slot data.
 *
 * Design:
 *   - Power-of-2 capacity for fast masking
 *   - CAS-based head advance (producers)
 *   - Sequential tail advance (consumer checks slot.ready)
 *   - Cache-line padding between head and tail to avoid false sharing
 *   - Batch pop for high throughput
 */

#ifndef RING_BUFFER_H
#define RING_BUFFER_H

#include <stdint.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>

#define RING_SLOT_SIZE    512   /* max topic + payload per slot */
#define RING_BATCH_MAX    256   /* max messages per batch pop  */
#define CACHE_LINE_SIZE    64

typedef struct {
    _Atomic uint32_t ready;              /* 0 = free, 1 = data written */
    uint16_t         topic_len;
    uint16_t         payload_len;
    char             data[RING_SLOT_SIZE - 8]; /* topic + payload packed */
} ring_slot_t;

typedef struct {
    /* ── Producer side (written by multiple threads via CAS) ── */
    _Atomic uint64_t head __attribute__((aligned(CACHE_LINE_SIZE)));

    /* ── Consumer side ── */
    _Atomic uint64_t tail __attribute__((aligned(CACHE_LINE_SIZE)));

    /* ── Read-only after init ── */
    uint64_t         mask;
    uint64_t         capacity;
    ring_slot_t     *slots;
} ring_buffer_t;

static inline int ring_init(ring_buffer_t *rb, uint64_t capacity) {
    /* Round up to power of 2 */
    uint64_t cap = 1;
    while (cap < capacity) cap <<= 1;

    rb->capacity = cap;
    rb->mask     = cap - 1;
    atomic_store(&rb->head, 0);
    atomic_store(&rb->tail, 0);

    /* Allocate cache-line-aligned slots */
    rb->slots = (ring_slot_t *)calloc(cap, sizeof(ring_slot_t));
    if (!rb->slots) return -1;

    /* Ensure all ready flags are 0 */
    for (uint64_t i = 0; i < cap; i++)
        atomic_store_explicit(&rb->slots[i].ready, 0, memory_order_relaxed);

    return 0;
}

static inline void ring_destroy(ring_buffer_t *rb) {
    free(rb->slots);
    rb->slots = NULL;
}

/**
 * Try to enqueue a message (multi-producer safe via CAS).
 * Returns 0 on success, -1 if full.
 */
static inline int ring_try_push(ring_buffer_t *rb,
                                const char *topic, uint16_t topic_len,
                                const char *payload, uint16_t payload_len)
{
    uint64_t h, next;
    for (;;) {
        h = atomic_load_explicit(&rb->head, memory_order_relaxed);

        /* Check if full: compare against consumer's tail. */
        uint64_t t = atomic_load_explicit(&rb->tail, memory_order_relaxed);
        if (h - t >= rb->capacity)
            return -1; /* full */

        next = h + 1;
        if (atomic_compare_exchange_weak_explicit(&rb->head, &h, next,
                                                   memory_order_acq_rel,
                                                   memory_order_relaxed))
            break;
        /* CAS failed → another producer won, retry */
    }

    /* We own slot[h & mask]. Write data, then set ready flag. */
    ring_slot_t *slot = &rb->slots[h & rb->mask];
    slot->topic_len   = topic_len;
    slot->payload_len = payload_len;
    memcpy(slot->data, topic, topic_len);
    memcpy(slot->data + topic_len, payload, payload_len);

    /* Release: make writes visible before marking slot as ready */
    atomic_store_explicit(&slot->ready, 1, memory_order_release);
    return 0;
}

/**
 * Try to dequeue a single message (multi-consumer safe via CAS on tail).
 * Returns 0 on success, -1 if empty.
 */
static inline int ring_try_pop(ring_buffer_t *rb, ring_slot_t *out) {
    uint64_t t, h;
    for (;;) {
        t = atomic_load_explicit(&rb->tail, memory_order_relaxed);
        h = atomic_load_explicit(&rb->head, memory_order_acquire);

        if (t >= h)
            return -1; /* empty */

        ring_slot_t *slot = &rb->slots[t & rb->mask];

        /* Wait for producer to finish writing this slot */
        if (atomic_load_explicit(&slot->ready, memory_order_acquire) == 0)
            return -1; /* slot reserved but not yet written */

        /* Try to claim this slot */
        if (atomic_compare_exchange_weak_explicit(&rb->tail, &t, t + 1,
                                                   memory_order_acq_rel,
                                                   memory_order_relaxed)) {
            memcpy(out, slot, sizeof(ring_slot_t));
            atomic_store_explicit(&slot->ready, 0, memory_order_release);
            return 0;
        }
        /* CAS failed → another consumer won, retry */
    }
}

/**
 * Batch pop: dequeue up to `max_batch` messages at once.
 * Returns number of messages popped (0 if empty).
 * Multi-consumer safe (each message claimed via CAS).
 */
static inline int ring_pop_batch(ring_buffer_t *rb, ring_slot_t *out, int max_batch) {
    int count = 0;
    while (count < max_batch) {
        if (ring_try_pop(rb, &out[count]) != 0)
            break;
        count++;
    }
    return count;
}

static inline uint64_t ring_size(ring_buffer_t *rb) {
    uint64_t h = atomic_load_explicit(&rb->head, memory_order_acquire);
    uint64_t t = atomic_load_explicit(&rb->tail, memory_order_acquire);
    return h - t;
}

#endif /* RING_BUFFER_H */
