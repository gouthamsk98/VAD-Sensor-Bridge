use crate::vad::{ VadResult, VadKind };

/// Binary response format for VAD results via UDP
/// Wire format (24 bytes fixed):
///   [ sensor_id: u32 LE ][ seq: u64 LE ][ is_active: u8 ][ kind: u8 ]
///   [ energy: f32 LE ][ threshold: f32 LE ]
///   [ valence: f32 LE ][ arousal: f32 LE ][ dominance: f32 LE ]
#[derive(Debug, Clone)]
pub struct VadResponsePacket {
    pub sensor_id: u32,
    pub seq: u64,
    pub is_active: u8,
    pub kind: u8,
    pub energy: f32,
    pub threshold: f32,
    pub valence: f32,
    pub arousal: f32,
    pub dominance: f32,
}

impl VadResponsePacket {
    /// Serialize VAD result to binary packet
    pub fn from_vad_result(result: &VadResult) -> Self {
        VadResponsePacket {
            sensor_id: result.sensor_id,
            seq: result.seq,
            is_active: if result.is_active {
                1
            } else {
                0
            },
            kind: match result.kind {
                VadKind::Audio => 1,
                VadKind::Emotional => 2,
            },
            energy: result.energy as f32,
            threshold: result.threshold as f32,
            valence: result.valence,
            arousal: result.arousal,
            dominance: result.dominance,
        }
    }

    /// Serialize to bytes (little-endian)
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(48);
        bytes.extend_from_slice(&self.sensor_id.to_le_bytes());
        bytes.extend_from_slice(&self.seq.to_le_bytes());
        bytes.push(self.is_active);
        bytes.push(self.kind);
        bytes.extend_from_slice(&self.energy.to_le_bytes());
        bytes.extend_from_slice(&self.threshold.to_le_bytes());
        bytes.extend_from_slice(&self.valence.to_le_bytes());
        bytes.extend_from_slice(&self.arousal.to_le_bytes());
        bytes.extend_from_slice(&self.dominance.to_le_bytes());
        bytes
    }
}
