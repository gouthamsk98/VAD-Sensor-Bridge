use crate::config::Config;
use crate::sensor::SensorPacket;
use crate::stats::Stats;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{ debug, warn };

/// Spawn UDP receiver tasks with SO_REUSEPORT for kernel load-balancing.
pub async fn spawn_udp_receivers(
    config: &Config,
    tx: mpsc::Sender<SensorPacket>,
    stats: Arc<Stats>
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    let n_threads = config.resolved_recv_threads();
    let addr = config.listen_addr();
    let recv_buf_size = config.recv_buf_size;

    let mut handles = Vec::with_capacity(n_threads);

    for i in 0..n_threads {
        let tx = tx.clone();
        let stats = stats.clone();
        let addr = addr.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = udp_recv_loop(i, &addr, recv_buf_size, tx, stats).await {
                tracing::error!(thread = i, error = %e, "UDP receiver failed");
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
    tx: mpsc::Sender<SensorPacket>,
    stats: Arc<Stats>
) -> anyhow::Result<()> {
    let socket = bind_reuseport(addr, recv_buf_size).await?;
    debug!(thread = thread_id, addr = addr, "UDP receiver started");

    let mut buf = vec![0u8; 65535];

    loop {
        let (len, _src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(thread = thread_id, error = %e, "UDP recv error");
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

        if tx.try_send(packet).is_err() {
            stats.record_channel_drop();
        }
    }
}

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
