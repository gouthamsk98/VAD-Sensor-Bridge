use crate::config::Config;
use crate::esp_audio_protocol::*;
use crate::sensor::SensorPacket;
use crate::stats::Stats;
use crate::transport_openai::OpenAiSession;
use crate::vad::VadResult;
use crate::vad_response::VadResponsePacket;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{ Hash, Hasher };
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{ mpsc, RwLock };
use tracing::{ debug, warn, info };

/// Shared map of sensor_id → last-seen client address (for sensor port responses).
type ClientMap = Arc<RwLock<HashMap<u32, SocketAddr>>>;

/// Per-ESP-client session data: protocol state + optional OpenAI bridge.
struct EspSessionEntry {
    session: EspSession,
    /// When OpenAI Realtime is active, this holds the audio sender.
    openai_tx: Option<mpsc::Sender<Vec<u8>>>,
}

/// Shared map of ESP client address → session entry (for audio port sessions).
type SessionMap = Arc<RwLock<HashMap<SocketAddr, EspSessionEntry>>>;

/// Spawn UDP receiver tasks for dual ports: audio and sensor.
///
/// * **Audio port** – speaks the ESP audio protocol: handles session
///   lifecycle (SESSION_START / SESSION_END), accumulates PCM audio,
///   saves WAV on session completion, and forwards chunks to the VAD
///   pipeline for real-time voice-activity detection.
/// * **Sensor port** – receives sensor-vector packets, remembers the sender
///   address, and later sends back VAD results once they are computed.
pub async fn spawn_udp_receivers(
    config: &Config,
    tx: mpsc::Sender<SensorPacket>,
    vad_rx: mpsc::Receiver<VadResult>,
    stats: Arc<Stats>
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    let n_threads = config.resolved_recv_threads();
    let audio_addr = config.audio_addr();
    let sensor_addr = config.sensor_addr();
    let recv_buf_size = config.recv_buf_size;

    let mut handles = Vec::with_capacity(n_threads * 2 + 1);

    // Bind sockets
    let audio_socket = Arc::new(bind_reuseport(&audio_addr, recv_buf_size).await?);
    let sensor_socket = Arc::new(bind_reuseport(&sensor_addr, recv_buf_size).await?);

    info!(audio_addr = %audio_addr, sensor_addr = %sensor_addr, "✅ UDP dual ports bound");

    // Shared map so the response handler knows where to send VAD results
    let client_map: ClientMap = Arc::new(RwLock::new(HashMap::new()));

    // ── Response handler: forwards VAD results to sensor clients ───────
    let sensor_socket_resp = sensor_socket.clone();
    let client_map_resp = client_map.clone();
    let resp_handle = tokio::spawn(async move {
        if let Err(e) = vad_response_loop(vad_rx, sensor_socket_resp, client_map_resp).await {
            tracing::error!(error = %e, "VAD response handler failed");
        }
    });
    handles.push(resp_handle);

    // Shared session map for ESP audio clients
    let sessions: SessionMap = Arc::new(RwLock::new(HashMap::new()));
    let audio_save_dir = config.audio_save_dir.clone();

    // Spawn persistent OpenAI Realtime session once at startup
    // (avoids WebSocket handshake latency on every ESP SESSION_START)
    let persistent_oai: Option<Arc<OpenAiSession>> = if config.openai_realtime {
        let active_esp = Arc::new(RwLock::new(None));
        info!("\u{1F916} Spawning persistent OpenAI Realtime session...");
        match
            crate::transport_openai::spawn_openai_session(
                &config,
                active_esp,
                audio_socket.clone()
            ).await
        {
            Ok(session) => {
                info!("\u{1F916} Persistent OpenAI Realtime session ready — WebSocket connected");
                Some(Arc::new(session))
            }
            Err(e) => {
                warn!(error = %e, "failed to create persistent OpenAI session — continuing without");
                None
            }
        }
    } else {
        None
    };

    // ── Audio receiver threads (ESP audio protocol) ───────────────────
    for i in 0..n_threads {
        let socket = audio_socket.clone();
        let tx = tx.clone();
        let stats = stats.clone();
        let sessions = sessions.clone();
        let save_dir = audio_save_dir.clone();
        let persistent_oai = persistent_oai.clone();

        handles.push(
            tokio::spawn(async move {
                if
                    let Err(e) = esp_audio_recv_loop(
                        i,
                        socket,
                        tx,
                        stats,
                        sessions,
                        save_dir,
                        persistent_oai
                    ).await
                {
                    tracing::error!(thread = i, error = %e, "ESP audio receiver failed");
                }
            })
        );
    }

    // ── Sensor receiver threads (track client, forward for VAD) ───────
    for i in 0..n_threads {
        let socket = sensor_socket.clone();
        let tx = tx.clone();
        let stats = stats.clone();
        let cmap = client_map.clone();

        handles.push(
            tokio::spawn(async move {
                if let Err(e) = sensor_recv_loop(i, socket, tx, stats, cmap).await {
                    tracing::error!(thread = i, error = %e, "UDP sensor receiver failed");
                }
            })
        );
    }

    Ok(handles)
}

// ═══════════════════════════════════════════════════════════════════════
//  ESP Audio Protocol receiver — session lifecycle + WAV recording
// ═══════════════════════════════════════════════════════════════════════

async fn esp_audio_recv_loop(
    thread_id: usize,
    socket: Arc<UdpSocket>,
    tx: mpsc::Sender<SensorPacket>,
    stats: Arc<Stats>,
    sessions: SessionMap,
    audio_save_dir: String,
    persistent_oai: Option<Arc<OpenAiSession>>
) -> anyhow::Result<()> {
    debug!(thread = thread_id, "ESP audio receiver started");

    let mut buf = vec![0u8; ESP_HEADER_SIZE + ESP_MAX_PAYLOAD + 64];

    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(thread = thread_id, error = %e, "UDP audio recv error");
                stats.record_recv_error();
                continue;
            }
        };

        stats.record_recv(len);

        let pkt = match EspPacket::parse(&buf[..len]) {
            Some(p) => p,
            None => {
                stats.record_parse_error();
                continue;
            }
        };

        match pkt.pkt_type {
            // ── Heartbeat: mirror sequence number back ────────────
            PKT_HEARTBEAT => {
                let reply = build_heartbeat(pkt.seq_num);
                let _ = socket.send_to(&reply, src).await;
                debug!(thread = thread_id, src = %src, seq = pkt.seq_num, "💓 heartbeat");
            }

            // ── Control messages: drive session state machine ─────
            PKT_CONTROL => {
                if let Some(cmd) = pkt.control_cmd() {
                    handle_esp_control(
                        thread_id,
                        cmd,
                        &pkt,
                        src,
                        &socket,
                        &sessions,
                        &tx,
                        &stats,
                        &audio_save_dir,
                        &persistent_oai
                    ).await;
                }
            }

            // ── Audio upstream: accumulate + forward to VAD + OpenAI ─
            PKT_AUDIO_UP => {
                let (should_forward, openai_tx) = {
                    let mut map = sessions.write().await;
                    let entry = map.entry(src).or_insert_with(|| {
                        let mut s = EspSession::new(src);
                        s.state = SessionState::Receiving;
                        warn!(thread = thread_id, src = %src,
                              "⚡ auto-started session on first audio packet (NO OpenAI tx!)");
                        EspSessionEntry { session: s, openai_tx: None }
                    });

                    if entry.session.state == SessionState::Receiving {
                        entry.session.record_audio(pkt.seq_num, &pkt.payload);
                        (true, entry.openai_tx.clone())
                    } else {
                        debug!(src = %src, state = %entry.session.state,
                               "audio packet ignored — session not receiving");
                        (false, None)
                    }
                };

                if should_forward && !pkt.payload.is_empty() {
                    // Forward to VAD pipeline
                    let sensor_pkt = esp_audio_to_sensor_packet(src, pkt.seq_num, &pkt.payload);
                    if tx.try_send(sensor_pkt).is_err() {
                        stats.record_channel_drop();
                    }

                    // Forward to OpenAI Realtime (if active)
                    if let Some(ref oai_tx) = openai_tx {
                        let payload_len = pkt.payload.len();
                        match oai_tx.try_send(pkt.payload.clone()) {
                            Ok(()) => {
                                debug!(
                                    src = %src, seq = pkt.seq_num, bytes = payload_len,
                                    "🔄 audio forwarded to OpenAI tx"
                                );
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                warn!(
                                    src = %src,
                                    "⚠️ OpenAI tx channel full — dropping audio chunk"
                                );
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                warn!(
                                    src = %src,
                                    "⚠️ OpenAI tx channel closed — session may have ended"
                                );
                            }
                        }
                    } else {
                        debug!(src = %src, "no OpenAI tx — audio not forwarded to OpenAI");
                    }
                }

                // If the END flag is set, treat it like SESSION_END
                if pkt.is_end() {
                    handle_esp_control(
                        thread_id,
                        CTRL_SESSION_END,
                        &pkt,
                        src,
                        &socket,
                        &sessions,
                        &tx,
                        &stats,
                        &audio_save_dir,
                        &persistent_oai
                    ).await;
                }
            }

            other => {
                debug!(thread = thread_id, src = %src, pkt_type = other,
                       "unexpected ESP packet type");
            }
        }
    }
}

/// Handle a single ESP control command within a session context.
async fn handle_esp_control(
    thread_id: usize,
    cmd: u8,
    pkt: &EspPacket,
    src: SocketAddr,
    socket: &Arc<UdpSocket>,
    sessions: &SessionMap,
    _tx: &mpsc::Sender<SensorPacket>,
    _stats: &Arc<Stats>,
    audio_save_dir: &str,
    persistent_oai: &Option<Arc<OpenAiSession>>
) {
    match cmd {
        // ── SESSION_START: create / reset session, reply SERVER_READY ─
        CTRL_SESSION_START => {
            // Wire the persistent OpenAI session to this ESP client
            // (no WebSocket handshake — session was created at server start)
            let openai_tx = if let Some(ref oai) = persistent_oai {
                oai.set_active_esp(src).await;
                oai.clear_input_buffer().await;
                info!(src = %src, "🤖 wired ESP client to persistent OpenAI session");
                Some(oai.audio_tx.clone())
            } else {
                debug!(src = %src, "OpenAI Realtime not enabled — skipping");
                None
            };

            {
                let mut map = sessions.write().await;
                let entry = map.entry(src).or_insert_with(|| EspSessionEntry {
                    session: EspSession::new(src),
                    openai_tx: None,
                });
                entry.session.reset();
                entry.session.state = SessionState::Receiving;
                let has_openai = openai_tx.is_some();
                entry.openai_tx = openai_tx;
                info!(src = %src, has_openai_tx = has_openai, "session entry updated");
            }

            let reply = build_control(pkt.seq_num, CTRL_SERVER_READY, 0);
            let _ = socket.send_to(&reply, src).await;
            info!(thread = thread_id, src = %src,
                  "📞 ESP session started → SERVER_READY sent");
        }

        // ── SESSION_END: save WAV, send ACK, reset ──────────────────
        CTRL_SESSION_END => {
            let session_data = {
                let mut map = sessions.write().await;
                if let Some(entry) = map.get_mut(&src) {
                    if entry.session.state == SessionState::Receiving {
                        entry.session.state = SessionState::Processing;
                        // Disconnect from persistent OpenAI session
                        // (WebSocket stays alive for the next ESP session)
                        entry.openai_tx = None;
                        Some((
                            entry.session.audio_buffer.clone(),
                            entry.session.audio_packets,
                            entry.session.audio_bytes,
                            entry.session.packets_lost,
                            entry.session.elapsed(),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            // Commit any remaining audio in the OpenAI buffer so it
            // gets processed even if server_vad hasn't auto-committed.
            // NOTE: we do NOT clear active_esp here — the response audio
            // may arrive after SESSION_END and must still route to this ESP.
            // active_esp gets overwritten on the next SESSION_START.
            if let Some(ref oai) = persistent_oai {
                oai.commit_input_buffer().await;
                info!(src = %src, "📝 committed OpenAI audio buffer on session end");
            }

            if let Some((audio_buf, pkts, bytes, lost, duration)) = session_data {
                let audio_secs = (bytes as f64) / (16_000.0 * 2.0);
                info!(
                    src = %src,
                    packets = pkts,
                    bytes = bytes,
                    lost = lost,
                    duration_secs = format!("{:.1}", duration.as_secs_f64()),
                    audio_secs = format!("{:.1}", audio_secs),
                    "📴 ESP session ended"
                );

                // Persist the accumulated audio as WAV
                if !audio_buf.is_empty() {
                    match save_session_wav(audio_save_dir, src, &audio_buf).await {
                        Ok(path) => info!(path = %path, "💾 session audio saved"),
                        Err(e) => warn!(error = %e, "failed to save session audio"),
                    }
                }

                // Send ACK
                let reply = build_control(pkt.seq_num, CTRL_ACK, 0);
                let _ = socket.send_to(&reply, src).await;

                // Reset to idle
                {
                    let mut map = sessions.write().await;
                    if let Some(entry) = map.get_mut(&src) {
                        entry.session.reset();
                        entry.openai_tx = None;
                    }
                }
            } else {
                // No active receiving session — still ACK
                let reply = build_control(pkt.seq_num, CTRL_ACK, 0);
                let _ = socket.send_to(&reply, src).await;
            }
        }

        // ── CANCEL: discard session, ACK ────────────────────────────
        CTRL_CANCEL => {
            {
                let mut map = sessions.write().await;
                if let Some(entry) = map.get_mut(&src) {
                    info!(src = %src, pkts = entry.session.audio_packets,
                          "🚫 ESP session cancelled");
                    entry.session.reset();
                    entry.openai_tx = None;
                }
            }
            // Detach from persistent OpenAI session + discard buffered audio
            if let Some(ref oai) = persistent_oai {
                oai.clear_active_esp().await;
                oai.clear_input_buffer().await;
            }
            let reply = build_control(pkt.seq_num, CTRL_ACK, 0);
            let _ = socket.send_to(&reply, src).await;
        }

        other => {
            debug!(src = %src, cmd = other, "unhandled ESP control command");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Helpers: SensorPacket bridge + WAV writer
// ═══════════════════════════════════════════════════════════════════════

/// Convert an ESP audio payload into a [`SensorPacket`] so it can travel
/// through the existing VAD processing pipeline.
fn esp_audio_to_sensor_packet(src: SocketAddr, seq_num: u16, payload: &[u8]) -> SensorPacket {
    // Derive a stable sensor_id from the source address.
    let mut hasher = DefaultHasher::new();
    src.hash(&mut hasher);
    let sensor_id = (hasher.finish() & 0xffff_ffff) as u32;

    SensorPacket {
        sensor_id,
        timestamp_us: std::time::SystemTime
            ::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64,
        data_type: crate::sensor::DATA_TYPE_AUDIO,
        seq: seq_num as u64,
        payload: payload.to_vec(),
    }
}

/// Write the accumulated PCM buffer to a WAV file (16 kHz, 16-bit, mono).
async fn save_session_wav(dir: &str, src: SocketAddr, pcm_data: &[u8]) -> anyhow::Result<String> {
    if pcm_data.is_empty() {
        anyhow::bail!("no audio data to save");
    }

    tokio::fs::create_dir_all(dir).await?;

    let epoch_secs = std::time::SystemTime
        ::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ip_str = src.ip().to_string().replace('.', "_").replace(':', "_");
    let filename = format!("esp_{}_{}.wav", ip_str, epoch_secs);
    let path = format!("{}/{}", dir, filename);

    let data_len = pcm_data.len() as u32;
    let sample_rate: u32 = 16_000;
    let bits_per_sample: u16 = 16;
    let channels: u16 = 1;
    let byte_rate = sample_rate * ((bits_per_sample as u32) / 8) * (channels as u32);
    let block_align = channels * (bits_per_sample / 8);

    let mut wav = Vec::with_capacity(44 + pcm_data.len());
    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    // fmt sub-chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&(16u32).to_le_bytes()); // sub-chunk size
    wav.extend_from_slice(&(1u16).to_le_bytes()); // PCM format
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    // data sub-chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm_data);

    tokio::fs::write(&path, &wav).await?;
    Ok(path)
}

// ═══════════════════════════════════════════════════════════════════════
//  Sensor receiver — remembers client addr, forwards packet for VAD
// ═══════════════════════════════════════════════════════════════════════

async fn sensor_recv_loop(
    thread_id: usize,
    socket: Arc<UdpSocket>,
    tx: mpsc::Sender<SensorPacket>,
    stats: Arc<Stats>,
    client_map: ClientMap
) -> anyhow::Result<()> {
    debug!(thread = thread_id, "UDP sensor receiver started");

    let mut buf = vec![0u8; 65535];

    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(thread = thread_id, error = %e, "UDP sensor recv error");
                stats.record_recv_error();
                continue;
            }
        };

        stats.record_recv(len);

        let packet = match SensorPacket::parse(&buf[..len]) {
            Some(p) => p,
            None => {
                stats.record_parse_error();
                continue;
            }
        };

        // Remember the sender so we can send VAD results back later
        {
            let mut map = client_map.write().await;
            map.insert(packet.sensor_id, src);
        }

        debug!(
            thread = thread_id,
            sensor_id = packet.sensor_id,
            seq = packet.seq,
            data_type = packet.data_type,
            src = %src,
            "📊 sensor packet received"
        );

        if tx.try_send(packet).is_err() {
            stats.record_channel_drop();
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Response handler — sends VAD results back to sensor clients
// ═══════════════════════════════════════════════════════════════════════

async fn vad_response_loop(
    mut vad_rx: mpsc::Receiver<VadResult>,
    sensor_socket: Arc<UdpSocket>,
    client_map: ClientMap
) -> anyhow::Result<()> {
    debug!("VAD response handler started");

    while let Some(result) = vad_rx.recv().await {
        // Only send VAD results back for sensor/emotional packets
        if result.kind != crate::vad::VadKind::Audio {
            let response = VadResponsePacket::from_vad_result(&result);
            let bytes = response.to_bytes();

            let dst = {
                let map = client_map.read().await;
                map.get(&result.sensor_id).copied()
            };

            if let Some(addr) = dst {
                if let Err(e) = sensor_socket.send_to(&bytes, addr).await {
                    warn!(error = %e, dst = %addr, "failed to send VAD response");
                } else {
                    debug!(
                        sensor_id = result.sensor_id,
                        seq = result.seq,
                        dst = %addr,
                        "📤 VAD result sent to sensor client"
                    );
                }
            } else {
                debug!(
                    sensor_id = result.sensor_id,
                    "no known client address for sensor, skipping response"
                );
            }
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
//  Socket helpers
// ═══════════════════════════════════════════════════════════════════════

async fn bind_reuseport(addr: &str, recv_buf_size: usize) -> anyhow::Result<UdpSocket> {
    use std::net::SocketAddr;
    let parsed: SocketAddr = addr.parse()?;

    let socket = socket2::Socket::new(
        match parsed {
            SocketAddr::V4(_) => socket2::Domain::IPV4,
            SocketAddr::V6(_) => socket2::Domain::IPV6,
        },
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP)
    )?;

    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.set_recv_buffer_size(recv_buf_size)?;
    socket.bind(&parsed.into())?;

    let std_socket: std::net::UdpSocket = socket.into();
    Ok(UdpSocket::from_std(std_socket)?)
}
