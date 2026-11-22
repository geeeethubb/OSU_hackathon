# Experiment ledger

One row per candidate and seed, in the format the bench-20m job prints at the top of its summary
and of the sticky pull-request comment. To add the row of a finished CI run:

    python bench/ledger.py RUN_ID      # appends the row(s); then fill in "next decision" and commit

Where `gh` is a wrapper script rather than a program Python can start (the Windows box), fetch
first and point at the folder:

    ghx run download RUN_ID --dir /tmp/run && python bench/ledger.py --dir /tmp/run

How the numbers are taken is in [README.md](README.md). In short: every time is one fresh
container with the scorer's flags; "container ms" is that container's own
`State.StartedAt`→`State.FinishedAt`; "paired ms saved" is baseline minus candidate over shuffled
AB/BA pairs on the same runner and input, to be read against the A/A variation next to it. All
rows are from GitHub-hosted runners (4 vCPUs; the CPU model differs from job to job and is named
in each row): they rank candidates within a row; they are not the scorer's numbers, and times in
different rows are not comparable with each other.

In "next decision", say what the experiment was for (hypothesis and the stage it targets), whether
it is kept or dropped, and what follows.

| exact SHA | baseline SHA | host/limits | data size, seed and bytes | timer boundary/cache state | raw paired ms | median paired ms saved | A/A variation | fixtures + dev diff | 60M result if run | next decision |
|---|---|---|---|---|---|---|---|---|---|---|
| 33bf8126a458737a54e4f8c1f2cce43fb849f13c (measured as 7001bc216b807756df1e10e6f27a2fe0fabb8f6b, which has the same Dockerfile, Cargo files and src/ plus the harness; [run 44924532613](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44924532613)) | none: this is the baseline | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 23106, 24151, 22967 (median 23106); client wall ms: 23228, 24256, 23066 | n/a: nothing to pair with | not run (the check was added after this run) | 36/36 fixtures; dev MATCH; 20M vs /reference: 5/5 runs of this commit MATCH | not run | Baseline: the simple reference pipeline as `/pipeline`. Largest stage dedup 13,372 ms (59%), then sort 4,555 ms; container minus in-process total 400.9 ms; peak memory 4.58 GiB. Kept as `/reference`; superseded as `/pipeline` by the optimized engine 35c4cace (now `bench/BEST_SHA`). |
| f1382a746f217d9fbf566d0fc2377d1bd6dec897 (image built from the same Dockerfile, Cargo files and src/ as e4eef020ce0bb9e005c329e57ea241c6e842dde8) [run 44926313067](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44926313067) | 35c4cace946a1aa93cef506588e6b9782af8a3c2 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 9V45 96-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1531, 1541, 1534 (median 1534); client wall ms: 2093, 2104, 2272; pairs prior/this: 2,491.9/1,543.3 (BA), 2,529.6/1,555.6 (AB), 2,498.0/1,533.1 (AB), 2,529.3/1,564.0 (AB), 2,531.9/1,551.8 (BA), 2,503.2/1,530.8 (BA) | +968.8 ms (6/6 pairs won; medians 2,516.3 → 1,547.5); larger than every A/A difference seen here | median abs diff 20.6 ms, max 54.4 ms over 6 A/A pairs (signed A−B: -12.2, -37.3, +17.4, +23.9, -54.4, +17.3); second run faster in 2/6 | 36/36 fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | same pipeline source at a484a1a8, [run 44927312977](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44927312977), Intel Xeon Platinum 8573C runner: 60M events / 15M clicks, seed 2, 5,698,099,490 bytes; container 6,747.9 ms, exit 0, peak 3.08 GiB, MATCH /reference (the reference was given 14g plus swap); Docker client wall 8,439.5 ms | Experiment: AVX2 event row parser (stage: parse, 1,699 ms in the prior best to 735 ms here). Largest stage is still parse 735 ms (53%), then dedup 370 ms; container minus in-process total 147.9 ms; peak memory 1.07 GiB. The saving is 47 times the median A/A difference and holds on the holdout seed below: the evidence is for keeping it and moving `bench/BEST_SHA` to e4eef020. The decision is the lead's. |
| f1382a746f217d9fbf566d0fc2377d1bd6dec897 (image built from the same Dockerfile, Cargo files and src/ as e4eef020ce0bb9e005c329e57ea241c6e842dde8) [run 44926313067](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44926313067) | 35c4cace946a1aa93cef506588e6b9782af8a3c2 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 9V45 96-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1549, 1538, 1567 (median 1549); client wall ms: 2079, 2073, 2094; pairs prior/this: 2,511.4/1,536.4 (AB), 2,476.4/1,571.5 (AB), 2,553.4/1,514.1 (BA), 2,504.6/1,518.1 (BA), 2,537.3/1,517.1 (BA), 2,545.0/1,542.3 (AB) | +994.6 ms (6/6 pairs won; medians 2,524.4 → 1,527.3); larger than every A/A difference seen here | median abs diff 20.9 ms, max 58.8 ms over 6 A/A pairs (signed A−B: +36.3, -17.2, +58.8, -16.3, -18.5, -23.4); second run faster in 1/6 | 36/36 fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | not run | Holdout confirmation of the row above: same direction and size (+994.6 ms, 6/6 pairs). Largest stage parse 748 ms (52%); container minus in-process total 151.9 ms; peak memory 1.08 GiB. |
| 03d41bcbeb955abca2ff9607397fe1efc317c4ee (image built from the same Dockerfile, Cargo files and src/ as abcda6b39f5e06f5d9975d54ab9cc2126f87940f) [run 44928521510](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44928521510) | e4eef020ce0bb9e005c329e57ea241c6e842dde8 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 9V45 96-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1197, 1241, 1219 (median 1219); client wall ms: 1700, 1757, 1739; pairs prior/this: 1,699.9/1,248.5 (BA), 1,675.1/1,210.0 (AB), 1,665.6/1,183.4 (AB), 1,703.4/1,182.7 (AB), 1,684.6/1,186.2 (BA), 1,749.9/1,209.0 (BA) | +490.3 ms (6/6 pairs won; medians 1,692.3 → 1,197.6); larger than every A/A difference seen here | median abs diff 10.6 ms, max 27.2 ms over 6 A/A pairs (signed A−B: +8.4, +1.8, +1.4, -13.6, +12.8, -27.2); second run faster in 3/6 | 36/36 fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | seed 2: container 4,150.6 ms, exit 0, peak 3.10 GiB, MATCH /reference | KEPT. Hypothesis: a lean AVX2 row parser (direct product table, cheaper timestamps) and a prefetching dedup pass cut the two largest stages. — largest stage parse 506 ms (46%); container minus in-process total 123.8 ms; peak memory 1.08 GiB |
| 03d41bcbeb955abca2ff9607397fe1efc317c4ee (image built from the same Dockerfile, Cargo files and src/ as abcda6b39f5e06f5d9975d54ab9cc2126f87940f) [run 44928521510](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44928521510) | e4eef020ce0bb9e005c329e57ea241c6e842dde8 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 9V45 96-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1239, 1238, 1248 (median 1239); client wall ms: 1796, 1794, 1808; pairs prior/this: 1,698.0/1,227.7 (AB), 1,762.8/1,292.7 (AB), 1,726.4/1,241.8 (BA), 1,722.4/1,177.3 (BA), 1,691.8/1,248.0 (BA), 1,727.4/1,246.1 (AB) | +475.8 ms (6/6 pairs won; medians 1,724.4 → 1,243.9); larger than every A/A difference seen here | median abs diff 18.4 ms, max 57.1 ms over 6 A/A pairs (signed A−B: +10.2, +9.2, -29.1, +17.7, +57.1, +19.0); second run faster in 4/6 | 36/36 fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | seed 2: container 4,150.6 ms, exit 0, peak 3.10 GiB, MATCH /reference | KEPT. Hypothesis: a lean AVX2 row parser (direct product table, cheaper timestamps) and a prefetching dedup pass cut the two largest stages. — largest stage parse 499 ms (44%); container minus in-process total 116.2 ms; peak memory 1.09 GiB |
| 03b15f2f8a1281773184f4f7389436bda4be0a44 (image built from the same Dockerfile, Cargo files and src/ as 67842d738078fe18990dd38a9765d00c51f78343) [run 44929974438](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44929974438) | 03d41bcbeb955abca2ff9607397fe1efc317c4ee | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 9V74 80-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1029, 1096, 1089 (median 1089); client wall ms: 1670, 1611, 1625; pairs prior/this: 1,288.3/1,184.0 (BA), 1,277.0/1,200.3 (AB), 1,304.6/1,114.3 (AB), 1,315.8/1,101.2 (AB), 1,278.4/1,108.8 (BA), 1,283.0/1,098.5 (BA) | +177.0 ms (6/6 pairs won; medians 1,285.6 → 1,111.6); larger than every A/A difference seen here | median abs diff 2.6 ms, max 28.0 ms over 6 A/A pairs (signed A−B: -28.0, -4.6, -3.7, +1.4, -0.4, +0.0); second run faster in 3/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | not run | KEPT. Hypothesis: flat union-find for identity links and partition counts that scale with the input cut link build and cache misses in dedup and sessions. — largest stage parse 456 ms (46%); container minus in-process total 104.2 ms; peak memory 1.11 GiB |
| 03b15f2f8a1281773184f4f7389436bda4be0a44 (image built from the same Dockerfile, Cargo files and src/ as 67842d738078fe18990dd38a9765d00c51f78343) [run 44929974438](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44929974438) | 03d41bcbeb955abca2ff9607397fe1efc317c4ee | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 9V74 80-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1034, 1033, 1042 (median 1034); client wall ms: 1581, 1579, 1586; pairs prior/this: 1,229.5/1,035.9 (AB), 1,216.6/1,040.4 (AB), 1,213.4/1,047.2 (BA), 1,225.9/1,037.1 (BA), 1,230.7/1,051.6 (BA), 1,205.4/1,041.6 (AB) | +177.6 ms (6/6 pairs won; medians 1,221.3 → 1,041.0); larger than every A/A difference seen here | median abs diff 9.2 ms, max 40.7 ms over 6 A/A pairs (signed A−B: +10.5, -6.3, -2.1, +8.0, -24.9, +40.7); second run faster in 2/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | not run | KEPT. Hypothesis: flat union-find for identity links and partition counts that scale with the input cut link build and cache misses in dedup and sessions. — largest stage parse 447 ms (48%); container minus in-process total 101.0 ms; peak memory 1.10 GiB |
| 91c180bc4f85c0a08db8d222664b853c7cda78d6 [run 44930828767](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44930828767) | 03b15f2f8a1281773184f4f7389436bda4be0a44 | GitHub runner ubuntu26 20261116.88.1, Intel(R) Xeon(R) 6975P-C, 4 vCPU, 15.3 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1141, 1139, 1143 (median 1141); client wall ms: 1201, 1199, 1219; pairs prior/this: 1,151.6/1,162.9 (BA), 1,146.1/1,145.0 (AB), 1,162.2/1,141.5 (AB), 1,144.9/1,134.3 (AB), 1,152.1/1,140.8 (BA), 1,154.1/1,160.6 (BA) | +5.8 ms (4/6 pairs won; medians 1,151.8 → 1,143.3); larger than the median A/A difference but inside its range: treat as unconfirmed | median abs diff 3.7 ms, max 29.8 ms over 6 A/A pairs (signed A−B: +5.2, -2.5, -5.0, -29.8, +2.1, +1.4); second run faster in 4/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | not run | KEPT (step toward 23b285da). Hypothesis: about 24K records per partition instead of a fixed 2^11 partitions keeps each partition in L2 at any scale. — largest stage parse 567 ms (53%); container minus in-process total 72.6 ms; peak memory 1.08 GiB |
| 91c180bc4f85c0a08db8d222664b853c7cda78d6 [run 44930828767](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44930828767) | 03b15f2f8a1281773184f4f7389436bda4be0a44 | GitHub runner ubuntu26 20261116.88.1, Intel(R) Xeon(R) 6975P-C, 4 vCPU, 15.3 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1149, 1162, 1164 (median 1162); client wall ms: 1224, 1240, 1236; pairs prior/this: 1,165.8/1,166.7 (AB), 1,143.6/1,158.6 (AB), 1,179.5/1,174.6 (BA), 1,159.8/1,149.5 (BA), 1,166.5/1,146.6 (BA), 1,149.5/1,155.9 (AB) | +2.0 ms (3/6 pairs won; medians 1,162.8 → 1,157.2); within the A/A noise: no measurable difference | median abs diff 5.9 ms, max 12.6 ms over 6 A/A pairs (signed A−B: -8.3, -12.6, +7.8, +2.1, -0.3, +4.1); second run faster in 2/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | not run | KEPT (step toward 23b285da). Hypothesis: about 24K records per partition instead of a fixed 2^11 partitions keeps each partition in L2 at any scale. — largest stage parse 570 ms (53%); container minus in-process total 78.6 ms; peak memory 1.09 GiB |
| 23b285da8246d00275b1ad191da0fffb8c5f81e4 [run 44930932775](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44930932775) | 03b15f2f8a1281773184f4f7389436bda4be0a44 | GitHub runner ubuntu26 20261116.202.1, INTEL(R) XEON(R) PLATINUM 8573C, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1416, 1405, 1430 (median 1416); client wall ms: 1904, 2040, 2086; pairs prior/this: 1,489.0/1,408.5 (BA), 1,505.3/1,446.9 (AB), 1,488.6/1,432.0 (AB), 1,491.3/1,424.0 (AB), 1,491.4/1,434.2 (BA), 1,515.7/1,441.7 (BA) | +62.9 ms (6/6 pairs won; medians 1,491.4 → 1,433.1); larger than the median A/A difference but inside its range: treat as unconfirmed | median abs diff 7.6 ms, max 105.9 ms over 6 A/A pairs (signed A−B: +17.4, -7.9, -105.9, -4.2, -4.0, +7.4); second run faster in 3/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | not run | KEPT, new prior best. Hypothesis: a third of the anonymous memory was still in 4 KiB pages; asking for huge pages on every large allocation removes page faults and exit teardown (variant SS_THP=0 is 24 to 29 ms slower, losing 11 of 12 pairs). — largest stage parse 711 ms (53%); container minus in-process total 94.8 ms; peak memory 1.08 GiB |
| 23b285da8246d00275b1ad191da0fffb8c5f81e4 [run 44930932775](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44930932775) | 03b15f2f8a1281773184f4f7389436bda4be0a44 | GitHub runner ubuntu26 20261116.202.1, INTEL(R) XEON(R) PLATINUM 8573C, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1456, 1470, 1456 (median 1456); client wall ms: 2052, 2076, 2055; pairs prior/this: 1,509.3/1,433.8 (AB), 1,526.4/1,431.0 (AB), 1,499.1/1,427.2 (BA), 1,496.1/1,494.5 (BA), 1,482.2/1,434.1 (BA), 1,510.9/1,439.3 (AB) | +71.8 ms (6/6 pairs won; medians 1,504.2 → 1,433.9); larger than every A/A difference seen here | median abs diff 9.2 ms, max 42.0 ms over 6 A/A pairs (signed A−B: -12.3, +1.6, +8.1, -10.3, +42.0, -7.9); second run faster in 4/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | not run | KEPT, new prior best. Hypothesis: a third of the anonymous memory was still in 4 KiB pages; asking for huge pages on every large allocation removes page faults and exit teardown (variant SS_THP=0 is 24 to 29 ms slower, losing 11 of 12 pairs). — largest stage parse 749 ms (54%); container minus in-process total 99.1 ms; peak memory 1.09 GiB |
| 16bc175b6e4fe947d03a94535a3baefecd6719a8 [run 44934751037](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44934751037) | 23b285da8246d00275b1ad191da0fffb8c5f81e4 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1053, 1043, 1046 (median 1046); client wall ms: 1129, 1120, 1122; pairs prior/this: 1,148.0/1,048.2 (BA), 1,149.2/1,049.0 (AB), 1,151.0/1,044.6 (AB), 1,145.2/1,049.5 (AB), 1,140.1/1,047.4 (BA), 1,148.8/1,043.5 (BA) | +100.0 ms (6/6 pairs won; medians 1,148.4 → 1,047.8); larger than every A/A difference seen here | median abs diff 9.2 ms, max 33.6 ms over 6 A/A pairs (signed A−B: +1.4, +13.7, -4.6, -0.0, +33.6, +15.4); second run faster in 3/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | seed 2: container 3,148.2 ms, exit 0, peak 2.61 GiB, MATCH /reference; against the baseline in the same job +298.3 ms (6/6 pairs, medians 3,373.6 → 3,071.7; A/A median abs 36.7 ms) [run 44934751037](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44934751037) | KEPT, new prior best and the commit to submit. Hypothesis: the general path with the streaming Parquet writer and the branch-light session pass beats 23b285da, and index mode as pushed (forward only) does not: the SS_STRUCT=1 variant of this image lost 0 of 18 pairs by 136 to 140 ms. — largest stage parse 482 ms (51%); container minus in-process total 98.1 ms; peak memory 916.6 MiB |
| 16bc175b6e4fe947d03a94535a3baefecd6719a8 [run 44934751037](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44934751037) | 23b285da8246d00275b1ad191da0fffb8c5f81e4 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1059, 1066, 1056 (median 1059); client wall ms: 1136, 1148, 1141; pairs prior/this: 1,166.5/1,067.1 (AB), 1,166.9/1,068.4 (AB), 1,158.1/1,060.1 (BA), 1,247.0/1,319.3 (BA), 1,170.0/1,060.6 (BA), 1,168.9/1,050.4 (AB) | +98.9 ms (5/6 pairs won; medians 1,167.9 → 1,063.9); larger than every A/A difference seen here | median abs diff 6.8 ms, max 14.6 ms over 6 A/A pairs (signed A−B: -11.1, +14.6, -0.7, -5.7, -3.9, +7.9); second run faster in 5/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | seed 2: container 3,148.2 ms, exit 0, peak 2.61 GiB, MATCH /reference; against the baseline in the same job +298.3 ms (6/6 pairs, medians 3,373.6 → 3,071.7; A/A median abs 36.7 ms) [run 44934751037](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44934751037) | KEPT, new prior best and the commit to submit. Hypothesis: the general path with the streaming Parquet writer and the branch-light session pass beats 23b285da, and index mode as pushed (forward only) does not: the SS_STRUCT=1 variant of this image lost 0 of 18 pairs by 136 to 140 ms. — largest stage parse 490 ms (51%); container minus in-process total 95.1 ms; peak memory 927.5 MiB |
| 16bc175b6e4fe947d03a94535a3baefecd6719a8 [run 44934751037](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44934751037) | 23b285da8246d00275b1ad191da0fffb8c5f81e4 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 5, 1,877,349,432 bytes in 107 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1049, 1045, 1043 (median 1045); client wall ms: 1123, 1120, 1125; pairs prior/this: 1,152.7/1,042.2 (BA), 1,158.9/1,043.1 (BA), 1,150.3/1,041.3 (AB), 1,190.1/1,039.3 (BA), 1,148.6/1,039.4 (AB), 1,139.6/1,038.5 (AB) | +109.9 ms (6/6 pairs won; medians 1,151.5 → 1,040.4); larger than every A/A difference seen here | median abs diff 12.8 ms, max 52.8 ms over 6 A/A pairs (signed A−B: +2.2, -52.8, +6.2, +19.4, -3.1, -27.1); second run faster in 4/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | seed 2: container 3,148.2 ms, exit 0, peak 2.61 GiB, MATCH /reference; against the baseline in the same job +298.3 ms (6/6 pairs, medians 3,373.6 → 3,071.7; A/A median abs 36.7 ms) [run 44934751037](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44934751037) | KEPT, new prior best and the commit to submit. Hypothesis: the general path with the streaming Parquet writer and the branch-light session pass beats 23b285da, and index mode as pushed (forward only) does not: the SS_STRUCT=1 variant of this image lost 0 of 18 pairs by 136 to 140 ms. — largest stage parse 479 ms (50%); container minus in-process total 100.6 ms; peak memory 916.9 MiB |
| 8e92bfaac21bb5eb4237f3d351c8301b5c7efd2d (image built from the same Dockerfile, Cargo files and src/ as 16bc175b6e4fe947d03a94535a3baefecd6719a8) [run 44934755854](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44934755854) | db0c2e040c65ee822fb12e408eb2b4fe2ec29f20 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 9V74 80-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1108, 1105, 1205 (median 1108); client wall ms: 1181, 1177, 1278; pairs prior/this: 1,375.9/1,113.8 (BA), 1,308.8/1,109.0 (AB), 1,308.6/1,104.2 (AB), 1,307.8/1,246.3 (AB), 1,314.9/1,109.9 (BA), 1,317.0/1,104.4 (BA) | +204.7 ms (6/6 pairs won; medians 1,311.9 → 1,109.4); larger than every A/A difference seen here | median abs diff 5.2 ms, max 33.1 ms over 6 A/A pairs (signed A−B: +4.3, -1.0, -33.1, +7.1, +6.1, +0.5); second run faster in 5/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | not run | Experiment only (same sources as 16bc175b, paired against the unchanged db0c2e04 image as the review asked): the general-path default wins by about 190 to 205 ms. — largest stage parse 513 ms (51%); container minus in-process total 98.3 ms; peak memory 920.4 MiB |
| 8e92bfaac21bb5eb4237f3d351c8301b5c7efd2d (image built from the same Dockerfile, Cargo files and src/ as 16bc175b6e4fe947d03a94535a3baefecd6719a8) [run 44934755854](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44934755854) | db0c2e040c65ee822fb12e408eb2b4fe2ec29f20 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 9V74 80-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1127, 1123, 1118 (median 1123); client wall ms: 1197, 1199, 1195; pairs prior/this: 1,345.5/1,164.1 (AB), 1,328.0/1,133.2 (AB), 1,333.6/1,128.3 (BA), 1,342.4/1,193.8 (BA), 1,354.5/1,278.7 (BA), 1,366.4/1,146.9 (AB) | +188.2 ms (6/6 pairs won; medians 1,344.0 → 1,155.5); larger than every A/A difference seen here | median abs diff 2.8 ms, max 15.7 ms over 6 A/A pairs (signed A−B: +3.0, -8.0, -2.6, +2.5, -0.4, -15.7); second run faster in 1/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | not run | Experiment only (same sources as 16bc175b, paired against the unchanged db0c2e04 image as the review asked): the general-path default wins by about 190 to 205 ms. — largest stage parse 530 ms (51%); container minus in-process total 102.6 ms; peak memory 928.5 MiB |
| 8e92bfaac21bb5eb4237f3d351c8301b5c7efd2d (image built from the same Dockerfile, Cargo files and src/ as 16bc175b6e4fe947d03a94535a3baefecd6719a8) [run 44934755854](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44934755854) | db0c2e040c65ee822fb12e408eb2b4fe2ec29f20 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 9V74 80-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 5, 1,877,349,432 bytes in 107 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1160, 1123, 1106 (median 1123); client wall ms: 1242, 1201, 1178; pairs prior/this: 1,313.9/1,111.6 (BA), 1,304.7/1,113.9 (BA), 1,308.9/1,120.1 (AB), 1,310.6/1,104.1 (BA), 1,306.5/1,112.1 (AB), 1,300.8/1,106.2 (AB) | +194.5 ms (6/6 pairs won; medians 1,307.7 → 1,111.8); larger than every A/A difference seen here | median abs diff 8.7 ms, max 57.8 ms over 6 A/A pairs (signed A−B: -12.0, -1.3, +8.1, -9.2, +57.8, -0.4); second run faster in 3/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | not run | Experiment only (same sources as 16bc175b, paired against the unchanged db0c2e04 image as the review asked): the general-path default wins by about 190 to 205 ms. — largest stage parse 511 ms (51%); container minus in-process total 102.4 ms; peak memory 918.6 MiB |
| 2ff7868fc861348db3e2d918e550ec1eb664bcf1 (image built from the same Dockerfile, Cargo files and src/ as d0caa7046f3f59046676a997a524b25752814772) [run 44936865073](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44936865073) | 16bc175b6e4fe947d03a94535a3baefecd6719a8 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1040, 1050, 1048 (median 1048); client wall ms: 1118, 1125, 1124; pairs prior/this: 1,052.5/1,079.8 (BA), 1,046.2/1,052.4 (AB), 1,047.6/1,069.5 (AB), 1,054.1/1,053.5 (AB), 1,054.3/1,042.0 (BA), 1,271.5/1,143.7 (BA) | -2.8 ms (3/6 pairs won; medians 1,053.3 → 1,061.5); within the A/A noise: no measurable difference | median abs diff 5.4 ms, max 12.3 ms over 6 A/A pairs (signed A−B: +1.2, -5.0, -12.3, -8.1, +5.7, -1.8); second run faster in 5/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | not run | KEPT, the commit to submit: 16bc175b plus the timestamp day-range fix (a day of 32 to 99 no longer lands on a date of the next month). No measurable speed difference, as expected; 60M 2,777 ms, MATCH, peak 2.61 GiB. — largest stage parse 477 ms (50%); container minus in-process total 103.3 ms; peak memory 920.9 MiB |
| 2ff7868fc861348db3e2d918e550ec1eb664bcf1 (image built from the same Dockerfile, Cargo files and src/ as d0caa7046f3f59046676a997a524b25752814772) [run 44936865073](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44936865073) | 16bc175b6e4fe947d03a94535a3baefecd6719a8 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1092, 1052, 1060 (median 1060); client wall ms: 1168, 1127, 1138; pairs prior/this: 1,073.9/1,053.3 (AB), 1,049.1/1,048.5 (AB), 1,066.8/1,056.8 (BA), 1,062.2/1,057.3 (BA), 1,051.4/1,050.1 (BA), 1,054.4/1,055.3 (AB) | +3.1 ms (5/6 pairs won; medians 1,058.3 → 1,054.3); within the A/A noise: no measurable difference | median abs diff 6.1 ms, max 22.0 ms over 6 A/A pairs (signed A−B: -22.0, -6.6, -5.4, +5.6, -2.8, +12.9); second run faster in 3/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | not run | KEPT, the commit to submit: 16bc175b plus the timestamp day-range fix (a day of 32 to 99 no longer lands on a date of the next month). No measurable speed difference, as expected; 60M 2,777 ms, MATCH, peak 2.61 GiB. — largest stage parse 489 ms (51%); container minus in-process total 102.8 ms; peak memory 926.9 MiB |
| 2ff7868fc861348db3e2d918e550ec1eb664bcf1 (image built from the same Dockerfile, Cargo files and src/ as d0caa7046f3f59046676a997a524b25752814772) [run 44936865073](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44936865073) | 16bc175b6e4fe947d03a94535a3baefecd6719a8 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 5, 1,877,349,432 bytes in 107 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1082, 1056, 1045 (median 1056); client wall ms: 1162, 1134, 1127; pairs prior/this: 1,055.6/1,040.4 (BA), 1,053.6/1,053.4 (BA), 1,035.9/1,044.1 (AB), 1,046.8/1,038.2 (BA), 1,040.8/1,035.0 (AB), 1,034.6/1,066.7 (AB) | +3.0 ms (4/6 pairs won; medians 1,043.8 → 1,042.3); within the A/A noise: no measurable difference | median abs diff 5.6 ms, max 10.5 ms over 6 A/A pairs (signed A−B: +6.8, +5.6, -4.8, -5.5, -10.5, +3.7); second run faster in 4/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | not run | KEPT, the commit to submit: 16bc175b plus the timestamp day-range fix (a day of 32 to 99 no longer lands on a date of the next month). No measurable speed difference, as expected; 60M 2,777 ms, MATCH, peak 2.61 GiB. — largest stage parse 476 ms (50%); container minus in-process total 100.1 ms; peak memory 919.2 MiB |
| 2b2cccd1ba1cb12bc5a074a899ce54c868205b86 (image built from the same Dockerfile, Cargo files and src/ as 7c91e15a58770f5367135449b8922aee3bfa92fb) [run 44938367859](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44938367859) | 2ff7868fc861348db3e2d918e550ec1eb664bcf1 | GitHub runner ubuntu26 20261116.202.1, INTEL(R) XEON(R) PLATINUM 8573C, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1165, 1158, 1170 (median 1165); client wall ms: 1372, 1345, 1373; pairs prior/this: 1,340.6/1,182.3 (BA), 1,327.8/1,174.7 (AB), 1,310.1/1,167.5 (AB), 1,299.6/1,197.6 (AB), 1,274.6/1,150.7 (BA), 1,279.7/1,144.4 (BA) | +138.9 ms (6/6 pairs won; medians 1,304.8 → 1,171.1); larger than every A/A difference seen here | median abs diff 11.5 ms, max 26.4 ms over 6 A/A pairs (signed A−B: -11.0, +26.4, +14.1, -5.0, -4.3, -12.1); second run faster in 1/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | seed 2: container 2,850.1 ms, exit 0, peak 2.62 GiB, MATCH /reference; against the baseline in the same job +359.8 ms (6/6 pairs, medians 3,158.1 → 2,784.7; A/A median abs 62.1 ms) [run 44938367859](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44938367859) | KEPT, new prior best and the commit to submit. Hypothesis: a lean event kernel for regular rows in front of the general one cuts the largest stage (parse 664 -> 531 ms on this runner); the 8-byte link forest and the writer hardening ride along. 60M 2,850 ms, MATCH, 360 ms faster than 2ff7868f (6/6 pairs). — largest stage parse 531 ms (50%); container minus in-process total 86.4 ms; peak memory 914.3 MiB |
| 2b2cccd1ba1cb12bc5a074a899ce54c868205b86 (image built from the same Dockerfile, Cargo files and src/ as 7c91e15a58770f5367135449b8922aee3bfa92fb) [run 44938367859](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44938367859) | 2ff7868fc861348db3e2d918e550ec1eb664bcf1 | GitHub runner ubuntu26 20261116.202.1, INTEL(R) XEON(R) PLATINUM 8573C, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1173, 1174, 1159 (median 1173); client wall ms: 1349, 1355, 1338; pairs prior/this: 1,378.3/1,158.0 (AB), 1,297.5/1,146.4 (AB), 1,297.7/1,151.2 (BA), 1,302.0/1,157.0 (BA), 1,298.6/1,158.7 (BA), 1,294.3/1,164.2 (AB) | +145.7 ms (6/6 pairs won; medians 1,298.2 → 1,157.5); larger than every A/A difference seen here | median abs diff 3.6 ms, max 6.7 ms over 6 A/A pairs (signed A−B: -0.6, +3.1, +5.6, -6.7, +4.1, -0.7); second run faster in 4/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | seed 2: container 2,850.1 ms, exit 0, peak 2.62 GiB, MATCH /reference; against the baseline in the same job +359.8 ms (6/6 pairs, medians 3,158.1 → 2,784.7; A/A median abs 62.1 ms) [run 44938367859](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44938367859) | KEPT, new prior best and the commit to submit. Hypothesis: a lean event kernel for regular rows in front of the general one cuts the largest stage (parse 664 -> 531 ms on this runner); the 8-byte link forest and the writer hardening ride along. 60M 2,850 ms, MATCH, 360 ms faster than 2ff7868f (6/6 pairs). — largest stage parse 536 ms (50%); container minus in-process total 95.4 ms; peak memory 928.5 MiB |
| 2b2cccd1ba1cb12bc5a074a899ce54c868205b86 (image built from the same Dockerfile, Cargo files and src/ as 7c91e15a58770f5367135449b8922aee3bfa92fb) [run 44938367859](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44938367859) | 2ff7868fc861348db3e2d918e550ec1eb664bcf1 | GitHub runner ubuntu26 20261116.202.1, INTEL(R) XEON(R) PLATINUM 8573C, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 5, 1,877,349,432 bytes in 107 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 1150, 1139, 1136 (median 1139); client wall ms: 1308, 1298, 1293; pairs prior/this: 1,282.8/1,137.1 (BA), 1,274.0/1,138.9 (BA), 1,273.8/1,136.9 (AB), 1,283.3/1,136.3 (BA), 1,274.9/1,164.5 (AB), 1,278.6/1,145.2 (AB) | +136.0 ms (6/6 pairs won; medians 1,276.8 → 1,138.0); larger than every A/A difference seen here | median abs diff 7.9 ms, max 24.2 ms over 6 A/A pairs (signed A−B: -24.2, +4.6, +9.2, +3.6, +11.2, -6.5); second run faster in 1/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 23/23 runs of this commit MATCH; prior best 7/7 MATCH | seed 2: container 2,850.1 ms, exit 0, peak 2.62 GiB, MATCH /reference; against the baseline in the same job +359.8 ms (6/6 pairs, medians 3,158.1 → 2,784.7; A/A median abs 62.1 ms) [run 44938367859](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44938367859) | KEPT, new prior best and the commit to submit. Hypothesis: a lean event kernel for regular rows in front of the general one cuts the largest stage (parse 664 -> 531 ms on this runner); the 8-byte link forest and the writer hardening ride along. 60M 2,850 ms, MATCH, 360 ms faster than 2ff7868f (6/6 pairs). — largest stage parse 523 ms (50%); container minus in-process total 95.1 ms; peak memory 913.4 MiB |
| 4567e9532f4019fbfd0e92908fd01cc49f24f67d [run 44940522202](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44940522202) | 2b2cccd1ba1cb12bc5a074a899ce54c868205b86 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 2, 1,901,148,800 bytes in 117 files | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 895, 883, 892 (median 892); client wall ms: 968, 959, 968; pairs prior/this: 945.6/887.9 (BA), 937.4/892.0 (AB), 944.5/888.7 (AB), 931.6/890.4 (AB), 943.3/890.4 (BA), 930.3/889.5 (BA) | +49.2 ms (6/6 pairs won; medians 940.4 → 889.9); larger than every A/A difference seen here | median abs diff 2.1 ms, max 28.6 ms over 6 A/A pairs (signed A−B: +2.1, -28.6, -1.7, -0.5, -2.1, -3.9); second run faster in 4/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | seed 2: container 2,235.6 ms, exit 0, peak 2.23 GiB, MATCH /reference; against the baseline in the same job +155.5 ms (6/6 pairs, medians 2,297.9 → 2,143.2; A/A median abs 6.5 ms) [run 44940522202](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44940522202) | KEPT, candidate for promotion (fresh-seed run 44941343462 pending). Hypothesis: index mode with the inline session path and the lean event kernel beats the general path as the default: the SS_STRUCT=0 variant of this image is 49 to 53 ms slower. 60M 2,236 ms, MATCH, peak 2.23 GiB, 155.5 ms faster than 2b2cccd1 (6/6 pairs). — largest stage parse 482 ms (60%); container minus in-process total 103.4 ms; peak memory 797.5 MiB |
| 4567e9532f4019fbfd0e92908fd01cc49f24f67d [run 44940522202](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44940522202) | 2b2cccd1ba1cb12bc5a074a899ce54c868205b86 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 3, 1,921,667,038 bytes in 152 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 910, 893, 898 (median 898); client wall ms: 982, 970, 972; pairs prior/this: 943.0/896.9 (AB), 943.7/892.7 (AB), 948.9/899.2 (BA), 948.2/902.8 (BA), 979.1/895.8 (BA), 949.6/896.2 (AB) | +50.4 ms (6/6 pairs won; medians 948.6 → 896.5); larger than every A/A difference seen here | median abs diff 4.0 ms, max 10.1 ms over 6 A/A pairs (signed A−B: -6.8, -1.6, -4.5, -0.7, +3.6, +10.1); second run faster in 5/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | seed 2: container 2,235.6 ms, exit 0, peak 2.23 GiB, MATCH /reference; against the baseline in the same job +155.5 ms (6/6 pairs, medians 2,297.9 → 2,143.2; A/A median abs 6.5 ms) [run 44940522202](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44940522202) | KEPT, candidate for promotion (fresh-seed run 44941343462 pending). Hypothesis: index mode with the inline session path and the lean event kernel beats the general path as the default: the SS_STRUCT=0 variant of this image is 49 to 53 ms slower. 60M 2,236 ms, MATCH, peak 2.23 GiB, 155.5 ms faster than 2b2cccd1 (6/6 pairs). — largest stage parse 484 ms (60%); container minus in-process total 106.8 ms; peak memory 794.1 MiB |
| 4567e9532f4019fbfd0e92908fd01cc49f24f67d [run 44940522202](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44940522202) | 2b2cccd1ba1cb12bc5a074a899ce54c868205b86 | GitHub runner ubuntu26 20261116.202.1, AMD EPYC 7763 64-Core Processor, 4 vCPU, 15.6 GiB RAM; `--cpus 4 --memory 13g --memory-swap 13g --cpuset-cpus 0-3`; native linux/amd64; Docker 29.4.2 | 20M events / 5M clicks, seed 5, 1,877,349,432 bytes in 107 files (holdout seed) | fresh container per run, `State.StartedAt`→`State.FinishedAt`; input page cache warm (100.0% resident before, 100.0% after) | 3 fresh runs, container ms: 887, 885, 895 (median 887); client wall ms: 962, 960, 972; pairs prior/this: 947.4/882.2 (BA), 944.6/883.7 (BA), 932.6/878.6 (AB), 937.3/879.8 (BA), 940.2/879.1 (AB), 936.2/882.4 (AB) | +59.3 ms (6/6 pairs won; medians 938.8 → 881.0); larger than every A/A difference seen here | median abs diff 2.6 ms, max 4.9 ms over 6 A/A pairs (signed A−B: -0.5, -0.8, -1.3, -3.9, -4.6, +4.9); second run faster in 4/6 | 36/36 fixtures; 43/43 extra fixtures; dev MATCH; 20M vs /reference: 29/29 runs of this commit MATCH; prior best 7/7 MATCH; variant 7/7 MATCH | seed 2: container 2,235.6 ms, exit 0, peak 2.23 GiB, MATCH /reference; against the baseline in the same job +155.5 ms (6/6 pairs, medians 2,297.9 → 2,143.2; A/A median abs 6.5 ms) [run 44940522202](https://github.com/geeeethubb/OSU_hackathon/actions/runs/44940522202) | KEPT, candidate for promotion (fresh-seed run 44941343462 pending). Hypothesis: index mode with the inline session path and the lean event kernel beats the general path as the default: the SS_STRUCT=0 variant of this image is 49 to 53 ms slower. 60M 2,236 ms, MATCH, peak 2.23 GiB, 155.5 ms faster than 2b2cccd1 (6/6 pairs). — largest stage parse 475 ms (61%); container minus in-process total 110.5 ms; peak memory 795.8 MiB |

## Sprint log (three-hour sprint from 2026-11-21 18:20 UTC)

Three kinds of numbers appear below and are never mixed: **laptop** (native Windows runs, no
container, 8 threads unless said otherwise), **CI** (fresh containers on 4-vCPU GitHub runners,
`State.StartedAt` to `State.FinishedAt`) and **official** (accepted practice scores; the only one
known so far is 1.38 s for `e4eef020`).

### 18:25 UTC

- **Best verified candidate:** `23b285da8246d00275b1ad191da0fffb8c5f81e4` (CI run 44930932775: 62.9 ms
  and 71.8 ms faster than `03b15f2f`, 6 of 6 pairs on both seeds; 36/36 fixtures, 43/43 extra
  fixtures, dev MATCH, 20M MATCH on both seeds).
- **In CI now:** `16bc175b` on branch `exp-general-default` (general path as the default, streaming
  Parquet writer, branch-light session pass; variant `SS_STRUCT=1`; 20M seeds 2, 3 and the new
  confirmation seed 5; 60M), and `8e92bfaa` (same sources, paired against the unchanged `db0c2e04` image).
- **Tried and learned:** index mode as pushed in `db0c2e04` only moved the dedup work and lost in CI
  (run 44932808661: `SS_STRUCT=0` 73.4 / 75.9 ms faster on the same image, 6/6 pairs each). The CI stage
  table says where: startup 9.8 -> 44.6 ms (key search), parse 358 -> 392 ms, dedup + indexed 232 -> 256 ms,
  sessions 179 -> 165 ms. The laptop had shown it as time-neutral: laptop stage times do not rank
  candidates, CI pairs do.
- **Current limiting work (CI, general path, EPYC 9V45, seed 2):** parse 358 ms, dedup 232 ms,
  sessions 179 ms, output 10 ms, startup 10 ms, container minus process 98 ms (a no-op container: 72 to 83 ms).
- **Three ways to remove work, ranked:**
  1. *Lay events out by event index and sessionize in place* (index mode with an inline session path).
     Disappears: the dedup hash table, one link lookup and one scatter per surviving event (542 CPU-ms of
     forwarding), per-partition grouping and per-person sorting, one lookup and scatter per click.
     Correct because every property it uses is checked at run time (keys verified on samples, IDs that do
     not fit go to the general path, user indexes must not decrease along the index order or the run starts
     over on the general path) and persons whose linked IDs span users stay on the general path whole.
     Next bottleneck: parse, then the click index. Disproof: same-image CI variant; drop it unless
     dedup + indexed + sessions falls well under the general path (411 ms on this host).
  2. *Decode several rows per step from tile-wide delimiter masks* (parser agent, `exp-parse`).
     Disappears: overlapping loads, the newline-to-next-row dependency (about 5 ns/row), mispredicted
     branches on the three timestamp forms. Correct by the row-for-row verify mode and the slow-path
     fallback. Next bottleneck: stores into partitions. Disproof: masks-only and decode-only builds on the
     same bytes; drop it if it loses on either 20M seed.
  3. *Identity links without the second table* (links agent, `exp-links`): count successful unions instead
     of scanning, and probe the forest itself. Disappears: one scan and one table build; small, because the
     build already overlaps the parse. Disproof: in-pipeline A/B at 20M and 60M.
- **Next experiment:** finish hypothesis 1 behind `SS_STRUCT=1` (cheap key search, click index, inline
  persons, edge runs joined across partitions, restart on the general path when the layout check fails).

### 18:35 UTC

- **Best verified candidate, and the commit to submit:** `16bc175b6e4fe947d03a94535a3baefecd6719a8`
  (now the pull request head). CI run 44934751037 (EPYC 7763, 4 vCPU), against `23b285da`:
  seed 2 saved 100.0 ms (6/6 pairs, medians 1,148.4 -> 1,047.8), seed 3 saved 98.9 ms (5/6), new
  confirmation seed 5 saved 109.9 ms (6/6); every saving is larger than every A/A difference of its seed.
  36/36 fixtures, 43/43 extra fixtures, dev MATCH, 29/29 runs MATCH per seed. 60M (seed 2): 3,148 ms,
  exit 0, peak 2.61 GiB, MATCH; 298.3 ms faster than `23b285da` (6/6 pairs). Run 44934755854 pairs the
  same sources against the unchanged `db0c2e04` image: 204.7 / 188.2 / 194.5 ms saved, 18/18 pairs.
- **Tried and learned:** the review's `SS_STRUCT=0` finding is incorporated: the general path is the
  default, and forward-only index mode on the same image loses by 136 to 140 ms (0/18 pairs).
  Stage table of `16bc175b` on that runner (seed 2): parse 482 ms, dedup 253 ms, sessions 180 ms
  (was 243 before the branch-light pass), output 14.5 ms (was 48), startup 15.5 ms, container minus
  process 98 ms.
- **Laptop, not yet in CI:** the inline index path (hypothesis 1) is written and exact (22 unit tests,
  36/36, 43/43, dev and 20M seeds 2 and 3 MATCH with `SS_VERIFY=1 SS_STRUCT=1`; 666,559 of the persons
  and 19.36M of 19.56M surviving events go through the inline path at seed 2). It is not faster yet:
  four alternating pairs on one binary, input pre-faulted, general path 482 to 537 ms in-process,
  index mode 549 to 598 ms. Where it loses: the click index costs 75 to 88 ms (a slow code-to-number
  conversion done twice per click), the index pass 127 to 139 ms (three passes over each record, a hash
  probe per user ID per user, whales processed one thread each after a serial copy), and its parse
  kernel is about 20 ms slower. The key search now costs about 1 ms instead of 35 ms in CI.
- **Current limiting work:** parse (half of the process time), then dedup forward, then sessions.
- **Next experiment:** make the inline path cheap in that order (one fast conversion per click, one
  fused pass over the slots, whales without the serial copy), then a same-image CI variant.

### 18:50 UTC

- **Correctness fix on top of the candidate:** the fast ISO timestamp path bounded month and day only by
  99 and then looked the date up by `month * 32 + day`, so a day of 32 to 99 landed on a real date of the
  next month (`2026-09-33T10:00:00Z` was read as October 1 and the row counted as valid). Found by the
  parser agent with a six-row probe; every commit since the date table was introduced has it, `16bc175b`
  included. The generator never writes such a day (its impossible dates are month 13, February 30,
  February 29 outside a leap year, hour 24 and second 60, none of which alias), so no generated dataset,
  fixture or score changes; a hand-written case could. Fixed by bounding month and day by 12 and 31
  (`d0caa70`). On the probe the old binary reports one user row too many and the fixed one matches
  `/reference`. Laptop checks on the fixed tree with `SS_VERIFY=1`: 22 unit tests, 36/36, 43/43, dev and
  20M seeds 2 and 3 MATCH.
- **Best verified candidate:** still `16bc175b` until CI has measured the fixed commit (this commit:
  same engine plus the fix, paired against `16bc175b`, with the 60M run). The fix changes two constants,
  so the expected paired difference is noise.
- **Next:** the inline index path continues on branch `exp-index-inline` (default path untouched).

### 18:58 UTC

- **Best verified candidate, and the commit to submit:** `2ff7868fc861348db3e2d918e550ec1eb664bcf1`
  (`16bc175b` plus the timestamp day-range fix). CI run 44936865073: 36/36, 43/43, dev MATCH, 23/23 runs
  MATCH on seeds 2, 3 and 5; no measurable speed difference from `16bc175b` (-2.8 / +3.1 / +3.0 ms);
  60M 2,777 ms, MATCH, peak 2.61 GiB.
- **In CI now, next candidate (this commit):** the general path with three agent branches merged:
  a lean event kernel for regular rows in front of the general one (parser agent; laptop, one thread,
  input pre-faulted: 28.2 -> 21.2 ns per event row) and eight differential tests of every fast kernel
  against the general parsers; link forest with 8-byte nodes at any size (links agent; laptop
  links_build 105 -> 90 ms at 20M, 420 -> 309 ms at 60M); writer errors instead of panics, output timing
  details and fewer allocations (output agent). Laptop checks on the merged tree with SS_VERIFY=1:
  37 unit tests, 36/36, 43/43, dev and 20M seeds 2 and 3 MATCH. Laptop, 4 alternating pairs, input
  pre-faulted, seed 2: parse stage 132 to 168 ms before, 101 to 117 ms after.
- **In CI now, experiment:** `146a333` on branch `exp-index-inline`: index mode with the inline session
  path as a same-image variant (SS_STRUCT=1). Laptop: dedup + indexed + sessions 125 to 130 ms against
  180 to 186 ms on the general path, but its parse stage is 45 to 60 ms slower here; whether that is
  real work or a small-page effect of this Windows laptop is the question for the Linux runner.
- **Tried and learned:** the reviewer's two link experiments are measured (links agent): counting
  merges instead of scanning buys nothing measurable (the scan was 2 ms of a 180 ms build at 60M); probing
  the forest itself halves the build but makes dedup about 45% slower (a hit needs a second dependent
  load), so it is not kept. A range-check audit of the fast parsers found no second bug.
- **Current limiting work:** parse, then dedup forward (one link lookup and one scatter per event).
- **Next experiment:** read both CI runs; if index mode wins on Linux, make it the default in a
  candidate of its own; in parallel the parser agent continues toward 15 ns per row (tile experiment).

### 19:07 UTC

- **Best verified candidate, and the commit to submit:** `2b2cccd1ba1cb12bc5a074a899ce54c868205b86`.
  CI run 44938367859 (Intel Xeon Platinum 8573C, 4 vCPU) against `2ff7868f`: 138.9 / 145.7 / 136.0 ms
  faster on seeds 2 / 3 / 5 (18/18 pairs, medians 1,304.8 -> 1,171.1 on seed 2); 36/36, 43/43, dev MATCH,
  23/23 runs MATCH per seed; 60M 2,850 ms, MATCH, peak 2.62 GiB, 360 ms faster (6/6). Parse stage on that
  runner 664 -> 531 ms (lean event kernel), dedup 300 ms, sessions 195 ms.
- **Architectural experiment, result:** index mode with the inline session path wins on Linux. CI run
  44938207474 (EPYC 9V45), same image: SS_STRUCT=1 was 52.1 / 46.2 / 59.1 ms faster than the general
  path on seeds 2 / 3 / 5, 18/18 pairs (A/A median 4 to 8 ms). Stage table, seed 2: general parse 305,
  dedup 186, sessions 103 ms; index mode parse 358, click index and odd events 20.5, index pass 154,
  general remainder 1.2 ms. So the work that disappeared is worth 115 ms there, and 53 ms of it is
  paid back in the parse (events +143 CPU-ms, clicks +65 CPU-ms over 4 threads): that run used the old
  event kernel in both modes. Laptop at 60M: output identical to the general path, 1,681 -> 1,452 ms.
- **In CI now (this commit):** the lean-kernel candidate plus index mode merged as an opt-in, variant
  SS_STRUCT=1: tells how index mode with the OLD event kernel compares with the general path on the
  lean one.
- **Current limiting work:** parse, in both modes. Index mode: its event kernel has no lean version
  yet (parser agent, in progress) and its click path converts a code back to a number per click (a new
  click-parser agent owns clicks); then the session kernel (half of the index pass; sessions agent).
- **Next experiment:** index mode as the default once its parse gap is closed, in a candidate of its own.

### 19:21 UTC

- **Best verified candidate, and the commit to submit:** `2b2cccd1ba1cb12bc5a074a899ce54c868205b86` (unchanged).
- **Measured since the last entry:** CI run 44939245591 on `fc97242` (EPYC 9V45): the default (general
  path, lean kernel, index mode merged as an opt-in) is within noise of `2b2cccd1` (-10.8 / +5.4 / +0.0 ms),
  and index mode with the OLD event kernel loses to the general path on the lean one by 28 to 31 ms
  (parse 380 against 243 ms; later stages 184 against 292 ms). The lean kernel alone took event parsing
  from 1,050 to 800 CPU-ms on that runner.
- **In CI now (this commit), the candidate that makes index mode the default:** lean event kernel in
  index mode too (parser agent, `f4be2dc`: 32.2 -> 25.2 ns per row on the laptop, general mode 22.1),
  the layout check on the key samples, summary writes prefetched on the link builder thread (laptop
  links_build in index mode 212 -> 145 ms). Variant SS_STRUCT=0 is the general path on the same image.
  Laptop checks with SS_VERIFY=1: 37 unit tests, 36/36, 43/43, dev and 20M seeds 2 and 3 MATCH (default),
  36/36, 43/43, dev and seed 2 MATCH (SS_STRUCT=0); 48 small generated datasets (1K to 200K events, 12 seeds)
  MATCH the reference pipeline with index mode engaged in each; 60M output equals the general path's.
- **Current limiting work:** parse in index mode (events about 3 ns per row behind the general kernel,
  clicks converted from code to number per click), then the session kernel inside the index pass.
- **Next experiment:** depends on this run: if index mode wins as the default, it becomes the commit
  to submit; the click-parser agent and the parser agent then close the parse gap.

## Official practice scores (reported by the team's submission watch, not measured here)

From 2026-11-22 the official page runs **shopping-sessions-5** (the republished generator: opaque
variable-width IDs, one in eight long hexadecimal). Scores on it are not comparable with the earlier ones.

| Commit | Official practice score | Receipt | Source |
|---|---:|---|---|
| `9735a63d039ee9ee843e0204f1d988bd5c822f41` (opaque IDs, general path) | **1.2745 s** (shopping-sessions-5) | BB-32BE0EA7 | relayed by the user 2026-11-22 about 03:20 UTC; current accepted baseline |
| `bad5c1b1bc3178508db7de5d7459197741497236` (candidate F4) | 5.6629 s (shopping-sessions-5) | not relayed | relayed by the user 2026-11-22 about 03:20 UTC: index mode, outside the new rules |
| `bad5c1b1bc3178508db7de5d7459197741497236` (candidate F4) | 0.4648 s (previous dataset) | BB-9044646E | relayed by the user 2026-11-22 about 01:13 UTC |
| `8dde2af12e542b0df94ba6d0be3da4f8660f896d` (candidate F3) | 0.4663 s | BB-9900D7A0 | relayed by the user 2026-11-22 about 00:45 UTC |
| `1033956062d3cd948462c664d822f2636bcf1153` (candidate F) | 0.5231 s | BB-337F162E | relayed by the user 2026-11-21 about 23:22 UTC; rank 37 when checked; the board's best was 0.2778 s |
| `2dae9298f4e1e0a5259eec54c400501b00fae26f` | 0.6389 s (shown earlier as 0.64 s) | not relayed | relayed by the user 2026-11-21 |
| `9cb9ad6e3cd0d75f160cecae8005d1c6cb8a7c4e` (candidate C) | 0.64 s | not relayed | same |
| `4567e9532f4019fbfd0e92908fd01cc49f24f67d` | 0.66 s | BB-898B1D4A | same |
| `2b2cccd1ba1cb12bc5a074a899ce54c868205b86` | 0.71 s | BB-3EDA1225 | observed in the team's signed-in submission history by the ChatGPT submission watch, relayed 2026-11-21 |
| `23b285da8246d00275b1ad191da0fffb8c5f81e4` | 0.84 s | not relayed | same |
| `e4eef020ce0bb9e005c329e57ea241c6e842dde8` | 1.38 s | not relayed | same |

CI container times and official scores are different measurements and are never mixed: for `2b2cccd1`
CI gave 1,165 ms on a Xeon 8573C runner and about 620 ms on an EPYC 9V45 runner (4 vCPUs each).

## Second sprint log (from 2026-11-21 19:30 UTC; brief: Shopping_Sessions_Next_Sprint.md)

### 19:31 UTC

```text
candidate_full_sha: 4567e9532f4019fbfd0e92908fd01cc49f24f67d  (index mode is the default)
baseline_full_sha: 2b2cccd1ba1cb12bc5a074a899ce54c868205b86  (official 0.71 s)
default_command_and_env: /pipeline, no environment variable (SS_STRUCT=0 selects the general path)
CPU_topology / limits / image_digest: GitHub runner, AMD EPYC 7763, 4 vCPU (2 cores x 2 threads), --cpus 4 --memory 13g; run 44940522202
input_size / seeds / cache_state: 20M events + 5M clicks, seeds 2, 3, 5; input page cache warm
36_official / extra / unit_differential / 1M_dev: 36/36 / 43/43 / 37 unit tests incl. 8 differential parser tests / MATCH
20M_raw_pairs / paired_median / A_A_variation: 18/18 pairs won; +49.2 / +50.4 / +59.3 ms (medians 940.4 -> 889.9 on seed 2); every saving larger than every A/A difference of its seed
same-image variant (SS_STRUCT=0, the general path): 49.3 / 53.0 / 53.3 ms slower than the default
60M_correctness / container_ms / peak_memory: MATCH /reference / 2,235.6 ms / 2.23 GiB; 155.5 ms faster than the baseline (6/6 pairs)
selected_route / fallback_count: index mode engaged on every generated dataset (stage "indexed" present); fixtures take the general path (no keys in hand-written data); no restart
changed_stage_and_hypothesis: dedup + sessions 431 ms -> click index 22 + index pass 257 + remainder 2 ms; parse 386 -> 482 ms (events +285, clicks +106 CPU-ms over 4 threads)
decision: keep; promote to "commit to submit" once the fresh-seed run (seeds 7 and 11, run 44941343462, same SHA) confirms
official_receipt_and_score: not yet submitted
```

- **The brief's P1 is already in this candidate** (lean event kernel in index mode, `f4be2dc`); its
  finding that index mode loses was about `fc97242`, where index mode still used the old event kernel.
- **Laptop checks behind the candidate:** SS_VERIFY=1 on all fixtures, dev, 20M seeds 2 and 3; 48 small
  generated datasets (1K to 200K events, 12 seeds) MATCH the reference pipeline with index mode engaged;
  three mutations of generated data MATCH the reference (two events with swapped user IDs: layout check
  fails, restart on the general path; upper-cased event and user IDs: handled as extras and general
  events; earlier-ingested copies naming another user: restart); 60M output equals the general
  path's, also after a forced restart (SS_FORCE_RESTART=1).
- **Current limiting work (CPU-ms over 4 threads, EPYC 7763, instrumented run):** event parse 1,554
  (general kernel: 1,268), click parse 372 (general: 266), session kernel inside the index pass 515,
  placing records 163, rest of the index pass 299.
- **Running experiments:** P2, the click path without the code-to-number round trip (click-parser agent);
  index-mode event kernel within 1 ns per row of the general one (parser agent); P3, a session kernel
  entry for events that are already in time order (sessions agent, verified at 233 -> 191 ns per person);
  partition sizes 2^13 and 2^15 as same-image variants (runs on branches exp-shift-13 and exp-shift-15).

### 19:41 UTC

- **Official:** the team's submission watch reports a new best of **0.66 s** (index mode as the default;
  commit as submitted from this PR's pointer, `4567e953`). Next submission possible in about one hour.
- **Confirmations of `4567e953` since the last entry:** runs 44940838388 and 44940841473 measured the same
  default path (plus a neutral parameter) against `2b2cccd1`: 63.9 / 62.0 / 58.8 ms and 48.8 / 55.0 / 59.1 ms
  faster, 36/36 pairs. Partition size: 2^13 is 6 to 12 ms slower than 2^14 (3/18 pairs), 2^15 is 3 to 6 ms
  faster (12/18 pairs).
- **In CI now (this commit), candidate A for the next submission, paired against `4567e953`:** index
  partitions of 2^15 events (variant: 2^14), compact link lookup table (links agent, `e33be28`), the
  sessions agent's `3a44f65` and `be4f180`, startup with the inputs mapped while the lookup tables load
  (laptop 14-18 -> 11-12.5 ms). Laptop checks with SS_VERIFY=1: 42 unit tests, 36/36, 43/43, dev and 20M
  seeds 2 and 3 MATCH; SS_STRUCT=0: 36/36, 43/43, dev and seed 2 MATCH; 24 small generated datasets MATCH.
- **Expected by 20:05 UTC from the agents, for candidate B:** index-mode click path without the
  code-to-number round trip, a faster index-mode event kernel, a session kernel entry for events in time order.

### 19:47 UTC

- **Correction:** the previous entries and `bench/BEST_SHA` in commit `b32d77f` named the index-default
  commit with a wrong full hash (typed from memory; the tail was invalid). The officially accepted
  0.66 s commit is `4567e9532f4019fbfd0e92908fd01cc49f24f67d`; the ledger and `bench/BEST_SHA` now carry
  it, and every 40-character hash in the ledger was checked against the repository. Because of the
  invalid baseline, the CI run of `b32d77f` has no prior-versus-candidate pairs: this commit repeats
  the comparison.
- **Confirmation of `4567e953` on fresh seeds (run 44941343462, same SHA, seeds 7 and 11):** 51.9 and
  59.0 ms faster than `2b2cccd1`, 12/12 pairs; 36/36, 43/43, dev MATCH, 29/29 runs MATCH per seed.
- **In CI now (this commit), paired against the 0.66 s commit:** candidate A of the last entry plus two
  small index-pass changes (click lists counted by all threads; "does this user have extra events"
  answered by a bit test instead of a hash probe per run). Laptop checks with SS_VERIFY=1: 42 unit
  tests, 36/36, 43/43, dev and 20M seeds 2 and 3 MATCH; 16 small generated datasets and three mutated
  ones MATCH the reference pipeline.

### 19:57 UTC

- **In CI now (this commit), candidate B for the next submission, paired against the 0.66 s commit
  `4567e9532f4019fbfd0e92908fd01cc49f24f67d`:** candidate A plus three agent steps, each verified by its
  author and again on the merged tree:
  - index-mode click path: the kernel reads the user number from the text and writes into the
    per-user-range list (click agent, `4989f58`; laptop, one thread: 29.9 -> 21.2 ns per click row);
  - lean event kernel v2: row end from the decode, range compares for the canonical-ID checks, a store
    loop without a call on the common path (parser agent, `82182bb`; laptop, one thread, index mode:
    25.3 -> 23.4 ns per row), with a whole-file differential test of the store loops in both modes;
  - sessions from events in time order in one pass (sessions agent, `5cb6ad0`; about 20% of the
    per-person kernel in its replay), called from the index pass.
- **Laptop checks on the merged tree:** 43 unit tests; with SS_VERIFY=1 and without it: 36/36, 43/43, dev
  and 20M seed 2 MATCH (seed 3 too, both ways); 24 small generated datasets and three mutated ones MATCH
  the reference pipeline in the scored configuration.

### 20:11 UTC

```text
candidate_full_sha: 75b2600374845cd51890684f9fcf12b008a968e8  (candidate B)
baseline_full_sha: 4567e9532f4019fbfd0e92908fd01cc49f24f67d  (official 0.66 s)
default_command_and_env: /pipeline, no environment variable
CPU_topology / limits: GitHub runner, AMD EPYC 9V74, 4 vCPU, --cpus 4 --memory 13g; run 44943649309
input_size / seeds / cache_state: 20M events + 5M clicks, seeds 2, 3, 5; input page cache warm
36_official / extra / unit_differential / 1M_dev: 36/36 / 43/43 / 43 unit tests incl. 9 differential parser tests / MATCH
20M_raw_pairs / paired_median / A_A_variation: 14/18 pairs won on a noisy runner; +68.8 / +71.1 / +57.3 ms (medians 762.4 -> 694.9, 770.3 -> 698.0, 792.9 -> 734.2); seed 3 larger than every A/A difference, seeds 2 and 5 larger than the median A/A difference but inside its range
60M_correctness / container_ms / peak_memory: MATCH /reference / 2,437.5 ms / 2.08 GiB; 241.8 ms faster than the baseline, 6/6 pairs
selected_route / fallback_count: index mode on every generated dataset; no restart
changed_stage_and_hypothesis: parse 413 -> 365 ms (events 1,327 -> 1,189, clicks 320 -> 270 CPU-ms), index pass 211 -> 197 ms (kernel in one ordered pass); in-process 676 -> 614 ms
decision: keep; commit to submit for the next window unless the 2^14 variant below measures better
official_receipt_and_score: not yet submitted
```

- **Candidate A alone (`4641345`, run 44942802240, Xeon 6975P-C) was neutral** against the 0.66 s commit
  (+1.1 / -0.6 / +3.5 ms; 60M -27.5 ms paired), as the audit notes; in the same run 2^14-event partitions
  beat 2^15 by 7.3 / 12.3 / 16.8 ms (17/18 pairs), the opposite of the EPYC 7763 result. The scorer is a
  Xeon, so the default goes back to 2^14.
- **In CI now (this commit):** candidate B with 2^14-event partitions as the default (variant: 2^15) and
  the purchase plane written only for purchases (the audit's suggestion; every reader is gated by the
  winner's purchase bit). Laptop checks in the scored configuration: 43 unit tests, 36/36, 43/43, dev
  and 20M seeds 2 and 3 MATCH.
- **Sessions agent, target 2 (attribution overhead):** three exact variants measured, none faster
  (within 0.5%); dropped. The remaining kernel cost is spread over five short per-person loops.

### 20:20 UTC

```text
candidate_full_sha: 2dae9298f4e1e0a5259eec54c400501b00fae26f  (candidate B2; the commit to submit)
baseline_full_sha: 4567e9532f4019fbfd0e92908fd01cc49f24f67d  (official 0.66 s, receipt BB-898B1D4A)
default_command_and_env: /pipeline, no environment variable
CPU_topology / limits: GitHub runner, AMD EPYC 7763, 4 vCPU (2 cores x 2 threads), --cpus 4 --memory 13g; run 44944919924
input_size / seeds / cache_state: 20M events + 5M clicks, seeds 2, 3, 5; input page cache warm
36_official / extra / unit_differential / 1M_dev: 36/36 / 43/43 / 43 unit tests incl. 9 differential parser tests / MATCH
20M_raw_pairs / paired_median / A_A_variation: 18/18 pairs won; +74.0 / +77.1 / +80.6 ms (medians 892.2 -> 818.2, 906.5 -> 830.4, 889.5 -> 808.0); each larger than every A/A difference of its seed
same-image variant (SS_INDEX_SHIFT=15): +5.2 / -0.7 / +5.8 ms: no clear difference on this CPU (2^14 was 7 to 17 ms better on a Xeon)
60M_correctness / container_ms / peak_memory: MATCH /reference / 2,055.2 ms / 2.12 GiB; 214.0 ms faster than the baseline, 6/6 pairs
selected_route / fallback_count: index mode on every generated dataset; no restart
changed_stage_and_hypothesis: fewer instructions in the event and click parsers, sessions in one ordered pass, purchase plane written only for purchases
decision: keep; named as the commit to submit in the PR at 20:20 UTC
official_receipt_and_score: not yet submitted
```

- **Correctness agent, on `4567e953` in the default (index) mode, exact comparison of both tables with the
  reference pipeline (and its own independent Python reference):** 56 mutated datasets x 4 environments,
  13 further mutations, 61 "handful of odd rows" datasets, 406 single-edit inputs, 36 plain 1K datasets,
  136 extra fixtures: 0 disagreements. Index mode stayed engaged or backed out to the general path as
  designed (restarts on moved events, cross-user copies and swapped event IDs). The same lists are being
  re-run on this commit.
- **`bench/build_prior.sh` now fails the job** when the baseline cannot be found or built.

### 20:34 UTC

- **Architectural experiment, rejected on its kill test: partition-owned (direct) winner construction.**
  Hypothesis: letting the parsers hand each record to the owner of its index partition, which keeps the
  winning copy directly, removes the intermediate partition lists and the placement pass. Smallest
  experiment that could disprove it: `bench/experiments/scatter.rs` on branch `exp-scatter` moves 20M
  (and 60M) records that arrive in random index order, plus 3% second copies, without any parsing,
  (a) the engine's way: per-thread block lists by 2^14-index partition, then placement in a slot table
  that stays in the cache, then a read in order; (b) the best case of an owner design: no queues, no
  batches, every producer writes straight into the final slot of one array (one atomic maximum per
  record, huge pages), then a read in order. Linux runner (run 44946867096, EPYC 9V74, 4 vCPU, huge
  pages granted on request): 20M, 4 threads: (a) 104 to 125 ms, (b) 229 to 276 ms; 20M, 2 threads:
  139 to 178 against 223 to 252 ms; 60M, 4 threads: 432 to 443 against 747 to 761 ms. Laptop, 8 threads:
  41 to 118 against 108 to 168 ms. The direct scheme pays a cache miss per record where the lists scheme
  pays a sequential write and an in-cache placement; queues and a completion protocol could only add to
  it. Decision: reject; keep lists and placement.
- **What the same numbers suggest instead:** in (a), writing the lists is 82 of the 104 ms: about 16 ns per
  record of thread time, mostly the read-for-ownership of fresh list memory. Next experiment in the same
  harness: write-combining buffers flushed with non-temporal stores for the partition lists.

### 20:37 UTC

- **Write-combining lists with non-temporal stores: rejected in the same harness.** A 64-byte buffer per
  partition, flushed with non-temporal stores, against plain appends to the block lists. Linux runner
  (run 44947126383, Xeon 6975P-C, 4 vCPU): 20M, 4 threads: 108 to 111 ms against 111 to 141 ms (steady
  rounds 108 against 111 to 114: about 3%); 20M, 2 threads: 157 to 159 against 149 to 155 ms (worse);
  60M, 4 threads: 402 to 465 against 415 to 451 ms (no difference). The direct scheme lost on this CPU as
  well (199 to 222 ms at 20M, 755 to 777 ms at 60M). Decision: reject; the partition lists stay as they are.
- **Read of both experiments:** the movement of event records (lists, placement, ordered read) is close
  to its efficient form for records of this size; the remaining cost is computing the records (parsing)
  and the per-person session work, plus the link builder thread once the parse gets shorter.

### 20:55 UTC

- **Official:** `2dae9298f4e1e0a5259eec54c400501b00fae26f` scored **0.64 s** (relayed by the user; the
  record on the board is 0.29 s). CI had shown 8.5% against the 0.66 s commit on 4 vCPUs; the 8-thread
  scorer gave 3%.
- **Reading, to be tested by the next submission:** at 8 threads the parse workers are no longer the
  last to finish in the parse stage; the single-threaded link builder is. Laptop, 20M seed 2, input
  pre-faulted, index mode: parse workers 687 / 375 / 243 / 122 ms at 1 / 2 / 4 / 8 threads, link builder
  (table plus per-user summary) 85 / 89 / 117 / 125 ms. A 4-vCPU runner cannot show this (parse 365 ms,
  builder 146 ms there), which is why CI over-predicted the official gain of parser work.
- **In CI now (this commit), candidate C:** the per-user summary on its own thread (`bf0f767`), the
  lookup tables, file mapping and link parse overlapped at startup so the builder starts earlier
  (`1d6110e`), and the parser agent's lean kernel in two halves run out of step with its own store
  loop (`c890351`; 3 to 5% of event parsing on the laptop). Laptop checks in the scored configuration:
  43 unit tests, 36/36, 43/43, dev and 20M seeds 2 and 3 MATCH. On 4 vCPUs the expected gain is small;
  the point of submitting it is the 8-thread answer.
- **If the reading holds, next:** links resolved per user range in parallel in index mode (pairs inside
  one user index never need the global union-find; only the few pairs that cross users do), and the
  click index built before the builder is waited for.

### 21:11 UTC

- **Official:** candidate C (`9cb9ad6e3cd0d75f160cecae8005d1c6cb8a7c4e`) scored **0.64 s**, the same as
  `2dae9298` (relayed by the user). CI run 44948826448 (Xeon 8573C) had it at -1.4 / -13.3 / -0.8 ms
  against `2dae9298` at 20M (within noise, 5/18 pairs) and 59.8 ms slower at 60M; all outputs MATCH.
- **Hypothesis refuted:** "at 8 threads the link builder is the last to finish". The laptop's 8 threads
  are 8 cores; the scorer's 8 hyperthreads are 4 cores, so its parse is about twice as fast as on a
  4-vCPU runner, not four times, and the builder stays behind it. The summary thread and the overlapped
  startup stay in the tree (they cost nothing), but they were not the limit.
- **Model from the official scores so far** (0.71, 0.66, 0.64 s against 940, 890, 818 ms on the EPYC 7763
  runner): official is about 0.17 s plus 0.57 of the CI container time; in other words a fixed part of
  roughly 0.17 to 0.25 s (container lifecycle plus serial steps) and about half of the CI parallel work.
  A CI gain of 74 ms arrived as 0.02 s.
- **Checked and discarded:** rows inside an events file are not in event-index order (the index goes
  back on half of the rows in every file looked at), so there is no order to use at parse time.
- **Open experiments:** branch `cand-t1` (candidate C2 without the parser step `c890351`, to see whether
  that step is the small Intel regression); candidate C2 (`a67dad3`, click index before the builder is
  waited for); 2^15-event partitions at the 60M scale only (branch `exp-shift60`).
- **Next, by expected size:** session work fused into the winner scan (sessions agent), fewer
  instructions per row in the event and click parsers (two agents), links resolved per user range in
  parallel (links agent), fixed costs (exit without teardown work, output overlapped with the index pass).

### 21:30 UTC

- **Isolation of the last candidates (EPYC 7763, 4 vCPU, against `2dae9298`):** candidate C2 (`a67dad3`,
  run 44949386535) lost every pair: -17.0 / -9.9 / -10.8 ms (0/18), 60M -19.0 ms. The same tree without
  the parser step `c890351` (branch `cand-t1`, run 44950017407): +8.5 / +1.3 / +4.2 ms (12/18 pairs;
  inside the A/A range, so "not slower, gain not demonstrated"). The parser step (lean kernel in two
  halves run out of step, own store loop) is therefore a measured regression of about 15 to 20 ms on this
  CPU although the laptop liked it; it is reverted on the PR branch (`0d6ac6a`). 2^15-event partitions
  at the 60M scale only (`exp-shift60`, run 44947867416): +4.6 ms at 60M, 4/6 pairs: no effect, dropped.
- **Elapsed critical path, measured** (run 44950017407, one instrumented run, seed 2; AMD EPYC 7763,
  4 logical CPUs = 2 cores x 2 threads, --cpus 4; these are elapsed times, not summed worker times):

  | step | elapsed ms | what finishes last / what waits |
  |---|---:|---|
  | container before and after the process | 104.3 | a no-op container on this runner: 73.8, so 30.5 is this process starting and exiting |
  | startup (manifest, event files mapped, keys) | 3.9 | serial |
  | link parse, lookup tables loaded beside it | 15.8 | all threads on links; one thread on the tables |
  | parse stage | 457.6 | parse workers finish at 436.7; the link builder had finished at 208.7 and its summary thread 82 ms later: nobody waits for links; then the click index, 20.9, all threads |
  | odd events, complex clicks | 2.5 | serial, small |
  | index pass | 228.7 | partitions done at 217.9 (all threads); joining edge runs 0.2 (serial); edge runs incl. the five whales 10.5 (one thread per whale) |
  | general-path remainder | 2.0 | all threads |
  | output | 13.5 | all threads |
  | in-process total | 723.9 | |

  Parsing finishes last in its stage, sessions (the index pass) are the second block, links and output
  are not on the critical path. Summed worker time for scale only: event parse 1,435, click parse 309,
  index pass 867 (of which placing 131 and the session kernel 456), link builder 209 + 82 CPU-ms.
- **Inferred, not measured:** the fit "official is about 0.17 s plus 0.57 of the CI time" rests on three
  official scores with two decimals each; what part of the 0.17 s is container lifecycle on the scorer
  is unknown. Not yet demonstrated: any 8-thread effect (the scorer's 4 cores x 2 threads cannot be
  reproduced on a 2-core runner or on the 8-core laptop).
- **Audit asked for: decode after dedup instead of before.** What must be decided at once for every row:
  whether the line is malformed (field count, quote balance, empty event_id), because a malformed copy
  never takes part in dedup; the event ID (which copy group) and ingest_ts (which copy wins). Everything
  else (user, ts, type, product, price, the validity verdict) is needed only for the winning copy. Maximum
  removable work: the non-winning rows only, i.e. the extra copies, 2 to 4% of events with 1 to 3 copies,
  about 3 to 6% of rows, times the roughly 60% of a row's decode that could wait: 2 to 4% of the event
  parse (about 8 to 15 ms of the 437 ms above) before paying for a second visit to each winning row,
  which is a random access into the input (two cache lines per row) plus a second structure scan of that
  row. Net effect not demonstrated and expected negative; no prototype.
- **Buffered partition ownership:** the atomic direct-scatter result stands only against that
  implementation. For a buffered owner design the test to pass first is the same harness with per-owner
  batches; its upper bound is the placing pass plus the list writes it would replace, 131 CPU-ms of
  placing and a part of the parse stores, about 35 to 60 ms elapsed on this runner. Not started.
- **In CI now (this commit), candidate D against `2dae9298`:** the structural changes of cand-t1 plus
  the links agent's `build_indexed` (`3bf5146`): links between two IDs of one user are resolved per user
  in shards with no shared table, only users touched by other links go through the single table, and
  the per-user summary comes out of the same pass (no summary thread). It runs on one thread beside the
  parse. Laptop: link builder 100-130 + 25 ms before, 55-98 ms after (noisy machine). Laptop checks in
  the scored configuration: 45 unit tests, 36/36, 43/43, dev, 20M seeds 2 and 3 and 60M MATCH, 24 small
  generated datasets MATCH the reference pipeline.

### 21:40 UTC

```text
candidate_full_sha: bb32616f718774d07b42c49d1a939c3ece637835  (candidate D; the commit to submit)
baseline_full_sha: 2dae9298f4e1e0a5259eec54c400501b00fae26f  (official 0.64 s)
default_command_and_env: /pipeline, no environment variable
CPU_topology / limits: GitHub runner, Intel Xeon Platinum 8573C, 4 logical CPUs (2 cores x 2 threads), --cpus 4 --memory 13g; run 44951847559
input_size / seeds / cache_state: 20M events + 5M clicks, seeds 2, 3, 5; input page cache warm
36_official / extra / unit_differential / 1M_dev: 36/36 / 43/43 / 45 unit tests / MATCH
20M_raw_pairs / paired_median / A_A_variation: 17/18 pairs won; +36.5 / +37.2 / +39.5 ms (medians 984.8 -> 951.3, 993.4 -> 954.9, 990.2 -> 948.6); each larger than every A/A difference of its seed
60M_correctness / container_ms / peak_memory: MATCH /reference / 2,428.1 ms / 2.15 GiB; 74.0 ms faster than the baseline, 6/6 pairs
selected_route / fallback_count: index mode on every generated dataset; no restart
changed_stage_and_hypothesis: link builder beside the parse 209 + 82 CPU-ms -> 101 ms on one thread (different runners, indicative only); fewer CPU-ms taken from the parse workers
decision: keep; named as the commit to submit in the PR at 21:40 UTC
official_receipt_and_score: not yet submitted
```

- **Experiment in CI:** candidate E (`5704266f417ff89b1d4d2627eb3188e706eb4e4e`: D plus the lean click
  kernel, variant `SS_CLICK_KERNEL=general`); kill test 2 on branch `exp-scatter` (two-level
  partitioning and one stream per owner against the lists). On the laptop both alternatives lose
  (lists 42 to 115 ms, two-level 91 to 142 ms, owner streams 232 to 267 ms at 20M, 8 threads), but the
  laptop has no huge pages; the Linux run decides.

### 21:40 UTC

- **Kill test 2, rejected (success line set beforehand: at least 25% less than "lists" at 20M and 60M):**
  `bench/experiments/scatter2.rs` on branch `exp-scatter`, Linux runner (run 44952604559, EPYC 9V74,
  4 logical CPUs, huge pages on request), elapsed ms, produce + rest = total, steady rounds:
  | records | lists (today) | two levels (2^18-index streams, then a cache-resident second scatter) | one stream per owner, prefetched plain stores into one slot array |
  |---|---|---|---|
  | 20M | 67 + 23 = 90 | 64-78 + 54-71 = 118-158 | 69-76 + 198-203 = 268-274 |
  | 60M | 286-344 + 71 = 358-415 | 187-204 + 169-171 = 355-374 | 206-212 + 611-623 = 817-830 |
  Fewer streams do not make the producers faster at 20M (their cost is the fresh memory they write, not
  the number of streams); at 60M they do (about 300 -> 190 ms) but the second scatter gives all of it
  back. An exclusive owner with plain stores and prefetch is two to three times slower than placing in a
  cache-resident table: the cache miss per record stays. With kill test 1 (atomic direct scatter, write
  combining) this closes the question for records of this size: lists by 2^14-index partition and
  in-cache placement stay.

### 21:44 UTC

```text
candidate_full_sha: 5704266f417ff89b1d4d2627eb3188e706eb4e4e  (candidate E; the commit to submit)
baseline_full_sha: 2dae9298f4e1e0a5259eec54c400501b00fae26f  (official 0.64 s)
default_command_and_env: /pipeline, no environment variable
CPU_topology / limits: GitHub runner, Intel Xeon 6975P-C, 4 logical CPUs (2 cores x 2 threads), --cpus 4 --memory 13g; run 44952308729
input_size / seeds / cache_state: 20M events + 5M clicks, seeds 2, 3, 5; input page cache warm
36_official / extra / unit_differential / 1M_dev: 36/36 / 43/43 / 45 unit tests / MATCH
20M_raw_pairs / paired_median / A_A_variation: 18/18 pairs won; +56.3 / +67.1 / +55.9 ms (medians 869.9 -> 813.5, 880.1 -> 824.1, 859.8 -> 804.3); each larger than every A/A difference of its seed
same-image variant (SS_CLICK_KERNEL=general): 18.2 / 21.5 / 16.2 ms slower, 0/18 pairs: the lean click kernel's own share
60M_correctness / container_ms / peak_memory: MATCH /reference / 2,426.6 ms / see run; 176.6 ms faster than the baseline, 6/6 pairs
selected_route / fallback_count: index mode on every generated dataset; no restart
changed_stage_and_hypothesis: candidate D (links per user in shards, overlapped startup) plus fewer instructions per click row
decision: keep; named as the commit to submit in the PR at 21:45 UTC; bench/BEST_SHA moves to it
official_receipt_and_score: not yet submitted
```

### 21:46 UTC

- **Calibration table (data, not a model).** Official scores are relayed by the submission watch and
  have two decimals; "paired CI change" is the median paired container-time difference against the
  previously scored commit, summed over the intermediate candidates where there were several, each
  measured on 4-logical-CPU GitHub runners (2 cores x 2 threads) whose CPU model differs from run to run.
  | scored commit | official | paired CI change since the previous scored commit | what changed |
  |---|---:|---|---|
  | `e4eef020` | 1.38 s | n/a | first AVX2 row parser |
  | `23b285da` | 0.84 s | about -630 ms (three steps: -380, -177, -63 on seed 2; EPYC runners) | leaner parser, prefetching dedup, flat union-find, session pass without sorts, huge pages |
  | `2b2cccd1` | 0.71 s | about -240 ms (-100 on an EPYC 7763, -139 on a Xeon 8573C) | streaming Parquet writer, branch-light sessions, lean event kernel |
  | `4567e953` | 0.66 s | -49 to -59 ms (EPYC 7763; -52 / -59 on fresh seeds) | index mode as the default |
  | `2dae9298` | 0.64 s | -74 to -81 ms (EPYC 7763) | click numbers from text, lean kernel v2, ordered sessions |
  | `9cb9ad6e` | 0.64 s | +1 to +13 ms slower (Xeon 8573C) | summary thread, overlapped startup, parser step later reverted |
  Official change per paired CI millisecond in these five steps: 0.86, 0.54, about 0.9, about 0.26, and
  no change for no change. The spread is as large as the values: the scores are rounded to 0.01 s, the
  scorer's run-to-run variation is unknown, and the CI runners differ. What the table supports: CI wins
  have always arrived as official wins of the same sign and usually of about half the size or more; it
  does not support a fixed-cost estimate for the scorer, and none is assumed from here on.
- **Unknown:** the scorer's container lifecycle cost, its CPU model and SMT behaviour, and how much its
  runs vary. Candidate E has not been scored yet; CI says it is 56 to 67 ms faster than `2dae9298`.

### 21:52 UTC

- **Fast exit, kept (small):** `b89089509140f293879be2130fc308dbaec38004` = candidate E plus leaving the
  process as soon as both tables are written. Run 44953146558 (EPYC 9V45): against candidate E +1.1 /
  +6.7 / +2.9 ms (12/18 pairs, inside the A/A range); the same image with the ordinary return
  (`SS_EXIT=drop`) is 3.5 / 4.9 / 4.2 ms slower (lost 14/18 pairs). About 4 ms; candidate E stays the
  named commit until something larger is confirmed on top of it.
- **In CI:** the sessions agent's first step alone (`9a2b760` on branch `exp-ci-sessions-1`: sessions
  built while the slots are read, no timestamp vector), paired against candidate E; and a dispatch run of
  the pipeline head with `NCPU=2` as the variant on seeds 2 and 3, to measure what the second hardware
  thread per core is worth on a 2-core runner (SMT sensitivity; the scorer is 4 cores x 2 threads).
- **Agents:** parser (one assembly block per event row on the in-step structure, about 240 instructions
  per row counted by hand against 351 today; its three assembly commits on the out-of-step structure are
  dropped with the revert), sessions (batch kernel and slot tags, steps 2 and 3), links (bucketing and
  packed summary of the sharded build; equality with the single build confirmed on 20M seed 3 and 60M),
  clicks (table-driven kernel), output (resumed: row groups drained during the index pass),
  correctness (mutation sets on the named candidates; 0 disagreements so far).

### 22:14 UTC

- **Sessions step 1, confirmed alone:** `9a2b760d31cfd42abe20b7b55d1fb9d5b82bcfb1` (a direct run's main
  person gets its sessions while its slots are read; no timestamp vector) against candidate E, run
  44953779232, EPYC 7763: **+18.8 / +13.2 / +17.9 ms, 18/18 pairs** (A/A medians -3.9 / +4.1 / -2.6).
  Its base already holds the fast exit (about 4 ms), so the step itself is worth 10 to 15 ms. Instrumented
  run, seed 2: index pass 229.4 -> 219.0 ms elapsed; summed worker time in the session kernel 450.8 ->
  410.3 CPU-ms, placing unchanged (131.8 -> 129.9). Keep; it enters the next candidate.
- **What the second hardware thread is worth (measured, 2-core runner):** run 44953815755, EPYC 9V45,
  same image with `NCPU=2` against the default 4 threads: 158.0 / 162.4 ms slower, 12/12 pairs (medians
  596.7 -> 759.5 and 625.7 -> 785.5). Two threads on two cores take about 27% longer than four threads on
  the same two cores. Inference, not measured: the scorer's eight threads on four cores gain a similar
  share from their second threads; nothing in this run says how the scorer's cores compare with these.
- **Elapsed critical path, candidate E + fast exit** (run 44953815755, EPYC 9V45, 2 cores x 2 threads,
  seed 2, one instrumented run): container 618.8 ms; process 524.7 = startup 2.1, link parse 14.5, parse
  stage 319.3 (workers 299.4, click index 19.6), odd events 2.4, index pass 173.0, general remainder 3.8,
  output about 13. Summed worker time for scale: event parse 1,034, click parse 161 CPU-ms.
- **New measured item: the end of the process.** Container time minus the process's own total, minus
  what a no-op container takes on the same runner: 94.2 - 60.2 = 34 ms (9V45), 73.2 - 56.5 = 17 ms and
  77.7 - 53.4 = 24 ms (run 44953146558), 100.2 - 74.8 = 25 ms (EPYC 7763). So 17 to 34 ms pass outside the
  stages the program times. Inferred cause, not yet demonstrated: the kernel takes the input mappings
  apart when the process exits (1.9 GB = 464,000 page-table entries, one thread, before the exit is
  reported). Experiment `9a9c467` on branch `exp-ci-release-1`: two helper threads drop those entries
  (`MADV_DONTNEED`; the bytes stay readable) as soon as the parse workers are done; `SS_RELEASE=0` keeps
  the old behaviour as the same-image variant. Success line set before the result: the variant at least
  10 ms slower on every seed with 5 of 6 pairs; rejected below 5 ms or inside the A/A range.
- **In CI, each alone against candidate E:** click agent's table-driven kernel `ff5feff` (branch
  `exp-ci-clicks-1`, variant `SS_CLICK_KERNEL=lean`); sessions step 2, persons in batches, `093ad2e`
  (`exp-ci-sessions-2`) and the same with its switch `2d58de3` (`exp-ci-sessions-2b`, variant
  `SS_BATCH=0`).
- **Startup edge cases (local, exact):** datasets with no link rows, no click rows, or neither, at 1, 4
  and 8 threads: all MATCH `/reference`, index mode chosen each time.

### 23:05 UTC

Round result: seven steps measured alone in CI, six kept, and candidate F assembled from them. Every
number is a median of six shuffled AB/BA pairs per seed on one runner; "x/18" counts pairs won.
(Reading note: in the same-image variant block of the CI report A is the default and B the variant. I
read one batch of them with the sign reversed at 22:20 and corrected it at 22:36; the lines below are
checked against the per-run medians.)

| Step (commit, owner) | Against | Paired result | Same-image variant | Decision |
|---|---|---|---|---|
| Event kernel in assembly, in step (`5db6322`, parser) | candidate E | **+82.4 / +71.3 / +69.4 ms**, 18/18, Xeon 6975P-C (run 44956742090); +31.1 / +31.5 / +24.1 ms, 18/18, EPYC 9V45 (run 44956768241) | `SS_KERNEL=plain` 23.3 / 23.9 / 31.7 ms slower, 18/18 | keep |
| Sessions while the slots are read (`9a2b760`, sessions step 1) | candidate E | +18.8 / +13.2 / +17.9 ms, 18/18, EPYC 7763 (run 44953779232) | none | keep |
| Persons in batches (`093ad2e` / `2d58de3`, step 2) | candidate E | +7.3 / +10.5 / +15.0 (15/18) and +15.2 / +9.3 / +16.2 (17/18), EPYC 9V45: not distinguishable from step 1 alone | `SS_BATCH=0` on `2d58de3`: 5.6 ms faster (3/6) and 12.8 ms slower (5/6), Xeon 6973P-C: neutral | kept only because step 3 builds on it |
| Slot tags instead of the bitmap, leaner scan (`cfc7799`, step 3) | its parent `2d58de3` | +29.4 / +7.0 / +11.6 ms, 17/18, Xeon 6975P-C (run 44956028362); +15.5 / +13.0 / +14.7 ms, 16/18 (run 44956292915) | `SS_BATCH=0` 20.3 / 23.2 / 20.7 ms slower, 17/18: with the tag scan the batch pays | keep |
| Table-driven click kernel (`ff5feff`, clicks) | candidate E | +9.5 / +13.1 / +9.4 ms, 18/18, EPYC 7763 (run 44956303415; a first run on a noisy runner gave +12.2 / +2.2 / +7.7) | `SS_CLICK_KERNEL=lean` 5.2 / 6.4 / 5.3 ms slower, 13/18 | keep (about 5 ms) |
| Links: packed summaries, quick code-to-number (`f080503`, links) | candidate E | +9.3 / +6.1 / +8.8 ms, 14/18 (run 44956751344); builder 146 -> 65 CPU-ms, parse workers done 1.7 ms earlier | none | keep (about 3 to 5 ms) |
| user_daily written while its rows are produced (`bec7dfd`, output) | candidate E | +21.0 / +14.9 / +30.4 ms, 18/18, **Xeon Platinum 8370C** (run 44957382595) | `SS_STREAM=0` 14.2 / 12.2 / 17.3 ms slower, 17/18 | keep |
| Input mappings let go beside the index pass (`9a9c467`, lead) | candidate E | +7.7 / +7.4 / +13.8, +14.2 / +8.9 / +24.4, +11.4 / +10.4 / +11.2 ms on nine seeds (runs 44955536301, 44957004270, 44957028968) | `SS_RELEASE=0` slower by 5.1 / 12.8 / 14.1, 1.0 / 1.5 / 18.2, 8.5 / 4.4 / 17.4 ms; 39/54 pairs | keep, see below |
| Click lists of 16,384 users (`CLICK_SHIFT` 14) | its base `b890895` | 7.9 / 7.2 / 10.6 ms **slower**, 16/18 (run 44956730007): click index build 21.9 -> 31.7 ms | none | rejected |
| Click lists of 4,096 users (`CLICK_SHIFT` 12) | its base `b890895` | +0.3 / +11.6 / +2.8 ms, 10/18 (run 44956695585): inside the noise | none | rejected |

All fixed-cost numbers before a step's own share include about 4 ms of the fast exit where the step's
base was `b890895` and the comparison was against candidate E.

- **Exit teardown, the measured facts.** The time between the last stage and the container's end,
  minus a no-op container, was 17 to 34 ms. With the mappings let go early it is 11 to 14 ms shorter
  on all three seeds of the first run (77.8 against 90.3, 84.9 against 95.7, 74.9 against 88.8 ms). The
  success line I set before the run (variant at least 10 ms slower on every seed, 5 of 6 pairs) is not
  met as written: four of nine seeds are below 10 ms. Kept on the pooled evidence instead: all nine seed
  medians favour it, 39 of 54 pairs, median of the nine 8.5 ms, and the gain against candidate E is
  positive on all nine seeds. Worth about 5 to 10 ms on a 2-core runner; with eight threads the helpers
  cost a smaller share, which is an inference.
- **Click rows are not bounded by their kernel or their lists.** A quarter fewer instructions bought 1
  to 6% of click CPU (5 ms); 8 and 32 times fewer lists bought 4% and lost it again in the index build.
  Click rows cost 35 to 48 ns in the pipeline against 12 to 15 ns for the kernel alone in one thread.
  Inference: two threads share each core (measured above: the second thread adds only 27%), and the
  input is read cold. Remaining click gains are proportional to instructions; no structural item left.
- **Why the assembly kernel gains three times as much on the Intel runner.** Not measured directly.
  The compiler's kernel kept the row's values on the stack and reloaded them; the assembly keeps them
  in registers. The Xeon 6975P-C run lost 9% of container time to that, the EPYC 9V45 run 5%. The
  scorer is an Intel Xeon of an older generation; how it behaves is unknown until it scores this.
- **Runner closest to the scorer's class seen so far:** Xeon Platinum 8370C (Ice Lake, 2 cores x 2
  threads): candidate E + fast exit takes about 900 to 930 ms of container time there. One runner, one
  run: data for the calibration table, not a model.
- **Correctness hazard found and fixed** (correctness agent): Parquet files already in the two table
  folders were left in place and the engine's part numbers differ from run to run, so a second run into
  the same output folder would have been read together with the first. `a9852a1` removes `*.parquet`
  in `user_daily/` and `campaign_daily/` before anything is written (tested with stale parts and a
  stale campaign file; other files are left alone). The accepted official runs show the scorer's folder
  was clean; this closes the case where it is not.
- **Correctness agent on `b890895`:** 0 disagreements in about 13,500 engine runs (link mutations at
  every alias boundary, link floods and cycles, click twins and strays, lookup tables of every size,
  thread counts 1 to 8, early-exit repetitions). Now on the merged steps: slot-tag traps for `cfc7799`,
  row fuzz for the assembly kernel.
- **Candidate F (integrated, local):** `b890895` + assembly event kernel + sessions steps 1 to 3 +
  links step + table-driven click kernel + output written during the index pass + mappings let go
  early + output folder cleared + startup timers. Unit tests 49/49, fixtures 36/36 and 43/43, dev, 20M
  seeds 2 and 3 MATCH on the merge before the output step; the full tree is verified again before the
  push. Its CI run carries `[60m]`; the PR keeps naming candidate E until that run confirms F.
- **In CI now:** parser step 2, loop and store inside the assembly (`dafe006`, branch
  `exp-ci-parse-2`, variant `SS_KERNEL=plain`); sessions switches `9407f49` (`exp-ci-sessions-4`):
  `SS_PLACE=1/2/3` (how the lists are read into the slot table) and `SS_BATCH=8/128`.
- **Next for the lead:** the stage before the event parse (11 to 16 ms elapsed, about 60 CPU-ms on
  870k link rows; timers added to split it), the whale tail of the index pass (12 ms on one or two
  threads), and the 60M check of candidate F.

### 23:30 UTC

- **Official: candidate F scored 0.5231 s** (`1033956062d3cd948462c664d822f2636bcf1153`, receipt
  BB-337F162E), against 0.6389 s for `2dae9298`: 115.8 ms less, 18.1%. Relayed by the user; rank 37
  when checked, the board's best 0.2778 s. `bench/BEST_SHA` now names F: every CI run from here on pairs
  against the officially scored commit.
- **Calibration row (data, not a model):** between `2dae9298` and F the paired CI changes were 56 to 67
  ms (E against `2dae9298`, Xeon 6975P-C) plus 127 to 137 ms (F against E, Xeon 6975P-C) or 155 to 169
  ms (F against E on the slower runner of the fresh-seed run): 183 to 236 ms in all on 2-core, 4-thread
  runners. Officially: 115.8 ms on the scorer (4 cores, 8 threads, CPU model not known to us). That is
  0.49 to 0.63 official milliseconds per CI millisecond for this step; earlier steps gave 0.26 to 0.9.
  Nothing here fixes a floor or a rule: the runners differ from each other by more than that spread,
  and one official number has unknown run-to-run variation.
- **Candidate F, full evidence.** 20M against candidate E, seeds 2 / 3 / 5 (run 44960193444, Xeon
  6975P-C, 2 cores x 2 threads): +126.6 / +136.7 / +127.1 ms, 18/18 pairs (medians 799.8 -> 673.9,
  809.5 -> 672.6, 785.1 -> 657.8), A/A medians +2.5 / +3.9 / -0.4; 23/23 runs per seed MATCH
  `/reference`. Fresh seeds 17 / 19 / 23 (run 44961013928): **+155.3 / +169.1 / +161.9 ms**, 18/18
  pairs (978.8 -> 821.4, 1,024.1 -> 854.0, 1,017.1 -> 852.6), A/A medians +1.8 / -0.3 / -1.5; all
  MATCH. 60M (seed 2, Xeon 8573C): 2,652.2 -> 2,174.4 ms, +479.4 ms, 6/6 pairs, 19/19 runs MATCH
  `/reference`, peak memory 1.87 GiB (limit 13 GiB there, 24 GiB on the scorer). Fixtures 36/36 and
  43/43, dev MATCH. Locally, on the tree before the output step: eight switch settings each 36/36,
  43/43, dev MATCH, and 180/180 runs on the small and mutated sets.
- **Elapsed critical path of F** (run 44960193444, Xeon 6975P-C, seed 2, one instrumented run):
  container 670.3 ms; process 608.3 = startup 1.3, link stage 10.1 (link rows 9.6; the lookup tables
  took 10.9 on their own thread and nobody waited), parse stage 435.2 (workers 409.8, click index
  25.2), odd events 2.8, index pass 155.9 (partitions 142.9, whale tail 12.5), general remainder 2.1,
  output 0.9. Summed worker time for scale: event parse 1,435, click parse 202, link builder 117, index
  partitions 532 (placing 128, session kernel 226, the rest 179), output encode 30 CPU-ms. **The event
  parse is 59% of the process on that runner: 69 ns per row and hardware thread for about 222 to 254
  instructions, roughly one instruction per cycle per thread with two threads on a core.** Why it is
  that low is not known: perf counters are the next measurement.
- **Parser steps after the one in F, each paired directly against its parent** (EPYC 7763): loop and
  store inside the assembly (`dafe006` against `5db6322`, run 44960744992): +14.8 / +6.5 / +8.1 ms,
  17/18 pairs; eleven instructions fewer (`94f1cbb` against `dafe006`, run 44960377730): +13.1 / +3.9 /
  +6.8 ms, 14/18 pairs, A/A up to +8: positive, close to the noise. `dafe006` against candidate E:
  +74.6 / +80.8 / +79.1 ms (run 44959147433) and +46.7 / +23.9 / +41.8 ms on an EPYC 7763, where the
  same image with `SS_KERNEL=plain` is 38.7 / 32.0 / 39.1 ms slower (18/18).
- **Branch padding (Skylake-family erratum), candidate F plus the build flag against F** (run
  44960336101, EPYC 7763, which is not an affected CPU): 3.9 / 1.0 / 5.3 ms **slower**, 14/18 pairs.
  The parser agent measured on the Linux object that the flag does pad the assembly loop: 8 of its 24
  jumps touched a 32-byte boundary before, none after, at the price of 8 no-ops per row. Whether the
  scorer is an affected CPU is not known; no CI runner is one. Not in any candidate; it can only be
  judged by an official score, and the padding can be had without the no-ops by hand if that score
  says it helps.
- **Sessions switches, same image** (`9407f49`, seeds 2 and 3, default against variant; negative means
  the default is faster): `SS_PLACE=1` -5.2 / +1.7; `SS_PLACE=2` +9.7 (two outliers) / -4.8 (6/6);
  `SS_PLACE=3` -21.9 / +15.4; `SS_BATCH=8` -6.6 / -3.4; `SS_BATCH=128` +1.3 / +2.5. No variant beats
  the default on both seeds: all rejected as defaults, the switches stay for later runners.

### 23:58 UTC

- **Where event-parse time goes, measured on hosted runners** (branch `exp-lab`, parser `64eecd8`,
  20M seed 2, not containers, three rounds each; runs 44962662888 on an EPYC 9V74 and 44963256442 on an
  EPYC 7763):
  | Configuration (EPYC 9V74 unless marked) | summed event-parse CPU-ms | per row and thread |
  |---|---:|---:|
  | kernel only, no store (`SS_DIAG=7`), 1 thread | 541 to 543 | 26 ns |
  | kernel plus real lists (`SS_DIAG=1`), 1 thread | 635 to 640 | 30.5 ns |
  | kernel only, 2 threads on 2 cores | 539 to 545 | 26 ns |
  | kernel only, 2 threads on 1 core (second hardware thread) | 815 to 890 | 39 to 43 ns |
  | kernel only, 4 threads (2 cores x 2) | 815 to 825 | 39 ns |
  | store into one block (`SS_DIAG=8`), 4 threads | 870 to 904 | 42 to 43 ns |
  | real lists (`SS_DIAG=1`), 4 threads | 950 to 955 | 45.5 ns |
  | real lists, input pages touched first | 918 to 964 | 44 to 46 ns |
  | EPYC 7763, kernel only, 1 thread: original rows / no quotes / one ts form / both | 630 / 607 to 630 / 597 to 599 / 597 to 600 | 30 / 29 / 28.6 / 28.6 ns |
  | EPYC 7763, compiler's kernel, 1 thread, original rows | 765 to 767 | 36.7 ns |
  What this says: (1) the kernel itself is 86% of event-parse CPU; the store's instructions add 6 to
  10% and the lists' memory traffic another 9%; input page faults about 3%. (2) Two threads on one core
  give 1.23 to 1.32 times the throughput of one: with both hardware threads busy the core is
  throughput-bound, so only fewer uops (or fewer stalls) make a row cheaper. Interleaving rows inside
  one thread would only fill slots the second hardware thread already fills. (3) The variety of row
  formats is not the cost: the same rows with no quotes and one timestamp form are 3 to 5% cheaper.
  The four rewritten datasets gave identical tables in the pipeline (keys found in all four; the
  `compare.py` call in the lab lacked a library, so the equality was checked by the run's exit and the
  later MATCH checks of the same binary, not by a table diff in that job).
- **Parser counter build** (`64eecd8` + counters, s2_20m): 95.55% of rows take the straight line
  (about 197 instructions, about 12 ns on the laptop); 2.95% are purchases that leave the assembly for
  a Rust path (about 90 ns each); 1.50% are declined and parsed again by two slower parsers (about 150
  ns each, the general parser about 450 ns for 37,483 rows). **Those 4.45% of rows take about 30% of
  kernel time.** Only two jumps on the straight line are shape decisions (offset form 9.3%, purchase or
  unknown type 3.05%); mispredicts are not where the time goes.
- **Index pass split** (sessions agent, time-stamp counters, tip `4f16ecb`, laptop): place 22 to 24%;
  batch finish (rows, attribution, credited cells, output) 37 to 39%; one-slot path 14 to 17% (3.75M
  slots cost more than the 16.2M of the tight loop: each is a mispredicted exit); tight loop 9 to 10%;
  assists flush 6 to 7%; run ends 6 to 7%; `process_run` 1%.
- **Results of this round, each alone against its parent** (EPYC 7763 unless marked): constants in
  registers (`47d7ec5`) +2.0 / +3.6 / +4.3 ms, 11/18: not demonstrated (expected only on Intel); extras
  merged by time (`9e8159d`) +7.9 / +2.5 / +3.8, 14/18: small win; per-partition assists table
  (`SS_ASSISTS=1` / `=2`, same image, 9V45 and 6975P-C) -4.1 / -7.3 / -1.8 and +0.4 / -0.8 / +0.3: no
  gain, stays off; links step 2 (`cfdf102`, Xeon 8370C) +5.1 / +0.4 / +2.0, 11/18: not demonstrated.
- **Candidate F2 (this push):** F + the parser chain to `64eecd8` + the index-pass chain to `4f16ecb` +
  links `cfdf102`. Unit tests 52/52, fixtures 36/36 and 43/43, dev, 20M seeds 2 and 3 MATCH locally.
  Its CI run pairs it against F and carries the 60M check; the steps were measured one at a time, and
  only this combined run counts for the combination.

### 00:34 UTC

- **Agents stopped.** At about 00:15 UTC the parser, index-pass, click and correctness agents all ended
  on the account's model usage limit (resets 2026-11-24, after the cutoff). Committed work is intact;
  the click agent's unfinished J prototype was finished, verified and committed by the lead (`48a5427`).
  The correctness agent's queued checks of F2 keep running on the laptop. From here the lead works
  alone; no replacement agents, to keep the budget for integration and validation.
- **Reviewer (independent agent, read-only)**, main points: Q's kill holds but for a different reason
  (placing early only moves work); M was refuted only in its latency-hiding form, two rows sharing
  vector registers is untested; on an Intel scorer the saturated resource may be one port (shuffles,
  tzcnt/pext/imul) or legacy decode (branch erratum), so uop counts per port matter more than totals;
  H's realistic bound is about 5% of the partition pass; two official points are consistent with a
  fixed official part of 105-140 ms, which a one-runner timing of all scored commits would test (not
  run yet). Correctness traps listed for G, H and J (used in the checks below).
- **Results** (median paired ms; positive = the change is faster):
  | Exp | Against | Result | Same-image variant | Decision |
  |---|---|---|---|---|
  | F2 + fixes `c8676bf` | F | +18.4 / +17.7 / +14.8, 18/18 (second run of the F2 code; first: +15.0 / +12.9 / +13.7, 18/18) | | candidate base |
  | G(a) purchases in the assembly `e552939` | `64eecd8` | +6.0 / +8.9 / +3.0 (14/18); +4.0 / +1.9 / +0.1 (13/18), EPYC 7763 | `SS_RARE=0` 3.5 / 6.3 / 4.5 slower (16/18) | keep |
  | H wide slot scan `2764dfa`, `f5aa625` | same image | | `SS_WIDE=1`: +11.9 / -0.2 / -8.1 (9V74); -1.1 / -3.3 / -0.0 (9V45) | not demonstrated; stays off |
  | J clicks grouped on demand `48a5427` | F2 `9e31bf3` | **-31.5 / -27.3 / -29.3 (0/18)**, EPYC 7763; -23.7 / -21.6 / -15.5 (1/18), 9V45 | `SS_CLICK_GROUP=0` 24.8 / 24.6 / 23.3 faster (18/18) | **killed** |
  | K chains processed early `eff462b` | `c8676bf` | +4.6 / +13.2 / +9.1 (14/18); +6.3 / +16.4 / +15.0 (17/18), EPYC 7763 | `SS_CHAINS_EARLY=0` 16.5 / 18.5 / 12.8 slower (15/18) | keep |
  Why J lost (measured): the click-index phase went from 23.8 to 0.4 ms, but the partition pass grew
  from 181.6 to 215.0 ms and its summed time by 130 CPU-ms. Neighbouring partitions run at the same
  time on different threads, so the range they share is grouped by one while the other waits, in the
  middle of its scan. A phase of its own does the same grouping with all threads busy and no waiting.
  K, measured: time after the last partition 12.7 ms -> 0.3 ms.
- **Fresh seeds for the F2 code** (seeds 29 / 31 / 37, run 44965105727): void, the runner was
  disturbed (A/A pairs up to +-264 ms); repeated as run 44966494904.
- **Candidate F3 (this push):** F2 + fixes + K + G(a). Unit tests 53/53, fixtures 36/36 and 43/43,
  dev, 20M seeds 2 and 3 MATCH locally. Its CI run pairs it against F with the 60M check.

### 00:45 UTC

- **Calibration, measured (branch `exp-calib`, run 44967358977).** The seven officially scored commits
  timed on one runner (AMD EPYC 7763, 2 cores x 2 threads), 20M seed 2, six shuffled rounds of fresh
  containers after one warm-up each:
  | commit | CI container median (min) ms | official |
  |---|---:|---:|
  | `e4eef020` | 2,015.2 (2,007.0) | 1.38 s |
  | `23b285da` | 1,141.6 (1,139.2) | 0.84 s |
  | `2b2cccd1` | 936.9 (931.9) | 0.71 s |
  | `4567e953` | 896.3 (891.8) | 0.66 s |
  | `2dae9298` | 820.0 (809.3) | 0.6389 s |
  | `9cb9ad6e` | 864.5 (831.6) | 0.64 s |
  | `10339560` (F) | 678.0 (673.3) | 0.5231 s |
  Least squares: official ms = 102 + 0.64 x CI ms with all seven points; 66 + 0.68 x CI ms without the
  oldest (high-leverage) one. Residuals within +-15 ms, the size of the old two-decimal rounding.
  **What this supports:** on that runner type, about two thirds of a CI millisecond shows up officially,
  and a part of 65 to 100 ms of the official time does not move with our CI time (the scorer's own
  container handling, timing method, or work of ours that is the same in every commit: not separable
  here). **What it implies, as arithmetic only:** 0.278 s would need about 270 to 310 ms of CI container
  time on this runner type, against about 640 ms for F3: roughly 55% less container time, or about 60%
  less process time (the CI time includes 55 to 75 ms of a no-op container). Seven points and one runner
  type: a calibration, not a law.
- **Candidate F3 `8dde2af12e542b0df94ba6d0be3da4f8660f896d` against F** (run 44967278499, EPYC 9V74):
  **+42.5 / +38.9 / +42.2 ms, 18/18 pairs**, A/A medians -0.0 / -6.6 / +0.6; all runs MATCH. 60M (EPYC
  9V45): 1,323.3 -> 1,240.0 ms, +84.8 ms, 5/6 pairs, MATCH; peak memory 1.69 GiB (F2: 1.86 GiB).
  By the calibration above, about 20 to 30 official ms, if it carries over. Fresh seeds 41 / 43 / 47 are
  running (run 44968021383).
- **F2 code `c8676bf`, fresh seeds 29 / 31 / 37** (run 44966494904, EPYC 7763, A/A within 14 ms):
  +18.2 / +19.1 / +29.5 ms, 18/18 pairs. F2 is confirmed and named in the PR until F3 is.
- **In CI:** declined event rows straight to the general kernel (`1b73234`, G(b) step 1, against F3,
  variant `SS_RARE=a`).

### 00:45 UTC (later)

- **Official: candidate F3 scored 0.4663 s** (`8dde2af12e542b0df94ba6d0be3da4f8660f896d`, receipt
  BB-9900D7A0): 56.8 ms and 10.9% less than F's 0.5231 s. Relayed by the user. `bench/BEST_SHA` now
  names F3; every CI run pairs against it.
- **Calibration row:** F3 against F in CI +42.5 / +38.9 / +42.2 ms (EPYC 9V74); the one-runner table
  above (EPYC 7763) has F at 678 ms. Official change 56.8 ms for a CI change of about 40 ms: this step
  carried over at more than one to one, where the earlier steps gave 0.5 to 0.9. Noise in a single
  official number is unknown; the intercept-plus-slope reading above is a fit over seven points and
  should not be taken as a rule for the next step either.
- **Agents remain stopped** (usage limit); the lead works alone with CI. Budget is kept for integration
  and the final candidate's validation.

### 00:59 UTC

- **F3 fresh seeds 41 / 43 / 47** (run 44968021383): +27.1 / +30.9 / +28.9 ms against F, 18/18 pairs,
  A/A medians +1.3 / -0.3 / -1.0. F3 is confirmed on nine seeds in all.
- **F3's profile, refreshed** (run 44967278499, EPYC 9V74, 2 cores x 2 threads, seed 2, one
  instrumented run): container 552.3 ms; process 473.2 = startup 2.4, link stage 14.7 (link rows 8.7 on
  all threads; the lookup tables took 16.8 ms on their own thread and were waited for 5.3 ms), parse
  stage 294.3 (workers 274.1, click index 20.0), odd events 2.1, index pass 156.3 (partitions 155.7,
  tail after the last partition 0.2), general remainder 2.1, output 1.1. Summed worker time: event
  parse 914, click parse 181, link builder 44.5 (beside the parse, never waited for), index partitions
  538 = place 131 + session kernel 229 + assists 26 + the rest 152; output encode 33 CPU-ms. **Event
  parse 62% of the process, index pass 33%, everything else 5%.** The only idle time on the critical
  path is the 5.3 ms table wait; the stages are otherwise parallel and compute-bound.
- **G(b)(1), declined rows straight to the general kernel** (`1b73234` against F3, runs 44967865665 and
  44967875462, EPYC 7763): +2.6 / -0.3 / -3.2 (10/18) and +1.1 / +4.3 / +0.7 (11/18); same image with
  `SS_RARE=a` (old route) +2.9 / -0.4 / -3.6 ms. **Not demonstrated**, as the bound predicted (1.5% of
  rows x 25 ns = about 2 ms elapsed). Stays (harmless), counts for nothing.
- **Lookup tables without one allocation per product** (`60758b0`, pushed 00:55, pairs against F3):
  in CI. Bound: the 5.3 ms wait (about 9 ms with eight threads, since the link rows then finish sooner).
- **The user's three hypotheses, screened:**
  1. *Multi-row parsing with shared vector work.* Assigned to the parser agent after its current step,
     time-boxed: count first (kill unless a pair saves at least 12% of the straight line's
     instructions), build behind `SS_KERNEL=pair` only then. Why it could pay: the straight line runs at
     about 3.4 IPC single-thread and a second hardware thread adds only 23-32%, so the core is
     throughput-bound and fewer uops per row are time. Prior failure it must avoid: the "two halves out
     of step" parser (c890351) lost 10-17 ms in CI because it lengthened the dependency chain of each
     row; the pair design must keep field finding per row and share only the ID and timestamp vector
     work. Bound: 10-15% of event parse CPU, about 6-9% of the process.
  2. *Partial-parse reuse for expensive fallback rows.* This is the G(b) line. Measured bound: rows
     that leave the straight line are 4.45% (purchases 2.95%, declines 1.50%); purchases are finished
     inside the assembly since F3 (G(a), +4 to +9 ms); the declines cost about 150 ns each (about 45
     CPU-ms of 914, 5% of event parse, about 2% of the process), of which the wasted re-read was
     measured as nothing. The last step with a measurable bound is (b)(2), invalid-timestamp rows
     finished inside the assembly (0.69% of rows), which the parser agent is building; everything
     beyond it is below the noise floor of the measurement. No separate agent.
  3. *A representation or ownership change that removes event rearrangement work.* What rearranges
     events today: the parse writes 16-byte records to per-thread lists by partition (lists cost 9% of
     event parse CPU, measured in the lab), `place` reads them into the slot table (131 CPU-ms, 24% of
     the index pass), the scan reads the slots. Together about 11% of all CPU. Designs measured and
     rejected: records written straight to their slots by all threads (direct atomic scatter, 1.5-2.2x
     slower: random writes over 335 MB from every thread), one stream per owner (2-3x slower),
     two-level partitioning (30-40% slower at 20M), 2^13 / 2^15 partitions, write-combining stores.
     Sessionizing from the lists without placing is killed on paper: within a user the copies arrive in
     file order, which is random, so every person would need a sort, which is more work than place; and
     a record cannot shrink below 16 bytes because the winning copy's own ts, ingest, user and purchase
     must travel with it (copies with different ingest may differ in any field). What has not been
     tried and could still pay: nothing with a bound above 5% that avoids the measured failures. Killed
     for this sprint; recorded so it is not retried without a new argument.
- **Agents:** three were resumed at 00:52 and two stopped again at 00:56 on the user's budget note; the
  parser agent alone continues, scoped to G(b)(2) and hypothesis 1, then stops.

### 01:03 UTC

- **T, lookup tables without one allocation per product** (`60758b0` on top of `1b73234`, against F3,
  run 44968983694, EPYC 9V74): **+11.8 / +12.6 / +20.5 ms, 18/18 pairs**, A/A medians -3.3 / -1.8 /
  +1.8. Instrumented runs, seeds 2 / 3 / 5: the tables' own thread 22.8 / 20.9 / 22.0 -> 8.1 / 7.6 / 7.7
  ms; the wait for them 7.4 / 4.8 / 6.9 -> 0.0 ms; the link stage (link rows plus that wait) 19.7 /
  17.7 / 19.8 -> 8.9 / 8.6 / 11.4 ms. More than the wait itself, and this run includes `1b73234`
  (measured neutral alone). Kept. With eight threads the link rows finish sooner, so the wait was the
  larger share of the start there; how much of that the scorer shows is unknown.
- **Parser agent, final report:** G(b)(2) (`069f752`, rows with no usable timestamp stored as invalid
  rows inside the assembly; per-case exactness argument in its commit message; 54 unit tests and the
  full verification list) is verified but **not integrated**: by its own counters it moves 0.58% of rows
  off one 85 ns call, about 2.5 ms of CPU-bound elapsed time on four threads, below what CI resolves.
  Hypothesis 1 (two rows per pass) **killed on the instruction count**: a pair shares only the ID vector
  sequence and one loop edge, about 5.5 instructions per row of 216 (2.5%) against the bar of 25 (12%);
  the timestamps already use both lanes of a row. The agent has stopped.
- **Candidate F4 (this push):** F3 + `1b73234` + `60758b0`. Its CI run carries the 60M check; fresh
  seeds follow as a dispatch run.

### 01:12 UTC

- **Candidate F4 `bad5c1b1bc3178508db7de5d7459197741497236` VERIFIED** (F3 + lookup tables without one
  allocation per product + declined rows straight to the general kernel), against F3:
  | run | runner | seeds | paired median ms | pairs | A/A medians |
  |---|---|---|---|---|---|
  | 44968983694 (code without the last ledger commit) | EPYC 9V74 | 2 / 3 / 5 | +11.8 / +12.6 / +20.5 | 18/18 | -3.3 / -1.8 / +1.8 |
  | 44969638463 | Xeon Platinum 8573C | 2 / 3 / 5 | +9.5 / +3.8 / +7.0 | 15/18 | -0.8 / -1.4 / -2.4 |
  | 44969646367 (fresh) | Xeon Platinum 8573C | 53 / 59 / 61 | +8.2 / +6.6 / +8.1 | 17/18 | -1.8 / -1.0 / -0.9 |
  60M (Xeon 6975P-C): MATCH, 1,846.6 against 1,845.1 ms, -6.2 ms median paired (2/6), inside the A/A
  noise of that runner (median 13.9 ms): no change, as expected (the tables were not waited for at 60M).
  Peak memory 1.70 GiB. Locally: ten switch settings and 180/180 small and mutated runs MATCH. Named in
  the PR as the commit to submit; a small, CPU-dependent gain (7 to 20 ms in CI).
- **Architecture rethink started** as a workflow (spec from the rules backward; three designs from
  different families made blind to the current engine, then compared with it; three adversarial reviews
  each; a ranking that picks at most two bounded prototypes). Results follow in the next entry.

### 01:13 UTC

- **Official: candidate F4 scored 0.4648 s** (`bad5c1b1bc3178508db7de5d7459197741497236`, receipt
  BB-9044646E), 1.5 ms below F3's 0.4663 s. In CI the step was +4 to +20 ms depending on the runner.
  `bench/BEST_SHA` now names F4. Calibration note: a 1.5 ms official change is inside any plausible
  run-to-run variation of the scorer; this step tells us nothing about the slope.
- **Attribution measured end to end** (diagnostic `SS_DIAG_ATTR=1` on branch `exp-ci-attr`, same image,
  run 44969828387, EPYC 9V74, seeds 2 and 3): skipping attribution for persons with clicks saves
  **67.4 / 63.8 ms of container time, 12/12 pairs** (containers 668 -> 603 and 672 -> 608 ms). Index pass
  191.5 -> 130.7 and 194.2 -> 131.9 ms; session kernel 294 -> 84 CPU-ms; partitions 654 -> 424 CPU-ms.
  So attribution (reach of each click, credited click per session, distinct assisting campaigns,
  campaign cells, assists flush) costs about 230 CPU-ms, about 35% of the index pass and about 10% of
  the container time on that runner: the largest single block outside the event parser. Its output is
  wrong by design under the switch; the diagnostic never enters a candidate.

### 01:16 UTC

- **Attribution anatomy** (profile lab, run 44970524890, EPYC 9V45, F4's code built with the session
  kernel's lap profiler on, 20M seed 2, NCPU=4, three rounds, per-worker laps summed over four workers):
  per-click reach loop about 56 CPU-ms (4.8M clicks, about 11.7 ns each); credit choice about 8;
  generating the assists (one pending entry per campaign and person-day with an assisted session) about
  52; applying them to the campaign cells (`cpu_index_flush`) about 26 (11.5M entries, about 6.8 ns
  each with the generation); the rest of the measured 230 CPU-ms (setup copies per clicked person, the
  per-person click copy, credited-cell updates) about 90, not separable with these laps (the profiler's
  setup bucket also absorbs non-attribution work in the batched path, and its first bucket counts from
  process start: both discarded). **No single removable block**; a redesign could plausibly save about a
  third of attribution, about 20 ms of CI container time (3%): an exploitation item, not an architecture
  change. Recorded as hypothesis I with that bound.

### 01:30 UTC

- **Architecture workflow stalled** (its first agent's request at 01:10 never returned; other model
  agents are unavailable): stopped; the architecture pass is the lead's own, written into
  `bench/HANDOFF.md` (spec traced back from the outputs, essential versus choice, designs D, I2, X ranked).
- **E1, the scorer's class of CPU, MEASURED unavailable:** a 20-job matrix (run 44971115429) drew AMD EPYC
  7763 x6, 9V74 x7, 9V45 x2 and Intel Xeon 6975P-C x3, 8573C x3; 14 of 20 have AVX-512 VBMI2. No
  Skylake-family Xeon (Cascade Lake) and no Ice Lake Xeon (one 8370C was seen once in about 60 earlier
  runs). The branch-erratum question (padded jumps) and the stage profile on the scorer's kind of core
  cannot be measured in CI; an official A/B would need the user's authorization.

### 01:50 UTC

- **D, click index built while event rows are parsed: REJECTED** (`bf947ce`, branch `exp-ci-click-early`,
  against F4): +5.0 / +1.8 / +2.7 ms, 14/18 pairs (run 44971495438); +2.4 / +0.8 ms, 7/12, EPYC 7763
  (run 44971502510); same image with the old phase -4.4 / +0.6 ms. Below the criterion set before
  measurement (+5 ms on both seeds with 10/12). MEASURED lesson: with both hardware threads of a core
  busy parsing, the grouping does not ride in idle issue slots; the phase's work moved and cost about
  the same (consistent with the core being throughput-bound under SMT).
- **G(b)(2), invalid-timestamp rows inside the assembly (`069f752`): not demonstrated** (+4.2 / +7.5 /
  +1.1, 13/18, EPYC 9V74; -9.8 / -0.6, 5/12, EPYC 7763; `SS_RARE=b` variant inside the noise), as its
  2.5 ms bound predicted. Not integrated.
- **Attribution split, MEASURED** (same-image variants of `SS_DIAG_ATTR`, branch `exp-ci-attr2`, seed 2):
  | skipped | runner | container | partitions CPU-ms | kernel CPU-ms |
  |---|---|---|---|---|
  | all attribution | Xeon 6973P-C | -67.8 ms (6/6) | 553 -> 374 | 231 -> 67 |
  | assists only | EPYC 9V74 | -27.9 ms (5/6) | 515 -> 432 | 232 -> 168 (flush 20 -> 0) |
  | credited-cell writes only | Xeon 6975P-C | -2.5 ms (5/6) | 539 -> 511 | 229 -> 213 |
  Assists (distinct assisting campaigns per session, per person-day entries, flush) about 83 CPU-ms,
  about 5% of container time, about 7 ns per (person, campaign, day) entry over 11.5M entries; credited
  cells cheap; reach, credit and setup about 70 CPU-ms (cross-runner subtraction, approximate).
- **E3, allocator with 2 MiB-aligned own mappings** (`ea7d6ac`, branch `exp-ci-align`, with 60M): in CI.

### 02:00-02:50 UTC: the republished generator, and the opaque-ID engine

- **Republish, MEASURED (about 01:30 UTC):** the track page's `rules.md`, generator and dev answer
  changed (generator.zip sha256 `78f973f7...`, dev-answer.zip `c2439e0e...`; `bench/fetch_tools.sh`
  pins both). New rules text: "pipelines must treat IDs as opaque variable-width strings rather than
  fixed-width or invertible generator numbers". One in eight distinct user, event and click IDs is now
  a prefix and 32 lowercase hexadecimal digits (33 bytes); every ID passes a per-seed keyed bijection.
  23.1% of event rows of new 20M seed 2 carry a long ID.
- **Index mode is outside the new rules** (it recovers the generator's keys and reads IDs as numbers)
  and the new data defeats it anyway: F4 `bad5c1b` on new 20M stays exact but takes 4.7-5.2 s in CI
  (run 44972016492: keys not found, every long ID through one lock). Index mode is now off unless
  `SS_STRUCT=1`; the general path, which codes IDs by their text only, is the engine.
- **Steps on the new data** (each paired directly against its parent; A = parent, B = step):

  | step | what | run | paired ms saved (seeds 2/3/5) | runner |
  |---|---|---|---|---|
  | `9735a63` | general path default; long hex IDs interned by value in 256 locked shards | 44973966804 | +3,100 / +3,010 / +3,153 vs F4 (18/18); 60M 6,350 vs 16,833 ms, MATCH, peak 3.41 GiB | EPYC 7763 |
  | `4cea00c` | event kernel takes rows up to 127 bytes | 44974331774 | +385 / +392 / +385 (18/18) | EPYC 7763 |
  | `727afb7` | long event IDs never interned: deduplicated by value in their partition | 44975293020 | **-526 / -500 / -507 (0/18)**: parse -210 ms, dedup +750 ms | EPYC 9V45 |
  | `da20171` | click kernel takes rows up to 127 bytes | 44975511792 | +36 / +63 / +46 (17/18) | EPYC 9V74 |
  | `a2cf9f0` | long click IDs kept per parse worker (no table, no lock) | in CI | | |
  | `e2aed79` | fix of `727afb7`: table slot from a second multiply (was the partition's own bits: quadratic probing) | in CI against `4cea00c` | | |
  | `6d9ec1f` | user IDs looked up without a lock (2^20-slot table, write-once slots); hex digits as one vector | in CI | | |

- **Where long IDs cost, MEASURED on the laptop** (one thread, parse only, new 2M set against the old
  generator's 2M set of the same seed): event parse 134 vs 92 ms, click parse 30 vs 14 ms. Faking codes
  (wrong output, timing only): the user interner was 12 ms of 141 (8.5%) of event parse, the click-ID
  interner 10 of 33 ms of click parse. Four threads on new 20M: the user lock cost grows with threads
  (event parse CPU -20% with the lock-free table).
- Lesson recorded: `727afb7` passed every correctness check and lost 500 ms; a step is judged by its
  paired CI run, never by its local parse timing alone.

### 03:20 UTC: official result on shopping-sessions-5; baseline moved

- **Official: `9735a63` scored 1.2745 s** (receipt BB-32BE0EA7) on the current dataset version
  shopping-sessions-5; F4 `bad5c1b` scores 5.6629 s there. `9735a63` is the accepted baseline;
  `bench/BEST_SHA` now names it, so CI pairs every push against it (F4 took about 5 s per run).
- Calibration point (one runner type, EPYC 7763): `9735a63` CI container 2,070 / 2,128 / 2,079 ms
  (seeds 2/3/5, run 44973966804) against 1,274.5 ms official: official/CI about 0.61 on this data
  (0.66 slope + 80 ms on the old data would have predicted 1.46 s). New goal: below 0.6018 s official,
  roughly CI 900-990 ms on an EPYC 7763 runner by this ratio; stretch 0.20 s.
- Further steps measured since the table above (A = parent, B = step; EPYC 7763 unless named):
  `e2aed79` stack (long-ID dedup by value + its fix, click kernel to 127 bytes, click IDs per worker)
  against `4cea00c`: +413 / +442 / +420 ms (18/18, Xeon 8370C, run 44977055289); `a2cf9f0` alone
  +72 / +70 / +70 (16/18, run 44976784731); `6d9ec1f` lock-free user table and vector hex digits
  +56 / +62 / +57 (18/18, run 44977620389); `1ffd278` prefetch ring for long-ID survivors
  +13 / +9 / -12 (10/18, run 44977877879): NOT DEMONSTRATED, neutral, left in the chain.
  Container of `1ffd278` on EPYC 7763: about 1,250-1,270 ms. Its startup stage is 18 ms (was 2):
  the user table's 32 MiB zeroed on one thread; `dffab6f` moves it to fresh zero pages (in CI).
