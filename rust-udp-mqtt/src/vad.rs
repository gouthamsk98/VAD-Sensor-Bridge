use crate::sensor::{ SensorPacket, SensorVector, DATA_TYPE_AUDIO, DATA_TYPE_SENSOR_VECTOR };

// ─────────────────────────────────────────────────────────────────────
//  Unified VAD result — can originate from audio OR emotional pipeline
// ─────────────────────────────────────────────────────────────────────

/// The kind of VAD computation that produced the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadKind {
    /// Energy-based audio voice-activity detection
    Audio,
    /// Weighted-sensor emotional Valence-Arousal-Dominance
    Emotional,
}

/// Unified result returned by [`process_packet`].
#[derive(Debug, Clone)]
pub struct VadResult {
    pub sensor_id: u32,
    pub seq: u64,
    pub kind: VadKind,
    /// True when any form of "activity" was detected.
    /// • Audio mode  → RMS energy above threshold
    /// • Emotional   → arousal above threshold
    pub is_active: bool,
    /// Audio-only: RMS energy value (0.0 for emotional mode)
    pub energy: f64,
    /// Audio-only: energy threshold used
    pub threshold: f64,
    /// Emotional-only: Valence–Arousal–Dominance triple (all 0 for audio mode)
    pub valence: f32,
    pub arousal: f32,
    pub dominance: f32,
}

// ─────────────────────────────────────────────────────────────────────
//  Top-level dispatcher — routes on data_type
// ─────────────────────────────────────────────────────────────────────

/// Process a sensor packet through the appropriate VAD pipeline.
///
/// * `data_type == 1` → audio RMS energy VAD
/// * `data_type == 2` → emotional Valence-Arousal-Dominance VAD
/// * anything else    → falls back to audio VAD
#[inline]
pub fn process_packet(packet: &SensorPacket) -> VadResult {
    match packet.data_type {
        DATA_TYPE_SENSOR_VECTOR => compute_emotional_vad(packet),
        DATA_TYPE_AUDIO | _ => compute_audio_vad(packet),
    }
}

// ═════════════════════════════════════════════════════════════════════
//  1.  Audio VAD  (original energy-based detector)
// ═════════════════════════════════════════════════════════════════════

/// Energy threshold for voice activity detection.
const VAD_ENERGY_THRESHOLD: f64 = 30.0;

/// Audio RMS energy VAD — treats payload as 16-bit LE PCM samples.
#[inline]
fn compute_audio_vad(packet: &SensorPacket) -> VadResult {
    let energy = compute_rms_energy(&packet.payload);
    let is_active = energy > VAD_ENERGY_THRESHOLD;

    VadResult {
        sensor_id: packet.sensor_id,
        seq: packet.seq,
        kind: VadKind::Audio,
        is_active,
        energy,
        threshold: VAD_ENERGY_THRESHOLD,
        valence: 0.0,
        arousal: 0.0,
        dominance: 0.0,
    }
}

/// Compute RMS energy of a byte buffer interpreted as 16-bit LE PCM samples.
#[inline]
fn compute_rms_energy(data: &[u8]) -> f64 {
    if data.len() < 2 {
        return 0.0;
    }

    let n_samples = data.len() / 2;
    let mut sum_sq: f64 = 0.0;

    for i in 0..n_samples {
        let sample = i16::from_le_bytes([data[i * 2], data[i * 2 + 1]]) as f64;
        sum_sq += sample * sample;
    }

    (sum_sq / (n_samples as f64)).sqrt()
}

// ═════════════════════════════════════════════════════════════════════
//  2.  Emotional VAD  (Valence – Arousal – Dominance)
// ═════════════════════════════════════════════════════════════════════
//
//  Maps a 10-channel environmental sensor vector to a V/A/D triple
//  using fixed linear weight vectors with bias, clamped to [0, 1].
//
//  Sensor channel order (all normalised 0–1):
//    0  battery_low     5  lifted
//    1  people_count    6  idle_time
//    2  known_face      7  sound_energy
//    3  unknown_face    8  voice_rate
//    4  fall_event      9  motion_energy
//
//  Weight design rationale
//  ───────────────────────
//  Valence   – positive emotions (known faces, company, conversation)
//              minus threats (unknown faces, falls, being grabbed)
//  Arousal   – overall activation level (motion, sound, events)
//              minus passivity (idle time)
//  Dominance – sense of control & familiarity
//              minus vulnerability (threats, low battery, being grabbed)

/// Arousal threshold above which `is_active` is set for emotional VAD.
const EMOTIONAL_ACTIVE_THRESHOLD: f32 = 0.35;

//                                bat   ppl   kno   unk   fal   lft   idl   snd   voi   mot   bias
const VALENCE_W: [f32; 11] = [-0.05, 0.15, 0.3, -0.2, -0.2, -0.15, -0.1, 0.05, 0.15, 0.0, 0.3];
const AROUSAL_W: [f32; 11] = [0.0, 0.1, 0.0, 0.1, 0.2, 0.15, -0.25, 0.25, 0.1, 0.25, 0.1];
const DOMINANCE_W: [f32; 11] = [
    -0.15, 0.1, 0.25, -0.2, -0.15, -0.15, -0.05, 0.05, 0.15, 0.05, 0.35,
];

/// Compute emotional VAD from a sensor-vector payload.
///
/// Falls back to a zero result if the payload is too short.
#[inline]
fn compute_emotional_vad(packet: &SensorPacket) -> VadResult {
    let sv = SensorVector::from_payload(&packet.payload);

    let (valence, arousal, dominance) = match sv {
        Some(v) => {
            let s = v.as_array();
            (
                weighted_sum(&s, &VALENCE_W),
                weighted_sum(&s, &AROUSAL_W),
                weighted_sum(&s, &DOMINANCE_W),
            )
        }
        None => (0.0, 0.0, 0.0),
    };

    VadResult {
        sensor_id: packet.sensor_id,
        seq: packet.seq,
        kind: VadKind::Emotional,
        is_active: arousal > EMOTIONAL_ACTIVE_THRESHOLD,
        energy: 0.0,
        threshold: 0.0,
        valence,
        arousal,
        dominance,
    }
}

/// Dot-product of a 10-element sensor array with an 11-element weight
/// vector (last element = bias), clamped to [0.0, 1.0].
#[inline]
fn weighted_sum(sensors: &[f32; 10], weights: &[f32; 11]) -> f32 {
    let mut sum = weights[10]; // bias
    for i in 0..10 {
        sum += sensors[i] * weights[i];
    }
    sum.clamp(0.0, 1.0)
}

// ═════════════════════════════════════════════════════════════════════
//  Tests
// ═════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensor::SENSOR_VECTOR_BYTES;

    // ── Audio VAD tests ──────────────────────────────────────────────

    #[test]
    fn test_silence_detection() {
        let packet = SensorPacket {
            sensor_id: 1,
            timestamp_us: 0,
            data_type: DATA_TYPE_AUDIO,
            seq: 0,
            payload: vec![0u8; 64],
        };
        let result = process_packet(&packet);
        assert_eq!(result.kind, VadKind::Audio);
        assert!(!result.is_active);
        assert_eq!(result.energy, 0.0);
    }

    #[test]
    fn test_loud_signal() {
        let packet = SensorPacket {
            sensor_id: 1,
            timestamp_us: 0,
            data_type: DATA_TYPE_AUDIO,
            seq: 0,
            payload: vec![0xff, 0x7f, 0xff, 0x7f, 0xff, 0x7f, 0xff, 0x7f],
        };
        let result = process_packet(&packet);
        assert_eq!(result.kind, VadKind::Audio);
        assert!(result.is_active);
        assert!(result.energy > VAD_ENERGY_THRESHOLD);
    }

    // ── Emotional VAD tests ──────────────────────────────────────────

    /// Helper: build a SensorPacket with data_type=2 from a float slice.
    fn sensor_packet_from_floats(vals: &[f32; 10]) -> SensorPacket {
        let mut payload = Vec::with_capacity(SENSOR_VECTOR_BYTES);
        for v in vals {
            payload.extend_from_slice(&v.to_le_bytes());
        }
        SensorPacket {
            sensor_id: 42,
            timestamp_us: 0,
            data_type: DATA_TYPE_SENSOR_VECTOR,
            seq: 1,
            payload,
        }
    }

    #[test]
    fn test_happy_scenario() {
        // HAPPY: social, known faces, conversation
        let pkt = sensor_packet_from_floats(
            &[
                0.1, // battery_low
                0.85, // people_count
                0.95, // known_face
                0.05, // unknown_face
                0.0, // fall_event
                0.0, // lifted
                0.15, // idle_time
                0.45, // sound_energy
                0.75, // voice_rate
                0.35, // motion_energy
            ]
        );
        let r = process_packet(&pkt);
        assert_eq!(r.kind, VadKind::Emotional);
        // Valence should be high (happy = positive emotion)
        assert!(r.valence > 0.65, "valence={:.3} expected > 0.65", r.valence);
        // Arousal should be moderate
        assert!(
            r.arousal > 0.25 && r.arousal < 0.65,
            "arousal={:.3} expected 0.25..0.65",
            r.arousal
        );
        // Dominance should be solid
        assert!(r.dominance > 0.55, "dominance={:.3} expected > 0.55", r.dominance);
    }

    #[test]
    fn test_sad_bored_scenario() {
        // SAD / BORED: alone, idle, quiet
        let pkt = sensor_packet_from_floats(
            &[
                0.3, // battery_low
                0.0, // people_count
                0.0, // known_face
                0.0, // unknown_face
                0.0, // fall_event
                0.0, // lifted
                0.95, // idle_time
                0.05, // sound_energy
                0.0, // voice_rate
                0.05, // motion_energy
            ]
        );
        let r = process_packet(&pkt);
        assert_eq!(r.kind, VadKind::Emotional);
        // Valence should be low
        assert!(r.valence < 0.3, "valence={:.3} expected < 0.30", r.valence);
        // Arousal should be very low
        assert!(r.arousal < 0.2, "arousal={:.3} expected < 0.20", r.arousal);
        // Should NOT be active (low arousal)
        assert!(!r.is_active, "expected inactive for sad/bored");
    }

    #[test]
    fn test_angry_fear_scenario() {
        // ANGRY / FEAR: fall, grabbed, unknown faces
        let pkt = sensor_packet_from_floats(
            &[
                0.25, // battery_low
                0.35, // people_count
                0.0, // known_face
                0.75, // unknown_face
                0.85, // fall_event
                0.65, // lifted
                0.05, // idle_time
                0.75, // sound_energy
                0.0, // voice_rate
                0.85, // motion_energy
            ]
        );
        let r = process_packet(&pkt);
        assert_eq!(r.kind, VadKind::Emotional);
        // Valence should be very low (negative emotion)
        assert!(r.valence < 0.2, "valence={:.3} expected < 0.20", r.valence);
        // Arousal should be very high (distress)
        assert!(r.arousal > 0.7, "arousal={:.3} expected > 0.70", r.arousal);
        // Dominance should be low (loss of control)
        assert!(r.dominance < 0.3, "dominance={:.3} expected < 0.30", r.dominance);
        // Should be active (high arousal)
        assert!(r.is_active, "expected active for angry/fear");
    }

    #[test]
    fn test_tired_scenario() {
        // TIRED: low battery, idle
        let pkt = sensor_packet_from_floats(
            &[
                0.95, // battery_low
                0.05, // people_count
                0.1, // known_face
                0.0, // unknown_face
                0.0, // fall_event
                0.0, // lifted
                0.75, // idle_time
                0.05, // sound_energy
                0.05, // voice_rate
                0.05, // motion_energy
            ]
        );
        let r = process_packet(&pkt);
        assert_eq!(r.kind, VadKind::Emotional);
        // Valence should be low-ish
        assert!(r.valence < 0.35, "valence={:.3} expected < 0.35", r.valence);
        // Arousal should be very low
        assert!(r.arousal < 0.2, "arousal={:.3} expected < 0.20", r.arousal);
        // Should NOT be active
        assert!(!r.is_active, "expected inactive for tired");
    }

    #[test]
    fn test_excited_scenario() {
        // EXCITED: high activity, crowded environment
        let pkt = sensor_packet_from_floats(
            &[
                0.15, // battery_low
                0.95, // people_count
                0.65, // known_face
                0.35, // unknown_face
                0.0, // fall_event
                0.0, // lifted
                0.0, // idle_time
                0.95, // sound_energy
                0.85, // voice_rate
                0.95, // motion_energy
            ]
        );
        let r = process_packet(&pkt);
        assert_eq!(r.kind, VadKind::Emotional);
        // Valence should be moderately high
        assert!(r.valence > 0.55, "valence={:.3} expected > 0.55", r.valence);
        // Arousal should be very high
        assert!(r.arousal > 0.7, "arousal={:.3} expected > 0.70", r.arousal);
        // Dominance should be moderate-high
        assert!(r.dominance > 0.45, "dominance={:.3} expected > 0.45", r.dominance);
        // Should be active (high arousal)
        assert!(r.is_active, "expected active for excited");
    }

    #[test]
    fn test_short_payload_returns_zeros() {
        let pkt = SensorPacket {
            sensor_id: 1,
            timestamp_us: 0,
            data_type: DATA_TYPE_SENSOR_VECTOR,
            seq: 0,
            payload: vec![0u8; 8], // too short for 40-byte sensor vector
        };
        let r = process_packet(&pkt);
        assert_eq!(r.kind, VadKind::Emotional);
        assert_eq!(r.valence, 0.0);
        assert_eq!(r.arousal, 0.0);
        assert_eq!(r.dominance, 0.0);
    }
}
