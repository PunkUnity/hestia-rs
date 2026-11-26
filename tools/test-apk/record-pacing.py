#!/usr/bin/env python3
"""Record compositor presentation intervals for Hestia RS (test) on a connected Android device."""

import argparse
import json
import math
import re
import shlex
import statistics
import subprocess
import time
from pathlib import Path


ACTIVITY = "com.mroz59.hestia.rs.test/com.mroz59.hestia.rs.HestiaActivity"


def adb_shell(adb, command, timeout=6):
    return subprocess.run(
        [*adb, "shell", command],
        check=True,
        capture_output=True,
        text=True,
        timeout=timeout,
    ).stdout


def layers(output):
    names = set()
    for line in output.splitlines():
        name = line.strip()
        if name.startswith("RequestedLayerState{"):
            match = re.match(r"RequestedLayerState\{(.+?#\d+)(?:\s|})", name)
            if not match:
                continue
            name = match[1]
        if ACTIVITY in name and "SurfaceView" in name and "Background" not in name:
            names.add(name)
    return sorted(names)


def presentation_times(snapshot):
    times = set()
    for line in snapshot.splitlines():
        fields = line.split()
        if len(fields) == 3 and all(field.isdecimal() for field in fields):
            time_ns = int(fields[1])
            if 0 < time_ns < 9_000_000_000_000_000_000:
                times.add(time_ns)
    return times


def layer_query(name):
    return f"dumpsys -t 2 SurfaceFlinger --latency {shlex.quote(name)}"


def find_layer(adb):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        candidates = layers(adb_shell(adb, "dumpsys -t 2 SurfaceFlinger --list"))
        before = {name: presentation_times(adb_shell(adb, layer_query(name))) for name in candidates}
        time.sleep(0.35)
        active = [
            name for name, timestamps in before.items()
            if presentation_times(adb_shell(adb, layer_query(name))) - timestamps
        ]
        preferred = [name for name in active if "(BLAST)" in name] or active
        if len(preferred) == 1:
            return preferred[0]
        if len(preferred) > 1:
            raise ValueError("Multiple active Hestia RS video surfaces. Keep one stream open.")
    raise ValueError("No active Hestia RS Direct video surface. Leave the stream open with continuous motion.")


def sample_command(layer, seconds, fps):
    # Run the sampling loop on Android so wireless ADB round-trip time does
    # not stretch the interval between queries.
    query = layer_query(layer)
    pause = "0.10" if fps > 60 else "0.20"
    return f"""
clock() {{
    read uptime rest < /proc/uptime
    centiseconds=$(( ${{uptime%.*}} * 100 + 1${{uptime#*.}} - 100 ))
}}
clock
deadline=$(( centiseconds + {seconds * 100} ))
while [ "$centiseconds" -lt "$deadline" ]; do
    printf '\\n=== snapshot ===\\n'
    {query} || exit 1
    sleep {pause}
    clock
done
"""


def measure(raw, fps):
    snapshots = [
        presentation_times(sample)
        for sample in raw.split("=== snapshot ===")[1:]
    ]
    if len(snapshots) < 2 or any(not snapshot for snapshot in snapshots):
        raise ValueError("Surface timing was unavailable during part of the capture.")
    if any(not a.intersection(b) for a, b in zip(snapshots, snapshots[1:])):
        raise ValueError("Surface history changed or did not overlap. Capture cannot be scored.")

    baseline = max(snapshots[0])
    timestamps = sorted({
        time_ns for snapshot in snapshots[1:] for time_ns in snapshot if time_ns > baseline
    })
    gaps = sorted((b - a) / 1e6 for a, b in zip(timestamps, timestamps[1:]))
    if len(gaps) < 30:
        raise ValueError("Too few presentation intervals. Use continuous motion.")
    target = 1000 / fps
    span = (timestamps[-1] - timestamps[0]) / 1e9
    return timestamps, {
        "requested_fps": fps,
        "intervals": len(gaps),
        "observed_span_s": span,
        "presentation_events_per_s": len(gaps) / span,
        "p50_gap_ms": statistics.median(gaps),
        "p95_gap_ms": gaps[math.ceil(0.95 * len(gaps)) - 1],
        "p99_gap_ms": gaps[math.ceil(0.99 * len(gaps)) - 1],
        "longest_gap_ms": gaps[-1],
        "gaps_at_least_100_ms": sum(gap >= 100 for gap in gaps),
        "uneven_percent": 100 * sum(not 0.7 * target <= gap <= 1.3 * target for gap in gaps) / len(gaps),
    }


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("output", type=Path)
    cli.add_argument("--fps", type=int, choices=(30, 60, 90, 120), default=60)
    cli.add_argument("--seconds", type=int, choices=range(10, 61), default=20, metavar="10–60")
    cli.add_argument("--serial", help="ADB serial if multiple devices are connected")
    args = cli.parse_args()
    adb = ["adb", "-s", args.serial] if args.serial else ["adb"]
    try:
        args.output.mkdir(parents=True, exist_ok=False)
        print("Looking for the active Hestia RS video surface. Keep motion on screen.", flush=True)
        layer = find_layer(adb)
        print(f"Recording {args.seconds} seconds.", flush=True)
        raw = adb_shell(adb, sample_command(layer, args.seconds, args.fps), args.seconds + 12)
        (args.output / "surfaceflinger.txt").write_text(raw, encoding="utf-8")
        timestamps, metrics = measure(raw, args.fps)
        (args.output / "presented-ns.txt").write_text("\n".join(map(str, timestamps)) + "\n", encoding="utf-8")
        summary = json.dumps({"layer": layer, **metrics}, indent=2) + "\n"
        (args.output / "summary.json").write_text(summary, encoding="utf-8")
        print(summary)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        cli.exit(1, f"{getattr(error, 'stderr', None) or error}\n")


if __name__ == "__main__":
    main()
