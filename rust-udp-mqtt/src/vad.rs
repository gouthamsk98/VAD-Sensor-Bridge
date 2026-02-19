use crate::persona::{ PersonaTrait, apply_deltas, persona_weight_deltas };
use crate::sensor::{ SensorPacket, SensorVector, DATA_TYPE_AUDIO, DATA_TYPE_SENSOR_VECTOR };
use crate::sensor_smoother::SensorSmoother;

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
///
/// The `persona` trait applies additive weight deltas to the emotional
/// VAD weights, shaping the robot's emotional response profile.
///
/// The `smoother` applies EMA decay to the idle_time channel so the
/// robot drifts into sadness gradually rather than instantly.
#[inline]
pub fn process_packet(
    packet: &SensorPacket,
    persona: PersonaTrait,
    smoother: &SensorSmoother
) -> VadResult {
    match packet.data_type {
        DATA_TYPE_SENSOR_VECTOR => compute_emotional_vad(packet, persona, smoother),
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
/// The active `persona` trait applies additive deltas to the base
/// V/A/D weight vectors before the dot-product computation.
///
/// Falls back to a zero result if the payload is too short.
#[inline]
fn compute_emotional_vad(
    packet: &SensorPacket,
    persona: PersonaTrait,
    smoother: &SensorSmoother
) -> VadResult {
    let sv = SensorVector::from_payload(&packet.payload);

    // Apply persona-specific weight deltas
    let deltas = persona_weight_deltas(persona);
    let val_w = apply_deltas(&VALENCE_W, &deltas.valence);
    let aro_w = apply_deltas(&AROUSAL_W, &deltas.arousal);
    let dom_w = apply_deltas(&DOMINANCE_W, &deltas.dominance);

    let (valence, arousal, dominance) = match sv {
        Some(v) => {
            let mut s = v.as_array();
            // Smooth idle_time via EMA so sadness ramps gradually
            smoother.smooth(packet.sensor_id, &mut s, persona);
            (weighted_sum(&s, &val_w), weighted_sum(&s, &aro_w), weighted_sum(&s, &dom_w))
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
    use crate::persona::PersonaTrait;
    use crate::sensor::SENSOR_VECTOR_BYTES;
    use crate::sensor_smoother::SensorSmoother;

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
        let smoother = SensorSmoother::new();
        let result = process_packet(&packet, PersonaTrait::Obedient, &smoother);
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
        let smoother = SensorSmoother::new();
        let result = process_packet(&packet, PersonaTrait::Obedient, &smoother);
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

    /// Helper: feed `n` identical packets through the smoother so the EMA
    /// reaches near-steady-state before the assertion packet.
    fn warm_smoother(smoother: &SensorSmoother, vals: &[f32; 10], n: usize, persona: PersonaTrait) {
        for _ in 0..n {
            let pkt = sensor_packet_from_floats(vals);
            let _ = process_packet(&pkt, persona, smoother);
        }
    }

    #[test]
    fn test_happy_scenario() {
        // HAPPY: social, known faces, conversation  (idle low → smoother barely matters)
        let smoother = SensorSmoother::new();
        let vals = [0.1, 0.85, 0.95, 0.05, 0.0, 0.0, 0.15, 0.45, 0.75, 0.35];
        warm_smoother(&smoother, &vals, 50, PersonaTrait::Obedient);
        let pkt = sensor_packet_from_floats(&vals);
        let r = process_packet(&pkt, PersonaTrait::Obedient, &smoother);
        assert_eq!(r.kind, VadKind::Emotional);
        assert!(r.valence > 0.65, "valence={:.3} expected > 0.65", r.valence);
        assert!(
            r.arousal > 0.25 && r.arousal < 0.65,
            "arousal={:.3} expected 0.25..0.65",
            r.arousal
        );
        assert!(r.dominance > 0.55, "dominance={:.3} expected > 0.55", r.dominance);
    }

    #[test]
    fn test_sad_bored_scenario_converged() {
        // SAD / BORED after many idle packets (EMA has converged)
        let smoother = SensorSmoother::new();
        let vals = [0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.95, 0.05, 0.0, 0.05];
        warm_smoother(&smoother, &vals, 200, PersonaTrait::Obedient);
        let pkt = sensor_packet_from_floats(&vals);
        let r = process_packet(&pkt, PersonaTrait::Obedient, &smoother);
        assert_eq!(r.kind, VadKind::Emotional);
        assert!(r.valence < 0.3, "valence={:.3} expected < 0.30", r.valence);
        assert!(r.arousal < 0.2, "arousal={:.3} expected < 0.20", r.arousal);
        assert!(!r.is_active, "expected inactive for sad/bored after convergence");
    }

    #[test]
    fn test_sad_bored_first_packet_is_not_sad() {
        // First idle packet should NOT produce full sadness thanks to EMA
        let smoother = SensorSmoother::new();
        let vals = [0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.95, 0.05, 0.0, 0.05];
        let pkt = sensor_packet_from_floats(&vals);
        let r = process_packet(&pkt, PersonaTrait::Obedient, &smoother);
        // With fresh smoother, idle_time is heavily damped → arousal should be near baseline
        // not deeply negative.  Valence should be closer to the bias (0.3) not dragged down.
        assert!(r.valence > 0.2, "valence={:.3} should be higher on first idle packet", r.valence);
    }

    #[test]
    fn test_angry_fear_scenario() {
        // ANGRY / FEAR: fall, grabbed, unknown faces (idle low → smoother barely matters)
        let smoother = SensorSmoother::new();
        let vals = [0.25, 0.35, 0.0, 0.75, 0.85, 0.65, 0.05, 0.75, 0.0, 0.85];
        warm_smoother(&smoother, &vals, 50, PersonaTrait::Obedient);
        let pkt = sensor_packet_from_floats(&vals);
        let r = process_packet(&pkt, PersonaTrait::Obedient, &smoother);
        assert_eq!(r.kind, VadKind::Emotional);
        assert!(r.valence < 0.2, "valence={:.3} expected < 0.20", r.valence);
        assert!(r.arousal > 0.55, "arousal={:.3} expected > 0.55", r.arousal);
        assert!(r.dominance < 0.3, "dominance={:.3} expected < 0.30", r.dominance);
        assert!(r.is_active, "expected active for angry/fear");
    }

    #[test]
    fn test_tired_scenario() {
        // TIRED: low battery, idle (needs many packets to converge)
        let smoother = SensorSmoother::new();
        let vals = [0.95, 0.05, 0.1, 0.0, 0.0, 0.0, 0.75, 0.05, 0.05, 0.05];
        warm_smoother(&smoother, &vals, 200, PersonaTrait::Obedient);
        let pkt = sensor_packet_from_floats(&vals);
        let r = process_packet(&pkt, PersonaTrait::Obedient, &smoother);
        assert_eq!(r.kind, VadKind::Emotional);
        assert!(r.valence < 0.35, "valence={:.3} expected < 0.35", r.valence);
        assert!(r.arousal < 0.2, "arousal={:.3} expected < 0.20", r.arousal);
        assert!(!r.is_active, "expected inactive for tired");
    }

    #[test]
    fn test_excited_scenario() {
        // EXCITED: high activity (idle=0 → smoother doesn't affect outcome)
        let smoother = SensorSmoother::new();
        let vals = [0.15, 0.95, 0.65, 0.35, 0.0, 0.0, 0.0, 0.95, 0.85, 0.95];
        warm_smoother(&smoother, &vals, 50, PersonaTrait::Obedient);
        let pkt = sensor_packet_from_floats(&vals);
        let r = process_packet(&pkt, PersonaTrait::Obedient, &smoother);
        assert_eq!(r.kind, VadKind::Emotional);
        assert!(r.valence > 0.55, "valence={:.3} expected > 0.55", r.valence);
        assert!(r.arousal > 0.5, "arousal={:.3} expected > 0.50", r.arousal);
        assert!(r.dominance > 0.45, "dominance={:.3} expected > 0.45", r.dominance);
        assert!(r.is_active, "expected active for excited");
    }

    #[test]
    fn test_short_payload_returns_zeros() {
        let pkt = SensorPacket {
            sensor_id: 1,
            timestamp_us: 0,
            data_type: DATA_TYPE_SENSOR_VECTOR,
            seq: 0,
            payload: vec![0u8; 8],
        };
        let smoother = SensorSmoother::new();
        let r = process_packet(&pkt, PersonaTrait::Obedient, &smoother);
        assert_eq!(r.kind, VadKind::Emotional);
        assert_eq!(r.valence, 0.0);
        assert_eq!(r.arousal, 0.0);
        assert_eq!(r.dominance, 0.0);
    }
}
