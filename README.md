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

For a native build on macOS or Linux, install Rust 1.90 or later with Cargo. The image pins Rust 1.90.0 and both container base images by digest. Its amd64 build targets x86-64-v3 for the scorer’s Intel Cascade Lake or newer processors. Native builds use the toolchain’s default CPU target. `Cargo.lock` pins the Rust packages.

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

1. **Read and validate CSV.** Workers read 1 MiB ranges into reusable buffers with positional reads. The worker owning the first byte of a row follows it to LF or EOF, including rows spanning several ranges. A 64-byte SIMD scan finds line ends, commas and quotes together. Short rows validate quote pairs from these masks. Long rows use the scalar dialect parser. Each worker caches fully validated calendar dates by their exact ten input bytes. Product and campaign lookups use dense arrays for matching decimal suffixes and full byte keys for other spellings.
2. **Link identities.** Union by size and path compression run alongside event parsing. Each component retains its byte-wise minimum name, including aliases found only in links. Nine nonzero ASCII bytes fit losslessly in 63 bits. Other IDs use a sharded dictionary that compares full strings. Stable partition bits and byte-wise ordering keep output reproducible across worker counts. A small worker-local cache resolves repeated aliases.
3. **Deduplicate events.** All structurally valid copies with a nonempty ID compete in the same partition, including invalid payloads. Two bitmaps identify singleton hash slots. Repeated slots use full ID equality and the earliest ingest. Common view and cart candidates occupy 24 bytes. Purchases and wide time ranges retain their full payload in a separate array. Full candidate pages can be traversed in place. Only validated winners move to canonical-person partitions.
4. **Form and attribute sessions.** A counting pass assigns each person contiguous event and click ranges and drops click-only people. Each range sorts independently. Large compact event ranges use a three-pass integer radix sort. Wide windows and catalogs retain full-width events. Identical clicks collapse. Two forward cursors maintain the inclusive seven-day window and track distinct assisting campaigns. Byte-wise click-ID ties, the 30-minute gap and UTC start day determine the final counters.
5. **Write the tables.** Eight workers each own one user Parquet part. They group their person partitions and write completed 65,536-row batches while other partitions are still processing. Each worker reuses its campaign, category and attribution counters. Small campaign-by-day grids use dense counters and large grids use a sparse map. Campaign totals merge into one file. Writers finish all nine Snappy Parquet footers before publishing under a directory lock. Retries replace the same part names and temporary hard links support rollback if publication fails.

For `E` structurally valid event copies, `C` clicks and `L` links, cleaning and hash partitioning are linear on average. Link operations are amortized near-constant. Sorting costs at most `O(E log E + C log C)`. The subsequent scan is linear in rows plus the number of distinct campaign assists counted. The implementation holds partitioned rows in RAM. Peak memory depends on row count and ID length. It assumes the specified clean lookups, valid F1 ingest timestamps, and identical click duplicates. Invalid manifest or filesystem operations exit with an error.

## Tests and public development data

The repository includes the DuckDB starter's 36 hand-written fixtures, restricted Docker runner and exact comparison scripts. The independent Python oracle uses graph traversal, standard-library calendar arithmetic and exhaustive selection of eligible clicks.

~~~sh
. .venv/bin/activate
make test
make fixtures MEM=5g
~~~

The native suite covers strict ASCII and calendar parsing, date-cache collisions, compact and wide candidate boundaries, real buffer splits, radix ordering, and the official fixtures. Exhaustive short dialect inputs and malformed records at all 64 SIMD block offsets are checked against independent or scalar parsers. Additional end-to-end tests compare 20 seeded randomized datasets with the oracle, vary worker counts, exercise multi-megabyte lines, run concurrent publishers, and retry empty or failed outputs. A separate 64,000-event Unicode-ID case compares the actual Parquet bytes with one and eight workers. A 96,000-purchase stress case joins 12,000 IDs across files and checks daily revenue of 480 billion cents per day.

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

On a native eight-core Apple M1 with 8 GiB of RAM, the released binary processed 20M generated events and 5M clicks (seed 73) in a median of **3.200 s**. Median process CPU time was 12.604 s and the highest measured resident set was 0.963 GiB. All 2,230,619 person-days and 28,014 campaign-days matched the previous main commit, `026f700`, including nulls and key uniqueness.

Each comparison below used eight workers, one warm-up and three complete process runs per binary. The two rows were timed in separate rounds. The scorer uses the median container start-to-exit time on hidden Intel data. [Organizer scoring rules](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions)

| Local comparison on seed 73 | Before | After |
| --- | ---: | ---: |
| Previous main to the buffered-reader and quote-validation version | 4.599 s | 3.599 s |
| Streaming output version to the released version with paged event buffers | 3.282 s | 3.200 s |

To compare a saved release binary with the current checkout, use a dedicated output directory. The comparator’s optional bucket count bounds memory on large outputs. Keys in a bucket still undergo full row comparison.

~~~sh
python scripts/benchmark.py .work/practice .work/paired --baseline /path/to/previous/eventforge --binary target/release/eventforge --runs 3
python scripts/compare.py .work/paired/baseline .work/paired/candidate --buckets 8
~~~

### Previous revision

The preceding implementation was measured against `6a82323` on an Apple silicon Mac with 16 GiB of memory, Rust 1.90.0, and eight workers. The binaries alternated after a warm-up. Timings cover the full process, including Parquet output.

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
| `src/csv.rs`, `parse.rs`, `input.rs`, `keys.rs` | Strict field and timestamp parsing, work distribution and exact ID storage |
| `src/catalog.rs`, `pipeline.rs` | Dimensions, identity graph, deduplication and person partitioning |
| `src/aggregate.rs`, `output.rs` | Sessions, attribution, daily aggregation and Parquet publication |
| `src/tests.rs`, `tests/`, `fixtures/` | Unit tests, independent oracle, randomized and integration tests, official fixtures |
| `scripts/`, `tools/generator.zip` | Scorer-style execution, comparison, accounting, benchmarking and dataset generation |
| `Dockerfile`, `Cargo.lock`, `Makefile` | Pinned image and dependencies, build and test entry points |

## Competition submission

Publish the tested commit to a public GitHub repository with the Dockerfile at the root. Enter its repository URL and full 40-character commit hash in the submission form. The commit field is optional and pins the revision to score. The scorer builds the executable and mounts the dataset. Keep every submitted commit publicly fetchable until results are published. The supplied competition brief allows two submissions per hour and lists a cutoff of Sunday at 11:00 a.m. ET.
