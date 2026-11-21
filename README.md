# EventForge

EventForge turns dirty shopping and advertising logs into two daily Parquet tables. It links shopper IDs before finding sessions, credits each session to its most recent eligible click, and counts every distinct campaign that assisted. A single Rust executable runs inside the scoring container.

## Build and run

Docker with Buildx and Make are sufficient to run a fixture from a clean checkout. The default image targets Linux amd64. This example compares the results with the supplied expectations using Python 3.10 or later.

~~~sh
make build
MEM=5g scripts/docker_run.sh fixtures/E3/input .work/example-out

python3 -m venv .venv
. .venv/bin/activate
python -m pip install -r requirements-test.txt
python scripts/compare.py .work/example-out fixtures/E3/expected
~~~

For a complete dataset, pass its root and a dedicated output directory to the same runner. The scorer supplies eight vCPUs and 24 GB. The runner defaults to those limits and accepts `CPUS` and `MEM` overrides for a smaller local Docker VM. It mounts the input read-only, disables networking, uses uid 1000, caps `/tmp` at 64 MB, and mounts `/output` and `/scratch` writable.

For a native build on macOS or Linux, install Rust 1.90 or later with Cargo. The image pins Rust 1.90.0 and both container base images by digest. `Cargo.lock` pins the Rust packages.

~~~sh
make native
NCPU=8 target/release/eventforge fixtures/E3/input .work/native-out
~~~

With no arguments, the executable reads `DATA_DIR` and `OUTPUT_DIR`, defaulting to `/data` and `/output`. `NCPU` sets the worker pool size. The pipeline uses memory for intermediate rows and logs phase times and row counts to stderr. The scorer enforces its memory limit externally.

## Inputs and outputs

Every CSV has a header. The pipeline reads all CSV parts in the three input subdirectories, including unevenly sized files and header-only files.

| Input | Columns or fields |
| --- | --- |
| `events/*.csv` | `event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents` |
| `clicks/*.csv` | `click_id,user_id,campaign_id,ts` |
| `links/*.csv` | `user_id_a,user_id_b` |
| `products.csv` | `product_id,category` |
| `campaigns.csv` | `campaign_id,channel` |
| `manifest.json` | `window_start` and `window_end` as UTC timestamps |

The manifest defines a half-open event window. Click eligibility extends seven days before its start. Timestamps accept whole-second UTC, whole-second signed offsets, and ASCII epoch milliseconds. The CSV parser preserves whitespace and carriage returns, strips matching whole-field quotes, and drops malformed physical lines. The full specification includes every boundary, tie-breaker, and output definition. [Competition rules](rules.md)

| Output folder | Row key | Other columns |
| --- | --- | --- |
| `user_daily/` | `user_id, day` | `sessions, events, revenue_cents, top_category` |
| `campaign_daily/` | `campaign_id, day` | `channel, sessions, purchasing_sessions, revenue_cents, assisted_sessions` |

Both tables use a Parquet `DATE`, UTF-8 strings and 64-bit integer measures. A person-day without purchases has a null `top_category`. An assist-only campaign-day has zero credited sessions, purchasing sessions and revenue. A successful run on empty input still emits both typed schemas.

For example, fixture E3 has a view by `u1` at 23:50 UTC on October 3 and a 1,000-cent shoe purchase at 00:10 the next day. The 20-minute gap makes one session. Its person-day is `(u1, 2026-10-03, 1, 2, 1000, shoes)` in schema order. The corresponding `organic` campaign-day has one credited session, one purchasing session, 1,000 cents and zero assists. [Fixture inputs and expected rows](fixtures/E3)

## How the pipeline works

1. **Resolve identities.** Union by size and path compression join every valid link while other workers parse events. Each component stores its byte-wise minimum ID independently of its tree root. This handles long chains, shared devices and names that appear only in links. Short nonzero ASCII IDs pack losslessly into 63 bits. Longer strings, Unicode and embedded zero bytes use a sharded dictionary with full byte equality.
2. **Read and deduplicate events.** Workers claim 1 MiB ranges in read-only memory maps and align them to LF boundaries. The worker owning a line's first byte processes the entire line. SIMD locates separators and quotes in short event and click rows on x86-64 and arm64. Each structurally valid, nonempty event ID enters one hash partition. A bitmap identifies hash slots with a single row. Rows in slots hit more than once are compared by exact ID and minimum ingest time. An invalid payload still competes for the minimum, so a later valid copy cannot rescue it. Purchases require 1 through 10,000,000 cents. The 24-hour lateness cutoff is inclusive.
3. **Form sessions.** Surviving events move into partitions by canonical person. Each partition groups by person, then sorts each person's events by millisecond time. When the manifest spans at most 2³¹ milliseconds and the catalog has at most 512 categories, each event takes 16 bytes in memory. A wider record supports larger windows and catalogs. Gaps over 1,800,000 ms start a new session. All events in a session use its UTC start day. Per-day category sums resolve revenue ties by byte order, including the fallback category `unknown`.
4. **Attribute sessions.** Clean clicks sort by person and time, with descending byte-wise click ID within a time tie. Identical copies become adjacent and collapse. Two forward-only cursors maintain the inclusive seven-day click window for each session. The last eligible click wins. Counts and an indexed list of active campaigns give one assist per qualifying campaign per session. Without an eligible click, `organic` receives the credit.
5. **Write the tables.** Workers aggregate person-days and campaign counters locally. Typical manifest windows use campaign counters indexed directly by day and campaign. Larger windows or catalogs use a sparse map. Eight user partitions encode Snappy Parquet in parallel, while campaign counters merge into one file. Each user row group stores IDs in one shared byte buffer during encoding. Every footer is finished before publication. A directory lock serializes publishers and temporary hard links support rollback on a rename error. Retries replace the same nine part names. Consumers should read after the process exits successfully.

For `E` structurally valid event copies, `C` clicks and `L` links, cleaning and hash partitioning are linear on average. Link operations are amortized near-constant. Sorting costs at most `O(E log E + C log C)`. The subsequent scan is linear in rows plus the number of distinct campaign assists counted. The implementation holds partitioned rows in RAM. Peak memory depends on row count and ID length. It assumes the specified clean lookups, valid F1 ingest timestamps, and identical click duplicates. Invalid manifest or filesystem operations exit with an error.

## Tests and public development data

The repository includes the DuckDB starter's 36 hand-written fixtures, restricted Docker runner and exact comparison scripts. The independent Python oracle uses graph traversal, standard-library calendar arithmetic and exhaustive selection of eligible clicks.

~~~sh
. .venv/bin/activate
make test
make fixtures MEM=5g
~~~

The native suite covers strict ASCII and calendar parsing, overflow, byte ordering, all short dialect inputs over a five-byte test alphabet, and all official fixtures. Additional end-to-end tests compare 20 seeded randomized datasets with the oracle, vary worker counts, exercise multi-megabyte lines, run concurrent publishers, and retry empty or failed outputs. A 96,000-purchase stress case joins 12,000 IDs across files and checks daily revenue of 480 billion cents per day.

The unmodified organizer generator is bundled as a small source archive. `make dev` needs Cargo and `unzip`. To reproduce the public dataset and compare every output row, put the competition's `dev-answer.zip` in the checkout and run the following commands.

~~~sh
make dev
unzip -oq dev-answer.zip -d .work
make run-dev MEM=5g
make check-dev
~~~

The public seed produced an exact match on **117,038 person-days and 27,400 campaign-days**, including schema and null handling, with zero missing rows, extra rows or duplicate keys. The Python oracle also reproduced both public answer tables and agreed with the Rust engine on a separate 100,000-event seed. For a seed without a published answer, `python scripts/check_totals.py OUTPUT` verifies keys, nullability, credited totals, purchasing counts, and assist invariants.

The independent baseline took 28.47 s in one native run on the public 1M seed, including its CSV output. To reproduce that check, run the oracle on the generated input and pass its output directory to the same comparator.

~~~sh
python tests/reference.py .work/dev .work/oracle-dev
python scripts/compare.py .work/oracle-dev .work/dev-answer
~~~

## Performance

The current implementation was measured against `6a82323` on an Apple silicon Mac with 16 GiB of memory, Rust 1.90.0, and eight workers. The binaries alternated after a warm-up. Timings cover the full process, including Parquet output.

| Generated events | Generated clicks | Seed | Before | After | Reduction |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 20,000,000 | 5,000,000 | 73 | 0.763 s | 0.619 s | 18.9% |
| 60,000,000 | 15,000,000 | 97 | 6.079 s | 5.506 s | 9.4% |

The 20M median uses five measured runs per binary, and the 60M median uses three. Both Parquet tables matched the baseline row for row on both datasets. The 1M dev seed matched the independent Python oracle. The buildathon's Linux amd64 container and hidden inputs require a separate timing.

### Earlier optimization

Compared with `081cac3`, the optimized build reduces end-to-end native runtime on both the practice-scale and final-scale generated datasets. These measurements used Rust 1.90.0 release builds, eight workers and an Apple silicon Mac with 16 GiB of memory. Each median covers three measured runs per binary after a warm-up, with the execution order alternating.

| Generated events | Generated clicks | Seed | Before | After | Reduction |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 20,000,000 | 5,000,000 | 73 | 1.106 s | 0.748 s | 32% |
| 60,000,000 | 15,000,000 | 97 | 6.058 s | 4.334 s | 28% |

Both output tables matched the baseline row for row at both scales. The 60M run accounted for 10,067,832 sessions and 17,827,491,900 cents. The 1M dev seed matched the independent Python oracle. These local benchmarks used generated data and the native executable. The buildathon scorer runs a Linux amd64 container on hidden inputs.

### Prior benchmarks

The following native measurements used an eight-core Apple M1 with 8 GiB of physical memory, Rust 1.90.0 release mode and eight workers. Each median covers three complete process invocations following one warm-up. Input generation and result comparison are timed separately. Counts in the first two columns are generator arguments before dirt and duplicate copies.

| Events | Clicks | Seed | Measured runs in seconds | Median |
| ---: | ---: | ---: | --- | ---: |
| 100,000 | 25,000 | 23 | 0.054, 0.053, 0.053 | 0.053 s |
| 1,000,000 | 250,000 | 1 | 0.171, 0.180, 0.164 | 0.171 s |
| 5,000,000 | 1,250,000 | 47 | 0.722, 0.732, 0.794 | 0.732 s |
| 20,000,000 | 5,000,000 | 73 | 5.256, 7.624, 5.312 | 5.312 s |
| 60,000,000 | 15,000,000 | 97 | 31.657, 21.707, 23.414 | 23.414 s |

On the 20M seed the first correct native build took a median of 7.419 s. Phase timings identified event input and Parquet output as tuning targets. Workers now resolve file boundaries in parallel, scan a field for quotes and delimiters together, and encode user files concurrently. Median event parsing fell from 3.254 s to 2.492 s and median Parquet output from 0.750 s to 0.338 s in these runs. All 2,230,619 person-days and 28,014 campaign-days matched the earlier build. A separate final invocation reached 1.90 GiB maximum RSS as reported by macOS `/usr/bin/time -l`. Disk cache state and host memory pressure affect these times. Official scores require the organizer's Intel machine and hidden inputs.

The final Linux amd64 image passed all 36 fixtures and the public dev comparison in a network-isolated container with a read-only root, uid 1000 and a 5 GiB cgroup limit. The same container completed the 20M seed in 22.07 s including the Docker Desktop wrapper on Apple silicon. Its two tables matched native execution exactly. Docker reports an image size of 29.0 MB.

The 60M seed contained 5.2 GiB of CSV inputs and one event file over 1 GB. Native peak child RSS across the warm-up and three runs was 2.883 GiB. The two tables contained 6,655,189 person-days and 28,014 campaign-days. The accounting check reconciled 10,067,832 sessions and 17,827,491,900 cents. The emulated Linux amd64 run completed in 169.34 s including the Docker wrapper, with a 6 GiB cgroup limit and swap disabled inside the container. All five Parquet files matched the native files byte for byte. The organizers supply a row-level reference for the public dev seed. The larger generated seeds are local scaling and consistency checks.

~~~sh
make dev SEED=73 EVENTS=20000000 CLICKS=5000000 DEV=.work/practice
make benchmark DEV=.work/practice OUT=.work/practice-out
python scripts/check_totals.py .work/practice-out

make dev SEED=97 EVENTS=60000000 CLICKS=15000000 DEV=.work/final-scale
make benchmark DEV=.work/final-scale OUT=.work/final-out
python scripts/check_totals.py .work/final-out
~~~

## Repository layout

| Location | Contents |
| --- | --- |
| `src/parse.rs`, `input.rs`, `keys.rs` | Strict field and timestamp parsing, work distribution and exact ID storage |
| `src/catalog.rs`, `pipeline.rs` | Dimensions, identity graph, deduplication and person partitioning |
| `src/aggregate.rs`, `output.rs` | Sessions, attribution, daily aggregation and Parquet publication |
| `src/tests.rs`, `tests/`, `fixtures/` | Unit tests, independent oracle, randomized and integration tests, official fixtures |
| `scripts/`, `tools/generator.zip` | Scorer-style execution, comparison, accounting, benchmarking and dataset generation |
| `Dockerfile`, `Cargo.lock`, `Makefile` | Pinned image and dependencies, build and test entry points |

## Competition submission

Publish the tested commit to a public GitHub repository with the Dockerfile at the root. Enter its repository URL and full 40-character commit hash in the submission form. The commit field is optional and pins the revision to score. The scorer builds the executable and mounts the dataset. Keep every submitted commit publicly fetchable until results are published. The supplied competition brief allows two submissions per hour and lists a cutoff of Sunday at 11:00 a.m. ET.
