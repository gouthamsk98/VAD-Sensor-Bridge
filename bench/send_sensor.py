#!/usr/bin/env python3
"""
Simple UDP sensor data sender for testing VAD output.

Sends sensor packets to the vad-sensor-bridge over UDP so you can see
VAD results in the bridge logs.

Modes:
  audio    – sends 16-bit PCM audio samples (data_type=1) → audio RMS VAD
  emotion  – sends 10×f32 sensor vector   (data_type=2) → emotional V/A/D

Usage:
  # Send a loud audio burst (will trigger is_active=true)
  python3 send_sensor.py --mode audio --amplitude 5000

  # Send a quiet audio signal (below VAD threshold)
  python3 send_sensor.py --mode audio --amplitude 5

  # Send emotional sensor vector with high arousal
  python3 send_sensor.py --mode emotion --arousal 0.8 --valence 0.6

  # Send 20 packets at 2 pps
  python3 send_sensor.py --mode emotion --count 20 --rate 2

  # Custom host/port
  python3 send_sensor.py --host 127.0.0.1 --port 9000 --mode audio
"""

import argparse
import math
import random
import socket
import struct
import time


# ── Wire format ─────────────────────────────────────────────────────
# Matches sensor_header_t (32 bytes, packed, little-endian):
#   [ sensor_id: u32 ][ timestamp_us: u64 ][ data_type: u8 ][ reserved: 3 ]
#   [ payload_len: u16 ][ reserved: 2 ][ seq: u64 ][ padding: 4 ]
HEADER_FMT = "<IQBxxxHxxQ4x"
HEADER_SIZE = struct.calcsize(HEADER_FMT)  # 32

DATA_TYPE_AUDIO = 1
DATA_TYPE_SENSOR_VECTOR = 2


def build_packet(sensor_id: int, seq: int, data_type: int, payload: bytes) -> bytes:
    """Build a binary sensor packet matching the bridge wire format."""
    timestamp_us = int(time.time() * 1_000_000)
    header = struct.pack(
        HEADER_FMT,
        sensor_id,
        timestamp_us,
        data_type,
        len(payload),
        seq,
    )
    return header + payload


# ── Audio payload ───────────────────────────────────────────────────

def make_audio_payload(amplitude: float, n_samples: int = 160) -> bytes:
    """Generate a 16-bit LE PCM sine-wave payload.

    `amplitude` controls the peak sample value (0–32767).
    160 samples ≈ 10 ms at 16 kHz – a typical VAD frame.
    """
    buf = bytearray(n_samples * 2)
    for i in range(n_samples):
        sample = int(amplitude * math.sin(2 * math.pi * 440 * i / 16000))
        sample = max(-32768, min(32767, sample))
        struct.pack_into("<h", buf, i * 2, sample)
    return bytes(buf)


# ── Emotional sensor vector payload ────────────────────────────────
#  Channel order (10 × f32 LE, each 0.0–1.0):
#    0 battery_low   1 people_count  2 known_face   3 unknown_face
#    4 fall_event    5 lifted        6 idle_time     7 sound_energy
#    8 voice_rate    9 motion_energy

CHANNEL_NAMES = [
    "battery_low", "people_count", "known_face", "unknown_face",
    "fall_event", "lifted", "idle_time", "sound_energy",
    "voice_rate", "motion_energy",
]


def make_sensor_vector_payload(
    battery_low: float = 0.0,
    people_count: float = 0.0,
    known_face: float = 0.0,
    unknown_face: float = 0.0,
    fall_event: float = 0.0,
    lifted: float = 0.0,
    idle_time: float = 0.0,
    sound_energy: float = 0.0,
    voice_rate: float = 0.0,
    motion_energy: float = 0.0,
) -> bytes:
    """Pack 10 f32 LE values into a 40-byte sensor vector payload."""
    return struct.pack(
        "<10f",
        battery_low, people_count, known_face, unknown_face,
        fall_event, lifted, idle_time, sound_energy,
        voice_rate, motion_energy,
    )


def make_emotion_preset(name: str) -> bytes:
    """Return a sensor vector for common emotional scenarios."""
    presets = {
        # Scenario                          bat  ppl  kno  unk  fal  lft  idl  snd  voi  mot
        "happy":      make_sensor_vector_payload(0.0, 0.6, 0.8, 0.0, 0.0, 0.0, 0.0, 0.4, 0.7, 0.3),
        "scared":     make_sensor_vector_payload(0.0, 0.3, 0.0, 0.8, 0.5, 0.0, 0.0, 0.6, 0.1, 0.7),
        "calm":       make_sensor_vector_payload(0.0, 0.2, 0.5, 0.0, 0.0, 0.0, 0.6, 0.1, 0.2, 0.1),
        "angry":      make_sensor_vector_payload(0.0, 0.4, 0.0, 0.6, 0.0, 0.3, 0.0, 0.8, 0.8, 0.6),
        "lonely":     make_sensor_vector_payload(0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.9, 0.0, 0.0, 0.0),
        "excited":    make_sensor_vector_payload(0.0, 0.8, 0.7, 0.1, 0.0, 0.0, 0.0, 0.7, 0.6, 0.8),
        "random":     make_sensor_vector_payload(*[random.random() for _ in range(10)]),
    }
    if name not in presets:
        print(f"Unknown preset '{name}'. Available: {', '.join(presets.keys())}")
        raise SystemExit(1)
    return presets[name]


# ── Main ────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="Send sensor data over UDP to vad-sensor-bridge",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s --mode audio --amplitude 5000          # loud → active
  %(prog)s --mode audio --amplitude 5             # quiet → inactive
  %(prog)s --mode emotion --preset happy          # happy robot
  %(prog)s --mode emotion --preset scared         # scared robot
  %(prog)s --mode emotion --arousal 0.9           # high arousal custom
  %(prog)s --mode emotion --preset random -n 50   # 50 random packets
""",
    )
    parser.add_argument("--host", default="127.0.0.1", help="Target host (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=9000, help="Target UDP port (default: 9000)")
    parser.add_argument("--mode", choices=["audio", "emotion"], default="audio",
                        help="Data mode: audio (PCM) or emotion (sensor vector)")
    parser.add_argument("--sensor-id", type=int, default=1, help="Sensor ID (default: 1)")

    # Audio options
    parser.add_argument("--amplitude", type=float, default=5000,
                        help="PCM sine amplitude 0–32767 (default: 5000)")
    parser.add_argument("--samples", type=int, default=160,
                        help="Number of PCM samples per packet (default: 160)")

    # Emotion options
    parser.add_argument("--preset", type=str, default=None,
                        help="Emotion preset: happy, scared, calm, angry, lonely, excited, random")
    parser.add_argument("--battery-low", type=float, default=0.0)
    parser.add_argument("--people-count", type=float, default=0.0)
    parser.add_argument("--known-face", type=float, default=0.0)
    parser.add_argument("--unknown-face", type=float, default=0.0)
    parser.add_argument("--fall-event", type=float, default=0.0)
    parser.add_argument("--lifted", type=float, default=0.0)
    parser.add_argument("--idle-time", type=float, default=0.0)
    parser.add_argument("--sound-energy", type=float, default=0.0)
    parser.add_argument("--voice-rate", type=float, default=0.0)
    parser.add_argument("--motion-energy", type=float, default=0.0)
    parser.add_argument("--arousal", type=float, default=None,
                        help="Shortcut: sets sound_energy, motion_energy, voice_rate to this value")

    # Sending options
    parser.add_argument("-n", "--count", type=int, default=5,
                        help="Number of packets to send (default: 5)")
    parser.add_argument("--rate", type=float, default=1.0,
                        help="Packets per second (default: 1)")

    args = parser.parse_args()

    # Build payloads
    if args.mode == "audio":
        data_type = DATA_TYPE_AUDIO
        payload = make_audio_payload(args.amplitude, args.samples)
        print(f"📡 Audio mode: amplitude={args.amplitude}, samples={args.samples}, "
              f"payload={len(payload)} bytes")
    else:
        data_type = DATA_TYPE_SENSOR_VECTOR
        if args.preset:
            payload = make_emotion_preset(args.preset)
            print(f"📡 Emotion mode: preset={args.preset}")
        else:
            # Apply arousal shortcut
            if args.arousal is not None:
                args.sound_energy = args.arousal
                args.motion_energy = args.arousal
                args.voice_rate = args.arousal * 0.8
            payload = make_sensor_vector_payload(
                battery_low=args.battery_low,
                people_count=args.people_count,
                known_face=args.known_face,
                unknown_face=args.unknown_face,
                fall_event=args.fall_event,
                lifted=args.lifted,
                idle_time=args.idle_time,
                sound_energy=args.sound_energy,
                voice_rate=args.voice_rate,
                motion_energy=args.motion_energy,
            )
            channels = struct.unpack("<10f", payload)
            print(f"📡 Emotion mode (custom):")
            for name, val in zip(CHANNEL_NAMES, channels):
                if val > 0:
                    print(f"    {name:16s} = {val:.2f}")
        print(f"    payload = {len(payload)} bytes")

    # Send packets
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    target = (args.host, args.port)
    interval = 1.0 / args.rate if args.rate > 0 else 0

    print(f"🎯 Sending {args.count} packets to {args.host}:{args.port} "
          f"(sensor_id={args.sensor_id}, rate={args.rate} pps)\n")

    for seq in range(args.count):
        # For random preset, regenerate each packet
        if args.mode == "emotion" and args.preset == "random":
            payload = make_emotion_preset("random")

        pkt = build_packet(args.sensor_id, seq, data_type, payload)
        sock.sendto(pkt, target)

        print(f"  ✉ seq={seq:4d}  size={len(pkt):4d}B  "
              f"type={'audio' if data_type == 1 else 'emotion'}")

        if seq < args.count - 1 and interval > 0:
            time.sleep(interval)

    sock.close()
    print(f"\n✅ Done — sent {args.count} packets")


if __name__ == "__main__":
    main()
