#!/usr/bin/env python3
"""
shrink.py: reduces a dataset on which an implementation disagrees with pyref to a
small one that still shows the disagreement.

Usage (run with the Python that has polars):
  python checks/shrink.py DATA_DIR OUT_DIR -- CMD...

CMD is called like in scripts/run_fixtures.py (DATA_DIR and OUTPUT_DIR in the
environment and as the last two arguments; it writes the Parquet folders
user_daily/ and campaign_daily/).

How: delta debugging. First whole people are removed (a person's link lines,
events and clicks go together, and people that share an event_id stay
together), then single lines, as long as pyref and CMD still disagree; last,
products.csv and campaigns.csv are cut to the IDs still used. A reduction is
only accepted when the kind of disagreement stays the same (different tables,
or a failing CMD).

Writes OUT_DIR/input/ (the small dataset, one file per folder) and
OUT_DIR/expected/ (pyref's tables for it, as CSV), so OUT_DIR's parent can be
given to scripts/run_fixtures.py, and prints the remaining lines and both
sides' rows.
"""
import collections
import math
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import crosscheck  # noqa: E402  (also puts scripts/ on the path)
import pyref  # noqa: E402

TABLES = ("user_daily", "campaign_daily")
KINDS = ("events", "clicks", "links")
HEADERS = {
    "events": b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n",
    "clicks": b"click_id,user_id,campaign_id,ts\n",
    "links": b"user_id_a,user_id_b\n",
}


def text(value):
    return None if value is None else value.decode("utf-8", errors="surrogateescape")


def write_dataset(folder, lines, lookups):
    """lines: [(kind, line bytes)]; lookups: {file name: bytes}."""
    for name, content in lookups.items():
        with open(os.path.join(folder, name), "wb") as handle:
            handle.write(content)
    for kind in KINDS:
        os.makedirs(os.path.join(folder, kind), exist_ok=True)
        with open(os.path.join(folder, kind, "part-00000.csv"), "wb") as handle:
            handle.write(HEADERS[kind])
            for line_kind, line in lines:
                if line_kind == kind:
                    handle.write(line + b"\n")


def pyref_rows(data_dir):
    """pyref's two tables as sets of tuples of str/int/None, or None if pyref refuses the input."""
    try:
        user_rows, campaign_rows = pyref.build_tables(data_dir)
    except SystemExit:
        return None
    users = {(text(r[0]), text(r[1]), r[2], r[3], r[4], text(r[5])) for r in user_rows}
    campaigns = {(text(r[0]), text(r[1]), text(r[2]), r[3], r[4], r[5], r[6]) for r in campaign_rows}
    return users, campaigns


def implementation_rows(command, data_dir, out_dir):
    """The implementation's two tables in the same shape, or the string 'failed'."""
    shutil.rmtree(out_dir, ignore_errors=True)
    os.makedirs(out_dir)
    env = dict(os.environ, DATA_DIR=data_dir, OUTPUT_DIR=out_dir)
    done = subprocess.run(command + [data_dir, out_dir], env=env, capture_output=True)
    if done.returncode != 0:
        return "failed"
    try:
        tables = []
        for table in TABLES:
            columns = list(crosscheck.SCHEMAS[table])
            rows = crosscheck.parquet_rows(os.path.join(out_dir, table), table)
            tables.append([tuple(row[column] for column in columns) for row in rows])
    except Exception:
        return "failed"
    if any(len(rows) != len(set(rows)) for rows in tables):
        return "failed"          # duplicate rows count as a failure of their own
    return set(tables[0]), set(tables[1])


class Checker:
    def __init__(self, command, lookups):
        self.command, self.lookups = command, lookups
        self.work = tempfile.mkdtemp(prefix="shrink-")
        self.runs = 0

    def disagreement(self, lines, lookups=None):
        """None if pyref and the implementation agree on this dataset, else 'failed' or 'tables'."""
        self.runs += 1
        data_dir = os.path.join(self.work, "data")
        shutil.rmtree(data_dir, ignore_errors=True)
        os.makedirs(data_dir)
        write_dataset(data_dir, lines, lookups or self.lookups)
        expected = pyref_rows(data_dir)
        if expected is None:
            return None
        actual = implementation_rows(self.command, data_dir, os.path.join(self.work, "out"))
        if actual == "failed":
            return "failed"
        return None if actual == expected else "tables"


def ddmin(items, still_fails):
    """The classic delta-debugging loop over complements."""
    pieces = 2
    while len(items) >= 2:
        size = math.ceil(len(items) / pieces)
        chunks = [items[i:i + size] for i in range(0, len(items), size)]
        for skip in range(len(chunks)):
            rest = [item for index, chunk in enumerate(chunks) if index != skip for item in chunk]
            if still_fails(rest):
                items = rest
                pieces = max(pieces - 1, 2)
                break
        else:
            if pieces >= len(items):
                break
            pieces = min(len(items), pieces * 2)
    return items


def people_groups(data_dir, lines):
    """Groups of lines that belong together: one person, or people sharing an event_id."""
    name_of = pyref.read_people(data_dir)
    parent = {}

    def find(x):
        while parent.setdefault(x, x) != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    owner_of_event = {}
    owners = []
    for kind, line in lines:
        fields = pyref.parse_line(line, {"events": 7, "clicks": 4, "links": 2}[kind])
        if fields is None:
            owner = b"\x00malformed"
        elif kind == "links":
            owner = name_of.get(fields[0], fields[0]) if fields[0] and fields[1] else b"\x00malformed"
        else:
            owner = name_of.get(fields[1], fields[1]) or b"\x00no user"
            if kind == "events" and fields[0]:
                first = owner_of_event.setdefault(fields[0], owner)
                parent[find(owner)] = find(first)
        owners.append(owner)
    groups = collections.defaultdict(list)
    for owner, entry in zip(owners, lines):
        groups[find(owner)].append(entry)
    return list(groups.values())


def trimmed_lookups(lookups, lines):
    """products.csv and campaigns.csv cut down to the IDs the remaining lines mention."""
    used = set()
    for kind, line in lines:
        for field in line.split(b","):
            used.add(field.strip(b'"'))
    trimmed = dict(lookups)
    for name in ("products.csv", "campaigns.csv"):
        rows = lookups[name].split(b"\n")
        kept = [row for row in rows[1:] if row and row.split(b",")[0] in used]
        trimmed[name] = b"\n".join([rows[0]] + kept) + b"\n"
    return trimmed


def main():
    sys.stdout.reconfigure(errors="backslashreplace")
    if "--" not in sys.argv or sys.argv.index("--") != 3:
        sys.exit(__doc__)
    data_dir, out_dir, command = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2]), sys.argv[4:]
    command[0] = shutil.which(command[0]) or command[0]

    lookups = {}
    for name in ("manifest.json", "products.csv", "campaigns.csv"):
        with open(os.path.join(data_dir, name), "rb") as handle:
            lookups[name] = handle.read()
    lines = [(kind, line) for kind in KINDS
             for path in pyref.csv_files(os.path.join(data_dir, kind)) for line in pyref.data_lines(path)]
    checker = Checker(command, lookups)

    kind_of_failure = checker.disagreement(lines)
    if kind_of_failure is None:
        print("pyref and the implementation agree on this dataset: nothing to shrink")
        return 0
    print(f"{len(lines)} lines; the disagreement: "
          f"{'the implementation fails' if kind_of_failure == 'failed' else 'different tables'}")

    def still_fails(candidate):
        return checker.disagreement(candidate) == kind_of_failure

    groups = people_groups(data_dir, lines)
    groups = ddmin(groups, lambda kept: still_fails([entry for group in kept for entry in group]))
    lines = [entry for group in groups for entry in group]
    print(f"after removing people: {len(groups)} group(s), {len(lines)} lines ({checker.runs} runs)")
    lines = ddmin(lines, still_fails)
    print(f"after removing lines: {len(lines)} lines ({checker.runs} runs)")
    smaller = trimmed_lookups(lookups, lines)
    if checker.disagreement(lines, smaller) == kind_of_failure:
        lookups = smaller

    shutil.rmtree(out_dir, ignore_errors=True)
    os.makedirs(os.path.join(out_dir, "input"))
    write_dataset(os.path.join(out_dir, "input"), lines, lookups)
    user_rows, campaign_rows = pyref.build_tables(os.path.join(out_dir, "input"))
    os.makedirs(os.path.join(out_dir, "expected"))
    pyref.write_csv(os.path.join(out_dir, "expected", "user_daily.csv"),
                    b"user_id,day,sessions,events,revenue_cents,top_category", user_rows)
    pyref.write_csv(os.path.join(out_dir, "expected", "campaign_daily.csv"),
                    b"campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions",
                    campaign_rows)

    print(f"\nsmall dataset written to {out_dir}")
    print(lookups["manifest.json"].decode().strip())
    for kind in KINDS:
        for line_kind, line in lines:
            if line_kind == kind:
                print(f"  {kind}: {line.decode('utf-8', errors='backslashreplace')}")
    expected = pyref_rows(os.path.join(out_dir, "input"))
    actual = implementation_rows(command, os.path.join(out_dir, "input"), os.path.join(checker.work, "out"))
    for index, table in enumerate(TABLES):
        print(f"  {table}, pyref:          {sorted(expected[index], key=repr)}")
        print(f"  {table}, implementation: "
              f"{'FAILED' if actual == 'failed' else sorted(actual[index], key=repr)}")
    shutil.rmtree(checker.work, ignore_errors=True)
    return 1


if __name__ == "__main__":
    sys.exit(main())
