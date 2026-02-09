#ifndef SENSOR_H
#define SENSOR_H

#include <stdint.h>
#include <stddef.h>
#include <string.h>

/*
 * Binary wire format (32-byte fixed header + variable payload):
 *   [ sensor_id: u32 LE ][ timestamp_us: u64 LE ][ data_type: u8 ][ reserved: 3 ]
 *   [ payload_len: u16 LE ][ reserved: 2 ][ seq: u64 LE ][ padding: 4 ]
 *   [ payload: payload_len bytes ]
 *
 * For TCP: length-prefixed: [ total_len: u32 LE ][ binary_packet ]
 */

#define SENSOR_HEADER_SIZE 32
#define SENSOR_MAX_PAYLOAD 4096

typedef struct __attribute__((packed)) {
    uint32_t sensor_id;
    uint64_t timestamp_us;
    uint8_t  data_type;
    uint8_t  reserved1[3];
    uint16_t payload_len;
    uint8_t  reserved2[2];
    uint64_t seq;
    uint8_t  padding[4];
} sensor_header_t;

typedef struct {
    uint32_t sensor_id;
    uint64_t timestamp_us;
    uint8_t  data_type;
    uint64_t seq;
    uint16_t payload_len;
    uint8_t  payload[SENSOR_MAX_PAYLOAD];
} sensor_packet_t;

/*
 * Parse a binary sensor packet.
 * Returns 0 on success, -1 on error.
 */
static inline int sensor_parse_binary(const uint8_t *buf, size_t len,
                                      sensor_packet_t *out)
{
    if (len < SENSOR_HEADER_SIZE)
        return -1;

    const sensor_header_t *hdr = (const sensor_header_t *)buf;
    uint16_t plen = hdr->payload_len;
    if (plen > SENSOR_MAX_PAYLOAD)
        return -1;
    if (len < SENSOR_HEADER_SIZE + (size_t)plen)
        return -1;

    out->sensor_id    = hdr->sensor_id;
    out->timestamp_us = hdr->timestamp_us;
    out->data_type    = hdr->data_type;
    out->seq          = hdr->seq;
    out->payload_len  = plen;
    __builtin_memcpy(out->payload, buf + SENSOR_HEADER_SIZE, plen);

    return 0;
}

#endif /* SENSOR_H */
