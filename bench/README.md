# Benchmark harness

How this repository measures the pipeline, so that a number in a report can be traced to exactly
what was timed. The workflow is `.github/workflows/ci.yml`; everything it calls lives here.

Every measurement runs on a GitHub-hosted `ubuntu-latest` runner: 4 vCPUs and 15.6 GiB RAM, but
not one machine type. Jobs so far landed on an AMD EPYC 7763 (no AVX-512), an AMD EPYC 9V45 and an
Intel Xeon 6975P-C (both with AVX-512), with different disks. So absolute times are comparable
only within one job, which is why every comparison is paired inside a job and every report names
its CPU. The scorer is yet another machine (8 pinned hyperthreads of an Intel Xeon with AVX-512,
24 GB). These numbers rank candidates; they do not predict the score.

## What is timed

The unit is **one fresh container**: `docker run` of an image, with the scorer's flags, used once.

| Number | Boundary | Taken with |
| --- | --- | --- |
| **Container time** (the headline) | `State.StartedAt` → `State.FinishedAt` of that container | `docker inspect` on the exited container; it is started with `--name` instead of `--rm`, inspected, then removed |
| Docker client wall | from just before the `docker run` process is started until it returns | Python's `perf_counter_ns` around the subprocess |
| In-process stages | `SS_TIMING stage=<name> ms=<float>` lines on stderr | one **extra** run with `SS_TIMING=1`; never one of the timed samples |

Both timed intervals contain only the container. Creating the output and scratch folders, a `sync`
before each run, `docker inspect`, `docker rm`, reading the exit status, reading the outputs and
comparing them all happen outside. The organizers' `scripts/docker_run.sh` is never timed for a
benchmark number: it may start a second, clean-up container after the scored one.

Container time is more than the pipeline's own work. It also holds Docker's start of the
container (measured below with a binary that exits at once: 55 to 78 ms on these runners) and the
kernel freeing the process's memory at exit, which `SS_TIMING` cannot see. The report shows
"container time minus in-process total" for the stage run so this share stays visible.

The client wall holds more again: whatever Docker does after the process has exited and before
`docker run` returns. That grows with the bytes the run wrote to `/output` (on one runner type,
about half a second for 98 MB), and it is why the two numbers are never merged. Which of the two
the official timer resembles is not known.

`SS_TIMING detail=<name> ms=<float>` lines (summed worker time, or work that overlaps a stage) are
listed apart from the stages and never added to them.

## The flags

The harness passes exactly the flags of the scored `docker run` in `scripts/docker_run.sh`. It
does not keep a copy to trust: on every run it parses that script and compares token by token
(`python3 bench/bench.py parity`), and the job fails if they differ. Two deliberate differences:
`--name` replaces `--rm`, and `--cpuset-cpus` is added.

- `--cpus` is `min(8, CPUs available)`; `--memory` and `--memory-swap` are `min(24g, RAM minus
  2 GiB, in whole GiB)`. On the 16 GB runner that is `--cpus 4 --memory 13g`. `NCPU` and `MEM_GB`
  follow, as in the script.
- `--cpus` is a quota, not pinning, so the container is also pinned with `--cpuset-cpus` to whole
  cores (every hyperthread of each core, lowest IDs first): `0-3` on the runner. If Docker refuses
  the flag, the run continues without it and the report says so.

## Cache state

Every number states its cache condition.

- **Warm** (all numbers unless labelled otherwise): after the generator has written the input,
  every file is read once. Before and after each series the harness asks the kernel, with
  `mincore` over every input file, how much of the input is resident in the page cache, and prints
  that percentage next to the numbers. A fresh container does not mean a cold cache.
- **First run**: the first of the three headline runs is the first time the image's binary runs
  in the job. It is reported next to the median of the later runs.
- **Cold**: one extra run after `sync; echo 3 > /proc/sys/vm/drop_caches`, labelled as such and
  never mixed into a median. This is only ever done on the job's own throwaway VM; never drop the
  caches of a machine other people use. It measures the runner's disk, not the scorer's.

## bench-20m, step by step

For each seed in the seed list (practice scale, `--events 20000000 --clicks 5000000`):

1. **Input.** `generate --seed N ...`; actual bytes and file counts per folder; one digest over
   every file's path, size and SHA-256, so two runs (or CI and a local copy) can be shown to use
   the same bytes. Then the warm-up read.
2. **Diagnostics**, each three fresh containers of `bench/diag` with the scorer's flags:
   - *(i) container start-up*: `diag noop`, a static binary that exits at once. Also as
     `docker create` followed by `docker start -a`, timed separately.
   - *(ii) read-through*: every byte under `/data` with `NCPU` threads. `mmap-scan` maps each file
     and counts newlines over every byte; `mmap-populate-scan` adds `MADV_POPULATE_READ`;
     `mmap-touch` reads one byte per 4 KiB page; `read-scan` is `pread` into a buffer plus the same
     count; `read` is `pread` alone. Reported as container time, client wall and in-process time,
     and as GB/s next to what the 0.31 s and 0.20 s targets require for this input.
   - *anonymous memory*: 1 GiB of fresh memory touched from `NCPU` threads, then exit: plain 4 KiB
     pages; 2 MiB-aligned with `MADV_HUGEPAGE`; with each thread unmapping its part before exit;
     with `MAP_POPULATE` instead of touching. In-process time is the fault-in cost; container time
     minus in-process time minus the no-op floor is the teardown at exit.
   - *output bytes*: 96 MiB written under `/output`, then exit, once left dirty in the page cache
     and once `fsync`ed first. Client wall minus container time is what the bytes cost after the
     process has exited; the `fsync` variant prices the same bytes inside the process.
   - `diag sysinfo`: CPUs, cgroup limits, huge-page settings and mounts as seen from inside.

   The first seed gets all of them; later seeds only the no-op and `mmap-scan`, which depend on
   the input or set the floor.
3. **Headline**: three fresh containers of this commit's image, default command. The three times
   and their median, as the scorer does for runs under 20 s.
4. **A/A noise check**: the same image under the labels A and B, six pairs, in a pre-shuffled
   order balanced between AB and BA (three each; the shuffle is seeded by the dataset seed, so it
   is the same every time). Reported: each signed difference A − B, the median absolute
   difference, and whether the second run of a pair is systematically faster.
5. **Prior best versus this commit**, when `bench/BEST_SHA` names another commit: its image is
   built in the same job and the two are run as six shuffled AB/BA pairs exactly like the A/A
   check (A is the prior). Reported: median per version, each paired saving (prior − this), the
   median paired saving, how many pairs this commit won, and the A/A variation to read it against.
   If both images hold a byte-identical `/pipeline`, there is nothing to compare and it says so.
6. **Stages**: one extra run with `SS_TIMING=1` (also of the prior best).
7. **Memory**: one extra run as `docker create` + `docker start -a` while a thread samples the
   container's cgroup every 5 ms: `memory.peak` (which counts page cache charged to the
   container), anonymous and file memory, and the size of the output and scratch folders.
8. **Correctness on this input**: `/reference` from the same image with the same flags, then both
   tables of **every** run above against it. Outputs with a byte content not seen before go
   through `scripts/compare.py`; an output with the very same bytes as a compared one shares its
   verdict. Any `DIFFER` from this commit, or a failing `/reference`, fails the job: a time for a
   wrong answer is void. The same output gives the person skew (surviving events per person).
9. **Cold run**, as above.

Pairs are sized to a time budget (300 s per seed by default). A pipeline too slow for six pairs
of both comparisons gets four; below that the A/A check is dropped first and the real comparison
keeps at least two pairs. The report says when this happened. Failed and slow runs stay in the
raw data.

The median of three is the middle value. Paired statistics use the container time; the client
wall is kept for every sample.

## Seeds, and the best commit so far

- `bench/SEEDS` lists the seeds. The first is the tuning seed. Any later one is a **holdout**: it
  confirms a result on input that never guided a decision, so do not choose between ideas on it.
  Override for one run with the `seeds` input, or `[seeds=2,7]` in the commit message.
- `bench/BEST_SHA` names the commit to beat. Move it only after a paired saving is confirmed.
- Every sample in the JSON carries its seed.

## The other jobs

- **correctness**: builds the root `Dockerfile` (`docker buildx build --platform linux/amd64
  --load`), then runs the organizers' commands unmodified: `python3 scripts/run_fixtures.py
  fixtures -- scripts/docker_run.sh` (36 cases), the same for `extra_fixtures/` when present, and
  the dev check (`generate --seed 1 --events 1000000 --clicks 250000`, `scripts/docker_run.sh dev
  out`, `python3 scripts/compare.py out dev-answer` must print `MATCH`). `CPUS` and `MEM` are set
  for the runner, because the script's defaults of 8 and 24g are refused on a smaller machine.
- **bench-60m** (final scale; `run_60m` input, or `[60m]` in the commit message): `--events
  60000000 --clicks 15000000`, one run with the scorer's flags. Reports the exit code, container
  time against the 180 s cap, peak memory, output and scratch use, disk free, the rules'
  self-check, stages from one extra run, and the diff against `/reference`. The reference is the
  correctness tool, not a scored run, so it gets all the memory the runner has (`--memory` of RAM
  minus 1 GiB, plus the swap); if it still does not finish, the report says that correctness at
  this scale rests on the self-check alone.
- **report**: one sticky pull-request comment per commit SHA, found again by a hidden marker and
  updated in place. When several runs tested the same commit (its push, then a 60M run started
  by hand), the comment gathers them: for bench-20m the result with the most paired evidence,
  for the other jobs the latest, each with a link to its run.

A pull request from a branch of this repository is tested by its push event; the pull_request
event only syncs the comment. Without a root `Dockerfile` the pipeline steps are skipped and only
the diagnostics run; the `standin` input runs any commit that way, as a self-test of the harness.

## Evidence

- Each job's step summary starts with the exact commit SHA. When the commit differs from the
  commit that last changed the `Dockerfile`, Cargo files or `src/` (a harness-only commit, or a
  merge of one), the report names that commit too: the image is the same.
- bench-20m starts with one compact line per seed, in the columns of [LEDGER.md](LEDGER.md).
  `python bench/ledger.py RUN_ID` appends it to the ledger.
- Raw results are JSON artifacts: `ss-correctness`, `ss-bench-20m`, `ss-bench-60m`.
- The generator and the dev answer come from the track page and are cached (`TOOLS_KEY` in the
  workflow; bump it to fetch again). Their archives' SHA-256 are checked against the values first
  seen, with a warning if the organizers republish. Nothing from them is committed.

## Local timing (no Docker)

    python bench/local_bench.py target/release/pipeline.exe DATA_DIR --cpus 8 --warmup
    python bench/local_bench.py target/release/pipeline.exe ../data/dev --compare ../dev-answer

Runs the native binary N times (default 3) with the scorer's environment variables and
`SS_TIMING=1`, prints each wall time, the median, and the median per stage. These are native
numbers on whatever machine and operating system you are on: good for a quick ranking, not
evidence. On a machine other people are timing on, run it through the shared lock
(`ss_work/tools/timed.sh`).

## Files

| File | Purpose |
| --- | --- |
| `bench.py` | The driver: `correctness`, `bench20m`, `bench60m`, `parity`, `dataset` |
| `report.py` | Renders a result JSON as Markdown, including the compact line |
| `sticky_comment.py` | Builds and posts the per-commit pull-request comment |
| `ledger.py`, `LEDGER.md` | The experiment ledger |
| `diag/diag.rs`, `diag.Dockerfile` | The diagnostics binary (`noop`, `readthrough`, `memtouch`, `writeout`, `sysinfo`) in an empty image |
| `fetch_tools.sh` | Generator and dev answer, cached |
| `build_prior.sh` | Builds the image of `bench/BEST_SHA` |
| `local_bench.py` | Native timing helper |
| `BEST_SHA`, `SEEDS` | The commit to beat, and the seed list |
