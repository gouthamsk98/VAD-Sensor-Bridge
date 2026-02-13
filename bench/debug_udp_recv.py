#!/usr/bin/env python3
"""
debug_udp_recv.py — Minimal end-to-end debug: mic → server → OpenAI → receive.

Logs every single packet sent and received with timestamps.
Run the Rust server with: RUST_LOG=info cargo run -- --openai-realtime

Usage:
  python3 bench/debug_udp_recv.py          # 5s mic capture (default)
  python3 bench/debug_udp_recv.py 10       # 10s mic capture
"""
import socket, struct, sys, threading, time, wave, os

ESP_HEADER = 4
PKT_AUDIO_UP = 0x01
PKT_AUDIO_DOWN = 0x02
PKT_CONTROL = 0x03
PKT_HEARTBEAT = 0x04
CTRL_SESSION_START = 0x01
CTRL_SESSION_END = 0x02
CTRL_STREAM_START = 0x03
CTRL_STREAM_END = 0x04
CTRL_ACK = 0x05
CTRL_SERVER_READY = 0x07

SAMPLE_RATE = 16000
CHUNK_SAMPLES = 700

SERVER = ("127.0.0.1", 9001)
CTRL_NAMES = {
    0x01: "SESSION_START", 0x02: "SESSION_END", 0x03: "STREAM_START",
    0x04: "STREAM_END", 0x05: "ACK", 0x06: "CANCEL", 0x07: "SERVER_READY",
}

def ts():
    return time.strftime("%H:%M:%S", time.localtime()) + f".{int(time.time()*1000)%1000:03d}"

def build_pkt(seq, ptype, flags=0, payload=b""):
    return struct.pack("<HBB", seq & 0xFFFF, ptype, flags) + payload

# ── Globals ────────────────────────────────────────────────────
recv_audio = bytearray()
recv_pkts = 0
recv_bytes = 0
recv_controls = []
running = True
stream_end = threading.Event()


def receiver(sock):
    """Background: receive ALL packets from server."""
    global recv_pkts, recv_bytes, running
    sock.settimeout(0.5)
    while running:
        try:
            data, addr = sock.recvfrom(65535)
        except socket.timeout:
            continue
        except Exception as e:
            print(f"[{ts()}] RECV ERROR: {e}", file=sys.stderr)
            break

        if len(data) < 4:
            print(f"[{ts()}] ??? tiny packet: {len(data)} bytes")
            continue

        pt = data[2]
        payload = data[4:]

        if pt == PKT_AUDIO_DOWN:
            recv_audio.extend(payload)
            recv_pkts += 1
            recv_bytes += len(payload)
            if recv_pkts <= 5 or recv_pkts % 50 == 0:
                print(f"[{ts()}] AUDIO_DOWN #{recv_pkts}: {len(payload)}B "
                      f"(total: {recv_bytes/1024:.1f}KB)")
        elif pt == PKT_CONTROL:
            cmd = payload[0] if payload else 0
            name = CTRL_NAMES.get(cmd, f"0x{cmd:02x}")
            print(f"[{ts()}] CONTROL: {name}")
            recv_controls.append(name)
            if cmd == CTRL_STREAM_END:
                stream_end.set()
        elif pt == PKT_HEARTBEAT:
            seq_r = struct.unpack("<H", data[:2])[0]
            sock.sendto(build_pkt(seq_r, PKT_HEARTBEAT), addr)
        else:
            print(f"[{ts()}] UNKNOWN type=0x{pt:02x} {len(data)}B")


def main():
    global running

    try:
        import sounddevice as sd
    except ImportError:
        print("pip install sounddevice numpy")
        sys.exit(1)

    duration = float(sys.argv[1]) if len(sys.argv) > 1 else 5.0

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(3.0)
    sock.bind(("0.0.0.0", 0))
    port = sock.getsockname()[1]

    print(f"[{ts()}] Bound on port {port}, server={SERVER}")
    print(f"[{ts()}] Will record {duration:.0f}s from default mic")
    print()

    # ── SESSION_START ──────────────────────────────────────────────
    print(f"[{ts()}] --> SESSION_START")
    sock.sendto(build_pkt(0, PKT_CONTROL, 0, bytes([CTRL_SESSION_START])), SERVER)
    try:
        data, _ = sock.recvfrom(4096)
        cmd = data[4] if len(data) > 4 else 0
        print(f"[{ts()}] <-- {CTRL_NAMES.get(cmd, hex(cmd))}")
        if cmd != CTRL_SERVER_READY:
            print("Expected SERVER_READY!")
            return
    except socket.timeout:
        print(f"[{ts()}] TIMEOUT — is the server running?")
        return

    # ── Start receiver thread ──────────────────────────────────────
    recv_thread = threading.Thread(target=receiver, args=(sock,), daemon=True)
    recv_thread.start()

    # ── Record + send mic audio ────────────────────────────────────
    print(f"\n[{ts()}] Recording {duration:.0f}s — SPEAK NOW!")
    sent_pcm = bytearray()
    seq = 1
    total_chunks = int(duration * SAMPLE_RATE / CHUNK_SAMPLES)
    t0 = time.monotonic()

    with sd.InputStream(samplerate=SAMPLE_RATE, blocksize=CHUNK_SAMPLES,
                        channels=1, dtype="int16") as stream:
        for i in range(total_chunks):
            frames, _ = stream.read(CHUNK_SAMPLES)
            pcm = frames.tobytes()
            sent_pcm.extend(pcm)
            pkt = build_pkt(seq, PKT_AUDIO_UP, 0, pcm)
            sock.sendto(pkt, SERVER)
            seq += 1

            if (i + 1) % 23 == 0:  # ~1s
                elapsed = time.monotonic() - t0
                print(f"[{ts()}] UP: {seq-1} pkts ({len(sent_pcm)/1024:.0f}KB)  "
                      f"DOWN: {recv_bytes/1024:.1f}KB ({recv_pkts} pkts)")

    dt = time.monotonic() - t0
    print(f"\n[{ts()}] Mic done — {seq-1} pkts, {len(sent_pcm)/1024:.0f}KB in {dt:.1f}s")

    # ── SESSION_END ────────────────────────────────────────────────
    print(f"[{ts()}] --> SESSION_END")
    sock.sendto(build_pkt(seq, PKT_CONTROL, 0, bytes([CTRL_SESSION_END])), SERVER)
    # Don't block waiting for ACK — the receiver thread will log it

    # ── Wait for response ──────────────────────────────────────────
    print(f"\n[{ts()}] Waiting up to 20s for OpenAI response audio...")
    wait_start = time.monotonic()
    last_bytes = 0
    last_recv_time = time.monotonic()

    while time.monotonic() - wait_start < 20:
        if stream_end.is_set():
            print(f"[{ts()}] STREAM_END received — waiting 1s for trailing...")
            time.sleep(1.0)
            break
        if recv_bytes > last_bytes:
            last_bytes = recv_bytes
            last_recv_time = time.monotonic()
        if recv_bytes > 0 and time.monotonic() - last_recv_time > 3.0:
            print(f"[{ts()}] No new audio for 3s — done")
            break
        elapsed = time.monotonic() - wait_start
        if int(elapsed) % 3 == 0 and elapsed - int(elapsed) < 0.3:
            print(f"[{ts()}] ... {elapsed:.0f}s elapsed, recv={recv_bytes/1024:.1f}KB ({recv_pkts} pkts)")
        time.sleep(0.2)

    running = False
    recv_thread.join(timeout=2)

    # ── Results ────────────────────────────────────────────────────
    print()
    print("=" * 60)
    print(f"  SENT:     {len(sent_pcm)/1024:.0f} KB ({seq-1} pkts, {len(sent_pcm)/32000:.1f}s)")
    print(f"  RECEIVED: {recv_bytes/1024:.1f} KB ({recv_pkts} pkts, {recv_bytes/32000:.1f}s)")
    print(f"  CONTROLS: {recv_controls}")
    print("=" * 60)

    if recv_audio:
        os.makedirs("esp_audio", exist_ok=True)
        path = "esp_audio/debug_response.wav"
        with wave.open(path, "wb") as wf:
            wf.setnchannels(1)
            wf.setsampwidth(2)
            wf.setframerate(SAMPLE_RATE)
            wf.writeframes(bytes(recv_audio))
        print(f"\n  Response saved: {path}")
        print(f"  Play: afplay {path}")
    else:
        print(f"\n  No audio received! Check server logs for:")
        print(f"    'OpenAI VAD: speech started'  -> Did OpenAI hear speech?")
        print(f"    'audio buffer committed'      -> Was audio committed?")
        print(f"    'response.audio.delta'        -> Did OpenAI respond?")
        print(f"    'sending AUDIO_DOWN to ESP'   -> Was it sent to you?")
        print(f"    'no active ESP client'        -> Was active_esp cleared?")


if __name__ == "__main__":
    main()
