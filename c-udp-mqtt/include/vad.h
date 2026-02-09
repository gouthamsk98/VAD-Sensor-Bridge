#ifndef VAD_H
#define VAD_H

#include <stdint.h>
#include <math.h>
#include "sensor.h"

/*
 * VAD (Voice Activity Detection) result.
 */
typedef struct {
    uint32_t sensor_id;
    uint64_t seq;
    double   energy;
    int      is_active;
} vad_result_t;

/* Energy threshold for voice activity */
#define VAD_ENERGY_THRESHOLD  30.0

/*
 * Compute VAD on a sensor packet's payload.
 *
 * Treats payload as 16-bit LE PCM samples, computes RMS energy,
 * compares against threshold. This is a realistic, non-trivial
 * computation for benchmarking.
 */
static inline vad_result_t vad_compute(const sensor_packet_t *pkt)
{
    vad_result_t result;
    result.sensor_id = pkt->sensor_id;
    result.seq       = pkt->seq;

    /* Compute RMS energy of 16-bit LE PCM samples */
    size_t n_samples = pkt->payload_len / 2;
    double sum_sq = 0.0;

    if (n_samples > 0) {
        for (size_t i = 0; i < n_samples; i++) {
            int16_t sample = (int16_t)(pkt->payload[i * 2] |
                                       (pkt->payload[i * 2 + 1] << 8));
            double s = (double)sample;
            sum_sq += s * s;
        }
        result.energy = sqrt(sum_sq / (double)n_samples);
    } else {
        result.energy = 0.0;
    }

    result.is_active = (result.energy > VAD_ENERGY_THRESHOLD) ? 1 : 0;
    return result;
}

#endif /* VAD_H */
