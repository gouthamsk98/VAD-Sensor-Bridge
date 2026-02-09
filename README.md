# VAD Sensor Bridge — UDP → MQTT QoS 0

High-performance sensor data ingestion pipeline: receives raw sensor datagrams
over **UDP** and publishes them to an **MQTT** broker at **QoS 0**
(fire-and-forget) for minimum latency.

Two identical implementations for performance comparison:

|                  | **Rust**              | **C**                  |
| ---------------- | --------------------- | ---------------------- |
| Directory        | `rust-udp-mqtt/`      | `c-udp-mqtt/`          |
| Async model      | tokio (epoll)         | pthreads (blocking)    |
| MQTT library     | rumqttc               | Eclipse Paho C         |
| Channel          | tokio::mpsc (bounded) | Lock-free ring buffer  |
| UDP multi-thread | SO_REUSEPORT          | SO_REUSEPORT           |
| CPU pinning      | —                     | pthread_setaffinity_np |

---

## Architecture

```
┌──────────────┐    UDP datagrams     ┌─────────────────────┐
│  Sensors /   │ ──────────────────▶  │  N × UDP Receiver   │
│  Devices     │   (binary packets)   │  Threads            │
└──────────────┘                      │  (SO_REUSEPORT)     │
                                      └────────┬────────────┘
                                               │
                                     bounded channel / ring buffer
                                               │
                                      ┌────────▼────────────┐
                                      │  MQTT Publisher      │
                                      │  (QoS 0, async)     │
                                      └────────┬────────────┘
                                               │
                                      ┌────────▼────────────┐
                                      │  MQTT Broker         │
                                      │  (mosquitto, etc.)   │
                                      └─────────────────────┘
```

### Wire Format (Binary Sensor Packet)

32-byte fixed header + variable payload:

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

---

## Quick Start

### Prerequisites

```bash
# MQTT broker
sudo apt install mosquitto mosquitto-clients

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# C dependencies
sudo apt install build-essential cmake libssl-dev
```

### Build & Run — Rust

```bash
cd rust-udp-mqtt
cargo build --release

# Run (defaults: UDP :9000, MQTT localhost:1883)
./target/release/vad-sensor-bridge

# Custom settings
./target/release/vad-sensor-bridge \
    --udp-port 9000 \
    --mqtt-host 10.0.0.1 \
    --mqtt-port 1883 \
    --udp-threads 4 \
    --channel-capacity 131072 \
    --stats-interval-secs 5
```

### Build & Run — C

```bash
cd c-udp-mqtt

# Install Paho MQTT C library (one-time)
make install-deps

# Build
make

# Run
./build/vad-sensor-bridge

# Custom settings
./build/vad-sensor-bridge \
    --udp-port 9000 \
    --mqtt-host 10.0.0.1 \
    --mqtt-port 1883 \
    --threads 4 \
    --ring-cap 131072 \
    --stats-interval 5
```

---

## Benchmarking

A Python load generator and comparison script are provided:

```bash
# Send 50k packets/sec for 30 seconds
python3 bench/udp_load_gen.py --rate 50000 --duration 30

# Full comparison (builds both, runs sequentially)
chmod +x bench/bench_compare.sh
./bench/bench_compare.sh 100000 60
```

### Monitoring MQTT output

```bash
# Subscribe to all sensor topics
mosquitto_sub -t "vad/sensors/#" -v
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

# Make persistent
echo "net.core.rmem_max=26214400" | sudo tee -a /etc/sysctl.conf
echo "net.core.rmem_default=4194304" | sudo tee -a /etc/sysctl.conf
```

### Application-level

- **More UDP threads** → better receive throughput (up to core count)
- **Larger ring/channel** → absorb burst traffic without drops
- **CPU pinning** (C version does this, Rust can use `taskset`)
- **`RUST_LOG=warn`** → reduce logging overhead in Rust

---

## Project Status

> **Infrastructure scaffolding — ready for implementation.**
> Both projects compile and run but are functionally identical.
> Future work: implement your application-specific sensor processing,
> add TLS, authentication, data validation, and run real benchmarks.

---

## File Structure

```
vad-api/
├── README.md
├── rust-udp-mqtt/               # Rust implementation
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs              # Entry point, wiring
│       ├── config.rs            # CLI config (clap)
│       ├── sensor.rs            # Packet parsing & serialization
│       ├── udp_receiver.rs      # Multi-threaded UDP listener
│       ├── mqtt_publisher.rs    # MQTT QoS 0 publisher
│       └── stats.rs             # Lock-free perf counters
├── c-udp-mqtt/                  # C implementation
│   ├── Makefile
│   ├── include/
│   │   ├── sensor.h             # Packet parsing (inline)
│   │   ├── stats.h              # Atomic perf counters
│   │   └── ring_buffer.h        # Lock-free SPSC ring buffer
│   └── src/
│       └── main.c               # Everything: UDP, MQTT, threads
└── bench/
    ├── udp_load_gen.py          # Python UDP load generator
    └── bench_compare.sh         # Side-by-side benchmark runner
```
