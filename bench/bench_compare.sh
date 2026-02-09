#!/usr/bin/env bash
#
# bench_compare.sh — Run both Rust and C bridges side-by-side and compare
#
# Prerequisites:
#   1. MQTT broker running (mosquitto) on localhost:1883
#   2. Both projects built (cargo build --release, make)
#   3. Python3 for the load generator
#
# Usage:
#   ./bench/bench_compare.sh [pps] [duration_secs]

set -euo pipefail

PPS=${1:-50000}
DURATION=${2:-30}
PORT_RUST=9001
PORT_C=9002
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="$ROOT_DIR/bench/results"
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULT_FILE="$RESULTS_DIR/bench_${TIMESTAMP}.txt"

mkdir -p "$RESULTS_DIR"

# Tee all output to both terminal and results file
exec > >(tee -a "$RESULT_FILE") 2>&1

echo "================================================================="
echo "  VAD Sensor Bridge — Performance Comparison"
echo "  Date:     $(date)"
echo "  Rate:     ${PPS} pps | Duration: ${DURATION}s"
echo "  System:   $(uname -srm)"
echo "  CPU:      $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
echo "  Cores:    $(nproc)"
echo "  RAM:      $(free -h | awk '/Mem:/{print $2}')"
echo "  Results:  $RESULT_FILE"
echo "================================================================="
echo

# ── Pre-flight checks ──
echo "🔍 Checking MQTT broker..."
if ! mosquitto_pub -t "bench/healthcheck" -m "ok" -q 0 2>/dev/null; then
    echo "❌ MQTT broker not reachable on localhost:1883"
    echo "   Run: sudo systemctl start mosquitto"
    exit 1
fi
echo "   ✅ MQTT broker OK"
echo

# ── Build both ──
echo "🔨 Building Rust bridge..."
cd "$ROOT_DIR/rust-udp-mqtt"
cargo build --release 2>&1 | tail -3

echo "🔨 Building C bridge..."
cd "$ROOT_DIR/c-udp-mqtt"
make clean && make 2>&1 | tail -3

echo

# ── Phase 1: Rust ──
echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 1: Rust Bridge"
echo "═══════════════════════════════════════════════════════════════"

cd "$ROOT_DIR"

RUST_LOG_FILE="$RESULTS_DIR/rust_${TIMESTAMP}.log"
RUST_LOG=info ./rust-udp-mqtt/target/release/vad-sensor-bridge \
    --udp-port $PORT_RUST \
    --stats-interval-secs 5 > "$RUST_LOG_FILE" 2>&1 &
RUST_PID=$!
sleep 2

# Verify it connected
if ! kill -0 $RUST_PID 2>/dev/null; then
    echo "❌ Rust bridge failed to start — check $RUST_LOG_FILE"
    cat "$RUST_LOG_FILE"
    exit 1
fi
echo "   Rust bridge PID=$RUST_PID listening on :$PORT_RUST"

python3 "$SCRIPT_DIR/udp_load_gen.py" \
    --port $PORT_RUST \
    --rate $PPS \
    --duration $DURATION

sleep 3
kill $RUST_PID 2>/dev/null || true
wait $RUST_PID 2>/dev/null || true

echo
echo "── Rust Bridge Stats ──"
grep -E "\[STATS\]" "$RUST_LOG_FILE" || echo "(no stats lines captured)"
echo

# ── Phase 2: C ──
echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 2: C Bridge"
echo "═══════════════════════════════════════════════════════════════"

C_LOG_FILE="$RESULTS_DIR/c_${TIMESTAMP}.log"
./c-udp-mqtt/build/vad-sensor-bridge \
    --udp-port $PORT_C \
    --stats-interval 5 > "$C_LOG_FILE" 2>&1 &
C_PID=$!
sleep 2

if ! kill -0 $C_PID 2>/dev/null; then
    echo "❌ C bridge failed to start — check $C_LOG_FILE"
    cat "$C_LOG_FILE"
    exit 1
fi
echo "   C bridge PID=$C_PID listening on :$PORT_C"

python3 "$SCRIPT_DIR/udp_load_gen.py" \
    --port $PORT_C \
    --rate $PPS \
    --duration $DURATION

sleep 3
kill $C_PID 2>/dev/null || true
wait $C_PID 2>/dev/null || true

echo
echo "── C Bridge Stats ──"
grep -E "\[STATS\]" "$C_LOG_FILE" || echo "(no stats lines captured)"
echo

# ── Summary ──
echo "================================================================="
echo "  ✅ Comparison complete!"
echo "================================================================="
echo
echo "  📁 Results saved to:"
echo "     Main:  $RESULT_FILE"
echo "     Rust:  $RUST_LOG_FILE"
echo "     C:     $C_LOG_FILE"
echo
echo "  📊 Quick diff (peak stats snapshot):"
echo "  ────────────────────────────────────────"
# Pick the snapshot with highest UDP pps (peak throughput) for fair comparison
RUST_LAST=$(grep '\[STATS\]' "$RUST_LOG_FILE" | while read -r line; do
    pps=$(echo "$line" | grep -oP 'UDP: \K[0-9]+')
    echo "$pps $line"
done | sort -rn | head -1 | sed 's/^[0-9]* //')
C_LAST=$(grep '\[STATS\]' "$C_LOG_FILE" | while read -r line; do
    pps=$(echo "$line" | grep -oP 'UDP: \K[0-9]+')
    echo "$pps $line"
done | sort -rn | head -1 | sed 's/^[0-9]* //')
echo "  RUST: $RUST_LAST"
echo "  C:    $C_LAST"
echo

# Extract values for side-by-side table
RUST_PPS=$(echo "$RUST_LAST" | grep -oP 'UDP: \K[0-9]+')
RUST_MBPS=$(echo "$RUST_LAST" | grep -oP '[0-9.]+ Mbps' | grep -oP '[0-9.]+')
RUST_MQTT=$(echo "$RUST_LAST" | grep -oP 'MQTT: \K[0-9]+')
RUST_PERR=$(echo "$RUST_LAST" | grep -oP 'parse=\K[0-9]+')
RUST_MERR=$(echo "$RUST_LAST" | grep -oP 'mqtt=\K[0-9]+')
RUST_DROP=$(echo "$RUST_LAST" | grep -oP 'drops=\K[0-9]+')

C_PPS=$(echo "$C_LAST" | grep -oP 'UDP: \K[0-9]+')
C_MBPS=$(echo "$C_LAST" | grep -oP '[0-9.]+ Mbps' | grep -oP '[0-9.]+')
C_MQTT=$(echo "$C_LAST" | grep -oP 'MQTT: \K[0-9]+')
C_PERR=$(echo "$C_LAST" | grep -oP 'parse=\K[0-9]+')
C_MERR=$(echo "$C_LAST" | grep -oP 'mqtt=\K[0-9]+')
C_DROP=$(echo "$C_LAST" | grep -oP 'drops=\K[0-9]+')

printf "  %-20s %12s %12s\n" "Metric" "Rust" "C"
printf "  %-20s %12s %12s\n" "────────────────────" "────────────" "────────────"
printf "  %-20s %10s pps %10s pps\n" "UDP recv rate" "${RUST_PPS:-?}" "${C_PPS:-?}"
printf "  %-20s %9s Mbps %9s Mbps\n" "UDP throughput" "${RUST_MBPS:-?}" "${C_MBPS:-?}"
printf "  %-20s %8s msg/s %8s msg/s\n" "MQTT publish rate" "${RUST_MQTT:-?}" "${C_MQTT:-?}"
printf "  %-20s %12s %12s\n" "Parse errors" "${RUST_PERR:-0}" "${C_PERR:-0}"
printf "  %-20s %12s %12s\n" "MQTT errors" "${RUST_MERR:-0}" "${C_MERR:-0}"
printf "  %-20s %12s %12s\n" "Channel drops" "${RUST_DROP:-0}" "${C_DROP:-0}"
echo "  ────────────────────────────────────────"
echo
echo "  Run again: ./bench/bench_compare.sh $PPS $DURATION"
echo "================================================================="
