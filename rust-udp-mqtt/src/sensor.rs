use serde::{ Deserialize, Serialize };

/// Raw sensor datagram layout (binary, packed, little-endian).
///
/// Wire format (32 bytes fixed header + variable payload):
///   [ sensor_id: u32 LE ][ timestamp_us: u64 LE ][ data_type: u8 ][ reserved: 3 bytes ]
///   [ payload_len: u16 LE ][ reserved: 2 bytes ][ seq: u64 LE ][ padding: 4 bytes ]
///   [ payload: payload_len bytes ]
///
/// For TCP, packets are length-prefixed: [ total_len: u32 LE ][ binary_packet ]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorPacket {
    pub sensor_id: u32,
    pub timestamp_us: u64,
    pub data_type: u8,
    pub seq: u64,
    pub payload: Vec<u8>,
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

    /// Try JSON parse as fallback (for MQTT text payloads)
    #[inline]
    pub fn from_json(buf: &[u8]) -> Option<Self> {
        serde_json::from_slice(buf).ok()
    }

    /// Parse: try binary first, then JSON fallback
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        Self::from_binary(buf).or_else(|| Self::from_json(buf))
    }
}
