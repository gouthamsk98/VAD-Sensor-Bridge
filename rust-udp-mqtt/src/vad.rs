use crate::sensor::SensorPacket;

/// VAD (Voice Activity Detection) result for a sensor packet.
#[derive(Debug, Clone)]
pub struct VadResult {
    pub sensor_id: u32,
    pub seq: u64,
    pub energy: f64,
    pub is_active: bool,
    pub threshold: f64,
}

/// Energy threshold for voice activity detection.
/// Packets with RMS energy above this are considered "active".
const VAD_ENERGY_THRESHOLD: f64 = 30.0;

/// Compute VAD on a sensor packet's payload.
///
/// Uses a simple energy-based detector:
///   1. Treat payload bytes as 16-bit LE PCM audio samples
///   2. Compute RMS (root mean square) energy
///   3. Compare against threshold
///
/// This is a realistic, non-trivial computation that exercises
/// the CPU pipeline for benchmarking purposes.
#[inline]
pub fn compute_vad(packet: &SensorPacket) -> VadResult {
    let energy = compute_rms_energy(&packet.payload);
    let is_active = energy > VAD_ENERGY_THRESHOLD;

    VadResult {
        sensor_id: packet.sensor_id,
        seq: packet.seq,
        energy,
        is_active,
        threshold: VAD_ENERGY_THRESHOLD,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_silence_detection() {
        let packet = SensorPacket {
            sensor_id: 1,
            timestamp_us: 0,
            data_type: 1,
            seq: 0,
            payload: vec![0u8; 64], // silence
        };
        let result = compute_vad(&packet);
        assert!(!result.is_active);
        assert_eq!(result.energy, 0.0);
    }

    #[test]
    fn test_loud_signal() {
        let packet = SensorPacket {
            sensor_id: 1,
            timestamp_us: 0,
            data_type: 1,
            seq: 0,
            payload: vec![0xff, 0x7f, 0xff, 0x7f, 0xff, 0x7f, 0xff, 0x7f], // max positive samples
        };
        let result = compute_vad(&packet);
        assert!(result.is_active);
        assert!(result.energy > VAD_ENERGY_THRESHOLD);
    }
}
