#!/usr/bin/env python3
"""Renders a bench.py result document as Markdown.

    report.py RESULT.json [--job NAME] [--correctness-json FILE] [--bench60m-json FILE]

The first line of every report is the exact commit SHA the numbers belong to. For bench-20m the
next thing is one compact line per candidate and seed (the bench/LEDGER.md format); the other
jobs' results fill its "fixtures + dev diff" and "60M result" cells when their JSON is given.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

GIB = 1024 ** 3
MIB = 1024 ** 2
TARGETS_S = (0.31, 0.20)  # the leaders' score, and the stretch target

LEDGER_COLUMNS = ["exact SHA", "baseline SHA", "host/limits", "data size, seed and bytes", "timer boundary/cache state",
                  "raw paired ms", "median paired ms saved", "A/A variation", "fixtures + dev diff", "60M result if run",
                  "next decision"]


def num(value):
    return "n/a" if value is None else f"{value:,}"


def secs(value):
    """A duration in the unit that reads best: ms below 10 s, seconds above."""
    if value is None:
        return "n/a"
    return f"{value * 1e3:,.1f} ms" if abs(value) < 10 else f"{value:,.2f} s"


def millis(value, signed=False):
    if value is None:
        return "n/a"
    return f"{value:+,.1f}" if signed else f"{value:,.1f}"


def size(value):
    if value is None:
        return "n/a"
    if value >= GIB:
        return f"{value / GIB:.2f} GiB"
    if value >= MIB:
        return f"{value / MIB:.1f} MiB"
    return f"{value:,} B"


def short(sha):
    return (sha or "")[:7]


def verdict(ok):
    return "PASS" if ok else "FAIL"


def _median(values):
    values = sorted(value for value in values if value is not None)
    if not values:
        return None
    middle = len(values) // 2
    return values[middle] if len(values) % 2 else (values[middle - 1] + values[middle]) / 2


def header(result):
    parts = [result.get("sha") or "(unknown commit)", result.get("job", ""), verdict(result.get("ok"))]
    if result.get("run_url"):
        attempt = result.get("run_attempt")
        parts.append(f"[Actions run]({result['run_url']})" + (f" attempt {attempt}" if attempt and attempt != "1" else ""))
    if result.get("utc"):
        parts.append(result["utc"])
    return " — ".join(parts)


def problems(result):
    lines = []
    for error in result.get("errors", []):
        lines.append(f"- **ERROR**: {error}")
    for warning in result.get("warnings", []):
        lines.append(f"- warning: {warning}")
    if result.get("skipped"):
        lines.append(f"- note: {result['skipped']}")
    return lines


# --------------------------------------------------------------------------
# shared pieces
# --------------------------------------------------------------------------

def host_text(result):
    hw = result.get("hardware") or {}
    limits = result.get("limits") or {}
    docker = hw.get("docker") or {}
    native = "native linux/amd64" if hw.get("arch", "x86_64") in ("x86_64", "AMD64") else f"EMULATED on {hw.get('arch')}"
    return (f"GitHub runner {hw.get('runner_image') or hw.get('os', '')}, {hw.get('cpu_model', 'n/a')}, {hw.get('nproc', '?')} vCPU, "
            f"{(hw.get('mem_total_kb') or 0) / 1048576:.1f} GiB RAM; `{limits.get('flags', '')}`; {native}; "
            f"Docker {docker.get('server_version', 'n/a')}")


def runner_table(result):
    hw = result.get("hardware") or {}
    docker = hw.get("docker") or {}
    data = hw.get("data_dir") or {}
    limits = result.get("limits") or {}
    parity = result.get("flag_parity") or {}
    present = set(hw.get("cpu_flags_present") or [])
    topology = ""
    if hw.get("sockets") and hw.get("cores_per_socket") and hw.get("threads_per_core"):
        topology = f" ({hw['sockets']} socket × {hw['cores_per_socket']} cores × {hw['threads_per_core']} threads)"
    caches = ", ".join(f"{name} {hw[key]}" for name, key in (("L1d", "cache_l1d"), ("L2", "cache_l2"), ("L3", "cache_l3")) if hw.get(key))
    memory = (f"{(hw.get('mem_total_kb') or 0) / 1048576:.1f} GiB RAM, {(hw.get('mem_available_kb') or 0) / 1048576:.1f} GiB available at start, "
              f"swap {(hw.get('swap_total_kb') or 0) / 1048576:.1f} GiB")
    rows = [
        ("CPU", f"{hw.get('cpu_model', 'n/a')} — {hw.get('nproc', 'n/a')} logical CPUs{topology}"
                + (f"; caches {caches}" if caches else "")
                + f"; AVX2 {'yes' if 'avx2' in present else 'no'}, AVX-512 {'yes' if 'avx512f' in present else 'no'}"
                + f"; {hw.get('arch', 'x86_64')}, no emulation"),
        ("Memory", memory),
        ("OS / kernel", f"{hw.get('os', 'n/a')}, kernel {hw.get('kernel', 'n/a')}"
                        + (f", runner image {hw['runner_image']}" if hw.get("runner_image") else "")),
        ("Docker", f"server {docker.get('server_version', 'n/a')}, client {docker.get('client_version', 'n/a')}, "
                   f"storage {docker.get('storage_driver', 'n/a')}, cgroup v{docker.get('cgroup_version', '?')} ({docker.get('cgroup_driver', '?')}), "
                   f"{docker.get('runc') or 'runc n/a'}"),
        ("Data directory", f"`{data.get('path', 'n/a')}` on {data.get('fstype') or 'n/a'} ({data.get('source') or 'n/a'}), "
                           f"{size(data.get('free_bytes'))} free at start"),
        ("Transparent huge pages", f"`{hw.get('thp_enabled', 'n/a')}`"),
    ]
    if limits:
        rows.append(("Container limits", f"`{limits.get('flags') or '--cpus %s --memory %sg' % (limits.get('cpus'), limits.get('memory_g'))}`"
                                         f" (the scorer: {limits.get('scorer', '--cpus 8 --memory 24g')})"))
    if parity:
        if parity.get("ok"):
            rows.append(("Docker flags", "identical to `scripts/docker_run.sh` (checked at run time), except `--name` instead of `--rm` "
                                         "so the exited container can be inspected, plus `--cpuset-cpus` when pinning"))
        else:
            rows.append(("Docker flags", f"**DIFFER from `scripts/docker_run.sh`**: {parity.get('detail', '')}"))
    lines = ["| | |", "|---|---|"]
    lines += [f"| {name} | {value} |" for name, value in rows]
    return lines


def data_text(entry):
    stats = entry.get("dataset") or {}
    events, clicks = entry.get("events") or 0, entry.get("clicks") or 0
    return (f"{events / 1e6:g}M events / {clicks / 1e6:g}M clicks, seed {entry.get('seed')}, "
            f"{num(stats.get('total_bytes'))} bytes in {num(stats.get('total_files'))} files")


def dataset_lines(entry):
    stats = entry.get("dataset") or {}
    folders = stats.get("folders") or {}
    generation = entry.get("generation") or {}
    lines = [f"`generate --seed {entry.get('seed')} --events {entry.get('events')} --clicks {entry.get('clicks')}` "
             f"({secs(generation.get('seconds'))} to generate)", "",
             "| folder | files | bytes | header-only files | largest file, bytes |", "|---|---:|---:|---:|---:|"]
    for name in ("events", "clicks", "links"):
        folder = folders.get(name) or {}
        lines.append(f"| `{name}/` | {num(folder.get('files'))} | {num(folder.get('bytes'))} | {num(folder.get('header_only_files'))} | "
                     f"{num(folder.get('largest_file_bytes'))} |")
    lines.append(f"| all files, with the lookups and the manifest | {num(stats.get('total_files'))} | {num(stats.get('total_bytes'))} | | |")
    manifest = stats.get("manifest") or {}
    lines += ["", f"Total input {size(stats.get('total_bytes'))}. Window {manifest.get('window_start', '?')} to {manifest.get('window_end', '?')}. "
                  f"Digest `{stats.get('digest', 'n/a')}` (every file's path, size and SHA-256)."]
    skew = entry.get("person_skew")
    if skew and "error" not in skew and skew.get("people"):
        shares = ", ".join(f"{share}%" for share in skew.get("top5_share_pct", []))
        lines.append(f"Person skew (surviving events per person, from `/reference`'s `user_daily`): {num(skew['people'])} people with a session, "
                     f"{num(skew.get('surviving_events'))} surviving events in {num(skew.get('sessions'))} sessions; per person median "
                     f"{skew.get('events_per_person_median'):g}, p99 {skew.get('events_per_person_p99'):g}, max {num(skew.get('events_per_person_max'))}; "
                     f"the five largest hold {shares}.")
    return lines


def cache_text(cache):
    if not cache:
        return "input page cache: state not recorded"
    before, after = cache.get("resident_pct_before"), cache.get("resident_pct_after")
    if before is None:
        return f"input page cache: {cache.get('condition', 'warm')} (read once beforehand; residency could not be measured)"
    return f"input page cache: {cache.get('condition', 'warm')}, {before}% of the input bytes resident before and {after}% after"


def cache_short(cache):
    if not cache or cache.get("resident_pct_before") is None:
        return "input page cache warm"
    return f"input page cache {cache.get('condition', 'warm')} ({cache['resident_pct_before']}% resident before, {cache.get('resident_pct_after')}% after)"


def series_rows(name, series, with_inner=False, with_split=False):
    """Table rows for one series of runs: the container's own time, the client wall, and extras."""
    runs = series.get("runs") or []
    cells = lambda key: " | ".join(secs(run.get(key)) for run in runs)  # noqa: E731
    rows = [f"| {name} | container: `State.StartedAt`→`FinishedAt` | {cells('docker_s')} | **{secs(series.get('median_docker_s'))}** |",
            f"| | Docker client wall | {cells('wall_s')} | {secs(series.get('median_wall_s'))} |"]
    if with_inner and any(run.get("diag") for run in runs):
        inner = " | ".join(f"{run['diag']['ms']:,.1f} ms" if run.get("diag") and "ms" in run["diag"] else "n/a" for run in runs)
        median = series.get("median_inprocess_ms")
        rows.append(f"| | in-process, start of `main` to done | {inner} | {'n/a' if median is None else f'{median:,.1f} ms'} |")
    if with_split:
        create = [run.get("create_s") for run in runs]
        start = [run.get("start_s") for run in runs]
        rows = [f"| {name} | `docker create` wall | {' | '.join(secs(value) for value in create)} | {secs(_median(create))} |",
                f"| | `docker start -a` wall | {' | '.join(secs(value) for value in start)} | {secs(_median(start))} |",
                f"| | container: `State.StartedAt`→`FinishedAt` | {cells('docker_s')} | {secs(series.get('median_docker_s'))} |"]
    failed = [run for run in runs if not run.get("ok")]
    if failed:
        rows.append(f"| | **failed runs** | {', '.join(str(run.get('label')) + ' exit ' + str(run.get('exit_code')) for run in failed)} | |")
    return rows


def run_columns(count):
    return " | ".join(f"run {index + 1}" for index in range(count)), "|".join(["---:"] * count)


def throughput_lines(entry):
    """What the targets demand of this input, next to what a bare read-through manages here."""
    total = (entry.get("dataset") or {}).get("total_bytes")
    if not total:
        return []
    gigabytes = total / 1e9
    wanted = " and ".join(f"**{gigabytes / target:.2f} GB/s** to finish in {target:.2f} s" for target in TARGETS_S)
    lines = [f"Required throughput: this input is {gigabytes:.3f} GB, so the whole timed interval has to average {wanted}."]
    diag = entry.get("diag") or {}
    measured = []
    for mode in ("mmap-scan", "read-scan"):
        series = (diag.get("readthrough") or {}).get(mode)
        if not series or not series.get("median_docker_s"):
            continue
        text = f"`{mode}` {gigabytes / series['median_docker_s']:.2f} GB/s by container time"
        if series.get("median_inprocess_ms"):
            text += f" ({gigabytes / (series['median_inprocess_ms'] / 1e3):.2f} GB/s in-process)"
        measured.append(text)
    if measured:
        threads = next((run["diag"].get("threads") for run in ((diag.get("readthrough") or {}).get("mmap-scan") or {}).get("runs", [])
                        if run.get("diag")), None)
        lines.append(f"Measured read-through on this runner ({threads} threads; {cache_short(diag.get('cache'))}): " + "; ".join(measured)
                     + ". That is only reading and counting newlines: the pipeline's parse, joins and output come on top.")
    return lines


MEMTOUCH_LABELS = {
    "4k": "plain 4 KiB pages, touched, left for the kernel to free at exit",
    "thp": "region 2 MiB-aligned, `madvise(MADV_HUGEPAGE)`, then touched",
    "4k-munmap": "4 KiB pages, touched, each thread `munmap`s its own part before exit",
    "populate": "`MAP_POPULATE` instead of touching",
}


def memtouch_lines(diag):
    """Fresh anonymous memory: what it costs to fault in and to tear down, both inside the timed interval."""
    variants = diag.get("memtouch") or {}
    if not variants:
        return []
    floor = (diag.get("noop") or {}).get("median_docker_s")
    sysinfo = {line.split(": ", 1)[0]: line.split(": ", 1)[1] for line in diag.get("sysinfo") or [] if ": " in line}
    first = next((run.get("diag") for series in variants.values() for run in series.get("runs") or [] if run.get("diag")), {}) or {}
    lines = [f"{first.get('mb', 1024)} MiB of fresh anonymous memory from {first.get('threads', '?')} threads, then exit; three fresh containers "
             "per variant. In-process time is start of `main` to the last touch (the page faults); the last column is what is left of the "
             f"container time after that and after the no-op floor ({secs(floor)}): the kernel freeing the memory at exit, plus noise.", "",
             "| variant | in-process ms (median) | container ms, `State.StartedAt`→`FinishedAt` (median) | Docker client wall ms (median) | "
             "container − in-process − no-op floor, ms | huge pages actually used |", "|---|---:|---:|---:|---:|---|"]
    for name, series in variants.items():
        runs = series.get("runs") or []
        inner = [run["diag"]["ms"] for run in runs if run.get("diag") and "ms" in run["diag"]]
        outer = [run["docker_s"] * 1e3 for run in runs if run.get("docker_s") is not None]
        wall = [run["wall_s"] * 1e3 for run in runs if run.get("wall_s") is not None]
        inner_median, outer_median = _median(inner), _median(outer)
        rest = None if None in (inner_median, outer_median, floor) else outer_median - inner_median - floor * 1e3
        huge = next((run["diag"].get("anon_huge_kb") for run in runs if run.get("diag")), None)
        huge_text = f"{huge:,} kB" if isinstance(huge, int) else ("not read" if huge in (None, "unread") else str(huge))
        cell = lambda values, middle: (", ".join(f"{value:,.1f}" for value in values) + f" (**{millis(middle)}**)") if values else "n/a"  # noqa: E731
        lines.append(f"| {MEMTOUCH_LABELS.get(name, name)} | {cell(inner, inner_median)} | {cell(outer, outer_median)} | "
                     f"{cell(wall, _median(wall))} | {millis(rest)} | {huge_text} |")
        failed = [run for run in runs if not run.get("ok")]
        if failed:
            lines.append(f"| | **failed**: {', '.join(str(run.get('label')) + ' exit ' + str(run.get('exit_code')) for run in failed)} | | | | |")
    settings = [f"`{key.split('.', 1)[1]}`: `{sysinfo[key]}`" for key in ("thp.enabled", "thp.defrag", "thp.shmem_enabled") if key in sysinfo]
    if settings:
        lines += ["", "Transparent huge pages as seen inside the container (`/sys/kernel/mm/transparent_hugepage/`): " + "; ".join(settings) + "."]
    return lines


WRITEOUT_LABELS = {
    "dirty": "written and left dirty in the page cache, as an ordinary write is",
    "fsync": "written and `fsync`ed before the process exits",
}


def gap_ms(run):
    if run.get("wall_s") is None or run.get("docker_s") is None:
        return None
    return (run["wall_s"] - run["docker_s"]) * 1e3


def writeout_lines(diag, pipeline=None):
    """Output bytes: what they cost inside the container's time, and after the process has exited."""
    variants = diag.get("writeout") or {}
    if not variants:
        return []
    noop_gap = _median([gap_ms(run) for run in (diag.get("noop") or {}).get("runs") or []])
    first = next((run.get("diag") for series in variants.values() for run in series.get("runs") or [] if run.get("diag")), {}) or {}
    lines = [f"{first.get('mb', 96)} MiB written under `/output`, then exit; three fresh containers per variant. The last column is the Docker "
             "client's wall time beyond the container's own time: what happens after the process has exited and before `docker run` returns. "
             f"For a no-op container that is {millis(noop_gap)} ms on this runner.", "",
             "| variant | in-process ms (median) | container ms, `State.StartedAt`→`FinishedAt` (median) | Docker client wall ms (median) | "
             "client wall − container, ms (median) |", "|---|---:|---:|---:|---:|"]
    cell = lambda values, middle: (", ".join(f"{value:,.1f}" for value in values) + f" (**{millis(middle)}**)") if values else "n/a"  # noqa: E731
    for name, series in variants.items():
        runs = series.get("runs") or []
        inner = [run["diag"]["ms"] for run in runs if run.get("diag") and "ms" in run["diag"]]
        outer = [run["docker_s"] * 1e3 for run in runs if run.get("docker_s") is not None]
        wall = [run["wall_s"] * 1e3 for run in runs if run.get("wall_s") is not None]
        gaps = [gap_ms(run) for run in runs if gap_ms(run) is not None]
        lines.append(f"| {WRITEOUT_LABELS.get(name, name)} | {cell(inner, _median(inner))} | {cell(outer, _median(outer))} | "
                     f"{cell(wall, _median(wall))} | {cell(gaps, _median(gaps))} |")
    if pipeline and pipeline.get("runs"):
        runs = pipeline["runs"]
        gaps = [gap_ms(run) for run in runs if gap_ms(run) is not None]
        produced = _median([run.get("output_bytes") for run in runs])
        lines += ["", f"The pipeline's own runs above wrote {size(produced)} each and show a client wall − container time of "
                      f"{', '.join(millis(value) for value in gaps)} ms (median {millis(_median(gaps))}). "
                      "The container's own time does not include it; a timer around `docker run` would."]
    return lines


def diag_lines(diag):
    lines = []
    noop = diag.get("noop") or {}
    count = len(noop.get("runs") or []) or 3
    columns, rule = run_columns(count)
    lines += [f"Each number is one fresh container with the scorer's flags; {cache_text(diag.get('cache'))}.", "",
              f"| diagnostic | measure | {columns} | median |", f"|---|---|{rule}|---:|"]
    lines += series_rows("(i) container start-up: `diag noop` (exits at once)", noop)
    if diag.get("noop_split"):
        lines += series_rows("(i) the same, as `docker create` then `docker start -a`", diag["noop_split"], with_split=True)
    for mode, series in (diag.get("readthrough") or {}).items():
        lines += series_rows(f"(ii) read-through `{mode}`", series, with_inner=True)
    first = next(iter((diag.get("readthrough") or {}).values()), None)
    info = (first or {}).get("runs") or []
    if info and info[0].get("diag"):
        detail = info[0]["diag"]
        lines += ["", f"Read-through: {num(detail.get('files'))} files, {num(detail.get('bytes'))} bytes, {num(detail.get('threads'))} threads "
                      f"(`NCPU`), {num(detail.get('chunk_kb'))} KiB chunks. `mmap-scan` maps each file and counts `\\n` over every byte; "
                      "`mmap-populate-scan` first faults each chunk in with `MADV_POPULATE_READ`; `mmap-touch` reads one byte per 4 KiB page "
                      "(page-fault cost only); `read-scan` is `pread` into a buffer plus the same count; `read` is `pread` alone."]
    if not diag.get("full", True):
        lines += ["", "The other diagnostics do not depend on the input and are reported with the first seed."]
    return lines


def stage_lines(run, title):
    stages = run.get("ss_timing") or []
    if not stages:
        return [f"{title}: the run printed no `SS_TIMING` lines."]
    total = next((value for name, value in stages if name == "total"), None)
    parts = [(name, value) for name, value in stages if name != "total"]
    denominator = total if total else sum(value for _, value in parts) or 1.0
    lines = [f"{title} (container {secs(run.get('docker_s'))}, client wall {secs(run.get('wall_s'))}):", "",
             "| stage | ms | share of total |", "|---|---:|---:|"]
    for name, value in parts:
        lines.append(f"| {name} | {value:,.1f} | {100.0 * value / denominator:.1f}% |")
    if total is not None:
        accounted = sum(value for _, value in parts)
        lines.append(f"| **total** (in-process) | **{total:,.1f}** | stages sum to {accounted:,.1f} |")
        if run.get("docker_s") is not None:
            lines.append(f"| container time minus in-process total | {run['docker_s'] * 1e3 - total:,.1f} | Docker's start, process start, exit and memory teardown |")
    details = run.get("ss_detail") or []
    if details:
        lines += ["", "`SS_TIMING detail=` lines of the same run (summed worker time or work that overlaps a stage: not wall-clock "
                      "stages, never added to the total):", "", "| detail | ms |", "|---|---:|"]
        lines += [f"| {name} | {value:,.1f} |" for name, value in details]
    return lines


def unseen_cost(run):
    """Container time minus the in-process total of an SS_TIMING run, in ms."""
    total = next((value for name, value in (run or {}).get("ss_timing") or [] if name == "total"), None)
    if total is None or (run or {}).get("docker_s") is None:
        return None
    return run["docker_s"] * 1e3 - total


def largest_stage(run):
    stages = [(name, value) for name, value in (run or {}).get("ss_timing") or [] if name != "total"]
    total = next((value for name, value in (run or {}).get("ss_timing") or [] if name == "total"), None)
    if not stages:
        return None
    name, value = max(stages, key=lambda item: item[1])
    whole = total or sum(item[1] for item in stages) or 1.0
    return f"{name} {value:,.0f} ms ({100.0 * value / whole:.0f}%)"


def monitor_peak(run):
    monitor = (run or {}).get("monitor") or {}
    return monitor.get("memory_peak_bytes") or monitor.get("max_memory_current_bytes")


def monitor_text(run):
    monitor = run.get("monitor") or {}
    if not monitor:
        return "not measured"
    if monitor_peak(run) is None:
        return "not captured (the container's cgroup was not found while it ran)"
    return (f"{size(monitor_peak(run))} (cgroup `memory.peak`, which also counts page cache charged to the container; sampled anonymous "
            f"memory peaked at {size(monitor.get('max_anon_bytes'))}, file cache at {size(monitor.get('max_file_bytes'))}; "
            f"{num(monitor.get('samples'))} samples)")


def compare_text(entry):
    comparisons = entry.get("compare") or []
    if not comparisons:
        return None
    new = [item for item in comparisons if item.get("which") == "new"]
    prior = [item for item in comparisons if item.get("which") == "prior"]
    text = f"{sum(1 for item in new if item.get('match'))}/{len(new)} runs of this commit **{'MATCH' if all(item.get('match') for item in new) else 'DIFFER'}**"
    if prior:
        text += f"; prior best {sum(1 for item in prior if item.get('match'))}/{len(prior)} {'MATCH' if all(item.get('match') for item in prior) else 'DIFFER'}"
    return text


def order_verdict(analysis):
    count, faster, typical = analysis.get("n") or 0, analysis.get("second_faster_pairs") or 0, analysis.get("median_first_minus_second_ms")
    if not count:
        return "n/a"
    middle = f"median first − second {millis(typical, signed=True)} ms"
    if faster == count:
        return f"**yes**: the second run of a pair was faster in all {count} pairs ({middle}), an order or cache bias"
    if faster == 0:
        return f"**yes**: the second run of a pair was slower in all {count} pairs ({middle}), an order bias"
    if faster >= count - 1 or faster <= 1:
        return f"possibly: the second run was faster in {faster} of {count} pairs ({middle})"
    return f"no: the second run was faster in {faster} of {count} pairs ({middle})"


def aa_summary(analysis):
    if not analysis:
        return "not run"
    signed = ", ".join(millis(row.get("a_minus_b_ms"), signed=True) for row in analysis.get("pairs") or [])
    return (f"median abs diff {millis(analysis.get('median_abs_diff_ms'))} ms, max {millis(analysis.get('max_abs_diff_ms'))} ms over "
            f"{analysis.get('n')} A/A pairs (signed A−B: {signed}); second run faster in {analysis.get('second_faster_pairs')}/{analysis.get('n')}")


def noise_judgement(saving, aa):
    if saving is None or not aa or aa.get("median_abs_diff_ms") is None:
        return "no A/A check to judge it against"
    if abs(saving) > (aa.get("max_abs_diff_ms") or 0):
        return "larger than every A/A difference seen here"
    if abs(saving) > aa["median_abs_diff_ms"]:
        return "larger than the median A/A difference but inside its range: treat as unconfirmed"
    return "within the A/A noise: no measurable difference"


def pair_table(analysis, a_name, b_name, difference_name):
    lines = [f"| pair | order | {a_name} ms | {b_name} ms | {difference_name} ms | first − second ms |", "|---:|---|---:|---:|---:|---:|"]
    for row in analysis.get("pairs") or []:
        lines.append(f"| {row.get('pair')} | {row.get('order')} | {millis(row.get('a_ms'))} | {millis(row.get('b_ms'))} | "
                     f"{millis(row.get('a_minus_b_ms'), signed=True)} | {millis(row.get('first_minus_second_ms'), signed=True)} |")
    return lines


def aa_lines(analysis):
    lines = [f"The same image under the labels A and B, {analysis.get('n')} pairs in the pre-shuffled order {' '.join(analysis.get('orders') or [])} "
             f"(balanced between AB and BA), each run a fresh container through the same timing path as a real comparison; "
             f"{cache_text(analysis.get('cache'))}. Times are the container's own (`State.StartedAt`→`FinishedAt`).", ""]
    lines += pair_table(analysis, "A", "B", "A − B")
    lines += ["", f"Signed median A − B {millis(analysis.get('median_a_minus_b_ms'), signed=True)} ms; **median absolute difference "
                  f"{millis(analysis.get('median_abs_diff_ms'))} ms**, largest {millis(analysis.get('max_abs_diff_ms'))} ms. "
                  f"Is the second run of a pair systematically faster? {order_verdict(analysis)}."]
    return lines


def ab_lines(analysis, aa, prior_sha, sha):
    saving = analysis.get("median_a_minus_b_ms")
    lines = [f"A is the prior best `{short(prior_sha)}`, B is this commit `{short(sha)}`; {analysis.get('n')} pairs in the pre-shuffled order "
             f"{' '.join(analysis.get('orders') or [])} on the same input, each run a fresh container; {cache_text(analysis.get('cache'))}.", ""]
    lines += pair_table(analysis, "prior", "this commit", "saved (prior − this)")
    lines += ["", f"Median per version: prior {millis(analysis.get('median_a_ms'))} ms, this commit {millis(analysis.get('median_b_ms'))} ms "
                  f"(Docker client wall {millis(analysis.get('median_a_wall_ms'))} and {millis(analysis.get('median_b_wall_ms'))} ms). "
                  f"**Median paired saving {millis(saving, signed=True)} ms**; this commit was faster in {analysis.get('b_faster_pairs')} of "
                  f"{analysis.get('n')} pairs. A/A noise on the same runner and input: {aa_summary(aa)}. The saving is {noise_judgement(saving, aa)}."]
    return lines


# --------------------------------------------------------------------------
# the compact line (bench/LEDGER.md)
# --------------------------------------------------------------------------

def correctness_text(correctness, entry=None):
    parts = []
    if correctness:
        for fixture in correctness.get("fixtures") or []:
            name = "fixtures" if fixture.get("set") == "fixtures" else "extra fixtures"
            parts.append(f"{fixture.get('passed')}/{fixture.get('total')} {name}")
        dev = correctness.get("dev")
        if dev:
            parts.append("dev " + ("MATCH" if (dev.get("compare") or {}).get("match") else "DIFFER"))
        if not parts:
            parts.append("correctness job " + verdict(correctness.get("ok")).lower())
    else:
        parts.append("fixtures and dev: see the correctness job of this run")
    if entry is not None:
        comparison = compare_text(entry)
        if comparison:
            parts.append(f"{(entry.get('events') or 0) // 1_000_000}M vs /reference: " + comparison.replace("**", ""))
    return "; ".join(parts)


def sixty_text(b60):
    if not b60:
        return "not run"
    entry = (b60.get("seeds") or [{}])[0]
    run = entry.get("pipeline_run")
    if not run:
        return "run failed before the pipeline started" if not b60.get("ok") else "not run"
    check = "MATCH /reference" if (entry.get("compare") and all(item.get("match") for item in entry["compare"])) else (
        "self-check OK" if (entry.get("self_check") or {}).get("ok") else "self-check FAILED")
    return (f"seed {entry.get('seed')}: container {secs(run.get('docker_s'))}, exit {run.get('exit_code')}, peak {size(monitor_peak(run))}, "
            f"{check}")


def compact_rows(result, correctness=None, b60=None):
    """One table row per seed, in the LEDGER.md column order."""
    rows = []
    prior = result.get("prior") or {}
    for entry in result.get("seeds") or []:
        pipeline = entry.get("pipeline") or {}
        runs = pipeline.get("runs") or []
        if not runs:
            continue
        container = ", ".join("n/a" if run.get("docker_s") is None else f"{run['docker_s'] * 1e3:.0f}" for run in runs)
        wall = ", ".join("n/a" if run.get("wall_s") is None else f"{run['wall_s'] * 1e3:.0f}" for run in runs)
        raw = f"{len(runs)} fresh runs, container ms: {container} (median {(pipeline.get('median_docker_s') or 0) * 1e3:.0f}); client wall ms: {wall}"
        ab, aa = entry.get("ab"), entry.get("aa")
        if ab:
            pairs = ", ".join(f"{millis(row.get('a_ms'))}/{millis(row.get('b_ms'))} ({row.get('order')})" for row in ab.get("pairs") or [])
            raw += f"; pairs prior/this: {pairs}"
            saved = (f"{millis(ab.get('median_a_minus_b_ms'), signed=True)} ms ({ab.get('b_faster_pairs')}/{ab.get('n')} pairs won; "
                     f"medians {millis(ab.get('median_a_ms'))} → {millis(ab.get('median_b_ms'))}); {noise_judgement(ab.get('median_a_minus_b_ms'), aa)}")
        elif prior.get("same_binary"):
            saved = "n/a: this commit's /pipeline is byte-identical to the baseline's"
        elif prior.get("status") == "built":
            saved = "n/a: the paired comparison did not run"
        else:
            saved = f"n/a: no baseline in this run (bench/BEST_SHA {prior.get('status', 'absent')})"
        baseline = prior.get("sha") if prior.get("status") in ("built",) else f"none ({prior.get('status', 'absent')})"
        hidden = unseen_cost(pipeline.get("timing_run"))
        facts = [fact for fact in (f"largest stage {largest_stage(pipeline.get('timing_run'))}" if largest_stage(pipeline.get("timing_run")) else None,
                                   f"container minus in-process total {millis(hidden)} ms" if hidden is not None else None,
                                   f"peak memory {size(monitor_peak(pipeline.get('memory_run')))}" if monitor_peak(pipeline.get("memory_run")) else None)
                 if fact]
        exact = result.get("sha") or "?"
        if result.get("source_sha") and result["source_sha"] != result.get("sha"):
            exact += f" (image built from the same Dockerfile, Cargo files and src/ as {result['source_sha']})"
        if result.get("run_url"):
            exact += f" [run {result['run_url'].rsplit('/', 1)[-1]}]({result['run_url']})"
        cells = [
            exact,
            baseline or "none",
            host_text(result),
            data_text(entry) + (" (holdout seed)" if entry.get("role") == "holdout" else ""),
            f"fresh container per run, `State.StartedAt`→`State.FinishedAt`; {cache_short(pipeline.get('cache'))}",
            raw,
            saved,
            aa_summary(aa) if aa else "not run" + (" (pair budget)" if (entry.get("pair_plan") or {}).get("requested") else ""),
            correctness_text(correctness, entry),
            sixty_text(b60),
            "(to decide)" + (" — " + "; ".join(facts) if facts else ""),
        ]
        rows.append("| " + " | ".join(str(cell).replace("|", "/").replace("\n", " ") for cell in cells) + " |")
    return rows


def compact_table(result, correctness=None, b60=None):
    rows = compact_rows(result, correctness, b60)
    if not rows:
        return []
    return ["| " + " | ".join(LEDGER_COLUMNS) + " |", "|" + "---|" * len(LEDGER_COLUMNS)] + rows


# --------------------------------------------------------------------------
# the three reports
# --------------------------------------------------------------------------

def render_bench20m(result, correctness=None, b60=None, with_line=True):
    lines = [header(result), ""]
    table = compact_table(result, correctness, b60) if with_line else []
    if table:
        lines += table + [""]
    lines += [f"## bench-20m — {verdict(result.get('ok'))}", ""]
    lines += problems(result)
    images = result.get("images") or {}
    prior = result.get("prior") or {}
    if result.get("mode") == "stand-in":
        lines += ["", "**Stand-in mode**: this commit has no root `Dockerfile`, so there are no pipeline numbers; only the diagnostics ran."]
    elif images.get("pipeline"):
        image = images["pipeline"]
        same = ""
        if result.get("source_sha") and result["source_sha"] != result.get("sha"):
            same = (f" This commit's `Dockerfile`, Cargo files and `src/` are identical to those of `{result['source_sha']}`, "
                    "where they were last changed, so this is that commit's image.")
        lines += ["", f"Pipeline image built from the root `Dockerfile` at this commit: `{(image.get('id') or '')[:19]}`, "
                      f"{size(image.get('size_bytes'))}, `/pipeline` sha256 `{(image.get('pipeline_sha256') or 'n/a')[:16]}`.{same} "
                      "**The headline number is the container's own run time, `State.StartedAt`→`State.FinishedAt` from `docker inspect` "
                      "on the exited container**; the Docker client's wall clock around `docker run` and the pipeline's internal stage "
                      "times are reported separately."]
    if prior.get("status") == "built":
        text = f"Prior best (`bench/BEST_SHA`): `{prior.get('sha')}`, built in this job"
        text += (", but its `/pipeline` is byte-identical to this commit's, so there is nothing to compare."
                 if prior.get("same_binary") else f", `/pipeline` sha256 `{((images.get('prior') or {}).get('pipeline_sha256') or 'n/a')[:16]}`.")
        lines += ["", text]
    elif prior.get("status") and prior["status"] != "absent":
        lines += ["", f"Prior best (`bench/BEST_SHA` = `{short(prior.get('sha'))}`): not timed, {prior['status']}."]
    if result.get("hardware"):
        lines += ["", "### Runner", ""] + runner_table(result)
    for entry in result.get("seeds") or []:
        seed = entry.get("seed")
        role = " (holdout: confirms a result, never used for tuning)" if entry.get("role") == "holdout" else ""
        lines += ["", f"### Seed {seed}{role}: input", ""] + dataset_lines(entry)
        throughput = throughput_lines(entry)
        if throughput:
            lines += [""] + throughput
        pipeline = entry.get("pipeline")
        if pipeline:
            runs = pipeline.get("runs") or []
            columns, rule = run_columns(len(runs))
            lines += ["", f"### Seed {seed}: pipeline", "",
                      f"{len(runs)} fresh containers with the scorer's flags, default command (`/pipeline`); {cache_text(pipeline.get('cache'))}. "
                      "Exit status and outputs are checked after each container has exited, outside both timed intervals.", "",
                      f"| image | measure | {columns} | median |", f"|---|---|{rule}|---:|"]
            lines += series_rows(f"this commit `{short(result.get('sha'))}`", pipeline)
            later = [run.get("docker_s") for run in runs[1:] if run.get("docker_s") is not None]
            if runs and runs[0].get("docker_s") is not None and later:
                lines += ["", f"First touch and warm separately: run 1 is the first time this image runs in the job (its binary has not been "
                              f"executed before; the input is already in the page cache), {secs(runs[0]['docker_s'])}; the later runs' median is "
                              f"{secs(_median(later))}. No number here is from a cold input cache except the one labelled so below."]
            noop = (entry.get("diag") or {}).get("noop") or {}
            if noop.get("median_wall_s") is not None and pipeline.get("median_wall_s") is not None:
                lines += ["", f"For scale: a no-op container on this runner costs {secs(noop.get('median_docker_s'))} of container time and "
                              f"{secs(noop.get('median_wall_s'))} of client wall, so that much of any score here is Docker; the pipeline's "
                              f"client wall exceeds its container time by "
                              f"{secs((pipeline.get('median_wall_s') or 0) - (pipeline.get('median_docker_s') or 0))}."]
            if entry.get("aa"):
                lines += ["", f"### Seed {seed}: A/A noise check", ""] + aa_lines(entry["aa"])
            if entry.get("ab"):
                lines += ["", f"### Seed {seed}: prior best versus this commit", ""] + ab_lines(entry["ab"], entry.get("aa"), prior.get("sha"), result.get("sha"))
            lines += ["", f"### Seed {seed}: stages, memory, correctness", ""]
            if pipeline.get("timing_run"):
                lines += stage_lines(pipeline["timing_run"], "Stage timings from one extra run with `SS_TIMING=1`")
            if (entry.get("prior_timing_run") or {}).get("ss_timing"):
                lines += ["", "<details><summary>Stage timings of the prior best</summary>", ""]
                lines += stage_lines(entry["prior_timing_run"], "Prior best, one extra run with `SS_TIMING=1`") + ["", "</details>"]
            memory = pipeline.get("memory_run")
            if memory:
                outputs = memory.get("outputs") or {}
                lines += ["", f"Peak memory, one extra run (container {secs(memory.get('docker_s'))}): {monitor_text(memory)}. "
                              f"Output: `user_daily` {size((outputs.get('user_daily') or {}).get('bytes'))} in "
                              f"{(outputs.get('user_daily') or {}).get('files')} file(s), `campaign_daily` "
                              f"{size((outputs.get('campaign_daily') or {}).get('bytes'))} in {(outputs.get('campaign_daily') or {}).get('files')} file(s); "
                              f"scratch peak {size((memory.get('monitor') or {}).get('peak_scratch_bytes'))}."]
            reference = entry.get("reference") or {}
            comparison = compare_text(entry)
            if comparison:
                by_bytes = sum(1 for item in entry.get("compare") or [] if str(item.get("how", "")).startswith("same bytes"))
                lines += ["", f"Correctness on this input: `/reference` (exit {reference.get('exit_code')}, container {secs(reference.get('docker_s'))}) "
                              f"against both tables of every run: {comparison}"
                              + (f" ({len(entry['compare']) - by_bytes} by `scripts/compare.py`, {by_bytes} by having the very same bytes as a compared output)."
                                 if by_bytes else " (each by `scripts/compare.py`).")]
                for item in [item for item in entry.get("compare") or [] if not item.get("match")][:3]:
                    lines += ["", f"`{item.get('run')}`:", "```", item.get("output", ""), "```"]
            elif reference:
                lines += ["", f"Correctness on this input: **not established** — `/reference` exited {reference.get('exit_code')}."]
            cold = pipeline.get("cold_run")
            if cold:
                cache = cold.get("cache") or {}
                lines += ["", f"Cold page cache, one labelled run (not in any median; `sync; echo 3 > /proc/sys/vm/drop_caches` just before, "
                              f"{cache.get('resident_pct_before')}% of the input resident before and {cache.get('resident_pct_after')}% after): "
                              f"container {secs(cold.get('docker_s'))}, client wall {secs(cold.get('wall_s'))}, exit {cold.get('exit_code')}. "
                              "This reflects this runner's disk, not the scorer's."]
        if entry.get("diag"):
            lines += ["", f"### Seed {seed}: diagnostics", ""] + diag_lines(entry["diag"])
            memory_lines = memtouch_lines(entry["diag"])
            if memory_lines:
                lines += ["", f"### Seed {seed}: anonymous memory, fault-in and exit teardown", ""] + memory_lines
            output_lines = writeout_lines(entry["diag"], pipeline)
            if output_lines:
                lines += ["", f"### Seed {seed}: output bytes, during and after the run", ""] + output_lines
            sysinfo = entry["diag"].get("sysinfo")
            if sysinfo:
                lines += ["", "<details><summary>What a process sees inside the container (<code>diag sysinfo</code>, scorer flags)</summary>", "",
                          "```"] + sysinfo + ["```", "", "</details>"]
    return "\n".join(lines) + "\n"


def render_bench60m(result):
    lines = [header(result), "", f"## bench-60m — {verdict(result.get('ok'))}", ""]
    lines += problems(result)
    if result.get("hardware"):
        lines += ["", "### Runner", ""] + runner_table(result)
    for entry in result.get("seeds") or []:
        lines += ["", f"### Seed {entry.get('seed')}: input", ""] + dataset_lines(entry)
        run = entry.get("pipeline_run")
        if not run:
            continue
        monitor = run.get("monitor") or {}
        outputs = run.get("outputs") or {}
        limit = (result.get("limits") or {}).get("memory_g")
        check = entry.get("self_check") or {}
        rows = [
            ("Exit code", f"**{run.get('exit_code')}**" + (" (OOM-killed)" if run.get("oom_killed") else "")),
            ("Container time, `State.StartedAt`→`FinishedAt`", f"**{secs(run.get('docker_s'))}** (hard cap 180 s)"),
            ("Docker client wall", f"{secs(run.get('wall_s'))} (`docker create` {secs(run.get('create_s'))} + `docker start -a` {secs(run.get('start_s'))})"),
            ("Peak memory", f"{monitor_text(run)}; limit {limit} GiB here, 24 GiB on the scorer"),
            ("Output", f"`user_daily` {size((outputs.get('user_daily') or {}).get('bytes'))} in {(outputs.get('user_daily') or {}).get('files')} file(s), "
                       f"`campaign_daily` {size((outputs.get('campaign_daily') or {}).get('bytes'))} in {(outputs.get('campaign_daily') or {}).get('files')} file(s)"),
            ("Scratch use", f"peak {size(monitor.get('peak_scratch_bytes'))} (sampled every 100 ms), {size(run.get('scratch_bytes_at_exit'))} left at exit"),
            ("Cache condition", cache_text(run.get("cache"))),
            ("Disk free", f"{size((result.get('disk_before') or {}).get('free_bytes'))} before generating, "
                          f"{size((result.get('disk_after_generation') or {}).get('free_bytes'))} after, "
                          f"{size((result.get('disk_after_runs') or {}).get('free_bytes'))} after the runs"),
        ]
        if check:
            if "error" in check:
                rows.append(("Rules' self-check", f"**FAILED**: {check['error']}"))
            else:
                rows.append(("Rules' self-check", f"{'OK' if check.get('ok') else '**FAILED**'}: sessions {num(check.get('user_sessions'))} = "
                                                  f"{num(check.get('campaign_sessions'))}, revenue {num(check.get('user_revenue_cents'))} = "
                                                  f"{num(check.get('campaign_revenue_cents'))}, assisted {num(check.get('assisted_sessions'))} ≥ credited "
                                                  f"non-organic {num(check.get('credited_non_organic_sessions'))}; rows {num(check.get('user_daily_rows'))} + "
                                                  f"{num(check.get('campaign_daily_rows'))}"))
        reference = entry.get("reference")
        if reference:
            if reference.get("ok"):
                rows.append(("`/reference` on the same input", f"exit 0, container {secs(reference.get('docker_s'))}, peak memory {monitor_text(reference)}; "
                                                               f"`scripts/compare.py` on both tables: {compare_text(entry)}"))
            else:
                rows.append(("`/reference` on the same input", f"did not finish on this runner: exit {reference.get('exit_code')}"
                                                               + (" (OOM-killed)" if reference.get("oom_killed") else "")
                                                               + f", peak memory {monitor_text(reference)}. Correctness rests on the self-check above."))
            if reference.get("limits"):
                rows.append(("`/reference` memory limit", f"`{reference['limits']}`: all this runner has. It is the correctness tool, not a "
                                                          "scored run, so its limit is not the pipeline's."))
        lines += ["", f"### Seed {entry.get('seed')}: one pipeline run with the scorer's flags", "", "| | |", "|---|---|"]
        lines += [f"| {name} | {value} |" for name, value in rows]
        for item in [item for item in entry.get("compare") or [] if not item.get("match")][:2]:
            lines += ["", "```", item.get("output", ""), "```"]
        staged = entry.get("timing_run") or run
        if staged.get("ss_timing"):
            lines += [""] + stage_lines(staged, "Stage timings from one extra run with `SS_TIMING=1`")
    return "\n".join(lines) + "\n"


def render_correctness(result):
    lines = [header(result), "", f"## correctness — {verdict(result.get('ok'))}", ""]
    lines += problems(result)
    image = result.get("image")
    if image:
        lines += ["", f"Image: root `Dockerfile` at this commit, `docker buildx build --platform linux/amd64 --load`, id `{(image.get('id') or '')[:19]}`, "
                      f"{size(image.get('size_bytes'))}. The organizers' scripts ran unmodified with `{result.get('script_env', '')}` "
                      f"(the runner has {(result.get('hardware') or {}).get('nproc')} CPUs; the scorer uses 8 and 24g)."]
    for entry in result.get("fixtures") or []:
        mark = "**passed**" if entry.get("ok") else "**FAILED**"
        lines += ["", f"- `{entry.get('command')}`: {entry.get('passed')}/{entry.get('total')} {mark} (exit {entry.get('exit_code')}, {entry.get('seconds')} s)"]
        for failure in entry.get("failures") or []:
            lines.append(f"  - `{failure[:300]}`")
    if result.get("fixtures") is not None and not any(entry.get("set") == "extra_fixtures" for entry in result.get("fixtures") or []):
        lines.append("- `extra_fixtures/`: not present at this commit")
    dev = result.get("dev")
    if dev:
        stats = dev.get("dataset") or {}
        folders = stats.get("folders") or {}
        described = ", ".join(f"`{name}/` {num((folders.get(name) or {}).get('files'))} files / {num((folders.get(name) or {}).get('bytes'))} bytes"
                              for name in ("events", "clicks", "links"))
        compare = dev.get("compare") or {}
        lines += ["", f"- Dev set, `generate --seed 1 --events 1000000 --clicks 250000`: {described}; digest `{stats.get('digest', 'n/a')}`.",
                  f"- `{dev.get('run_command')}`: exit {dev.get('run_exit_code')} (the whole script took {dev.get('run_script_seconds')} s, "
                  "which includes Docker's set-up and the script's clean-up container; it is not a benchmark number).",
                  f"- `{dev.get('compare_command')}`: **{'MATCH' if compare.get('match') else 'DIFFER'}**", "", "```", compare.get("output", ""), "```"]
    if result.get("hardware"):
        lines += ["", "<details><summary>Runner</summary>", ""] + runner_table(result) + ["", "</details>"]
    return "\n".join(lines) + "\n"


def render(result, correctness=None, b60=None, with_line=True):
    job = result.get("job")
    if job == "bench-20m":
        return render_bench20m(result, correctness, b60, with_line)
    if job == "bench-60m":
        return render_bench60m(result)
    if job == "correctness":
        return render_correctness(result)
    return header(result) + "\n\n" + "\n".join(problems(result)) + "\n"


def load(path):
    if not path:
        return None
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return None


def stub(job, path, error):
    """A failure report for a job that stopped before bench.py wrote its results (a failed build, say)."""
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    repository, run_id = os.environ.get("GITHUB_REPOSITORY", ""), os.environ.get("GITHUB_RUN_ID", "")
    result = {
        "schema": 1,
        "job": job,
        "sha": os.environ.get("BENCH_SHA") or os.environ.get("GITHUB_SHA", ""),
        "run_url": f"{server}/{repository}/actions/runs/{run_id}" if repository and run_id else "",
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", ""),
        "ok": False,
        "errors": [f"the job stopped before the benchmark driver wrote its results ({error}); an earlier step failed, see the job log"],
        "warnings": [],
    }
    try:
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(result, handle, indent=1)
    except OSError:
        pass
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("json")
    parser.add_argument("--job", default="unknown", help="job name for the failure report when the result file is missing")
    parser.add_argument("--correctness-json", help="the correctness job's result, for the compact line")
    parser.add_argument("--bench60m-json", help="the bench-60m job's result, for the compact line")
    parser.add_argument("--line-only", action="store_true", help="print only the compact table (header and rows)")
    args = parser.parse_args()
    result = load(args.json)
    if result is None:
        result = stub(args.job, args.json, "no result file")
    correctness, b60 = load(args.correctness_json), load(args.bench60m_json)
    sys.stdout.reconfigure(encoding="utf-8")
    if args.line_only:
        sys.stdout.write("\n".join(compact_table(result, correctness, b60)) + "\n")
    else:
        sys.stdout.write(render(result, correctness, b60))


if __name__ == "__main__":
    main()
