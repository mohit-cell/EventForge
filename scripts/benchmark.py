#!/usr/bin/env python3
"""Time complete processes, optionally alternating two binaries on the same input."""
import argparse
from dataclasses import dataclass
import os
from pathlib import Path
from statistics import median
import subprocess
import sys
from time import perf_counter


@dataclass
class Run:
    wall: float
    cpu: float
    rss: float


def measure(binary, data, output, threads):
    begin = perf_counter()
    with subprocess.Popen([binary, data, output], env={**os.environ, "NCPU": str(threads)}) as proc:
        _, status, usage = os.wait4(proc.pid, 0)
        proc.returncode = os.waitstatus_to_exitcode(status)
        if proc.returncode:
            raise subprocess.CalledProcessError(proc.returncode, proc.args)
    wall = perf_counter() - begin
    rss_bytes = usage.ru_maxrss if sys.platform == "darwin" else usage.ru_maxrss * 1024
    return Run(wall, usage.ru_utime + usage.ru_stime, rss_bytes / (1024 ** 3))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--binary", default="target/release/eventforge")
    parser.add_argument("--baseline", help="also time this binary, using OUTPUT/baseline and OUTPUT/candidate")
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--warmup", type=int, default=1)
    args = parser.parse_args()
    if min(args.runs, args.threads) < 1 or args.warmup < 0:
        parser.error("runs and threads must be positive, warmup must be nonnegative")
    programs = [("candidate", args.binary, args.output)]
    if args.baseline:
        programs = [("baseline", args.baseline, args.output / "baseline"),
                    ("candidate", args.binary, args.output / "candidate")]
    results = {label: [] for label, _, _ in programs}
    for i in range(-args.warmup, args.runs):
        for label, binary, output in (programs if i % 2 == 0 else reversed(programs)):
            print(f"{label} {'warmup' if i < 0 else f'run {i + 1}'}", flush=True)
            result = measure(binary, args.data, output, args.threads)
            print(f"  wall {result.wall:.3f} s, CPU {result.cpu:.3f} s, "
                  f"average cores {result.cpu/result.wall:.2f}, RSS {result.rss:.3f} GiB", flush=True)
            if i >= 0:
                results[label].append(result)
    times = {}
    for label, samples in results.items():
        times[label] = median(r.wall for r in samples)
        print(f"{label} median {times[label]:.3f} s, median CPU {median(r.cpu for r in samples):.3f} s, "
              f"maximum measured RSS {max(r.rss for r in samples):.3f} GiB")
    if args.baseline:
        ratio = times["candidate"] / times["baseline"]
        print(f"wall-time reduction {100 * (1 - ratio):.1f}%, speedup {1/ratio:.3f}x")


if __name__ == "__main__":
    main()
