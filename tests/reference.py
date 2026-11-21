"""Small-data oracle. Uses graph traversal, datetime and exhaustive click selection."""

import argparse
import csv
from collections import defaultdict
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import re


DAY = 86_400_000
EPOCH = datetime(1970, 1, 1, tzinfo=timezone.utc)
ISO = re.compile(rb"([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})(Z|([+-])([0-9]{2}):([0-9]{2}))")
COLUMNS = {
    "user_daily": ("user_id", "day", "sessions", "events", "revenue_cents", "top_category"),
    "campaign_daily": ("campaign_id", "channel", "day", "sessions", "purchasing_sessions", "revenue_cents", "assisted_sessions"),
}


def records(paths, width):
    for path in paths:
        with path.open("rb") as stream:
            next(stream, None)
            for line in stream:
                fields = line.removesuffix(b"\n").split(b",")
                if len(fields) != width:
                    continue
                for i, field in enumerate(fields):
                    if len(field) >= 2 and field.startswith(b'"') and field.endswith(b'"'):
                        field = field[1:-1]
                    if b'"' in field:
                        break
                    fields[i] = field
                else:
                    yield fields


def timestamp(field):
    if field.isdigit():
        significant = field.lstrip(b"0")
        return int(significant or b"0") if len(significant) <= 19 else None
    match = ISO.fullmatch(field)
    if match is None:
        return None
    offset = 0
    if match[7] != b"Z":
        hours, minutes = int(match[9]), int(match[10])
        if hours > 23 or minutes > 59:
            return None
        offset = (hours * 60 + minutes) * (1 if match[8] == b"+" else -1)
    try:
        local = datetime(*map(int, match.groups()[:6]), tzinfo=timezone.utc)
    except ValueError:
        return None
    return (local - EPOCH) // timedelta(milliseconds=1) - offset * 60_000


def people(data):
    graph = defaultdict(set)
    for a, b in records((data / "links").glob("*.csv"), 2):
        if a and b:
            graph[a].add(b)
            graph[b].add(a)
    names = {}
    for start in graph:
        if start in names:
            continue
        stack, component = [start], {start}
        while stack:
            for other in graph[stack.pop()] - component:
                component.add(other)
                stack.append(other)
        name = min(component)
        names.update(dict.fromkeys(component, name))
    return names


def solve(data):
    windows = json.loads((data / "manifest.json").read_bytes())
    begin, end = (timestamp(windows[k].encode()) for k in ("window_start", "window_end"))
    products = dict(records([data / "products.csv"], 2))
    campaigns = dict(records([data / "campaigns.csv"], 2))
    names = people(data)
    earliest = {}
    for eid, uid, ts, ingest, kind, product, price in records((data / "events").glob("*.csv"), 7):
        if eid and (eid not in earliest or earliest[eid][0] > ingest):
            earliest[eid] = (ingest, uid, ts, kind, product, price)
    events = defaultdict(list)
    for ingest, uid, ts, kind, product, price in earliest.values():
        time = timestamp(ts)
        if not uid or time is None or not begin <= time < end or timestamp(ingest) - time > DAY:
            continue
        if not product or kind not in (b"view", b"cart", b"purchase"):
            continue
        amount = 0
        if kind == b"purchase":
            significant = price.lstrip(b"0")
            if not price.isdigit() or not significant or len(significant) > 8:
                continue
            amount = int(significant)
            if amount > 10_000_000:
                continue
        events[names.get(uid, uid)].append((time, products.get(product, b"unknown"), amount))
    clicks = defaultdict(set)
    for cid, uid, campaign, ts in records((data / "clicks").glob("*.csv"), 4):
        time = timestamp(ts)
        if cid and uid and campaign in campaigns and time is not None and begin - 7 * DAY <= time < end:
            clicks[names.get(uid, uid)].add((time, cid, campaign))

    users = defaultdict(lambda: [0, 0, 0, defaultdict(int)])
    totals = defaultdict(lambda: [0, 0, 0, 0])
    for uid, rows in events.items():
        sessions = []
        for row in sorted(rows):
            if not sessions or row[0] - sessions[-1][-1][0] > 1_800_000:
                sessions.append([])
            sessions[-1].append(row)
        for session in sessions:
            start = session[0][0]
            day = (EPOCH + timedelta(milliseconds=start)).date()
            user = users[uid, day]
            user[0] += 1
            user[1] += len(session)
            revenue = sum(row[2] for row in session)
            user[2] += revenue
            for _, category, amount in session:
                if amount:
                    user[3][category] += amount
            eligible = [click for click in clicks[uid] if start - 7 * DAY <= click[0] <= start]
            for campaign in {click[2] for click in eligible}:
                totals[campaign, day][3] += 1
            winner = min(eligible, key=lambda c: (-c[0], c[1]))[2] if eligible else b"organic"
            credited = totals[winner, day]
            credited[0] += 1
            credited[1] += bool(revenue)
            credited[2] += revenue
    user_rows = []
    for (uid, day), (sessions, count, revenue, categories) in users.items():
        top = min(categories, key=lambda c: (-categories[c], c)).decode() if categories else None
        user_rows.append((uid.decode(), day, sessions, count, revenue, top))
    campaign_rows = [(cid.decode(), campaigns.get(cid, b"organic").decode(), day, *values)
                     for (cid, day), values in totals.items()]
    return {"user_daily": sorted(user_rows), "campaign_daily": sorted(campaign_rows)}


def write_answer(data, output):
    output.mkdir(parents=True, exist_ok=True)
    for table, rows in solve(data).items():
        with (output / f"{table}.csv").open("w", encoding="utf-8", newline="") as stream:
            writer = csv.writer(stream)
            writer.writerow(COLUMNS[table])
            writer.writerows(rows)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    write_answer(args.input, args.output)
