#!/usr/bin/env python3
"""
Test VAD-driven prompt mode switching.

Sends emotional sensor vectors with different presets to the sensor port,
cycling through moods so you can see the bridge switch PromptMode and
send session.update to OpenAI in the server logs.

The script sends to the SENSOR port (default 9002), which is where
sensor-vector packets are expected.

Usage:
    # 1) Start the server (with or without OpenAI — logs show mode changes either way):
    #    OPENAI_API_KEY=sk-... cargo run -- --openai-realtime
    #
    # 2) Run this test:
    python3 bench/test_prompt_mode.py

    # Cycle faster:
    python3 bench/test_prompt_mode.py --dwell 2

    # Only test one mood:
    python3 bench/test_prompt_mode.py --moods calm

    # Custom host/port:
    python3 bench/test_prompt_mode.py --host 127.0.0.1 --sensor-port 9002

What to look for in the server logs:
    🧠 updated OpenAI prompt from emotional VAD   mode=Calm
    🧠 updated OpenAI prompt from emotional VAD   mode=Energetic
    🧭 session.update sent (instructions)
"""

import argparse
import math
import socket
import struct
import time

# ── Wire format (matches sensor_header_t) ───────────────────────────
HEADER_FMT = "<IQBxxxHxxQ4x"
HEADER_SIZE = struct.calcsize(HEADER_FMT)  # 32
DATA_TYPE_SENSOR_VECTOR = 2


def build_packet(sensor_id: int, seq: int, data_type: int, payload: bytes) -> bytes:
    timestamp_us = int(time.time() * 1_000_000)
    header = struct.pack(HEADER_FMT, sensor_id, timestamp_us, data_type, len(payload), seq)
    return header + payload


def make_sensor_vector(**kw) -> bytes:
    """Pack 10 f32 LE values into a 40-byte sensor vector payload."""
    keys = [
        "battery_low", "people_count", "known_face", "unknown_face",
        "fall_event", "lifted", "idle_time", "sound_energy",
        "voice_rate", "motion_energy",
    ]
    vals = [float(kw.get(k, 0.0)) for k in keys]
    return struct.pack("<10f", *vals)


# ── Mood presets designed to hit each PromptMode ────────────────────
#
# From transport_udp.rs prompt_mode_from_vad():
#   Angry:      arousal > 0.6  && valence < 0.35 && dominance > 0.5
#   Anxious:    arousal > 0.5  && valence < 0.35 && dominance < 0.35
#   Sad:        arousal < 0.25 && valence < 0.3  && dominance < 0.35
#   Tired:      arousal < 0.2  && valence < 0.4
#   Calm:       arousal < 0.25 && valence < 0.5
#   Energetic:  arousal > 0.7  && valence > 0.6
#   Playful:    arousal > 0.45 && valence > 0.55 && dominance > 0.45
#   Supportive: arousal > 0.5  && valence < 0.4
#   Friendly:   valence > 0.6
#   Neutral:    everything else
#
# The actual V/A/D values are computed from the sensor vector via
# the weight matrices in vad.rs, so we pick inputs that produce the
# right ranges.

MOOD_PRESETS = {
    # ── Angry (low V, high A, high D) ───────────────────────────────
    # loud yelling, unknown faces, grabbed → frustrated / angry
    "angry": dict(
        battery_low=0.0, people_count=0.4, known_face=0.0, unknown_face=0.6,
        fall_event=0.0, lifted=0.3, idle_time=0.0, sound_energy=0.9,
        voice_rate=0.8, motion_energy=0.7,
    ),
    # ── Anxious (low V, high A, low D) ──────────────────────────────
    # threats, low battery, unknown face, fall → panicky
    "anxious": dict(
        battery_low=0.7, people_count=0.1, known_face=0.0, unknown_face=0.7,
        fall_event=0.6, lifted=0.3, idle_time=0.0, sound_energy=0.5,
        voice_rate=0.1, motion_energy=0.6,
    ),
    # ── Sad (low V, low A, low D) ───────────────────────────────────
    # alone, idle, low battery, silence
    "sad": dict(
        battery_low=0.5, people_count=0.0, known_face=0.0, unknown_face=0.0,
        fall_event=0.0, lifted=0.0, idle_time=0.95, sound_energy=0.0,
        voice_rate=0.0, motion_energy=0.0,
    ),
    # ── Tired (very low A, low V) ───────────────────────────────────
    # low battery, idle, no activity at all
    "tired": dict(
        battery_low=0.95, people_count=0.0, known_face=0.0, unknown_face=0.0,
        fall_event=0.0, lifted=0.0, idle_time=0.8, sound_energy=0.05,
        voice_rate=0.05, motion_energy=0.05,
    ),
    # ── Calm (low V, low A) ─────────────────────────────────────────
    # idle + alone → low arousal, low-ish valence
    "calm": dict(
        battery_low=0.3, people_count=0.0, known_face=0.0, unknown_face=0.0,
        fall_event=0.0, lifted=0.0, idle_time=0.95, sound_energy=0.05,
        voice_rate=0.0, motion_energy=0.05,
    ),
    # ── Energetic (high V, high A) ──────────────────────────────────
    # lots of people, known faces, sound, motion
    "energetic": dict(
        battery_low=0.0, people_count=0.8, known_face=0.7, unknown_face=0.1,
        fall_event=0.0, lifted=0.0, idle_time=0.0, sound_energy=0.7,
        voice_rate=0.6, motion_energy=0.8,
    ),
    # ── Playful (high V, moderate-high A, moderate-high D) ──────────
    # known faces, company, some motion and sound
    "playful": dict(
        battery_low=0.0, people_count=0.6, known_face=0.6, unknown_face=0.0,
        fall_event=0.0, lifted=0.0, idle_time=0.0, sound_energy=0.5,
        voice_rate=0.5, motion_energy=0.5,
    ),
    # ── Supportive (low V, high A) ──────────────────────────────────
    # threat scenario: unknown face + fall + loud
    "supportive": dict(
        battery_low=0.0, people_count=0.3, known_face=0.0, unknown_face=0.8,
        fall_event=0.5, lifted=0.0, idle_time=0.0, sound_energy=0.6,
        voice_rate=0.1, motion_energy=0.7,
    ),
    # ── Friendly (high V, moderate A) ───────────────────────────────
    # known face, some talk, relaxed
    "friendly": dict(
        battery_low=0.0, people_count=0.5, known_face=0.8, unknown_face=0.0,
        fall_event=0.0, lifted=0.0, idle_time=0.2, sound_energy=0.3,
        voice_rate=0.5, motion_energy=0.2,
    ),
    # ── Neutral (middle ground) ─────────────────────────────────────
    "neutral": dict(
        battery_low=0.2, people_count=0.2, known_face=0.2, unknown_face=0.1,
        fall_event=0.0, lifted=0.0, idle_time=0.3, sound_energy=0.2,
        voice_rate=0.2, motion_energy=0.2,
    ),
}


def main():
    parser = argparse.ArgumentParser(
        description="Test VAD-driven prompt mode switching",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--sensor-port", type=int, default=9002,
                        help="Sensor UDP port (default: 9002)")
    parser.add_argument("--sensor-id", type=int, default=42)
    parser.add_argument("--moods", nargs="*", default=list(MOOD_PRESETS.keys()),
                        help="Moods to cycle through (default: all)")
    parser.add_argument("--dwell", type=float, default=3.0,
                        help="Seconds to dwell on each mood (default: 3)")
    parser.add_argument("--rate", type=float, default=2.0,
                        help="Packets per second within each dwell (default: 2)")
    parser.add_argument("--cycles", type=int, default=2,
                        help="Number of full cycles (default: 2)")
    args = parser.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    target = (args.host, args.sensor_port)
    interval = 1.0 / args.rate if args.rate > 0 else 0
    seq = 0

    print(f"🎯 Target: {args.host}:{args.sensor_port}")
    print(f"📋 Moods:  {', '.join(args.moods)}")
    print(f"⏱  Dwell:  {args.dwell}s per mood, {args.rate} pps")
    print(f"🔁 Cycles: {args.cycles}")
    print()

    for cycle in range(args.cycles):
        print(f"━━━ Cycle {cycle + 1}/{args.cycles} ━━━")
        for mood in args.moods:
            if mood not in MOOD_PRESETS:
                print(f"  ⚠️  Unknown mood '{mood}', skipping")
                continue

            payload = make_sensor_vector(**MOOD_PRESETS[mood])
            pkts_per_dwell = max(1, int(args.dwell * args.rate))

            # Show expected V/A/D for reference
            vals = struct.unpack("<10f", payload)
            print(f"  🎭 {mood.upper():12s}  → sending {pkts_per_dwell} packets ...")

            for i in range(pkts_per_dwell):
                pkt = build_packet(args.sensor_id, seq, DATA_TYPE_SENSOR_VECTOR, payload)
                sock.sendto(pkt, target)
                seq += 1
                if i < pkts_per_dwell - 1 and interval > 0:
                    time.sleep(interval)

            # Small gap between moods
            time.sleep(0.5)

        print()

    sock.close()
    print(f"✅ Done — sent {seq} sensor packets total")
    print()
    print("Check the server logs for lines like:")
    print("  🧠 updated OpenAI prompt from emotional VAD   mode=Calm")
    print("  🧭 session.update sent (instructions)")


if __name__ == "__main__":
    main()
