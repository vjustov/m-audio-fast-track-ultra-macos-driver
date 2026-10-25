#!/usr/bin/env python3
"""Hardware test run: drives the tools in hwtest/build and the daemon, prints a verdict.

Needs the device, the installed driver (sudo packaging/install.sh), the tools
(make hwtest) and a loopback cable from output OUT to input IN (default 1 -> 1) with
that input muted in the monitor mix so the loop does not feed back. No root needed.

    python3 -I hwtest/run.py                    # all checks, 5-minute soak
    python3 -I hwtest/run.py --quick            # 1-minute soak, latency at 2 starts per rate
    python3 -I hwtest/run.py --compare-delay    # also the rate matrix without the playback delay
    python3 -I hwtest/run.py --channels 6       # cables on outputs 1-6 to inputs 1-6: check the channel map

Writes the full log to captures/hwtest-<date>-<time>.txt. Exit status 1 if a check fails.
"""

import argparse
import datetime
import json
import os
import re
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = os.path.join(ROOT, "hwtest", "build")
DAEMON = "/Library/Application Support/FastTrackUltra/ftu-daemon"
DAEMON_LOG = os.path.expanduser("~/Library/Logs/ftu-daemon.log")
RATES = [44100, 48000, 88200, 96000]

# Thresholds. Round trips are in frames.
ROUND_TRIP_SPREAD = 1.0  # one rate, across starts and previous rates
UNREPORTED_MAX = 2.0  # measured round trip minus the latency the plugin reports
COREAUDIOD_CPU_MAX = 30.0


class Run:
    def __init__(self, path):
        self.out = open(path, "w")
        self.results = []

    def log(self, line=""):
        print(line)
        self.out.write(line + "\n")
        self.out.flush()

    def check(self, name, ok, detail):
        self.results.append((name, ok, detail))
        self.log(f"{'PASS' if ok else 'FAIL'}  {name}: {detail}")


def tool(name, *args, timeout=None):
    p = subprocess.run([os.path.join(BUILD, name), *map(str, args)], capture_output=True, text=True, timeout=timeout)
    return p.returncode, p.stdout + p.stderr


def ctl(*args):
    p = subprocess.run([DAEMON, "ctl", *map(str, args)], capture_output=True, text=True, timeout=10)
    return p.returncode, p.stdout.strip()


def coreaudiod_cpu():
    out = subprocess.run(["ps", "-Ao", "%cpu,comm"], capture_output=True, text=True).stdout
    vals = [float(l.split()[0]) for l in out.splitlines() if l.strip().endswith("/coreaudiod")]
    return max(vals) if vals else None


def log_mark():
    try:
        return os.path.getsize(DAEMON_LOG)
    except OSError:
        return 0


def log_since(mark):
    try:
        with open(DAEMON_LOG, "rb") as f:
            f.seek(mark)
            return f.read().decode(errors="replace").splitlines()
    except OSError:
        return []


GAIN_RE = re.compile(r"gain ([-+][\d.]+)")
# A burst that came back: below this the tool correlates against silence or noise.
# With the input gain knobs at minimum (line level into the preamps) a clean loop
# returns about 0.04; silence returns a 0-frame round trip.
MIN_GAIN = 0.01
LATENCY_RE = re.compile(r"round trip ([\d.]+) frames .*spread ([\d.]+); reported latency sum (\d+); unreported (-?[\d.]+)")


def latency(out_ch, in_ch, bursts, rate):
    code, text = tool("latency", out_ch, in_ch, bursts, rate, timeout=60)
    m = LATENCY_RE.search(text)
    if code != 0 or not m:
        return None, text
    trip, spread, reported, unreported = (float(x) for x in m.groups())
    gains = [float(g) for g in GAIN_RE.findall(text)]
    if trip <= 0 or not gains or min(gains) < MIN_GAIN:
        return None, f"no burst came back (gains {gains}): check the loop and the device"
    return {"trip": trip, "spread": spread, "reported": reported, "unreported": unreported}, text


def preflight(r, a):
    code, out = ctl("get", "status")
    st = json.loads(out) if code == 0 and out.startswith("{") else {}
    r.check("daemon", st.get("connected") and st.get("streaming"), f"status {out or 'no reply'}")
    cpu = coreaudiod_cpu()
    r.check("coreaudiod idle (before)", cpu is not None and cpu < COREAUDIOD_CPU_MAX, f"{cpu}% CPU")
    if not st.get("connected"):
        return False
    res, text = latency(a.out, a.inp, 2, 48000)
    r.check("loopback", res is not None and res["spread"] <= 1.0, (f"{res['trip']:.2f} frames at 48 kHz" if res else text.strip()[-200:]))
    return all(ok for _, ok, _ in r.results)


def inputs_isolated(channels):
    """Looped inputs must not reach any output through the monitor mix, or the loop
    feeds back. Returns the offending (bus, input) pairs."""
    code, out = ctl("get", "state")
    if code != 0 or not out.startswith("{"):
        return None
    m = json.loads(out)
    return [(b + 1, s + 1) for b, bus in enumerate(m["buses"]) for s in range(channels) if bus[s]["level"] is not None and not bus[s]["mute"]]


def channel_map(r, a):
    """Each output looped to the same input: the burst must come back on that input,
    with the same round trip on every channel."""
    trips = []
    for ch in range(1, a.channels + 1):
        res, text = latency(ch, ch, 2, 48000)
        if res is None:
            r.check(f"channel {ch}: output {ch} -> input {ch}", False, text.strip()[-150:])
            continue
        trips.append(res["trip"])
        r.log(f"  channel {ch}: {res['trip']:.2f} frames")
    ok = len(trips) == a.channels and max(trips) - min(trips) <= ROUND_TRIP_SPREAD
    r.check(f"channel map 1-{a.channels}", ok, f"round trips {min(trips):.2f} to {max(trips):.2f} frames" if trips else "no data")


def rate_matrix(r, a, label):
    """Each rate measured right after each other rate: the round trip may not depend on
    the previous rate (it did before the playback start delay), and the plugin must
    report it."""
    starts = 1 if a.quick else 2
    trips = {t: [] for t in RATES}
    unreported = []
    for _ in range(starts):
        for target in RATES:
            for prev in RATES:
                if prev == target:
                    continue
                latency(a.out, a.inp, 1, prev)
                res, text = latency(a.out, a.inp, 3, target)
                if res is None:
                    r.log(f"  {prev} -> {target}: no measurement ({text.strip()[-120:]})")
                    continue
                r.log(f"  {prev:>6} -> {target:>6}: {res['trip']:7.2f} frames, unreported {res['unreported']:6.2f}")
                trips[target].append(res["trip"])
                unreported.append(res["unreported"])
    for t in RATES:
        v = trips[t]
        ok = len(v) > 0 and max(v) - min(v) <= ROUND_TRIP_SPREAD
        r.check(f"{label}: round trip at {t} Hz repeats", ok, f"{min(v):.2f} to {max(v):.2f} frames" if v else "no data")
    worst = max((abs(u) for u in unreported), default=None)
    r.check(f"{label}: reported latency matches", worst is not None and worst <= UNREPORTED_MAX, f"worst unreported {worst} frames")


def soak(r, a, minutes, stalls_ms=()):
    """Soak at 48 kHz; optionally inject event-thread stalls through the daemon's test
    hook, one every 20 s from 20 s in."""
    latency(a.out, a.inp, 1, 48000)
    mark = log_mark()
    p = subprocess.Popen([os.path.join(BUILD, "soak"), str(minutes), str(a.out), str(a.inp)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    for ms in stalls_ms:
        time.sleep(20)
        ctl("stall", ms)
        r.log(f"  stall {ms} ms injected")
    out, _ = p.communicate(timeout=minutes * 60 + 120)
    for line in out.splitlines():
        if "glitch" in line or line.startswith("done") or "silent" in line:
            r.log("  " + line.strip())
    m = re.search(r"done: (\d+) glitches, (\d+) overloads", out)
    glitches, overloads = (int(m.group(1)), int(m.group(2))) if m else (None, None)
    lines = log_since(mark)
    jumps = [l for l in lines if "lead jump" in l]
    odd = [l for l in lines if "capture packet of" in l]
    usb = [l for l in lines if re.search(r"usb errors [1-9]", l)]
    return glitches, overloads, jumps, odd, usb


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=int, default=1, help="output looped back (1-8)")
    ap.add_argument("--in", dest="inp", type=int, default=1, help="input the loop returns on (1-8)")
    ap.add_argument("--soak-minutes", type=float, default=5)
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--compare-delay", action="store_true", help="also run the rate matrix with playback delay 0")
    ap.add_argument("--channels", type=int, default=0, help="outputs 1..N looped to inputs 1..N: check the channel map")
    a = ap.parse_args()
    if a.quick:
        a.soak_minutes = min(a.soak_minutes, 1)
    for t in ("latency", "soak"):
        if not os.access(os.path.join(BUILD, t), os.X_OK):
            sys.exit(f"hwtest/build/{t} missing: run make hwtest")

    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M")
    path = os.path.join(ROOT, "captures", f"hwtest-{stamp}.txt")
    r = Run(path)
    r.log(f"hwtest {stamp}, loop output {a.out} -> input {a.inp}")

    looped = max(a.channels, a.inp)
    bad = inputs_isolated(looped)
    if bad is None or bad:
        r.check("looped inputs muted in the mix", False, f"audible: {bad} (bus, input); mute them first" if bad else "no state from the daemon")
    elif preflight(r, a):
        if a.channels:
            r.log(f"channel map, outputs 1-{a.channels} to inputs 1-{a.channels}:")
            channel_map(r, a)
        if a.compare_delay:
            r.log("rate matrix, playback delay 0 ms (vendor-style delay off):")
            ctl("playback-delay", 0)
            rate_matrix(r, a, "delay 0")
        r.log("rate matrix, playback delay 10 ms:")
        ctl("playback-delay", 10)
        rate_matrix(r, a, "delay 10")

        r.log("stall recovery (soak 1.5 min, three 20 ms stalls):")
        g, o, jumps, odd, usb = soak(r, a, 1.5, stalls_ms=(20, 20, 20))
        r.check("stalls: one lead jump each", len(jumps) == 3, f"{len(jumps)} lead jumps")
        # A 20 ms stall skips playback microframes, so one glitch per stall is expected.
        # The extra phase jump about 0.7 s later (2026-10-24) should be gone.
        r.check("stalls: at most one glitch each", g is not None and g <= 3, f"{g} glitches")

        r.log(f"soak ({a.soak_minutes:g} min):")
        g, o, jumps, odd, usb = soak(r, a, a.soak_minutes)
        r.check("soak: no glitches", g == 0, f"{g} glitches, {o} overloads")
        r.check("soak: no lead jumps or USB errors", not jumps and not usb, f"{len(jumps)} lead jumps, {len(usb)} USB error lines")
        if odd:
            r.log(f"  note: {len(odd)} out-of-pattern capture packets, first: {odd[0].strip()}")

    cpu = coreaudiod_cpu()
    r.check("coreaudiod idle (after)", cpu is not None and cpu < COREAUDIOD_CPU_MAX, f"{cpu}% CPU")
    failed = [n for n, ok, _ in r.results if not ok]
    r.log("")
    r.log(f"{len(r.results) - len(failed)} passed, {len(failed)} failed" + (": " + "; ".join(failed) if failed else ""))
    r.log(f"log: {path}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
