#!/usr/bin/env bash
#
# bench_compare.sh — Run Rust vs C comparison across UDP, TCP, and MQTT transports
#
# Prerequisites:
#   1. MQTT broker (mosquitto) on localhost:1883
#   2. Both projects built (cargo build --release, make)
#   3. Python3 + paho-mqtt (pip3 install paho-mqtt) for MQTT load gen
#
# Usage:
#   ./bench/bench_compare.sh [pps] [duration_secs] [mqtt_pps]

set -euo pipefail

PPS=${1:-50000}
DURATION=${2:-20}
MQTT_PPS=${3:-1000}  # MQTT rate (broker-limited, default 1k)
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="$ROOT_DIR/bench/results"
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULT_FILE="$RESULTS_DIR/bench_${TIMESTAMP}.txt"

# Ports — different for Rust vs C to avoid conflicts
PORT_BASE_RUST=9001
PORT_BASE_C=9002

RUST_BIN="$ROOT_DIR/rust-udp-mqtt/target/release/vad-sensor-bridge"
C_BIN="$ROOT_DIR/c-udp-mqtt/build/vad-sensor-bridge"
LOAD_GEN="$SCRIPT_DIR/load_gen.py"

mkdir -p "$RESULTS_DIR"
exec > >(tee -a "$RESULT_FILE") 2>&1

echo "================================================================="
echo "  VAD Sensor Bridge — Multi-Transport Performance Comparison"
echo "  Date:     $(date)"
echo "  Rate:     UDP/TCP: ${PPS} pps | MQTT: ${MQTT_PPS} pps"
echo "  Duration: ${DURATION}s"
echo "  System:   $(uname -srm)"
echo "  CPU:      $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
echo "  Cores:    $(nproc)"
echo "  RAM:      $(free -h | awk '/Mem:/{print $2}')"
echo "  Results:  $RESULT_FILE"
echo "================================================================="
echo
echo "  ⚠ MQTT Note: mosquitto broker is single-threaded and delivers"
echo "    messages in bursts at high rates. MQTT benchmarks measure total"
echo "    messages processed (including post-load drain), not real-time pps."
echo

# ── Pre-flight ──
echo "🔍 Pre-flight checks..."
if ! mosquitto_pub -t "bench/healthcheck" -m "ok" -q 0 2>/dev/null; then
    echo "❌ MQTT broker not reachable. Run: sudo systemctl start mosquitto"
    exit 1
fi
echo "   ✅ MQTT broker OK"

echo "🔨 Building Rust..."
cd "$ROOT_DIR/rust-udp-mqtt" && cargo build --release 2>&1 | tail -3
echo "🔨 Building C..."
cd "$ROOT_DIR/c-udp-mqtt" && make clean && make 2>&1 | tail -3
echo

cd "$ROOT_DIR"

# ── Helper: kill a process and its group ──
kill_bridge() {
    local PID="$1"
    kill -TERM -- -$PID 2>/dev/null || kill -TERM $PID 2>/dev/null || true
    sleep 1
    kill -9 -- -$PID 2>/dev/null || kill -9 $PID 2>/dev/null || true
    wait $PID 2>/dev/null || true
}

# ── Helper function to run a single benchmark ──
run_bench() {
    local LANG="$1"       # rust or c
    local TRANSPORT="$2"  # udp, tcp, mqtt
    local BIN="$3"
    local PORT="$4"
    local RATE="$5"
    local LOG_FILE="$RESULTS_DIR/${LANG}_${TRANSPORT}_${TIMESTAMP}.log"
    local DRAIN_WAIT=6

    # MQTT needs longer drain time for broker to flush buffered messages
    if [ "$TRANSPORT" = "mqtt" ]; then
        DRAIN_WAIT=15
    fi

    echo "───────────────────────────────────────────────────────────────"
    echo "  ${LANG^^} + ${TRANSPORT^^}  (${RATE} pps × ${DURATION}s)"
    echo "───────────────────────────────────────────────────────────────"

    # Start bridge in a new session group (immune to SIGINT from this shell)
    if [ "$LANG" = "rust" ]; then
        setsid bash -c "RUST_LOG=info $BIN \
            --transport $TRANSPORT \
            --port $PORT \
            --stats-interval-secs 5" > "$LOG_FILE" 2>&1 &
    else
        setsid "$BIN" \
            --transport "$TRANSPORT" \
            --port "$PORT" \
            --stats-interval 5 > "$LOG_FILE" 2>&1 &
    fi
    local PID=$!
    sleep 2

    if ! kill -0 $PID 2>/dev/null; then
        echo "   ❌ Failed to start — check $LOG_FILE"
        cat "$LOG_FILE"
        return 1
    fi
    echo "   PID=$PID listening on :$PORT via $TRANSPORT"

    # Send load
    if [ "$TRANSPORT" = "mqtt" ]; then
        python3 "$LOAD_GEN" --transport mqtt --rate "$RATE" --duration "$DURATION" 2>&1 || true
    else
        python3 "$LOAD_GEN" --transport "$TRANSPORT" --port "$PORT" --rate "$RATE" --duration "$DURATION" 2>&1 || true
    fi

    echo "   ⏳ Waiting ${DRAIN_WAIT}s for processing to complete..."
    sleep "$DRAIN_WAIT"

    kill_bridge $PID

    echo
    echo "── Stats (${LANG^^} ${TRANSPORT^^}) ──"
    grep -E "\[STATS\]" "$LOG_FILE" || echo "(no stats)"

    # For MQTT, also show total messages processed across all intervals
    if [ "$TRANSPORT" = "mqtt" ]; then
        local TOTAL_PROC=0
        local TOTAL_RECV=0
        while IFS= read -r line; do
            local proc_val
            local recv_val
            proc_val=$(echo "$line" | grep -oP '[0-9]+ proc/s' | grep -oP '^[0-9]+' || echo 0)
            recv_val=$(echo "$line" | grep -oP ': [0-9]+ pps' | grep -oP '[0-9]+' || echo 0)
            TOTAL_PROC=$((TOTAL_PROC + proc_val * 5))  # approximate: pps × interval(5s)
            TOTAL_RECV=$((TOTAL_RECV + recv_val * 5))
        done < <(grep '\[STATS\]' "$LOG_FILE" 2>/dev/null || true)
        local EXPECTED=$((RATE * DURATION))
        echo "   📬 MQTT totals (approx): ~${TOTAL_RECV} recv, ~${TOTAL_PROC} processed (expected: ${EXPECTED})"
    fi
    echo
}

# ── Declare arrays for results ──
declare -A RESULTS

extract_peak() {
    local LOG_FILE="$1"
    local KEY="$2"
    local PEAK_LINE
    PEAK_LINE=$(grep '\[STATS\]' "$LOG_FILE" 2>/dev/null | while read -r line; do
        pps=$(echo "$line" | grep -oP ': \K[0-9]+(?= pps)')
        echo "$pps $line"
    done | sort -rn | head -1 | sed 's/^[0-9]* //')
    RESULTS[$KEY]="$PEAK_LINE"
}

# ── Run all 6 benchmarks: 2 langs × 3 transports ──
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Phase 1: UDP (${PPS} pps)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
run_bench "rust" "udp" "$RUST_BIN" "$PORT_BASE_RUST" "$PPS"
run_bench "c"    "udp" "$C_BIN"    "$PORT_BASE_C"    "$PPS"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Phase 2: TCP (${PPS} pps)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
run_bench "rust" "tcp" "$RUST_BIN" "$PORT_BASE_RUST" "$PPS"
run_bench "c"    "tcp" "$C_BIN"    "$PORT_BASE_C"    "$PPS"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Phase 3: MQTT (${MQTT_PPS} pps — broker-limited)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
run_bench "rust" "mqtt" "$RUST_BIN" "$PORT_BASE_RUST" "$MQTT_PPS"
run_bench "c"    "mqtt" "$C_BIN"    "$PORT_BASE_C"    "$MQTT_PPS"

# ── Extract peak results ──
TRANSPORTS=("udp" "tcp" "mqtt")
for TRANSPORT in "${TRANSPORTS[@]}"; do
    extract_peak "$RESULTS_DIR/rust_${TRANSPORT}_${TIMESTAMP}.log" "rust_${TRANSPORT}"
    extract_peak "$RESULTS_DIR/c_${TRANSPORT}_${TIMESTAMP}.log" "c_${TRANSPORT}"
done

# ── Summary table ──
echo "================================================================="
echo "  📊 Multi-Transport Performance Comparison (peak snapshot)"
echo "================================================================="
echo

# Helper to extract a field
get_field() {
    local LINE="$1"
    local PATTERN="$2"
    echo "$LINE" | grep -oP "$PATTERN" | head -1
}

printf "  %-12s | %-28s | %-28s\n" "Transport" "Rust" "C"
printf "  %-12s-+-%-28s-+-%-28s\n" "------------" "----------------------------" "----------------------------"

for TRANSPORT in "${TRANSPORTS[@]}"; do
    RUST_LINE="${RESULTS[rust_${TRANSPORT}]:-}"
    C_LINE="${RESULTS[c_${TRANSPORT}]:-}"

    R_PPS=$(get_field "$RUST_LINE" ': [0-9]+ pps' | grep -oP '[0-9]+' || echo "?")
    R_MBPS=$(get_field "$RUST_LINE" '[0-9.]+ Mbps' | grep -oP '[0-9.]+' || echo "?")
    R_PROC=$(get_field "$RUST_LINE" '[0-9]+ proc/s' | grep -oP '[0-9]+' || echo "?")
    R_DROP=$(get_field "$RUST_LINE" 'drops=[0-9]+' | grep -oP '[0-9]+' || echo "?")

    C_PPS=$(get_field "$C_LINE" ': [0-9]+ pps' | grep -oP '[0-9]+' || echo "?")
    C_MBPS=$(get_field "$C_LINE" '[0-9.]+ Mbps' | grep -oP '[0-9.]+' || echo "?")
    C_PROC=$(get_field "$C_LINE" '[0-9]+ proc/s' | grep -oP '[0-9]+' || echo "?")
    C_DROP=$(get_field "$C_LINE" 'drops=[0-9]+' | grep -oP '[0-9]+' || echo "?")

    RUST_SUM="${R_PPS}pps ${R_MBPS}Mbps ${R_PROC}vad/s d=${R_DROP}"
    C_SUM="${C_PPS}pps ${C_MBPS}Mbps ${C_PROC}vad/s d=${C_DROP}"

    printf "  %-12s | %-28s | %-28s\n" "${TRANSPORT^^}" "$RUST_SUM" "$C_SUM"
done

echo
echo "  📁 Full results: $RESULT_FILE"
echo "  📁 Individual logs: $RESULTS_DIR/"
echo "  🔁 Run again: ./bench/bench_compare.sh $PPS $DURATION $MQTT_PPS"
echo
echo "  ⚠ MQTT peak numbers reflect burst processing (broker delivers"
echo "    messages in batches). Real-time MQTT throughput is ~10-30 pps"
echo "    due to mosquitto being single-threaded. Both Rust and C process"
echo "    the backlog at 20k+ pps once the broker delivers."
echo "================================================================="
