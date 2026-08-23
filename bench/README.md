# Benchmarking scx_cohort against EEVDF

This directory holds the tooling for the DESIGN.md §6 exit criteria: hard,
statistically defensible numbers for scx_cohort versus the default
scheduler (EEVDF), collected on the machine the scheduler targets — a
dual-CCD Ryzen with a sched_ext kernel. Everything here runs on real
hardware; none of it is meaningful in a VM or on a single-LLC part.

## What counts as evidence

Three tiers, strongest first. A convincing case needs the first two; the
third explains *why* the first two moved.

1. **Application-level outcomes.** Frame-time percentiles for games
   (MangoHud), Speedometer for Chrome, wall time for builds. These are the
   numbers users feel and the only ones that justify running a custom
   scheduler at all.
2. **Scheduler-neutral system metrics.** The microbenchmarks in this
   suite (`ipc_pingpong`, schbench, hackbench) plus perf counters
   (`sched:sched_migrate_task`, `perf c2c` remote-HITM counts). Measured
   identically under both schedulers by tools that know nothing about
   either.
   Read `cross_ccd_migrations_per_sec`, not `migrations_per_sec`. A raw
   migration count says nothing about this design: hopping between two
   cores of one CCD is cheap and crossing the fabric is the cost the
   scheduler exists to avoid, and the two schedulers do not even pay the
   same price per counted event. A run can migrate far *more* in total
   while crossing CCDs far less, which is a win the raw counter reports
   as a regression.
3. **scx_cohort's own telemetry.** The affinity hit rate from `--stats` /
   `scx_cohort top`. This is *diagnostic*, not proof — it shows the
   mechanism engaged (placements landing on the home CCD), but only tiers
   1–2 show that engaging it helped. The harness records it per run so
   you can correlate: an improvement with 97% affinity and a regression
   with 60% affinity tell very different stories.

Claim parity where the design claims parity: DESIGN.md's bar for batch
throughput (hackbench, kernel builds) is "within 3% of EEVDF", not a win.
Report those numbers with the same rigor as the wins — a benchmark
appendix that omits the ties looks like it's hiding losses.

## Quick start

```sh
cargo build --release
sudo bench/run-suite.sh                 # eevdf vs cohort, 5 rounds each
python3 bench/analyze.py bench/results/<timestamp>/results.csv
```

`run-suite.sh` alternates schedulers in ABBA order across rounds (so
thermal and background drift cancel instead of biasing whichever side runs
second), pins the cpufreq governor to `performance`, snapshots the system
configuration into `sysinfo.txt`, counts migrations around every run, and
records scx_cohort's achieved affinity per run.

Migration counting prefers `bpftrace`, which bins each
`sched:sched_migrate_task` by whether it stayed inside one last-level
cache and emits `same_ccd_migrations_per_sec` and
`cross_ccd_migrations_per_sec` alongside the total. The CPU-to-LLC map is
read from sysfs at startup, so nothing about the topology is hardcoded and
the generated program is dry-run before the suite starts. Without root or
bpftrace the harness falls back to `perf stat` for the total alone and
logs a SKIP; `sysinfo.txt` records which source produced the numbers. Raw logs for every run are
kept under `bench/results/<timestamp>/` so any number in the CSV can be
traced back to the output that produced it.

`analyze.py` prints per-scheduler median/mean/spread for every
(workload, metric) pair and tests each scheduler against the baseline with
a two-sided Mann-Whitney U test (exact at these sample sizes). Nothing
should be quoted as a win unless it's marked `<< improved`; five rounds is
the floor, ten makes tighter claims. Metric direction is inferred from the
name, and rates of bad events (`migrations_per_sec`, cache misses, faults)
are judged lower-is-better despite the `per_sec` suffix — for this
scheduler a migration is a cost being paid, not work being done.

Comparing against other scx schedulers is one flag:

```sh
sudo bench/run-suite.sh --sched lavd=/usr/bin/scx_lavd \
    --schedulers eevdf,cohort,lavd
```

## The built-in workloads

- **`pingpong` family** (the `ipc_pingpong` workspace crate, built by the
  same `cargo build --release` as the scheduler) — the thesis test.
  Pairs of processes exchange messages through shared memory with futex
  wakeups, and every message walks the buffer cache line by cache line, so
  line ownership ping-pongs between the processes exactly like Chrome's
  Mojo rings or Wine's fsync-guarded state. Same CCD: lines move through
  the shared L3. Split across CCDs: every line crosses the Infinity
  Fabric (~75–85 ns per hop on Zen 4/5 vs ~15–30 ns in-L3).

  | variant | duty cycle | background load | what it measures |
  |---|---|---|---|
  | `pingpong` | flat out | none | wake-affine placement on an idle machine |
  | `pingpong_loaded` | flat out | one exec'd spinner per CPU | placement under saturation |
  | `pingpong_real` | 50 µs think, 1 MiB scratch | one exec'd spinner per CPU | **the number to quote** |
  | `pingpong_spill` | flat out | one *forked* spinner per CPU | the oversized-cohort spill path |

  `pingpong_real` is the honest one, for two reasons the others get wrong.
  A pair that round-trips flat out never yields, and its 4 KiB payload
  never leaves L1, so the measurement is coherence traffic on hot lines
  rather than the L3-versus-fabric distinction this design targets. `-t`
  gives the pair work to do between messages and `-S` dirties a private
  scratch buffer larger than L2, which evicts the shared payload down to
  L3 where the CCD boundary starts to cost something. Think time is
  excluded from the measured RTT and included in throughput, so RTT stays
  comparable across variants while throughput drops to the slower cadence.

  `pingpong_spill` exists to keep an artifact visible rather than
  accidental. Its loaders are plain forks, so on a scheduler that groups
  by fork lineage they inherit the pairs' cohort, and the run oversubscribes
  a single cohort to roughly 5x one CCD. That measures spill behavior, not
  placement. Everywhere else the loaders `exec` themselves, which is a
  program-identity boundary (scx_cohort severs lineage there), so they
  compete from their own group the way a real background job does.

  Interesting deltas live in `rtt_p99_ns`/`rtt_p999_ns` under load; on an
  idle machine wake-affine heuristics often keep pairs together fine.

  Two confounds to separate before quoting a win. Under saturation a tail
  measured in milliseconds against a microsecond median is queueing delay,
  not fabric latency, and scx_cohort's interactive preemption attacks it
  directly — so rerun with `--preempt-min-us` set high enough to disable
  preemption and see how much of the win survives as placement. And check
  `cross_ccd_migrations_per_sec` rather than the raw total, which is
  dominated by cheap same-CCD hops.
- **schbench** — wakeup-latency percentiles under scheduler saturation;
  the standard "did tail latency regress" check.
- **`schbench_loaded`** — the same wakeup percentiles measured *while*
  hackbench saturates the machine. This is the interactive-under-load
  case the whole design exists for, and the only workload here that puts
  a number on it: everything else measures throughput or latency alone.
  Read `wakeup_p99_usec` and `wakeup_p999_usec`; a desktop that stalls
  under a kernel build shows up here and nowhere else. Worth running
  across a slice sweep rather than once, because `--slice-us` turned out
  to dominate this metric (see DESIGN.md §4.2).
- **hackbench** (or `perf bench sched messaging`) — messaging throughput.
  This is a *parity* check: cohort should be within a few percent, and a
  big win here would itself be suspicious.

Custom workloads plug in with `--custom name:command`. If the command
prints `RESULT <metric> <value>` lines they're recorded; otherwise wall
time is recorded as `elapsed_sec` — so `--custom "kbuild:make -C
~/src/linux -j32 vmlinux"` (with a `clean` in a prep wrapper) gives the
kernel-compile parity number from DESIGN.md with no extra code.

## Games: frame-time percentiles with MangoHud

The headline claim is about 1% lows, so measure exactly that:

1. Pick games with a **built-in benchmark scene** (deterministic camera
   path) — free play is not repeatable. Fix resolution/settings, disable
   VRR and frame caps, let shader caches warm up with one throwaway run.
2. Capture with MangoHud logging:
   `MANGOHUD=1 MANGOHUD_CONFIG=log_duration=120,autostart_log=1 %command%`
   (or toggle `Shift_L+F2`), one CSV per run.
3. Do **5+ runs per scheduler, alternating** schedulers between runs
   (`sudo systemctl start/stop scx_cohort` between captures), not 5 then 5.
4. Feed the logs through the same pipeline as everything else:

```sh
python3 bench/analyze.py mangohud eevdf-run*.csv \
    --workload cp2077 --scheduler eevdf --emit-csv game.csv
python3 bench/analyze.py mangohud cohort-run*.csv \
    --workload cp2077 --scheduler cohort --emit-csv game.csv
python3 bench/analyze.py compare game.csv
```

This yields `fps_avg`, `fps_1pct_low`, `fps_01pct_low`, and
`frametime_p99_ms` per run, with the same significance testing. The design
predicts average FPS roughly ties and the lows move — report both.

The strongest gaming scenario for scx_cohort is **game + background
load** (shader compilation, an OBS encode, a compile in another
workspace): run the same captures with a fixed background job and expect
EEVDF's lows to collapse while cohort's hold.

## Chrome

Speedometer 3 (browserbench.org) for the headline score: 5+ runs per
scheduler, fresh profile (`--user-data-dir` to a temp dir), no other
tabs, alternating schedulers. Speedometer prints a score with its own
confidence interval; record each run's score into the CSV format by hand
or via `--custom`. For the compositor-latency story, record a
`chrome://tracing` session while scrolling a heavy page under background
load and compare dropped-frame counts — harder to automate, and secondary
to the Speedometer number.

## Ground truth beneath the numbers

When a delta shows up (or refuses to), these attribute the cause:

- `sudo bpftrace -e 'tracepoint:sched:sched_migrate_task {
  @[(((args->orig_cpu % 16) >= 8) == ((args->dest_cpu % 16) >= 8)) ?
  "same-ccd" : "CROSS-CCD"] = count(); }'` — same-versus-cross-CCD
  migrations during real desktop use, where no repeatable workload exists.
  Verify the CCD split for your part first with
  `cat /sys/devices/system/cpu/cpu0/cache/index3/shared_cpu_list`; add a
  `/comm == "chrome"/` filter to scope it. The harness records the same
  breakdown per run.
- `perf stat -a -e sched:sched_migrate_task -- <workload>` — migration
  rate, total only (the harness falls back to this without bpftrace).
- `sudo perf c2c record -a -- sleep 10; sudo perf c2c report` during a
  workload — remote-HITM counts are the direct measurement of cache lines
  bouncing across the fabric; this is the counter the whole design exists
  to reduce.
- `sudo perf sched record -- sleep 10; sudo perf sched timehist -M` —
  per-event migration traces showing *which* threads crossed CCDs.
- `scx_cohort top` — per-cohort/per-process affinity live, to confirm the
  workload's cohort formed correctly and stayed home.

## Methodology checklist

Every published comparison should be able to answer yes to all of these:

- Same machine, kernel, and firmware for both sides; `sysinfo.txt` kept.
- cpufreq governor pinned (the harness does this); CPU boost state
  identical on both sides — either disabled, or accepted-and-recorded.
- ≥5 interleaved rounds per scheduler; ABBA ordering (harness default).
- Percentiles reported, not just means — the design targets tails, and a
  mean can hide a fixed tail (or a regressed one).
- Significance tested; ties reported as ties ("within noise") rather than
  as small wins.
- Idle desktop otherwise: no browser during microbenchmarks, compositor
  and background services identical across sides.
- For games: deterministic benchmark scene, warm shader cache, VRR and
  frame caps off, 120 s+ captures (a 0.1% low needs thousands of frames
  to be more than one hiccup).

Three honest caveats to keep in mind when reading results: the daemon
itself costs a little CPU (it shows up in hackbench-class benchmarks as
part of cohort's side, which is fair — users pay it too), the loaded
variants' spinners are also being scheduled by the scheduler under test,
so they measure the whole system's behavior rather than the pairs in
isolation (that's the point), and the bpftrace classifier adds a map
update per migration, which both schedulers pay equally but which is not
free on hackbench-class migration rates. `--no-ccd` drops back to
`perf stat` if you need the lighter instrument.

Results predating the exec'd loaders are not comparable to results after
them. `pingpong_loaded` used forked spinners, which on scx_cohort joined
the cohort under measurement; those runs are what `pingpong_spill` now
reproduces deliberately.
