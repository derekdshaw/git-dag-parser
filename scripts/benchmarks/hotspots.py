"""Summarize a samply profile (saved with --save-only --unstable-presymbolicate).

Usage: hotspots.py PROFILE.json.gz [--top N] [--thread-prefix NAME]

Prints CPU samples per thread, the top functions by self time and by
inclusive time (counted once per sample). Names are the symbols that contain
each address. Inlined functions are not shown on their own: samply keeps one
inline-frame list per symbol, which is not valid for every address in it.
"""
from __future__ import annotations

import argparse
import bisect
import gzip
import json
import re
from collections import Counter
from pathlib import Path


def load_symbols(path: Path) -> dict[str, tuple[list[int], list[dict], list[str]]]:
    syms = json.loads(path.read_text())
    strings = syms["string_table"]
    libs = {}
    for lib in syms["data"]:
        table = sorted(lib["symbol_table"], key=lambda e: e["rva"])
        libs[lib["debug_name"]] = ([e["rva"] for e in table], table, strings)
    return libs


def clean(name: str) -> str:
    # Drop the Rust hash suffix and generic noise to keep names readable
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    return name


def lookup(libs, lib_name: str, rva: int) -> list[str]:
    """Function names at an address, innermost inlined function first and
    the outer symbol last."""
    if lib_name not in libs or rva < 0:
        return [f"{lib_name}+{rva:#x}"]
    rvas, table, strings = libs[lib_name]
    i = bisect.bisect_right(rvas, rva) - 1
    if i < 0 or rva >= table[i]["rva"] + table[i]["size"]:
        return [f"{lib_name}+{rva:#x}"]
    return [clean(strings[table[i]["symbol"]])]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("profile")
    ap.add_argument("--top", type=int, default=25)
    ap.add_argument("--thread-prefix", default="git-dag-parser")
    ap.add_argument("--main-only", action="store_true", help="only the busiest thread")
    ap.add_argument("--callers", action="append", default=[],
                    help="also list the nearest caller frames of samples whose leaf is this function")
    args = ap.parse_args()

    profile_path = Path(args.profile)
    prof = json.load(gzip.open(profile_path))
    syms_path = profile_path.with_name(profile_path.name.replace(".json.gz", ".json.syms.json"))
    libs = load_symbols(syms_path)
    lib_names = [lib["debugName"] for lib in prof["libs"]]
    interval_ms = prof["meta"]["interval"]

    self_inner: Counter[str] = Counter()
    incl_outer: Counter[str] = Counter()
    per_thread: Counter[str] = Counter()
    callers: dict[str, Counter[str]] = {name: Counter() for name in args.callers}
    total = 0

    threads = [t for t in prof["threads"] if t["name"].startswith(args.thread_prefix)]
    if args.main_only:
        threads = [max(threads, key=lambda t: t["samples"]["length"])]
    for th in threads:
        frames = th["frameTable"]
        funcs = th["funcTable"]
        res = th["resourceTable"]
        stacks = th["stackTable"]
        cache: dict[int, list[str]] = {}

        def frame_names(fi: int) -> list[str]:
            if fi not in cache:
                r = funcs["resource"][frames["func"][fi]]
                lib = lib_names[res["lib"][r]] if r >= 0 else "?"
                cache[fi] = lookup(libs, lib, frames["address"][fi])
            return cache[fi]

        samples = th["samples"]
        weights = samples.get("weight") or [1] * samples["length"]
        tid = th["tid"]
        for si, st in enumerate(samples["stack"]):
            if st is None:
                continue
            w = weights[si] if weights[si] is not None else 1
            total += w
            per_thread[f"{th['name']}:{tid}"] += w
            leaf = True
            seen = set()
            s = st
            leaf_name = frame_names(stacks["frame"][st])[0]
            if leaf_name in callers:
                # Nearest frame that is not the leaf function itself
                p = stacks["prefix"][st]
                while p is not None and frame_names(stacks["frame"][p])[0] == leaf_name:
                    p = stacks["prefix"][p]
                callers[leaf_name][frame_names(stacks["frame"][p])[0] if p is not None else "(none)"] += w
            while s is not None:
                names = frame_names(stacks["frame"][s])
                if leaf:
                    self_inner[names[0]] += w
                    leaf = False
                for name in names:
                    if name not in seen:
                        incl_outer[name] += w
                        seen.add(name)
                s = stacks["prefix"][s]

    busy = [(t, n) for t, n in per_thread.items() if n > 0]
    print(f"Total CPU samples: {total} ({total * interval_ms / 1000:.1f} s at {interval_ms} ms)")
    print(f"Threads with samples: {len(busy)}; top: "
          + ", ".join(f"{t}={n * interval_ms / 1000:.1f}s" for t, n in sorted(busy, key=lambda x: -x[1])[:4]))
    print(f"\nTop {args.top} by self time:")
    for name, n in self_inner.most_common(args.top):
        print(f"  {100 * n / total:5.1f}%  {n * interval_ms / 1000:7.1f}s  {name[:150]}")
    print(f"\nTop {args.top} by inclusive time:")
    for name, n in incl_outer.most_common(args.top):
        print(f"  {100 * n / total:5.1f}%  {n * interval_ms / 1000:7.1f}s  {name[:150]}")
    for name, counts in callers.items():
        n_all = sum(counts.values()) or 1
        print(f"\nNearest callers of {name}:")
        for caller, n in counts.most_common(8):
            print(f"  {100 * n / n_all:5.1f}%  {n * interval_ms / 1000:7.1f}s  {caller[:150]}")


if __name__ == "__main__":
    main()
