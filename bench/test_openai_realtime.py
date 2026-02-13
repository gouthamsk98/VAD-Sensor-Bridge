#!/usr/bin/env python3
"""
Test OpenAI Realtime API via ESP audio protocol.

Streams live microphone audio through the ESP32 UDP protocol to the
Rust server, which bridges it to OpenAI's Realtime API.  Response audio
from OpenAI is sent back as AUDIO_DOWN packets and played through the
speakers.

Usage:
    # List audio devices:
    python test_openai_realtime.py --list-devices

    # Run with default mic + speaker:
    python test_openai_realtime.py

    # Run with specific input/output devices:
    python test_openai_realtime.py --input-device 2 --output-device 4

Prerequisites:
    pip install sounddevice numpy

    The Rust server must be running with OpenAI Realtime enabled:
    OPENAI_API_KEY=sk-... cargo run -- --openai-realtime
"""

import argparse
import socket
import struct
import sys
import threading
import time
import queue

import numpy as np

try:
    import sounddevice as sd
except ImportError:
    print("ERROR: sounddevice not installed. Run: pip install sounddevice")
    sys.exit(1)


# ── ESP Protocol Constants ──────────────────────────────────────────────
HEADER_SIZE = 4
MAX_PAYLOAD = 1400
PKT_AUDIO_UP = 0x01
PKT_AUDIO_DOWN = 0x02
PKT_CONTROL = 0x03
PKT_HEARTBEAT = 0x04
CTRL_SESSION_START = 0x01
CTRL_SESSION_END = 0x02
CTRL_STREAM_START = 0x03
CTRL_STREAM_END = 0x04
CTRL_ACK = 0x05
CTRL_CANCEL = 0x06
CTRL_SERVER_READY = 0x07

SAMPLE_RATE = 16000
CHANNELS = 1
DTYPE = np.int16


def build_packet(seq: int, pkt_type: int, flags: int, payload: bytes) -> bytes:
    header = struct.pack("<HBB", seq & 0xFFFF, pkt_type, flags)
    return header + payload


def build_control(seq: int, cmd: int, flags: int = 0) -> bytes:
    return build_packet(seq, PKT_CONTROL, flags, bytes([cmd]))


def parse_packet(data: bytes):
    if len(data) < HEADER_SIZE:
        return None
    seq, pkt_type, flags = struct.unpack("<HBB", data[:HEADER_SIZE])
    payload = data[HEADER_SIZE:]
    return seq, pkt_type, flags, payload


class OpenAiRealtimeTest:
    """Full-duplex ESP audio ↔ OpenAI Realtime test client."""

    def __init__(self, host: str, port: int, input_device=None, output_device=None,
                 wav_input: str = None):
        self.host = host
        self.port = port
        self.input_device = input_device
        self.output_device = output_device
        self.wav_input = wav_input

        # Validate / resolve audio devices early so we fail fast with a
        # helpful message instead of a cryptic PortAudio -1 error.
        self._resolve_devices()

        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(5.0)
        self.server_addr = (host, port)

        self.seq_out = 0
        self.running = False
        self.session_active = False

        # Queue for received audio to play back
        self.playback_queue = queue.Queue(maxsize=500)

        # Stats
        self.packets_sent = 0
        self.packets_recv = 0
        self.audio_bytes_sent = 0
        self.audio_bytes_recv = 0

    def _resolve_devices(self):
        """Validate audio devices, falling back to first available if defaults fail."""
        devices = sd.query_devices()

        # Resolve input device (skip if using WAV file input)
        if self.wav_input is None and self.input_device is None:
            try:
                sd.check_input_settings(device=None, samplerate=SAMPLE_RATE,
                                        channels=CHANNELS, dtype="float32")
            except Exception:
                # Default device doesn't work — pick the first input device
                for i, d in enumerate(devices):
                    if d["max_input_channels"] >= 1:
                        print(f"⚠️  No default input device; using [{i}] {d['name']}")
                        self.input_device = i
                        break
                else:
                    print("ERROR: No input audio devices found!")
                    print("       Use --wav-input <file.wav> to send a WAV file instead.")
                    print("\nAvailable devices:")
                    print(sd.query_devices())
                    sys.exit(1)

        # Resolve output device
        if self.output_device is None:
            try:
                sd.check_output_settings(device=None, samplerate=SAMPLE_RATE,
                                         channels=CHANNELS, dtype="float32")
            except Exception:
                # Default device doesn't work — pick the first output device
                for i, d in enumerate(devices):
                    if d["max_output_channels"] >= 1:
                        print(f"⚠️  No default output device; using [{i}] {d['name']}")
                        self.output_device = i
                        break
                else:
                    print("ERROR: No output audio devices found!")
                    print("Available devices:")
                    print(sd.query_devices())
                    sys.exit(1)

    def next_seq(self) -> int:
        s = self.seq_out
        self.seq_out = (self.seq_out + 1) & 0xFFFF
        return s

    def send_session_start(self):
        pkt = build_control(self.next_seq(), CTRL_SESSION_START)
        self.sock.sendto(pkt, self.server_addr)
        print("📞 SESSION_START sent, waiting for SERVER_READY...")

        # Wait for SERVER_READY
        try:
            data, _ = self.sock.recvfrom(2048)
            parsed = parse_packet(data)
            if parsed:
                seq, pkt_type, flags, payload = parsed
                if pkt_type == PKT_CONTROL and len(payload) > 0 and payload[0] == CTRL_SERVER_READY:
                    print("✅ SERVER_READY received — session active")
                    self.session_active = True
                    return True
        except socket.timeout:
            print("❌ Timeout waiting for SERVER_READY")
            return False

        print("❌ Unexpected response")
        return False

    def send_session_end(self):
        pkt = build_control(self.next_seq(), CTRL_SESSION_END)
        self.sock.sendto(pkt, self.server_addr)
        print("📴 SESSION_END sent")
        self.session_active = False

    def mic_callback(self, indata, frames, time_info, status):
        """Called by sounddevice for each mic chunk."""
        if status:
            print(f"⚠️ Mic status: {status}", file=sys.stderr)

        if not self.running or not self.session_active:
            return

        # Convert float32 → int16
        pcm = (indata[:, 0] * 32767).astype(np.int16).tobytes()

        # Split into ESP-sized chunks
        for offset in range(0, len(pcm), MAX_PAYLOAD):
            chunk = pcm[offset:offset + MAX_PAYLOAD]
            pkt = build_packet(self.next_seq(), PKT_AUDIO_UP, 0, chunk)
            try:
                self.sock.sendto(pkt, self.server_addr)
                self.packets_sent += 1
                self.audio_bytes_sent += len(chunk)
            except Exception as e:
                print(f"⚠️ Send error: {e}", file=sys.stderr)

    def receiver_thread(self):
        """Receives AUDIO_DOWN packets and queues them for playback."""
        self.sock.settimeout(0.5)

        while self.running:
            try:
                data, _ = self.sock.recvfrom(2048)
            except socket.timeout:
                continue
            except Exception as e:
                if self.running:
                    print(f"⚠️ Recv error: {e}", file=sys.stderr)
                break

            parsed = parse_packet(data)
            if not parsed:
                continue

            seq, pkt_type, flags, payload = parsed

            if pkt_type == PKT_AUDIO_DOWN and payload:
                self.packets_recv += 1
                self.audio_bytes_recv += len(payload)

                # Convert int16 bytes → float32 for playback
                samples = np.frombuffer(payload, dtype=np.int16).astype(np.float32) / 32768.0
                try:
                    self.playback_queue.put_nowait(samples)
                except queue.Full:
                    pass  # drop if playback can't keep up

            elif pkt_type == PKT_CONTROL and len(payload) > 0:
                cmd = payload[0]
                if cmd == CTRL_STREAM_END:
                    print("🔊 AI response audio complete")
                elif cmd == CTRL_ACK:
                    print("✅ ACK received")

            elif pkt_type == PKT_HEARTBEAT:
                # Mirror heartbeat back
                reply = build_packet(seq, PKT_HEARTBEAT, 0, b"")
                self.sock.sendto(reply, self.server_addr)

    def playback_callback(self, outdata, frames, time_info, status):
        """Called by sounddevice to fill the output buffer."""
        if status:
            print(f"⚠️ Output status: {status}", file=sys.stderr)

        filled = 0
        while filled < frames:
            try:
                chunk = self.playback_queue.get_nowait()
                n = min(len(chunk), frames - filled)
                outdata[filled:filled + n, 0] = chunk[:n]
                filled += n
                # If chunk had more data than we needed, put remainder back
                if n < len(chunk):
                    try:
                        self.playback_queue.put_nowait(chunk[n:])
                    except queue.Full:
                        pass
            except queue.Empty:
                break

        # Fill remainder with silence
        if filled < frames:
            outdata[filled:, 0] = 0.0

    def _send_wav_file(self):
        """Read a WAV file and send it as ESP audio packets."""
        import wave
        with wave.open(self.wav_input, "rb") as wf:
            assert wf.getnchannels() == 1, f"WAV must be mono, got {wf.getnchannels()} channels"
            assert wf.getsampwidth() == 2, f"WAV must be 16-bit, got {wf.getsampwidth() * 8}-bit"
            rate = wf.getframerate()
            if rate != SAMPLE_RATE:
                print(f"⚠️  WAV sample rate is {rate} Hz, expected {SAMPLE_RATE} Hz")
            n_frames = wf.getnframes()
            audio_secs = n_frames / rate
            print(f"📁 Sending WAV: {self.wav_input} ({audio_secs:.1f}s, {rate} Hz)")

            chunk_samples = int(rate * 0.05)  # 50ms chunks
            while self.running:
                pcm = wf.readframes(chunk_samples)
                if not pcm:
                    break
                for offset in range(0, len(pcm), MAX_PAYLOAD):
                    chunk = pcm[offset:offset + MAX_PAYLOAD]
                    pkt = build_packet(self.next_seq(), PKT_AUDIO_UP, 0, chunk)
                    try:
                        self.sock.sendto(pkt, self.server_addr)
                        self.packets_sent += 1
                        self.audio_bytes_sent += len(chunk)
                    except Exception as e:
                        print(f"⚠️ Send error: {e}", file=sys.stderr)
                time.sleep(0.05)  # pace to ~real-time
        print("📁 WAV file fully sent")

    def run(self, duration: float = 30.0):
        """Run the full-duplex test for `duration` seconds."""
        input_label = f"WAV file: {self.wav_input}" if self.wav_input else f"device {self.input_device or 'default'}"
        print(f"\n🎤 OpenAI Realtime test — {duration}s")
        print(f"   Server: {self.host}:{self.port}")
        print(f"   Audio:  {SAMPLE_RATE} Hz, 16-bit, mono")
        print(f"   Input:  {input_label}")
        print(f"   Output: device {self.output_device or 'default'}")
        print()

        # Start session
        if not self.send_session_start():
            return

        self.running = True

        # Start receiver thread
        recv_thread = threading.Thread(target=self.receiver_thread, daemon=True)
        recv_thread.start()

        # Start audio streams
        blocksize = int(SAMPLE_RATE * 0.05)  # 50ms chunks

        try:
            if self.wav_input:
                # WAV mode: send file in a thread, optionally play output
                wav_thread = threading.Thread(target=self._send_wav_file, daemon=True)
                wav_thread.start()

                # Try to open output stream for playback, but don't fail if unavailable
                out_stream = None
                try:
                    out_stream = sd.OutputStream(
                        samplerate=SAMPLE_RATE,
                        channels=CHANNELS,
                        dtype="float32",
                        blocksize=blocksize,
                        device=self.output_device,
                        callback=self.playback_callback,
                    )
                    out_stream.start()
                    print("🔊 Output audio stream opened")
                except Exception as e:
                    print(f"⚠️  No output audio device — response audio won't be played ({e})")

                print(f"📁 Sending WAV file... (waiting up to {duration}s for response)")
                start = time.time()
                while time.time() - start < duration and self.running:
                    time.sleep(0.1)
                    elapsed = time.time() - start
                    if int(elapsed) % 5 == 0 and int(elapsed) > 0:
                        print(
                            f"  [{elapsed:.0f}s] sent {self.packets_sent} pkts "
                            f"({self.audio_bytes_sent / 1024:.1f} KB) | "
                            f"recv {self.packets_recv} pkts "
                            f"({self.audio_bytes_recv / 1024:.1f} KB)"
                        )

                if out_stream:
                    out_stream.stop()
                    out_stream.close()
            else:
                # Live mic mode
                with sd.InputStream(
                    samplerate=SAMPLE_RATE,
                    channels=CHANNELS,
                    dtype="float32",
                    blocksize=blocksize,
                    device=self.input_device,
                    callback=self.mic_callback,
                ), sd.OutputStream(
                    samplerate=SAMPLE_RATE,
                    channels=CHANNELS,
                    dtype="float32",
                    blocksize=blocksize,
                    device=self.output_device,
                    callback=self.playback_callback,
                ):
                    print("🎙️  Speak now! (Ctrl+C to stop early)")
                    print(f"    Recording for {duration}s...")
                    print()

                    start = time.time()
                    while time.time() - start < duration and self.running:
                        time.sleep(0.1)

                        # Print progress every 5 seconds
                        elapsed = time.time() - start
                        if int(elapsed) % 5 == 0 and int(elapsed) > 0:
                            print(
                                f"  [{elapsed:.0f}s] sent {self.packets_sent} pkts "
                                f"({self.audio_bytes_sent / 1024:.1f} KB) | "
                                f"recv {self.packets_recv} pkts "
                                f"({self.audio_bytes_recv / 1024:.1f} KB)"
                            )

        except KeyboardInterrupt:
            print("\n⏹️  Interrupted by user")
        finally:
            self.running = False
            self.send_session_end()
            time.sleep(0.5)  # Let final packets arrive
            recv_thread.join(timeout=2.0)

        # Summary
        print(f"\n📊 Summary:")
        print(f"   Audio sent:     {self.audio_bytes_sent / 1024:.1f} KB "
              f"({self.packets_sent} packets)")
        print(f"   Audio received: {self.audio_bytes_recv / 1024:.1f} KB "
              f"({self.packets_recv} packets)")
        print(f"   Audio sent:     {self.audio_bytes_sent / (SAMPLE_RATE * 2):.1f}s")
        print(f"   Audio received: {self.audio_bytes_recv / (SAMPLE_RATE * 2):.1f}s")


def main():
    parser = argparse.ArgumentParser(
        description="Test OpenAI Realtime API via ESP audio protocol"
    )
    parser.add_argument("--host", default="127.0.0.1", help="Server host")
    parser.add_argument("--port", type=int, default=9001, help="Server audio port")
    parser.add_argument("--duration", type=float, default=30.0, help="Recording duration (seconds)")
    parser.add_argument("--list-devices", action="store_true", help="List audio devices and exit")
    parser.add_argument("--input-device", type=int, default=None, help="Input device index")
    parser.add_argument("--output-device", type=int, default=None, help="Output device index")
    parser.add_argument("--wav-input", type=str, default=None,
                        help="Send a WAV file instead of live mic (16kHz 16-bit mono)")

    args = parser.parse_args()

    if args.list_devices:
        print(sd.query_devices())
        return

    test = OpenAiRealtimeTest(
        host=args.host,
        port=args.port,
        input_device=args.input_device,
        output_device=args.output_device,
        wav_input=args.wav_input,
    )
    test.run(duration=args.duration)


if __name__ == "__main__":
    main()
