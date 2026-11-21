#!/usr/bin/env python3
"""
Checks the sample files of the Parquet writer's self-test with polars and
with pyarrow: both must give exactly the rows of the JSON file next to each
sample, with string, date and integer columns.

Usage:
  SS_PARQUET_SAMPLES=DIR cargo test --release parquet_samples
  python check_parquet_samples.py DIR
"""
from datetime import date
import json
from pathlib import Path
import sys

import polars as pl
import pyarrow as pa
import pyarrow.parquet as pq

EPOCH = date(1970, 1, 1)
FIELDS = ["id", "key", "label", "tag", "day", "count", "sum"]
STRINGS = {"key", "label", "tag"}


def plain(row):
    return tuple((value - EPOCH).days if isinstance(value, date) else value for value in row)


def check(path):
    # `<name>-gzip.parquet` holds the rows of `<name>.json` too.
    rows = path.with_name(path.stem.removesuffix("-gzip") + ".json")
    expected = sorted(tuple(row) for row in json.loads(rows.read_text(encoding="utf-8")))
    frame = pl.read_parquet(path)
    table = pq.read_table(path)
    table.validate(full=True)
    fields = [name for name in FIELDS if name in frame.columns]
    assert set(frame.columns) == set(fields) == set(table.column_names), (frame.columns, table.column_names)
    for name in fields:
        polars_type, arrow_type = frame.schema[name], table.schema.field(name).type
        if name in STRINGS:
            assert polars_type == pl.String and pa.types.is_string(arrow_type), (name, polars_type, arrow_type)
        elif name == "day":
            assert polars_type == pl.Date and pa.types.is_date32(arrow_type), (name, polars_type, arrow_type)
        else:
            assert polars_type.is_integer() and pa.types.is_integer(arrow_type), (name, polars_type, arrow_type)
        if name != "label":
            assert frame[name].null_count() == 0 and table.column(name).null_count == 0, name
    from_polars = sorted(plain(row) for row in frame.select(fields).rows())
    from_arrow = sorted(plain(tuple(row[name] for name in fields)) for row in table.to_pylist())
    width = len(fields)
    expected = [row[:width] for row in expected]
    assert from_polars == expected, f"{path.name}: polars rows differ from the expected rows"
    assert from_arrow == expected, f"{path.name}: pyarrow rows differ from the expected rows"
    meta = pq.ParquetFile(path).metadata
    chunks = [meta.row_group(group).column(column) for group in range(meta.num_row_groups)
              for column in range(meta.num_columns)]
    kinds = sorted({encoding for chunk in chunks for encoding in chunk.encodings} | {chunk.compression for chunk in chunks})
    print(f"ok   {path.name:33s} {len(expected):6d} rows, {meta.num_row_groups:3d} row groups, {path.stat().st_size:8d} bytes, "
          f"{','.join(kinds)}")


def main():
    folder = Path(sys.argv[1])
    samples = sorted(folder.glob("*.parquet"))
    if not samples:
        sys.exit(f"no samples in {folder}")
    for path in samples:
        check(path)
    print(f"\n{len(samples)} samples: polars {pl.__version__} and pyarrow {pa.__version__} read every row as written")


if __name__ == "__main__":
    main()
