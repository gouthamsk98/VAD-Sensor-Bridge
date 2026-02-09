use crate::config::Config;
use crate::sensor::SensorPacket;
use crate::stats::Stats;
use rumqttc::{ AsyncClient, EventLoop, MqttOptions, QoS, Event, Packet };
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{ debug, error, info };

/// Spawn MQTT subscriber that receives sensor data from broker topics.
pub async fn spawn_mqtt_subscriber(
    config: &Config,
    tx: mpsc::Sender<SensorPacket>,
    stats: Arc<Stats>
) -> anyhow::Result<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)> {
    let mut mqtt_opts = MqttOptions::new(
        &config.mqtt_client_id,
        &config.mqtt_host,
        config.mqtt_port
    );
    mqtt_opts.set_keep_alive(Duration::from_secs(30));
    mqtt_opts.set_inflight(u16::MAX);
    mqtt_opts.set_clean_session(true);

    let (client, eventloop) = AsyncClient::new(mqtt_opts, 65536);

    // Subscribe to sensor topics
    let topic = config.mqtt_topic.clone();
    let sub_handle = tokio::spawn({
        let client = client.clone();
        async move {
            // Wait a moment for connection
            tokio::time::sleep(Duration::from_millis(500)).await;
            match client.subscribe(&topic, QoS::AtMostOnce).await {
                Ok(_) => info!(topic = %topic, "MQTT subscribed"),
                Err(e) => error!(error = %e, "MQTT subscribe failed"),
            }
        }
    });

    // Drive event loop and extract incoming messages
    let recv_handle = tokio::spawn(async move {
        mqtt_recv_loop(eventloop, tx, stats).await;
    });

    // Subscribe task is fire-and-forget
    let _ = sub_handle.await;

    Ok((recv_handle, tokio::spawn(async {})))
}

async fn mqtt_recv_loop(
    mut eventloop: EventLoop,
    tx: mpsc::Sender<SensorPacket>,
    stats: Arc<Stats>
) {
    info!("MQTT subscriber event loop started");

    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let data = publish.payload.to_vec();
                let data_len = data.len();
                stats.record_recv(data_len);

                // Try to parse the payload as a sensor packet
                let packet = if let Some(p) = SensorPacket::from_binary(&data) {
                    p
                } else if let Some(p) = SensorPacket::from_json(&data) {
                    p
                } else {
                    stats.record_parse_error();
                    continue;
                };

                if tx.try_send(packet).is_err() {
                    stats.record_channel_drop();
                }
            }
            Ok(_event) => {
                debug!("MQTT event");
            }
            Err(e) => {
                error!(error = %e, "MQTT connection error, reconnecting in 1s");
                stats.record_recv_error();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}
