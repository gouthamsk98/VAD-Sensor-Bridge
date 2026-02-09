use serde::{ Deserialize, Serialize };

/// Raw sensor datagram layout (binary, packed, little-endian).
///
/// Wire format (32 bytes fixed header + variable payload):
///   [ sensor_id: u32 LE ][ timestamp_us: u64 LE ][ data_type: u8 ][ reserved: 3 bytes ]
///   [ payload_len: u16 LE ][ reserved: 2 bytes ][ seq: u64 LE ]
///   [ payload: payload_len bytes ]
///
/// For JSON fallback, the entire datagram is treated as UTF-8 JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorPacket {
    pub sensor_id: u32,
    pub timestamp_us: u64,
    pub data_type: u8,
    pub seq: u64,
    pub payload: Vec<u8>,
}

/// Fixed header size for binary wire format
const HEADER_SIZE: usize = 32;

impl SensorPacket {
    /// Parse a binary sensor packet from raw UDP bytes.
    /// Returns None if the datagram is too short or malformed.
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
        // buf[13..16] reserved
        let payload_len = u16::from_le_bytes([buf[16], buf[17]]) as usize;
        // buf[18..20] reserved
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
        // buf[28..32] padding to align

        // Validate
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

    /// Try JSON parse as fallback
    #[inline]
    pub fn from_json(buf: &[u8]) -> Option<Self> {
        serde_json::from_slice(buf).ok()
    }

    /// Parse: try binary first, then JSON fallback
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        Self::from_binary(buf).or_else(|| Self::from_json(buf))
    }

    /// MQTT topic for this sensor
    #[inline]
    pub fn topic(&self, prefix: &str) -> String {
        format!("{}/{}", prefix, self.sensor_id)
    }

    /// Serialize to compact JSON bytes for MQTT publish
    #[inline]
    pub fn to_mqtt_payload(&self) -> Vec<u8> {
        // Unwrap is safe: SensorPacket is always serializable
        serde_json::to_vec(self).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_roundtrip() {
        let mut buf = vec![0u8; 40];
        // sensor_id = 42
        buf[0..4].copy_from_slice(&(42u32).to_le_bytes());
        // timestamp_us = 1234567890
        buf[4..12].copy_from_slice(&(1234567890u64).to_le_bytes());
        // data_type = 1
        buf[12] = 1;
        // payload_len = 8
        buf[16..18].copy_from_slice(&(8u16).to_le_bytes());
        // seq = 99
        buf[20..28].copy_from_slice(&(99u64).to_le_bytes());
        // payload
        buf[32..40].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);

        let pkt = SensorPacket::from_binary(&buf).unwrap();
        assert_eq!(pkt.sensor_id, 42);
        assert_eq!(pkt.timestamp_us, 1234567890);
        assert_eq!(pkt.data_type, 1);
        assert_eq!(pkt.seq, 99);
        assert_eq!(pkt.payload, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_json_parse() {
        let json = r#"{"sensor_id":1,"timestamp_us":100,"data_type":0,"seq":1,"payload":[10,20]}"#;
        let pkt = SensorPacket::from_json(json.as_bytes()).unwrap();
        assert_eq!(pkt.sensor_id, 1);
    }
}
