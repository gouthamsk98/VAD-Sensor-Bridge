#!/bin/bash
# Self-contained MQTT test — run from project root
# Usage: bash bench/test_mqtt.sh

cd "$(dirname "$0")/.."

LOG_DIR="bench/results"
mkdir -p "$LOG_DIR"

echo "=== Testing C MQTT ==="

# Kill any old instances
pkill -f vad-sensor-bridge 2>/dev/null || true
sleep 0.5

# Start bridge
./c-udp-mqtt/build/vad-sensor-bridge --transport mqtt --stats-interval 3 \
    > "$LOG_DIR/c_mqtt_test.log" 2>&1 &
BPID=$!

sleep 3

# Send load (longer duration for steady state)
/usr/bin/python3 bench/load_gen.py --transport mqtt --rate 5000 --duration 20 \
    > "$LOG_DIR/lg_mqtt_test.log" 2>&1 || true

# Wait for remaining messages to be delivered and processed
sleep 10

# Kill bridge
kill $BPID 2>/dev/null || true
wait $BPID 2>/dev/null || true

echo ""
echo "=== C MQTT Bridge Output ==="
cat "$LOG_DIR/c_mqtt_test.log"
echo ""
echo "=== Load Gen Output ==="
cat "$LOG_DIR/lg_mqtt_test.log"
echo ""
echo "=== TEST COMPLETE ==="
