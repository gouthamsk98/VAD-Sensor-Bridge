#!/usr/bin/env python3
"""Generate test WAV files for testing without a microphone."""
import wave, struct, math

rate = 16000

# 5s speech-like WAV: varying frequency with silence gaps
samples = []
for t in range(rate * 5):
    f = 200 + 300 * math.sin(2 * math.pi * 2 * t / rate)
    s = int(8000 * math.sin(2 * math.pi * f * t / rate))
    sec = t / rate
    if 1.0 < sec < 1.5 or 3.0 < sec < 3.5:
        s = 0
    samples.append(s)

with wave.open("bench/test_speech_like.wav", "w") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(rate)
    w.writeframes(struct.pack(f"<{len(samples)}h", *samples))

print(f"Created bench/test_speech_like.wav ({len(samples)/rate:.1f}s, {rate} Hz)")
