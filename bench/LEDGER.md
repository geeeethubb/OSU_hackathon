# Experiment ledger

One row per candidate and seed, in the format the bench-20m job prints at the top of its summary
and of the sticky pull-request comment. To add the row of a finished CI run:

    python bench/ledger.py RUN_ID      # appends the row(s); then fill in "next decision" and commit

How the numbers are taken is in [README.md](README.md). In short: every time is one fresh
container with the scorer's flags; "container ms" is that container's own
`State.StartedAt`→`State.FinishedAt`; "paired ms saved" is baseline minus candidate over shuffled
AB/BA pairs on the same runner and input, to be read against the A/A variation next to it. All
rows so far are from GitHub-hosted runners (4 vCPUs, AMD EPYC 7763, no AVX-512): they rank
candidates; they are not the scorer's numbers.

In "next decision", say what the experiment was for (hypothesis and the stage it targets), whether
it is kept or dropped, and what follows.

| exact SHA | baseline SHA | host/limits | data size, seed and bytes | timer boundary/cache state | raw paired ms | median paired ms saved | A/A variation | fixtures + dev diff | 60M result if run | next decision |
|---|---|---|---|---|---|---|---|---|---|---|
| 33bf8126a458737a54e4f8c1f2cce43fb849f13c (measured as 7001bc216b807756df1e10e6f27a2fe0fabb8f6b, which has the same Dockerfile, Cargo files and src/ plus the harness; [run 44924532613](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44924532613)) | none: this is the baseline | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 23106, 24151, 22967 (median 23106); client wall ms: 23228, 24256, 23066 | n/a: nothing to pair with | not run (the check was added after this run) | 36/36 fixtures; dev MATCH; 20M vs /reference: 5/5 runs of this commit MATCH | not run | Baseline: the simple reference pipeline as `/pipeline`. Largest stage dedup 13,372 ms (59%), then sort 4,555 ms; container minus in-process total 400.9 ms; peak memory 4.58 GiB. Kept as `/reference`; superseded as `/pipeline` by the optimized engine 35c4cace (now `bench/BEST_SHA`). |
