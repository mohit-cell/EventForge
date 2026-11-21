#!/usr/bin/env python3
"""Time complete invocations. Input generation and verification are separate."""
import argparse
import os
from pathlib import Path
import resource
from statistics import median
import subprocess
import sys
from time import perf_counter


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--binary", default="target/release/eventforge")
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--warmup", type=int, default=1)
    args = parser.parse_args()
    if min(args.runs, args.threads) < 1 or args.warmup < 0:
        parser.error("runs and threads must be positive, warmup must be nonnegative")
    elapsed = []
    for i in range(-args.warmup, args.runs):
        begin = perf_counter()
        subprocess.run([args.binary, args.data, args.output], check=True,
                       env={**os.environ, "NCPU": str(args.threads)})
        wall = perf_counter() - begin
        if i < 0:
            print(f"warmup  {wall:.3f} s", flush=True)
        else:
            elapsed.append(wall)
            print(f"run {i + 1}  {wall:.3f} s", flush=True)
    print(f"median {median(elapsed):.3f} s")
    rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    rss_bytes = rss if sys.platform == "darwin" else rss * 1024
    print(f"peak child RSS {rss_bytes / (1024 ** 3):.3f} GiB")


if __name__ == "__main__":
    main()
