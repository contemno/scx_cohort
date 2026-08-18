# scx_cohort

A CCD-affine [sched_ext](https://docs.kernel.org/scheduler/sched-ext.html)
scheduler for dual-CCD AMD Ryzen CPUs (7950X / 9950X class).

scx_cohort discovers groups of related tasks ("cohorts") — via thread
grouping, fork lineage, and observed wakeup patterns — gives each cohort a
home CCD, schedules within that CCD by default, and crosses the Infinity
Fabric only when the math clearly favors it. The motivating workloads are
Chrome (browser/GPU/renderer processes chatting over Mojo IPC) and games
under Wine/Proton (game ↔ wineserver round trips, fsync wake chains),
where the default scheduler's machine-wide load balancing turns shared
cache lines into cross-CCD fabric traffic. See [DESIGN.md](DESIGN.md) for
the full design.

## Architecture

Hybrid BPF/userspace split, with **Rust as the single source of truth**:

- **`scx_cohort_common`** — a `no_std` Rust crate defining every struct
  and constant shared between the two sides, in `#[repr(C)]` with
  [zerocopy](https://crates.io/crates/zerocopy) derives (implicit padding
  is a compile error, and layout tests pin the sizes).
- **`scx_cohort/src/bpf/main.bpf.c`** — the kernel fast path: per-CCD
  vtime DSQs, the in-CCD CPU-selection ladder, gated cross-CCD stealing,
  cohort formation from tgid/fork lineage (severed at exec), sampled
  wake-edge recording.
  Every per-wakeup decision is made from map lookups only. The C header
  it includes (`intf.h`) is **generated from the common crate by
  cbindgen** at build time — nothing is defined twice, and the daemon
  reads map bytes through the very same Rust types with no conversion
  layer.
- **`scx_cohort/src/*.rs`** — the daemon: topology discovery, a ~200 ms
  tick running the load balancer (whole-cohort moves with two-tick
  confirmation and residency hysteresis), wake-graph clustering
  (merge/split), sticky spill for oversized cohorts, TOML rules, and an
  scx_stats metrics server. All policy is pure Rust over snapshot types
  (`balancer.rs`, `wake_graph.rs`, `config.rs`) and unit-tested without a
  kernel; `main.rs` only shuttles bytes.

The sched_ext callbacks themselves are C because the Rust eBPF toolchain
cannot produce struct_ops programs yet (no BTF relocations from rustc, no
scx kfunc calls). The C surface is deliberately thin so a future port to
Rust BPF is a contained change.

## Requirements

- Linux 6.12+ with `CONFIG_SCHED_CLASS_EXT=y` (check:
  `ls /sys/kernel/sched_ext/`)
- clang ≥ 16 (17+ recommended), rustc ≥ 1.82
- `libelf-dev` and `zlib1g-dev` (libbpf is vendored and built from source)

## Build & run

```sh
cargo build --release
sudo ./target/release/scx_cohort
```

The scheduler detaches (falling back to EEVDF) on Ctrl-C, on any daemon
exit, via the sched_ext watchdog if a runnable task ever starves past the
timeout, or manually with SysRq-S.

Root is required (loading sched_ext programs needs CAP_BPF +
CAP_SYS_ADMIN); an unprivileged run fails with EPERM. If the load fails
with EINVAL, rerun with `-vv` to print the libbpf/verifier log.

Watch it work:

```sh
scx_cohort top                              # per-cohort/per-process view
./target/release/scx_cohort --monitor 1     # aggregate metrics (or scxtop)
```

`scx_cohort top` shows each cohort (home CCD, `*` = rule-pinned,
utilization, affinity) with its member processes beneath (threads, `sN` =
N threads currently spilled to the other CCD). The headline number is
**affinity** — the percentage of runtime executed on the home CCD.
Regressions show up there before they show up in frame times.

The daemon raises itself to `SCHED_FIFO 10` so it can never be starved by
the scheduling class it implements (`--no-rt` or `rt_priority = 0` in the
config opts out). On suspend/resume or CPU hotplug the kernel ejects the
scheduler with a restart code; the daemon re-initializes automatically
(logged as "scheduler ejected for restart").

## Command-line options

| Flag | Default | Meaning |
|---|---|---|
| `--slice-us` | 5000 | Scheduling slice duration in microseconds. |
| `--interval-ms` | 200 | Daemon tick (balancer/discovery) interval. |
| `--steal-min` | 2 | Steal across the fabric only when the foreign queue is deeper than this… |
| `--steal-delay-us` | 500 | …and its head task has waited this long. Higher = stickier CCDs. |
| `--preempt-min-us` | 20 | An interactive wakee preempts its prev CPU (if running batch work) at most this often per CPU. |
| `--imbalance-pct` | 20 | Inter-CCD load gap (as % of one CCD) that triggers a cohort move. |
| `--residency-ms` | 2000 | Post-move immunity: a cohort that moved stays put this long. |
| `--merge-wakes-per-sec` | 300 | Sustained cross-cohort wake rate that merges two cohorts. |
| `--config <path>` | `/etc/scx_cohort.conf` | Config file. The default path may be absent; an explicit one must exist. |
| `--no-rt` | off | Don't raise the daemon to SCHED_FIFO. |
| `--stats <sec>` | — | Print aggregate metrics every interval while scheduling. |
| `--monitor <sec>` | — | Metrics-only mode against a running scheduler. |
| `-v` / `-vv` | — | Debug / trace logging (`-vv` includes the libbpf/verifier log). |
| `top [--interval <sec>]` | 2 | Live per-cohort/per-process view against a running scheduler. |

`scx_cohort --help` is authoritative; the table is a convenience copy.

## Configuration file

Read from `/etc/scx_cohort.conf` by default (TOML). Both tables are
optional; unknown keys are rejected so typos fail loudly. Precedence:
built-in defaults < `[options]` < explicitly passed CLI flags.

```toml
[options]
slice_us = 5000
interval_ms = 200
steal_min = 2
steal_delay_us = 500
preempt_min_us = 20
imbalance_pct = 20
residency_ms = 2000
merge_wakes_per_sec = 300.0
rt_priority = 10              # SCHED_FIFO priority; 0 disables

[[rule]]
match_comm = ["wineserver", "wine64-preloader"]
join_cohort_of = "parent"     # default anyway; shown for illustration

[[rule]]
match_cgroup = "user.slice/*/app-steam*"
min_ccd_residency_ms = 5000   # extra migration hysteresis for games

[[rule]]
match_comm = ["my-benchmark"]
pin_ccd = 0                   # nail this cohort to CCD 0
```

`[[rule]]` keys: `match_comm` (list of exact comm names), `match_cgroup`
(glob against the cgroup v2 path), `pin_ccd` (pin the matching task's
cohort to a CCD), `min_ccd_residency_ms` (per-cohort balancer hysteresis
override), `join_cohort_of = "parent"` (documents the default lineage
behavior). Rules are evaluated in order; the first match wins; criteria
within a rule AND together.

## Running as a service

```sh
sudo install -m755 target/release/scx_cohort /usr/local/bin/
sudo install -m644 systemd/scx_cohort.service /etc/systemd/system/
sudo install -m644 systemd/journald@scx-cohort.conf /etc/systemd/
sudo systemctl daemon-reload
sudo systemctl enable --now scx_cohort
```

Logs go to a dedicated journald namespace (requires systemd ≥ 245),
RAM-backed and capped at one hour / 64M so scheduler chatter never
accumulates in the main journal:

```sh
journalctl --namespace scx-cohort -u scx_cohort -f
```

The unit restarts the daemon on failure and also sets the RT scheduling
class as defense in depth.

## Hardening

The daemon's privilege needs are front-loaded: loading and attaching the
scheduler requires `CAP_BPF` + `CAP_SYS_ADMIN` (+ `CAP_PERFMON` on some
kernels) and the RT boost needs `CAP_SYS_NICE`, but steady-state
operation — map access through already-open fds, world-readable procfs
and sysfs reads, a unix stats socket — needs no capabilities at all.

The shipped unit sandboxes accordingly: capability bounding set trimmed
to those four, read-only filesystem (`ProtectSystem=strict`, the stats
socket dir in `/run/scx` is the only writable path), no network
(`PrivateNetwork=yes`; the AF_UNIX stats socket still works), no module
loading, kernel tunables/logs/clock protected, namespaces and personality
locked, W^X memory, and a seccomp allow-list (`@system-service` plus
`bpf` and `sched_setscheduler`, which no `@` group covers). Deliberately
NOT set, because this daemon needs them: `RestrictRealtime` (would break
the SCHED_FIFO boost) and `ProtectProc=invisible`/`ProcSubset=pid` (rule
matching reads other processes' `/proc/<pid>/cgroup`). Measure with
`systemd-analyze security scx_cohort`.

For a full post-attach drop, set `drop_privs = true` in `[options]` (or
pass `--drop-privs`): once the scheduler is attached the daemon
irrevocably drops **all** capabilities (ambient, bounding, and the
per-thread sets — the process is kept single-threaded until the drop so
no thread retains privileges) plus `no_new_privs`. The tradeoff: it can
no longer re-load BPF after a suspend/resume ejection, so it exits
instead and relies on the unit's `Restart=on-failure` to relaunch it
with fresh privileges. Enable it when running under systemd; standalone
runs would need an external restart mechanism to survive suspend.

## Benchmarking

`bench/` holds an A/B harness for proving (or disproving) the design
against the default scheduler on real hardware:

```sh
cargo build --release
sudo bench/run-suite.sh          # interleaved eevdf-vs-cohort rounds
python3 bench/analyze.py bench/results/<timestamp>/results.csv
```

It runs a cache-line-bouncing IPC microbenchmark modeled on the Chrome/
Wine failure mode plus schbench/hackbench, records migration counts and
the scheduler's achieved affinity per run, and the analyzer reports
median deltas with Mann-Whitney significance tests. Game (MangoHud) and
Chrome (Speedometer) measurement flows, and the full methodology
checklist, are in [bench/README.md](bench/README.md).

## Development

```sh
make test               # policy unit tests + shared-layout assertions
make lint               # rustfmt + clippy -D warnings + script syntax
./scripts/install-hooks.sh   # once: fast pre-push lint hook
```

Contribution flow (branches, PRs, releases, project invariants) is in
[CONTRIBUTING.md](CONTRIBUTING.md); vulnerabilities go through
[SECURITY.md](SECURITY.md), never public issues.

The BPF program compiles against the header bundle shipped by the
`scx_cargo` crate (vmlinux.h, `compat.bpf.h` shims for the 6.13 kfunc
renames), so no kernel headers or bpftool are needed to build — but
loading it requires the sched_ext kernel above. Behavioral exit criteria
per design phase (CCD residency under stress, affinity > 95%, watchdog
survival under overload, frame-time percentiles vs EEVDF) are in
DESIGN.md §6 and need real hardware.

## License

GPL-2.0-only.
