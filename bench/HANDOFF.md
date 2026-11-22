# Handoff: where the work stands

Short and current; the evidence itself is in `LEDGER.md`. Times are UTC, 2026-11-22 unless said otherwise.
Competition cutoff: 2026-11-22 16:00 UTC (10:00 America/Chicago). No major rewrite after 14:30 UTC;
from about 13:00 the effort shifts to validating one reliable final candidate.

## Best verified implementation

- **Official best: 0.4663 s**, candidate F3, `8dde2af12e542b0df94ba6d0be3da4f8660f896d` (receipt
  BB-9900D7A0). Before it: 0.5231 s F `1033956062d3cd948462c664d822f2636bcf1153`, 0.6389 s
  `2dae9298...`, 0.66 s `4567e953...`, 0.71 s `2b2cccd1...`. Board best when last checked: 0.2778 s.
  Official scores are relayed by the user's submission watch; nothing here submits.
- `bench/BEST_SHA` = F3: every CI run pairs HEAD against the officially scored commit. F3 must stay
  recoverable: it is the commit named in the PR until a successor is confirmed (fresh seeds and 60M).
- F3 = F + event loop and store in assembly, merged vector checks, constants in registers, purchases
  finished in the assembly; edge chains processed while partitions run; place restructured, odd-ID
  events merged by time; new link builder; stale output files removed. In CI against F: +42.5 / +38.9 /
  +42.2 ms (18/18), fresh seeds +27 / +31 / +29 ms (18/18), 60M +84.8 ms, peak memory 1.69 GiB.
- Calibration of official scores against one runner (seven points, EPYC 7763): slope about 0.65,
  intercept 65 to 100 ms; F3's own step carried over at more than one to one. Data, not a model.

## What the measurements say about the bottleneck (details: LEDGER 23:30 and 23:58)

- F on a 2-core x 2-thread Xeon 6975P-C runner: container 670 ms, process 608 ms: event+click parse
  435 (event parse 1,435 CPU-ms, click 202), index pass 156 (place 128, session kernel 226, rest 179
  CPU-ms), link stage 10, whale tail 12.5, output 1. The process is compute-bound and parallel.
- Event parse: the kernel is 86% of it, the lists' memory traffic 9%, input page faults ~3%, the variety
  of row formats 3-5%. 4.45% of rows (purchases, declines) take ~30% of kernel time.
- With both hardware threads of a core busy, a core is throughput-bound (the second thread adds only
  23-32%): only fewer uops or fewer stalls make a row cheaper; latency hiding inside a thread does not.
- Index pass (time-stamp counters): place 22-24%, batch finish (rows, attribution, cells, output)
  37-39%, one-slot path 14-17%, tight loop 9-10%, assists 6-7%, run ends 6-7%.
- No single change identified removes half of the time. A 40-50% cut would need the per-row parse cost
  and the index pass each roughly halved; that is the direction of the registry below.
- Unknown: the scorer's CPU model (an n2 Xeon: Cascade Lake or Ice Lake), its run-to-run variation, its
  container overhead. From `2dae9298` to F, CI pairs added 183-236 ms and the official score moved 116 ms.

## Experiment registry (hypotheses, not candidates)

| ID | Architectural difference | Correctness argument | Measured / bounded benefit | Added costs | Decision |
|---|---|---|---|---|---|
| G | Purchases and declined rows finished inside the assembly instead of leaving for slower parsers that re-read them | the assembly takes exactly the rows the general parser accepts, with identical values; differential tests both ways | (a) purchases: +4 to +9 ms, `SS_RARE=0` 3.5-6.3 ms slower (16/18): in F3. (b)(1) declined rows without the re-read: +2.6 / -0.3 / -3.2, nothing. (b)(2) invalid-ts rows inside: building | a purchase path in the assembly | (a) KEPT, (b)(1) neutral, (b)(2) in progress |
| H | Slot scan four slots per step, purchases and invalid winners included | per-slot semantics tested against the one-at-a-time code | `SS_WIDE=1` same image: +11.9 / -0.2 / -8.1 and -1.1 / -3.3 / -0.0 ms | AVX2 classification | NOT DEMONSTRATED; stays off |
| I | Batch finish (attribution, cells, rows) restructured to remove work | rules for credited/assists/organic unchanged | 37-39% of the partition pass (about 200 CPU-ms); split not measured | unknown | PARKED (agent stopped for budget) |
| J | Clicks grouped per partition inside the index pass; no click-index phase | same clicks per user, same order | **-31.5 / -27.3 / -29.3 ms** (0/18): the phase went from 23.8 to 0.4 ms but the partition pass grew 130 CPU-ms (neighbouring partitions wait on the range they share) | waiting | KILLED by measurement |
| K | Edge chains joined and processed while partitions still run | same joins and layout check; restart drops everything | +6.3 / +16.4 / +15.0 ms (17/18); tail after the last partition 12.7 -> 0.3 ms | a mutex per partition | KEPT, in F3 |
| M2 | Two rows per pass sharing the ID and timestamp vector work (the other form of M) | every row through today's checks, grouped in pairs; a declined second row falls back to the one-row path for both | bound 10-15% of event parse; killed on paper unless a pair saves 12% of the straight line's instructions | gathering the second row's fields | COUNT FIRST, parser agent |
| T | Lookup tables without one allocation per product | same table and map contents; readers try the table first | bound: the 5.3 ms table wait at startup (about 9 ms with eight threads) | none | IN CI (`60758b0`) |
| L | 8-byte records | needs at least 80 bits without truncation; truncating user numbers would make the layout check unsound | lists are 9% of event parse | | KILLED on paper (bit budget); 12-byte variant bounded at ~3%, not now |
| M | Two rows interleaved per loop iteration | | none: with both hardware threads busy the core is already throughput-bound | | KILLED by measurement |
| N | Eight rows per vector lane (transposed) for timestamp and ID arithmetic | same decode, vertical | arithmetic ~90 of 197 straight-line instructions; transposition eats much of it; estimate under 15% | large rewrite | PAPER SCREEN by the parser agent after G; build only above 15% |
| O | EVEX-256 kernel (AVX-512 F/BW/VL/VNNI) with runtime dispatch; AVX2 fallback | same acceptance set | unknown; the scorer's n2 Xeons have these instructions | second kernel; the scorer's exact CPU cannot be tested | PAPER SCREEN (uop count) by the parser agent |
| P | Decode after dedup (offsets instead of records) | | measured earlier: at most 2-4% of parse before the random re-read of 1.6 GB | | KILLED; nothing has changed since |
| Q | Index pass overlapped with the parse | any file holds copies of any event: no partition is final before the last row | | | KILLED on paper |
| R | Scorer calibration through official submissions (identical binary again; F + jump padding) | both builds pass every CI check | padding costs 1-5 ms on an unaffected AMD runner; the scorer's CPU is unknown | two submissions | NOT PURSUED: the user ruled out calibration submissions; the one-runner calibration above replaces R1 |

Bounded incremental stream on F, one step at a time: the parser's kernel steps, the sessions agent's
scan steps, links and click kernels (small), memory reuse of consumed list blocks (small).

## Architecture pass from the rules (lead, 2026-11-22 01:30 UTC; own review passes, not independent agents)

What the outputs require (traced back from rules.md):
- A person's sessions depend on ALL of that person's surviving events (an intermediate event can merge
  two sessions), so no session is final before every event file is read: rows are shuffled across files
  (MEASURED). Barrier 1: all event copies read. Dedup needs every copy's event_id and ingest_ts; the other
  fields matter only for the winning copy, which is known only after all copies are seen.
- Persons need all link lines (transitive, name = byte-wise smallest ID incl. link-only IDs). Barrier 2:
  all links read before persons are final (links are 1% of the input and are read first).
- Credit and assists need the person's final sessions and all of their valid clicks. Barrier 3: all
  click files read. Click dedup is unnecessary for exact output (copies are identical: same ts, same
  click_id, same campaign).
- user_daily needs per (person, start day): sessions, events, revenue, and per-category revenue sums for
  top_category (ties byte-wise, `unknown` competes): not mergeable as a single top value.
- campaign_daily needs per (campaign, day): credited sessions, purchasing sessions, revenue, and distinct
  assisting campaigns per session aggregated by day: sums, mergeable across persons in any order.

Essential versus implementation choice: scanning every byte (malformed lines) ESSENTIAL; event_id and
ingest for every copy ESSENTIAL; decoding user/ts/type/product/price for losing copies CHOICE (bounded:
deferral measured at 2-4% of parse before the random re-read of 1.6 GB: REJECTED); grouping by person
ESSENTIAL; a global sort CHOICE (the engine places by dense index instead, a checked data property);
waiting for all rows before sessions ESSENTIAL (barrier 1); click dedup CHOICE (skipped); materializing
every output row before writing CHOICE (the engine streams).

Designs considered this round, ranked by plausible saving x confidence / effort:
| ID | Design (family) | Removes / moves | Bound on CI container time | Decision |
|---|---|---|---|---|
| E1 | Measure on the scorer's class of CPU (Skylake-family / Ice Lake Xeon): stage profile, padded jumps | nothing; tests whether the scorer's front end penalizes the assembly loop | unknown | DONE: 20 runners drew EPYC 7763/9V74/9V45 and Xeon 6975P-C/8573C only; the scorer's CPU class is not reproducible here. Padded jumps stay untested (needs an authorized official run) |
| D | Click index built while the event parse still runs (memory-bound grouping beside uop-bound parsing on SMT siblings; B: scheduling) | the 20 ms barrier phase after the parse; its work moves to idle issue slots of busy cores | <= 20 ms | PROTOTYPE 1 |
| I2 | Attribution by one merge walk per person over time-sorted clicks and sessions (A) | per-click reach searches and range masks (about 56 CPU-ms) and most assist generation (about 52) | about 15-25 ms | PROTOTYPE 2, after D |
| X | EVEX-256 event kernel with VBMI2 compress, runtime dispatch (parser) | quote handling and part of field finding (about 30 of 216 instructions) | up to about 60 ms on VBMI2 CPUs, 0 on Cascade Lake and Zen 3 | DEFER: large, scorer CPU unknown, the AVX2 kernel stays the fallback |
| L, P, Q, M2, J | 8-byte records; decode after dedup; overlap of the index pass with the parse; two rows per pass; on-demand click grouping | see registry | | REJECTED (impossible / bounded / measured slower) |

## How the engine works now (index mode, the default)

1. Startup: Parquet files left in the output folders are removed; manifest; event files mapped; the
   generator's two ID scramble keys found from the first 64 KiB of the largest events file and the
   layout checked on those samples. Lookup tables load on a side thread while links are parsed.
2. Parse stage, all threads: event rows -> 16-byte records appended to per-thread lists by index
   partition (2^14 event indexes each), the common row through one piece of assembly; click rows ->
   per-user-range lists through a table-driven kernel. Beside it, one thread resolves links. Then, all
   threads: the click index, while two helper threads let go of the input mappings.
3. Index pass, all threads, partition by partition: `place` puts each record in the slot of its index
   (smallest ingest wins: the dedup; the slot word carries the partition's tag), a scan reads the slots
   in order, persons whose IDs are one simple link component are sessionized in batches of 32 while
   their slots are read, everything else goes to the general path. First and last runs are joined
   across partitions afterwards ("chains"; the whales end up here). Each thread writes its user_daily
   row groups as they fill.
4. General-path remainder, the campaign table, exit.

What the fast path assumes and checks: IDs are scrambled dense numbers (keys found on samples, every
row checked; rows that do not fit go to the general path); user indexes never decrease along the
event-index order (checked on every surviving event; a violation restarts on the general path).

Switches (each keeps the previous behaviour for same-image comparisons): `SS_STRUCT=0`, `SS_KERNEL=plain`,
`SS_CLICK_KERNEL=lean|general`, `SS_BATCH=0|n`, `SS_PLACE=0..3`, `SS_ASSISTS=0|1|2`, `SS_WIDE=1`,
`SS_STREAM=0`, `SS_RELEASE=0`, `SS_EXIT=drop`, `SS_INDEX_SHIFT`, `SS_VERIFY=1`, `SS_TIMING=1`,
`SS_DIAG=1|7|8` (diagnostics, stop after the parse).

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
