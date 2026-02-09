use std::sync::atomic::{ AtomicU64, Ordering };
use std::sync::Arc;
use std::time::{ Duration, Instant };

/// Lock-free performance counters
#[derive(Debug)]
pub struct Stats {
    pub udp_packets_received: AtomicU64,
    pub udp_bytes_received: AtomicU64,
    pub mqtt_messages_published: AtomicU64,
    pub parse_errors: AtomicU64,
    pub mqtt_publish_errors: AtomicU64,
    pub channel_drops: AtomicU64,
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            udp_packets_received: AtomicU64::new(0),
            udp_bytes_received: AtomicU64::new(0),
            mqtt_messages_published: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            mqtt_publish_errors: AtomicU64::new(0),
            channel_drops: AtomicU64::new(0),
        })
    }

    #[inline(always)]
    pub fn record_udp_recv(&self, bytes: usize) {
        self.udp_packets_received.fetch_add(1, Ordering::Relaxed);
        self.udp_bytes_received.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn record_mqtt_publish(&self) {
        self.mqtt_messages_published.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn record_parse_error(&self) {
        self.parse_errors.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn record_mqtt_error(&self) {
        self.mqtt_publish_errors.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn record_channel_drop(&self) {
        self.channel_drops.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot and reset counters, return rates
    pub fn snapshot_and_reset(&self, elapsed: Duration) -> StatsSnapshot {
        let secs = elapsed.as_secs_f64().max(0.001);

        let udp_pkts = self.udp_packets_received.swap(0, Ordering::Relaxed);
        let udp_bytes = self.udp_bytes_received.swap(0, Ordering::Relaxed);
        let mqtt_msgs = self.mqtt_messages_published.swap(0, Ordering::Relaxed);
        let parse_err = self.parse_errors.swap(0, Ordering::Relaxed);
        let mqtt_err = self.mqtt_publish_errors.swap(0, Ordering::Relaxed);
        let drops = self.channel_drops.swap(0, Ordering::Relaxed);

        StatsSnapshot {
            udp_pps: (udp_pkts as f64) / secs,
            udp_mbps: ((udp_bytes as f64) * 8.0) / (secs * 1_000_000.0),
            mqtt_mps: (mqtt_msgs as f64) / secs,
            parse_errors: parse_err,
            mqtt_errors: mqtt_err,
            channel_drops: drops,
        }
    }
}

#[derive(Debug)]
pub struct StatsSnapshot {
    pub udp_pps: f64,
    pub udp_mbps: f64,
    pub mqtt_mps: f64,
    pub parse_errors: u64,
    pub mqtt_errors: u64,
    pub channel_drops: u64,
}

/// Background stats reporter task
pub async fn stats_reporter(stats: Arc<Stats>, interval_secs: u64) {
    if interval_secs == 0 {
        // Stats disabled — park forever
        std::future::pending::<()>().await;
        return;
    }

    let interval = Duration::from_secs(interval_secs);
    let mut last = Instant::now();

    loop {
        tokio::time::sleep(interval).await;
        let now = Instant::now();
        let elapsed = now - last;
        last = now;

        let snap = stats.snapshot_and_reset(elapsed);
        println!(
            "[STATS] UDP: {:.0} pps, {:.2} Mbps | MQTT: {:.0} msg/s | errors: parse={} mqtt={} drops={}",
            snap.udp_pps,
            snap.udp_mbps,
            snap.mqtt_mps,
            snap.parse_errors,
            snap.mqtt_errors,
            snap.channel_drops
        );
    }
}
