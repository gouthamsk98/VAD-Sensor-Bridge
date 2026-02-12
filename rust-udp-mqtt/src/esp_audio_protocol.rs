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
