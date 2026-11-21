# Handoff: where the work stands

Short and current; the evidence itself is in `LEDGER.md`. Times are UTC, 2026-11-21 unless said otherwise.
Competition cutoff: 2026-11-22 16:00 UTC (10:00 America/Chicago). No major rewrite after 14:30 UTC.

## Best verified implementation

- **Accepted official best: 0.64 s**, commit `2dae9298f4e1e0a5259eec54c400501b00fae26f` (also 0.64 s:
  `9cb9ad6e3cd0d75f160cecae8005d1c6cb8a7c4e`). Earlier: 0.66 s `4567e9532f4019fbfd0e92908fd01cc49f24f67d`,
  0.71 s `2b2cccd1ba1cb12bc5a074a899ce54c868205b86`, 0.84 s `23b285da8246d00275b1ad191da0fffb8c5f81e4`.
  Official scores are relayed by the user's submission watch; nothing here submits.
- **Best confirmed in CI, not yet scored officially:** candidate E,
  `5704266f417ff89b1d4d2627eb3188e706eb4e4e` (+56 to +67 ms against `2dae9298`, 60M +177 ms). It is the
  commit in `bench/BEST_SHA` (every CI run pairs HEAD against it) and the commit the PR names.
- **Candidate F** (pushed 23:10 with `[60m]`; see the PR for its state): E plus the steps listed below,
  each confirmed alone. The PR names it only once its own CI run confirms the integrated tree.
- Move `bench/BEST_SHA` only to a commit with a confirmed paired win, and read the hash from git
  (`git rev-parse`), never from memory.

## How the engine works now (index mode, the default)

1. Startup: Parquet files left in the output folders are removed; manifest; event files mapped; the
   generator's two ID scramble keys found from the first 64 KiB of the largest events file and the
   layout checked on those samples. Lookup tables (products, campaigns) load on a side thread while
   links are parsed.
2. Parse stage, all threads: event rows -> 16-byte records appended to per-thread lists by index
   partition (2^14 event indexes each), the common row through one piece of assembly; click rows ->
   per-user-range lists through a table-driven kernel. Beside it, one thread resolves links
   (`links::build_indexed`). Then, all threads: the click index (clicks grouped by user), while two
   helper threads let go of the input mappings.
3. Index pass, all threads, partition by partition: `place` puts each record in the slot of its index
   (smallest ingest wins: the dedup; the slot word carries the partition's tag), a scan reads the slots
   in order (events of one user are one run), persons whose IDs are one simple link component are
   sessionized in batches of 32 while their slots are read, everything else goes to the general path.
   First and last run of every partition are joined across partitions afterwards ("chains"; the five
   whale users end up here). Each thread writes its user_daily row groups as they fill.
4. General-path remainder (complex link components, odd IDs), the campaign table, exit.

What must be complete before anything is final: every file must be parsed before any partition can be
placed (rows are fully shuffled across files; any file can hold a copy of any event). What the fast
path assumes and checks: IDs are scrambled dense numbers (keys found on samples, every row checked;
rows that do not fit go to the general path), user indexes never decrease along the event-index order
(checked on every surviving event; a violation restarts the run on the general path, exercised by
`SS_FORCE_RESTART=1` and by mutation tests).

Switches (each keeps the previous behaviour for same-image comparisons): `SS_STRUCT=0` general path;
`SS_KERNEL=plain` compiler's event kernel instead of the assembly; `SS_CLICK_KERNEL=lean|general`;
`SS_BATCH=0` persons one at a time; `SS_STREAM=0` output written at the end; `SS_RELEASE=0` mappings
left to the exit; `SS_EXIT=drop` ordinary return; `SS_INDEX_SHIFT`; `SS_VERIFY=1` fast parsers checked
against the slow one row by row; `SS_TIMING=1` stage times (never in ranked samples).

## Measured critical path

Candidate E + fast exit, run 44953815755, EPYC 9V45, 4 logical CPUs = 2 cores x 2 threads, seed 2, one
instrumented run: container 618.8 ms (no-op container 60); process 524.7 = startup 2.1, link parse
14.5, parse stage 319.3 (workers 299.4, click index 19.6), odd events 2.4, index pass 173.0, general
remainder 3.8, output about 13; then 17 to 34 ms between the last stage and the container's end beyond
a no-op (mapping teardown at exit). Summed worker time, for scale only: event parse 1,034, click parse
161 CPU-ms. The same tree on an EPYC 7763: parse 411, index pass 229; on Intel runners (Xeon 6975P-C,
8370C) the parse stage is 510 to 550 ms.
Measured on that runner: two threads on two cores take 27% longer than four threads (second hardware
thread per core). Unknown: the scorer's container lifecycle time, its exact CPU, how its runs vary, and
the relation between CI times and official scores (five official points; see the calibration table in
the ledger, which is data and not a model).

## Steps confirmed alone since candidate E (all in candidate F)

| Step | Commit | Paired result against E (or its parent) |
|---|---|---|
| Event kernel in assembly | `5db6322` | +69 to +82 ms (Xeon 6975P-C), +24 to +31 ms (EPYC 9V45), 36/36 pairs |
| Sessions while slots are read; batches; slot tags | `9a2b760`, `2d58de3`, `cfc7799` | step 1 +13 to +19; step 3 +7 to +29 over step 2; batch 20 to 23 ms with tags |
| Output during the index pass | `bec7dfd` | 12 to 17 ms (Xeon 8370C, same-image variant) |
| Mappings let go before exit | `9a9c467` | about 5 to 10 ms (39/54 pairs over nine seeds) |
| Table-driven click kernel | `ff5feff` | about 5 ms |
| Links: packed summaries | `f080503` | about 3 to 5 ms |
| Fast exit | `b890895` | about 4 ms |

## Open experiments (ranked by what they could remove)

| # | Change | Could remove at most | State |
|---|---|---:|---|
| 1 | More of the event row in assembly: loop + store (`dafe006`), eleven instructions fewer (`94f1cbb`), constants in registers | proportional to instructions; the Intel runner pays more per instruction | in CI / agent `exp-parse` |
| 2 | Branch padding for the Skylake-family erratum (build flag `-x86-branches-within-32B-boundaries`) | unknown; cannot show on CI runners (none is affected); only an official score can tell | lead, next |
| 3 | `place` variants (`SS_PLACE=1/2/3`), batch size, per-partition assists table (`SS_ASSISTS=1`) | place is about 35 ms elapsed, assists unknown | in CI / agent `exp-sessions` |
| 4 | Stage before the event parse: link rows through a vector kernel | 11 to 16 ms elapsed today | click + links agents |
| 5 | Whale tail of the index pass (chains start early; extras kept in time order) | 12 ms on one or two threads | lead (dispatch) + sessions agent (extras) |

Rejected, with evidence in the ledger: forward-only index mode; partition-owned winners by direct
atomic scatter (1.5-2.2x slower than lists); one stream per owner with prefetched plain stores (2-3x
slower); two-level partitioning (slower at 20M, equal at 60M); write-combining/non-temporal list
writes (no gain); 2^13 and 2^15 partitions (CPU-dependent, no robust gain); decode-after-dedup (at most
2-4% of parse, before reread costs); link builder as an 8-thread bottleneck (official score
unchanged); parser "two halves out of step" (CI regression); probing the forest instead of a lookup
table (slower dedup); rows-in-file order (there is none); click lists of 4,096 or 16,384 users (no gain
/ 7 to 11 ms slower); one-window click timestamp (no difference).

Not allowed and not used: reconstructing rows from generator state, hard-coded seeds or answers.

## Working rules that cost time when forgotten

- Windows laptop: no Docker/WSL; containers are timed only in GitHub Actions (push to any branch runs
  `ci.yml`; `[60m]` and `[variant=NAME=VALUE]` in the head commit message; `workflow_dispatch` with
  `-f seeds=... -f variant_env=...`). Laptop times do not rank candidates; they have misranked twice.
- In the CI report's same-image variant block A is the default and B the variant: a negative
  "A minus B" means the default is faster.
- One step per branch: an agent's verified commit is pushed to `exp-ci-<name>` and paired against
  candidate E; `scratchpad/lead/pair_with.sh` pairs a commit against any other commit directly.
- Agents work in `C:\Users\zuyua\dev\ss_work\wt\<name>` on `exp-<name>` branches; no `git stash`
  (shared across worktrees); timed runs through `ss_work/tools/timed.sh`, builds and correctness runs
  outside it.
- Verification before a candidate: `cargo test --release`; 36 fixtures and 43 extra fixtures through
  `scripts/run_fixtures.py`; dev against `dev-answer`; 20M seeds 2 and 3 against `ss_work/out/s?_ref`;
  small generated sets and the mutated sets under `ss_work/data/small` against `reference.exe`.
