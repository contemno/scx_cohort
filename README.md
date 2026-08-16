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
  cohort formation from tgid/fork lineage, sampled wake-edge recording.
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

Watch it work:

```sh
./target/release/scx_cohort --monitor 1     # or scxtop
```

The headline number is `affinity` — the percentage of placements that
landed on the task's home CCD. Regressions show up there before they show
up in frame times.

## Tuning

The four numbers that matter (DESIGN.md §6 Phase 6):

| Flag | Default | Meaning |
|---|---|---|
| `--steal-min` | 2 | Steal across the fabric only when the foreign queue is deeper than this… |
| `--steal-delay-us` | 500 | …or its head task has waited this long. Higher = stickier CCDs. |
| `--imbalance-pct` | 20 | Inter-CCD load gap (as % of one CCD) that triggers a cohort move. |
| `--residency-ms` | 2000 | Post-move immunity: a cohort that moved stays put this long. |
| `--merge-wakes-per-sec` | 300 | Sustained cross-cohort wake rate that merges two cohorts. |

## Rules file (optional)

The automatic mechanisms need no configuration; a TOML file layered on
top handles overrides and experiments (`--config path.toml`):

```toml
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

Rules are evaluated in order; the first match wins; criteria within a
rule AND together.

## Development

```sh
cargo test              # policy unit tests + shared-layout assertions
cargo clippy --all-targets
```

The BPF program compiles against the header bundle shipped by the
`scx_cargo` crate (vmlinux.h, `compat.bpf.h` shims for the 6.13 kfunc
renames), so no kernel headers or bpftool are needed to build — but
loading it requires the sched_ext kernel above. Behavioral exit criteria
per design phase (CCD residency under stress, affinity > 95%, watchdog
survival under overload, frame-time percentiles vs EEVDF) are in
DESIGN.md §6 and need real hardware.

## License

GPL-2.0-only.
