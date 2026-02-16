/// OpenAI Realtime WebSocket bridge.
///
/// Connects to the OpenAI Realtime API via WebSocket and provides a
/// bidirectional audio channel:
///
/// ```text
///  ESP (16 kHz PCM)           Rust Server            OpenAI (24 kHz PCM)
///  ───────────────── ──UDP──▶ ┌──────────────┐ ──WS──▶ ┌───────────┐
///    AUDIO_UP chunks          │  resample    │         │  Realtime  │
///                             │  16→24 kHz   │         │   API      │
///                             │  + base64    │         │           │
///  ◀──UDP── ──────── ◀─────── │  resample    │ ◀──WS── │           │
///    AUDIO_DOWN chunks        │  24→16 kHz   │         └───────────┘
///                             └──────────────┘
/// ```
///
/// Audio format notes:
///   - ESP protocol: 16-bit LE PCM, 16 kHz, mono
///   - OpenAI Realtime: 16-bit LE PCM, 24 kHz, mono
///   - We resample using linear interpolation (good enough for voice)

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::{ SinkExt, StreamExt };
use serde_json::{ json, Value };
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{ mpsc, RwLock };
use tokio_tungstenite::tungstenite;
use tracing::{ debug, error, info, warn };

use crate::config::Config;
use crate::esp_audio_protocol::*;

// ═══════════════════════════════════════════════════════════════════════
//  Public types
// ═══════════════════════════════════════════════════════════════════════

/// Handle to a running OpenAI Realtime session.
///
/// Drop or call `close()` to shut down the WebSocket connection.
pub struct OpenAiSession {
    /// Send raw 16 kHz PCM chunks here; they'll be resampled + forwarded.
    pub audio_tx: mpsc::Sender<Vec<u8>>,
    /// Send WebSocket control messages (e.g. input_audio_buffer.clear).
    pub control_tx: mpsc::Sender<tungstenite::Message>,
    /// The currently-active ESP client address (reader sends AUDIO_DOWN here).
    pub active_esp: Arc<RwLock<Option<SocketAddr>>>,
    /// Join handle for the reader (response.audio.delta → ESP).
    reader_handle: tokio::task::JoinHandle<()>,
    /// Join handle for the writer (audio_tx → input_audio_buffer.append).
    writer_handle: tokio::task::JoinHandle<()>,
}

impl OpenAiSession {
    /// Gracefully shut down the session.
    pub fn close(&self) {
        self.reader_handle.abort();
        self.writer_handle.abort();
    }

    /// Clear the OpenAI input audio buffer (discard un-committed audio).
    pub async fn clear_input_buffer(&self) {
        let event = json!({"type": "input_audio_buffer.clear"}).to_string();
        let _ = self.control_tx.send(tungstenite::Message::Text(event)).await;
        info!("🧹 input_audio_buffer.clear sent to OpenAI");
    }

    /// Commit the OpenAI input audio buffer (force processing of any
    /// audio that server_vad hasn't auto-committed yet).
    pub async fn commit_input_buffer(&self) {
        let event = json!({"type": "input_audio_buffer.commit"}).to_string();
        let _ = self.control_tx.send(tungstenite::Message::Text(event)).await;
        info!("📝 input_audio_buffer.commit sent to OpenAI");
    }

    /// Explicitly trigger a response from OpenAI.
    ///
    /// With `server_vad`, responses are normally triggered automatically
    /// when silence is detected.  However, if we manually commit the
    /// buffer (e.g. on SESSION_END) we bypass that auto-trigger and
    /// must explicitly ask for a response.
    pub async fn create_response(&self) {
        let event = json!({"type": "response.create"}).to_string();
        let _ = self.control_tx.send(tungstenite::Message::Text(event)).await;
        info!("🗣️ response.create sent to OpenAI");
    }

    /// Update the session instructions (prompt) on the fly.
    pub async fn update_instructions(&self, instructions: &str) {
        let event =
            json!({
            "type": "session.update",
            "session": {
                "instructions": instructions
            }
        }).to_string();
        let _ = self.control_tx.send(tungstenite::Message::Text(event)).await;
        info!(len = instructions.len(), "🧭 session.update sent (instructions)");
    }

    /// Set the active ESP client that receives audio responses.
    pub async fn set_active_esp(&self, addr: SocketAddr) {
        *self.active_esp.write().await = Some(addr);
        debug!(esp = %addr, "active ESP client updated");
    }

    /// Clear the active ESP client (audio responses will be dropped).
    pub async fn clear_active_esp(&self) {
        *self.active_esp.write().await = None;
        debug!("active ESP client cleared");
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Session spawner
// ═══════════════════════════════════════════════════════════════════════

/// Open a new OpenAI Realtime WebSocket session and return a handle.
///
/// * `config`       — server configuration (API key, model, voice, etc.)
/// * `esp_addr`     — the ESP client's UDP address (for sending audio back)
/// * `audio_socket` — shared audio UDP socket (for sending AUDIO_DOWN)
///
/// The returned [`OpenAiSession`] has an `audio_tx` sender: push 16 kHz
/// PCM chunks into it and they'll be streamed to OpenAI in real time.
/// Response audio is automatically resampled to 16 kHz and sent back to
/// the ESP as `PKT_AUDIO_DOWN` packets.
pub async fn spawn_openai_session(
    config: &Config,
    active_esp: Arc<RwLock<Option<SocketAddr>>>,
    audio_socket: Arc<UdpSocket>
) -> anyhow::Result<OpenAiSession> {
    let api_key = config.openai_api_key.clone();
    let model = config.openai_model.clone();
    let voice = config.openai_voice.clone();
    let instructions = config.openai_instructions.clone();

    if api_key.is_empty() {
        anyhow::bail!("OpenAI API key not set (use --openai-api-key or OPENAI_API_KEY env var)");
    }

    // ── Connect WebSocket ──────────────────────────────────────────────
    let ws_url = format!("wss://api.openai.com/v1/realtime?model={}", model);

    let request = tungstenite::http::Request
        ::builder()
        .uri(&ws_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("OpenAI-Beta", "realtime=v1")
        .header("Host", "api.openai.com")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
        .body(())?;

    let (ws_stream, response) = tokio_tungstenite
        ::connect_async(request).await
        .map_err(|e| { anyhow::anyhow!("Failed to connect to OpenAI Realtime API: {}", e) })?;

    info!(
        model = %model, voice = %voice,
        status = %response.status(),
        "OpenAI Realtime WebSocket connected (persistent session)"
    );

    // Split into independent read / write halves
    let (mut ws_sink, mut ws_reader) = ws_stream.split();

    // ── Send session.update ────────────────────────────────────────────
    let session_update =
        json!({
        "type": "session.update",
        "session": {
            "instructions": instructions,
            "modalities": ["audio", "text"],
            "voice": voice,
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "input_audio_transcription": {
                "model": "whisper-1"
            },
            "turn_detection": {
                "type": "server_vad",
                "threshold": 0.5,
                "prefix_padding_ms": 300,
                "silence_duration_ms": 500
            }
        }
    });

    let session_update_str = session_update.to_string();
    info!(payload = %session_update_str, "session.update payload");

    ws_sink
        .send(tungstenite::Message::Text(session_update_str)).await
        .map_err(|e| anyhow::anyhow!("Failed to send session.update: {}", e))?;

    info!(voice = %voice, "session.update sent (server_vad)");

    // ── Internal channels ──────────────────────────────────────────────
    //
    //  audio_tx / audio_rx — transport_udp pushes 16 kHz PCM chunks
    //  ws_msg_tx / ws_msg_rx — reader task injects control msgs (Pong)
    //                          into the sink owned by the writer task
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(512);
    let (ws_msg_tx, mut ws_msg_rx) = mpsc::channel::<tungstenite::Message>(64);
    let control_tx = ws_msg_tx.clone();

    // ── Writer task ────────────────────────────────────────────────────
    //  Merges two sources into the single WS sink:
    //    1. audio chunks  → resample 16→24 kHz → base64 → append event
    //    2. control msgs  → forwarded as-is (e.g. Pong)
    let writer_handle = tokio::spawn(async move {
        info!("OpenAI writer task started");
        let mut audio_chunks_sent: u64 = 0;
        loop {
            tokio::select! {
                biased;

                Some(msg) = ws_msg_rx.recv() => {
                    if let Err(e) = ws_sink.send(msg).await {
                        error!("WS control send error: {}", e);
                        break;
                    }
                }

                Some(pcm_16k) = audio_rx.recv() => {
                    let pcm_16k_len = pcm_16k.len();
                    let pcm_24k = resample_16k_to_24k(&pcm_16k);
                    let pcm_24k_len = pcm_24k.len();
                    let b64 = BASE64.encode(&pcm_24k);

                    debug!(
                        pcm_16k_bytes = pcm_16k_len,
                        pcm_24k_bytes = pcm_24k_len,
                        b64_len = b64.len(),
                        "sending input_audio_buffer.append to OpenAI"
                    );

                    let event = json!({
                        "type": "input_audio_buffer.append",
                        "audio": b64,
                    });

                    if let Err(e) = ws_sink
                        .send(tungstenite::Message::Text(event.to_string()))
                        .await
                    {
                        error!("WS audio send error: {}", e);
                        break;
                    }
                    audio_chunks_sent += 1;
                }

                else => {
                    info!("OpenAI writer: all channels closed");
                    break;
                },
            }
        }
        info!(audio_chunks_sent = audio_chunks_sent, "OpenAI writer task exiting");
    });

    // ── Reader task ────────────────────────────────────────────────────
    //  Reads server events from the WS; when we get audio deltas
    //  we decode + resample 24→16 kHz + packetise as AUDIO_DOWN.
    let active_esp_reader = active_esp.clone();
    let reader_handle = tokio::spawn(async move {
        info!("👂 OpenAI reader task started (persistent session)");
        let mut out_seq: u16 = 0;
        let mut total_audio_deltas: u64 = 0;
        let mut total_audio_bytes_to_esp: u64 = 0;

        while let Some(msg_result) = ws_reader.next().await {
            let msg = match msg_result {
                Ok(m) => m,
                Err(e) => {
                    error!("WS read error: {}", e);
                    break;
                }
            };

            let text = match &msg {
                tungstenite::Message::Text(t) => {
                    // Log a truncated preview of every text frame
                    let preview: String = t.chars().take(200).collect();
                    debug!(len = t.len(), preview = %preview, "WS text frame received");
                    t.clone()
                }
                tungstenite::Message::Close(frame) => {
                    info!(frame = ?frame, "OpenAI WebSocket closed by server");
                    break;
                }
                tungstenite::Message::Ping(data) => {
                    debug!(len = data.len(), "WS Ping received");
                    let _ = ws_msg_tx.send(tungstenite::Message::Pong(data.clone())).await;
                    continue;
                }
                tungstenite::Message::Binary(data) => {
                    debug!(len = data.len(), "WS Binary frame received (unexpected)");
                    continue;
                }
                other => {
                    debug!(msg_type = ?other, "WS unknown frame type");
                    continue;
                }
            };

            let event: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Failed to parse OpenAI event: {}", e);
                    continue;
                }
            };

            let event_type = event["type"].as_str().unwrap_or("");

            match event_type {
                // ── Lifecycle ─────────────────────────────────────
                "session.created" => {
                    let session_id = event["session"]["id"].as_str().unwrap_or("?");
                    let model = event["session"]["model"].as_str().unwrap_or("?");
                    info!(
                        session_id = session_id,
                        model = model,
                        "OpenAI Realtime session created"
                    );
                    debug!(raw = %text, "session.created full payload");
                }
                "session.updated" => {
                    info!("OpenAI session config confirmed");
                    debug!(raw = %text, "session.updated full payload");
                }

                // ── Audio response: stream back to ESP ────────────
                "response.audio.delta" => {
                    if let Some(b64) = event["delta"].as_str() {
                        info!(b64_len = b64.len(), "🔊 response.audio.delta received");
                        match BASE64.decode(b64) {
                            Ok(pcm_24k) => {
                                let pcm_16k = resample_24k_to_16k(&pcm_24k);
                                let n_chunks = pcm_16k.chunks(ESP_MAX_PAYLOAD).len();

                                let current_esp = { *active_esp_reader.read().await };
                                if let Some(esp_addr) = current_esp {
                                    info!(
                                        pcm_24k_bytes = pcm_24k.len(),
                                        pcm_16k_bytes = pcm_16k.len(),
                                        n_chunks = n_chunks,
                                        esp = %esp_addr,
                                        seq = out_seq,
                                        "sending AUDIO_DOWN to ESP"
                                    );

                                    for chunk in pcm_16k.chunks(ESP_MAX_PAYLOAD) {
                                        let pkt = build_audio_down(out_seq, 0, chunk);
                                        out_seq = out_seq.wrapping_add(1);

                                        match audio_socket.send_to(&pkt, esp_addr).await {
                                            Ok(sent) => {
                                                debug!(
                                                    sent_bytes = sent,
                                                    seq = out_seq.wrapping_sub(1),
                                                    "AUDIO_DOWN packet sent"
                                                );
                                            }
                                            Err(e) => {
                                                warn!(
                                                    error = %e,
                                                    esp = %esp_addr,
                                                    "failed to send AUDIO_DOWN to ESP"
                                                );
                                            }
                                        }
                                    }
                                    total_audio_bytes_to_esp += pcm_16k.len() as u64;
                                } else {
                                    warn!(
                                        pcm_bytes = pcm_16k.len(),
                                        "⚠️ no active ESP client — dropping audio response!"
                                    );
                                }
                            }
                            Err(e) => {
                                warn!(b64_len = b64.len(), error = %e, "base64 decode error on audio delta");
                            }
                        }
                        total_audio_deltas += 1;
                    } else {
                        warn!("response.audio.delta event has no 'delta' field");
                    }
                }

                "response.audio.done" => {
                    let current_esp = { *active_esp_reader.read().await };
                    info!(
                        esp = ?current_esp,
                        total_deltas = total_audio_deltas,
                        total_bytes_to_esp = total_audio_bytes_to_esp,
                        "OpenAI audio response complete"
                    );
                    if let Some(esp_addr) = current_esp {
                        let pkt = build_control(out_seq, CTRL_STREAM_END, 0);
                        out_seq = out_seq.wrapping_add(1);
                        let _ = audio_socket.send_to(&pkt, esp_addr).await;
                    }
                }

                "response.done" => {
                    let st = event["response"]["status"].as_str().unwrap_or("?");
                    let usage = &event["response"]["usage"];
                    info!(status = st, usage = %usage, "OpenAI response.done");
                    debug!(raw = %text, "response.done full");
                }

                // ── VAD events ────────────────────────────────────
                "input_audio_buffer.speech_started" => {
                    info!("OpenAI VAD: speech started");
                }
                "input_audio_buffer.speech_stopped" => {
                    info!("OpenAI VAD: speech stopped");
                }
                "input_audio_buffer.committed" => {
                    info!("audio buffer committed");
                }

                // ── Transcripts ───────────────────────────────────
                "response.audio_transcript.delta" => {
                    if let Some(d) = event["delta"].as_str() {
                        info!(delta = d, "transcript delta");
                    }
                }
                "response.audio_transcript.done" => {
                    if let Some(t) = event["transcript"].as_str() {
                        info!(transcript = t, "AI said");
                    }
                }
                "conversation.item.input_audio_transcription.completed" => {
                    if let Some(t) = event["transcript"].as_str() {
                        info!(transcript = t, "User said");
                    }
                }

                // ── Errors ────────────────────────────────────────
                "error" => {
                    let msg = event["error"]["message"].as_str().unwrap_or("unknown");
                    let code = event["error"]["code"].as_str().unwrap_or("unknown");
                    let etype = event["error"]["type"].as_str().unwrap_or("unknown");
                    error!(
                        code = code, error_type = etype, message = msg,
                        raw = %text,
                        "❌ OpenAI error"
                    );
                }

                // everything else → log with full payload so we can spot unknown events
                other => {
                    info!(event_type = other, raw = %text, "unhandled OpenAI event");
                }
            }
        }

        info!(
            total_audio_deltas = total_audio_deltas,
            total_audio_bytes_to_esp = total_audio_bytes_to_esp,
            "OpenAI reader task exiting"
        );
    });

    Ok(OpenAiSession {
        audio_tx,
        control_tx,
        active_esp,
        reader_handle,
        writer_handle,
    })
}

// ═══════════════════════════════════════════════════════════════════════
//  Audio resampling — linear interpolation
// ═══════════════════════════════════════════════════════════════════════

/// Resample 16 kHz → 24 kHz PCM16 (×1.5) via linear interpolation.
///
/// Input / output: raw bytes, 16-bit LE, mono.
pub fn resample_16k_to_24k(pcm: &[u8]) -> Vec<u8> {
    resample(pcm, 16_000, 24_000)
}

/// Resample 24 kHz → 16 kHz PCM16 (×⅔) via linear interpolation.
///
/// Input / output: raw bytes, 16-bit LE, mono.
pub fn resample_24k_to_16k(pcm: &[u8]) -> Vec<u8> {
    resample(pcm, 24_000, 16_000)
}

/// Generic linear-interpolation resampler for 16-bit LE PCM.
fn resample(pcm: &[u8], from_rate: u64, to_rate: u64) -> Vec<u8> {
    let n_in = pcm.len() / 2;
    if n_in == 0 {
        return Vec::new();
    }

    // Parse input samples
    let src: Vec<i16> = (0..n_in)
        .map(|i| i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]))
        .collect();

    let n_out = (((n_in as u64) * to_rate) / from_rate) as usize;
    let mut out = Vec::with_capacity(n_out * 2);

    if n_out <= 1 {
        // Edge case: just copy first sample
        out.extend_from_slice(&src[0].to_le_bytes());
        return out;
    }

    for j in 0..n_out {
        let pos = ((j as f64) * ((n_in - 1) as f64)) / ((n_out - 1) as f64);
        let idx = pos as usize;
        let frac = pos - (idx as f64);

        let s = if idx + 1 < n_in {
            ((src[idx] as f64) * (1.0 - frac) + (src[idx + 1] as f64) * frac).round() as i16
        } else {
            src[n_in - 1]
        };

        out.extend_from_slice(&s.to_le_bytes());
    }

    out
}

// ═══════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resample_round_trip() {
        let n = 16_000usize;
        let mut pcm = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = (i as f64) / (n as f64);
            let s = (t * 440.0 * 2.0 * std::f64::consts::PI).sin() * 16_000.0;
            pcm.extend_from_slice(&(s as i16).to_le_bytes());
        }

        let up = resample_16k_to_24k(&pcm);
        let back = resample_24k_to_16k(&up);

        let n_orig = pcm.len() / 2;
        let n_back = back.len() / 2;
        assert!(
            ((n_orig as i64) - (n_back as i64)).abs() <= 2,
            "sample count drift: {} vs {}",
            n_orig,
            n_back
        );

        // First 100 samples should be close
        for i in 0..(100).min(n_back) {
            let a = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]);
            let b = i16::from_le_bytes([back[i * 2], back[i * 2 + 1]]);
            assert!(
                ((a as i32) - (b as i32)).abs() < 500,
                "sample {} too different: {} vs {}",
                i,
                a,
                b
            );
        }
    }

    #[test]
    fn test_resample_empty() {
        assert!(resample_16k_to_24k(&[]).is_empty());
        assert!(resample_24k_to_16k(&[]).is_empty());
    }

    #[test]
    fn test_resample_ratio() {
        // 700 samples @ 16 kHz → 1050 @ 24 kHz
        let pcm = vec![0u8; 700 * 2];
        let up = resample_16k_to_24k(&pcm);
        assert_eq!(up.len() / 2, 1050, "16→24 ratio wrong");

        // 1050 @ 24 kHz → 700 @ 16 kHz
        let down = resample_24k_to_16k(&up);
        assert_eq!(down.len() / 2, 700, "24→16 ratio wrong");
    }
}
