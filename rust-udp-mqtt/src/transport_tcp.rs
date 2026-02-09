use crate::config::Config;
use crate::sensor::{ SensorPacket, HEADER_SIZE };
use crate::stats::Stats;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{ debug, info, warn };

/// Spawn TCP listener that accepts connections and reads length-prefixed sensor packets.
///
/// Wire format per message on TCP:
///   [ total_len: u32 LE ] [ binary_sensor_packet: total_len bytes ]
pub async fn spawn_tcp_receiver(
    config: &Config,
    tx: mpsc::Sender<SensorPacket>,
    stats: Arc<Stats>
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let addr = config.listen_addr();
    let listener = TcpListener::bind(&addr).await?;
    info!(addr = %addr, "TCP listener started");

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    debug!(peer = %peer, "TCP client connected");
                    let tx = tx.clone();
                    let stats = stats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_tcp_client(stream, tx, stats).await {
                            debug!(peer = %peer, error = %e, "TCP client error");
                        }
                        debug!(peer = %peer, "TCP client disconnected");
                    });
                }
                Err(e) => {
                    warn!(error = %e, "TCP accept error");
                }
            }
        }
    });

    Ok(handle)
}

async fn handle_tcp_client(
    mut stream: tokio::net::TcpStream,
    tx: mpsc::Sender<SensorPacket>,
    stats: Arc<Stats>
) -> anyhow::Result<()> {
    let mut len_buf = [0u8; 4];
    let mut pkt_buf = vec![0u8; 65535];

    loop {
        // Read 4-byte length prefix
        if stream.read_exact(&mut len_buf).await.is_err() {
            break; // Client disconnected
        }
        let msg_len = u32::from_le_bytes(len_buf) as usize;

        if msg_len < HEADER_SIZE || msg_len > pkt_buf.len() {
            stats.record_parse_error();
            continue;
        }

        // Read the full packet
        if stream.read_exact(&mut pkt_buf[..msg_len]).await.is_err() {
            break;
        }

        stats.record_recv(msg_len + 4); // include length prefix in byte count

        let packet = match SensorPacket::from_binary(&pkt_buf[..msg_len]) {
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

    Ok(())
}
