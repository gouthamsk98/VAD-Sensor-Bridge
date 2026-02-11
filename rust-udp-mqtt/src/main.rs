mod config;
mod sensor;
mod stats;
mod vad;
mod transport_udp;
mod transport_tcp;
mod transport_mqtt;

use clap::Parser;
use config::{ Config, Transport };
use stats::Stats;
use tokio::sync::mpsc;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber
        ::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter
                ::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .with_target(false)
        .with_thread_ids(true)
        .with_ansi(atty::is(atty::Stream::Stderr))
        .init();

    let config = Config::parse();

    info!(
        transport = %config.transport,
        listen = config.listen_addr(),
        recv_threads = config.resolved_recv_threads(),
        proc_threads = config.resolved_proc_threads(),
        channel_cap = config.channel_capacity,
        "🚀 vad-sensor-bridge starting"
    );

    let stats = Stats::new();

    // Channel: transport receivers → VAD processors
    let (tx, rx) = mpsc::channel(config.channel_capacity);

    // Spawn stats reporter
    let transport_name = config.transport.to_string();
    let stats_clone = stats.clone();
    let stats_interval = config.stats_interval_secs;
    tokio::spawn(async move {
        stats::stats_reporter(stats_clone, stats_interval, &transport_name).await;
    });

    // Spawn VAD processor workers
    let proc_threads = config.resolved_proc_threads();
    let rx = std::sync::Arc::new(tokio::sync::Mutex::new(rx));
    for i in 0..proc_threads {
        let rx = rx.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            loop {
                let packet = {
                    let mut guard = rx.lock().await;
                    guard.recv().await
                };
                match packet {
                    Some(pkt) => {
                        let result = vad::process_packet(&pkt);
                        match result.kind {
                            vad::VadKind::Audio => {
                                info!(
                                    sensor_id = result.sensor_id,
                                    seq = result.seq,
                                    kind = "audio",
                                    is_active = result.is_active,
                                    energy = format!("{:.2}", result.energy),
                                    threshold = format!("{:.2}", result.threshold),
                                    "🎙️  VAD result"
                                );
                            }
                            vad::VadKind::Emotional => {
                                info!(
                                    sensor_id = result.sensor_id,
                                    seq = result.seq,
                                    kind = "emotional",
                                    is_active = result.is_active,
                                    valence = format!("{:.3}", result.valence),
                                    arousal = format!("{:.3}", result.arousal),
                                    dominance = format!("{:.3}", result.dominance),
                                    "💡 VAD result"
                                );
                            }
                        }
                        stats.record_processed(result.is_active);
                    }
                    None => {
                        break;
                    }
                }
            }
            tracing::debug!(worker = i, "VAD processor stopped");
        });
    }

    // Spawn transport-specific receivers
    match config.transport {
        Transport::Udp => {
            let handles = transport_udp::spawn_udp_receivers(&config, tx, stats.clone()).await?;
            info!("✅ All systems go — listening for sensor data via UDP");
            for h in handles {
                h.await?;
            }
        }
        Transport::Tcp => {
            let handle = transport_tcp::spawn_tcp_receiver(&config, tx, stats.clone()).await?;
            info!("✅ All systems go — listening for sensor data via TCP");
            handle.await?;
        }
        Transport::Mqtt => {
            let (handle, _) = transport_mqtt::spawn_mqtt_subscriber(
                &config,
                tx,
                stats.clone()
            ).await?;
            info!("✅ All systems go — listening for sensor data via MQTT");
            handle.await?;
        }
    }

    Ok(())
}
