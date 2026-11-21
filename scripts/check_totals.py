#!/usr/bin/env python3
"""Check output schema, key uniqueness and accounting when no reference answer exists."""
import argparse
from pathlib import Path

import polars as pl
from run_fixtures import SCHEMAS, load_actual


def check(folder):
    users, campaigns = (load_actual(folder / table, schema) for table, schema in SCHEMAS.items())
    errors = []
    for frame, keys in [(users, ["user_id", "day"]), (campaigns, ["campaign_id", "day"])]:
        if frame.n_unique(subset=keys) != frame.height:
            errors.append(f"duplicate {keys}")
        for name in frame.columns:
            if name != "top_category" and frame[name].null_count():
                errors.append(f"unexpected null in {name}")
    sessions = users["sessions"].sum()
    revenue = users["revenue_cents"].sum()
    if users.filter((pl.col("sessions") <= 0) | (pl.col("events") < pl.col("sessions"))
                    | (pl.col("revenue_cents") < 0)
                    | ((pl.col("revenue_cents") == 0) != pl.col("top_category").is_null())).height:
        errors.append("invalid person-day counters or category")
    if campaigns["sessions"].sum() != sessions or campaigns["revenue_cents"].sum() != revenue:
        errors.append("credited sessions or revenue disagree with person-day totals")
    if campaigns.filter((pl.col("sessions") < pl.col("purchasing_sessions")) |
                        (pl.col("purchasing_sessions") < 0) | (pl.col("assisted_sessions") < 0) |
                        (pl.col("revenue_cents") < 0) |
                        ((pl.col("purchasing_sessions") == 0) != (pl.col("revenue_cents") == 0)) |
                        ((pl.col("sessions") == 0) & (pl.col("assisted_sessions") == 0)) |
                        ((pl.col("campaign_id") == "organic") &
                         ((pl.col("assisted_sessions") != 0) | (pl.col("channel") != "organic"))) |
                        ((pl.col("campaign_id") != "organic") &
                         (pl.col("assisted_sessions") < pl.col("sessions")))).height:
        errors.append("invalid campaign-day counters or organic channel")
    if errors:
        raise ValueError(", ".join(errors))
    print(f"Accounting checks passed for {len(users)} person-days, {len(campaigns)} campaign-days, "
          f"{sessions} sessions and {revenue} cents")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    check(parser.parse_args().output)
