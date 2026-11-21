# Handoff: where the work stands

Short and current; the evidence itself is in `LEDGER.md`. Times are UTC, 2026-11-21 unless said otherwise.
Competition cutoff: 2026-11-22 16:00 UTC (10:00 America/Chicago). No major rewrite after 14:30 UTC.

## Best verified implementation

- **Accepted official best: 0.64 s**, commit `2dae9298f4e1e0a5259eec54c400501b00fae26f` (also 0.64 s:
  `9cb9ad6e3cd0d75f160cecae8005d1c6cb8a7c4e`). Earlier: 0.66 s `4567e9532f4019fbfd0e92908fd01cc49f24f67d`,
  0.71 s `2b2cccd1ba1cb12bc5a074a899ce54c868205b86`, 0.84 s `23b285da8246d00275b1ad191da0fffb8c5f81e4`.
  Official scores are relayed by the user's submission watch; nothing here submits.
- `bench/BEST_SHA` = `2dae9298…`: every CI run pairs HEAD against it. Move it only to a commit with a
  confirmed paired win, and read the hash from git (`git rev-parse`), never from memory.
- The PR description names the commit to submit. It must name a commit only after its CI run shows a
  paired win with all correctness checks; replace the pointer when a better one is confirmed.

## How the engine works now (index mode, the default)

1. Startup: manifest; event files mapped; the generator's two ID scramble keys found from the first
   64 KiB of the largest events file and the layout checked on those samples. Lookup tables
   (products, campaigns) load on a side thread while links are parsed.
2. Parse stage, all threads: event rows -> 16-byte records appended to per-thread lists by index
   partition (2^14 event indexes each); click rows -> per-user-range lists. Beside it, one thread
   resolves links (`links::build_indexed`: per user, no shared table; only users touched by cross-user
   links go through the single table). Then, all threads: the click index (clicks grouped by user).
3. Index pass, all threads, partition by partition: `place` puts each record in the slot of its index
   (smallest ingest wins: the dedup), a scan reads the slots in order (events of one user are one run),
   persons whose IDs are one simple link component go straight to the session kernel
   (`one_person_in_order`), everything else to the general path. First and last run of every partition
   are joined across partitions afterwards ("chains"; the five whale users end up here).
4. General-path remainder (complex link components, odd IDs), then the Parquet output.

What must be complete before anything is final: every file must be parsed before any partition can be
placed (rows are fully shuffled across files; any file can hold a copy of any event). What the fast
path assumes and checks: IDs are scrambled dense numbers (keys found on samples, every row checked;
rows that do not fit go to the general path), user indexes never decrease along the event-index order
(checked on every surviving event; a violation restarts the run on the general path, exercised by
`SS_FORCE_RESTART=1` and by mutation tests).

Switches: `SS_STRUCT=0` general path; `SS_INDEX_SHIFT`; `SS_KERNEL=general`, `SS_CLICK_KERNEL=general`
old parser kernels; `SS_VERIFY=1` fast parser checked against the slow one row by row; `SS_TIMING=1`
stage times (never in ranked samples).

## Measured critical path (CI run 44950017407, EPYC 7763, 4 logical CPUs = 2 cores x 2 threads)

Elapsed, one instrumented run of the tree before candidate D, seed 2: container 828 ms (a no-op
container on that runner: 74 ms); process 724 ms = startup 4, link parse + tables 16, parse stage 458
(parse workers 437, then click index 21; the link builder had finished at 209 and is not waited for),
odd events 2.5, index pass 229 (partitions 218, edge runs/whales 10.5), general remainder 2, output 13.5.
Summed worker time, for scale only: event parse 1,435, click parse 309, index pass 867 (placing 131,
session kernel 456) CPU-ms. Unknown: the scorer's container lifecycle time and its exact CPU; the
relation between CI times and official scores (three official points only; CI gains of 74 and 50 ms
arrived as 0.02 and 0.05 s).

## Open questions as experiments (ranked by the elapsed time they could remove on that runner)

| # | Change | Could remove at most | Rejection test and success line | Owner / state |
|---|---|---:|---|---|
| 1 | Fewer instructions per event row (asm halves; about 340 -> 250) | 65-110 ms of the 437 | paired CI win on AMD and Intel runners; laptop-only wins do not count (c890351 won on the laptop and lost 15-20 ms in CI) | parser agent, `exp-parse` |
| 2 | Session work fused into the slot scan, persons in batches | about 70 ms of the 229 | paired CI win; exact on fixtures, dev, 20M x2, mutated sets | sessions agent, `exp-sessions` (owns the hot loop of `indexed.rs`) |
| 3 | Two-level partitioning / one stream per owner | about 45 ms (parse stores + placing) | REJECTED 21:45: `bench/experiments/scatter2.rs`, run 44952604559: two levels 30-40% slower at 20M and equal at 60M, owner streams 2-3x slower | closed |
| 4 | Lean click kernel (in candidate E), then table-driven | 18 + about 10 ms | same-image CI variant `SS_CLICK_KERNEL=general` | click agent, `exp-clicks` |
| 5 | Links per user in shards (in candidate D), compact summary | 20-25 ms | candidate D's paired run | links agent, `exp-links` |
| 6 | Edge runs/whales without the tail; click index without the second copy; output overlapped; exit without teardown work | 8 + 10 + 5 + 5 ms | each alone, paired CI | lead, not started |

Rejected, with evidence in the ledger: forward-only index mode; partition-owned winners by direct
atomic scatter (1.5-2.2x slower than lists); one stream per owner with prefetched plain stores (2-3x
slower); two-level partitioning (slower at 20M, equal at 60M); write-combining/non-temporal list writes (no gain);
2^13 and 2^15 partitions (CPU-dependent, no robust gain); decode-after-dedup (at most 2-4% of parse,
before reread costs); link builder as an 8-thread bottleneck (official score unchanged); parser
"two halves out of step" (CI regression); probing the forest instead of a lookup table (slower dedup);
rows-in-file order (there is none).

Not allowed and not used: reconstructing rows from generator state, hard-coded seeds or answers.

## Candidates in flight

- `bb32616f718774d07b42c49d1a939c3ece637835` candidate D (structural changes + sharded links): CONFIRMED
  in run 44951847559 (+36.5 / +37.2 / +39.5 ms against `2dae9298`, 60M +74 ms); named in the PR since 21:40.
- `5704266f417ff89b1d4d2627eb3188e706eb4e4e` candidate E (D + lean click kernel; variant = old click
  kernel), CI started 21:35.

## Working rules that cost time when forgotten

- Windows laptop: no Docker/WSL; containers are timed only in GitHub Actions (push to any branch runs
  `ci.yml`; `[60m]` and `[variant=NAME=VALUE]` in the head commit message). Laptop times do not rank
  candidates; they have misranked twice.
- Agents work in `C:\Users\zuyua\dev\ss_work\wt\<name>` on `exp-<name>` branches; no `git stash`
  (shared across worktrees); timed runs through `ss_work/tools/timed.sh`, builds and correctness runs
  outside it.
- Verification before a candidate: `cargo test --release`; 36 fixtures and 43 extra fixtures through
  `scripts/run_fixtures.py`; dev against `dev-answer`; 20M seeds 2 and 3 against `ss_work/out/s?_ref`;
  small generated sets and the three mutated sets under `ss_work/data/small` against `reference.exe`.
