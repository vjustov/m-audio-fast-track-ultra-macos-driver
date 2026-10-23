"""Check a metronome recording (32-bit float WAV, 8 channels) for regular clicks and dropouts on input 1."""
import struct, sys
import numpy as np

raw = open(sys.argv[1], "rb").read()
rate = struct.unpack("<I", raw[24:28])[0]
i = raw.find(b"data")
n = struct.unpack("<I", raw[i + 4:i + 8])[0]
x = np.frombuffer(raw[i + 8:i + 8 + n], dtype=np.float32).reshape(-1, 8)
v = x[:, 0]
env = np.abs(v)
above = np.where(env > env.max() * 0.3)[0]
clicks = [above[0]] if len(above) else []
for a in above[1:]:
    if a - clicks[-1] > rate * 0.1:
        clicks.append(a)
t = np.array(clicks) / rate
d = np.diff(t)
z = (v == 0).astype(int)
runs = np.diff(np.concatenate([[0], z, [0]]))
gaps = [(round(a / rate, 3), int(b - a)) for a, b in zip(np.where(runs == 1)[0], np.where(runs == -1)[0]) if b - a >= 8]
print(f"rate {rate}, {len(v) / rate:.1f} s, peak {20 * np.log10(env.max() + 1e-12):.1f} dBFS, clicks {len(t)}")
if len(d):
    med = np.median(d)
    bad = [(round(t[k], 3), round((d[k] - med) * 1000, 2)) for k in range(len(d)) if abs(d[k] - med) > 0.001]
    print(f"interval median {med:.4f} s, spread {(d.max() - d.min()) * 1000:.2f} ms, intervals off by >1 ms: {len(bad)} {bad[:10]}")
print(f"zero runs >= 8 samples: {len(gaps)} {gaps[:10]}")
