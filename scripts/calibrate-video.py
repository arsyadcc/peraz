#!/usr/bin/env python3
"""Measure size and VMAF for every video encoder and quality setting, and pick preset values.

Usage:
    scripts/calibrate-video.py --out results/ CLIP [CLIP ...]

A CLIP is `path`, `path@start` or `path@start+duration` (seconds), e.g. `talk.mov@30+6`.

For each clip, a lossless reference (FFV1, at most 1080p) is made first. Every available encoder
then encodes the reference across a sweep of quality settings, with the same arguments Peraz uses,
and each result is scored with VMAF (libvmaf, model vmaf_v0.6.1) against the reference.

For each preset, the chosen setting is the most economical one whose mean VMAF over all clips
reaches the preset's target and whose worst clip stays within 3 points of it. Results are written
to `results.json` and printed as a table. Needs FFmpeg with libvmaf, libx264, and libx265
(VideoToolbox encoders are measured when present). Only the Python standard library is used.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys

# Preset → target mean VMAF.
TARGETS = {"highest": 97.0, "high": 95.0, "balanced": 93.0, "small": 89.0, "smallest": 84.0}

# Encoder → (quality flag, values to sweep from best to most economical, extra arguments).
ENCODERS = {
    "libx264": ("-crf", list(range(14, 36, 2)), []),
    "libx265": ("-crf", list(range(16, 40, 2)), ["-tag:v", "hvc1", "-x265-params", "log-level=error"]),
    "h264_videotoolbox": ("-q:v", list(range(90, 35, -5)), []),
    "hevc_videotoolbox": ("-q:v", list(range(90, 35, -5)), ["-tag:v", "hvc1"]),
}


def run(args):
    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(f"{' '.join(args)}\n{result.stderr.strip()[-2000:]}")
    return result


def available_encoders():
    listing = run(["ffmpeg", "-hide_banner", "-encoders"]).stdout
    return [name for name in ENCODERS if f" {name} " in listing]


def parse_clip(spec):
    path, _, rest = spec.partition("@")
    start, _, duration = rest.partition("+")
    return path, float(start or 0), float(duration or 6)


def make_reference(spec, index, work):
    path, start, duration = parse_clip(spec)
    reference = os.path.join(work, f"ref{index}.mkv")
    if not os.path.exists(reference):
        run([
            "ffmpeg", "-hide_banner", "-v", "error", "-y", "-ss", str(start), "-t", str(duration), "-i", path,
            "-an", "-vf", "scale='min(1920,iw)':'min(1080,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2:flags=lanczos,format=yuv420p",
            "-c:v", "ffv1", reference,
        ])
    return reference


def vmaf(distorted, reference):
    log = distorted + ".vmaf.json"
    threads = str(os.cpu_count() or 4)
    run([
        "ffmpeg", "-hide_banner", "-v", "error", "-i", distorted, "-i", reference,
        # Pair frames by position: the reference (MKV, millisecond timestamps) and the encode (MP4)
        # round frame times differently, and pairing by time would compare neighbouring frames.
        "-lavfi", "[0:v]settb=1/1000,setpts=N[d];[1:v]settb=1/1000,setpts=N[r];"
        f"[d][r]libvmaf=log_fmt=json:log_path={log}:n_threads={threads}",
        "-f", "null", "-",
    ])
    with open(log) as f:
        return json.load(f)["pooled_metrics"]["vmaf"]["mean"]


def measure(encoder, value, reference, work, index):
    flag, _, extra = ENCODERS[encoder]
    out = os.path.join(work, f"{encoder}-{value}-{index}.mp4")
    run([
        "ffmpeg", "-hide_banner", "-v", "error", "-y", "-i", reference, "-c:v", encoder, flag, str(value),
        *extra, "-pix_fmt", "yuv420p", "-an", "-movflags", "+faststart", out,
    ])
    seconds = float(run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", out]).stdout)
    size = os.path.getsize(out)
    score = vmaf(out, reference)
    os.remove(out)
    return {"vmaf": score, "bytes": size, "kbps": size * 8 / seconds / 1000}


def choose(rows):
    """For each preset: the most economical value meeting the target (None if none does)."""
    chosen = {}
    for preset, target in TARGETS.items():
        meeting = [r for r in rows if r["mean_vmaf"] >= target and r["min_vmaf"] >= target - 3]
        # The last one in sweep order is the most economical.
        chosen[preset] = meeting[-1]["value"] if meeting else None
    return chosen


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", required=True, help="folder for references and results")
    parser.add_argument("--encoders", help="comma-separated subset to measure (default: all available)")
    parser.add_argument("clips", nargs="+")
    args = parser.parse_args()
    os.makedirs(args.out, exist_ok=True)

    references = [make_reference(spec, i, args.out) for i, spec in enumerate(args.clips)]
    path = os.path.join(args.out, "results.json")
    # Measuring a subset updates those encoders and keeps earlier results for the others.
    results = {"targets": TARGETS, "clips": args.clips, "encoders": {}}
    if args.encoders and os.path.exists(path):
        with open(path) as f:
            results["encoders"] = json.load(f).get("encoders", {})
    wanted = set(args.encoders.split(",")) if args.encoders else None
    for encoder in available_encoders():
        if wanted is not None and encoder not in wanted:
            continue
        flag, values, _ = ENCODERS[encoder]
        rows = []
        for value in values:
            per_clip = [measure(encoder, value, ref, args.out, i) for i, ref in enumerate(references)]
            row = {
                "value": value,
                "mean_vmaf": statistics.mean(c["vmaf"] for c in per_clip),
                "min_vmaf": min(c["vmaf"] for c in per_clip),
                "mean_kbps": statistics.mean(c["kbps"] for c in per_clip),
                "clips": per_clip,
            }
            rows.append(row)
            print(f"{encoder:18} {flag} {value:>3}  VMAF mean {row['mean_vmaf']:6.2f}  min {row['min_vmaf']:6.2f}  "
                  f"{row['mean_kbps']:8.0f} kb/s", flush=True)
        results["encoders"][encoder] = {"flag": flag, "rows": rows, "chosen": choose(rows)}

    with open(path, "w") as f:
        json.dump(results, f, indent=2)
    print("\nChosen values (preset: setting):")
    for encoder, data in results["encoders"].items():
        picks = ", ".join(f"{p}={v}" for p, v in data["chosen"].items())
        print(f"  {encoder:18} {data['flag']}  {picks}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
