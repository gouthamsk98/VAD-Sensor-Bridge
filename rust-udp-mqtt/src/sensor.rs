/// Raw sensor datagram layout (binary, packed, little-endian).
///
/// Wire format (32 bytes fixed header + variable payload):
///   [ sensor_id: u32 LE ][ timestamp_us: u64 LE ][ data_type: u8 ][ reserved: 3 bytes ]
///   [ payload_len: u16 LE ][ reserved: 2 bytes ][ seq: u64 LE ][ padding: 4 bytes ]
///   [ payload: payload_len bytes ]
///
/// Data types:
///   1 = 16-bit LE PCM audio (for audio RMS VAD)
///   2 = 10×f32 LE sensor vector (for emotional VAD: Valence-Arousal-Dominance)
#[derive(Debug, Clone)]
pub struct SensorPacket {
    pub sensor_id: u32,
    pub timestamp_us: u64,
    pub data_type: u8,
    pub seq: u64,
    pub payload: Vec<u8>,
}

/// Sensor data type: 16-bit LE PCM audio
pub const DATA_TYPE_AUDIO: u8 = 1;
/// Sensor data type: 10×f32 LE environmental sensor vector
pub const DATA_TYPE_SENSOR_VECTOR: u8 = 2;

/// Number of sensor channels in the emotional sensor vector
pub const SENSOR_VECTOR_LEN: usize = 10;
/// Byte size of a sensor vector payload (10 × 4 bytes)
pub const SENSOR_VECTOR_BYTES: usize = SENSOR_VECTOR_LEN * 4;

/// Environmental/social sensor vector used for emotional VAD computation.
///
/// Each field is normalised to \[0.0, 1.0\].
///
/// The 10 channels map to specific contextual signals from the robot's
/// perception pipeline (camera, IMU, microphone, battery monitor).
#[derive(Debug, Clone, Copy, Default)]
pub struct SensorVector {
    /// Battery is low/depleted (0 = full, 1 = critical)
    pub battery_low: f32,
    /// Normalised count of people detected nearby
    pub people_count: f32,
    /// Confidence that a recognised/known face is present
    pub known_face: f32,
    /// Confidence that an unknown/unfamiliar face is present
    pub unknown_face: f32,
    /// Fall / impact event intensity
    pub fall_event: f32,
    /// Robot was lifted / grabbed
    pub lifted: f32,
    /// How long the robot has been idle (0 = just active, 1 = very idle)
    pub idle_time: f32,
    /// Ambient sound energy level
    pub sound_energy: f32,
    /// Speech / voice cadence rate (proxy for conversation)
    pub voice_rate: f32,
    /// Overall motion energy from IMU / accelerometer
    pub motion_energy: f32,
}

impl SensorVector {
    /// Parse a sensor vector from a 40-byte LE payload.
    ///
    /// Returns `None` if the payload is too short.
    #[inline]
    pub fn from_payload(payload: &[u8]) -> Option<Self> {
        if payload.len() < SENSOR_VECTOR_BYTES {
            return None;
        }
        let f = |i: usize| -> f32 {
            let off = i * 4;
            f32::from_le_bytes([payload[off], payload[off + 1], payload[off + 2], payload[off + 3]])
        };
        Some(SensorVector {
            battery_low: f(0),
            people_count: f(1),
            known_face: f(2),
            unknown_face: f(3),
            fall_event: f(4),
            lifted: f(5),
            idle_time: f(6),
            sound_energy: f(7),
            voice_rate: f(8),
            motion_energy: f(9),
        })
    }

    /// Return the vector as a `[f32; 10]` array in channel order.
    #[inline]
    pub fn as_array(&self) -> [f32; SENSOR_VECTOR_LEN] {
        [
            self.battery_low,
            self.people_count,
            self.known_face,
            self.unknown_face,
            self.fall_event,
            self.lifted,
            self.idle_time,
            self.sound_energy,
            self.voice_rate,
            self.motion_energy,
        ]
    }
}

/// Fixed header size for binary wire format
pub const HEADER_SIZE: usize = 32;

impl SensorPacket {
    /// Parse a binary sensor packet from raw bytes.
    #[inline]
    pub fn from_binary(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_SIZE {
            return None;
        }

        let sensor_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let timestamp_us = u64::from_le_bytes([
            buf[4],
            buf[5],
            buf[6],
            buf[7],
            buf[8],
            buf[9],
            buf[10],
            buf[11],
        ]);
        let data_type = buf[12];
        let payload_len = u16::from_le_bytes([buf[16], buf[17]]) as usize;
        let seq = u64::from_le_bytes([
            buf[20],
            buf[21],
            buf[22],
            buf[23],
            buf[24],
            buf[25],
            buf[26],
            buf[27],
        ]);

        if buf.len() < HEADER_SIZE + payload_len {
            return None;
        }

        let payload = buf[HEADER_SIZE..HEADER_SIZE + payload_len].to_vec();

        Some(SensorPacket {
            sensor_id,
            timestamp_us,
            data_type,
            seq,
            payload,
        })
    }

    /// Parse a binary sensor packet.
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        Self::from_binary(buf)
    }
}
