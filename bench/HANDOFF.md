# Handoff: where the work stands

Short and current; the evidence itself is in `LEDGER.md`. Times are UTC, 2026-11-22 unless said otherwise.
Competition cutoff: 2026-11-22 16:00 UTC (10:00 America/Chicago). No major rewrite after 14:30 UTC;
from about 13:00 the effort shifts to validating one reliable final candidate.

## Where it stands (checkpoint 2026-11-22 07:30 UTC, shopping-sessions-5)

- **Candidate F11 = head of `pipeline` (this commit)** = F10 + long-ID link kernel (`SS_LINK_LONG`) + user
  indexes in per-thread blocks with padded region locks (`SS_INDEX_BLOCK`); build inputs = `9612344` (C4).
  Against F10: Intel +7 to +16 ms (CI 34/36, e1 native 68/72), AMD +10 to +14 (49/54 incl. fresh seeds),
  60M 19/19 MATCH (LEDGER 07:30). F10 `20d2f71` and F8 `8652004` are the fallbacks.

- **Candidate F10 = head of `pipeline` (this commit)** = F8 + click long-ID path (`SS_CLICK_LONG`) + block
  recycling in pass B (`SS_RECYCLE`) + wide-row kernel (`SS_WIDE`); build inputs = `929baca` (C2). Against
  F8: Xeon 8573C +16 to +33 ms (35/36 over two runs incl. fresh seeds), AMD +11 to +24 (48/54), 60M +36
  and +55 ms on a 6975P-C with peak 2.13 GiB (LEDGER 07:00). F8 `8652004` stays the fallback.

- **Candidate F8 = `284f1ca` + its ledger commit (head of `pipeline`)**: F7 + input lead 2 KB ahead in the
  row loops (`SS_LEAD`) + long-user lookups trailing the parse in a ring of 8 (`SS_DEFER`). Gate passed
  against F7 (LEDGER 06:00): +15 to +21 ms on seeds 2/3/5 and +1 to +13 on fresh seeds on AMD, 60M
  +137.9 ms with F8's 19/19 runs MATCH and 2.54 GiB; on Intel the lead alone (measured on `907111d` =
  F7 + lead) is +106 to +184 ms (containers, Xeon 6975P-C and 8573C) and +60 to +68 ms (native, Ice
  Lake 8370C), the ring (on F8's binary) +4 to +16 ms; F8's exact commit against F6 on a 6975P-C
  +246 to +254 ms (18/18). No direct F8-against-F7 pair on Intel.
- Rejected since 05:20: the staged person lookup (slower on Ice Lake and AMD), the pass-B scan prefetch
  (slower on AMD, no stall to remove on Intel), the link rows' lead (no change on an 8573C), 11-bit
  partitions (slower on Ice Lake), F9 = F8 + click path alone (whole binary neutral on Intel).
- **Next candidate under test (06:50): C2 = `929baca` = F8 + click long-ID path (`SS_CLICK_LONG`) + block
  recycling in pass B (`SS_RECYCLE`) + wide-row kernel (`SS_WIDE`).** Against F8: Xeon 8573C +17 to +27
  ms (17/18), AMD +11 to +24 (34/36), 60M on a 6975P-C +36 ms with peak 2.13 GiB (2.55 for F8). Local
  gate on the exact tree passed. Pending: fresh seeds (runs 44995297391, -301087), a 60M run with a
  `/reference` compare (44995320953), e1's native whole-image Intel pairs. Branch `scratch-f10`
  (`9870032`) = the same three commits cherry-picked onto `pipeline` (identical build inputs): fast-forward
  `pipeline` to it and add the ledger commit when C2 passes; then name it F10 in PR #1.
- Parked: `link_lean_long` (C3; exact, but the link stage only 28.4 -> 26.8 ms on the laptop).
- The e1 session's Intel measurements: cpu-clock annotates and native AB/BA pairs on Intel draws (its
  branches perf-probe-*; record in its `bench/experiments/cost_model_probe.md`).
- A read-only verification workflow re-derived the published numbers (LEDGER 06:40 corrections). Reply to
  ChatGPT's follow-up drafted from it (PGO: pass-B instructions identical in matched windows; no alignment
  evidence; the af75107 forward rise has no established cause; PGO is in no candidate).

## Checkpoint 2026-11-22 05:20 UTC (superseded above)

- **Accepted official best: F6 `2676ee043442f0c33357a6f07f3548ae6ead3a4e` = 0.7493 s** (BB-86EAB76C;
  board as relayed 05:00: #1 0.5084 s, #2 0.5469 s, us #19). `bench/BEST_SHA` = F6.
  Validated: 20M seeds 2/3/5 +46-49 ms vs F5 (18/18), fresh seeds 59/61/67 +63-67 ms (18/18), 60M
  3,445 vs 3,649 ms with `/reference` 19/19 MATCH, peak 2.57 GiB; locally 60M MATCH.
- Earlier on this dataset: F5 `32b8fff` 0.7698 s (BB-4A6C474E), `9735a63` 1.2745 s (BB-32BE0EA7);
  F4 (index mode, outside the rules) 5.6629 s. Old-dataset scores are not comparable.
- **F7 candidate = `7bb3836` + this ledger commit** (on `pipeline` from 05:20): F6 + general-path
  assembly event kernel + mixed name reused from the probe + click map with the probe ring, **no PGO**.
  Same build inputs as `a33259f`, which beat F6 on every runner sampled: Xeon 8573C +61-68 ms (18/18),
  EPYC 7763 +41-46 (17/18), EPYC 9V45 +15-30 (17/18) (LEDGER 05:20). Local gate passed incl. 60M MATCH.
  CI on the pushed commit ([60m]) and a fresh-seed dispatch pending; PR #1 names it only once they pass.
- **PGO is rejected for the scored build:** on Intel it slows pass B (click map 195 -> 287 thread-ms)
  and loses 13-19 ms; neutral on EPYC 9V74. The earlier "assembly loses on Intel" reading came from PGO
  images whose profile had no counts for the compiled kernel; without PGO the compiled kernel loses
  45-56 ms on the 8573C (1/54 pairs across three CPUs).
- **Laptop timing:** shared with the session osu-hackathon-e1. All CPU-heavy laptop work goes through
  `ss_work/tools/timed.sh` (lock directory `ss_work/bench.lock`); CI ranks, the laptop only screens.
- **Next:** e1's sampling probe of F7 (`bench/experiments/cost_model_probe.md` on e1's branch
  perf-probe-f7; no runner exposes hardware counters): on Intel, 39% of the short-row assembly loop waits
  on the row's own bytes (about 9% of all CPU; under 2% on AMD), and 26-29% of `event_lean_long` waits
  on the user-table slot whose prefetch is issued in the same row. Bounded, exact experiment: input
  lead-ahead prefetch in the row loops (same-image variant, Intel runners), then the long-row probe
  moved rows ahead.

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
