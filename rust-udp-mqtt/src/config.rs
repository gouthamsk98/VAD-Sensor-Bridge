use clap::Parser;

/// High-performance UDP → MQTT QoS 0 sensor data bridge
#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
pub struct Config {
    /// UDP listen address
    #[arg(long, default_value = "0.0.0.0")]
    pub udp_host: String,

    /// UDP listen port
    #[arg(long, default_value_t = 9000)]
    pub udp_port: u16,

    /// MQTT broker host
    #[arg(long, default_value = "127.0.0.1")]
    pub mqtt_host: String,

    /// MQTT broker port
    #[arg(long, default_value_t = 1883)]
    pub mqtt_port: u16,

    /// MQTT topic prefix — sensor data published to {prefix}/{sensor_id}
    #[arg(long, default_value = "vad/sensors")]
    pub mqtt_topic_prefix: String,

    /// MQTT client ID
    #[arg(long, default_value = "vad-sensor-bridge")]
    pub mqtt_client_id: String,

    /// MQTT keep-alive seconds
    #[arg(long, default_value_t = 30)]
    pub mqtt_keep_alive_secs: u16,

    /// Size of the internal channel between UDP receiver and MQTT publisher
    #[arg(long, default_value_t = 65536)]
    pub channel_capacity: usize,

    /// UDP receive buffer size (SO_RCVBUF)
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    pub udp_recv_buf_size: usize,

    /// Number of UDP receiver threads (0 = num CPUs)
    #[arg(long, default_value_t = 0)]
    pub udp_threads: usize,

    /// Enable performance stats logging every N seconds (0 = disabled)
    #[arg(long, default_value_t = 5)]
    pub stats_interval_secs: u64,
}

impl Config {
    pub fn udp_addr(&self) -> String {
        format!("{}:{}", self.udp_host, self.udp_port)
    }

    pub fn resolved_udp_threads(&self) -> usize {
        if self.udp_threads == 0 { num_cpus() } else { self.udp_threads }
    }
}

fn num_cpus() -> usize {
    std::thread
        ::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
