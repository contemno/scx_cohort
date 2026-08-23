#!/usr/bin/env python3
# Copyright (c) scx_cohort authors.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.
"""Turn run-suite.sh CSVs into scheduler comparisons with significance tests.

    python3 bench/analyze.py compare bench/results/<ts>/results.csv
    python3 bench/analyze.py mangohud eevdf-run*.csv --workload game \\
        --scheduler eevdf --emit-csv results.csv

`compare` groups rows by (workload, metric), prints per-scheduler
median/mean/spread, and tests each scheduler against the baseline
(default: eevdf) with a two-sided Mann-Whitney U test — exact for the
small sample counts benchmarking produces, normal approximation beyond
that. No dependencies outside the standard library.

`mangohud` parses MangoHud frame-time logs into average FPS, 1%-low and
0.1%-low FPS, and p99 frame time; with --emit-csv it appends rows in
run-suite.sh's format so game captures flow through the same `compare`
pipeline as everything else.
"""

import argparse
import csv
import math
import statistics
import sys
from collections import defaultdict
from itertools import combinations

# Whether a bigger number is better, decided from the metric name. Counts
# of bad events are checked first: their rates would otherwise read as
# wins via the "per_sec"/"ops" suffixes ("migrations_per_sec" is a cost,
# not a throughput). Metrics not matched anywhere are printed without a
# verdict arrow.
# Diagnostic context rather than a target, checked ahead of everything
# else: a scheduler that converts fabric crossings into cheap in-CCD hops
# should push same-CCD migrations *up*, so neither direction is a verdict.
NEUTRAL = ("same_ccd_migrations",)
BAD_EVENT = ("migrations", "misses", "faults", "drops", "stalls")
HIGHER_BETTER = ("per_sec", "fps", "pct", "throughput", "ops", "score", "rps")
LOWER_BETTER = ("_ns", "_us", "_usec", "_ms", "_sec", "latency", "time",
                "frametime")


def direction(metric: str):
    m = metric.lower()
    if any(k in m for k in NEUTRAL):
        return None
    if any(k in m for k in BAD_EVENT):
        return "lower"
    if any(k in m for k in HIGHER_BETTER):
        return "higher"
    if any(k in m for k in LOWER_BETTER):
        return "lower"
    return None


def mann_whitney(a, b):
    """Two-sided Mann-Whitney U p-value: exact permutation when feasible
    (always, at benchmark sample counts), else normal approximation with
    tie correction."""
    n1, n2 = len(a), len(b)
    if n1 == 0 or n2 == 0:
        return None

    def u_stat(xs, pooled_sorted):
        # Rank-sum with midranks for ties.
        ranks = {}
        i = 0
        while i < len(pooled_sorted):
            j = i
            while j < len(pooled_sorted) and pooled_sorted[j] == pooled_sorted[i]:
                j += 1
            ranks[pooled_sorted[i]] = (i + j + 1) / 2  # 1-based midrank
            i = j
        r1 = sum(ranks[x] for x in xs)
        return r1 - len(xs) * (len(xs) + 1) / 2

    pooled = sorted(a + b)
    u1 = u_stat(a, pooled)

    if math.comb(n1 + n2, n1) <= 200_000:
        # Exact: enumerate every assignment of the pooled values to group
        # sizes and count U statistics at least as extreme as observed.
        mean_u = n1 * n2 / 2
        obs = abs(u1 - mean_u)
        extreme = total = 0
        for idx in combinations(range(n1 + n2), n1):
            xs = [pooled[i] for i in idx]
            if abs(u_stat(xs, pooled) - mean_u) >= obs - 1e-9:
                extreme += 1
            total += 1
        return extreme / total

    # Normal approximation with tie correction.
    n = n1 + n2
    tie_term = 0
    i = 0
    while i < n:
        j = i
        while j < n and pooled[j] == pooled[i]:
            j += 1
        t = j - i
        tie_term += t**3 - t
        i = j
    sigma2 = n1 * n2 / 12 * ((n + 1) - tie_term / (n * (n - 1)))
    if sigma2 <= 0:
        return 1.0
    z = (u1 - n1 * n2 / 2) / math.sqrt(sigma2)
    return math.erfc(abs(z) / math.sqrt(2))


def fmt(v):
    if abs(v) >= 1000:
        return f"{v:,.0f}"
    return f"{v:.4g}"


def cmd_compare(args):
    groups = defaultdict(lambda: defaultdict(list))
    with open(args.csv) as f:
        for row in csv.DictReader(f):
            try:
                v = float(row["value"])
            except (ValueError, KeyError):
                continue
            groups[(row["workload"], row["metric"])][row["scheduler"]].append(v)

    if not groups:
        sys.exit(f"no data rows in {args.csv}")

    any_baseline = any(args.baseline in scheds for scheds in groups.values())
    if not any_baseline:
        print(f"note: baseline '{args.baseline}' absent; printing stats only\n")

    for (workload, metric), scheds in sorted(groups.items()):
        d = direction(metric)
        note = {"higher": "higher is better", "lower": "lower is better"}.get(d, "")
        print(f"== {workload} / {metric}" + (f"  ({note})" if note else ""))
        base = scheds.get(args.baseline)
        for name, vals in sorted(scheds.items()):
            med = statistics.median(vals)
            mean = statistics.mean(vals)
            sd = statistics.stdev(vals) if len(vals) > 1 else 0.0
            line = (f"  {name:<10} n={len(vals):<3} median={fmt(med):>10} "
                    f"mean={fmt(mean):>10} ±{fmt(sd):<8} "
                    f"[{fmt(min(vals))} .. {fmt(max(vals))}]")
            if base and name != args.baseline:
                bmed = statistics.median(base)
                delta = (med - bmed) / bmed * 100 if bmed else float("nan")
                p = mann_whitney(base, vals)
                verdict = ""
                if d and p is not None:
                    improved = delta > 0 if d == "higher" else delta < 0
                    if p < args.alpha:
                        verdict = ("  << improved" if improved
                                   else "  << REGRESSED")
                    else:
                        verdict = "  (not significant)"
                line += f"  Δ={delta:+.1f}%"
                if p is not None:
                    line += f" p={p:.3f}{verdict}"
            print(line)
        print()

    print(f"significance: two-sided Mann-Whitney U at α={args.alpha}. "
          "Deltas are of medians vs the baseline. Runs with n<5 per side "
          "rarely reach significance; use --rounds 10 for tighter claims.")


def parse_mangohud(path):
    """MangoHud CSV logs: a couple of metadata lines, then a header row
    containing 'frametime', then data rows. Frame time is in ms in current
    MangoHud; older builds logged µs (detected by magnitude)."""
    with open(path) as f:
        rows = list(csv.reader(f))
    header_idx = col = None
    for i, row in enumerate(rows):
        for j, cell in enumerate(row):
            if cell.strip().lower() == "frametime":
                header_idx, col = i, j
                break
        if header_idx is not None:
            break
    if header_idx is None:
        sys.exit(f"{path}: no 'frametime' column found")
    ft = []
    for row in rows[header_idx + 1:]:
        try:
            ft.append(float(row[col]))
        except (ValueError, IndexError):
            continue
    if len(ft) < 100:
        sys.exit(f"{path}: only {len(ft)} frames; capture a longer run")
    if statistics.median(ft) > 1000:  # µs, not ms
        ft = [x / 1000 for x in ft]
    return ft


def low_fps(ft_sorted, frac):
    """X%-low FPS: 1000 / mean of the worst X% frame times (the convention
    MangoHud and reviewers use)."""
    n = max(1, int(len(ft_sorted) * frac))
    worst = ft_sorted[-n:]
    return 1000 / (sum(worst) / len(worst))


def cmd_mangohud(args):
    out_rows = []
    for i, path in enumerate(args.files, 1):
        ft = parse_mangohud(path)
        s = sorted(ft)
        avg_fps = 1000 / statistics.mean(ft)
        stats = {
            "fps_avg": round(avg_fps, 2),
            "fps_1pct_low": round(low_fps(s, 0.01), 2),
            "fps_01pct_low": round(low_fps(s, 0.001), 2),
            "frametime_p99_ms": round(s[int(0.99 * (len(s) - 1))], 3),
        }
        print(f"{path}: frames={len(ft)} " +
              " ".join(f"{k}={v}" for k, v in stats.items()))
        for metric, value in stats.items():
            out_rows.append((args.workload, args.scheduler, i, metric, value))

    if args.emit_csv:
        import os
        new = not os.path.exists(args.emit_csv)
        with open(args.emit_csv, "a", newline="") as f:
            w = csv.writer(f)
            if new:
                w.writerow(["workload", "scheduler", "round", "metric", "value"])
            w.writerows(out_rows)
        print(f"appended {len(out_rows)} rows to {args.emit_csv}")


def main():
    # Die quietly when piped into head/less.
    try:
        import signal
        signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    except (ImportError, AttributeError, ValueError):
        pass
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd")

    c = sub.add_parser("compare", help="compare schedulers in a results.csv")
    c.add_argument("csv")
    c.add_argument("--baseline", default="eevdf")
    c.add_argument("--alpha", type=float, default=0.05)

    m = sub.add_parser("mangohud", help="summarize MangoHud frame-time logs")
    m.add_argument("files", nargs="+")
    m.add_argument("--workload", default="game")
    m.add_argument("--scheduler", required=False, default="unknown")
    m.add_argument("--emit-csv", help="append rows in results.csv format")

    # Allow `analyze.py results.csv` as shorthand for compare.
    argv = sys.argv[1:]
    if argv and argv[0] not in ("compare", "mangohud", "-h", "--help"):
        argv = ["compare"] + argv
    args = ap.parse_args(argv)
    if args.cmd is None:
        ap.print_help()
        sys.exit(2)
    {"compare": cmd_compare, "mangohud": cmd_mangohud}[args.cmd](args)


if __name__ == "__main__":
    main()
