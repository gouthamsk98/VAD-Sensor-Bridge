#ifndef RING_BUFFER_H
#define RING_BUFFER_H

/*
 * Lock-free SPMC (single-producer, single-consumer) ring buffer.
 * For multi-producer we use per-thread ring buffers that a single
 * MQTT publisher thread drains.
 *
 * This is a power-of-2 sized ring with atomic head/tail for zero-copy
 * message passing between UDP recv threads and MQTT publish thread.
 */

#include <stdint.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>

#define RING_SLOT_SIZE 4096   /* max message size per slot */

typedef struct {
    uint16_t topic_len;
    uint16_t payload_len;
    char     data[RING_SLOT_SIZE - 4]; /* topic + payload packed */
} ring_slot_t;

typedef struct {
    _Atomic uint64_t head;  /* written by producer */
    _Atomic uint64_t tail;  /* written by consumer */
    uint64_t         mask;
    uint64_t         capacity;
    ring_slot_t     *slots;
} ring_buffer_t;

static inline int ring_init(ring_buffer_t *rb, uint64_t capacity) {
    /* Round up to power of 2 */
    uint64_t cap = 1;
    while (cap < capacity) cap <<= 1;

    rb->capacity = cap;
    rb->mask = cap - 1;
    atomic_store(&rb->head, 0);
    atomic_store(&rb->tail, 0);
    rb->slots = (ring_slot_t *)calloc(cap, sizeof(ring_slot_t));
    return rb->slots ? 0 : -1;
}

static inline void ring_destroy(ring_buffer_t *rb) {
    free(rb->slots);
    rb->slots = NULL;
}

/*
 * Try to enqueue a message. Returns 0 on success, -1 if full.
 * Called from UDP receiver thread.
 */
static inline int ring_try_push(ring_buffer_t *rb,
                                const char *topic, uint16_t topic_len,
                                const char *payload, uint16_t payload_len)
{
    uint64_t h = atomic_load_explicit(&rb->head, memory_order_relaxed);
    uint64_t t = atomic_load_explicit(&rb->tail, memory_order_acquire);

    if (h - t >= rb->capacity)
        return -1; /* full */

    ring_slot_t *slot = &rb->slots[h & rb->mask];
    slot->topic_len   = topic_len;
    slot->payload_len = payload_len;
    memcpy(slot->data, topic, topic_len);
    memcpy(slot->data + topic_len, payload, payload_len);

    atomic_store_explicit(&rb->head, h + 1, memory_order_release);
    return 0;
}

/*
 * Try to dequeue a message. Returns 0 on success, -1 if empty.
 * Called from MQTT publisher thread.
 */
static inline int ring_try_pop(ring_buffer_t *rb, ring_slot_t *out) {
    uint64_t t = atomic_load_explicit(&rb->tail, memory_order_relaxed);
    uint64_t h = atomic_load_explicit(&rb->head, memory_order_acquire);

    if (t >= h)
        return -1; /* empty */

    const ring_slot_t *slot = &rb->slots[t & rb->mask];
    memcpy(out, slot, sizeof(ring_slot_t));

    atomic_store_explicit(&rb->tail, t + 1, memory_order_release);
    return 0;
}

static inline uint64_t ring_size(ring_buffer_t *rb) {
    uint64_t h = atomic_load_explicit(&rb->head, memory_order_acquire);
    uint64_t t = atomic_load_explicit(&rb->tail, memory_order_acquire);
    return h - t;
}

#endif /* RING_BUFFER_H */
