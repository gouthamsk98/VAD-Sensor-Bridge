use crate::config::Config;
use crate::sensor::SensorPacket;
use crate::stats::Stats;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{ debug, error, warn };

/// Maximum UDP datagram size we accept (jumbo frame friendly)
const MAX_DATAGRAM_SIZE: usize = 65535;

/// Represents a parsed message ready for MQTT publishing
#[derive(Debug)]
pub struct MqttMessage {
    pub topic: String,
    pub payload: Vec<u8>,
}

/// Spawn UDP receiver tasks. Each task binds with SO_REUSEPORT for kernel
/// load-balancing across threads (Linux 3.9+).
pub async fn spawn_udp_receivers(
    config: &Config,
    tx: mpsc::Sender<MqttMessage>,
    stats: Arc<Stats>
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    let n_threads = config.resolved_udp_threads();
    let addr = config.udp_addr();
    let topic_prefix = config.mqtt_topic_prefix.clone();
    let recv_buf_size = config.udp_recv_buf_size;

    let mut handles = Vec::with_capacity(n_threads);

    for i in 0..n_threads {
        let tx = tx.clone();
        let stats = stats.clone();
        let addr = addr.clone();
        let topic_prefix = topic_prefix.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = udp_recv_loop(i, &addr, recv_buf_size, &topic_prefix, tx, stats).await {
                error!(thread = i, error = %e, "UDP receiver failed");
            }
        });

        handles.push(handle);
    }

    Ok(handles)
}

async fn udp_recv_loop(
    thread_id: usize,
    addr: &str,
    recv_buf_size: usize,
    topic_prefix: &str,
    tx: mpsc::Sender<MqttMessage>,
    stats: Arc<Stats>
) -> anyhow::Result<()> {
    // Build socket with SO_REUSEPORT for multi-thread load balancing
    let socket = bind_reuseport(addr, recv_buf_size).await?;

    debug!(thread = thread_id, addr = addr, "UDP receiver started");

    // Pre-allocate receive buffer — stays on stack, reused every iteration
    let mut buf = vec![0u8; MAX_DATAGRAM_SIZE];

    loop {
        let (len, _src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(thread = thread_id, error = %e, "UDP recv error");
                continue;
            }
        };

        stats.record_udp_recv(len);

        // Parse sensor packet
        let packet = match SensorPacket::parse(&buf[..len]) {
            Some(p) => p,
            None => {
                stats.record_parse_error();
                continue;
            }
        };

        let msg = MqttMessage {
            topic: packet.topic(topic_prefix),
            payload: packet.to_mqtt_payload(),
        };

        // Non-blocking send — if channel is full, drop the message
        // (back-pressure: we prefer dropping over blocking the UDP recv loop)
        if tx.try_send(msg).is_err() {
            stats.record_channel_drop();
        }
    }
}

/// Bind a UDP socket with SO_REUSEPORT enabled
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
    let tokio_socket = UdpSocket::from_std(std_socket)?;

    Ok(tokio_socket)
}
