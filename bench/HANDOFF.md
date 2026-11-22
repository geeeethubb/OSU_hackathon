# Handoff: where the work stands

Short and current; the evidence itself is in `LEDGER.md`. Times are UTC, 2026-11-22 unless said otherwise.
Competition cutoff: 2026-11-22 16:00 UTC (10:00 America/Chicago). No major rewrite after 14:30 UTC;
from about 13:00 the effort shifts to validating one reliable final candidate.

## Best verified implementation

- **Official best: 0.5231 s**, candidate F, `1033956062d3cd948462c664d822f2636bcf1153` (receipt
  BB-337F162E). Previous: 0.6389 s `2dae9298f4e1e0a5259eec54c400501b00fae26f`, 0.66 s `4567e953...`,
  0.71 s `2b2cccd1...`. Board best when last checked: 0.2778 s. Official scores are relayed by the
  user's submission watch; nothing here submits.
- `bench/BEST_SHA` = F: every CI run pairs HEAD against the officially scored commit. F must stay
  recoverable: it is the commit named in the PR until a successor is confirmed.
- **Candidate F2 in CI** (pushed 00:00): `9e31bf35b6926a20b282ad270c8005082c6ec40f` = F + parser chain
  to `64eecd8` + index-pass chain to `4f16ecb` + links `cfdf102`. Named in the PR only if its own run
  confirms it against F (20M three seeds, fresh seeds, 60M).

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
| G | Purchases and declined rows finished inside the assembly instead of leaving for slower parsers that re-read them | the assembly takes exactly the rows the general parser accepts, with identical values; per-case proof for rows rejected inside; differential tests both ways | 4.45% of rows take ~30% of kernel time; bound ~25% of event parse, ~10% of process | code size, a purchase path | BUILD, parser agent (`SS_RARE=0` switch) |
| H | Slot scan four slots per step, purchases and invalid winners included | per-slot semantics unchanged; tested against one-at-a-time code | one-slot path 14-17% of the partition pass (3.75M mispredicted exits) | AVX2 classification per four slots | IN CI (`2764dfa`, `SS_WIDE=1`), sessions agent |
| I | Batch finish (attribution, cells, rows) restructured to remove work | rules for credited/assists/organic unchanged | 37-39% of the partition pass; bound not yet known | unknown | DESIGN FIRST, sessions agent; stop if under 5% |
| J | Clicks grouped per partition inside the index pass; no click-index phase and no 80 MB second copy | same click set per user and same filter; the kernel is order-free over a person's clicks | the phase is 19.6-25.2 ms elapsed (3-4% of process) | ranges straddling two partitions read twice or built once with a wait | BUILD, click agent (`SS_CLICK_GROUP=0`); kill on no paired win |
| K | The five whale persons spread over threads (split at session gaps or stitch the boundary sessions) and chains started early | session boundaries come from consecutive ts; a session's day and attribution depend on its start only | tail 12.5 ms on CI; larger share with 8 threads | stitching code | MEASURE the tail left in F2 first; lead |
| L | 8-byte records | needs at least 80 bits without truncation; truncating user numbers would make the layout check unsound | lists are 9% of event parse | | KILLED on paper (bit budget); 12-byte variant bounded at ~3%, not now |
| M | Two rows interleaved per loop iteration | | none: with both hardware threads busy the core is already throughput-bound | | KILLED by measurement |
| N | Eight rows per vector lane (transposed) for timestamp and ID arithmetic | same decode, vertical | arithmetic ~90 of 197 straight-line instructions; transposition eats much of it; estimate under 15% | large rewrite | PAPER SCREEN by the parser agent after G; build only above 15% |
| O | EVEX-256 kernel (AVX-512 F/BW/VL/VNNI) with runtime dispatch; AVX2 fallback | same acceptance set | unknown; the scorer's n2 Xeons have these instructions | second kernel; the scorer's exact CPU cannot be tested | PAPER SCREEN (uop count) by the parser agent |
| P | Decode after dedup (offsets instead of records) | | measured earlier: at most 2-4% of parse before the random re-read of 1.6 GB | | KILLED; nothing has changed since |
| Q | Index pass overlapped with the parse | any file holds copies of any event: no partition is final before the last row | | | KILLED on paper |
| R | Scorer calibration through official submissions: R1 an identical binary again (run-to-run variation); R2 F + jumps padded off 32-byte boundaries (Skylake-family erratum; CI has no affected CPU) | both builds pass every CI check | R2 unknown; CI cost of the padding 1-5 ms on an EPYC 7763 | two submissions | RECOMMENDED to the submission watch |

Bounded incremental stream on F, one step at a time: the parser's kernel steps, the sessions agent's
scan steps, links and click kernels (small), memory reuse of consumed list blocks (small).

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
