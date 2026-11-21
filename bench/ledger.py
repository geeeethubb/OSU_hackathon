#!/usr/bin/env python3
"""Appends a CI run's compact line(s) to bench/LEDGER.md: one row per candidate and seed.

    python bench/ledger.py RUN_ID            # fetches that run's result artifacts with gh
    python bench/ledger.py --dir DIR         # artifacts already downloaded (DIR/ss-*/<job>.json)
    python bench/ledger.py RUN_ID --print    # only print the rows

CI cannot commit to the branch, so the ledger is updated by hand: run this, fill in the last
cell ("next decision"), commit. A row that is already in the ledger (same SHA, same data) is
not added twice. GH names the GitHub CLI when it is not plain `gh`.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import report  # noqa: E402

LEDGER = Path(__file__).resolve().parent / "LEDGER.md"


def load_results(folder):
    results = {}
    for path in sorted(Path(folder).rglob("*.json")):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if isinstance(data, dict) and data.get("job"):
            results[data["job"]] = data
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("run", nargs="?", help="Actions run id")
    parser.add_argument("--dir", help="folder with the run's downloaded artifacts")
    parser.add_argument("--repo", default=os.environ.get("GH_REPO", "geeeethubb/OSU_hackathon"))
    parser.add_argument("--ledger", default=str(LEDGER))
    parser.add_argument("--print", action="store_true", dest="only_print")
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")

    folder = args.dir
    if not folder:
        if not args.run:
            parser.error("give a run id or --dir")
        folder = tempfile.mkdtemp(prefix="ss-ledger-")
        command = shlex.split(os.environ.get("GH", "gh")) + ["run", "download", args.run, "--repo", args.repo, "--dir", folder]
        done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace")
        if done.returncode != 0:
            sys.exit(f"could not download the artifacts of run {args.run}: {done.stderr.strip()[:400]}")
    results = load_results(folder)
    if "bench-20m" not in results:
        sys.exit(f"no bench-20m result under {folder}")
    rows = report.compact_rows(results["bench-20m"], results.get("correctness"), results.get("bench-60m"))
    if not rows:
        sys.exit("that run has no pipeline numbers (no root Dockerfile at its commit?)")
    if args.only_print:
        print("\n".join(rows))
        return 0
    ledger = Path(args.ledger)
    text = ledger.read_text(encoding="utf-8") if ledger.is_file() else ""
    added = 0
    for row in rows:
        cells = [cell.strip() for cell in row.strip().strip("|").split(" | ")]
        if any(cells[0] in line and cells[3] in line for line in text.splitlines()):
            print(f"already in the ledger: {cells[0][:12]} / {cells[3]}")
            continue
        text = text.rstrip("\n") + "\n" + row + "\n"
        added += 1
    with open(ledger, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
    print(f"added {added} row(s) to {ledger}; fill in the last cell before committing")
    return 0


if __name__ == "__main__":
    sys.exit(main())
