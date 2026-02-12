#!/usr/bin/env python3
"""
stream_mic_esp.py — Stream live microphone audio via the ESP protocol
to the Rust server, then compare sent vs server-saved WAV.

This emulates a real ESP32 device sending mic audio (SESSION_START →
AUDIO_UP stream → SESSION_END) but uses your Mac webcam/USB mic as
the audio source instead of PDM.

Saves:
  <audio-dir>/mic_sent.wav     — raw PCM Python captured from the mic
  <audio-dir>/esp_*.wav        — what the Rust server accumulated & saved

After the session ends it runs a byte-level comparison.

Usage:
    # Terminal 1 — start the server
    cd rust-udp-mqtt && cargo run -- --transport udp

    # Terminal 2 — stream from mic
    python3 bench/stream_mic_esp.py --duration 5

Flags:
    --host        server host                  (default 127.0.0.1)
    --port        audio UDP port               (default 9001)
    --duration    seconds to record            (default 5)
    --samplerate  mic sample rate              (default 16000)
    --chunk       samples per ESP packet       (default 700 = 43.75 ms)
    --audio-dir   dir for WAV outputs          (default ./esp_audio)
    --device      input device index           (default: system default)
    --list-devices show available audio devices and exit

Requires: pip install sounddevice
"""

import argparse
import glob
import os
import signal
import socket
import struct
import sys
import threading
import time
import wave

# ═══════════════════════════════════════════════════════════════════════
#  ESP protocol constants (mirrors esp_audio_protocol.rs)
# ═══════════════════════════════════════════════════════════════════════

ESP_HEADER     = 4
PKT_AUDIO_UP   = 0x01
PKT_CONTROL    = 0x03
PKT_HEARTBEAT  = 0x04

FLAG_START = 0x01
FLAG_END   = 0x02

CTRL_SESSION_START = 0x01
CTRL_SESSION_END   = 0x02
CTRL_ACK           = 0x05
CTRL_SERVER_READY  = 0x07


def build_packet(seq, pkt_type, flags, payload=b""):
    return struct.pack("<HBB", seq & 0xFFFF, pkt_type, flags) + payload

def build_control(seq, cmd, flags=0):
    return build_packet(seq, PKT_CONTROL, flags, bytes([cmd]))

def parse_packet(data):
    if len(data) < ESP_HEADER:
        return None
    seq, pkt_type, flags = struct.unpack("<HBB", data[:ESP_HEADER])
    return seq, pkt_type, flags, data[ESP_HEADER:]


# ═══════════════════════════════════════════════════════════════════════
#  WAV helper + comparison (reused from test_esp_protocol.py)
# ═══════════════════════════════════════════════════════════════════════

def write_wav(path, pcm, rate=16000):
    with wave.open(path, "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(rate)
        wf.writeframes(pcm)


def compare_wav_files(path_a, path_b, label_a="A", label_b="B"):
    print(f"\n  ┌─ Comparing: {label_a} vs {label_b}")
    with wave.open(path_a, "rb") as wa:
        rate_a, frames_a = wa.getframerate(), wa.getnframes()
        data_a = wa.readframes(frames_a)
    with wave.open(path_b, "rb") as wb:
        rate_b, frames_b = wb.getframerate(), wb.getnframes()
        data_b = wb.readframes(frames_b)

    dur_a = frames_a / rate_a
    dur_b = frames_b / rate_b
    print(f"  │  {label_a}: {rate_a}Hz {frames_a} frames {len(data_a)}B {dur_a:.3f}s")
    print(f"  │  {label_b}: {rate_b}Hz {frames_b} frames {len(data_b)}B {dur_b:.3f}s")

    min_len = min(len(data_a), len(data_b))
    if min_len == 0:
        print("  │  ⚠️  One file is empty")
        print("  └─")
        return

    if data_a[:min_len] == data_b[:min_len] and len(data_a) == len(data_b):
        print("  │  ✅ Byte-exact match (identical)")
        print("  └─")
        return

    n_samples = min_len // 2
    max_diff = 0
    sum_diff = 0
    diff_count = 0
    for i in range(n_samples):
        sa = struct.unpack_from("<h", data_a, i * 2)[0]
        sb = struct.unpack_from("<h", data_b, i * 2)[0]
        d = abs(sa - sb)
        if d > 0:
            diff_count += 1
        max_diff = max(max_diff, d)
        sum_diff += d

    avg_diff = sum_diff / n_samples if n_samples else 0
    pct = (1.0 - diff_count / n_samples) * 100 if n_samples else 0

    print(f"  │  Samples compared: {n_samples}")
    print(f"  │  Identical: {n_samples - diff_count}/{n_samples} ({pct:.1f}%)")
    print(f"  │  Max diff:  {max_diff}   Avg diff: {avg_diff:.2f}")

    if len(data_a) != len(data_b):
        longer = label_a if len(data_a) > len(data_b) else label_b
        print(f"  │  Size gap: {longer} has {abs(len(data_a)-len(data_b))} extra bytes")

    verdict = "✅ PASS" if (pct >= 99.9 and max_diff <= 1) else "⚠️  DIFFERS"
    print(f"  │  Verdict: {verdict}")
    print("  └─")


# ═══════════════════════════════════════════════════════════════════════
#  Main
# ═══════════════════════════════════════════════════════════════════════

def main():
    ap = argparse.ArgumentParser(
        description="Stream live mic → ESP protocol → Rust server, compare WAVs")
    ap.add_argument("--host",        default="127.0.0.1")
    ap.add_argument("--port",        type=int, default=9001)
    ap.add_argument("--duration",    type=float, default=5.0,
                    help="seconds to record (default 5)")
    ap.add_argument("--samplerate",  type=int, default=16000)
    ap.add_argument("--chunk",       type=int, default=700,
                    help="samples per AUDIO_UP packet (default 700 = 43.75 ms)")
    ap.add_argument("--audio-dir",   default="./esp_audio")
    ap.add_argument("--device",      type=int, default=None,
                    help="input device index (see --list-devices)")
    ap.add_argument("--list-devices", action="store_true",
                    help="show audio devices and exit")
    args = ap.parse_args()

    # ── optional: list devices ─────────────────────────────────────
    try:
        import sounddevice as sd
        import numpy as np
    except ImportError:
        print("❌ sounddevice required:  pip install sounddevice")
        sys.exit(1)

    if args.list_devices:
        print(sd.query_devices())
        sys.exit(0)

    server = (args.host, args.port)
    os.makedirs(args.audio_dir, exist_ok=True)

    # ── banner ─────────────────────────────────────────────────────
    dev_name = "(system default)"
    if args.device is not None:
        dev_name = sd.query_devices(args.device)["name"]

    print("=" * 60)
    print("  ESP Protocol Mic Test — Mic → UDP → Rust → WAV compare")
    print("=" * 60)
    print(f"  Server     : {args.host}:{args.port}")
    print(f"  Mic device : {dev_name}")
    print(f"  Sample rate: {args.samplerate} Hz, 16-bit mono")
    print(f"  Chunk      : {args.chunk} samples ({args.chunk/args.samplerate*1000:.1f} ms)")
    print(f"  Duration   : {args.duration:.1f}s")
    print(f"  Audio dir  : {args.audio_dir}")
    print()

    # ── UDP socket ─────────────────────────────────────────────────
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(3.0)
    sock.bind(("0.0.0.0", 0))

    # ── Snapshot existing server WAVs ──────────────────────────────
    existing_wavs = set(glob.glob(os.path.join(args.audio_dir, "esp_*.wav")))

    # ── 1. SESSION_START ───────────────────────────────────────────
    print("🔹 SESSION_START …", end=" ", flush=True)
    sock.sendto(build_control(0, CTRL_SESSION_START, FLAG_START), server)
    try:
        data, _ = sock.recvfrom(1500)
        r = parse_packet(data)
        if r and r[1] == PKT_CONTROL and r[3] and r[3][0] == CTRL_SERVER_READY:
            print("✅ SERVER_READY")
        else:
            print("❌ unexpected reply")
            sys.exit(1)
    except socket.timeout:
        print("❌ timeout (is the server running?)")
        sys.exit(1)

    # ── 2. Capture mic + stream AUDIO_UP ───────────────────────────
    sent_pcm = bytearray()
    seq = 0
    stop = threading.Event()

    def on_sig(s, f):
        stop.set()
    signal.signal(signal.SIGINT, on_sig)

    chunk_ms = args.chunk / args.samplerate * 1000
    total_chunks = int(args.duration * 1000 / chunk_ms)

    print(f"🎙️  Recording {args.duration:.1f}s ({total_chunks} chunks) …")

    t0 = time.monotonic()
    with sd.InputStream(samplerate=args.samplerate, blocksize=args.chunk,
                        channels=1, dtype="int16", device=args.device) as stream:
        while seq < total_chunks and not stop.is_set():
            frames, overflowed = stream.read(args.chunk)
            pcm = frames.tobytes()

            sent_pcm.extend(pcm)
            seq += 1

            pkt = build_packet(seq, PKT_AUDIO_UP, 0, pcm)
            sock.sendto(pkt, server)

            # live status
            elapsed = time.monotonic() - t0
            print(f"\r   📦 {seq}/{total_chunks}  "
                  f"⏱ {elapsed:.1f}s  "
                  f"📤 {len(sent_pcm)/1024:.0f} KB sent",
                  end="", flush=True)

    dt = time.monotonic() - t0
    print(f"\n   done — {seq} packets in {dt:.2f}s "
          f"({len(sent_pcm)/1024:.1f} KB)")

    # ── 3. SESSION_END ─────────────────────────────────────────────
    print("🔹 SESSION_END …", end=" ", flush=True)
    sock.sendto(build_control(seq + 1, CTRL_SESSION_END, FLAG_END), server)
    try:
        data, _ = sock.recvfrom(1500)
        r = parse_packet(data)
        if r and r[1] == PKT_CONTROL and r[3] and r[3][0] == CTRL_ACK:
            print("✅ ACK")
        else:
            print("⚠️  unexpected reply")
    except socket.timeout:
        print("⚠️  timeout")

    sock.close()

    # ── 4. Save sent WAV ───────────────────────────────────────────
    sent_path = os.path.join(args.audio_dir, "mic_sent.wav")
    write_wav(sent_path, bytes(sent_pcm), args.samplerate)
    sent_dur = len(sent_pcm) / (args.samplerate * 2)
    print(f"💾 Mic audio saved:    {sent_path}  "
          f"({len(sent_pcm)} bytes, {sent_dur:.2f}s)")

    # ── 5. Find server WAV ─────────────────────────────────────────
    time.sleep(0.5)
    current_wavs = set(glob.glob(os.path.join(args.audio_dir, "esp_*.wav")))
    new_wavs = current_wavs - existing_wavs

    if new_wavs:
        server_path = sorted(new_wavs)[-1]
        srv_size = os.path.getsize(server_path)
        print(f"💾 Server audio saved: {server_path}  ({srv_size} bytes)")
    else:
        print("❌ No server WAV file found!")
        sys.exit(1)

    # ── 6. Compare ─────────────────────────────────────────────────
    compare_wav_files(sent_path, server_path,
                      label_a="MIC SENT (Python)",
                      label_b="SERVER SAVED (Rust)")

    # ── 7. Playback instructions ───────────────────────────────────
    print()
    print("=" * 60)
    print("  ▶️  Play sent (mic capture):")
    print(f"     afplay {sent_path}")
    print(f"  ▶️  Play server-saved:")
    print(f"     afplay {server_path}")
    print(f"  ▶️  Play side-by-side (left=sent, right=server) — if ffmpeg installed:")
    merged = os.path.join(args.audio_dir, "mic_compare_stereo.wav")
    print(f"     ffmpeg -y -i {sent_path} -i {server_path} "
          f"-filter_complex amerge=inputs=2 {merged} && afplay {merged}")
    print("=" * 60)
    print()


if __name__ == "__main__":
    main()
