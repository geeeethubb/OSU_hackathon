#!/usr/bin/env python3
"""
Runs checks/pyref.py on every case of one or more fixture folders and compares
both tables with the case's expected CSVs: exact, order-insensitive, CSV to CSV.
Pure standard library (no polars needed).

Usage: python checks/pyref_fixtures.py [FIXTURES_DIR ...]
Default: the repo's fixtures/ and extra_fixtures/.
"""
import csv
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
PYREF = os.path.join(HERE, "pyref.py")
TABLES = {
    "user_daily": ["user_id", "day", "sessions", "events", "revenue_cents", "top_category"],
    "campaign_daily": ["campaign_id", "channel", "day", "sessions", "purchasing_sessions",
                       "revenue_cents", "assisted_sessions"],
}


def read_table(path, header):
    """Rows of a table CSV as sorted tuples of str; bytes are kept 1:1 via latin-1."""
    with open(path, "r", encoding="latin-1", newline="") as handle:
        rows = list(csv.reader(handle))
    if not rows or rows[0] != header:
        raise ValueError(f"{path}: bad header {rows[:1]}")
    for row in rows[1:]:
        if len(row) != len(header):
            raise ValueError(f"{path}: bad row {row}")
    return sorted(tuple(row) for row in rows[1:])


def run_case(case_dir):
    data = os.path.join(case_dir, "input")
    if not os.path.isdir(data):
        data = os.path.join(case_dir, "data")
    problems = []
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as out:
        done = subprocess.run([sys.executable, PYREF, data, out], capture_output=True, text=True)
        if done.returncode != 0:
            return [f"pyref exit {done.returncode}: {done.stderr[-2000:]}"]
        for line in done.stderr.splitlines():
            if "WARNING" in line:
                problems.append(line)
        for table, header in TABLES.items():
            expected = read_table(os.path.join(case_dir, "expected", f"{table}.csv"), header)
            actual = read_table(os.path.join(out, f"{table}.csv"), header)
            if expected != actual:
                problems.append(f"{table}\n  expected {expected}\n  pyref    {actual}")
    return problems


def main():
    folders = sys.argv[1:] or [os.path.join(REPO, "fixtures"), os.path.join(REPO, "extra_fixtures")]
    failed = total = 0
    for folder in folders:
        cases = sorted(name for name in os.listdir(folder) if os.path.isdir(os.path.join(folder, name)))
        for case in cases:
            total += 1
            problems = run_case(os.path.join(folder, case))
            if problems:
                failed += 1
                print(f"FAIL {os.path.basename(folder)}/{case}")
                for problem in problems:
                    print(f"  {problem}")
            else:
                print(f"ok   {os.path.basename(folder)}/{case}")
    print(f"\n{total - failed}/{total} fixtures match pyref")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
