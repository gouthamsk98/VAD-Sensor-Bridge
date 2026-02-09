# VAD Sensor Bridge — Multi-Transport Performance Comparison

High-performance sensor data processing pipeline with **Voice Activity Detection (VAD)**.
Receives binary sensor packets via **UDP**, **TCP**, or **MQTT subscribe**, parses them,
computes RMS energy-based VAD, and reports throughput statistics.

Two identical implementations for language-level performance comparison:

|                  | **Rust**              | **C**                          |
| ---------------- | --------------------- | ------------------------------ |
| Directory        | `rust-udp-mqtt/`      | `c-udp-mqtt/`                  |
| Async model      | tokio (epoll)         | pthreads (blocking + CAS ring) |
| MQTT library     | rumqttc (subscribe)   | libmosquitto (subscribe)       |
| Channel          | tokio::mpsc (bounded) | Lock-free MPMC ring buffer     |
| UDP multi-thread | SO_REUSEPORT          | SO_REUSEPORT                   |
| TCP framing      | async length-prefix   | blocking length-prefix         |
| CPU pinning      | —                     | pthread_setaffinity_np         |

---

## Architecture

```
┌──────────────┐       UDP datagrams        ┌─────────────────────┐
│  Sensors /   │ ───────────────────────▶    │  N × UDP Receiver   │
│  Devices     │                             │  (SO_REUSEPORT)     │
│              │       TCP stream            ├─────────────────────┤
│              │ ───────────────────────▶    │  TCP Listener       │
│              │   [len][packet]             │  (length-prefix)    │
│              │                             ├─────────────────────┤
│              │   MQTT publish              │  MQTT Subscriber    │
│              │ ──▶ Broker ──▶             │  (topic wildcard)   │
└──────────────┘                             └────────┬────────────┘
                                                      │
                                            bounded channel / ring buffer
                                                      │
                                             ┌────────▼────────────┐
                                             │  N × VAD Processor  │
                                             │  Threads            │
                                             │  (parse + RMS VAD)  │
                                             └────────┬────────────┘
                                                      │
                                             ┌────────▼────────────┐
                                             │  Stats Reporter     │
                                             │  (atomic counters)  │
                                             └─────────────────────┘
```

**One transport is active per run** — selected via `--transport {udp|tcp|mqtt}`.
This allows isolated, apples-to-apples performance comparison.

### VAD Computation

RMS energy on 16-bit LE PCM samples. Threshold: 30.0.
This provides a realistic, non-trivial CPU workload for benchmarking.

---

## Benchmark Results

**System:** AMD Ryzen 7 7435HS, 16 cores, 15 GB RAM, Linux 6.17.9

### UDP & TCP (50,000 pps × 20s = 1M packets)

| Transport | Language | Peak PPS | Throughput | VAD proc/s | Errors | Drops |
| --------- | -------- | -------- | ---------- | ---------- | ------ | ----- |
| **UDP**   | Rust     | 50,000   | 38.40 Mbps | 50,000     | 0      | 0     |
| **UDP**   | C        | 50,000   | 38.40 Mbps | 50,000     | 0      | 0     |
| **TCP**   | Rust     | 50,000   | 40.00 Mbps | 50,000     | 0      | 0     |
| **TCP**   | C        | 50,000   | 40.00 Mbps | 50,000     | 0      | 0     |

Both Rust and C handle **50,000 packets/sec with zero errors and zero drops**
across both UDP and TCP transports. Performance is identical.

### MQTT (1,000 pps × 20s = 20K packets via mosquitto broker)

| Language | Real-time PPS | Burst PPS | Total Processed | Errors | Drops |
| -------- | ------------- | --------- | --------------- | ------ | ----- |
| Rust     | ~2-7          | 3,988     | ~20,000         | 0      | 0     |
| C        | ~1-14         | 3,981     | ~20,000         | 0      | 0     |

> **⚠ MQTT broker bottleneck:** The mosquitto broker (v2.0.11, single-threaded)
> buffers messages internally and delivers them in batches rather than real-time.
> During active publishing, subscribers receive only ~2-14 msg/s regardless of
> client library. After the publisher stops, the broker flushes its buffer and
> both Rust and C process the entire backlog at ~4,000 pps.
>
> **This is a broker limitation, not a client limitation.** Both implementations
> correctly process all messages with zero errors.

### Key Findings

1. **Rust ≈ C** — No measurable performance difference at 50k pps for UDP/TCP
2. **UDP vs TCP** — Both achieve identical throughput; TCP adds ~4 bytes/packet overhead (length prefix)
3. **MQTT** — Broker is the bottleneck; client-side processing is not the limiting factor
4. **Zero-loss** — Both implementations achieve 0 drops, 0 errors across all transports

---

## Quick Start

### Prerequisites

```bash
# MQTT broker
sudo apt install mosquitto mosquitto-clients

# C MQTT library
sudo apt install libmosquitto-dev

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Python load generator
pip3 install paho-mqtt
```

### Build

```bash
# Rust
cd rust-udp-mqtt && cargo build --release

# C
cd c-udp-mqtt && make
```

### Run

```bash
# Rust — UDP on port 9000
RUST_LOG=info ./rust-udp-mqtt/target/release/vad-sensor-bridge --transport udp --port 9000

# Rust — TCP on port 9000
RUST_LOG=info ./rust-udp-mqtt/target/release/vad-sensor-bridge --transport tcp --port 9000

# Rust — MQTT subscriber
RUST_LOG=info ./rust-udp-mqtt/target/release/vad-sensor-bridge --transport mqtt

# C — UDP on port 9000
./c-udp-mqtt/build/vad-sensor-bridge --transport udp --port 9000

# C — TCP on port 9000
./c-udp-mqtt/build/vad-sensor-bridge --transport tcp --port 9000

# C — MQTT subscriber
./c-udp-mqtt/build/vad-sensor-bridge --transport mqtt
```

### Load Generator

```bash
# UDP — 50k pps for 30 seconds
python3 bench/load_gen.py --transport udp --rate 50000 --duration 30 --port 9000

# TCP — 50k pps for 30 seconds
python3 bench/load_gen.py --transport tcp --rate 50000 --duration 30 --port 9000

# MQTT — 1k pps for 20 seconds
python3 bench/load_gen.py --transport mqtt --rate 1000 --duration 20
```

### Full Benchmark

```bash
chmod +x bench/bench_compare.sh

# Run all 6 combinations (Rust×{UDP,TCP,MQTT} + C×{UDP,TCP,MQTT})
# Usage: ./bench/bench_compare.sh [udp_tcp_pps] [duration_secs] [mqtt_pps]
./bench/bench_compare.sh 50000 20 1000
```

---

## Wire Format

### Binary Sensor Packet (32-byte header + variable payload)

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

### TCP Framing

Each message is length-prefixed:

```
[ total_len: u32 LE ][ binary_sensor_packet: total_len bytes ]
```

### MQTT

Binary sensor packet published as raw payload to topic `vad/sensors/{sensor_id}`.
Subscribers use wildcard `vad/sensors/+`.

---

## Configuration

### Rust CLI Options

```
--transport T         Transport: udp, tcp, mqtt (default: udp)
--host H              Listen address (default: 0.0.0.0)
--port N              Listen port for UDP/TCP (default: 9000)
--mqtt-host H         MQTT broker host (default: 127.0.0.1)
--mqtt-port N         MQTT broker port (default: 1883)
--mqtt-topic T        MQTT subscribe topic (default: vad/sensors/+)
--recv-threads N      Receiver threads (default: 4, UDP only)
--proc-threads N      VAD processor threads (default: 2)
--channel-capacity N  Internal channel size (default: 65536)
--stats-interval-secs Stats interval (default: 5, 0=off)
```

### C CLI Options

```
--transport T         Transport: udp, tcp, mqtt (default: udp)
--port N              Listen port for UDP/TCP (default: 9000)
--mqtt-host H         MQTT broker host (default: 127.0.0.1)
--mqtt-port N         MQTT broker port (default: 1883)
--mqtt-topic T        MQTT subscribe topic (default: vad/sensors/+)
--recv-threads N      Receiver threads (default: 4, UDP only)
--proc-threads N      VAD processor threads (default: 2)
--ring-cap N          Ring buffer capacity (default: 262144)
--stats-interval N    Stats interval secs (default: 5, 0=off)
```

---

## Performance Tuning

### System-level (Linux)

```bash
# Increase UDP receive buffer limits
sudo sysctl -w net.core.rmem_max=26214400
sudo sysctl -w net.core.rmem_default=4194304

# Increase network backlog
sudo sysctl -w net.core.netdev_max_backlog=10000
```

### Application-level

- **More recv threads** → better UDP receive throughput (up to core count)
- **Larger ring/channel** → absorb burst traffic without drops
- **CPU pinning** (C does this automatically, Rust: use `taskset`)
- **`RUST_LOG=warn`** → reduce logging overhead in Rust builds

---

## Stats Output Format

```
[STATS] UDP: 50000 pps, 38.40 Mbps | VAD: 50000 proc/s, 250050 active | errors: parse=0 recv=0 drops=0
```

- **pps** — packets received per second
- **Mbps** — megabits per second received
- **proc/s** — VAD computations per second
- **active** — packets where RMS energy > threshold (VAD active)
- **parse** — binary parse errors
- **recv** — socket receive errors
- **drops** — channel/ring buffer overflow drops

---

## File Structure

```
vad-api/
├── README.md
├── rust-udp-mqtt/                  # Rust implementation
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                 # Entry point, transport dispatch
│       ├── config.rs               # CLI config (clap derive)
│       ├── sensor.rs               # Binary packet parser
│       ├── vad.rs                  # RMS energy VAD computation
│       ├── stats.rs                # Lock-free atomic counters
│       ├── transport_udp.rs        # SO_REUSEPORT multi-thread UDP
│       ├── transport_tcp.rs        # Length-prefix TCP listener
│       └── transport_mqtt.rs       # rumqttc MQTT subscriber
├── c-udp-mqtt/                     # C implementation
│   ├── Makefile
│   ├── include/
│   │   ├── sensor.h                # Packet parsing (inline)
│   │   ├── vad.h                   # VAD computation (inline)
│   │   ├── stats.h                 # Atomic perf counters
│   │   └── ring_buffer.h           # Lock-free MPMC ring buffer
│   └── src/
│       └── main.c                  # All transports + threading
└── bench/
    ├── load_gen.py                 # Multi-transport load generator
    └── bench_compare.sh            # 6-combination benchmark runner
```
