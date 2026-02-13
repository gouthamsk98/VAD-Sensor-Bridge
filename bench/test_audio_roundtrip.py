#!/usr/bin/env python3
"""
test_audio_roundtrip.py — Full audio round-trip test via the Rust server.

Flow:
  ┌──────────┐   AUDIO_UP (UDP)    ┌──────────────┐  WebSocket   ┌──────────┐
  │  This     │ ──────────────────▶ │ Rust Server   │ ──────────▶ │  OpenAI  │
  │  Script   │                     │ (port 9001)   │             │ Realtime │
  │           │ ◀────────────────── │               │ ◀────────── │  API     │
  └──────────┘   AUDIO_DOWN (UDP)   └──────────────┘  WebSocket   └──────────┘
       │                                                                │
       ▼                                                                │
  mic_sent.wav                                                          │
  openai_response.wav  ◀── PCM response audio from OpenAI ─────────────┘

Steps:
  1. Capture audio from your microphone (or send a WAV file)
  2. Send as ESP AUDIO_UP packets over UDP to the Rust server
  3. Server forwards audio to OpenAI Realtime API for inference
  4. OpenAI sends response audio back through the server
  5. This script receives AUDIO_DOWN packets and saves them as WAV

Usage:
  # Prerequisites — start the Rust server with OpenAI Realtime enabled:
  cd rust-udp-mqtt
  OPENAI_API_KEY=sk-... cargo run -- --openai-realtime

  # Terminal 2 — run the test (live mic, 10 seconds):
  python3 bench/test_audio_roundtrip.py --duration 10

  # Or send a WAV file instead of using the mic:
  python3 bench/test_audio_roundtrip.py --wav-input bench/test_speech_like.wav

  # List audio devices:
  python3 bench/test_audio_roundtrip.py --list-devices

  # Pick a specific mic device:
  python3 bench/test_audio_roundtrip.py --input-device 2 --duration 10

Outputs (saved to --output-dir, default ./esp_audio):
  mic_sent.wav          — raw PCM captured from the microphone
  openai_response.wav   — response audio received from OpenAI via the server

Requires: pip install sounddevice numpy
"""

import argparse
import os
import queue
import signal
import socket
import struct
import sys
import threading
import time
import wave

# ═══════════════════════════════════════════════════════════════════════
#  ESP Protocol constants (mirrors esp_audio_protocol.rs)
# ═══════════════════════════════════════════════════════════════════════

ESP_HEADER = 4
MAX_PAYLOAD = 1400  # bytes — fits inside a single UDP packet under MTU

PKT_AUDIO_UP   = 0x01
PKT_AUDIO_DOWN = 0x02
PKT_CONTROL    = 0x03
PKT_HEARTBEAT  = 0x04

CTRL_SESSION_START = 0x01
CTRL_SESSION_END   = 0x02
CTRL_STREAM_START  = 0x03
CTRL_STREAM_END    = 0x04
CTRL_ACK           = 0x05
CTRL_CANCEL        = 0x06
CTRL_SERVER_READY  = 0x07

SAMPLE_RATE = 16_000   # Hz — 16 kHz mono
BYTES_PER_SAMPLE = 2   # 16-bit PCM
CHUNK_SAMPLES = 700    # 43.75 ms per packet (matches ESP protocol)
CHUNK_BYTES = CHUNK_SAMPLES * BYTES_PER_SAMPLE

CTRL_NAMES = {
    CTRL_SESSION_START: "SESSION_START",
    CTRL_SESSION_END:   "SESSION_END",
    CTRL_STREAM_START:  "STREAM_START",
    CTRL_STREAM_END:    "STREAM_END",
    CTRL_ACK:           "ACK",
    CTRL_CANCEL:        "CANCEL",
    CTRL_SERVER_READY:  "SERVER_READY",
}


# ═══════════════════════════════════════════════════════════════════════
#  Packet builders / parsers
# ═══════════════════════════════════════════════════════════════════════

def build_packet(seq: int, pkt_type: int, flags: int, payload: bytes = b"") -> bytes:
    return struct.pack("<HBB", seq & 0xFFFF, pkt_type, flags) + payload

def build_control(seq: int, cmd: int, flags: int = 0) -> bytes:
    return build_packet(seq, PKT_CONTROL, flags, bytes([cmd]))

def parse_packet(data: bytes):
    """Return (seq, pkt_type, flags, payload) or None."""
    if len(data) < ESP_HEADER:
        return None
    seq, pkt_type, flags = struct.unpack("<HBB", data[:ESP_HEADER])
    return seq, pkt_type, flags, data[ESP_HEADER:]


# ═══════════════════════════════════════════════════════════════════════
#  WAV writer helper
# ═══════════════════════════════════════════════════════════════════════

def write_wav(path: str, pcm: bytes, rate: int = SAMPLE_RATE):
    """Write raw 16-bit mono PCM bytes to a WAV file."""
    with wave.open(path, "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(BYTES_PER_SAMPLE)
        wf.setframerate(rate)
        wf.writeframes(pcm)


# ═══════════════════════════════════════════════════════════════════════
#  Audio Round-Trip Client
# ═══════════════════════════════════════════════════════════════════════

class AudioRoundTripTest:
    """
    Captures mic audio, streams it to the Rust server over UDP,
    receives OpenAI response audio back, and saves both as WAV.
    """

    def __init__(self, host: str, port: int, output_dir: str,
                 input_device=None, wav_input: str = None,
                 response_timeout: float = 15.0):
        self.host = host
        self.port = port
        self.output_dir = output_dir
        self.input_device = input_device
        self.wav_input = wav_input
        self.response_timeout = response_timeout

        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(5.0)
        self.sock.bind(("0.0.0.0", 0))  # ephemeral port
        self.server_addr = (host, port)
        self.local_port = self.sock.getsockname()[1]

        self.seq_out = 0
        self.running = False

        # Buffers
        self.sent_pcm = bytearray()
        self.recv_pcm = bytearray()

        # Stats
        self.pkts_sent = 0
        self.pkts_recv = 0
        self.bytes_sent = 0
        self.bytes_recv = 0

        # Events
        self.stream_end_event = threading.Event()
        self.stop_event = threading.Event()

    def next_seq(self) -> int:
        s = self.seq_out
        self.seq_out = (self.seq_out + 1) & 0xFFFF
        return s

    # ── Session lifecycle ──────────────────────────────────────────

    def session_start(self) -> bool:
        """Send SESSION_START, wait for SERVER_READY."""
        pkt = build_control(self.next_seq(), CTRL_SESSION_START)
        self.sock.sendto(pkt, self.server_addr)
        print("📞 SESSION_START sent → waiting for SERVER_READY …")

        try:
            data, _ = self.sock.recvfrom(2048)
            parsed = parse_packet(data)
            if parsed:
                _, pkt_type, _, payload = parsed
                if (pkt_type == PKT_CONTROL and len(payload) > 0
                        and payload[0] == CTRL_SERVER_READY):
                    print("✅ SERVER_READY received — session active")
                    return True
        except socket.timeout:
            print("❌ Timeout waiting for SERVER_READY")
            print("   Is the Rust server running with --openai-realtime?")
            return False

        print("❌ Unexpected response from server")
        return False

    def session_end(self):
        """Send SESSION_END, wait for ACK."""
        pkt = build_control(self.next_seq(), CTRL_SESSION_END)
        self.sock.sendto(pkt, self.server_addr)
        print("📴 SESSION_END sent → waiting for ACK …")

        try:
            data, _ = self.sock.recvfrom(2048)
            parsed = parse_packet(data)
            if parsed:
                _, pkt_type, _, payload = parsed
                if pkt_type == PKT_CONTROL and len(payload) > 0 and payload[0] == CTRL_ACK:
                    print("✅ ACK received")
                    return
        except socket.timeout:
            pass
        print("⚠️  No ACK received (continuing anyway)")

    # ── Receiver thread ────────────────────────────────────────────

    def receiver_loop(self):
        """Receive AUDIO_DOWN packets and heartbeats from the server."""
        self.sock.settimeout(0.5)

        while self.running:
            try:
                data, _ = self.sock.recvfrom(2048)
            except socket.timeout:
                continue
            except Exception as e:
                if self.running:
                    print(f"⚠️  recv error: {e}", file=sys.stderr)
                break

            parsed = parse_packet(data)
            if not parsed:
                continue

            seq, pkt_type, flags, payload = parsed

            if pkt_type == PKT_AUDIO_DOWN and payload:
                self.recv_pcm.extend(payload)
                self.pkts_recv += 1
                self.bytes_recv += len(payload)

            elif pkt_type == PKT_CONTROL and len(payload) > 0:
                cmd = payload[0]
                cmd_name = CTRL_NAMES.get(cmd, f"0x{cmd:02x}")
                if cmd == CTRL_STREAM_END:
                    print(f"   🔊 STREAM_END — OpenAI response audio complete")
                    self.stream_end_event.set()
                elif cmd == CTRL_STREAM_START:
                    print(f"   🔊 STREAM_START — receiving OpenAI response …")
                else:
                    print(f"   📩 Control: {cmd_name}")

            elif pkt_type == PKT_HEARTBEAT:
                reply = build_packet(seq, PKT_HEARTBEAT, 0)
                self.sock.sendto(reply, self.server_addr)

    # ── Mic capture + send ─────────────────────────────────────────

    def send_mic_audio(self, duration: float):
        """Capture audio from mic and stream as AUDIO_UP packets."""
        import sounddevice as sd
        import numpy as np

        dev_name = "(system default)"
        if self.input_device is not None:
            dev_name = sd.query_devices(self.input_device)["name"]

        print(f"🎙️  Recording from: {dev_name}")
        print(f"    Duration: {duration:.1f}s, {SAMPLE_RATE} Hz, 16-bit mono")
        print(f"    Chunk: {CHUNK_SAMPLES} samples ({CHUNK_SAMPLES / SAMPLE_RATE * 1000:.1f} ms)")
        print()

        total_chunks = int(duration * SAMPLE_RATE / CHUNK_SAMPLES)
        t0 = time.monotonic()

        with sd.InputStream(samplerate=SAMPLE_RATE, blocksize=CHUNK_SAMPLES,
                            channels=1, dtype="int16", device=self.input_device) as stream:
            for i in range(total_chunks):
                if self.stop_event.is_set():
                    break

                frames, overflowed = stream.read(CHUNK_SAMPLES)
                if overflowed:
                    print("⚠️  mic buffer overflow", file=sys.stderr)

                pcm = frames.tobytes()
                self.sent_pcm.extend(pcm)

                # Split into ESP-sized chunks (usually fits in one)
                for offset in range(0, len(pcm), MAX_PAYLOAD):
                    chunk = pcm[offset:offset + MAX_PAYLOAD]
                    pkt = build_packet(self.next_seq(), PKT_AUDIO_UP, 0, chunk)
                    self.sock.sendto(pkt, self.server_addr)
                    self.pkts_sent += 1
                    self.bytes_sent += len(chunk)

                # Progress
                elapsed = time.monotonic() - t0
                sys.stdout.write(
                    f"\r   📦 {i+1}/{total_chunks}  "
                    f"⏱ {elapsed:.1f}s  "
                    f"📤 {self.bytes_sent / 1024:.0f} KB sent  "
                    f"📥 {self.bytes_recv / 1024:.0f} KB recv"
                )
                sys.stdout.flush()

        dt = time.monotonic() - t0
        print(f"\n   ✅ Mic capture done — {self.pkts_sent} packets, "
              f"{self.bytes_sent / 1024:.1f} KB in {dt:.1f}s")

    # ── WAV file input ─────────────────────────────────────────────

    def send_wav_file(self):
        """Read a WAV file and send as AUDIO_UP packets at real-time pace."""
        with wave.open(self.wav_input, "rb") as wf:
            assert wf.getnchannels() == 1, f"WAV must be mono, got {wf.getnchannels()} ch"
            assert wf.getsampwidth() == 2, f"WAV must be 16-bit, got {wf.getsampwidth() * 8}-bit"
            rate = wf.getframerate()
            n_frames = wf.getnframes()
            audio_secs = n_frames / rate
            if rate != SAMPLE_RATE:
                print(f"⚠️  WAV sample rate is {rate} Hz, expected {SAMPLE_RATE} Hz")

        print(f"📁 Sending WAV: {self.wav_input}")
        print(f"   Duration: {audio_secs:.1f}s, {rate} Hz, 16-bit mono")
        print()

        chunk_samples = CHUNK_SAMPLES
        chunk_duration = chunk_samples / rate
        t0 = time.monotonic()

        with wave.open(self.wav_input, "rb") as wf:
            chunk_idx = 0
            while not self.stop_event.is_set():
                pcm = wf.readframes(chunk_samples)
                if not pcm:
                    break

                self.sent_pcm.extend(pcm)

                for offset in range(0, len(pcm), MAX_PAYLOAD):
                    chunk = pcm[offset:offset + MAX_PAYLOAD]
                    pkt = build_packet(self.next_seq(), PKT_AUDIO_UP, 0, chunk)
                    self.sock.sendto(pkt, self.server_addr)
                    self.pkts_sent += 1
                    self.bytes_sent += len(chunk)

                chunk_idx += 1

                # Pace to approximately real-time
                target_time = t0 + chunk_idx * chunk_duration
                now = time.monotonic()
                if now < target_time:
                    time.sleep(target_time - now)

                # Progress
                elapsed = time.monotonic() - t0
                sys.stdout.write(
                    f"\r   📦 chunk {chunk_idx}  "
                    f"⏱ {elapsed:.1f}s  "
                    f"📤 {self.bytes_sent / 1024:.0f} KB sent  "
                    f"📥 {self.bytes_recv / 1024:.0f} KB recv"
                )
                sys.stdout.flush()

        dt = time.monotonic() - t0
        print(f"\n   ✅ WAV send done — {self.pkts_sent} packets, "
              f"{self.bytes_sent / 1024:.1f} KB in {dt:.1f}s")

    # ── Main orchestrator ──────────────────────────────────────────

    def run(self, duration: float):
        """Execute the full round-trip test."""
        os.makedirs(self.output_dir, exist_ok=True)

        input_label = (f"WAV file: {self.wav_input}" if self.wav_input
                       else f"mic device {self.input_device or 'default'}")
        print()
        print("=" * 64)
        print("  Audio Round-Trip Test")
        print("  Mic → UDP → Rust Server → OpenAI → UDP → Save WAV")
        print("=" * 64)
        print(f"  Server      : {self.host}:{self.port}")
        print(f"  Local port  : {self.local_port}")
        print(f"  Input       : {input_label}")
        print(f"  Audio format: {SAMPLE_RATE} Hz, 16-bit, mono")
        print(f"  Output dir  : {self.output_dir}")
        print(f"  Resp timeout: {self.response_timeout:.0f}s")
        print()

        # ── Step 1: Start session ──────────────────────────────────
        print("─── Step 1: Start session ───────────────────────────")
        if not self.session_start():
            self.sock.close()
            return

        # ── Step 2: Start receiver thread ──────────────────────────
        self.running = True
        recv_thread = threading.Thread(target=self.receiver_loop, daemon=True, name="recv")
        recv_thread.start()

        # ── Step 3: Send audio (mic or WAV) ────────────────────────
        print()
        print("─── Step 2: Send audio ──────────────────────────────")
        try:
            if self.wav_input:
                self.send_wav_file()
            else:
                self.send_mic_audio(duration)
        except KeyboardInterrupt:
            print("\n⏹️  Interrupted by user")
            self.stop_event.set()

        # ── Step 4: End session ────────────────────────────────────
        print()
        print("─── Step 3: End session ─────────────────────────────")
        self.session_end()

        # ── Step 5: Wait for OpenAI response audio ─────────────────
        print()
        print("─── Step 4: Wait for OpenAI response ────────────────")
        print(f"   Waiting up to {self.response_timeout:.0f}s for response audio …")

        recv_before = self.bytes_recv
        wait_start = time.monotonic()
        last_recv_time = time.monotonic()

        while time.monotonic() - wait_start < self.response_timeout:
            if self.stream_end_event.is_set():
                # Got STREAM_END — wait a tiny bit more for trailing packets
                time.sleep(0.5)
                break

            # Track if we're still receiving data
            if self.bytes_recv > recv_before:
                recv_before = self.bytes_recv
                last_recv_time = time.monotonic()

            # If we started receiving but stopped for 3 seconds, we're done
            if self.bytes_recv > 0 and (time.monotonic() - last_recv_time) > 3.0:
                print("   ⏱  No new audio for 3s — assuming response is complete")
                break

            elapsed = time.monotonic() - wait_start
            sys.stdout.write(
                f"\r   ⏳ {elapsed:.0f}s elapsed  "
                f"📥 {self.bytes_recv / 1024:.1f} KB received  "
                f"({self.pkts_recv} packets)"
            )
            sys.stdout.flush()
            time.sleep(0.2)

        print()

        # ── Step 6: Stop receiver ──────────────────────────────────
        self.running = False
        recv_thread.join(timeout=2.0)

        # ── Step 7: Save WAV files ─────────────────────────────────
        print()
        print("─── Step 5: Save audio files ────────────────────────")

        sent_path = os.path.join(self.output_dir, "mic_sent.wav")
        resp_path = os.path.join(self.output_dir, "openai_response.wav")

        if self.sent_pcm:
            write_wav(sent_path, bytes(self.sent_pcm))
            sent_dur = len(self.sent_pcm) / (SAMPLE_RATE * BYTES_PER_SAMPLE)
            print(f"   💾 Sent audio saved:     {sent_path}")
            print(f"      {len(self.sent_pcm):,} bytes, {sent_dur:.2f}s")
        else:
            print("   ⚠️  No audio was sent")
            sent_path = None

        if self.recv_pcm:
            write_wav(resp_path, bytes(self.recv_pcm))
            resp_dur = len(self.recv_pcm) / (SAMPLE_RATE * BYTES_PER_SAMPLE)
            print(f"   💾 Response audio saved: {resp_path}")
            print(f"      {len(self.recv_pcm):,} bytes, {resp_dur:.2f}s")
        else:
            print("   ⚠️  No response audio received from OpenAI")
            resp_path = None

        # ── Summary ────────────────────────────────────────────────
        print()
        print("─── Summary ─────────────────────────────────────────")
        sent_dur = len(self.sent_pcm) / (SAMPLE_RATE * BYTES_PER_SAMPLE) if self.sent_pcm else 0
        recv_dur = len(self.recv_pcm) / (SAMPLE_RATE * BYTES_PER_SAMPLE) if self.recv_pcm else 0
        print(f"   📤 Audio sent:     {self.bytes_sent / 1024:.1f} KB "
              f"({self.pkts_sent} pkts, {sent_dur:.1f}s)")
        print(f"   📥 Audio received: {self.bytes_recv / 1024:.1f} KB "
              f"({self.pkts_recv} pkts, {recv_dur:.1f}s)")

        if sent_path and resp_path:
            print()
            print("─── Playback ────────────────────────────────────────")
            print(f"   ▶️  Play what you said:")
            print(f"      afplay {sent_path}")
            print(f"   ▶️  Play OpenAI response:")
            print(f"      afplay {resp_path}")

        print()
        if self.recv_pcm:
            print("✅ Round-trip test complete!")
        else:
            print("⚠️  Round-trip test complete but no response audio was received.")
            print("   Check that the Rust server is running with --openai-realtime")
            print("   and that OPENAI_API_KEY is set correctly.")
        print()

        self.sock.close()


# ═══════════════════════════════════════════════════════════════════════
#  Main
# ═══════════════════════════════════════════════════════════════════════

def main():
    parser = argparse.ArgumentParser(
        description="Audio round-trip test: Mic → UDP → Server → OpenAI → UDP → WAV",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s --duration 10                          # 10s of mic capture
  %(prog)s --wav-input bench/test_speech_like.wav  # send a WAV file
  %(prog)s --list-devices                          # show audio devices
  %(prog)s --input-device 2 --duration 5           # use specific mic
""",
    )
    parser.add_argument("--host", default="127.0.0.1",
                        help="Rust server host (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=9001,
                        help="Rust server audio UDP port (default: 9001)")
    parser.add_argument("--duration", type=float, default=10.0,
                        help="Mic recording duration in seconds (default: 10)")
    parser.add_argument("--output-dir", default="./esp_audio",
                        help="Directory for output WAV files (default: ./esp_audio)")
    parser.add_argument("--input-device", type=int, default=None,
                        help="Input audio device index (see --list-devices)")
    parser.add_argument("--wav-input", type=str, default=None,
                        help="Send a WAV file instead of using the mic "
                             "(must be 16 kHz, 16-bit, mono)")
    parser.add_argument("--response-timeout", type=float, default=15.0,
                        help="Seconds to wait for OpenAI response after sending "
                             "(default: 15)")
    parser.add_argument("--list-devices", action="store_true",
                        help="List available audio devices and exit")

    args = parser.parse_args()

    # ── Dependency check ───────────────────────────────────────────
    if args.wav_input is None:
        try:
            import sounddevice as sd
            import numpy as np
        except ImportError:
            print("❌ sounddevice + numpy required for mic capture:")
            print("   pip install sounddevice numpy")
            sys.exit(1)

        if args.list_devices:
            print(sd.query_devices())
            return
    elif args.list_devices:
        try:
            import sounddevice as sd
            print(sd.query_devices())
        except ImportError:
            print("sounddevice not installed — can't list devices")
        return

    # ── Validate WAV input ─────────────────────────────────────────
    if args.wav_input and not os.path.isfile(args.wav_input):
        print(f"❌ WAV file not found: {args.wav_input}")
        sys.exit(1)

    # ── Signal handling ────────────────────────────────────────────
    test = AudioRoundTripTest(
        host=args.host,
        port=args.port,
        output_dir=args.output_dir,
        input_device=args.input_device,
        wav_input=args.wav_input,
        response_timeout=args.response_timeout,
    )

    def on_sigint(sig, frame):
        print("\n⏹️  Ctrl+C — stopping …")
        test.stop_event.set()
    signal.signal(signal.SIGINT, on_sigint)

    test.run(duration=args.duration)


if __name__ == "__main__":
    main()
