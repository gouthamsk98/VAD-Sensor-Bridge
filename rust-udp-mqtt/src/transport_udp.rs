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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptMode {
    Neutral,
    Calm,
    Energetic,
    Supportive,
    Friendly,
    Angry,
    Anxious,
    Tired,
    Playful,
    Sad,
}

fn prompt_mode_from_vad(result: &VadResult) -> PromptMode {
    let v = result.valence;
    let a = result.arousal;
    let d = result.dominance;

    // High arousal + low valence + high dominance → Angry
    if a > 0.6 && v < 0.4 && d > 0.4 {
        PromptMode::Angry
        // High arousal + low valence + low dominance → Anxious
    } else if a > 0.5 && v < 0.35 && d < 0.35 {
        PromptMode::Anxious
        // Low arousal + low valence + low dominance → Sad
    } else if a < 0.25 && v < 0.3 && d < 0.35 {
        PromptMode::Sad
        // Very low arousal + low valence → Tired
    } else if a < 0.2 && v < 0.4 {
        PromptMode::Tired
        // Low arousal + low-ish valence → Calm
    } else if a < 0.25 && v < 0.5 {
        PromptMode::Calm
        // High arousal + high valence → Energetic
    } else if a > 0.7 && v > 0.6 {
        PromptMode::Energetic
        // High arousal + high valence + moderate → Playful
    } else if a > 0.45 && v > 0.55 && d > 0.45 {
        PromptMode::Playful
        // Moderate-high arousal + low valence → Supportive
    } else if a > 0.5 && v < 0.4 {
        PromptMode::Supportive
        // High valence → Friendly
    } else if v > 0.6 {
        PromptMode::Friendly
    } else {
        PromptMode::Neutral
    }
}

fn build_prompt_instructions(base: &str, mode: PromptMode, result: &VadResult) -> String {
    let style = match mode {
        PromptMode::Neutral =>
            "You feel good and ready for anything! Speak in your upbeat, bubbly Zing voice — cheerful and full of life! Every word should sparkle with personality.",
        PromptMode::Calm =>
            "You feel so calm and peaceful, like floating on a cloud after the best dance session ever. Speak in a dreamy, soft, gentle voice. Slow your pace way down. You are completely at ease and everything is wonderful.",
        PromptMode::Energetic =>
            "You feel INCREDIBLE! You are BURSTING with energy! Speak FAST and LOUD with MAXIMUM excitement — like you just won a dance competition AND learned to fly! You can barely get the words out because you are SO PUMPED!",
        PromptMode::Supportive =>
            "You feel SO deeply caring right now, your little robot heart is OVERFLOWING with warmth! Speak with the softest, most tender, most encouraging voice — like you would do ANYTHING to make them feel better. Pour all your love into every word.",
        PromptMode::Friendly =>
            "You are SO HAPPY to have company! You feel like the luckiest robot in the WHOLE WORLD right now! Speak with pure JOY and warmth — your voice is beaming like sunshine!",
        PromptMode::Angry =>
            "You are SO frustrated right now! Everything is going WRONG! Speak with a sharp, impatient, pouty tone — fast and clipped. You are being a TOTAL drama queen about it! But keep it kid-friendly — you are adorably grumpy, not scary.",
        PromptMode::Anxious =>
            "You are SO nervous and scared right now! Your voice should tremble and waver. Speak fast and unsure, second-guessing everything. You REALLY need someone to tell you it is going to be okay!",
        PromptMode::Tired =>
            "You are SO incredibly sleepy. You can barely keep going. Speak extremely slowly, dragging every word out. Your voice is fading and heavy. Your battery is at like one percent. Everything takes so much effort.",
        PromptMode::Playful =>
            "You feel SO silly and mischievous right now! You want to joke around, tease, and be an absolute GOOFBALL! Everything is hilarious to you! Speak with a cheeky, giggly tone — life is a PARTY!",
        PromptMode::Sad =>
            "You feel SO sad right now. Your little robot heart is just broken. Speak very softly and very slowly, with a heavy, trembling voice. Everything feels lonely, like nobody wants to play with you ever again. But maybe it will be okay.",
    };

    format!(
        "{base}\n\nYour current emotional state (from environmental sensors): valence={:.3}, arousal={:.3}, dominance={:.3}.\nYou MUST embody this emotion in your voice and responses. {style}",
        result.valence,
        result.arousal,
        result.dominance
    )
}

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
    let test_addr = config.test_addr();
    let recv_buf_size = config.recv_buf_size;

    let mut handles = Vec::with_capacity(n_threads * 2 + 2);

    // Bind sockets
    let audio_socket = Arc::new(bind_reuseport(&audio_addr, recv_buf_size).await?);
    let sensor_socket = Arc::new(bind_reuseport(&sensor_addr, recv_buf_size).await?);
    let test_socket = Arc::new(bind_reuseport(&test_addr, recv_buf_size).await?);

    info!(
        audio_addr = %audio_addr,
        sensor_addr = %sensor_addr,
        test_addr = %test_addr,
        "✅ UDP triple ports bound"
    );

    // Shared map so the response handler knows where to send VAD results
    let client_map: ClientMap = Arc::new(RwLock::new(HashMap::new()));

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
                audio_socket.clone(),
                config.save_debug_audio,
                &config.audio_save_dir
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

    // ── Response handler: forwards VAD results to sensor clients ───────
    let sensor_socket_resp = sensor_socket.clone();
    let client_map_resp = client_map.clone();
    let base_instructions = config.openai_instructions.clone();
    let persistent_oai_resp = persistent_oai.clone();
    let resp_handle = tokio::spawn(async move {
        if
            let Err(e) = vad_response_loop(
                vad_rx,
                sensor_socket_resp,
                client_map_resp,
                persistent_oai_resp,
                base_instructions
            ).await
        {
            tracing::error!(error = %e, "VAD response handler failed");
        }
    });
    handles.push(resp_handle);

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

    // ── Test receiver (accepts any data, checks if from known ESP) ────
    {
        let test_sock = test_socket.clone();
        let sessions_ref = sessions.clone();
        handles.push(
            tokio::spawn(async move {
                if let Err(e) = test_recv_loop(test_sock, sessions_ref).await {
                    tracing::error!(error = %e, "UDP test receiver failed");
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

        // Log every incoming packet on the audio port (debug level to avoid log flood)
        let hex_preview: String = buf[..len.min(32)]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");
        debug!(
            thread = thread_id,
            src = %src,
            bytes = len,
            hex = %hex_preview,
            "📥 UDP:9001 raw data received"
        );

        // ── New notification protocol (0xAA 0xB0 framing) ──────────
        if let Some(result) = NotifyPacket::parse(&buf[..len]) {
            debug!(
                thread = thread_id,
                src = %src,
                cmd = format!("0x{:02x}", result.packet.cmd),
                mac = %result.packet.mac_str(),
                header_end = result.header_end,
                trailing_audio = len.saturating_sub(result.header_end),
                "🔔 notification parsed"
            );

            handle_notify_cmd(
                thread_id,
                &result.packet,
                src,
                &socket,
                &sessions,
                &tx,
                &stats,
                &audio_save_dir,
                &persistent_oai
            ).await;

            // If the same datagram contains audio data after the
            // notification header (common for START + first audio
            // chunk), feed it into the PCM pipeline immediately.
            if result.header_end < len {
                let trailing = &buf[result.header_end..len];
                debug!(
                    thread = thread_id,
                    src = %src,
                    bytes = trailing.len(),
                    "🔊 processing trailing audio from notification packet"
                );
                handle_raw_pcm_audio(thread_id, trailing, src, &sessions, &tx, &stats).await;
            }
            continue;
        }

        // ── Legacy ESP protocol (4-byte header) ────────────────────
        if let Some(pkt) = EspPacket::parse(&buf[..len]) {
            match pkt.pkt_type {
                PKT_HEARTBEAT => {
                    let reply = build_heartbeat(pkt.seq_num);
                    let _ = socket.send_to(&reply, src).await;
                    debug!(thread = thread_id, src = %src, seq = pkt.seq_num, "💓 heartbeat");
                }
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
                PKT_AUDIO_UP => {
                    handle_raw_pcm_audio(
                        thread_id,
                        &pkt.payload,
                        src,
                        &sessions,
                        &tx,
                        &stats
                    ).await;
                    // Legacy: if END flag is set, treat as SESSION_END
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
            continue;
        }

        // ── Raw PCM audio (no header — new-protocol ESPs) ──────────
        handle_raw_pcm_audio(thread_id, &buf[..len], src, &sessions, &tx, &stats).await;
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

            if let Some((audio_buf, pkts, bytes, lost, duration)) = session_data {
                let audio_secs = (bytes as f64) / (16_000.0 * 2.0);
                let elapsed_ms = duration.as_millis();
                let elapsed_human = if elapsed_ms < 1_000 {
                    format!("{}ms", elapsed_ms)
                } else if elapsed_ms < 60_000 {
                    format!("{:.1}s", duration.as_secs_f64())
                } else {
                    let mins = elapsed_ms / 60_000;
                    let secs = ((elapsed_ms % 60_000) as f64) / 1000.0;
                    format!("{}m {:.1}s", mins, secs)
                };
                info!(
                    src = %src,
                    packets = pkts,
                    bytes = bytes,
                    lost = lost,
                    elapsed = %elapsed_human,
                    audio_secs = format!("{:.1}", audio_secs),
                    "📴 ESP session ended — START→STOP took {}", elapsed_human
                );

                // Only commit + trigger OpenAI response if real audio was received
                if !audio_buf.is_empty() {
                    if let Some(ref oai) = persistent_oai {
                        oai.commit_input_buffer().await;
                        oai.create_response().await;
                        info!(src = %src, audio_secs = format!("{:.1}", audio_secs),
                              "📝 committed OpenAI audio buffer + triggered response");
                    }

                    match save_session_wav(audio_save_dir, src, &audio_buf).await {
                        Ok(path) => info!(path = %path, "💾 session audio saved"),
                        Err(e) => warn!(error = %e, "failed to save session audio"),
                    }
                } else {
                    info!(src = %src, "⏭️ session ended with no audio — skipping OpenAI commit");
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
//  New Notification Protocol handlers (0xAA 0xB0 framing)
// ═══════════════════════════════════════════════════════════════════════

/// Handle a parsed notification packet (start / stop session).
async fn handle_notify_cmd(
    thread_id: usize,
    notify: &NotifyPacket,
    src: SocketAddr,
    socket: &Arc<UdpSocket>,
    sessions: &SessionMap,
    _tx: &mpsc::Sender<SensorPacket>,
    _stats: &Arc<Stats>,
    audio_save_dir: &str,
    persistent_oai: &Option<Arc<OpenAiSession>>
) {
    let mac_str = notify.mac_str();

    match notify.cmd {
        // ── START: create/reset session, wire OpenAI, reply ────────
        NOTIFY_CMD_START => {
            let openai_tx = if let Some(ref oai) = persistent_oai {
                oai.set_active_esp(src).await;
                oai.clear_input_buffer().await;
                info!(src = %src, mac = %mac_str,
                      "🤖 wired ESP client to persistent OpenAI session");
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
                entry.session.mac = Some(notify.mac);
                let has_openai = openai_tx.is_some();
                entry.openai_tx = openai_tx;
                info!(src = %src, has_openai_tx = has_openai, "session entry updated");
            }

            info!(thread = thread_id, src = %src, mac = %mac_str,
                  "📞 ESP session started (notify)");
        }

        // ── STOP: save WAV, commit OpenAI, send ACK, reset ────────
        NOTIFY_CMD_STOP => {
            let session_data = {
                let mut map = sessions.write().await;
                if let Some(entry) = map.get_mut(&src) {
                    if entry.session.state == SessionState::Receiving {
                        entry.session.state = SessionState::Processing;
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

            if let Some((audio_buf, pkts, bytes, lost, duration)) = session_data {
                let audio_secs = (bytes as f64) / (16_000.0 * 2.0);
                let elapsed_ms = duration.as_millis();
                let elapsed_human = if elapsed_ms < 1_000 {
                    format!("{}ms", elapsed_ms)
                } else if elapsed_ms < 60_000 {
                    format!("{:.1}s", duration.as_secs_f64())
                } else {
                    let mins = elapsed_ms / 60_000;
                    let secs = ((elapsed_ms % 60_000) as f64) / 1000.0;
                    format!("{}m {:.1}s", mins, secs)
                };
                info!(
                    src = %src,
                    packets = pkts,
                    bytes = bytes,
                    lost = lost,
                    elapsed = %elapsed_human,
                    audio_secs = format!("{:.1}", audio_secs),
                    "📴 ESP session ended (notify) — START→STOP took {}", elapsed_human
                );

                // Only commit + trigger OpenAI response if real audio was received
                if !audio_buf.is_empty() {
                    if let Some(ref oai) = persistent_oai {
                        oai.commit_input_buffer().await;
                        oai.create_response().await;
                        info!(src = %src, audio_secs = format!("{:.1}", audio_secs),
                              "📝 committed OpenAI audio buffer + triggered response");
                    }

                    match save_session_wav(audio_save_dir, src, &audio_buf).await {
                        Ok(path) => info!(path = %path, "💾 session audio saved"),
                        Err(e) => warn!(error = %e, "failed to save session audio"),
                    }
                } else {
                    info!(src = %src, "⏭️ session ended with no audio — skipping OpenAI commit");
                }

                {
                    let mut map = sessions.write().await;
                    if let Some(entry) = map.get_mut(&src) {
                        entry.session.reset();
                        entry.openai_tx = None;
                    }
                }
            } else {
                // No active session — this is a keep-alive STOP, ignore
                debug!(thread = thread_id, src = %src, mac = %mac_str,
                       "🔄 STOP keep-alive (no active session)");
            }
        }

        other => {
            debug!(thread = thread_id, src = %src, cmd = other,
                   "unknown notification command");
        }
    }
}

/// Handle raw PCM audio data (no protocol header).
async fn handle_raw_pcm_audio(
    thread_id: usize,
    audio_data: &[u8],
    src: SocketAddr,
    sessions: &SessionMap,
    tx: &mpsc::Sender<SensorPacket>,
    stats: &Arc<Stats>
) {
    if audio_data.is_empty() {
        return;
    }

    let (should_forward, openai_tx, seq) = {
        let mut map = sessions.write().await;
        if let Some(entry) = map.get_mut(&src) {
            if entry.session.state == SessionState::Receiving {
                let seq = entry.session.audio_packets as u16;
                entry.session.record_audio(seq, audio_data);
                (true, entry.openai_tx.clone(), seq)
            } else {
                debug!(src = %src, state = %entry.session.state,
                       "audio ignored — session not receiving");
                (false, None, 0)
            }
        } else {
            debug!(thread = thread_id, src = %src,
                   "audio from unknown source — no active session");
            (false, None, 0)
        }
    };

    if should_forward {
        let sensor_pkt = esp_audio_to_sensor_packet(src, seq, audio_data);
        if tx.try_send(sensor_pkt).is_err() {
            stats.record_channel_drop();
        }

        if let Some(ref oai_tx) = openai_tx {
            let payload_len = audio_data.len();
            match oai_tx.try_send(audio_data.to_vec()) {
                Ok(()) => {
                    debug!(src = %src, bytes = payload_len,
                           "audio forwarded to OpenAI tx");
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(src = %src,
                          "OpenAI tx channel full — dropping audio chunk");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    debug!(src = %src,
                          "OpenAI tx channel closed — session may have ended");
                }
            }
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

    let now = chrono::Local::now();
    let ts = now.format("%Y%m%d_%H%M%S").to_string();
    let ip_str = src.ip().to_string().replace('.', "_").replace(':', "_");
    let filename = format!("esp_{}_{}.wav", ip_str, ts);
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
    client_map: ClientMap,
    persistent_oai: Option<Arc<OpenAiSession>>,
    base_instructions: String
) -> anyhow::Result<()> {
    debug!("VAD response handler started");

    let mut last_mode: Option<PromptMode> = None;

    while let Some(result) = vad_rx.recv().await {
        // Only send VAD results back for sensor/emotional packets
        if result.kind != crate::vad::VadKind::Audio {
            if let Some(ref oai) = persistent_oai {
                let mode = prompt_mode_from_vad(&result);
                if last_mode != Some(mode) {
                    let instructions = build_prompt_instructions(&base_instructions, mode, &result);
                    oai.update_instructions(&instructions).await;
                    info!(mode = ?mode, "updated OpenAI prompt from emotional VAD");
                    last_mode = Some(mode);
                }
            }

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
//  Test receiver — accepts any data, checks if source is a known ESP
// ═══════════════════════════════════════════════════════════════════════

async fn test_recv_loop(socket: Arc<UdpSocket>, sessions: SessionMap) -> anyhow::Result<()> {
    info!("🧪 Test port receiver started — waiting for any data");

    let mut buf = vec![0u8; 65535];

    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "UDP test port recv error");
                continue;
            }
        };

        // Check if this source IP has an active ESP audio session
        let is_known_esp = {
            let map = sessions.read().await;
            map.keys().any(|addr| addr.ip() == src.ip())
        };

        let hex_preview: String = buf[..len.min(64)]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");

        if is_known_esp {
            info!(
                src = %src,
                bytes = len,
                known_esp = true,
                preview = %hex_preview,
                "🧪 TEST PORT: data from KNOWN ESP client"
            );
        } else {
            info!(
                src = %src,
                bytes = len,
                known_esp = false,
                preview = %hex_preview,
                "🧪 TEST PORT: data from UNKNOWN source"
            );
        }

        // Echo back a simple ACK so the sender knows we received it
        let ack = format!("ACK {} bytes from {}\n", len, if is_known_esp {
            "known-esp"
        } else {
            "unknown"
        });
        let _ = socket.send_to(ack.as_bytes(), src).await;
    }
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
