# scx_cohort: A CCD-Affine sched_ext Scheduler

**Design document and implementation roadmap**
Target: dual-CCD AMD Ryzen (7950X / 9950X class), Linux 6.12+ with `CONFIG_SCHED_CLASS_EXT`
Approach: from-scratch Rust scheduler on the scx framework (hybrid BPF fast path + Rust userspace policy daemon)

---

## 1. Problem statement

A 7950X or 9950X exposes 16 cores to the kernel as one flat SMP machine, but the silicon is two 8-core CCDs, each with its own 32 MB L3, joined by Infinity Fabric. Two threads on the same CCD share L3 and exchange cache lines in tens of nanoseconds. Two threads on different CCDs miss each other's L3 entirely and every cache line transfer crosses the fabric.

Approximate core-to-core latencies on Zen 4/5 parts:

| Path | Latency |
|---|---|
| SMT siblings | ~7–10 ns |
| Same CCD, different core | ~15–30 ns |
| Cross-CCD (Zen 4, and Zen 5 after the AGESA 1.2.0.2 microcode fix) | ~75–85 ns |
| Cross-CCD (Zen 5 at launch, before the fix) | ~180–200 ns |

The kernel's default scheduler (EEVDF) knows about the L3 boundary through scheduler domains and has wake-affine heuristics, but it optimizes for fairness and utilization across the whole machine. It has no concept of a *group* of tasks that share data. Under load it spreads threads across both CCDs, and its load balancer migrates tasks across the L3 boundary whenever utilization looks uneven. For workloads with heavy cross-thread communication, that produces the failure mode this project targets: producer on CCD0, consumer on CCD1, every shared cache line bouncing across the fabric.

Two workloads motivate the design:

**Chrome.** One browser process, one GPU process, and a renderer process per site, all chatting over Mojo IPC (shared memory rings plus futex and socket wakeups). The compositor path (renderer → GPU process → browser) is latency sensitive and crosses process boundaries, so per-process affinity alone doesn't capture it. The kernel sees five unrelated processes; the hardware would prefer them on one L3.

**Games under Wine/Proton.** The game itself is one process with many threads (render thread, RHI submission thread, worker pools, audio), which EEVDF already tends to keep semi-local. The trap is the Wine plumbing: `wineserver` is a separate process the game round-trips to for Windows object operations, and esync/fsync/ntsync synchronization generates dense futex/eventfd wake chains between game threads. A migration of the render thread to the other CCD mid-frame shows up as a frame-time spike. The 1% lows suffer even when average FPS looks fine.

The goal: an eBPF scheduler that discovers groups of related tasks ("cohorts"), gives each cohort a home CCD, schedules within that CCD by default, and crosses the fabric only when the math clearly favors it.

## 2. Goals and non-goals

The scheduler should keep a cohort's tasks on one CCD, make cross-CCD migration a deliberate, rate-limited decision instead of a load-balancer reflex, discover cohorts without per-application configuration, keep both CCDs busy when total demand exceeds one CCD, and stay safe: any bug degrades to EEVDF via the sched_ext watchdog rather than hanging the machine.

Non-goals for v1: X3D asymmetric CCDs (cache CCD vs. frequency CCD placement is a natural v2 extension, and the design keeps the door open), multi-socket/NUMA, energy-aware scheduling, and beating EEVDF on batch throughput (parity is the bar; kernel compiles don't care about CCD locality, latency-sensitive interactive work does).

## 3. Background

### 3.1 sched_ext mechanics

sched_ext (mainline since 6.12) lets a BPF program implement the scheduling policy via `struct sched_ext_ops`. The callbacks this design uses:

```c
s32  select_cpu(p, prev_cpu, wake_flags)  // wakeup path: pick a CPU hint, may direct-dispatch
void enqueue(p, enq_flags)                 // place task into a DSQ
void dispatch(cpu, prev)                   // CPU is idle: pull work into its local DSQ
void running(p) / stopping(p, runnable)    // task on/off CPU: accounting
s32  init_task(p, args)                    // new task: args->fork tells us if this is a fork
void exit_task(p, args)                    // cleanup
s32  init() / void exit(ei)                // scheduler lifecycle
```

Tasks flow through dispatch queues (DSQs). Each CPU has a local FIFO (`SCX_DSQ_LOCAL`) it executes from; custom DSQs created with `scx_bpf_create_dsq()` support vtime-ordered insertion via `scx_bpf_dsq_insert_vtime()`. The kernel provides `scx_bpf_select_cpu_dfl()` for default idle-CPU selection and `scx_bpf_kick_cpu()` to wake an idle CPU. On wakeups, `select_cpu` receives wake flags including `SCX_WAKE_SYNC` (waker expects to sleep soon, the classic pipe/futex handoff signal). Note the 6.13 kfunc renames (`scx_bpf_dispatch()` → `scx_bpf_dsq_insert()`); the scx repo's `compat.bpf.h` papers over this, so build against it.

Safety comes for free: a runnable task that gets no CPU within the watchdog timeout causes the kernel to eject the BPF scheduler and fall back to EEVDF. SysRq-S does the same manually.

### 3.2 The scx Rust framework

The [scx repository](https://github.com/sched-ext/scx) provides the Rust scaffolding this project builds on. `scx_utils` handles BPF skeleton building/loading, and its `Topology` API parses sysfs into the hierarchy this scheduler needs: `Node → Llc → Core → Cpu`. On a 7950X, `topo.nodes[0].llcs` yields exactly the two CCDs, each with 8 `Core`s and 16 `Cpu`s. No custom topology parsing required. `scx_stats` serves metrics over a socket (what `scxtop` reads).

### 3.3 Prior art, and why a new scheduler

`scx_rusty` partitions the machine into per-LLC domains with a userspace load balancer, which proves the architecture, but it balances by load alone and will happily split communicating processes across domains. `scx_layered` has LLC awareness and could pin a game to one CCD via JSON config, but every application needs hand-written rules. `scx_lavd` targets gaming latency with virtual deadlines and has per-LLC awareness, but no cross-process grouping. `scx_p2dq` does pick-two balancing between LLCs, again load-driven.

The gap scx_cohort fills: *automatic* relationship discovery (fork lineage plus observed wake patterns) driving *whole-group* placement. Steal `scx_rusty`'s BPF/userspace split, `scx_lavd`'s latency instincts, and add the cohort layer neither has.

## 4. Architecture

Hybrid split, following the scx_rusty pattern:

```
┌────────────────────────── userspace (Rust) ──────────────────────────┐
│ scx_cohort daemon                                                    │
│  • Topology discovery (scx_utils::Topology)                          │
│  • Cohort graph: merge/split decisions from wake-edge data           │
│  • Load balancer: cohort → CCD assignment, hysteresis, tick ~200 ms  │
│  • Config (TOML rules), scx_stats metrics server                     │
└───────────────┬───────────────────────────────▲──────────────────────┘
        writes cohort→CCD map              reads wake edges, loads
┌───────────────▼───────────────────────────────┴──────────────────────┐
│ BPF (kernel fast path)                                               │
│  • task_ctx { cohort_id, vtime, … } per task (BPF task storage)      │
│  • cohort map: cohort_id → { home_ccd, weight, load }                │
│  • wake-edge map: (waker_cohort, wakee_cohort) → count (LRU hash)    │
│  • 2 vtime DSQs (one per CCD) + select_cpu/enqueue/dispatch logic    │
└──────────────────────────────────────────────────────────────────────┘
```

The BPF side makes every per-wakeup decision using only map lookups; nothing waits on userspace. The daemon does the slow thinking: graph clustering, rebalancing, rule matching. If the daemon stalls, the BPF side keeps scheduling with the last-written assignments.

### 4.1 Cohort formation

A cohort is the placement unit. Tasks acquire cohort membership through three mechanisms, cheapest first:

**Thread grouping (BPF, free).** Every thread of a process (same `tgid`) starts in the same cohort. This alone covers the single-process bulk of a Wine game.

**Fork lineage (BPF, free).** `init_task` fires with `args->fork` set; the child inherits the parent's cohort. Chrome's browser process forks its zygote which forks renderers, so the whole tree lands in one cohort with zero configuration. `wineserver` and the game share a `wine` ancestor, same effect. Lineage inheritance survives `exec`, which is what you want (the interesting relationships flow through the fork tree), and userspace can split cohorts that turn out to be unrelated (see below).

**Wake-edge observation (BPF records, userspace decides).** Lineage misses relationships between separately launched programs (a game launched by Steam and a Discord overlay, or `pipewire` serving a game's audio). In `select_cpu`/`enqueue`, when a wakeup arrives (especially with `SCX_WAKE_SYNC`), the BPF side bumps a counter keyed `(waker_cohort, wakee_cohort)` in an LRU hash. The daemon reads and decays these counters each tick. When the edge rate between two cohorts crosses a threshold (say, sustained hundreds of wakes/sec), it merges them. The same data drives splits: a cohort whose internal wake graph shows two disconnected components with big combined footprint gets split. Edge recording is a single map update on a percpu-friendly key; sample it (e.g., 1-in-8 wakeups) if profiling shows overhead.

**Explicit rules (userspace, optional).** A TOML config can pin matches by comm/cgroup path to a cohort or a CCD, for overrides and experiments:

```toml
[[rule]]
match_comm = ["wineserver", "wine64-preloader"]
join_cohort_of = "parent"     # default anyway; shown for illustration

[[rule]]
match_cgroup = "user.slice/*/app-steam*"
min_ccd_residency_ms = 5000   # extra migration hysteresis for games
```

Rules are a tuning aid. The design stands on the automatic mechanisms.

### 4.2 Placement policy (BPF fast path)

Two custom DSQs, one per CCD, vtime-ordered for fairness within the CCD. Per-CPU local DSQs are used for direct dispatch on the wakeup path.

**`select_cpu`** implements a strict preference ladder, all within the cohort's home CCD: `prev_cpu` if idle and in the home CCD (cache-warm, cheapest), else a fully idle core in the home CCD (both SMT siblings free), else any idle SMT sibling in the home CCD, and on a `SCX_WAKE_SYNC` wakeup, the waker's CPU if the waker is in the same cohort and about to sleep. On a hit, direct-dispatch to that CPU's local DSQ and kick it. On a miss (home CCD fully busy), return `prev_cpu` clamped into the home CCD and let `enqueue` queue the task; do *not* fall through to a machine-wide idle search. That fall-through is exactly the fabric-crossing reflex this scheduler exists to remove.

**`enqueue`** inserts into the home CCD's DSQ with vtime, charged from the task's accumulated runtime scaled by weight. Tasks with the interactive signature (high wake frequency, short average runtime, the profile of render threads, compositors, and audio threads) get a bounded vtime credit so they jump the queue within their CCD. This borrows scx_bpfland/scx_lavd's insight without importing their machinery; the credit is clamped so batch tasks can't be starved past the watchdog's patience.

**`dispatch`** on an idle CPU consumes from its own CCD's DSQ first. If empty, it may steal from the other CCD's DSQ, gated by a threshold: steal only when the foreign DSQ has more than `steal_min` waiting tasks or its oldest task has waited longer than `steal_delay_us` (a few hundred µs). Ungated stealing would re-create EEVDF's spreading behavior; no stealing at all would idle half the machine under single-cohort overload and eventually trip the watchdog. The gate makes crossing the fabric a priced decision. A stolen task's cohort assignment does not change; it drifts home on its next wakeup unless the daemon reassigns the cohort.

### 4.3 Load balancing (userspace, ~200 ms tick)

The daemon computes per-CCD load as the sum of member tasks' duty cycles (tracked in BPF via `running`/`stopping` timestamps, read per tick). If the imbalance between CCDs exceeds a threshold (default 20% of one CCD's capacity) for two consecutive ticks, it moves the smallest cohort whose relocation best repairs the imbalance, and writes the new `home_ccd` into the cohort map. Tasks drift to the new home lazily via their next `select_cpu`; nothing is force-migrated, so the transition costs no IPIs.

Three rules keep this stable. Whole cohorts move; the balancer never splits one to fix a number. Hysteresis is mandatory: a cohort that just moved is immune for `min_ccd_residency_ms` (default 2 s). And oversized cohorts get spill handling instead of relocation: a cohort whose runnable demand exceeds one CCD (Chrome with 30 renderers, a 24-thread game) sheds members to the other CCD until what remains fits — coldest members first (lowest duty cycle, e.g. idle background renderers, near-zero cost to run remotely), and if that still leaves the home overloaded, hot members too, hottest-first: a CPU-bound thread rarely sleeps on cohort mates and pays the least for remoteness, while often-waking latency-sensitive threads keep the home L3. Growth is planned against the demand still at home, so the set converges instead of creeping. Spill choice is sticky per task, so the *same* threads stay remote instead of the whole cohort churning across the boundary. (An earlier revision never spilled hot members and let the steal gate carry the remainder; on uniformly-hot oversized cohorts that produced a per-dispatch cross-CCD steal storm — the steal gate is a transient smoother, not a load-shedding mechanism.)

### 4.4 Daemon scheduling class: SCHED_FIFO, not SCHED_DEADLINE

The daemon raises itself to `SCHED_FIFO 10` (and the systemd unit sets the same policy as a backstop). The point of the boost is narrow: RT sits above sched_ext in the class hierarchy, so an RT daemon can never be starved by the scheduling class it implements. It is not a latency guarantee — the BPF side makes every per-wakeup decision, and the daemon is a 5 Hz control loop that sleeps almost all the time.

`SCHED_DEADLINE` also outranks sched_ext, and looks tempting on paper: the tick is genuinely periodic, a runtime/period budget would hard-cap a buggy spinning daemon, and there'd be no magic priority number to pick. It loses on deployment fragility and on being *more* privilege than the job needs:

- **Admission control makes the boost unreliable.** `sched_setattr(SCHED_DEADLINE)` is rejected when the task's affinity doesn't span the root domain — any `CPUAffinity=`, taskset, or cpuset restriction silently defeats it — and when other DL tasks hold the bandwidth. FIFO has no admission control; the warn-and-continue path stays a rarity instead of firing on exactly the complicated systems where the protection matters most.
- **systemd can't set it.** `CPUSchedulingPolicy=` supports fifo/rr but not deadline, so the unit-level backstop disappears. DEADLINE is also strictly per-thread and not inherited, so the stats-server threads would each need their own `sched_setattr`, versus inheriting FIFO from the unit at exec.
- **DEADLINE outranks the entire RT class.** On a box with PREEMPT_RT irq threads or audio threads at FIFO 70–99, a DL daemon preempts all of them every tick. FIFO 10 is the minimal sufficient rung: above every sched_ext task, below latency-critical RT work.
- **The budget can't be sized honestly.** Tick cost scales with task and cohort count, and the stats server wakes the loop asynchronously, so demand isn't purely periodic. Undersize the budget and a tick gets throttled mid-scan on a busy machine — exactly when the balancer matters; oversize it and the containment benefit that motivated DL is gone. Meanwhile FIFO's runaway-spin risk is already bounded by the kernel's RT throttling, and `--no-rt` exists.

The trade would flip if the daemon ever became the hot path (a userspace-dispatch design where scheduling decisions wait on it, à la scx_rustland); bounded tardiness would then be worth the admission-control fragility. For a control-plane daemon with a fallback-safe BPF data plane, it isn't.

### 4.5 Chrome and Wine walkthroughs

**Chrome:** browser forks zygote forks renderers/GPU process; lineage puts everything in one cohort with home on, say, CCD0. Mojo wakeups stay intra-L3. When tab count pushes demand past 8 cores, background renderers (near-zero duty cycle, no recent wake edges to the GPU process) spill to CCD1, while the focused renderer, GPU, and browser processes hold CCD0. The compositor chain never crosses the fabric.

**Wine game:** `steam` → `wine` → game and `wineserver` share lineage, one cohort. Render thread wakes RHI thread via fsync futex with `WAKE_SYNC`; select_cpu places wakee near waker inside the CCD. `wineserver` round-trips stay intra-L3. PipeWire runs outside the lineage tree, but its dense wake edges to the game's audio thread merge it into the cohort within a few daemon ticks. The balancer's residency rule means a momentary background load spike (shader compilation in another Steam process) doesn't evict the game from its CCD.

## 5. Observability

Exported via `scx_stats` (readable with `scxtop` or the daemon's `--monitor` flag): per-CCD load and DSQ depth, cohort count and sizes, merges/splits per minute, cross-CCD steals/sec, spilled-task count, wakeups landing on home CCD (the headline "affinity hit rate"), and cohort migrations with cause. The affinity hit rate is the number to watch during tuning: EEVDF on a chatty workload sits well below what this design should deliver, and regressions show up there before they show up in frame times.

For ground truth beneath the scheduler's own numbers: `perf c2c` for cache-line contention and HITM-remote counts, `perf sched timehist` for migration traces, and MangoHud/`mangoapp` frame-time percentiles for games.

## 6. Implementation roadmap

### Phase 0: Environment (half a day)

Confirm `CONFIG_SCHED_CLASS_EXT=y` (`ls /sys/kernel/sched_ext/`), install toolchain (clang ≥ 17, rustc ≥ 1.82, meson, `bpftool`, `libbpf`), clone scx, build and run `scx_rusty` to validate the stack, and record EEVDF baselines now: schbench, hackbench, a kernel compile, Speedometer in Chrome, and MangoHud captures of two or three Wine games (average, 1% low, frame-time variance). Baselines gathered before writing code are the only ones you'll trust later.

### Phase 1: Skeleton (1–2 days)

New crate `scx_cohort` (crib the layout of `scx_rusty`: `main.rs`, `bpf/main.bpf.c`, build.rs against `scx_utils`). Single global vtime DSQ, default `select_cpu`, no cohorts. Exit criteria: loads, schedules the desktop without visible jank, survives a stress test, ejects on SysRq-S, and the daemon prints the discovered topology correctly (2 LLCs × 8 cores × 2 SMT).

### Phase 2: Per-CCD DSQs and static cohorts (3–5 days)

Two DSQs keyed by LLC id. `task_ctx` with `cohort_id`; tgid grouping and fork inheritance in `init_task`. Cohorts assigned to CCDs round-robin at creation. The `select_cpu` ladder from §4.2, with the steal gate in `dispatch`. Exit criteria: `stress-ng --cpu 8` in one process stays on one CCD (verify with `perf sched` or per-CPU utilization); two such processes land on opposite CCDs; affinity hit rate > 95% for a single multithreaded process; no watchdog ejections under overload (steal gate works).

### Phase 3: Userspace load balancer (3–5 days)

Duty-cycle accounting in BPF, the ~200 ms balancer tick, whole-cohort moves with hysteresis, `scx_stats` integration. Exit criteria: start 3 competing 6-thread processes, observe sensible assignment and at most occasional cohort moves (migrations/min in single digits once settled); kill one, watch rebalance within a second.

### Phase 4: Wake-edge discovery (1 week)

The LRU edge map, sampled recording on the wakeup path, daemon-side decay/merge/split. Exit criteria: launch Chrome, confirm the process tree forms one cohort (lineage) and that a separately launched `pipewire` merges into a game's cohort within ~1 s of audio starting (edges); synthetic test with two pipe-connected processes launched from different shells merges them; overhead of edge recording < 0.5% on hackbench.

### Phase 5: Spill and latency polish (1 week)

Oversized-cohort spill with sticky selection, interactive vtime credit, SMT preference refinements, TOML rules. Exit criteria: Chrome with 20+ tabs keeps compositor chain on one CCD (verify via per-process CPU masks over time); game frame-time 1% lows ≥ EEVDF and ideally ≥ `scx_lavd`; kernel-compile throughput within 3% of EEVDF.

### Phase 6: Bench, tune, package (1 week)

Full benchmark sweep against EEVDF, `scx_rusty`, `scx_lavd`, `scx_bpfland`: games (Proton, MangoHud percentiles), Chrome (Speedometer, dropped-frame counts under load), mixed load (game + compile), microbenches (schbench wakeup latencies, hackbench). Tune the four numbers that matter: steal gate, imbalance threshold, residency time, merge threshold. Package: systemd unit, `scx_loader` integration so it's switchable like the stock schedulers, README with tuning guide. Consider upstreaming to the scx repo; out-of-tree schedulers rot as the kfunc surface evolves, and the compat headers only help if you track the repo.

Total: roughly 4–6 weeks of part-time work, with a usable scheduler (static cohorts, the part that helps games most) at the end of Phase 3.

## 7. Risks and mitigations

*BPF verifier friction.* Loops over CPUs and map-heavy code trip the verifier in non-obvious ways. Mitigate by keeping BPF-side logic to bounded ladders (the scx schedulers are a corpus of verifier-approved patterns to copy) and pushing anything complicated to the daemon.

*Starvation and watchdog ejections.* A too-strict steal gate strands runnable tasks behind a saturated CCD. The vtime clamp plus the wait-time-based steal condition bound worst-case latency; Phase 2's overload test exists to prove it.

*Cohort thrash.* Merge/split flapping or balancer ping-pong would be worse than EEVDF. Every state change carries hysteresis (residency timers, two-tick confirmation, decayed edge counters), and the migrations/min metric makes flapping visible immediately.

*Chrome exceeding one CCD.* Handled by spill, but the "which members are cold" heuristic needs real-world tuning; the fallback is capping cohort size and letting the split logic separate background renderers.

*ntsync (kernel 6.14+).* Wine's newest sync primitive waits in an ioctl on `/dev/ntsync` rather than a futex. Wakeups still traverse the normal wake path, so `SCX_WAKE_SYNC` edges and lineage grouping still work; verify in Phase 4 with a Proton build using ntsync.

*Kernel API drift.* sched_ext's kfunc surface is still moving (the 6.13 renames). Build against scx's `compat.bpf.h` and CI against the kernels you actually run.

## 8. References

- [Extensible Scheduler Class (kernel docs)](https://docs.kernel.org/scheduler/sched-ext.html)
- [sched-ext/scx repository](https://github.com/sched-ext/scx) and [wiki](https://github.com/sched-ext/scx/wiki)
- [scx_utils crate docs (Topology)](https://sched-ext.github.io/scx/scx_utils/index.html)
- [sched_ext_ops reference (docs.ebpf.io)](https://docs.ebpf.io/linux/program-type/BPF_PROG_TYPE_STRUCT_OPS/sched_ext_ops/)
- [LLC awareness in default idle selection (LKML)](https://lkml.rescloud.iu.edu/2410.2/06554.html), [WAKE_SYNC behavior patch (LKML)](https://lkml.rescloud.iu.edu/2410.2/04974.html)
- [Zen 5 cross-CCD latency microcode fix (Tom's Hardware)](https://www.tomshardware.com/pc-components/cpus/amd-microcode-improves-cross-ccd-latency-on-ryzen-9000-cpus-ryzen-9-9900x-and-ryzen-9-9950x-cross-ccd-latency-cut-in-half-to-match-previous-gen-models)
- [scx scheduler overviews (DeepWiki)](https://deepwiki.com/sched-ext/scx)

