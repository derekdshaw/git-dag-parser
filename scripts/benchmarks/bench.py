"""Run the divan benchmarks of git-dag-parser, keep the results, and compare runs.

divan prints a table and has no saved baselines or machine-readable output,
so this script saves the raw table with run details and a parsed JSON copy.

Usage:
  bench.py run [--repo PATH] [--label NAME] [-- DIVAN_ARGS...]
      Run `cargo bench --features bench --bench parser` in the repository and
      save <out>/<UTC time>-<commit>[-dirty][-label].txt and .json.
  bench.py parse FILE.txt
      Parse a saved table and print the JSON.
  bench.py compare OLD NEW
      Compare two saved runs (.txt or .json): median time and allocations.

Results go to `docs/benchmarks/` in the repository.
"""
from __future__ import annotations

import argparse
import json
import platform
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

# This script is in <repo>/scripts/benchmarks/
REPO_ROOT = Path(__file__).resolve().parents[2]
OUT_DIR = REPO_ROOT / "docs" / "benchmarks"
DEFAULT_REPO = REPO_ROOT

TIME_NS = {"ns": 1.0, "µs": 1e3, "us": 1e3, "ms": 1e6, "s": 1e9}
BYTES = {"B": 1, "KB": 1e3, "MB": 1e6, "GB": 1e9, "TB": 1e12,
         "KiB": 2**10, "MiB": 2**20, "GiB": 2**30, "TiB": 2**40}
STATS = ["fastest", "slowest", "median", "mean"]


def parse_number(text: str) -> float | None:
    """A divan cell: a time, a byte size, a plain count, or a throughput."""
    text = text.strip()
    m = re.fullmatch(r"([\d.]+)\s*(ns|µs|us|ms|s)", text)
    if m:
        return float(m.group(1)) * TIME_NS[m.group(2)]
    m = re.fullmatch(r"([\d.]+)\s*(B|KB|MB|GB|TB|KiB|MiB|GiB|TiB)", text)
    if m:
        return float(m.group(1)) * BYTES[m.group(2)]
    m = re.fullmatch(r"[\d.]+", text)
    if m:
        return float(text)
    return None


def is_time(text: str) -> bool:
    return re.fullmatch(r"[\d.]+\s*(ns|µs|us|ms|s)", text.strip()) is not None


def parse_table(text: str) -> dict:
    """Benchmarks by path, each with time stats (ns), samples, iters,
    throughput text, and allocation stats {label: {count: {...}, bytes: {...}}}.

    Output that the measured code prints itself can split a benchmark's row:
    the name, then the program's lines, then the values on a row of their own.
    Such a values row is joined to the last named benchmark."""
    lines = text.splitlines()
    header_i = next(i for i, l in enumerate(lines) if "fastest" in l and "median" in l)
    start = lines[header_i].index("fastest")
    stack: list[tuple[int, str]] = []
    benches: dict[str, dict] = {}
    current = None
    label = None
    pending = 0  # value lines left for the current allocation label

    def new_bench(cols: list[str]) -> dict:
        path = "/".join(n for _, n in stack if n != "benches")
        benches[path] = {"time_ns": dict(zip(STATS, map(parse_number, cols[:4]))),
                         "samples": int(cols[4]), "iters": int(cols[5]), "alloc": {}}
        return benches[path]

    for line in lines[header_i + 1:]:
        if not line.strip():
            continue
        if "│" not in line:
            # Printed by the code being measured
            continue
        # A values row split from its name starts at column 0 with the time
        if current is None and stack and is_time(line.split("│")[0]):
            cols = [c.strip() for c in line.split("│")]
            if len(cols) >= 6:
                current = new_bench(cols)
                continue
        left, right = line[:start], line[start:]
        cols = [c.strip() for c in right.split("│")]
        marker = max(left.rfind("├─"), left.rfind("╰─"))
        if marker >= 0:
            depth = marker // 3
            name = left[marker + 2:].split("  ")[0].strip()
            name = re.sub(r"\s*\(ignored\)$", "", name)
            while stack and stack[-1][0] >= depth:
                stack.pop()
            stack.append((depth, name))
            current, label, pending = None, None, 0
            if is_time(cols[0]):
                current = new_bench(cols)
            continue
        if current is None or not cols[0]:
            continue
        if cols[0].endswith(":"):
            label, pending = cols[0][:-1], 2
            current["alloc"][label] = {}
        elif "/s" in cols[0] and label is None:
            current["throughput"] = dict(zip(STATS, cols[:4]))
        elif pending:
            kind = "count" if pending == 2 else "bytes"
            current["alloc"][label][kind] = dict(zip(STATS, map(parse_number, cols[:4])))
            pending -= 1
    return benches


def run(args: argparse.Namespace) -> None:
    repo = Path(args.repo).resolve()
    git = lambda *a: subprocess.run(["git", "-C", str(repo), *a], capture_output=True, text=True).stdout.strip()
    commit = git("rev-parse", "--short", "HEAD")
    dirty = bool(git("status", "--porcelain", "--untracked-files=no"))
    lock = (repo / "Cargo.lock").read_text()
    m = re.search(r'name = "divan"\nversion = "([^"]+)"', lock)
    meta = {
        "time_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "commit": commit,
        "dirty": dirty,
        "branch": git("rev-parse", "--abbrev-ref", "HEAD"),
        "rustc": subprocess.run(["rustc", "-V"], capture_output=True, text=True).stdout.strip(),
        "divan": m.group(1) if m else None,
        "host": platform.node(),
        "kernel": platform.release(),
        "cpus": int(subprocess.run(["nproc"], capture_output=True, text=True).stdout.strip()),
        "label": args.label,
        "divan_args": args.divan_args,
    }
    cmd = ["cargo", "bench", "--features", "bench", "--bench", "parser", "--", "--color", "never", *args.divan_args]
    proc = subprocess.run(cmd, cwd=repo, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        sys.exit(proc.returncode)

    OUT_DIR.mkdir(exist_ok=True)
    name = f"{meta['time_utc'].replace(':', '').replace('-', '')}-{commit}"
    name += "-dirty" if dirty else ""
    name += f"-{args.label}" if args.label else ""
    txt = OUT_DIR / f"{name}.txt"
    header = "".join(f"# {k}: {v}\n" for k, v in meta.items())
    txt.write_text(header + "\n" + proc.stdout)
    (OUT_DIR / f"{name}.json").write_text(json.dumps({"meta": meta, "benchmarks": parse_table(proc.stdout)}, indent=1))
    print(proc.stdout)
    print(f"Saved {txt} and {txt.with_suffix('.json')}")


def load(path: str) -> dict:
    p = Path(path)
    if p.suffix == ".json":
        return json.loads(p.read_text())
    text = p.read_text()
    meta = dict(re.findall(r"^# (\w+): (.*)$", text, re.M))
    return {"meta": meta, "benchmarks": parse_table(text)}


def fmt_ns(ns: float) -> str:
    for unit, scale in (("s", 1e9), ("ms", 1e6), ("µs", 1e3)):
        if ns >= scale:
            return f"{ns / scale:.3g} {unit}"
    return f"{ns:.3g} ns"


def compare(args: argparse.Namespace) -> None:
    old, new = load(args.old), load(args.new)
    for side, d in (("old", old), ("new", new)):
        m = d["meta"]
        print(f"{side}: {m.get('commit')}{' (dirty)' if m.get('dirty') in (True, 'True') else ''} "
              f"{m.get('time_utc', '')} {m.get('label') or ''}")
    print()
    print("| Benchmark | Old median | New median | Change | Old allocs | New allocs | Old bytes | New bytes |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    names = sorted(set(old["benchmarks"]) | set(new["benchmarks"]))
    for name in names:
        o, n = old["benchmarks"].get(name), new["benchmarks"].get(name)
        if not (o and n):
            print(f"| {name} | {'–' if not o else fmt_ns(o['time_ns']['median'])} | "
                  f"{'–' if not n else fmt_ns(n['time_ns']['median'])} | only in {'new' if n else 'old'} | | | | |")
            continue
        om, nm = o["time_ns"]["median"], n["time_ns"]["median"]

        def alloc(bench: dict, kind: str) -> float:
            # divan leaves out the allocation rows when a benchmark allocates nothing
            return (bench["alloc"].get("alloc", {}).get(kind) or {}).get("median") or 0.0

        def cell(v: float, bytes_: bool = False) -> str:
            if bytes_:
                for unit, scale in (("GB", 1e9), ("MB", 1e6), ("KB", 1e3)):
                    if v >= scale:
                        return f"{v / scale:.3g} {unit}"
                return f"{v:,.0f} B"
            return f"{v:,.0f}"
        print(f"| {name} | {fmt_ns(om)} | {fmt_ns(nm)} | {100 * (nm - om) / om:+.1f}% | "
              f"{cell(alloc(o, 'count'))} | {cell(alloc(n, 'count'))} | "
              f"{cell(alloc(o, 'bytes'), True)} | {cell(alloc(n, 'bytes'), True)} |")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--repo", default=str(DEFAULT_REPO))
    r.add_argument("--label")
    r.add_argument("divan_args", nargs=argparse.REMAINDER)
    p = sub.add_parser("parse")
    p.add_argument("file")
    c = sub.add_parser("compare")
    c.add_argument("old")
    c.add_argument("new")
    args = ap.parse_args()
    if args.cmd == "run":
        if args.divan_args[:1] == ["--"]:
            args.divan_args = args.divan_args[1:]
        run(args)
    elif args.cmd == "parse":
        print(json.dumps(load(args.file)["benchmarks"], indent=1))
    else:
        compare(args)


if __name__ == "__main__":
    main()
