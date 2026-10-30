"""Run `git-dag-parser init` on one repo and record time, memory and phase timings.

Usage: harness.py LABEL REPO_PATH OUT_JSON --bin PATH [--limit-gb N] [--timeout-s N]

Writes OUT_JSON (summary), plus .stdout, .stderr and .timeline.csv beside it.
The phase timings are read from the tool's info logs, so RUST_LOG is set to
HARNESS_LOG, default `info`. The logs go to stdout.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import time
from pathlib import Path



def proc_kb(pid: int, field: str) -> int:
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith(field + ":"):
                return int(line.split()[1])
    except (FileNotFoundError, ProcessLookupError):
        pass
    return 0


def child_of(pid: int) -> int | None:
    try:
        kids = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
    except FileNotFoundError:
        return None
    return int(kids[0]) if kids else None


def duration_s(text: str) -> float:
    """Parse a Rust Debug Duration such as 1.234s, 56.7ms, 890µs, 12ns."""
    m = re.fullmatch(r"([\d.]+)(ns|µs|us|ms|s)", text.strip())
    if not m:
        return float("nan")
    scale = {"ns": 1e-9, "µs": 1e-6, "us": 1e-6, "ms": 1e-3, "s": 1.0}[m.group(2)]
    return float(m.group(1)) * scale


PHASES = {
    "pass1_s": r"Pass 1 \(headers\):.* in (\S+)$",
    "pass2_s": r"Pass 2 \(bodies\):.* in (\S+)$",
    "parse_s": r"^Parse:\s+(\S+)$",
    "total_s": r"^Total:\s+(\S+)$",
}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("label")
    ap.add_argument("repo")
    ap.add_argument("out")
    ap.add_argument("--bin", required=True, help="git-dag-parser binary to run")
    ap.add_argument("--limit-gb", type=float, default=48.0)
    ap.add_argument("--timeout-s", type=float, default=3600.0)
    args = ap.parse_args()

    out_txt = Path(args.out).with_suffix(".stdout")
    err_txt = Path(args.out).with_suffix(".stderr")
    start = time.monotonic()
    with out_txt.open("w") as so, err_txt.open("w") as se:
        proc = subprocess.Popen(
            ["/usr/bin/time", "-v", args.bin, "init", "--repo", args.repo],
            stdout=so,
            stderr=se,
            env={"RUST_LOG": __import__("os").environ.get("HARNESS_LOG", "info"), "PATH": "/usr/bin:/bin"},
        )
        peak_anon = peak_file = 0
        killed = None
        timeline = []
        while proc.poll() is None:
            child = child_of(proc.pid)
            if child:
                anon = proc_kb(child, "RssAnon")
                timeline.append((time.monotonic() - start, anon))
                peak_anon = max(peak_anon, anon)
                peak_file = max(peak_file, proc_kb(child, "RssFile"))
                elapsed = time.monotonic() - start
                if anon > args.limit_gb * 2**20:
                    killed = f"RssAnon over {args.limit_gb} GB at {elapsed:.0f}s"
                elif elapsed > args.timeout_s:
                    killed = f"timeout {args.timeout_s:.0f}s"
                if killed:
                    subprocess.run(["kill", str(child)])
                    proc.wait()
                    break
            time.sleep(0.2)
    wall = time.monotonic() - start

    stdout = out_txt.read_text()
    stderr = err_txt.read_text()
    result = {
        "label": args.label,
        "repo": args.repo,
        "wall_s": round(wall, 3),
        "peak_rss_anon_mb": round(peak_anon / 1024, 1),
        "peak_rss_file_mb": round(peak_file / 1024, 1),
        "killed": killed,
        "exit": proc.returncode,
    }
    for key, pattern in PHASES.items():
        # The summary prints an object count on an earlier `Total:` line
        found = re.findall(pattern, stdout, re.M)
        result[key] = round(duration_s(found[-1]), 3) if found else None
    m = re.search(r"Pass 1 \(headers\): C:(\d+) T:(\d+) B:(\d+) t:(\d+) from (\d+) copies", stdout)
    if m:
        result.update(dict(zip(["commits", "trees", "blobs", "tags", "copies"], map(int, m.groups()))))
    for key, pattern in {
        "user_s": r"User time \(seconds\): ([\d.]+)",
        "sys_s": r"System time \(seconds\): ([\d.]+)",
        "cpu_pct": r"Percent of CPU this job got: (\d+)%",
        "max_rss_mb": r"Maximum resident set size \(kbytes\): (\d+)",
        "major_faults": r"Major \(requiring I/O\) page faults: (\d+)",
    }.items():
        m = re.search(pattern, stderr)
        if m:
            v = float(m.group(1))
            result[key] = round(v / 1024, 1) if key == "max_rss_mb" else v
    # Peak heap within each phase window, from the phase durations printed
    if result.get("pass1_s") is not None and result.get("pass2_s") is not None:
        p1 = result["pass1_s"]
        p2 = p1 + result["pass2_s"]
        end = result.get("total_s") or wall
        windows = {"pass1": (0, p1), "pass2": (p1, p2), "resolve_refs": (p2, end + 1)}
        for name, (lo, hi) in windows.items():
            vals = [a for t, a in timeline if lo <= t < hi]
            result[f"peak_anon_mb_{name}"] = round(max(vals) / 1024, 1) if vals else None
    Path(args.out).with_suffix(".timeline.csv").write_text(
        "t_s,rss_anon_mb\n" + "".join(f"{t:.2f},{a / 1024:.1f}\n" for t, a in timeline)
    )
    Path(args.out).write_text(json.dumps(result, indent=1))
    print(json.dumps(result))


if __name__ == "__main__":
    main()
