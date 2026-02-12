#!/usr/bin/env python3
"""
test_esp_protocol.py — simulate an ESP32 audio session against the Rust server.

Exercises the full session lifecycle:
  1. HEARTBEAT  → expect heartbeat echo
  2. SESSION_START → expect SERVER_READY
  3. Stream AUDIO_UP packets (440 Hz sine tone, ~3 s)
  4. SESSION_END → expect ACK
  5. Verify WAV was saved on disk
  6. Compare: source (sent) vs server-saved WAV

Saves two WAV files for comparison:
  - <audio-dir>/test_sent.wav       — the PCM Python actually sent
  - <audio-dir>/esp_*.wav           — what the Rust server accumulated & saved

Usage:
    python3 bench/test_esp_protocol.py [--host 127.0.0.1] [--port 9001]
                                       [--duration 3.0] [--audio-dir /tmp/esp_audio]
"""

import argparse
import glob
import math
import os
import socket
import struct
import sys
import time
import wave

# ── Protocol constants (must mirror esp_audio_protocol.rs) ──────────

ESP_HEADER = 4  # bytes

PKT_AUDIO_UP   = 0x01
PKT_AUDIO_DOWN = 0x02
PKT_CONTROL    = 0x03
PKT_HEARTBEAT  = 0x04

FLAG_START  = 0x01
FLAG_END    = 0x02
FLAG_URGENT = 0x04

CTRL_SESSION_START = 0x01
CTRL_SESSION_END   = 0x02
CTRL_STREAM_START  = 0x03
CTRL_STREAM_END    = 0x04
CTRL_ACK           = 0x05
CTRL_CANCEL        = 0x06
CTRL_SERVER_READY  = 0x07

PKT_TYPE_NAMES = {
    PKT_AUDIO_UP:   "AUDIO_UP",
    PKT_AUDIO_DOWN: "AUDIO_DOWN",
    PKT_CONTROL:    "CONTROL",
    PKT_HEARTBEAT:  "HEARTBEAT",
}

CTRL_NAMES = {
    CTRL_SESSION_START: "SESSION_START",
    CTRL_SESSION_END:   "SESSION_END",
    CTRL_STREAM_START:  "STREAM_START",
    CTRL_STREAM_END:    "STREAM_END",
    CTRL_ACK:           "ACK",
    CTRL_CANCEL:        "CANCEL",
    CTRL_SERVER_READY:  "SERVER_READY",
}

# ── Packet builders ────────────────────────────────────────────────

def build_packet(seq: int, pkt_type: int, flags: int, payload: bytes = b"") -> bytes:
    return struct.pack("<HBB", seq, pkt_type, flags) + payload

def build_control(seq: int, cmd: int, flags: int = 0) -> bytes:
    return build_packet(seq, PKT_CONTROL, flags, bytes([cmd]))

def build_heartbeat(seq: int) -> bytes:
    return build_packet(seq, PKT_HEARTBEAT, 0)

def parse_packet(data: bytes):
    """Return (seq, pkt_type, flags, payload) or None."""
    if len(data) < ESP_HEADER:
        return None
    seq, pkt_type, flags = struct.unpack("<HBB", data[:ESP_HEADER])
    payload = data[ESP_HEADER:]
    return seq, pkt_type, flags, payload

def describe(seq, pkt_type, flags, payload):
    name = PKT_TYPE_NAMES.get(pkt_type, f"0x{pkt_type:02x}")
    extra = ""
    if pkt_type == PKT_CONTROL and payload:
        cmd = payload[0]
        extra = f" cmd={CTRL_NAMES.get(cmd, f'0x{cmd:02x}')}"
    flag_parts = []
    if flags & FLAG_START: flag_parts.append("START")
    if flags & FLAG_END:   flag_parts.append("END")
    if flags & FLAG_URGENT: flag_parts.append("URGENT")
    flag_str = "|".join(flag_parts) if flag_parts else "0"
    return f"seq={seq} type={name} flags={flag_str}{extra} payload={len(payload)}B"

# ── Audio generator ────────────────────────────────────────────────

def gen_sine_chunk(freq: float = 440.0, n_samples: int = 700,
                   sample_rate: int = 16000, amplitude: float = 0.5) -> bytes:
    """Generate one chunk of 16-bit LE PCM sine wave (700 samples = 1400 B = 43.75 ms)."""
    buf = bytearray(n_samples * 2)
    for i in range(n_samples):
        t = i / sample_rate
        val = int(32767 * amplitude * math.sin(2 * math.pi * freq * t))
        struct.pack_into("<h", buf, i * 2, val)
    return bytes(buf)


def gen_sine_continuous(freq: float, n_samples: int, sample_offset: int,
                        sample_rate: int = 16000, amplitude: float = 0.5) -> bytes:
    """Generate a chunk with correct phase continuity across packets."""
    buf = bytearray(n_samples * 2)
    for i in range(n_samples):
        t = (sample_offset + i) / sample_rate
        val = int(32767 * amplitude * math.sin(2 * math.pi * freq * t))
        struct.pack_into("<h", buf, i * 2, val)
    return bytes(buf)


def write_wav(path: str, pcm_data: bytes,
              sample_rate: int = 16000, channels: int = 1, bits: int = 16):
    """Write raw PCM bytes to a WAV file."""
    with wave.open(path, "wb") as wf:
        wf.setnchannels(channels)
        wf.setsampwidth(bits // 8)
        wf.setframerate(sample_rate)
        wf.writeframes(pcm_data)


def compare_wav_files(path_a: str, path_b: str, label_a: str = "A",
                      label_b: str = "B"):
    """Compare two WAV files and print detailed stats."""
    print(f"\n  ┌─ Comparing: {label_a} vs {label_b}")

    with wave.open(path_a, "rb") as wa:
        rate_a, ch_a, sw_a = wa.getframerate(), wa.getnchannels(), wa.getsampwidth()
        frames_a = wa.getnframes()
        data_a = wa.readframes(frames_a)

    with wave.open(path_b, "rb") as wb:
        rate_b, ch_b, sw_b = wb.getframerate(), wb.getnchannels(), wb.getsampwidth()
        frames_b = wb.getnframes()
        data_b = wb.readframes(frames_b)

    print(f"  │  {label_a}: {rate_a}Hz {ch_a}ch {sw_a*8}bit  "
          f"{frames_a} frames  {len(data_a)} bytes  "
          f"{frames_a/rate_a:.3f}s")
    print(f"  │  {label_b}: {rate_b}Hz {ch_b}ch {sw_b*8}bit  "
          f"{frames_b} frames  {len(data_b)} bytes  "
          f"{frames_b/rate_b:.3f}s")

    # Format match
    fmt_match = (rate_a == rate_b and ch_a == ch_b and sw_a == sw_b)
    print(f"  │  Format match: {'✅' if fmt_match else '❌'}")

    # Byte-level comparison
    min_len = min(len(data_a), len(data_b))
    if min_len == 0:
        print("  │  ⚠️  One file is empty — cannot compare")
        print("  └─")
        return False

    byte_match = (data_a[:min_len] == data_b[:min_len])
    if byte_match and len(data_a) == len(data_b):
        print("  │  Byte-exact match: ✅ (identical)")
        print("  └─")
        return True

    # Sample-level diff stats
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
    pct_match = (1.0 - diff_count / n_samples) * 100 if n_samples else 0

    print(f"  │  Samples compared: {n_samples}")
    print(f"  │  Identical samples: {n_samples - diff_count}/{n_samples} "
          f"({pct_match:.1f}%)")
    print(f"  │  Max sample diff:  {max_diff}")
    print(f"  │  Avg sample diff:  {avg_diff:.2f}")

    if len(data_a) != len(data_b):
        extra = abs(len(data_a) - len(data_b))
        longer = label_a if len(data_a) > len(data_b) else label_b
        print(f"  │  Size mismatch: {longer} has {extra} extra bytes")

    ok = (pct_match >= 99.9 and max_diff <= 1)
    print(f"  │  Verdict: {'✅ PASS' if ok else '⚠️  DIFFERS'}")
    print("  └─")
    return ok


# ── Main test ──────────────────────────────────────────────────────

def main():
    ap = argparse.ArgumentParser(description="ESP audio protocol test")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=9001)
    ap.add_argument("--duration", type=float, default=3.0,
                    help="seconds of audio to stream")
    ap.add_argument("--audio-dir", default="/tmp/esp_audio",
                    help="directory where server saves WAV files")
    args = ap.parse_args()

    server = (args.host, args.port)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(3.0)
    sock.bind(("0.0.0.0", 0))  # explicit bind for reliable recvfrom

    passed = 0
    failed = 0

    def expect(label, cond):
        nonlocal passed, failed
        if cond:
            passed += 1
            print(f"  ✅ {label}")
        else:
            failed += 1
            print(f"  ❌ {label}")

    # ── 1. Heartbeat ───────────────────────────────────────────────
    print("\n🔹 Step 1: HEARTBEAT")
    sock.sendto(build_heartbeat(42), server)
    try:
        data, _ = sock.recvfrom(1500)
        r = parse_packet(data)
        expect("got heartbeat reply", r is not None)
        if r:
            seq, pkt_type, flags, payload = r
            print(f"     ← {describe(seq, pkt_type, flags, payload)}")
            expect("type == HEARTBEAT", pkt_type == PKT_HEARTBEAT)
            expect("seq == 42 (mirrored)", seq == 42)
    except socket.timeout:
        expect("heartbeat reply received", False)

    # ── 2. SESSION_START → SERVER_READY ────────────────────────────
    print("\n🔹 Step 2: SESSION_START → SERVER_READY")
    # Record existing WAV files so we can detect a new one later
    existing_wavs = set(glob.glob(os.path.join(args.audio_dir, "esp_*.wav")))

    sock.sendto(build_control(0, CTRL_SESSION_START, FLAG_START), server)
    try:
        data, _ = sock.recvfrom(1500)
        r = parse_packet(data)
        expect("got control reply", r is not None)
        if r:
            seq, pkt_type, flags, payload = r
            print(f"     ← {describe(seq, pkt_type, flags, payload)}")
            expect("type == CONTROL", pkt_type == PKT_CONTROL)
            expect("cmd == SERVER_READY", payload and payload[0] == CTRL_SERVER_READY)
    except socket.timeout:
        expect("SERVER_READY received", False)

    # ── 3. Stream AUDIO_UP ────────────────────────────────────────
    samples_per_chunk = 700           # 1400 B per chunk, 43.75 ms
    chunk_bytes = samples_per_chunk * 2
    ms_per_chunk = 43.75
    n_packets = int(args.duration * 1000 / ms_per_chunk)
    sent_pcm = bytearray()            # accumulate everything we send

    print(f"\n🔹 Step 3: Stream {n_packets} AUDIO_UP packets "
          f"({args.duration:.1f}s, {n_packets * chunk_bytes} bytes)")

    t0 = time.monotonic()
    for seq in range(1, n_packets + 1):
        sample_offset = (seq - 1) * samples_per_chunk
        chunk = gen_sine_continuous(440.0, samples_per_chunk, sample_offset)
        sent_pcm.extend(chunk)

        flags = 0
        pkt = build_packet(seq, PKT_AUDIO_UP, flags, chunk)
        sock.sendto(pkt, server)

        # Pace at roughly real-time to avoid flooding
        elapsed = time.monotonic() - t0
        target = seq * ms_per_chunk / 1000.0
        if target > elapsed:
            time.sleep(target - elapsed)

    dt = time.monotonic() - t0
    rate = n_packets * chunk_bytes / dt / 1024
    print(f"     sent {n_packets} packets in {dt:.2f}s ({rate:.1f} KB/s)")
    expect(f"all {n_packets} packets sent", True)

    # Save sent audio as WAV
    os.makedirs(args.audio_dir, exist_ok=True)
    sent_wav_path = os.path.join(args.audio_dir, "test_sent.wav")
    write_wav(sent_wav_path, bytes(sent_pcm))
    print(f"     💾 sent audio saved: {sent_wav_path} "
          f"({len(sent_pcm)} bytes, {len(sent_pcm)/32000:.2f}s)")

    # ── 4. SESSION_END → ACK ─────────────────────────────────────
    print("\n🔹 Step 4: SESSION_END → ACK")
    sock.sendto(build_control(n_packets + 1, CTRL_SESSION_END, FLAG_END), server)
    try:
        data, _ = sock.recvfrom(1500)
        r = parse_packet(data)
        expect("got control reply", r is not None)
        if r:
            seq, pkt_type, flags, payload = r
            print(f"     ← {describe(seq, pkt_type, flags, payload)}")
            expect("type == CONTROL", pkt_type == PKT_CONTROL)
            expect("cmd == ACK", payload and payload[0] == CTRL_ACK)
    except socket.timeout:
        expect("ACK received", False)

    # ── 5. Verify WAV file saved ─────────────────────────────────
    print("\n🔹 Step 5: Verify WAV saved")
    time.sleep(0.5)  # let the server flush
    current_wavs = set(glob.glob(os.path.join(args.audio_dir, "esp_*.wav")))
    new_wavs = current_wavs - existing_wavs
    expect("new WAV file created", len(new_wavs) > 0)
    if new_wavs:
        wav_path = sorted(new_wavs)[-1]
        wav_size = os.path.getsize(wav_path)
        expected_audio_bytes = n_packets * chunk_bytes
        expected_wav_size = 44 + expected_audio_bytes  # 44-byte WAV header
        print(f"     file: {wav_path}")
        print(f"     size: {wav_size} bytes (expected ~{expected_wav_size})")
        expect("WAV size matches audio data",
               abs(wav_size - expected_wav_size) < 100)

    # ── 6. CANCEL test (bonus) ───────────────────────────────────
    print("\n🔹 Step 6: SESSION_START → stream briefly → CANCEL")
    cancel_chunk = gen_sine_chunk(440.0)  # single repeated chunk is fine for cancel
    sock.sendto(build_control(100, CTRL_SESSION_START, FLAG_START), server)
    try:
        data, _ = sock.recvfrom(1500)
        r = parse_packet(data)
        if r and r[1] == PKT_CONTROL and r[3] and r[3][0] == CTRL_SERVER_READY:
            # Send a few audio packets then cancel
            for s in range(101, 106):
                sock.sendto(build_packet(s, PKT_AUDIO_UP, 0, cancel_chunk), server)
                time.sleep(0.01)
            sock.sendto(build_control(106, CTRL_CANCEL, 0), server)
            data2, _ = sock.recvfrom(1500)
            r2 = parse_packet(data2)
            expect("CANCEL → ACK", r2 and r2[1] == PKT_CONTROL and r2[3] and r2[3][0] == CTRL_ACK)
        else:
            expect("SERVER_READY for cancel test", False)
    except socket.timeout:
        expect("cancel flow completed", False)

    # ── 7. Compare sent WAV vs server-saved WAV ──────────────────
    print("\n🔹 Step 7: Compare SENT audio vs SERVER-saved audio")
    if new_wavs:
        server_wav = sorted(new_wavs)[-1]
        match = compare_wav_files(
            sent_wav_path, server_wav,
            label_a="SENT (Python)",
            label_b="SERVER (Rust)",
        )
        expect("sent vs server audio match", match)
    else:
        expect("server WAV exists for comparison", False)

    # ── Summary ──────────────────────────────────────────────────
    sock.close()
    total = passed + failed
    print(f"\n{'='*50}")
    print(f"  Results: {passed}/{total} passed", end="")
    if failed:
        print(f" ({failed} FAILED) ❌")
    else:
        print(" ✅")
    print(f"{'='*50}\n")
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
