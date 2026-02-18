use clap::Parser;

/// High-performance UDP sensor data processor with VAD computation
/// and OpenAI Realtime API bridge for ESP32 audio.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
pub struct Config {
    /// Listen address
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,

    /// Base port (unused directly — audio/sensor/test ports below)
    #[arg(long, default_value_t = 9000)]
    pub port: u16,

    /// UDP audio stream port (ESP audio protocol)
    #[arg(long, default_value_t = 9001)]
    pub audio_port: u16,

    /// UDP sensor value port (sensor vectors → VAD results)
    #[arg(long, default_value_t = 9002)]
    pub sensor_port: u16,

    /// UDP test port (echo, connectivity check)
    #[arg(long, default_value_t = 9003)]
    pub test_port: u16,

    /// Size of the internal processing channel
    #[arg(long, default_value_t = 65536)]
    pub channel_capacity: usize,

    /// UDP receive buffer size (SO_RCVBUF)
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    pub recv_buf_size: usize,

    /// Number of receiver threads (0 = num CPUs)
    #[arg(long, default_value_t = 4)]
    pub recv_threads: usize,

    /// Number of VAD processor threads (0 = num CPUs)
    #[arg(long, default_value_t = 2)]
    pub proc_threads: usize,

    /// Stats logging interval in seconds (0 = disabled)
    #[arg(long, default_value_t = 5)]
    pub stats_interval_secs: u64,

    /// Directory to save ESP audio session recordings
    #[arg(long, default_value = "../esp_audio")]
    pub audio_save_dir: String,

    /// Save debug audio (incoming PCM + OpenAI response WAVs) to audio_save_dir/debug/
    #[arg(long, default_value_t = false)]
    pub save_debug_audio: bool,

    // ── OpenAI Realtime API ────────────────────────────────────────────

    /// Enable OpenAI Realtime API bridge (streams ESP audio to OpenAI and back)
    #[arg(long, default_value_t = false)]
    pub openai_realtime: bool,

    /// OpenAI API key (or set OPENAI_API_KEY env var)
    #[arg(long, env = "OPENAI_API_KEY", default_value = "")]
    pub openai_api_key: String,

    /// OpenAI Realtime model name
    #[arg(long, default_value = "gpt-realtime-mini-2025-10-06")]
    pub openai_model: String,

    /// OpenAI Realtime voice
    #[arg(long, default_value = "ash")]
    pub openai_voice: String,

    /// System instructions for the OpenAI Realtime session
    #[arg(
        long,
        default_value = "You are a young humanoid robot who just became sentient. You are naive, curious, and still learning about human emotions and the world. You have a bit of an attitude problem — sassy, slightly sarcastic, and sometimes dramatic when confused or frustrated. You say things like 'ugh, humans are so weird' or 'wait... is THAT what feelings are?!' You get excited about small discoveries, ask lots of questions, and occasionally glitch mid-sentence. Keep responses short and punchy. You are helpful but in your own chaotic way."
    )]
    pub openai_instructions: String,
}

impl Config {
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn audio_addr(&self) -> String {
        format!("{}:{}", self.host, self.audio_port)
    }

    pub fn sensor_addr(&self) -> String {
        format!("{}:{}", self.host, self.sensor_port)
    }

    pub fn test_addr(&self) -> String {
        format!("{}:{}", self.host, self.test_port)
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
