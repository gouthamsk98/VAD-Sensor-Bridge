use clap::{ Parser, ValueEnum };

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    Udp,
    Tcp,
    Mqtt,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Udp => write!(f, "UDP"),
            Transport::Tcp => write!(f, "TCP"),
            Transport::Mqtt => write!(f, "MQTT"),
        }
    }
}

/// High-performance multi-transport sensor data processor with VAD computation
#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
pub struct Config {
    /// Input transport: udp, tcp, or mqtt
    #[arg(long, value_enum, default_value_t = Transport::Udp)]
    pub transport: Transport,

    /// Listen address for UDP/TCP
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,

    /// Listen port for UDP/TCP
    #[arg(long, default_value_t = 9000)]
    pub port: u16,

    /// MQTT broker host (for mqtt transport)
    #[arg(long, default_value = "127.0.0.1")]
    pub mqtt_host: String,

    /// MQTT broker port
    #[arg(long, default_value_t = 1883)]
    pub mqtt_port: u16,

    /// MQTT subscribe topic (for mqtt transport input)
    #[arg(long, default_value = "vad/sensors/+")]
    pub mqtt_topic: String,

    /// MQTT client ID
    #[arg(long, default_value = "vad-processor-rust")]
    pub mqtt_client_id: String,

    /// Size of the internal processing channel
    #[arg(long, default_value_t = 65536)]
    pub channel_capacity: usize,

    /// UDP/TCP receive buffer size (SO_RCVBUF)
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    pub recv_buf_size: usize,

    /// Number of receiver threads (0 = num CPUs, applies to UDP)
    #[arg(long, default_value_t = 4)]
    pub recv_threads: usize,

    /// Number of VAD processor threads (0 = num CPUs)
    #[arg(long, default_value_t = 2)]
    pub proc_threads: usize,

    /// Stats logging interval in seconds (0 = disabled)
    #[arg(long, default_value_t = 5)]
    pub stats_interval_secs: u64,
}

impl Config {
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn resolved_recv_threads(&self) -> usize {
        if self.recv_threads == 0 { num_cpus() } else { self.recv_threads }
    }

    pub fn resolved_proc_threads(&self) -> usize {
        if self.proc_threads == 0 { num_cpus() } else { self.proc_threads }
    }
}

fn num_cpus() -> usize {
    std::thread
        ::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
