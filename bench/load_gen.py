#!/usr/bin/env python3
"""
Multi-transport Sensor Data Load Generator

Sends binary sensor packets via UDP, TCP, or MQTT for benchmarking.

Usage:
    python3 load_gen.py --transport udp --rate 50000 --duration 30 --port 9000
    python3 load_gen.py --transport tcp --rate 50000 --duration 30 --port 9000
    python3 load_gen.py --transport mqtt --rate 50000 --duration 30 --mqtt-host 127.0.0.1
"""

import argparse
import socket
import struct
import time
import os
import json

HEADER_FORMAT = "<IQBxxxHxxQ4x"  # matches sensor_header_t (32 bytes)
HEADER_SIZE = struct.calcsize(HEADER_FORMAT)


def make_packet(sensor_id: int, seq: int, payload: bytes) -> bytes:
    """Build a binary sensor packet matching the wire format."""
    timestamp_us = int(time.time() * 1_000_000)
    header = struct.pack(
        HEADER_FORMAT,
        sensor_id,
        timestamp_us,
        1,  # data_type
        len(payload),
        seq,
    )
    return header + payload


def run_udp(args, payload):
    """Send sensor packets via UDP."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    target = (args.host, args.port)

    print(f"🚀 [UDP] Sending {args.rate} pps to {args.host}:{args.port} "
          f"for {args.duration}s ({args.sensors} sensors, {args.payload_size}B payload)")

    return send_loop(args, lambda pkt: sock.sendto(pkt, target), payload)


def run_tcp(args, payload):
    """Send sensor packets via TCP with length-prefix framing."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    sock.connect((args.host, args.port))

    print(f"🚀 [TCP] Sending {args.rate} pps to {args.host}:{args.port} "
          f"for {args.duration}s ({args.sensors} sensors, {args.payload_size}B payload)")

    def send_tcp(pkt):
        # Length-prefix: [u32 LE total_len][packet_data]
        frame = struct.pack("<I", len(pkt)) + pkt
        sock.sendall(frame)

    result = send_loop(args, send_tcp, payload)
    sock.close()
    return result


def run_mqtt(args, payload):
    """Send sensor packets via MQTT publish."""
    try:
        import paho.mqtt.client as mqtt
    except ImportError:
        print("❌ paho-mqtt not installed. Run: pip3 install paho-mqtt")
        return

    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id="vad-loadgen", protocol=mqtt.MQTTv311)
    client.connect(args.mqtt_host, args.mqtt_port, keepalive=60)
    client.loop_start()
    time.sleep(0.5)  # wait for connection

    topic_prefix = args.mqtt_topic_prefix

    print(f"🚀 [MQTT] Sending {args.rate} pps to {args.mqtt_host}:{args.mqtt_port} "
          f"for {args.duration}s ({args.sensors} sensors, {args.payload_size}B payload)")

    def send_mqtt(pkt):
        sensor_id = struct.unpack_from("<I", pkt, 0)[0]
        topic = f"{topic_prefix}/{sensor_id}"
        client.publish(topic, pkt, qos=0)

    result = send_loop(args, send_mqtt, payload)
    client.loop_stop()
    client.disconnect()
    return result


def send_loop(args, send_fn, payload):
    """Common send loop with rate limiting and progress reporting."""
    seq = 0
    sent = 0
    start = time.monotonic()
    interval = 1.0 / args.rate if args.rate > 0 else 0
    next_send = start

    try:
        while True:
            now = time.monotonic()
            elapsed = now - start
            if elapsed >= args.duration:
                break

            if args.rate > 0 and now < next_send:
                continue  # busy wait for timing precision

            sensor_id = seq % args.sensors
            pkt = make_packet(sensor_id, seq, payload)
            send_fn(pkt)
            seq += 1
            sent += 1
            next_send += interval

            if sent % max(args.rate, 1) == 0:
                rate_actual = sent / elapsed if elapsed > 0 else 0
                print(f"  [{elapsed:.1f}s] sent={sent}, rate={rate_actual:.0f} pps")

    except KeyboardInterrupt:
        pass

    elapsed = time.monotonic() - start
    print(f"\n📊 Done: {sent} packets in {elapsed:.2f}s "
          f"({sent / elapsed:.0f} pps actual)")
    return sent


def main():
    parser = argparse.ArgumentParser(description="Multi-Transport Sensor Load Generator")
    parser.add_argument("--transport", choices=["udp", "tcp", "mqtt"],
                        default="udp", help="Transport protocol")
    parser.add_argument("--host", default="127.0.0.1", help="Target host (UDP/TCP)")
    parser.add_argument("--port", type=int, default=9000, help="Target port (UDP/TCP)")
    parser.add_argument("--mqtt-host", default="127.0.0.1", help="MQTT broker host")
    parser.add_argument("--mqtt-port", type=int, default=1883, help="MQTT broker port")
    parser.add_argument("--mqtt-topic-prefix", default="vad/sensors",
                        help="MQTT topic prefix")
    parser.add_argument("--rate", type=int, default=50000,
                        help="Packets per second (0 = unlimited)")
    parser.add_argument("--duration", type=int, default=30,
                        help="Test duration in seconds")
    parser.add_argument("--sensors", type=int, default=100,
                        help="Number of simulated sensors")
    parser.add_argument("--payload-size", type=int, default=64,
                        help="Payload size in bytes")
    args = parser.parse_args()

    payload = os.urandom(args.payload_size)

    if args.transport == "udp":
        run_udp(args, payload)
    elif args.transport == "tcp":
        run_tcp(args, payload)
    elif args.transport == "mqtt":
        run_mqtt(args, payload)


if __name__ == "__main__":
    main()
