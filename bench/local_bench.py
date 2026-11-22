#!/usr/bin/env python3
"""Quick native timing of a pipeline binary, no Docker (Windows, Linux or macOS).

    python bench/local_bench.py BINARY DATA_DIR [-n 3] [--cpus 8] [--compare REF] [--warmup]

Runs BINARY N times on DATA_DIR with DATA_DIR, OUTPUT_DIR, SCRATCH_DIR, NCPU and MEM_GB set the
way the scorer sets them and SS_TIMING=1, then prints each wall time, the median, and a table
of the `SS_TIMING stage=<name> ms=<float>` lines (median per stage).

Examples (Git Bash on the Windows box):

    cargo build --release
    python bench/local_bench.py target/release/pipeline.exe /c/Users/zuyua/dev/ss_work/data/s2_20m --cpus 8
    python bench/local_bench.py target/release/pipeline.exe ../data/dev --compare ../dev-answer

These are native numbers on your own machine and operating system: use them to rank changes, not
to predict the score. The numbers that count come from bench-20m in CI (fresh Linux containers).
While other people or agents time things on the same machine, run this through the shared lock:

    /c/Users/zuyua/dev/ss_work/tools/timed.sh python bench/local_bench.py ...
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def parse_ss_timing(text, kind="stage"):
    """[(name, ms)] in the order printed; a name printed twice in one run is summed. kind="stage" reads
    the wall-clock stages, kind="detail" the `SS_TIMING detail=` lines (summed worker time or work
    that overlaps a stage), which are shown apart and never added up with the stages."""
    stages = {}
    for line in text.splitlines():
        match = re.match(rf"^\s*SS_TIMING\s+{kind}=(\S+)\s+ms=([-+0-9.eE]+)\s*$", line)
        if match:
            try:
                stages[match.group(1)] = stages.get(match.group(1), 0.0) + float(match.group(2))
            except ValueError:
                pass
    return list(stages.items())


def pin_to(cpus):
    """Pins this process, and so the children it starts, to the given logical CPUs. Returns a description."""
    try:
        if hasattr(os, "sched_setaffinity"):
            os.sched_setaffinity(0, cpus)
        elif os.name == "nt":
            import ctypes
            mask = sum(1 << cpu for cpu in cpus)
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.GetCurrentProcess.restype = ctypes.c_void_p
            kernel32.SetProcessAffinityMask.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
            if not kernel32.SetProcessAffinityMask(kernel32.GetCurrentProcess(), mask):
                return f"not pinned (SetProcessAffinityMask failed, error {ctypes.get_last_error()})"
        else:
            return "not pinned (no affinity call on this platform)"
    except OSError as error:
        return f"not pinned ({error})"
    return "pinned to logical CPUs " + ",".join(str(cpu) for cpu in cpus)


def tree_bytes(path):
    return sum(entry.stat().st_size for entry in Path(path).rglob("*") if entry.is_file())


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binary", help="the pipeline executable")
    parser.add_argument("data", help="dataset directory (events/, clicks/, links/, products.csv, ...)")
    parser.add_argument("-n", "--runs", type=int, default=3, help="timed runs (default 3)")
    parser.add_argument("--cpus", type=int, help="set NCPU to this and pin to that many logical CPUs (the scorer has 8)")
    parser.add_argument("--no-pin", action="store_true", help="with --cpus: set NCPU only, do not pin")
    parser.add_argument("--mem-gb", type=int, default=24, help="MEM_GB for the pipeline (default 24, as on the scorer)")
    parser.add_argument("--out", help="keep outputs here (out/run-1, ...) instead of a temporary folder")
    parser.add_argument("--compare", metavar="REF", help="after the runs, diff the last output against REF with "
                        "scripts/compare.py (a folder of the two tables as Parquet folders or CSV files)")
    parser.add_argument("--warmup", action="store_true", help="one untimed run first (warms the file cache)")
    parser.add_argument("--env", action="append", default=[], metavar="KEY=VALUE", help="extra environment for the pipeline")
    parser.add_argument("--no-timing", action="store_true", help="do not set SS_TIMING=1 (time the uninstrumented path)")
    args = parser.parse_args()

    binary = Path(args.binary).resolve()
    data = Path(args.data).resolve()
    if not binary.is_file():
        sys.exit(f"no such binary: {binary}")
    if not (data / "manifest.json").is_file():
        sys.exit(f"not a dataset directory (no manifest.json): {data}")

    logical = os.cpu_count() or 1
    ncpu = args.cpus or logical
    pinning = "not pinned (all logical CPUs)"
    if args.cpus and not args.no_pin:
        pinning = pin_to(list(range(min(args.cpus, logical))))

    base = Path(args.out).resolve() if args.out else Path(tempfile.mkdtemp(prefix="ss-local-bench-"))
    base.mkdir(parents=True, exist_ok=True)
    environment = dict(os.environ, DATA_DIR=str(data), NCPU=str(ncpu), MEM_GB=str(args.mem_gb))
    if not args.no_timing:
        environment["SS_TIMING"] = "1"
    for pair in args.env:
        key, _, value = pair.partition("=")
        environment[key] = value

    input_bytes = tree_bytes(data)
    print(f"binary   {binary}")
    print(f"data     {data}  ({input_bytes:,} bytes)")
    print(f"machine  {logical} logical CPUs, NCPU={ncpu}, {pinning}, MEM_GB={args.mem_gb}")
    print(f"cache    the operating system's file cache is whatever earlier reads left; "
          f"{'one untimed warm-up run first' if args.warmup else 'pass --warmup for an untimed first run'}")

    def one(label):
        out, scratch = base / label, base / f"{label}-scratch"
        for folder in (out, scratch):
            shutil.rmtree(folder, ignore_errors=True)
            folder.mkdir(parents=True)
        env = dict(environment, OUTPUT_DIR=str(out), SCRATCH_DIR=str(scratch))
        started = time.perf_counter()
        done = subprocess.run([str(binary)], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace")
        wall_ms = (time.perf_counter() - started) * 1e3
        shutil.rmtree(scratch, ignore_errors=True)
        return {"label": label, "wall_ms": wall_ms, "exit": done.returncode, "stages": parse_ss_timing(done.stderr),
                "details": parse_ss_timing(done.stderr, kind="detail"), "stderr": done.stderr, "out": out}

    if args.warmup:
        warm = one("warmup")
        print(f"warm-up  {warm['wall_ms']:10.1f} ms  exit {warm['exit']}  (not counted)")
        shutil.rmtree(warm["out"], ignore_errors=True)

    runs = []
    for index in range(args.runs):
        run = one(f"run-{index + 1}")
        runs.append(run)
        total = dict(run["stages"]).get("total")
        print(f"run {index + 1:<3}  {run['wall_ms']:10.1f} ms  exit {run['exit']}"
              + (f"  in-process total {total:.1f} ms" if total is not None else "")
              + f"  output {tree_bytes(run['out']):,} bytes")
        if run["exit"] != 0:
            print("  stderr: " + run["stderr"].strip()[-1500:].replace("\n", "\n  "))
        if index + 1 < args.runs and not args.out:
            shutil.rmtree(run["out"], ignore_errors=True)

    walls = [run["wall_ms"] for run in runs]
    print(f"\nwall     median {statistics.median(walls):.1f} ms   min {min(walls):.1f}   max {max(walls):.1f}   "
          f"(process start to exit, {len(runs)} runs)")

    names = []
    for run in runs:
        for name, _ in run["stages"]:
            if name not in names:
                names.append(name)
    if names:
        names = [name for name in names if name != "total"] + (["total"] if "total" in names else [])
        medians = {name: statistics.median([dict(run["stages"])[name] for run in runs if name in dict(run["stages"])]) for name in names}
        whole = medians.get("total") or sum(value for name, value in medians.items() if name != "total") or 1.0
        width = max(len(name) for name in names + ["stage"])
        print(f"\n{'stage':<{width}}  {'median ms':>10}  {'min':>9}  {'max':>9}  {'share':>6}")
        for name in names:
            values = [dict(run["stages"])[name] for run in runs if name in dict(run["stages"])]
            share = "" if name == "total" else f"{100.0 * medians[name] / whole:5.1f}%"
            print(f"{name:<{width}}  {medians[name]:10.1f}  {min(values):9.1f}  {max(values):9.1f}  {share:>6}")
        if "total" in medians:
            print(f"{'(wall - total)':<{width}}  {statistics.median(walls) - medians['total']:10.1f}  "
                  f"{'':>9}  {'':>9}  process start and exit")
    elif not args.no_timing:
        print("\nno `SS_TIMING stage=<name> ms=<float>` lines on stderr")
    detail_names = []
    for run in runs:
        for name, _ in run["details"]:
            if name not in detail_names:
                detail_names.append(name)
    if detail_names:
        width = max(len(name) for name in detail_names + ["detail (not wall time)"])
        print(f"\n{'detail (not wall time)':<{width}}  {'median ms':>10}  {'min':>9}  {'max':>9}")
        for name in detail_names:
            values = [dict(run["details"])[name] for run in runs if name in dict(run["details"])]
            print(f"{name:<{width}}  {statistics.median(values):10.1f}  {min(values):9.1f}  {max(values):9.1f}")

    status = 0 if all(run["exit"] == 0 for run in runs) else 1
    if args.compare and runs and runs[-1]["exit"] == 0:
        compare = subprocess.run([sys.executable, str(REPO / "scripts" / "compare.py"), str(runs[-1]["out"]), str(Path(args.compare).resolve())],
                                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
        print("\ncompare.py (last run's output vs " + args.compare + "):")
        print(compare.stdout.strip())
        if compare.returncode != 0:
            status = 1
    if args.out:
        print(f"\noutputs kept in {base}")
    else:
        shutil.rmtree(base, ignore_errors=True)
    return status


if __name__ == "__main__":
    sys.exit(main())
