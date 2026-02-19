use crate::persona::PersonaTrait;
use std::collections::HashMap;
use std::sync::Mutex;

// ─────────────────────────────────────────────────────────────────────
//  Sensor Smoother — EMA-based idle-time decay
// ─────────────────────────────────────────────────────────────────────
//
//  Problem:  When `idle_time` jumps from 0 → 0.9 in a single packet
//            the robot instantly becomes "sad".  Real boredom is gradual.
//
//  Solution: Exponential Moving Average on the idle_time channel (index 6).
//            smoothed = α × raw + (1 − α) × prev_smoothed
//
//  α is persona-dependent — higher α = faster ramp = gets sad quicker:
//
//    Stubborn    α = 0.03   — resists boredom stubbornly (~33 pkt half-life)
//    Obedient    α = 0.05   — stays content for a long time (~20 pkt)
//    Cute        α = 0.08   — slow drift, lingers on social memory (~12 pkt)
//    Mischievous α = 0.15   — gets antsy quickly (~6 pkt)
//
//  Half-life in packets ≈ ln(2) / α  (continuous approximation).
//
//  All other channels are passed through unmodified.

/// Index of the idle_time channel in the 10-element sensor vector.
const IDLE_TIME_IDX: usize = 6;

/// Return the EMA alpha for idle_time given the active persona.
///
/// Higher alpha → idle_time ramps up faster → robot gets sad sooner.
#[inline]
fn idle_alpha(persona: PersonaTrait) -> f32 {
    match persona {
        PersonaTrait::Stubborn => 0.03,
        PersonaTrait::Obedient => 0.05,
        PersonaTrait::Cute => 0.08,
        PersonaTrait::Mischievous => 0.15,
    }
}

/// Per-sensor smoothing state.
///
/// Currently only tracks the EMA of `idle_time`; other channels are
/// passed through.  Extensible to smooth additional channels if needed.
#[derive(Debug, Clone)]
struct SensorEma {
    idle_time: f32,
}

impl SensorEma {
    fn new() -> Self {
        Self { idle_time: 0.0 }
    }
}

/// Thread-safe sensor smoother shared across VAD workers.
///
/// Maintains an EMA state per `sensor_id` so each physical ESP32 device
/// has its own independent idle-time ramp.
pub struct SensorSmoother {
    state: Mutex<HashMap<u32, SensorEma>>,
}

impl SensorSmoother {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Smooth a 10-element sensor array in-place.
    ///
    /// Currently only the idle_time channel (index 6) is EMA-smoothed.
    /// All other channels pass through unchanged.
    ///
    /// The EMA alpha depends on the active persona: a Mischievous robot
    /// ramps idle faster (gets bored sooner), while a Stubborn one
    /// resists boredom for many more packets.
    pub fn smooth(&self, sensor_id: u32, sensors: &mut [f32; 10], persona: PersonaTrait) {
        let alpha = idle_alpha(persona);
        let mut map = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ema = map.entry(sensor_id).or_insert_with(SensorEma::new);

        // EMA update:  smoothed = α * raw + (1 − α) * prev
        let raw_idle = sensors[IDLE_TIME_IDX];
        ema.idle_time = alpha * raw_idle + (1.0 - alpha) * ema.idle_time;
        sensors[IDLE_TIME_IDX] = ema.idle_time;
    }

    /// Reset smoothing state for a specific sensor (e.g. on reconnect).
    #[allow(dead_code)]
    pub fn reset_sensor(&self, sensor_id: u32) {
        let mut map = self.state.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&sensor_id);
    }

    /// Reset all smoothing state.
    #[allow(dead_code)]
    pub fn reset_all(&self) {
        let mut map = self.state.lock().unwrap_or_else(|e| e.into_inner());
        map.clear();
    }
}

// ─────────────────────────────────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sensors(idle: f32) -> [f32; 10] {
        let mut s = [0.0f32; 10];
        s[IDLE_TIME_IDX] = idle;
        s
    }

    #[test]
    fn test_first_packet_is_heavily_damped() {
        let smoother = SensorSmoother::new();
        let mut s = make_sensors(0.9);
        smoother.smooth(1, &mut s, PersonaTrait::Obedient); // α=0.05

        // First packet: 0.05 * 0.9 + 0.95 * 0.0 = 0.045
        assert!(
            s[IDLE_TIME_IDX] < 0.05,
            "idle={:.4} expected < 0.05 after first packet",
            s[IDLE_TIME_IDX]
        );
    }

    #[test]
    fn test_converges_after_many_packets() {
        let smoother = SensorSmoother::new();
        // Feed 200 packets with idle=0.9, α=0.05 for Obedient
        for _ in 0..200 {
            let mut s = make_sensors(0.9);
            smoother.smooth(1, &mut s, PersonaTrait::Obedient);
        }
        let mut s = make_sensors(0.9);
        smoother.smooth(1, &mut s, PersonaTrait::Obedient);

        // After 200 packets, EMA should be very close to 0.9
        assert!(
            s[IDLE_TIME_IDX] > 0.85,
            "idle={:.4} expected > 0.85 after convergence",
            s[IDLE_TIME_IDX]
        );
    }

    #[test]
    fn test_mischievous_ramps_faster_than_obedient() {
        let smoother = SensorSmoother::new();

        // Feed 20 packets to sensor 1 (Obedient, α=0.05)
        for _ in 0..20 {
            let mut s = make_sensors(0.9);
            smoother.smooth(1, &mut s, PersonaTrait::Obedient);
        }
        let mut s_obed = make_sensors(0.9);
        smoother.smooth(1, &mut s_obed, PersonaTrait::Obedient);

        // Feed 20 packets to sensor 2 (Mischievous, α=0.15)
        for _ in 0..20 {
            let mut s = make_sensors(0.9);
            smoother.smooth(2, &mut s, PersonaTrait::Mischievous);
        }
        let mut s_misc = make_sensors(0.9);
        smoother.smooth(2, &mut s_misc, PersonaTrait::Mischievous);

        // Mischievous should be further along toward 0.9
        assert!(
            s_misc[IDLE_TIME_IDX] > s_obed[IDLE_TIME_IDX],
            "mischievous idle={:.4} should be > obedient idle={:.4}",
            s_misc[IDLE_TIME_IDX],
            s_obed[IDLE_TIME_IDX]
        );
    }

    #[test]
    fn test_recovery_when_idle_drops() {
        let smoother = SensorSmoother::new();

        // Ramp up idle over 50 packets
        for _ in 0..50 {
            let mut s = make_sensors(0.9);
            smoother.smooth(1, &mut s, PersonaTrait::Obedient);
        }

        // Now idle drops to 0 (activity resumed)
        let mut s = make_sensors(0.0);
        smoother.smooth(1, &mut s, PersonaTrait::Obedient);

        // Smoothed value should still be high-ish (slow decay back down)
        assert!(
            s[IDLE_TIME_IDX] > 0.5,
            "idle={:.4} should decay slowly after activity resumes",
            s[IDLE_TIME_IDX]
        );
    }

    #[test]
    fn test_other_channels_untouched() {
        let smoother = SensorSmoother::new();
        let mut s = [0.5f32; 10];
        s[IDLE_TIME_IDX] = 0.9;
        smoother.smooth(1, &mut s, PersonaTrait::Obedient);

        // All channels except idle_time should be unchanged
        for (i, &v) in s.iter().enumerate() {
            if i != IDLE_TIME_IDX {
                assert_eq!(v, 0.5, "channel {i} should be untouched");
            }
        }
    }

    #[test]
    fn test_independent_per_sensor_id() {
        let smoother = SensorSmoother::new();

        // Ramp sensor 1
        for _ in 0..50 {
            let mut s = make_sensors(0.9);
            smoother.smooth(1, &mut s, PersonaTrait::Obedient);
        }

        // Sensor 2 should start fresh
        let mut s2 = make_sensors(0.9);
        smoother.smooth(2, &mut s2, PersonaTrait::Obedient);
        assert!(
            s2[IDLE_TIME_IDX] < 0.05,
            "sensor 2 should start from 0, got {:.4}",
            s2[IDLE_TIME_IDX]
        );
    }
}
