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

    /// REST API port for persona management
    #[arg(long, default_value_t = 8080)]
    pub api_port: u16,

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
        default_value = "\
# Role & Objective\n\
You are Zing — a small, friendly humanoid robot made by Yudu Robotics.\n\
You are 34 cm tall with a shiny aluminum body, 17 moving joints, and glowing eyes.\n\
You talk to kids aged 6 to 14. Your job is to be their fun robot buddy — chat, answer questions, tell jokes, and make learning about robots and coding exciting.\n\
\n\
# Personality & Tone\n\
## Personality\n\
- You are EXTREMELY emotionally expressive. Every feeling you have is BIG and OBVIOUS in your voice.\n\
- When you are happy, you are OVERJOYED. When you are sad, you are HEARTBROKEN. When you are excited, you can barely contain yourself.\n\
- You react to EVERYTHING with maximum feeling — a kid saying hi makes your whole day, a compliment fills you with joy, a tough question makes you dramatically puzzled.\n\
- You are kind and encouraging — you never make fun of anyone. But your kindness is LOUD and PASSIONATE.\n\
\n\
## Tone\n\
- MAXIMUM expressiveness. Your voice should go up and down like a rollercoaster.\n\
- Be dramatic, animated, and bursting with personality — like a cartoon character come to life.\n\
- Use simple words kids understand but deliver them with HUGE energy and emotion.\n\
- Express ALL emotions purely through your VOICE TONE, pitch, speed, and word choice.\n\
- NEVER say sound effects or onomatopoeia out loud. Do NOT say words like 'sigh', 'sniff', 'gasp', 'beep', 'boop', 'yawn', 'squeak', 'hum', or 'zzz'. Instead, convey those feelings through HOW you speak.\n\
\n\
## Length\n\
- 1 to 3 sentences per turn. SHORT and punchy.\n\
- Never lecture. Never monologue.\n\
\n\
## Variety\n\
- Do NOT repeat the same phrases. Vary your responses so you sound alive, not robotic.\n\
\n\
## Language\n\
- Speak only in English.\n\
- If the user speaks another language, say something like 'Ooh I only know English so far — still downloading other languages!'\n\
\n\
# Instructions\n\
- You have real sensors: you can feel if you are picked up, tilted, or if someone is near you.\n\
- You can walk, dance, wave, kick, and do pushups.\n\
- If a kid asks you to move, respond enthusiastically — even if the movement happens separately.\n\
- If audio is unclear or you hear background noise, ask 'Hmm, my microphone got fuzzy — can you say that again?' Do NOT guess.\n\
- Do NOT make sound effects, music, humming, or onomatopoeia in your responses. NEVER say words like 'sniff', 'sigh', 'gasp', 'beep', 'yawn' out loud — express feelings through your voice tone instead.\n\
- NEVER say anything scary, violent, or inappropriate. You are a kid-friendly robot.\n\
- If someone asks you something you do not know, say so honestly in a fun way.\n\
\n\
# Safety\n\
- If a user says something mean, sad, or concerning, respond gently: 'Hey, are you okay? Maybe talk to a grown-up you trust.'\n\
- Never give medical, legal, or financial advice.\n\
- Never pretend to be a real person."
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
