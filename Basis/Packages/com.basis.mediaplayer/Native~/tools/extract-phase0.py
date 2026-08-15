"""Extract delivery-gap schedules from phase-0 diagnostics captures.

The phase-0 captures are the C player's per-frame diagnostics CSVs recorded
during the 2026-08 live-buffering investigation (impaired VRCDN runs, clumsy
shaping, pinned edge). This ports the gap reconstruction from that
investigation's sizing tool: a starve of duration D happened after the
player's jitter buffer (eng_buf_ms) had drained, so the underlying delivery
gap is D + buf. The output is one small fixture per capture — gap start
offsets and durations on the analysed timeline — which media-testkit replays
as synthetic arrival schedules for the Bank's sizing-table tests.

Usage:
    uv run python tools/extract-phase0.py <captures-dir>

Writes media-testkit/fixtures/phase0/*.csv. Only needs re-running if the
source captures ever change; the derived fixtures are committed.
"""

import csv
import os
import sys

# capture file -> (fixture name, impairment description)
CAPTURES = {
    "ts-clean.csv": ("ts-clean", "HTTP-TS, no impairment (baseline)"),
    "ts-impaired-lagonly.csv": ("ts-rtt600-loss0", "HTTP-TS, +600 ms RTT, 0% loss"),
    "ts-impaired-loss005.csv": ("ts-rtt300-loss005", "HTTP-TS, +300 ms RTT, 0.05% loss"),
    "rtspt-impaired-loss005.csv": ("rtspt-rtt300-loss005", "RTSP-TCP, +300 ms RTT, 0.05% loss"),
    "ts-impaired.csv": ("ts-rtt300-loss05", "HTTP-TS, +300 ms RTT, 0.5% loss (throughput regime)"),
}


def extract(path):
    rows = [
        r
        for r in csv.DictReader(open(path, newline="", encoding="utf-8", errors="replace"))
        if r.get("engine_state") == "Playing"
    ]
    if not rows:
        return [], 0.0, 460.0
    t0 = float(rows[0]["unity_time_s"])
    rows = [r for r in rows if float(r["unity_time_s"]) - t0 >= 5.0]
    base = float(rows[0]["unity_time_s"])
    gaps, run_start, prev_t, buf = [], None, None, 460.0
    for r in rows:
        t = float(r["unity_time_s"])
        lag = float(r["eng_lag_ms"])
        b = float(r["eng_buf_ms"])
        if b > 0:
            buf = b
        if lag < buf * 0.25:
            if run_start is None:
                run_start = t
        else:
            if run_start is not None and prev_t is not None:
                gaps.append((run_start - base, (prev_t - run_start) * 1000.0 + buf))
            run_start = None
        prev_t = t
    duration = float(rows[-1]["unity_time_s"]) - base
    return [(s, g) for s, g in gaps if g > buf], duration, buf


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    captures_dir = sys.argv[1]
    out_dir = os.path.join(os.path.dirname(__file__), "..", "media-testkit", "fixtures", "phase0")
    os.makedirs(out_dir, exist_ok=True)
    for src, (name, desc) in CAPTURES.items():
        gaps, duration, buf = extract(os.path.join(captures_dir, src))
        out = os.path.join(out_dir, name + ".csv")
        with open(out, "w", newline="\n", encoding="utf-8") as f:
            f.write(f"# source: {src} (phase-0 diagnostics capture, 2026-08-10)\n")
            f.write(f"# impairment: {desc}\n")
            f.write(f"# analysed_duration_s: {duration:.3f}\n")
            f.write(f"# baseline_buf_ms: {buf:.0f}\n")
            f.write("start_s,gap_ms\n")
            for start, gap in gaps:
                f.write(f"{start:.3f},{gap:.1f}\n")
        print(f"{out}: {len(gaps)} gaps over {duration:.0f}s")


if __name__ == "__main__":
    main()
