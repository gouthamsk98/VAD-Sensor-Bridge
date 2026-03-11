/// ESP32 ↔ Server UDP Audio Protocol
///
/// Packet format (4-byte header + variable payload):
/// ```text
/// ┌─────────────┬──────────┬──────────┬────────────────┐
/// │ Byte 0-1    │ Byte 2   │ Byte 3   │ Byte 4..N      │
/// │ Seq Num     │ Type     │ Flags    │ Payload         │
/// │ (uint16 LE) │ (uint8)  │ (uint8)  │ (up to 1400B)  │
/// └─────────────┴──────────┴──────────┴────────────────┘
/// ```
///
/// Audio format: 16-bit LE PCM, 16 kHz, mono.
/// 1400 B payload = 700 samples = 43.75 ms per packet.

// ═══════════════════════════════════════════════════════════════════════
//  Constants
// ═══════════════════════════════════════════════════════════════════════

/// Minimum header size (seq_num + type + flags).
pub const ESP_HEADER_SIZE: usize = 4;

/// Maximum payload size — stays under typical 1500-byte MTU.
pub const ESP_MAX_PAYLOAD: usize = 1400;
/// Response audio chunk size (smaller for ESP receive reliability).
pub const RESPONSE_CHUNK_SIZE: usize = 1024;

// ── Packet Types ───────────────────────────────────────────────────────

/// ESP → Server: microphone PCM audio chunk.
pub const PKT_AUDIO_UP: u8 = 0x01;
/// Server → ESP: raw I2S playback data.
pub const PKT_AUDIO_DOWN: u8 = 0x02;
/// Bidirectional: control / command messages.
pub const PKT_CONTROL: u8 = 0x03;
/// Bidirectional: keep-alive / RTT measurement.
pub const PKT_HEARTBEAT: u8 = 0x04;

// ── Flags (bitfield in byte 3) ─────────────────────────────────────────

/// BIT0 — start of stream.
pub const FLAG_START: u8 = 0x01;
/// BIT1 — end of stream.
pub const FLAG_END: u8 = 0x02;
/// BIT2 — urgent / priority.
pub const FLAG_URGENT: u8 = 0x04;

// ── Control Commands (first byte of payload when type == PKT_CONTROL) ──

/// ESP → Server: wake word detected, begin session.
pub const CTRL_SESSION_START: u8 = 0x01;
/// ESP → Server: user stopped speaking (timeout / silence).
pub const CTRL_SESSION_END: u8 = 0x02;
/// Server → ESP: about to send audio response.
pub const CTRL_STREAM_START: u8 = 0x03;
/// Server → ESP: finished sending audio response.
pub const CTRL_STREAM_END: u8 = 0x04;
/// Bidirectional: acknowledge a control message.
pub const CTRL_ACK: u8 = 0x05;
/// Bidirectional: abort current session.
pub const CTRL_CANCEL: u8 = 0x06;
/// Server → ESP: server is ready for audio.
pub const CTRL_SERVER_READY: u8 = 0x07;

// ═══════════════════════════════════════════════════════════════════════
//  Parsed Packet
// ═══════════════════════════════════════════════════════════════════════

/// A parsed ESP audio-protocol packet.
#[derive(Debug, Clone)]
pub struct EspPacket {
    pub seq_num: u16,
    pub pkt_type: u8,
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl EspPacket {
    /// Parse an ESP packet from raw UDP bytes.
    ///
    /// Returns `None` if the buffer is too short, the type is unknown, or
    /// the payload exceeds `ESP_MAX_PAYLOAD`.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < ESP_HEADER_SIZE {
            return None;
        }

        let seq_num = u16::from_le_bytes([buf[0], buf[1]]);
        let pkt_type = buf[2];
        let flags = buf[3];
        let payload = buf[ESP_HEADER_SIZE..].to_vec();

        // Validate known packet type
        if !matches!(pkt_type, PKT_AUDIO_UP | PKT_AUDIO_DOWN | PKT_CONTROL | PKT_HEARTBEAT) {
            return None;
        }

        // Guard against oversized payloads
        if payload.len() > ESP_MAX_PAYLOAD {
            return None;
        }

        Some(EspPacket { seq_num, pkt_type, flags, payload })
    }

    /// `true` when the START flag is set.
    #[inline]
    pub fn is_start(&self) -> bool {
        (self.flags & FLAG_START) != 0
    }

    /// `true` when the END flag is set.
    #[inline]
    pub fn is_end(&self) -> bool {
        (self.flags & FLAG_END) != 0
    }

    /// `true` when the URGENT flag is set.
    #[inline]
    pub fn is_urgent(&self) -> bool {
        (self.flags & FLAG_URGENT) != 0
    }

    /// For control packets, returns the command byte (first byte of payload).
    pub fn control_cmd(&self) -> Option<u8> {
        if self.pkt_type == PKT_CONTROL && !self.payload.is_empty() {
            Some(self.payload[0])
        } else {
            None
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Packet Builders (Server → ESP)
// ═══════════════════════════════════════════════════════════════════════

/// Build a raw packet for transmission.
pub fn build_packet(seq_num: u16, pkt_type: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ESP_HEADER_SIZE + payload.len());
    buf.extend_from_slice(&seq_num.to_le_bytes());
    buf.push(pkt_type);
    buf.push(flags);
    buf.extend_from_slice(payload);
    buf
}

/// Build a control packet (type = `PKT_CONTROL`, payload = `[cmd]`).
pub fn build_control(seq_num: u16, cmd: u8, flags: u8) -> Vec<u8> {
    build_packet(seq_num, PKT_CONTROL, flags, &[cmd])
}

/// Build a heartbeat response mirroring the incoming sequence number.
pub fn build_heartbeat(seq_num: u16) -> Vec<u8> {
    build_packet(seq_num, PKT_HEARTBEAT, 0, &[])
}

/// Build an audio-down packet (type = `PKT_AUDIO_DOWN`).
pub fn build_audio_down(seq_num: u16, flags: u8, pcm: &[u8]) -> Vec<u8> {
    build_packet(seq_num, PKT_AUDIO_DOWN, flags, pcm)
}

// ═══════════════════════════════════════════════════════════════════════
//  Session State Machine
// ═══════════════════════════════════════════════════════════════════════

/// Lifecycle state of an ESP audio session.
///
/// ```text
/// Idle ──SESSION_START──▶ Receiving ──SESSION_END──▶ Processing
///   ▲                                                    │
///   │                                                    ▼
///   └───────────────────── done ◀──────────────── Responding
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No active session — waiting for `CTRL_SESSION_START`.
    Idle,
    /// Session active — receiving audio from the ESP.
    Receiving,
    /// ESP signalled end — server is processing the accumulated audio.
    Processing,
    /// Server is streaming an audio response back to the ESP.
    Responding,
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionState::Idle => write!(f, "idle"),
            SessionState::Receiving => write!(f, "receiving"),
            SessionState::Processing => write!(f, "processing"),
            SessionState::Responding => write!(f, "responding"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Per-Client Session
// ═══════════════════════════════════════════════════════════════════════

/// Tracks the state and accumulated audio for a single ESP client.
#[derive(Debug)]
pub struct EspSession {
    pub state: SessionState,
    /// Remote socket address of the ESP client.
    pub addr: std::net::SocketAddr,
    /// MAC address from notification packet.
    pub mac: Option<[u8; 6]>,
    /// Next outgoing sequence number (wraps at u16::MAX).
    pub out_seq: u16,
    /// Last received sequence number.
    pub last_recv_seq: u16,
    /// Total audio packets received this session.
    pub audio_packets: u32,
    /// Total audio bytes received this session.
    pub audio_bytes: u64,
    /// Accumulated raw PCM buffer for the session.
    pub audio_buffer: Vec<u8>,
    /// Number of detected sequence gaps (lost packets).
    pub packets_lost: u32,
    /// Timestamp when the session entered `Receiving`.
    pub started_at: std::time::Instant,
}

impl EspSession {
    /// Create a new idle session for the given client address.
    ///
    /// Pre-allocates ~30 s of 16 kHz/16-bit/mono audio (960 kB).
    pub fn new(addr: std::net::SocketAddr) -> Self {
        EspSession {
            state: SessionState::Idle,
            addr,
            mac: None,
            out_seq: 0,
            last_recv_seq: 0,
            audio_packets: 0,
            audio_bytes: 0,
            audio_buffer: Vec::with_capacity(16_000 * 2 * 30),
            packets_lost: 0,
            started_at: std::time::Instant::now(),
        }
    }

    /// Return the next outgoing sequence number and advance the counter.
    pub fn next_seq(&mut self) -> u16 {
        let s = self.out_seq;
        self.out_seq = self.out_seq.wrapping_add(1);
        s
    }

    /// Record an incoming audio packet: append payload, detect gaps.
    pub fn record_audio(&mut self, seq: u16, payload: &[u8]) {
        if self.audio_packets > 0 {
            let expected = self.last_recv_seq.wrapping_add(1);
            if seq != expected {
                let gap = seq.wrapping_sub(expected) as u32;
                self.packets_lost += gap;
            }
        }
        self.last_recv_seq = seq;
        self.audio_packets += 1;
        self.audio_bytes += payload.len() as u64;
        self.audio_buffer.extend_from_slice(payload);
    }

    /// Reset all counters and transition to `Idle`.
    pub fn reset(&mut self) {
        self.state = SessionState::Idle;
        self.audio_packets = 0;
        self.audio_bytes = 0;
        self.audio_buffer.clear();
        self.packets_lost = 0;
        self.started_at = std::time::Instant::now();
    }

    /// Wall-clock duration since the session started receiving.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    /// Estimated audio duration in seconds (16 kHz, 16-bit, mono).
    pub fn audio_duration_secs(&self) -> f64 {
        (self.audio_bytes as f64) / (16_000.0 * 2.0)
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Notification Protocol (0xAA 0xB0 framing)
// ═══════════════════════════════════════════════════════════════════════
//
// Packet format (14 bytes):
// ┌───────┬───────┬───────┬───────┬─────┬──────────────┬──────┬───────┬───────┐
// │ B0    │ B1    │ B2    │ B3    │ B4  │ B5..B10      │ B11  │ B12   │ B13   │
// │ 0xAA  │ 0xB0  │ LenHi │ LenLo│ CMD │ MAC (6B)     │ Chk  │ 0xFF  │ 0xF5  │
// └───────┴───────┴───────┴───────┴─────┴──────────────┴──────┴───────┴───────┘

/// Start marker byte 0.
pub const NOTIFY_START_0: u8 = 0xaa;
/// Start marker byte 1.
pub const NOTIFY_START_1: u8 = 0xb0;
/// End marker byte 0.
pub const NOTIFY_END_0: u8 = 0xff;
/// End marker byte 1.
pub const NOTIFY_END_1: u8 = 0xf5;

/// ESP → Server: session start (wake-word detected).
pub const NOTIFY_CMD_START: u8 = 0x51;
/// ESP → Server: session stop (user stopped speaking).
pub const NOTIFY_CMD_STOP: u8 = 0x50;
/// Server → ESP: server is ready.
pub const NOTIFY_CMD_SERVER_READY: u8 = 0x52;
/// Server → ESP: acknowledge.
pub const NOTIFY_CMD_ACK: u8 = 0x53;

/// Fixed size of a notification packet.
pub const NOTIFY_PACKET_SIZE: usize = 14;

/// Position of the checksum byte in the notification packet.
const NOTIFY_CHECKSUM_POS: usize = 11;

/// Result of parsing a notification packet — includes the byte offset
/// where the notification header ends, so any trailing audio data in the
/// same UDP datagram can be forwarded to the PCM pipeline.
#[derive(Debug, Clone)]
pub struct NotifyParseResult {
    pub packet: NotifyPacket,
    /// Byte offset (from the start of the original buffer) where the
    /// notification header ends.  Bytes `[header_end..len]` are audio.
    pub header_end: usize,
}

/// A parsed notification packet (new 0xAA/0xB0 framing).
#[derive(Debug, Clone)]
pub struct NotifyPacket {
    pub cmd: u8,
    pub mac: [u8; 6],
}

impl NotifyPacket {
    /// Try to parse a notification packet from raw bytes.
    ///
    /// The parser is intentionally lenient to handle the several packet
    /// variants the ESP firmware emits:
    ///
    /// 1. **12-byte bare**  – `AA B0 LEN_H LEN_L CMD MAC[6] CHK`
    ///    (no end markers).
    /// 2. **14-byte full**   – same as above + `FF F5` end markers.
    /// 3. **16-byte prefixed** – `FF F5` prefix + 14-byte full.
    /// 4. **14-byte header + trailing PCM audio** – START notification
    ///    followed by raw audio in the same UDP datagram.
    ///
    /// Checksum validation is **skipped** because the ESP firmware
    /// currently uses a different algorithm than originally specified.
    pub fn parse(buf: &[u8]) -> Option<NotifyParseResult> {
        // Scan forward for the AA B0 start marker (handles FF F5 prefix).
        // Limit scan to the first 2 bytes of offset to avoid false
        // positives inside raw audio data (only legitimate prefix is
        // the 2-byte FF F5 end-marker from a previous frame).
        let start = buf
            .windows(2)
            .take(3) // positions 0, 1, 2 only
            .position(|w| w[0] == NOTIFY_START_0 && w[1] == NOTIFY_START_1)?;

        let remaining = &buf[start..];

        // Need at least 12 bytes: AA B0 LEN_H LEN_L CMD MAC[6] CHK
        if remaining.len() < 12 {
            return None;
        }

        let cmd = remaining[4];

        // Only accept known notification commands to avoid false
        // positives from audio data that coincidentally starts with
        // AA B0.
        if
            !matches!(
                cmd,
                NOTIFY_CMD_START | NOTIFY_CMD_STOP | NOTIFY_CMD_SERVER_READY | NOTIFY_CMD_ACK
            )
        {
            return None;
        }

        let mut mac = [0u8; 6];
        mac.copy_from_slice(&remaining[5..11]);

        // Determine header length — with or without FF F5 end markers.
        let header_end = if
            remaining.len() >= 14 &&
            remaining[12] == NOTIFY_END_0 &&
            remaining[13] == NOTIFY_END_1
        {
            start + 14
        } else {
            start + 12
        };

        Some(NotifyParseResult {
            packet: NotifyPacket { cmd, mac },
            header_end,
        })
    }

    /// Format the MAC address as a colon-separated hex string.
    pub fn mac_str(&self) -> String {
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.mac[0],
            self.mac[1],
            self.mac[2],
            self.mac[3],
            self.mac[4],
            self.mac[5]
        )
    }
}

/// Compute the XOR checksum for a notification packet (all bytes except
/// the checksum position itself).
pub fn compute_notify_checksum(buf: &[u8]) -> u8 {
    buf.iter()
        .enumerate()
        .take(NOTIFY_PACKET_SIZE)
        .filter(|&(i, _)| i != NOTIFY_CHECKSUM_POS)
        .fold(0u8, |acc, (_, &b)| acc ^ b)
}

/// Build a 14-byte notification packet for server → ESP replies.
pub fn build_notify_packet(cmd: u8, mac: &[u8; 6]) -> [u8; NOTIFY_PACKET_SIZE] {
    let payload_len: u16 = 7; // MAC(6) + CMD(1)
    let mut buf = [0u8; NOTIFY_PACKET_SIZE];
    buf[0] = NOTIFY_START_0;
    buf[1] = NOTIFY_START_1;
    buf[2] = (payload_len >> 8) as u8;
    buf[3] = (payload_len & 0xff) as u8;
    buf[4] = cmd;
    buf[5..11].copy_from_slice(mac);
    buf[12] = NOTIFY_END_0;
    buf[13] = NOTIFY_END_1;
    buf[NOTIFY_CHECKSUM_POS] = compute_notify_checksum(&buf);
    buf
}

// ═══════════════════════════════════════════════════════════════════════
//  Server → Device Protocol (0xB0 0xAA framing)
// ═══════════════════════════════════════════════════════════════════════
//
// Frame format:
//   [0xB0][0xAA][len_hi][len_lo][CMD][...payload...][CRC-8][0xFF][0xF5]
//
// CRC-8 polynomial 0x07 — computed over bytes [2 .. len-3).

/// Server → Device header byte 0.
pub const S2D_HEADER_0: u8 = 0xB0;
/// Server → Device header byte 1.
pub const S2D_HEADER_1: u8 = 0xAA;
/// Footer byte 0.
pub const S2D_FOOTER_0: u8 = 0xFF;
/// Footer byte 1.
pub const S2D_FOOTER_1: u8 = 0xF5;

/// CMD: audio settings (reconfigure I2S).
pub const S2D_CMD_AUDIO_SETTINGS: u8 = 0x5A;
/// CMD: stop playback.
pub const S2D_CMD_STOP: u8 = 0x0A;

/// CRC-8 with polynomial 0x07.
///
/// Matches the ESP32 `checkSum()` in `app_dataparse.cpp`:
/// iterates over `frame[2 .. len-3]` with CRC-8 poly 0x07.
fn crc8_s2d(frame: &[u8]) -> u8 {
    if frame.len() < 5 {
        return 0;
    }
    let end = frame.len() - 3; // skip last 3 bytes (CRC + footer)
    let mut crc: u8 = 0x00;
    for &b in &frame[2..end] {
        crc ^= b;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Build an audio-settings frame (CMD 0x5A) — 17 bytes.
///
/// Tells the device to reconfigure I2S:
///   `[0xB0][0xAA][0x00][0x0A][0x5A][rate:4B][bits:4B][ch:1B][CRC][0xFF][0xF5]`
pub fn build_s2d_audio_settings(rate: u32, bits: u32, channels: u8) -> [u8; 17] {
    let payload_len: u16 = 10; // CMD(1) + rate(4) + bits(4) + ch(1)
    let mut f = [0u8; 17];
    f[0] = S2D_HEADER_0;
    f[1] = S2D_HEADER_1;
    f[2] = (payload_len >> 8) as u8;
    f[3] = (payload_len & 0xFF) as u8;
    f[4] = S2D_CMD_AUDIO_SETTINGS;
    f[5] = (rate >> 24) as u8;
    f[6] = (rate >> 16) as u8;
    f[7] = (rate >> 8) as u8;
    f[8] = rate as u8;
    f[9] = (bits >> 24) as u8;
    f[10] = (bits >> 16) as u8;
    f[11] = (bits >> 8) as u8;
    f[12] = bits as u8;
    f[13] = channels;
    f[14] = crc8_s2d(&f);
    f[15] = S2D_FOOTER_0;
    f[16] = S2D_FOOTER_1;
    f
}

/// Build a stop frame (CMD 0x0A) — 8 bytes.
///
/// Tells the device to stop playback:
///   `[0xB0][0xAA][0x00][0x01][0x0A][CRC][0xFF][0xF5]`
pub fn build_s2d_stop() -> [u8; 8] {
    let payload_len: u16 = 1; // just CMD
    let mut f = [0u8; 8];
    f[0] = S2D_HEADER_0;
    f[1] = S2D_HEADER_1;
    f[2] = (payload_len >> 8) as u8;
    f[3] = (payload_len & 0xFF) as u8;
    f[4] = S2D_CMD_STOP;
    f[5] = crc8_s2d(&f);
    f[6] = S2D_FOOTER_0;
    f[7] = S2D_FOOTER_1;
    f
}
