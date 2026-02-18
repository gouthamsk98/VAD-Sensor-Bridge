use std::sync::atomic::{ AtomicU64, Ordering };
use std::sync::Arc;
use std::time::{ Duration, Instant };

/// Lock-free performance counters — transport-agnostic
#[derive(Debug)]
pub struct Stats {
    pub recv_packets: AtomicU64,
    pub recv_bytes: AtomicU64,
    pub processed: AtomicU64,
    pub vad_active: AtomicU64,
    pub parse_errors: AtomicU64,
    pub recv_errors: AtomicU64,
    pub channel_drops: AtomicU64,
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            recv_packets: AtomicU64::new(0),
            recv_bytes: AtomicU64::new(0),
            processed: AtomicU64::new(0),
            vad_active: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            recv_errors: AtomicU64::new(0),
            channel_drops: AtomicU64::new(0),
        })
    }

    #[inline(always)]
    pub fn record_recv(&self, bytes: usize) {
        self.recv_packets.fetch_add(1, Ordering::Relaxed);
        self.recv_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn record_processed(&self, is_active: bool) {
        self.processed.fetch_add(1, Ordering::Relaxed);
        if is_active {
            self.vad_active.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline(always)]
    pub fn record_parse_error(&self) {
        self.parse_errors.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn record_recv_error(&self) {
        self.recv_errors.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn record_channel_drop(&self) {
        self.channel_drops.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot and reset counters
    pub fn snapshot_and_reset(&self, elapsed: Duration) -> StatsSnapshot {
        let secs = elapsed.as_secs_f64().max(0.001);

        let pkts = self.recv_packets.swap(0, Ordering::Relaxed);
        let bytes = self.recv_bytes.swap(0, Ordering::Relaxed);
        let proc = self.processed.swap(0, Ordering::Relaxed);
        let active = self.vad_active.swap(0, Ordering::Relaxed);
        let perr = self.parse_errors.swap(0, Ordering::Relaxed);
        let rerr = self.recv_errors.swap(0, Ordering::Relaxed);
        let drops = self.channel_drops.swap(0, Ordering::Relaxed);

        StatsSnapshot {
            recv_pps: (pkts as f64) / secs,
            recv_mbps: ((bytes as f64) * 8.0) / (secs * 1_000_000.0),
            proc_pps: (proc as f64) / secs,
            vad_active: active,
            parse_errors: perr,
            recv_errors: rerr,
            channel_drops: drops,
        }
    }
}

#[derive(Debug)]
pub struct StatsSnapshot {
    pub recv_pps: f64,
    pub recv_mbps: f64,
    pub proc_pps: f64,
    pub vad_active: u64,
    pub parse_errors: u64,
    pub recv_errors: u64,
    pub channel_drops: u64,
}

/// Background stats reporter task
pub async fn stats_reporter(stats: Arc<Stats>, interval_secs: u64, transport_name: &str) {
    if interval_secs == 0 {
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

        // Skip logging when nothing is happening
        let has_activity =
            snap.recv_pps > 0.0 ||
            snap.proc_pps > 0.0 ||
            snap.vad_active > 0 ||
            snap.parse_errors > 0 ||
            snap.recv_errors > 0 ||
            snap.channel_drops > 0;

        if has_activity {
            println!(
                "[STATS] {}: {:.0} pps, {:.2} Mbps | VAD: {:.0} proc/s, {} active | errors: parse={} recv={} drops={}",
                transport_name,
                snap.recv_pps,
                snap.recv_mbps,
                snap.proc_pps,
                snap.vad_active,
                snap.parse_errors,
                snap.recv_errors,
                snap.channel_drops
            );
        }
    }
}
