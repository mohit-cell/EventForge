"""End-to-end differential, boundary and stress tests for the production binary."""
import json
import os
from pathlib import Path
import random
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone

from reference import DAY, solve

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from run_fixtures import SCHEMAS, load_actual

HEAD = {
    "events": b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents",
    "clicks": b"click_id,user_id,campaign_id,ts",
    "links": b"user_id_a,user_id_b",
}
START = datetime(2028, 2, 27, tzinfo=timezone.utc)
BASE = int(START.timestamp()) * 1000
BINARY = Path(os.environ.get("EVENTFORGE_BIN", ROOT / "target/release/eventforge")).resolve()


def iso(ms, offset=0):
    wall = datetime(1970, 1, 1, tzinfo=timezone.utc) + timedelta(milliseconds=ms, minutes=offset)
    suffix = "Z" if not offset else f"{'+' if offset > 0 else '-'}{abs(offset) // 60:02}:{abs(offset) % 60:02}"
    return (wall.strftime("%Y-%m-%dT%H:%M:%S") + suffix).encode()


def setup_data(data):
    data.mkdir()
    for folder in HEAD:
        (data / folder).mkdir()
    (data / "manifest.json").write_text(json.dumps({
        "window_start": iso(BASE).decode(), "window_end": iso(BASE + 14 * DAY).decode(),
    }))
    (data / "products.csv").write_bytes('product_id,category\n"p1","ä"\np2,BOOK\np3, books\n'.encode())
    (data / "campaigns.csv").write_bytes(b'campaign_id,channel\nc1,search\nc2,social\n" c",other\n')


def write_part(data, kind, name, rows):
    (data / kind / f"{name}.csv").write_bytes(b"\n".join([HEAD[kind], *rows]))


def production(data, out, threads=4):
    result = subprocess.run([BINARY], env={**os.environ, "NCPU": str(threads),
                            "DATA_DIR": str(data), "OUTPUT_DIR": str(out)},
                            capture_output=True, check=True)
    assert not result.stdout
    return {table: sorted(load_actual(out / table, schema).rows()) for table, schema in SCHEMAS.items()}


def randomized(data, seed):
    rng = random.Random(seed)
    users = [b"u", b" U", b"U", b"u ", "ζ".encode(), b"a\0b", b"\t", b"long-user-123",
             b"long-user-124", b"x", b"y", b"orphan", b"", b"u\r"]
    links = [b'u," U"', b"x,y", b"y,!!unused", b"long-user-124,u ", b",orphan",
             b'a,b"c', b'u\r,\t', b"u,u", b'y,x,', b'long-user-123,long-user-123']
    origins = [BASE - 1, BASE, BASE + 1, BASE + DAY - 1, BASE + DAY, BASE + 7 * DAY, BASE + 14 * DAY]
    origins.extend(BASE + rng.randrange(14 * DAY) for _ in range(25))
    def instant():
        return rng.choice(origins) + rng.choice([0, 0, 1, -1, 1_800_000, 1_800_001, -7 * DAY])
    def form(ms):
        if ms % 1000 == 0 and rng.random() < .7:
            return iso(ms, rng.choice([0, 0, -240, 1439, -1439, 330]))
        return b"0" * rng.randrange(5) + str(ms).encode()
    def row(fields):
        return b",".join(b'"' + f + b'"' if rng.random() < .35 else f for f in fields)
    events, clicks = [], []
    for i in range(650):
        at = instant()
        uid = rng.choice(users)
        eid = f"event-long-{i}".encode() if i % 7 == 0 else f"e{i}".encode()
        ingest = at - at % 1000 + rng.choice([-1000, 0, 1000, DAY, DAY + 1000])
        kind = rng.choice([b"purchase", b"view", b"cart", b"view", b"Purchase", b"purchase"])
        fields = [eid, uid, form(at), iso(ingest), kind, rng.choice([b"p1", b"p2", b"p3", b"p9", b""]),
                  rng.choice([b"0", b"0007", b"10000000", b"10000001", b"-2", b"7", b"", "７".encode()])]
        if rng.random() < .10:
            fields[2] = rng.choice([b"", b"-7", b"9" * 40, b"2028-02-30T01:00:00Z", b"5e12"])
        events.append(row(fields))
        if rng.random() < .2:
            later = fields.copy()
            later[1:4] = [rng.choice(users), form(at), iso(ingest + 1000)]
            events.append(row(later))
        if rng.random() < .05:
            events.append(row(fields) + rng.choice([b',', b'"']))
    for i in range(450):
        cid = rng.choice([b"k", b"long-click-", "é".encode()]) + str(i).encode()
        fields = [cid, rng.choice(users), rng.choice([b"c1", b"c2", b" c", b"invalid"]), form(instant())]
        clicks.append(row(fields))
        if rng.random() < .35:
            clicks.append(row(fields))
    for kind, rows in [("events", events), ("clicks", clicks), ("links", links)]:
        rng.shuffle(rows)
        for i, part in enumerate((rows[:2], [], rows[2:30], rows[30:])):
            write_part(data, kind, i, part)


class PipelineTests(unittest.TestCase):
    def test_randomized_against_oracle(self):
        for seed in range(20):
            with self.subTest(seed=seed), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                data, out = root / "data", root / "out"
                setup_data(data)
                randomized(data, seed)
                expected = solve(data)
                self.assertEqual(production(data, out, 1 if seed % 2 else 8), expected)
                if seed in (0, 1):
                    self.assertEqual(production(data, out, 8 if seed % 2 else 1), expected)

    def test_empty_rerun_replaces_previous_tables(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            data, out = root / "data", root / "out"
            setup_data(data)
            randomized(data, 7)
            self.assertTrue(production(data, out)["user_daily"])
            for kind in HEAD:
                for part in (data / kind).glob("*.csv"):
                    write_part(data, kind, part.stem, [])
            self.assertEqual(production(data, out), {"user_daily": [], "campaign_daily": []})

    def test_failed_publish_preserves_previous_files(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            data, out = root / "data", root / "out"
            setup_data(data)
            randomized(data, 5)
            expected = production(data, out)["user_daily"]
            campaign = out / "campaign_daily/part-00000.parquet"
            campaign.unlink()
            campaign.mkdir()
            for part in (data / "events").glob("*.csv"):
                write_part(data, "events", part.stem, [])
            with self.assertRaises(subprocess.CalledProcessError):
                production(data, out)
            self.assertEqual(sorted(load_actual(out / "user_daily", SCHEMAS["user_daily"]).rows()), expected)
            campaign.rmdir()
            self.assertEqual(production(data, out), {"user_daily": [], "campaign_daily": []})

    def test_concurrent_publish_keeps_one_complete_result(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            out = root / "out"
            inputs = [root / "one", root / "two"]
            for seed, data in enumerate(inputs):
                setup_data(data)
                randomized(data, seed)
            procs = [subprocess.Popen([BINARY, data, out], env={**os.environ, "NCPU": "2"},
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE) for data in inputs]
            for proc in procs:
                _, error = proc.communicate()
                self.assertEqual(proc.returncode, 0, error)
            tables = {t: sorted(load_actual(out / t, schema).rows()) for t, schema in SCHEMAS.items()}
            self.assertIn(tables, [solve(data) for data in inputs])

    def test_multimegabyte_row_and_missing_final_lf(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            data, out = root / "data", root / "out"
            setup_data(data)
            stamp = iso(BASE)
            prefix = b"e1,u," + stamp + b"," + stamp + b",purchase,p1,"
            write_part(data, "events", 0, [prefix + b"0" * 2_100_000 + b"7", prefix.replace(b"e1", b"e2") + b"3"])
            self.assertEqual(production(data, out), solve(data))

    def test_large_component_whale_and_64bit_revenue(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            data, out = root / "data", root / "out"
            setup_data(data)
            n = 12_000
            ids = [f"u{i:05}".encode() for i in range(n)]
            # Several long chains reach the same unused shared device, across files.
            links = [ids[i] + b"," + (ids[i - 1] if i % 400 else b"!unused") for i in range(n)]
            for i in range(4):
                write_part(data, "links", i, links[i::4])
            stamps = [iso(BASE), iso(BASE + DAY)]
            with (data / "events/large.csv").open("wb") as events:
                events.write(HEAD["events"] + b"\n")
                for i in range(n * 8):
                    ts = stamps[i % 2]
                    events.write(b",".join([str(i).encode(), ids[i % n], ts, ts, b"purchase", b"p2", b"10000000"]) + b"\n")
            write_part(data, "clicks", 0, [b",".join([b"k" + str(i).encode(), uid,
                       b"c1" if i % 2 == 0 else b"c2", stamps[0]]) for i, uid in enumerate(ids)])
            rows = production(data, out, 8)
            count = n * 4
            self.assertEqual(rows["user_daily"], [("!unused", (START + timedelta(days=d)).date(),
                             1, count, count * 10_000_000, "BOOK") for d in range(2)])
            expected = [(campaign, channel, (START + timedelta(days=d)).date(),
                         win, win, count * 10_000_000 * win, 1)
                        for campaign, channel, win in [("c1", "search", 1), ("c2", "social", 0)] for d in range(2)]
            self.assertEqual(rows["campaign_daily"], expected)


if __name__ == "__main__":
    unittest.main()
