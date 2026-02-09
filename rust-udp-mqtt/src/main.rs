mod config;
mod mqtt_publisher;
mod sensor;
mod stats;
mod udp_receiver;

use clap::Parser;
use config::Config;
use stats::Stats;
use tokio::sync::mpsc;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize structured logging
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
        udp_addr = config.udp_addr(),
        mqtt = format!("{}:{}", config.mqtt_host, config.mqtt_port),
        udp_threads = config.resolved_udp_threads(),
        channel_cap = config.channel_capacity,
        "🚀 vad-sensor-bridge starting"
    );

    // Lock-free stats counters
    let stats = Stats::new();

    // Bounded MPSC channel: UDP receivers → MQTT publisher
    let (tx, rx) = mpsc::channel(config.channel_capacity);

    // Spawn stats reporter
    let stats_clone = stats.clone();
    let stats_interval = config.stats_interval_secs;
    tokio::spawn(async move {
        stats::stats_reporter(stats_clone, stats_interval).await;
    });

    // Spawn MQTT publisher (must start before UDP so channel is ready)
    let (_pub_handle, _el_handle) = mqtt_publisher::spawn_mqtt_publisher(
        &config,
        rx,
        stats.clone()
    ).await?;

    // Spawn UDP receivers (multi-thread with SO_REUSEPORT)
    let udp_handles = udp_receiver::spawn_udp_receivers(&config, tx, stats.clone()).await?;

    info!("✅ All systems go — listening for sensor data");

    // Wait for any task to finish (shouldn't happen normally)
    for h in udp_handles {
        h.await?;
    }

    Ok(())
}
