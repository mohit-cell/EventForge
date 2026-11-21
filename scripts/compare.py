#!/usr/bin/env python3
"""Compare all rows, nulls, schemas and key uniqueness independently of file and row order."""
import argparse
from pathlib import Path
import sys

import polars as pl
from run_fixtures import SCHEMAS, load_actual, load_expected


def load(folder, table, predicate):
    csv = folder / f"{table}.csv"
    if csv.is_file():
        frame = load_expected(csv, SCHEMAS[table])
        return frame if predicate is None else frame.filter(predicate)
    return load_actual(folder / table, SCHEMAS[table], predicate)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("left", type=Path)
    parser.add_argument("right", type=Path)
    parser.add_argument("--buckets", type=int, default=1,
                        help="compare one key partition at a time to bound memory on large tables")
    args = parser.parse_args()
    if args.buckets < 1:
        parser.error("--buckets must be positive")
    matched = True
    for table, schema in SCHEMAS.items():
        keys = [column for column in ("user_id", "campaign_id", "day") if column in schema]
        heights = [0, 0]
        duplicates = [0, 0]
        missing = [0, 0]
        for index in range(args.buckets):
            predicate = None
            if args.buckets > 1:
                # The hash only partitions work. Both comparisons use every original column.
                predicate = pl.col(keys[0]).hash(seed=0).fill_null(0).mod(args.buckets) == index
            a, b = (load(folder, table, predicate) for folder in (args.left, args.right))
            for side, (frame, other) in enumerate(((a, b), (b, a))):
                heights[side] += frame.height
                duplicates[side] += frame.height - frame.n_unique(subset=keys)
                diff = frame.join(other, on=list(schema), how="anti", nulls_equal=True)
                if diff.height and missing[side] == 0:
                    print(diff.sort(keys).head(5))
                missing[side] += diff.height
        print(f"{table}: rows {heights[0]} vs {heights[1]}; only in A {missing[0]}; "
              f"only in B {missing[1]}; duplicate keys {duplicates[0]} vs {duplicates[1]}")
        if any(duplicates) or any(missing):
            matched = False
    print("MATCH" if matched else "DIFFER")
    return 0 if matched else 1


if __name__ == "__main__":
    sys.exit(main())
