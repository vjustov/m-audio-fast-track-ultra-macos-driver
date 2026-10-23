"""Compare a loopback WAV channel against the ideal ftu_stream sine. Reports glitches."""
import sys, numpy as np

path, ch, freq, dbfs = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4])
raw = open(path, "rb").read()
rate = int.from_bytes(raw[24:28], "little")
data = np.frombuffer(raw[44:], dtype=np.uint8)
frames = len(data) // 24
b = data[: frames * 24].reshape(frames, 8, 3).astype(np.int32)
x = ((b[:, :, 0] | (b[:, :, 1] << 8) | (b[:, :, 2] << 16)) << 8) >> 8
y = x[:, ch - 1].astype(np.float64)

start = np.argmax(np.abs(y) > 1000)  # first non-silent sample
amp = 10 ** (dbfs / 20) * 8388607
n = np.arange(len(y) - start)
seg = y[start:]
# Least-squares fit of a sine at the known frequency over 1 s blocks.
block = rate
worst, glitches = 0.0, []
for i in range(0, len(seg) - block, block):
    t = (n[i:i + block]) / rate
    A = np.column_stack([np.sin(2 * np.pi * freq * t), np.cos(2 * np.pi * freq * t)])
    coef, *_ = np.linalg.lstsq(A, seg[i:i + block], rcond=None)
    res = seg[i:i + block] - A @ coef
    peak = np.max(np.abs(res))
    worst = max(worst, peak)
    if peak > amp * 0.01:
        glitches.append((round((start + i) / rate, 2), int(np.argmax(np.abs(res))), round(20 * np.log10(peak / amp), 1)))
    if i == block * 3:
        fitted = np.hypot(*coef)
print(f"rate {rate}, {frames} frames, signal starts at {start / rate:.3f}s")
print(f"fitted amplitude {20 * np.log10(fitted / 8388607):.2f} dBFS (expected {dbfs})")
print(f"worst residual {20 * np.log10(max(worst, 1) / amp):.1f} dB relative to tone")
print(f"blocks with residual > -40 dB: {len(glitches)}", glitches[:10])
