#ifndef VAD_H
#define VAD_H

#include <stdint.h>
#include <math.h>
#include "sensor.h"

/* ─────────────────────────────────────────────────────────────────────
 *  Unified VAD result
 * ───────────────────────────────────────────────────────────────────── */

typedef enum {
    VAD_KIND_AUDIO     = 0,
    VAD_KIND_EMOTIONAL = 1,
} vad_kind_t;

typedef struct {
    uint32_t   sensor_id;
    uint64_t   seq;
    vad_kind_t kind;
    int        is_active;
    /* Audio-only */
    double     energy;
    double     threshold;
    /* Emotional-only */
    float      valence;
    float      arousal;
    float      dominance;
} vad_result_t;

/* ═════════════════════════════════════════════════════════════════════
 *  1.  Audio VAD  (original RMS energy detector)
 * ═════════════════════════════════════════════════════════════════════ */

/* Energy threshold for voice activity */
#define VAD_ENERGY_THRESHOLD  30.0

static inline vad_result_t vad_compute_audio(const sensor_packet_t *pkt)
{
    vad_result_t result;
    memset(&result, 0, sizeof(result));
    result.sensor_id = pkt->sensor_id;
    result.seq       = pkt->seq;
    result.kind      = VAD_KIND_AUDIO;
    result.threshold = VAD_ENERGY_THRESHOLD;

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

/* ═════════════════════════════════════════════════════════════════════
 *  2.  Emotional VAD  (Valence – Arousal – Dominance)
 * ═════════════════════════════════════════════════════════════════════
 *
 *  Maps a 10-channel environmental sensor vector to a V/A/D triple
 *  using fixed linear weight vectors with bias, clamped to [0, 1].
 *
 *  Sensor channel order (all normalised 0–1):
 *    0  battery_low     5  lifted
 *    1  people_count    6  idle_time
 *    2  known_face      7  sound_energy
 *    3  unknown_face    8  voice_rate
 *    4  fall_event      9  motion_energy
 *
 *  Weight design rationale
 *  ───────────────────────
 *  Valence   – positive emotions (known faces, company, conversation)
 *              minus threats (unknown faces, falls, being grabbed)
 *  Arousal   – overall activation level (motion, sound, events)
 *              minus passivity (idle time)
 *  Dominance – sense of control & familiarity
 *              minus vulnerability (threats, low battery, being grabbed)
 */

#define EMOTIONAL_ACTIVE_THRESHOLD  0.35f

/*                              bat    ppl    kno    unk    fal    lft    idl    snd    voi    mot   bias */
static const float VALENCE_W[11]   = {-0.05f, 0.15f, 0.30f,-0.20f,-0.20f,-0.15f,-0.10f, 0.05f, 0.15f, 0.00f, 0.30f};
static const float AROUSAL_W[11]   = { 0.00f, 0.10f, 0.00f, 0.10f, 0.20f, 0.15f,-0.25f, 0.25f, 0.10f, 0.25f, 0.10f};
static const float DOMINANCE_W[11] = {-0.15f, 0.10f, 0.25f,-0.20f,-0.15f,-0.15f,-0.05f, 0.05f, 0.15f, 0.05f, 0.35f};

static inline float vad_clampf(float v, float lo, float hi)
{
    if (v < lo) return lo;
    if (v > hi) return hi;
    return v;
}

static inline float vad_weighted_sum(const float sensors[SENSOR_VECTOR_LEN],
                                     const float weights[11])
{
    float sum = weights[10]; /* bias */
    for (int i = 0; i < SENSOR_VECTOR_LEN; i++)
        sum += sensors[i] * weights[i];
    return vad_clampf(sum, 0.0f, 1.0f);
}

static inline vad_result_t vad_compute_emotional(const sensor_packet_t *pkt)
{
    vad_result_t result;
    memset(&result, 0, sizeof(result));
    result.sensor_id = pkt->sensor_id;
    result.seq       = pkt->seq;
    result.kind      = VAD_KIND_EMOTIONAL;

    sensor_vector_t sv;
    if (sensor_parse_vector(pkt->payload, pkt->payload_len, &sv) == 0) {
        float s[SENSOR_VECTOR_LEN];
        sensor_vector_to_array(&sv, s);

        result.valence   = vad_weighted_sum(s, VALENCE_W);
        result.arousal   = vad_weighted_sum(s, AROUSAL_W);
        result.dominance = vad_weighted_sum(s, DOMINANCE_W);
    }
    /* else: all fields already zeroed by memset */

    result.is_active = (result.arousal > EMOTIONAL_ACTIVE_THRESHOLD) ? 1 : 0;
    return result;
}

/* ═════════════════════════════════════════════════════════════════════
 *  Top-level dispatcher — routes on data_type
 * ═════════════════════════════════════════════════════════════════════ */

/*
 * Process a sensor packet through the appropriate VAD pipeline.
 *
 *   data_type == 1  → audio RMS energy VAD
 *   data_type == 2  → emotional Valence-Arousal-Dominance VAD
 *   anything else   → falls back to audio VAD
 */
static inline vad_result_t vad_process(const sensor_packet_t *pkt)
{
    if (pkt->data_type == DATA_TYPE_SENSOR_VECTOR)
        return vad_compute_emotional(pkt);
    return vad_compute_audio(pkt);
}

#endif /* VAD_H */
