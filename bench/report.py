#!/usr/bin/env python3
"""Renders a bench.py result document as Markdown. Usage: report.py RESULT.json [> report.md]

The first line of every report is the exact commit SHA the numbers belong to.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

GIB = 1024 ** 3
MIB = 1024 ** 2


def num(value):
    return "n/a" if value is None else f"{value:,}"


def secs(value):
    """A duration in the unit that reads best: ms below 10 s, seconds above."""
    if value is None:
        return "n/a"
    return f"{value * 1e3:,.1f} ms" if abs(value) < 10 else f"{value:,.2f} s"


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
                + f"; AVX2 {'yes' if 'avx2' in present else 'no'}, AVX-512 {'yes' if 'avx512f' in present else 'no'}"),
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


def _median(values):
    values = sorted(value for value in values if value is not None)
    if not values:
        return None
    middle = len(values) // 2
    return values[middle] if len(values) % 2 else (values[middle - 1] + values[middle]) / 2


def run_columns(count):
    return " | ".join(f"run {index + 1}" for index in range(count)), "|".join(["---:"] * count)


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
    return lines


def stage_lines(run, title):
    stages = run.get("ss_timing") or []
    lines = []
    if not stages:
        return [f"{title}: the run printed no `SS_TIMING` lines."]
    total = next((value for name, value in stages if name == "total"), None)
    parts = [(name, value) for name, value in stages if name != "total"]
    denominator = total if total else sum(value for _, value in parts) or 1.0
    lines += [f"{title} (container {secs(run.get('docker_s'))}, client wall {secs(run.get('wall_s'))}):", "",
              "| stage | ms | share of total |", "|---|---:|---:|"]
    for name, value in parts:
        lines.append(f"| {name} | {value:,.1f} | {100.0 * value / denominator:.1f}% |")
    if total is not None:
        accounted = sum(value for _, value in parts)
        lines.append(f"| **total** (in-process) | **{total:,.1f}** | stages sum to {accounted:,.1f} |")
        if run.get("docker_s") is not None:
            lines.append(f"| container time minus in-process total | {run['docker_s'] * 1e3 - total:,.1f} | process start, exit and teardown |")
    return lines


def monitor_text(run):
    monitor = run.get("monitor") or {}
    if not monitor:
        return "not measured"
    if monitor.get("memory_peak_bytes") is None and monitor.get("max_memory_current_bytes") is None:
        return "not captured (the container's cgroup was not found while it ran)"
    peak = monitor.get("memory_peak_bytes") or monitor.get("max_memory_current_bytes")
    return (f"{size(peak)} (cgroup `memory.peak`, which also counts page cache charged to the container; sampled anonymous "
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


def delta(old, new):
    if old is None or new is None or old == 0:
        return "n/a"
    return f"{secs(old)} → {secs(new)} ({(new - old) / old * 100:+.1f}%)"


def ledger_row(result, entry):
    pipeline = entry.get("pipeline") or {}
    runs = pipeline.get("runs") or []
    samples = ", ".join(f"{run['docker_s'] * 1e3:.0f}" if run.get("docker_s") is not None else "n/a" for run in runs)
    walls = ", ".join(f"{run['wall_s'] * 1e3:.0f}" if run.get("wall_s") is not None else "n/a" for run in runs)
    monitor = (pipeline.get("memory_run") or {}).get("monitor") or {}
    peak = monitor.get("memory_peak_bytes") or monitor.get("max_memory_current_bytes")
    comparison = compare_text(entry) or "not compared"
    stages = (pipeline.get("timing_run") or {}).get("ss_timing") or []
    parts = [(name, value) for name, value in stages if name != "total"]
    biggest = max(parts, key=lambda item: item[1])[0] if parts else "?"
    events = entry.get("events") or 0
    hw = result.get("hardware") or {}
    return (f"| `{short(result.get('sha'))}` | (hypothesis) | (stage; largest now: {biggest}) | {events // 1_000_000}M seed {entry.get('seed')}, "
            f"{hw.get('nproc')} vCPU {hw.get('cpu_model', '')} | container ms: {samples}; client wall ms: {walls} | "
            f"{secs(pipeline.get('median_docker_s'))} | {size(peak)} | {comparison.replace('**', '')} | (keep/drop) | (next) |")


def render_bench20m(result):
    lines = [header(result), "", f"## bench-20m — {verdict(result.get('ok'))}", ""]
    lines += problems(result)
    images = result.get("images") or {}
    if result.get("mode") == "stand-in":
        lines += ["", "**Stand-in mode**: this commit has no root `Dockerfile`, so there are no pipeline numbers; only the diagnostics ran."]
    elif images.get("pipeline"):
        lines += ["", f"Pipeline image built from the root `Dockerfile` at this commit: `{(images['pipeline'].get('id') or '')[:19]}`, "
                      f"{size(images['pipeline'].get('size_bytes'))}. **The headline number is the container's own run time, "
                      "`State.StartedAt`→`State.FinishedAt` from `docker inspect` on the exited container**; the Docker client's wall "
                      "clock around `docker run` and the pipeline's internal stage times are reported separately."]
    prior = result.get("prior") or {}
    if prior.get("status") and prior["status"] not in ("absent", "built"):
        lines += ["", f"Prior best (`bench/BEST_SHA` = `{short(prior.get('sha'))}`): not timed, {prior['status']}."]
    if result.get("hardware"):
        lines += ["", "### Runner", ""] + runner_table(result)
    for entry in result.get("seeds") or []:
        lines += ["", f"### Seed {entry.get('seed')}: input", ""] + dataset_lines(entry)
        if entry.get("diag"):
            lines += ["", f"### Seed {entry.get('seed')}: diagnostics", ""] + diag_lines(entry["diag"])
        pipeline = entry.get("pipeline")
        if not pipeline:
            continue
        runs = pipeline.get("runs") or []
        columns, rule = run_columns(len(runs))
        lines += ["", f"### Seed {entry.get('seed')}: pipeline", "",
                  f"{len(runs)} fresh containers with the scorer's flags, default command (`/pipeline`); {cache_text(pipeline.get('cache'))}. "
                  "Exit status and outputs are checked after each container has exited, outside both timed intervals.", "",
                  f"| image | measure | {columns} | median |", f"|---|---|{rule}|---:|"]
        lines += series_rows(f"this commit `{short(result.get('sha'))}`", pipeline)
        before = entry.get("prior")
        if before:
            lines += series_rows(f"prior best `{short(prior.get('sha'))}`", before)
            lines += ["", "Run order (balanced, alternating which image goes first): " + ", ".join(
                          f"{'this' if which == 'new' else 'prior'}" for which in pipeline.get("order") or []) + ".",
                      "", f"**Prior median vs new median** — container time: {delta(before.get('median_docker_s'), pipeline.get('median_docker_s'))}; "
                          f"Docker client wall: {delta(before.get('median_wall_s'), pipeline.get('median_wall_s'))}."]
        noop = (entry.get("diag") or {}).get("noop") or {}
        if noop.get("median_wall_s") is not None and pipeline.get("median_wall_s") is not None:
            lines += ["", f"For scale: a no-op container on this runner costs {secs(noop.get('median_wall_s'))} of client wall and "
                          f"{secs(noop.get('median_docker_s'))} of container time; the pipeline's client wall exceeds its container time by "
                          f"{secs((pipeline.get('median_wall_s') or 0) - (pipeline.get('median_docker_s') or 0))}."]
        if pipeline.get("timing_run"):
            lines += [""] + stage_lines(pipeline["timing_run"], "Stage timings from one extra run with `SS_TIMING=1`")
        if before and before.get("timing_run") and before["timing_run"].get("ss_timing"):
            lines += ["", "<details><summary>Stage timings of the prior best</summary>", ""]
            lines += stage_lines(before["timing_run"], "Prior best, one extra run with `SS_TIMING=1`") + ["", "</details>"]
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
            lines += ["", f"Correctness on this input: `/reference` (exit {reference.get('exit_code')}, container {secs(reference.get('docker_s'))}) "
                          f"against both tables of every run with `scripts/compare.py`: {comparison}."]
            failed = [item for item in entry.get("compare") or [] if not item.get("match")]
            for item in failed[:3]:
                lines += ["", f"`{item.get('run')}`:", "```", item.get("output", ""), "```"]
        elif reference:
            lines += ["", f"Correctness on this input: **not established** — `/reference` exited {reference.get('exit_code')}."]
        cold = pipeline.get("cold_run")
        if cold:
            cache = cold.get("cache") or {}
            lines += ["", f"Cold page cache, one labelled run (not in the medians; `sync; echo 3 > /proc/sys/vm/drop_caches` just before, "
                          f"{cache.get('resident_pct_before')}% of the input resident before and {cache.get('resident_pct_after')}% after): "
                          f"container {secs(cold.get('docker_s'))}, client wall {secs(cold.get('wall_s'))}, exit {cold.get('exit_code')}. "
                          "This reflects this runner's disk, not the scorer's."]
        lines += ["", "Ledger row for `bench/LEDGER.md`:", "", "```", ledger_row(result, entry), "```"]
        sysinfo = (entry.get("diag") or {}).get("sysinfo")
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
        lines += ["", f"### Seed {entry.get('seed')}: one pipeline run with the scorer's flags", "", "| | |", "|---|---|"]
        lines += [f"| {name} | {value} |" for name, value in rows]
        for item in [item for item in entry.get("compare") or [] if not item.get("match")][:2]:
            lines += ["", "```", item.get("output", ""), "```"]
        if run.get("ss_timing"):
            lines += [""] + stage_lines(run, "Stage timings")
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


def render(result):
    job = result.get("job")
    if job == "bench-20m":
        return render_bench20m(result)
    if job == "bench-60m":
        return render_bench60m(result)
    if job == "correctness":
        return render_correctness(result)
    return header(result) + "\n\n" + "\n".join(problems(result)) + "\n"


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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("json")
    parser.add_argument("--job", default="unknown", help="job name for the failure report when the result file is missing")
    args = parser.parse_args()
    try:
        with open(args.json, encoding="utf-8") as handle:
            result = json.load(handle)
    except (OSError, ValueError) as error:
        result = stub(args.job, args.json, type(error).__name__)
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stdout.write(render(result))


if __name__ == "__main__":
    main()
