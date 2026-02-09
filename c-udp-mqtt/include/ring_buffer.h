/**
 * Lock-free MPMC (multi-producer, multi-consumer) ring buffer.
 * CAS-based head (producers) and CAS-based tail (consumers).
 * Per-slot ready flag for safe handoff.
 */

#ifndef RING_BUFFER_H
#define RING_BUFFER_H

#include <stdint.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>

#define RING_SLOT_SIZE    512
#define RING_BATCH_MAX    256
#define CACHE_LINE_SIZE    64

typedef struct {
    _Atomic uint32_t ready;
    uint16_t         len;       /* total data length */
    uint16_t         _pad;
    uint8_t          data[RING_SLOT_SIZE - 8];
} ring_slot_t;

typedef struct {
    _Atomic uint64_t head __attribute__((aligned(CACHE_LINE_SIZE)));
    _Atomic uint64_t tail __attribute__((aligned(CACHE_LINE_SIZE)));
    uint64_t         mask;
    uint64_t         capacity;
    ring_slot_t     *slots;
} ring_buffer_t;

static inline int ring_init(ring_buffer_t *rb, uint64_t capacity) {
    uint64_t cap = 1;
    while (cap < capacity) cap <<= 1;

    rb->capacity = cap;
    rb->mask     = cap - 1;
    atomic_store(&rb->head, 0);
    atomic_store(&rb->tail, 0);

    rb->slots = (ring_slot_t *)calloc(cap, sizeof(ring_slot_t));
    if (!rb->slots) return -1;

    for (uint64_t i = 0; i < cap; i++)
        atomic_store_explicit(&rb->slots[i].ready, 0, memory_order_relaxed);

    return 0;
}

static inline void ring_destroy(ring_buffer_t *rb) {
    free(rb->slots);
    rb->slots = NULL;
}

/* Push raw data (multi-producer safe). Returns 0 on success, -1 if full. */
static inline int ring_try_push(ring_buffer_t *rb,
                                const void *data, uint16_t len)
{
    if (len > sizeof(((ring_slot_t *)0)->data))
        return -1;

    uint64_t h, next;
    for (;;) {
        h = atomic_load_explicit(&rb->head, memory_order_relaxed);
        uint64_t t = atomic_load_explicit(&rb->tail, memory_order_relaxed);
        if (h - t >= rb->capacity)
            return -1;

        next = h + 1;
        if (atomic_compare_exchange_weak_explicit(&rb->head, &h, next,
                                                   memory_order_acq_rel,
                                                   memory_order_relaxed))
            break;
    }

    ring_slot_t *slot = &rb->slots[h & rb->mask];
    slot->len = len;
    memcpy(slot->data, data, len);
    atomic_store_explicit(&slot->ready, 1, memory_order_release);
    return 0;
}

/* Pop raw data (multi-consumer safe). Returns 0 on success, -1 if empty. */
static inline int ring_try_pop(ring_buffer_t *rb, void *out, uint16_t *out_len) {
    uint64_t t, h;
    for (;;) {
        t = atomic_load_explicit(&rb->tail, memory_order_relaxed);
        h = atomic_load_explicit(&rb->head, memory_order_acquire);

        if (t >= h)
            return -1;

        ring_slot_t *slot = &rb->slots[t & rb->mask];
        if (atomic_load_explicit(&slot->ready, memory_order_acquire) == 0)
            return -1;

        if (atomic_compare_exchange_weak_explicit(&rb->tail, &t, t + 1,
                                                   memory_order_acq_rel,
                                                   memory_order_relaxed)) {
            *out_len = slot->len;
            memcpy(out, slot->data, slot->len);
            atomic_store_explicit(&slot->ready, 0, memory_order_release);
            return 0;
        }
    }
}

static inline uint64_t ring_size(ring_buffer_t *rb) {
    uint64_t h = atomic_load_explicit(&rb->head, memory_order_acquire);
    uint64_t t = atomic_load_explicit(&rb->tail, memory_order_acquire);
    return h - t;
}

#endif /* RING_BUFFER_H */
