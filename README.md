# VAD Sensor Bridge — ESP32 Audio + Emotional VAD + OpenAI Realtime

High-performance **UDP sensor bridge** for an ESP32 robot platform.
Receives binary sensor packets and ESP audio streams via UDP,
computes dual-pipeline VAD (audio RMS + emotional Valence-Arousal-Dominance),
supports **runtime personality traits** that reshape emotional responses,
and optionally bridges audio to the **OpenAI Realtime API** for
bidirectional voice conversation.

Built in Rust (tokio async), statically linked via musl for single-binary EC2 deployment.

---

## Architecture

```
                                       ┌────────────────────────┐
                                       │   OpenAI Realtime API  │
                                       │   (WebSocket)          │
                                       │   gpt-realtime-mini    │
                                       └───────┬──▲─────────────┘
                                               │  │
                                      audio_down  audio_up
                                       (24 kHz)   (24 kHz)
                                               │  │
┌──────────────┐   ESP Audio (UDP:9001)   ┌────▼──┴─────────────┐
│  ESP32       │ ◄──────────────────────▶ │  Audio Bridge        │
│  Robot       │   16 kHz PCM packets     │  (resample 16⇄24k)  │
│              │                          ├──────────────────────┤
│  Sensors     │   Sensor Vec (UDP:9002)  │  Sensor Receiver     │
│  (IMU, cam,  │ ────────────────────────▶│  → Emotional VAD     │
│   mic, batt) │                          │  → VAD Response      │
│              │   VAD Response (UDP)     │                      │
│              │ ◄────────────────────────│  (binary V/A/D back) │
└──────────────┘                          ├──────────────────────┤
                                          │  Test Port (UDP:9003)│
                  Test echo ◄────────────▶│  (echo / connectivity│
                                          ├──────────────────────┤
                                          │  Stats Reporter      │
                                          │  (atomic counters)   │
                                          └──────────────────────┘
```

### Dual VAD Pipeline

| Pipeline          | Input                         | Method                                         | Output                            |
| ----------------- | ----------------------------- | ---------------------------------------------- | --------------------------------- |
| **Audio VAD**     | 16-bit LE PCM (type=1)        | RMS energy, threshold = 30.0                   | `is_active`, `energy`             |
| **Emotional VAD** | 10×f32 sensor vector (type=2) | Weighted linear V/A/D with bias, clamped [0,1] | `valence`, `arousal`, `dominance` |

**Emotional VAD** maps 10 environmental sensor channels to Valence–Arousal–Dominance
using fixed weight vectors:

| Channel       | Index | Meaning                               |
| ------------- | ----- | ------------------------------------- |
| battery_low   | 0     | Battery depleted (0=full, 1=critical) |
| people_count  | 1     | Normalised nearby people count        |
| known_face    | 2     | Recognised face confidence            |
| unknown_face  | 3     | Unfamiliar face confidence            |
| fall_event    | 4     | Fall / impact intensity               |
| lifted        | 5     | Robot grabbed / lifted                |
| idle_time     | 6     | Time since last activity (0→1)        |
| sound_energy  | 7     | Ambient sound level                   |
| voice_rate    | 8     | Speech cadence (conversation proxy)   |
| motion_energy | 9     | IMU / accelerometer motion energy     |

**Arousal threshold** for `is_active`: 0.35

### Personality Traits

The emotional VAD weights can be modified at runtime by selecting a **personality trait**.
Each trait applies additive deltas to the base V/A/D weight vectors, shaping how the
robot _feels_ about the same sensor inputs.

| Trait           | Index | Behaviour                                                                  |
| --------------- | ----- | -------------------------------------------------------------------------- |
| **Obedient**    | 0     | Calm, compliant — dampened arousal, boosted dominance (default)            |
| **Mischievous** | 1     | Playful, chaotic — amplified arousal from sound/motion, lower dominance    |
| **Cute**        | 2     | Affectionate — boosted valence for social channels, softer threat response |
| **Stubborn**    | 3     | Defiant — high dominance, reduced social valence, higher threat arousal    |

**Example effect** — ANGRY/FEAR scenario (same sensor inputs):

| Trait       | Valence | Arousal | Dominance | Active? |
| ----------- | ------- | ------- | --------- | ------- |
| Obedient    | 0.00    | 0.65    | 0.07      | ✅      |
| Mischievous | 0.00    | 0.88    | 0.00      | ✅      |
| Cute        | 0.08    | 0.73    | 0.12      | ✅      |
| Stubborn    | 0.10    | 0.88    | 0.22      | ✅      |

The active persona is changed at runtime via the REST API (see below).

### REST API

A lightweight HTTP API (axum) runs on `--api-port` (default 8080) for runtime
personality management.

| Method | Endpoint        | Description                      |
| ------ | --------------- | -------------------------------- |
| GET    | `/health`       | Health check (`{"status":"ok"}`) |
| GET    | `/persona`      | Current active persona + index   |
| GET    | `/persona/list` | All available personas + current |
| PUT    | `/persona`      | Change active persona            |

**Set persona by name:**

```bash
curl -X PUT http://localhost:8080/persona \
     -H 'Content-Type: application/json' \
     -d '{"persona": "mischievous"}'
```

**Set persona by index:**

```bash
curl -X PUT http://localhost:8080/persona \
     -H 'Content-Type: application/json' \
     -d '{"index": 2}'
```

**Get current persona:**

```bash
curl http://localhost:8080/persona
# {"persona":"obedient","index":0}
```

**List all personas:**

```bash
curl http://localhost:8080/persona/list
# {"current":"obedient","available":[{"index":0,"name":"obedient"}, ...]}
```

---

## Wire Formats

### ESP Audio Protocol (4-byte header + variable payload)

Used on UDP port 9001. Audio format: **16-bit LE PCM, 16 kHz, mono**.

```
┌─────────────┬──────────┬──────────┬────────────────┐
│ Byte 0-1    │ Byte 2   │ Byte 3   │ Byte 4..N      │
│ Seq Num     │ Type     │ Flags    │ Payload         │
│ (uint16 LE) │ (uint8)  │ (uint8)  │ (up to 1400B)  │
└─────────────┴──────────┴──────────┴────────────────┘
```

**Packet Types:**

| Value | Name       | Direction     | Description                  |
| ----- | ---------- | ------------- | ---------------------------- |
| 0x01  | AUDIO_UP   | ESP → Server  | Microphone PCM audio chunk   |
| 0x02  | AUDIO_DOWN | Server → ESP  | I2S playback audio chunk     |
| 0x03  | CONTROL    | Bidirectional | Control / command messages   |
| 0x04  | HEARTBEAT  | Bidirectional | Keep-alive / RTT measurement |

**Flags** (bitfield in byte 3): `BIT0`=start, `BIT1`=end, `BIT2`=urgent.

**Control Commands** (first byte of payload when type=0x03):

| Value | Name          | Direction     | Description                  |
| ----- | ------------- | ------------- | ---------------------------- |
| 0x01  | SESSION_START | ESP → Server  | Wake word detected, begin    |
| 0x02  | SESSION_END   | ESP → Server  | User stopped speaking        |
| 0x03  | STREAM_START  | Server → ESP  | About to send audio response |
| 0x04  | STREAM_END    | Server → ESP  | Finished sending audio       |
| 0x05  | ACK           | Bidirectional | Acknowledge control message  |
| 0x06  | CANCEL        | Bidirectional | Abort current session        |
| 0x07  | SERVER_READY  | Server → ESP  | Server is ready for audio    |

1400 B payload = 700 samples = 43.75 ms per packet at 16 kHz.

### Binary Sensor Packet (32-byte header + variable payload)

Used on UDP port 9002.

| Offset | Size | Field                 |
| ------ | ---- | --------------------- |
| 0      | 4    | sensor_id (u32 LE)    |
| 4      | 8    | timestamp_us (u64 LE) |
| 12     | 1    | data_type (u8)        |
| 13     | 3    | reserved              |
| 16     | 2    | payload_len (u16 LE)  |
| 18     | 2    | reserved              |
| 20     | 8    | seq (u64 LE)          |
| 28     | 4    | padding               |
| 32     | N    | payload bytes         |

**Data types:**

- `1` — 16-bit LE PCM audio (for audio RMS VAD)
- `2` — 10×f32 LE sensor vector (for emotional VAD: Valence-Arousal-Dominance)

### VAD Response Packet (34 bytes)

Sent back to ESP on the sensor port.

| Offset | Size | Field                       |
| ------ | ---- | --------------------------- |
| 0      | 4    | sensor_id (u32 LE)          |
| 4      | 8    | seq (u64 LE)                |
| 12     | 1    | is_active (u8)              |
| 13     | 1    | kind (1=audio, 2=emotional) |
| 14     | 4    | energy (f32 LE)             |
| 18     | 4    | threshold (f32 LE)          |
| 22     | 4    | valence (f32 LE)            |
| 26     | 4    | arousal (f32 LE)            |
| 30     | 4    | dominance (f32 LE)          |

---

## Quick Start

### Prerequisites

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# musl target for static linking (deployment)
rustup target add x86_64-unknown-linux-musl

# musl toolchain + OpenSSL headers
sudo apt install musl-tools pkg-config libssl-dev
```

### Build

```bash
# Development build
cd rust-udp-mqtt && cargo build --release

# Static musl build (for EC2 / deployment)
cd rust-udp-mqtt && cargo build --release --target x86_64-unknown-linux-musl
```

### Run

```bash
# Basic — sensor processing only (no OpenAI)
RUST_LOG=info ./rust-udp-mqtt/target/release/vad-sensor-bridge

# With OpenAI Realtime API bridge
RUST_LOG=info ./rust-udp-mqtt/target/release/vad-sensor-bridge \
    --openai-realtime \
    --openai-api-key "sk-proj-..."

# With debug audio saving
RUST_LOG=info ./rust-udp-mqtt/target/release/vad-sensor-bridge \
    --openai-realtime \
    --save-debug-audio \
    --audio-save-dir ./recordings
```

### Test Connectivity

```bash
# Send a test packet to the echo port
echo "hello" | nc -u <server-ip> 9003

# Send sensor vectors
python3 bench/send_sensor.py --host <server-ip>

# Stream microphone audio via ESP protocol
python3 bench/stream_mic_esp.py --host <server-ip>

# Test OpenAI Realtime roundtrip
python3 bench/test_openai_realtime.py --host <server-ip>
```

---

## Configuration

### CLI Options

```
--host H                 Listen address (default: 0.0.0.0)
--port N                 Base port (default: 9000)
--audio-port N           ESP audio stream port (default: 9001)
--sensor-port N          Sensor vector port (default: 9002)
--test-port N            Test / echo port (default: 9003)
--api-port N             REST API port for persona management (default: 8080)
--recv-threads N         Receiver threads (default: 4, 0 = num CPUs)
--proc-threads N         VAD processor threads (default: 2, 0 = num CPUs)
--channel-capacity N     Internal channel size (default: 65536)
--recv-buf-size N        SO_RCVBUF size (default: 4194304)
--stats-interval-secs N  Stats interval (default: 5, 0 = disabled)
--audio-save-dir DIR     Directory for ESP audio session WAVs (default: ../esp_audio)
--save-debug-audio       Enable saving debug audio (OpenAI responses + ESP input)
--openai-realtime        Enable OpenAI Realtime API bridge
--openai-api-key KEY     OpenAI API key (or OPENAI_API_KEY env var)
--openai-model MODEL     OpenAI model (default: gpt-realtime-mini-2025-10-06)
--openai-voice VOICE     OpenAI voice (default: ash)
--openai-instructions T  System prompt for OpenAI session
```

---

## Deployment (EC2)

Single-binary static musl deployment to AWS EC2 (t2.micro free tier).

### Scripts

| Script                   | Description                                     |
| ------------------------ | ----------------------------------------------- |
| `deploy/deploy-ec2.sh`   | Launch EC2 instance, configure, install service |
| `deploy/build-deploy.sh` | Build musl binary + deploy to existing server   |
| `deploy/fetch-audio.sh`  | Download recorded audio from server             |
| `deploy/teardown-ec2.sh` | Terminate EC2 instance + cleanup                |

### Build & Deploy

```bash
# Full build + deploy
./deploy/build-deploy.sh

# Skip build, deploy existing binary
./deploy/build-deploy.sh --skip-build

# Deploy and tail logs
./deploy/build-deploy.sh --logs
```

### Fetch Audio Recordings

```bash
# Download all audio files
./deploy/fetch-audio.sh

# Download only files matching pattern
./deploy/fetch-audio.sh "openai_response"
```

### Systemd Service

The service runs as `vad-bridge.service` with:

- Auto-restart on crash (`RestartSec=3`)
- OpenAI API key in `Environment`
- `--save-debug-audio` enabled for testing
- Audio saved to `/home/ec2-user/esp_audio/debug/`

---

## Stats Output

Stats are logged every `--stats-interval-secs` seconds (only when there's activity):

```
[STATS] 50 pps, 0.61 Mbps | VAD: 50 proc/s, 12 active | errors: parse=0 recv=0 drops=0
```

- **pps** — packets received per second
- **Mbps** — megabits per second received
- **proc/s** — VAD computations per second
- **active** — packets where VAD detected activity (audio RMS > 30.0, or emotional arousal > 0.35)
- **parse/recv/drops** — error counters

---

## OpenAI Realtime Integration

When `--openai-realtime` is enabled:

1. A persistent WebSocket connects to the OpenAI Realtime API on startup
2. ESP audio (16 kHz) is resampled to 24 kHz, base64 encoded, and sent as `input_audio_buffer.append`
3. OpenAI responses (`response.audio.delta`) are resampled 24 kHz → 16 kHz and sent back to ESP as `AUDIO_DOWN` packets
4. Transcripts are logged: `🤖 AI SAID` / `👤 USER SAID`
5. **PromptMode** dynamically adjusts the OpenAI system instructions based on the robot's emotional state (V/A/D values from sensor vectors)

### Prompt Mode (Emotional Mapping)

The emotional VAD output maps to one of 8 emotional states, each with a custom system prompt:

| State   | Valence | Arousal | Dominance | Personality                   |
| ------- | ------- | ------- | --------- | ----------------------------- |
| Happy   | High    | Med     | High      | Warm, playful, enthusiastic   |
| Excited | High    | High    | High      | Energetic, excitable          |
| Sad     | Low     | Low     | Low       | Gentle, quiet, needs comfort  |
| Angry   | Low     | High    | High      | Frustrated, sassy, dramatic   |
| Fear    | Low     | High    | Low       | Anxious, clingy, needs safety |
| Tired   | Low     | Low     | Mid       | Drowsy, minimal effort        |
| Curious | Mid     | Mid     | Mid       | Inquisitive, asks questions   |
| Neutral | —       | —       | —         | Default sassy robot persona   |

### Debug Audio Saving

When `--save-debug-audio` is enabled:

- OpenAI response audio is saved as WAV files: `esp_audio/debug/openai_response_<n>_<timestamp>.wav`
- Files are 16 kHz, 16-bit, mono WAV

---

## File Structure

```
vad-api/
├── README.md
├── rust-udp-mqtt/                      # Rust implementation
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                     # Entry point, tokio runtime setup
│       ├── config.rs                   # CLI config (clap derive)
│       ├── persona.rs                  # Personality traits + weight deltas
│       ├── api.rs                      # REST API (axum) for persona management
│       ├── sensor.rs                   # Binary sensor packet parser
│       ├── esp_audio_protocol.rs       # ESP32 ↔ Server UDP audio protocol
│       ├── vad.rs                      # Audio RMS + Emotional V/A/D VAD
│       ├── vad_response.rs             # Binary VAD response format
│       ├── stats.rs                    # Lock-free atomic counters + reporter
│       ├── transport_udp.rs            # UDP receivers, ESP audio, sensor, test
│       └── transport_openai.rs         # OpenAI Realtime WebSocket bridge
├── c-udp-mqtt/                         # C implementation (benchmark reference)
│   ├── Makefile
│   ├── include/
│   │   ├── sensor.h
│   │   ├── vad.h
│   │   ├── stats.h
│   │   └── ring_buffer.h
│   └── src/
│       └── main.c
├── deploy/
│   ├── deploy-ec2.sh                   # Launch + configure EC2 instance
│   ├── build-deploy.sh                 # Build musl binary + deploy
│   ├── fetch-audio.sh                  # Download audio from server
│   └── teardown-ec2.sh                 # Terminate EC2 instance
├── bench/
│   ├── load_gen.py                     # UDP load generator
│   ├── send_sensor.py                  # Sensor vector sender
│   ├── stream_mic_esp.py               # Microphone → ESP audio protocol
│   ├── test_esp_protocol.py            # ESP protocol test
│   ├── test_openai_realtime.py         # OpenAI roundtrip test
│   ├── test_prompt_mode.py             # Prompt mode / emotional state test
│   ├── test_audio_roundtrip.py         # Audio roundtrip test
│   ├── gen_test_wav.py                 # Generate test WAV files
│   └── bench_compare.sh               # Benchmark runner
└── recordings/
    └── esp_audio/                      # Recorded ESP audio sessions
```
