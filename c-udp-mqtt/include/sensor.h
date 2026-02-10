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
 *
 * Data types:
 *   1 = 16-bit LE PCM audio  (for audio RMS VAD)
 *   2 = 10×f32 LE sensor vector (for emotional VAD: Valence-Arousal-Dominance)
 */

#define SENSOR_HEADER_SIZE 32
#define SENSOR_MAX_PAYLOAD 4096

#define DATA_TYPE_AUDIO          1
#define DATA_TYPE_SENSOR_VECTOR  2

#define SENSOR_VECTOR_LEN    10
#define SENSOR_VECTOR_BYTES  (SENSOR_VECTOR_LEN * sizeof(float))  /* 40 */

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

/*
 * Environmental / social sensor vector for emotional VAD.
 *
 * 10 channels, each normalised to [0.0, 1.0].
 * Packed as 10 × float32 LE in the payload (40 bytes).
 */
typedef struct {
    float battery_low;    /* 0 = full, 1 = critical  */
    float people_count;   /* normalised person count  */
    float known_face;     /* known-face confidence    */
    float unknown_face;   /* unknown-face confidence  */
    float fall_event;     /* fall / impact intensity   */
    float lifted;         /* robot grabbed / lifted    */
    float idle_time;      /* 0 = just active, 1 = very idle */
    float sound_energy;   /* ambient sound level       */
    float voice_rate;     /* speech cadence rate        */
    float motion_energy;  /* IMU motion energy          */
} sensor_vector_t;

/*
 * Parse a sensor vector from the first 40 bytes of a payload.
 * Returns 0 on success, -1 if payload is too short.
 */
static inline int sensor_parse_vector(const uint8_t *payload, uint16_t payload_len,
                                      sensor_vector_t *out)
{
    if (payload_len < SENSOR_VECTOR_BYTES)
        return -1;

    /* On little-endian hosts (x86, ARM-LE) this is a straight copy.
     * For strict portability, decode each float from LE bytes.       */
    memcpy(out, payload, SENSOR_VECTOR_BYTES);
    return 0;
}

/*
 * Return the sensor vector as a plain float array (channel order).
 */
static inline void sensor_vector_to_array(const sensor_vector_t *sv, float out[SENSOR_VECTOR_LEN])
{
    memcpy(out, sv, SENSOR_VECTOR_BYTES);
}

#endif /* SENSOR_H */
