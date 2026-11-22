# Handoff: where the work stands

Short and current; the evidence itself is in `LEDGER.md`. Times are UTC, 2026-11-22 unless said otherwise.
Competition cutoff: 2026-11-22 16:00 UTC (10:00 America/Chicago). No major rewrite after 14:30 UTC;
from about 13:00 the effort shifts to validating one reliable final candidate.

## Where it stands (2026-11-22, shopping-sessions-5)

- **Accepted official best: F5 `32b8fffd658af203da5e5809c9d3ce9eaede94ee` = 0.7698 s** (receipt
  BB-4A6C474E; rank 11 as relayed; board top 0.5528 s as relayed). `bench/BEST_SHA` names F5. Earlier on
  this dataset: `9735a63` 1.2745 s (BB-32BE0EA7); F4 `bad5c1b` 5.6629 s (index mode, outside the rules).
- **Goal:** below 0.6018 s official (stretch 0.20 s). Only official scores count; CI ratios are not forecasts.
- **F6 (being validated):** F5 + prepared link-table probes in the forward rings (`951373b`) + long user
  IDs spelled out by the output writers (`9643656`), with direct click emission removed (`0e71b47`) and
  the link-pool step reverted (`df5a154`). Steps measured in CI against their parents: probe tokens
  +20 to +39 ms (12/12 seed runs, EPYC 9V74/9V45), names +35.5 / +40.0 / +29.2 ms (17/18, Xeon 8370C).
- 60M: the CI reference is OOM-killed at 14 GiB; complete comparisons run on the laptop (31 GiB): F5 and
  the later heads MATCH new 60M seed 2 (reference exit 0, 140 s).

## Performance model on the current data (reset 03:00-03:27 UTC; MEASURED unless marked)

Container time on CI (2 cores x 2 threads; seed 2, EPYC 7763, `dffab6f`): 1,219 ms; in-process 1,124 ms.

| stage, 4 threads | wall ms | thread-ms | share of thread time |
|---|---:|---:|---:|
| startup + link rows | 33 | ~120 | 3% |
| event parse | 534 (both kinds) | 1,815 | 40% |
| click parse | (same stage) | 316 | 7% |
| dedup winners | 336 (stage) | 295 | 7% |
| dedup forward (link lookup, scatter by person) | | 833 | 19% |
| click map (link lookup, scatter by person) | | 178 | 4% |
| sessions (group, sessionize, attribute, emit) | 221 | ~880 | 20% |
| link build (side thread, off the critical path) | | 140 | |

- **Event parse split (Linux, same runner; `SS_DIAG=2` scan only, `SS_DIAG=7` decode without store):**
  one thread 187 scan / +856 decode / +149 store = 1,192 ms (16% / 72% / 12%); four threads 293 / +1,173
  / +349 = 1,815 thread-ms (16% / 65% / 19%). Decoding fields is the cost, not reading bytes.
- **Inside decode (laptop, Ryzen AI 9 365, Windows):** user-table lookups for long user IDs are 10% of
  decode-without-store (1,481 -> 1,332 thread-ms at 4 threads with `SS_DIAG_USERS=1`, which fakes them);
  general-kernel steps one thread: structure +61, bounds +49, ID codes ~0, timestamps +194, type/user/
  price +371, store +259 ms. Linux figure for the lookups: run pending (`3462185`, diag envs).
- **Shared coordination left:** the user table (reads lock-free since `6d9ec1f`; inserts lock a region,
  about 183K distinct long user IDs at 20M); one atomic per task. Contention measured before `6d9ec1f`:
  event-parse CPU -20% at 4 threads when the shard locks went (laptop), +56-62 ms in CI.
- **Scaling, CI (2 cores x 2 threads):** in-process 2,855 / 1,501 / 1,124 ms at 1 / 2 / 4 threads
  (1.90x, 2.54x); the hyperthread step adds 1.34x, and per-thread CPU rises 28-60% (forward most).
  **Laptop (10 cores / 20 threads, 4 Zen 5 + 6 Zen 5c, 1 MiB L2 per core, 24 MiB L3):** 2,694 / 1,440 /
  856 / 555 ms at 1 / 2 / 4 / 8 threads (4.86x at 8). The scorer (4 cores x 2 threads, n2 Xeon) has
  not been measured; nothing here is an 8-thread measurement of it.
- **Sessions phases (laptop, one thread, 265 of 334 ms):** grouping 79, sessionize 46, attribution 87,
  emit 32, rows and purchases 19. No dominant block.

## Architecture search on this data (hypotheses; the decision rule is end-to-end paired CI)

| ID | Design | Removes | Adds | Exactness | Bound / evidence | Status |
|---|---|---|---|---|---|---|
| A | No global interning where identity is not needed: event IDs deduplicated by value inside their hash partition; click IDs kept per parse worker (only their order matters); user IDs (identity needed for links and names) in a table read without locks | all event- and click-ID map traffic; user shard locks | 2 more words per long-event record; per-worker click-ID lists | equality of event IDs by full 128-bit value inside the partition their value picks; click ties compared by value; user table write-once slots under region locks, never two slots for one ID | MEASURED: `e2aed79` stack +413-442 ms, `a2cf9f0` +70, `6d9ec1f` +56-62 | DONE, in the chain |
| B | Partition ownership instead of shared maps | | | | the engine already routes by hash to one owner per partition (dedup by event-ID partition, sessions by person partition, per-worker lists in between); the one shared structure in the hot loop is the user table (lock-free reads) | nothing left beyond C |
| C | Resolve user identity after dedup, not in the parse loop: long user IDs ride in the record as values; forward and click map look them up (prefetched) together with the link table | the parse's user-table lookups (laptop: 10% of decode) | 2 words per long-user record (about 1 in 8 rows), a lookup per survivor in forward (prefetched ring), inserts of never-linked long users at forward time | the same table, the same write-once protocol; only where the lookup happens moves | ceiling = the parse's lookup cost minus the forward's; Linux figure pending; reject below +15 ms paired | PROTOTYPE 1 if the Linux figure is above about 100 thread-ms |
| D1 | Clicks parsed straight into person partitions (the link table is ready long before the click tasks, which run after the event tasks) | the raw click lists (write and read, about 120 MB) and the click-map pass (178 thread-ms) | link lookups inside the click loop (a small prefetch ring), a wait if links are not ready | the same link table and the same forward_click record; the order of clicks never matters | ceiling about 180 thread-ms minus the lookups moved, about 20-30 ms wall | PROTOTYPE 2 |
| D2 | Forward survivors from the dedup table instead of re-reading the partition lists | the second read of each event record | larger table slots | same winners | the re-read hits L2/L3 right after the first pass; bounded small | REJECTED on paper |
| D3 | Link rows parsed inside the main task pool (no barrier before the event parse) | idle time of the link-row stage (30 ms wall at 4 threads for 66 ms of one-thread work) | handing the pairs to the builder when the last link task ends | same pairs, same build | about 10-15 ms | LATER, small |
| E | Dedup inside person partitions (route every row by person at parse time) | the forward (19% of thread time) | a global by-event-ID check for copies that name another person (1 in 64 duplicated events) | a copy in another person partition cannot be seen locally; any global check is the event-ID shuffle again | no saving once the check is counted | REJECTED on paper |

## Audit experiments (2026-11-22 04:00 UTC audit of `32b8fff`), outcomes

| Audit item | What was done | Measured | Decision |
|---|---|---|---|
| 1A classification reuse at the short-to-long handoff | long-row kernel for every row (laptop) | 4% slower on mixed rows, 12% on short rows; reuse of the 64-byte masks bounded at about 10 instructions on a quarter of rows | REJECTED / bounded |
| 1B/1C small batches or an opaque-ID assembly loop | generated code of the hot loop inspected: about 250 instructions per view/cart row, about 10 spilled-pointer reloads | assembly could cut 20-25% of decode (decode is about 30% of process time) | NEXT, after PGO |
| 2A prepared probe token | `951373b` | +20 to +39 ms, 12/12 seed runs | KEPT |
| 2A ring size 8 / 32 vs 16 | laptop screen | 8 worse; 32 better at 4 threads, not at 8 | no change |
| 2B worker-private user-to-person cache | reuse distance from the data | a user recurs only after hundreds of thousands of survivors | REJECTED on reuse |
| 3 direct click emission (D1) | `f1121d0` | -9 ms (3/18); same image +7 ms faster without it | REJECTED, removed |
| 4 partition sizes | event 11/12, person 11/12 bits (10 is the floor of the key layout) | every variant slower or inconclusive | defaults stay |
| (lead) serial sections | phase/idle timers; long-ID names spelled out lazily (`9643656`) | sessions setup 31 -> 5 ms; +29 to +40 ms in CI | KEPT |
| (lead) link rows in the parse pool | `b236711` | -2.7 / +3.3 / -6.9 ms (7/18): the work moved, nothing idled | REVERTED |
| (lead) PGO of the scored build | `pgo.yml` makes a profile in the pinned image | in progress | |

## How the engine works now (general path, the default)

1. Startup: stale Parquet files removed; manifest; files mapped; link rows parsed by all threads into
   pairs of user codes (a user ID is its exact text code: prefix and up to 8 base-37 characters, or an
   index from the user table for anything else). Lookup tables load on a side thread.
2. Parse, all threads: event rows -> records in per-worker lists by event-ID hash partition (long event
   IDs as their 128-bit value in lists of their own); regular rows through the lean kernel, rows with a
   long ID through `event_lean_long`, the rest through the general kernel and parser. Click rows -> raw
   per-worker lists. Beside it, one thread builds the link table.
3. Dedup, all threads, partition by partition: smallest ingest wins (event ID by code, or by value for
   long ones); survivors forwarded by person (link lookup) into person partitions; clicks mapped the same.
4. Sessions, all threads, partition by partition: group by person, sessionize, attribute, emit
   user_daily rows (each thread its own file); campaign table; exit.

## Final candidate procedure (from about 13:00 UTC on 2026-11-22)

1. Freeze: no new experiments merged after 14:30 UTC. The candidate is whatever head of `pipeline` has
   passed the gate below; if none has since the last official score, the officially scored commit
   stays the named one. Never name a commit whose CI run is still pending.
2. Gate, all on the combined tree, not on its steps: `cargo test --release`; 36 + 43 fixtures; dev
   MATCH; 20M seeds 2 and 3 MATCH locally; CI run with `[60m]`: 18/18 or at least 15/18 pairs faster
   than `bench/BEST_SHA` on three seeds with A/A medians inside +-10 ms, one fresh-seed dispatch run
   (three seeds never used for tuning) also positive on every seed, 60M MATCH with container time and
   peak memory no worse than the baseline's; the local switch sweep (`scratchpad/lead/extended3.sh`)
   all MATCH.
3. Hand-off text for the submission watch: full 40-character SHA read from `git rev-parse` and checked
   with `git cat-file -e`; baseline SHA; what changed; the CI run links; raw paired medians and A/A;
   fresh seeds; 60M; known limitations (CPU dependence, anything measured on one runner type only).
4. After the official result is relayed: record it in the ledger's official table, move
   `bench/BEST_SHA` only if it is the new best, update the PR's "Commit to submit".
5. Leave the repository in a state where the named commit builds from the root Dockerfile with no
   uncommitted change needed.

## Working rules that cost time when forgotten

- Windows laptop: no Docker/WSL; containers are timed only in GitHub Actions (push to any branch runs
  `ci.yml`; `[60m]`, `[variant=NAME=VALUE]`, `[diag=...]` in the head commit message; `workflow_dispatch`
  with `-f seeds=... -f variant_env=... -f diag_envs=...`). Laptop times do not rank candidates.
- In the CI report's same-image variant block A is the default and B the variant: a negative
  "A minus B" means the default is faster.
- One step per branch: `scratchpad/lead/pair_with.sh COMMIT PRIOR BRANCH` pairs any commit against any
  other directly. Combined candidates are benchmarked as a whole; separate gains are never added up.
- Agents work in `C:\Users\zuyua\dev\ss_work\wt\<name>`; no `git stash`; timed runs through
  `ss_work/tools/timed.sh`, builds and correctness runs outside it; never edit timed.sh while waiters exist.
- Verification before a candidate: `cargo test --release`; 36 + 43 fixtures; dev against `dev-answer`;
  20M seeds 2 and 3 against `ss_work/out/s?_ref`; small and mutated sets against `reference.exe`;
  in CI: three seeds paired, fresh seeds, 60M.
