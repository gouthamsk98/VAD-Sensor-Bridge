#!/usr/bin/env python3
"""
UDP Sensor Data Load Generator
Sends binary sensor packets at configurable rate for benchmarking
the Rust and C bridges.

Usage:
    python3 udp_load_gen.py --rate 100000 --duration 30 --port 9000
"""

import argparse
import socket
import struct
import time
import os

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

def main():
    parser = argparse.ArgumentParser(description="UDP Sensor Load Generator")
    parser.add_argument("--host", default="127.0.0.1", help="Target host")
    parser.add_argument("--port", type=int, default=9000, help="Target port")
    parser.add_argument("--rate", type=int, default=50000,
                        help="Packets per second (0 = unlimited)")
    parser.add_argument("--duration", type=int, default=30,
                        help="Test duration in seconds")
    parser.add_argument("--sensors", type=int, default=100,
                        help="Number of simulated sensors")
    parser.add_argument("--payload-size", type=int, default=64,
                        help="Payload size in bytes")
    args = parser.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    target = (args.host, args.port)
    payload = os.urandom(args.payload_size)

    print(f"🚀 Sending {args.rate} pps to {args.host}:{args.port} "
          f"for {args.duration}s ({args.sensors} sensors, "
          f"{args.payload_size}B payload)")

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
                # Busy wait for timing precision
                continue

            sensor_id = seq % args.sensors
            pkt = make_packet(sensor_id, seq, payload)
            sock.sendto(pkt, target)
            seq += 1
            sent += 1
            next_send += interval

            # Print progress every second
            if sent % max(args.rate, 1) == 0:
                rate_actual = sent / elapsed if elapsed > 0 else 0
                print(f"  [{elapsed:.1f}s] sent={sent}, "
                      f"rate={rate_actual:.0f} pps")

    except KeyboardInterrupt:
        pass

    elapsed = time.monotonic() - start
    print(f"\n📊 Done: {sent} packets in {elapsed:.2f}s "
          f"({sent/elapsed:.0f} pps actual)")

    sock.close()

if __name__ == "__main__":
    main()
