use crate::config::Config;
use crate::stats::Stats;
use crate::udp_receiver::MqttMessage;
use rumqttc::{ AsyncClient, EventLoop, MqttOptions, QoS };
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{ debug, error, info, warn };

/// Spawn MQTT client: one connection, QoS 0 fire-and-forget.
/// Returns the publisher task handle and the eventloop task handle.
pub async fn spawn_mqtt_publisher(
    config: &Config,
    mut rx: mpsc::Receiver<MqttMessage>,
    stats: Arc<Stats>
) -> anyhow::Result<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)> {
    let mut mqtt_opts = MqttOptions::new(
        &config.mqtt_client_id,
        &config.mqtt_host,
        config.mqtt_port
    );

    mqtt_opts.set_keep_alive(Duration::from_secs(config.mqtt_keep_alive_secs as u64));

    // Large in-flight cap for QoS 0 — we don't wait for ACKs
    mqtt_opts.set_inflight(u16::MAX);

    // Clean session — no persistent state needed for QoS 0
    mqtt_opts.set_clean_session(true);

    let (client, eventloop) = AsyncClient::new(mqtt_opts, 65536);

    // Task 1: Drive the MQTT event loop (handles TCP I/O)
    let eventloop_handle = tokio::spawn(mqtt_eventloop_task(eventloop));

    // Task 2: Read from channel and publish
    let publish_handle = tokio::spawn(async move {
        info!("MQTT publisher started");

        while let Some(msg) = rx.recv().await {
            match client.publish(&msg.topic, QoS::AtMostOnce, false, msg.payload).await {
                Ok(_) => {
                    stats.record_mqtt_publish();
                }
                Err(e) => {
                    stats.record_mqtt_error();
                    debug!(error = %e, "MQTT publish error");
                }
            }
        }

        warn!("MQTT publisher channel closed, shutting down");
    });

    Ok((publish_handle, eventloop_handle))
}

/// Drive the rumqttc event loop — must be polled continuously
async fn mqtt_eventloop_task(mut eventloop: EventLoop) {
    loop {
        match eventloop.poll().await {
            Ok(event) => {
                // QoS 0: we mostly get Outgoing notifications and PingResp
                debug!(event = ?event, "MQTT event");
            }
            Err(e) => {
                error!(error = %e, "MQTT connection error, reconnecting in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}
