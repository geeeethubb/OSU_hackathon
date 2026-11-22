#!/usr/bin/env python3
"""
crosscheck.py: generated datasets, pyref against an implementation under test.

For every (seed, events) pair it
  1. generates the dataset with the organizers' generator (cached),
  2. runs checks/pyref.py on it (cached per dataset and per pyref.py version),
  3. runs the implementation under test: the command after `--`, called the way
     scripts/run_fixtures.py calls it (DATA_DIR and OUTPUT_DIR in the
     environment and as the last two arguments; it writes the Parquet folders
     user_daily/ and campaign_daily/),
  4. diffs BOTH tables exactly, row by row, and prints counts and the first few
     differing rows per table.
Exit code 1 on any difference (or if a step fails), 0 if everything matches.

Usage (run with the Python that has polars):
  python checks/crosscheck.py --seeds 2-6 --events 200000,1000000 -- path/to/pipeline.exe
  python checks/crosscheck.py --seeds 7,8 --events 1000 --clicks 400 -- path/to/pipeline.exe
  python checks/crosscheck.py --data some/data/dir -- path/to/pipeline.exe      (an existing dataset)

Options:
  --seeds LIST      seeds, e.g. 2,3,9 or 2-6                      (default 2)
  --events LIST     event counts, at least 1000 each             (default 200000)
  --clicks N        clicks per dataset                           (default events / 4)
  --data DIR        check an existing dataset instead (repeatable; pyref is not cached)
  --work DIR        where datasets and outputs go
                    (default: $XCHECK_DIR, else ../ss_work/data/xcheck next to the repo)
  --generator EXE   (default: $GENERATOR, else ../ss_work/gen/target/release/generate.exe)
  --show N          differing rows to print per table and kind   (default 5)

The Parquet side is loaded with the organizers' own loader (scripts/run_fixtures.py),
so a wrong column set, a non-DATE `day` or a non-integer count fails here exactly
as it would in the official check.
"""
import csv
import hashlib
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
PYREF = os.path.join(HERE, "pyref.py")
SS_WORK = os.path.join(os.path.dirname(REPO), "ss_work")

sys.path.insert(0, os.path.join(REPO, "scripts"))
from run_fixtures import SCHEMAS, load_actual  # noqa: E402  (the official loader and schemas)

KEYS = {"user_daily": ("user_id", "day"), "campaign_daily": ("campaign_id", "day")}
INT_COLUMNS = {"sessions", "events", "revenue_cents", "purchasing_sessions", "assisted_sessions"}


def parse_list(text):
    values = []
    for part in text.split(","):
        if "-" in part:
            first, last = part.split("-")
            values.extend(range(int(first), int(last) + 1))
        else:
            values.append(int(part))
    return values


def parse_args(argv):
    if "--" not in argv:
        sys.exit(__doc__)
    split = argv.index("--")
    own, command = argv[:split], argv[split + 1:]
    if not command:
        sys.exit("crosscheck: no command after --")
    options = {"seeds": [2], "events": [200000], "clicks": None, "data": [], "show": 5,
               "work": os.environ.get("XCHECK_DIR") or os.path.join(SS_WORK, "data", "xcheck"),
               "generator": os.environ.get("GENERATOR")
               or os.path.join(SS_WORK, "gen", "target", "release", "generate.exe")}
    index = 0
    while index < len(own):
        name = own[index]
        if name not in ("--seeds", "--events", "--clicks", "--data", "--work", "--generator", "--show") \
                or index + 1 >= len(own):
            sys.exit(f"crosscheck: bad option {name}\n{__doc__}")
        value = own[index + 1]
        index += 2
        if name in ("--seeds", "--events"):
            options[name[2:]] = parse_list(value)
        elif name in ("--clicks", "--show"):
            options[name[2:]] = int(value)
        elif name == "--data":
            options["data"].append(value)
        else:
            options[name[2:]] = value
    return options, command


def file_sha(path):
    with open(path, "rb") as handle:
        return hashlib.sha1(handle.read()).hexdigest()


def run(command, **kwargs):
    started = time.time()
    done = subprocess.run(command, capture_output=True, text=True, errors="replace", **kwargs)
    return done, time.time() - started


def generate(options, seed, events, clicks, data_dir):
    marker = os.path.join(os.path.dirname(data_dir), "generated.ok")
    if os.path.isfile(marker):
        return True
    shutil.rmtree(data_dir, ignore_errors=True)
    os.makedirs(os.path.dirname(data_dir), exist_ok=True)
    done, seconds = run([options["generator"], "--seed", str(seed), "--events", str(events),
                         "--clicks", str(clicks), "--output", data_dir])
    if done.returncode != 0:
        print(f"  generator FAILED (exit {done.returncode}): {done.stderr[-1000:]}")
        return False
    with open(marker, "w") as handle:
        handle.write(f"seed {seed} events {events} clicks {clicks}\n")
    print(f"  generated in {seconds:.1f}s")
    return True


def run_pyref(data_dir, out_dir, cache):
    """Runs pyref (or reuses its cached output). Returns False if it failed."""
    marker = os.path.join(out_dir, "pyref.sha1")
    sha = file_sha(PYREF)
    if cache and os.path.isfile(marker) and open(marker).read().strip() == sha \
            and all(os.path.isfile(os.path.join(out_dir, f"{table}.csv")) for table in SCHEMAS):
        print("  pyref: cached")
        return True
    shutil.rmtree(out_dir, ignore_errors=True)
    os.makedirs(out_dir)
    done, seconds = run([sys.executable, PYREF, data_dir, out_dir])
    with open(os.path.join(out_dir, "pyref.log"), "w") as handle:
        handle.write(done.stderr)
    if done.returncode != 0:
        print(f"  pyref FAILED (exit {done.returncode}): {done.stderr[-2000:]}")
        return False
    for line in done.stderr.splitlines():
        if "WARNING" in line:
            print(f"  {line}")
    with open(marker, "w") as handle:
        handle.write(sha + "\n")
    print(f"  pyref: {seconds:.1f}s")
    return True


def run_implementation(command, data_dir, out_dir):
    shutil.rmtree(out_dir, ignore_errors=True)
    os.makedirs(out_dir)
    env = dict(os.environ, DATA_DIR=data_dir, OUTPUT_DIR=out_dir)
    done, seconds = run(command + [data_dir, out_dir], env=env)
    if done.returncode != 0:
        print(f"  implementation FAILED (exit {done.returncode}): {done.stderr[-2000:]}")
        return False
    print(f"  implementation: {seconds:.1f}s")
    return True


def csv_rows(path, table):
    """pyref's CSV as {column: value} rows: str IDs, `day` as YYYY-MM-DD, int counts, None for null."""
    columns = list(SCHEMAS[table])
    with open(path, "rb") as handle:
        text = handle.read().decode("utf-8", errors="surrogateescape")
    lines = list(csv.reader(text.split("\n")[:-1]))
    if lines[0] != columns:
        raise ValueError(f"{path}: bad header {lines[0]}")
    rows = []
    for fields in lines[1:]:
        row = dict(zip(columns, fields, strict=True))
        for column in columns:
            if column in INT_COLUMNS:
                row[column] = int(row[column])
        if table == "user_daily" and row["top_category"] == "":
            row["top_category"] = None
        rows.append(row)
    return rows


def parquet_rows(folder, table):
    """The implementation's Parquet folder, through the official loader, in the same shape."""
    frame = load_actual(folder, SCHEMAS[table])
    rows = []
    for row in frame.iter_rows(named=True):
        row["day"] = row["day"].isoformat() if row["day"] is not None else None
        rows.append(row)
    return rows


def diff_table(table, expected_rows, actual_rows, show):
    """Prints the differences of one table; returns True if there are none."""
    key_columns = KEYS[table]
    columns = list(SCHEMAS[table])

    def key(row):
        return tuple(row[column] for column in key_columns)

    expected = {key(row): row for row in expected_rows}
    actual = {}
    duplicate_keys = []
    for row in actual_rows:
        if key(row) in actual:
            duplicate_keys.append(key(row))
        actual[key(row)] = row
    if len(expected) != len(expected_rows):
        raise ValueError(f"pyref wrote duplicate keys in {table}")

    def sortable(k):
        return tuple("" if value is None else str(value) for value in k)

    missing = sorted((k for k in expected if k not in actual), key=sortable)
    extra = sorted((k for k in actual if k not in expected), key=sortable)
    different = sorted((k for k in expected if k in actual and expected[k] != actual[k]), key=sortable)
    same = not (missing or extra or different or duplicate_keys)
    print(f"  {table}: pyref rows {len(expected_rows)}, implementation rows {len(actual_rows)}; "
          f"missing {len(missing)}, extra {len(extra)}, different {len(different)}, "
          f"duplicate keys {len(duplicate_keys)} -> {'match' if same else 'DIFFER'}")
    for column in columns:
        if column in INT_COLUMNS:
            total_expected = sum(row[column] for row in expected_rows)
            total_actual = sum(row[column] for row in actual_rows if row[column] is not None)
            if total_expected != total_actual:
                print(f"    sum({column}): pyref {total_expected}, implementation {total_actual}")
    for k in missing[:show]:
        print(f"    missing in implementation: {expected[k]}")
    for k in extra[:show]:
        print(f"    only in implementation:    {actual[k]}")
    for k in different[:show]:
        changed = [column for column in columns if expected[k][column] != actual[k][column]]
        print(f"    different at {dict(zip(key_columns, k))} in {changed}:")
        print(f"      pyref          {expected[k]}")
        print(f"      implementation {actual[k]}")
    for k in sorted(set(duplicate_keys), key=sortable)[:show]:
        print(f"    duplicate key in implementation: {dict(zip(key_columns, k))}")
    return same


def check(name, data_dir, base_dir, command, cache, show):
    """pyref and the implementation on one dataset. Returns True if both tables match."""
    pyref_out = os.path.join(base_dir, "pyref")
    impl_out = os.path.join(base_dir, "impl")
    if not run_pyref(data_dir, pyref_out, cache):
        return False
    if not run_implementation(command, data_dir, impl_out):
        return False
    same = True
    for table in SCHEMAS:
        try:
            expected_rows = csv_rows(os.path.join(pyref_out, f"{table}.csv"), table)
            actual_rows = parquet_rows(os.path.join(impl_out, table), table)
        except Exception as error:  # a wrong schema, no Parquet, an unreadable file
            print(f"  {table}: cannot load: {type(error).__name__}: {error}")
            same = False
            continue
        if not diff_table(table, expected_rows, actual_rows, show):
            same = False
    return same


def main():
    options, command = parse_args(sys.argv[1:])
    command[0] = shutil.which(command[0]) or command[0]
    results = []

    for data_dir in options["data"]:
        data_dir = os.path.abspath(data_dir)
        tag = hashlib.sha1(data_dir.encode()).hexdigest()[:8]
        name = f"data_{os.path.basename(data_dir)}_{tag}"
        print(f"== {data_dir}")
        results.append((data_dir, check(name, data_dir, os.path.join(options["work"], name),
                                        command, cache=False, show=options["show"])))

    if not options["data"]:
        for events in options["events"]:
            for seed in options["seeds"]:
                clicks = options["clicks"] if options["clicks"] is not None else events // 4
                name = f"s{seed}_e{events}_c{clicks}"
                base_dir = os.path.join(options["work"], name)
                data_dir = os.path.join(base_dir, "data")
                print(f"== seed {seed}, {events} events, {clicks} clicks  ({base_dir})")
                ok = generate(options, seed, events, clicks, data_dir) and \
                    check(name, data_dir, base_dir, command, cache=True, show=options["show"])
                results.append((name, ok))

    failed = [name for name, ok in results if not ok]
    print()
    for name, ok in results:
        print(f"{'MATCH ' if ok else 'DIFFER'} {name}")
    print(f"\n{len(results) - len(failed)}/{len(results)} datasets match")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
