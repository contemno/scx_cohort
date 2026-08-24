// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! Shared definitions for the scx_cohort scheduler.
//!
//! This crate is the single source of truth for every struct and constant
//! that crosses the BPF/userspace boundary. The userspace daemon consumes
//! these types directly (map bytes are reinterpreted via `zerocopy`, no
//! conversion layer), and the BPF component consumes them through
//! `intf.h`, generated from this crate by cbindgen in `scx_cohort/build.rs`.
//! Nothing here is ever defined twice.
//!
//! Every type is `#[repr(C)]` with explicit padding. The `IntoBytes` derive
//! rejects implicit padding at compile time, so the Rust and C layouts
//! cannot drift apart silently.

#![cfg_attr(not(test), no_std)]

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Compile-time bound on CPUs the BPF side sizes per-CPU arrays for.
pub const MAX_CPUS: u32 = 512;

/// Compile-time bound on LLCs (CCDs). The design targets 2; sized with
/// headroom so odd topologies don't fault.
pub const MAX_LLCS: u32 = 8;

/// Maximum number of live cohorts tracked in the cohort map.
pub const MAX_COHORTS: u32 = 1024;

/// Capacity of the LRU wake-edge map.
pub const WAKE_EDGE_SLOTS: u32 = 8192;

/// Cohort id 0 is never allocated; it means "no cohort".
pub const COHORT_INVALID: u64 = 0;

/// `CohortCtx::flags` bit: cohort is pinned to its home LLC by an explicit
/// rule; the load balancer must not move it.
pub const COHORT_PINNED: u32 = 1;

/// `CohortPolicy::flags` bit: wakeups of this cohort's tasks never preempt
/// a running task, even when the wakee carries the interactive signature.
/// The wakee still receives the interactive vtime credit; only the
/// `SCX_ENQ_PREEMPT` insert in `cohort_enqueue` is skipped. Set from a
/// `preempt = false` rule.
pub const COHORT_NO_PREEMPT: u32 = 2;

/// Placement-statistics slots, aggregated per-CPU on the BPF side and
/// summed by the daemon each tick.
pub const STAT_SYNC_LOCAL: u32 = 0;
pub const STAT_PREV_IDLE: u32 = 1;
pub const STAT_IDLE_CORE: u32 = 2;
pub const STAT_IDLE_SMT: u32 = 3;
pub const STAT_HOME_MISS_CLAMP: u32 = 4;
pub const STAT_ENQ_HOME: u32 = 5;
pub const STAT_ENQ_SPILL: u32 = 6;
pub const STAT_STEAL: u32 = 7;
pub const STAT_TCTX_ERR: u32 = 8;
pub const STAT_EXEC_SEVER: u32 = 9;
pub const STAT_PREEMPT: u32 = 10;
/// Wakeups that skipped the idle ladder because the home LLC had no idle
/// CPU at all. Diagnostic for the early-out, not a placement outcome:
/// these still land in `STAT_HOME_MISS_CLAMP`.
pub const STAT_IDLE_SKIP: u32 = 11;
/// Shadow confusion matrix for the latency score vs the binary
/// interactive test, bumped at the enqueue credit gate. The dominant
/// both-false cell is not counted: it would dwarf the interesting cells
/// and its size is recoverable from the enqueue counters anyway.
pub const STAT_SHADOW_BOTH: u32 = 12;
pub const STAT_SHADOW_ONLY_SCORE: u32 = 13;
pub const STAT_SHADOW_ONLY_BINARY: u32 = 14;
/// Failure modes of the wakeup-preemption attempt, split so the two can
/// be told apart on hardware. An interactive wakeup that reaches the
/// victim check either preempts (`STAT_PREEMPT`), finds the previous CPU
/// running a task that wakes too often to be a legal victim
/// (`STAT_PREEMPT_VNOTBATCH` — the middle-class guard), or finds a legal
/// victim inside the per-victim rate limit (`STAT_PREEMPT_RATELIM`).
/// Under a saturating fork-heavy load, VNOTBATCH dwarfing PREEMPT is the
/// signature of interactive wakeups queueing behind full slices because
/// no running task qualifies as a victim.
pub const STAT_PREEMPT_VNOTBATCH: u32 = 15;
pub const STAT_PREEMPT_RATELIM: u32 = 16;
pub const NR_STATS: u32 = 17;

/// Wake-interval clamp for the periodicity EWMAs. Anything slower than
/// ~1 s is "not periodic" for scheduling purposes; the clamp also keeps
/// the interval and jitter EWMAs inside their u32 fields.
pub const SCORE_INTERVAL_CLAMP_NS: u64 = 1 << 30;

/// Interval EWMA ceiling for periodicity points: a task waking less than
/// once a second earns none, however regular it is.
pub const SCORE_PERIODIC_MAX_NS: u64 = 1_000_000_000;

/// Latency-score threshold the shadow counters compare against while
/// `score_gate` is 0, chosen from the expected workload vectors: pingpong
/// pairs land well above it, hackbench receivers (the throughput middle
/// class the preemption guard exists to protect) well below.
pub const SCORE_SHADOW_THRESH: u32 = 60;

/// Descheduling runs before a task's `latency_score` is considered
/// meaningful by the daemon (the `duty_known` analog: the EWMAs need a
/// few samples before they say anything).
pub const SCORE_MIN_RUNS: u32 = 4;

/// Periodicity points from the wake-interval EWMAs: 100 at perfectly
/// regular wakeups, falling linearly as jitter approaches the interval
/// itself. The first observed interval seeds jitter = interval (100%
/// relative jitter, 0 points), so regularity must accumulate before any
/// points are earned — no first-sample flag needed.
///
/// Mirrored in main.bpf.c's periodicity_points(); keep in lockstep.
pub const fn periodicity_points(interval_ewma_ns: u64, jitter_ewma_ns: u64) -> u64 {
    if interval_ewma_ns == 0 || interval_ewma_ns > SCORE_PERIODIC_MAX_NS {
        return 0;
    }
    let rel = jitter_ewma_ns * 100 / interval_ewma_ns;
    100 - if rel > 100 { 100 } else { rel }
}

/// Combine the three 0..=100 signal points into the 0..=100 latency
/// score. Voluntary-block and slice-utilization behavior carry most of
/// the weight; wake periodicity refines within them. The >>3 keeps the
/// whole computation shift-and-add.
///
/// Mirrored in main.bpf.c's combine_score(); keep in lockstep.
pub const fn combine_score(vol_pts: u64, util_pts: u64, per_pts: u64) -> u64 {
    (3 * vol_pts + 3 * util_pts + 2 * per_pts) >> 3
}

/// Per-task scheduling state, stored in BPF task storage. Written almost
/// exclusively by the BPF fast path; the daemon only reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct TaskCtx {
    /// Cohort this task belongs to (key into the cohort map).
    pub cohort_id: u64,
    /// Virtual time charged from weighted runtime; orders the task within
    /// its LLC's DSQ.
    pub vtime: u64,
    /// Timestamp of the last `running` transition.
    pub last_run_at: u64,
    /// EWMA of per-run runtime, for the interactive signature.
    pub avg_runtime_ns: u64,
    /// EWMA of wakeup frequency (wakes/sec), for the interactive signature.
    pub wake_freq: u64,
    /// Timestamp of the last wakeup, feeding the `wake_freq` EWMA.
    pub last_wake_at: u64,
    /// Nonzero when the daemon spilled this task off its cohort's home LLC.
    pub is_spilled: u32,
    /// Target LLC while spilled (valid only when `is_spilled != 0`).
    pub spill_llc: u32,
    /// Cached placement LLC from the last wakeup resolution; `stopping`
    /// compares it against the LLC that actually ran the task to account
    /// home-vs-away runtime.
    pub home_llc: u32,
    /// Nonzero when the task's cohort carries [`COHORT_NO_PREEMPT`];
    /// cached alongside `slice_ns` so `enqueue` needs no policy lookup.
    pub no_preempt: u32,
    /// Effective slice for this task, cached from its cohort's policy on
    /// the wakeup path (`task_home_llc`). Always a real slice once the
    /// task has been placed; the BPF side falls back to the global slice
    /// while it is still zero.
    pub slice_ns: u64,
    /// Timestamp of the last real wakeup (`SCX_ENQ_WAKEUP` only), feeding
    /// the periodicity EWMAs. Separate from `last_wake_at`, which fires
    /// on every runnable transition including preempt requeues.
    pub last_wakeup_at: u64,
    /// EWMA of the wakeup interval, clamped to
    /// [`SCORE_INTERVAL_CLAMP_NS`].
    pub wake_interval_ewma: u32,
    /// EWMA of |interval − interval EWMA|: a mean-absolute-deviation
    /// proxy for jitter — no squares, no history buffer.
    pub wake_jitter_ewma: u32,
    /// EWMA (0..=100) of the voluntary-deschedule fraction: 100 when
    /// every stop is a voluntary block, 0 when every stop is preemption
    /// or slice exhaustion.
    pub vol_ewma: u16,
    /// EWMA (0..=100) of per-run granted-slice utilization. Slice-
    /// relative by construction (each run is divided by the slice the
    /// task was actually granted), so it needs no `scale_to_slice`
    /// recalibration.
    pub slice_util_ewma: u16,
    /// The combined latency score (0..=100); see [`combine_score`].
    /// Shadow-mode: computed and exported everywhere, gating only when
    /// [`Tunables::score_gate`] is nonzero.
    pub latency_score: u16,
    pub _pad_score: u16,
}

/// Per-cohort placement policy. Created once by the BPF side when a
/// cohort first appears; thereafter written only by the daemon (moves,
/// pins) and read by the fast path. Kept separate from [`CohortCounters`]
/// so each side has exclusive write ownership of whole map values —
/// userspace map updates replace entire values and would otherwise race
/// the BPF side's atomic counter arithmetic.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct CohortPolicy {
    /// The LLC this cohort's tasks are placed on by default.
    pub home_llc: u32,
    /// `COHORT_PINNED` etc.
    pub flags: u32,
    /// Per-cohort scheduling slice in nanoseconds; 0 means "use the
    /// global slice". Set from a `slice_us` rule. Deliberately NOT fed
    /// into [`Tunables::scale_to_slice`]: the interactive-credit
    /// calibration stays tied to the global slice (see the comment at
    /// the vtime clamp in `cohort_enqueue`).
    pub slice_ns: u64,
}

/// Per-cohort counters, written only by the BPF side (atomically); the
/// daemon reads them and never writes existing entries back.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct CohortCounters {
    /// Approximate live member count (informational; the daemon derives
    /// exact membership from task_stats).
    pub nr_tasks: u64,
    /// Monotonic weight-scaled runtime sum; the daemon diffs per tick to
    /// compute the cohort's demand.
    pub load_sum: u64,
}

/// Key of the LRU wake-edge map. Keyed by tgid pair (not cohort pair) so
/// one dataset supports both cross-cohort merges and intra-cohort splits.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct WakeEdgeKey {
    pub waker_tgid: u32,
    pub wakee_tgid: u32,
}

/// Per-task bookkeeping the daemon iterates each tick (duty cycles, spill
/// candidate selection, comm-based rule matching). Keyed by pid.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct TaskStat {
    pub tgid: u32,
    /// Duplicated from `TaskCtx` at each deschedule so the daemon's one
    /// map iteration sees it.
    pub latency_score: u16,
    /// Saturating count of descheduling runs that have updated the
    /// score; the daemon treats scores with fewer than
    /// [`SCORE_MIN_RUNS`] as unknown.
    pub score_runs: u16,
    /// Duplicated from `TaskCtx` so one map iteration suffices.
    pub cohort_id: u64,
    /// Monotonic on-CPU nanoseconds; the daemon diffs per tick for duty.
    pub runtime_sum: u64,
    /// The subset of `runtime_sum` executed on the task's home LLC; the
    /// per-tick ratio of the two deltas is the task's affinity.
    pub runtime_home_sum: u64,
    /// Kernel comm, NUL-padded.
    pub comm: [u8; 16],
}

/// The `--slice-us` default, in nanoseconds. [`Tunables::default`] is
/// calibrated against it, so [`Tunables::scale_to_slice`] is a no-op at
/// this slice and only ever fires when the operator picks another.
pub const DEFAULT_SLICE_NS: u64 = 2_500_000;

/// Live-tunable knobs, resident in BPF .bss so the daemon can adjust them
/// without reloading the scheduler.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Tunables {
    /// `dispatch` may steal from the foreign LLC only when its DSQ holds
    /// more than this many tasks...
    pub steal_min: u64,
    /// ...AND its head task has waited longer than this. Both must hold:
    /// depth alone is normal operation on a busy CCD, and depth-triggered
    /// stealing un-places what spill placed (see cohort_dispatch).
    pub steal_delay_ns: u64,
    /// Upper bound on the interactive vtime credit. Meaningful only
    /// against the clamp window, which is one slice wide: a credit far
    /// larger than the slice collapses every enqueued task onto the same
    /// effective vtime and the ordering stops discriminating. Kept below
    /// the slice by [`Tunables::scale_to_slice`].
    pub credit_max_ns: u64,
    /// Minimum wake frequency (wakes/sec EWMA) for the interactive credit.
    pub credit_wake_freq_min: u64,
    /// Maximum average runtime for the interactive credit. "Runs briefly"
    /// is only meaningful relative to how long a task is *allowed* to run,
    /// so [`Tunables::scale_to_slice`] holds this under the slice; left
    /// absolute, a short slice caps every task's runtime beneath it and
    /// the whole machine classifies as interactive.
    pub credit_runtime_max_ns: u64,
    /// An interactive wakee may preempt its previous CPU (when that CPU
    /// runs a firmly-batch, rarely-waking task) at most once per victim
    /// CPU per this interval. Bounds the worst-case preemption IPI rate.
    pub preempt_min_ns: u64,
    /// Wake-edge recording is sampled 1-in-2^sample_shift.
    pub sample_shift: u32,
    /// EXPERIMENTAL. 0 (the default): the latency score runs in shadow
    /// mode — computed, exported, counted against the binary interactive
    /// test, but never gating. Nonzero (1..=100): `latency_score >=
    /// score_gate` replaces the binary test for the vtime credit and
    /// wakee-side preemption. The victim-side `curr_batch` predicate
    /// stays on `wake_freq` either way: a low score does not make a task
    /// safe to preempt (that is the hackbench middle class again).
    pub score_gate: u32,
}

impl Tunables {
    /// Rescale the interactive knobs to fit `slice_ns`.
    ///
    /// Both credit knobs describe behaviour *relative to a slice* but are
    /// stored absolute, calibrated against [`DEFAULT_SLICE_NS`]. Shorten
    /// the slice without rescaling and they invert: once no task can run
    /// as long as `credit_runtime_max_ns`, every task on the machine
    /// passes the "runs briefly" test and the credit is granted
    /// universally. A bonus everyone receives orders nothing, and a
    /// credit wider than the clamp window (one slice) flattens what
    /// ordering remains. Measured on a 5950X at a 750 us slice as
    /// hackbench taking three times as long as EEVDF.
    ///
    /// Scaling is one-directional: these only ever move *down* to fit a
    /// shorter slice, so an explicitly configured ceiling is never raised
    /// and the defaults are unchanged at the default slice.
    pub fn scale_to_slice(&mut self, slice_ns: u64) {
        if slice_ns == 0 {
            return;
        }
        // Half a slice: a task that blocks before burning half of what it
        // was given is running briefly by any reading.
        self.credit_runtime_max_ns = self.credit_runtime_max_ns.min(slice_ns / 2);
        // Four fifths of a slice, the ratio the 5 ms default already used
        // (4 ms of 5 ms). Division first so a large slice can't overflow.
        self.credit_max_ns = self.credit_max_ns.min(slice_ns / 5 * 4);
    }
}

impl Default for Tunables {
    fn default() -> Self {
        Self {
            steal_min: 2,
            steal_delay_ns: 500_000,
            credit_max_ns: 2_000_000,
            credit_wake_freq_min: 50,
            credit_runtime_max_ns: 1_250_000,
            preempt_min_ns: 200_000,
            sample_shift: 3,
            score_gate: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    /// The C side sees these exact layouts via the generated header; these
    /// assertions pin them so refactors can't change them unnoticed.
    #[test]
    fn layouts_are_stable() {
        assert_eq!(size_of::<TaskCtx>(), 96);
        assert_eq!(align_of::<TaskCtx>(), 8);
        assert_eq!(size_of::<CohortPolicy>(), 16);
        assert_eq!(align_of::<CohortPolicy>(), 8);
        assert_eq!(size_of::<CohortCounters>(), 16);
        assert_eq!(align_of::<CohortCounters>(), 8);
        assert_eq!(size_of::<WakeEdgeKey>(), 8);
        assert_eq!(size_of::<TaskStat>(), 48);
        assert_eq!(align_of::<TaskStat>(), 8);
        assert_eq!(size_of::<Tunables>(), 56);
    }

    #[test]
    fn defaults_are_a_fixed_point_of_the_default_slice() {
        // The stored knobs are calibrated against DEFAULT_SLICE_NS, so
        // scaling there must change nothing. If someone retunes the slice
        // default without retuning these, this catches it.
        let base = Tunables::default();
        let mut t = Tunables::default();
        t.scale_to_slice(DEFAULT_SLICE_NS);
        assert_eq!(t.credit_runtime_max_ns, base.credit_runtime_max_ns);
        assert_eq!(t.credit_max_ns, base.credit_max_ns);
    }

    #[test]
    fn a_short_slice_tightens_the_interactive_signature() {
        // The regression: at 750 us the stored 2 ms ceiling exceeds the
        // slice, so every task reads as "runs briefly" and the credit
        // stops discriminating.
        let slice = 750_000;
        let mut t = Tunables::default();
        t.scale_to_slice(slice);
        assert!(
            t.credit_runtime_max_ns < slice,
            "a task cannot run a full slice and still count as brief"
        );
        assert!(
            t.credit_max_ns < slice,
            "credit above the clamp window flattens the vtime ordering"
        );
        assert_eq!(t.credit_runtime_max_ns, 375_000);
        assert_eq!(t.credit_max_ns, 600_000);
    }

    #[test]
    fn scaling_never_raises_a_configured_ceiling() {
        let mut t = Tunables {
            credit_max_ns: 1_000,
            credit_runtime_max_ns: 2_000,
            ..Tunables::default()
        };
        t.scale_to_slice(5_000_000);
        assert_eq!(t.credit_max_ns, 1_000);
        assert_eq!(t.credit_runtime_max_ns, 2_000);
    }

    #[test]
    fn a_zero_slice_is_left_alone() {
        let mut t = Tunables::default();
        t.scale_to_slice(0);
        assert_eq!(t.credit_max_ns, Tunables::default().credit_max_ns);
    }

    #[test]
    fn score_is_bounded_and_monotone_in_its_inputs() {
        assert_eq!(combine_score(0, 0, 0), 0);
        assert_eq!(combine_score(100, 100, 100), 100);
        // The weights sum to 8, so >>3 can never exceed the input range.
        for v in [0u64, 25, 50, 75, 100] {
            assert!(combine_score(v, v, v) <= 100);
        }
        // More of any signal never lowers the score.
        assert!(combine_score(80, 50, 50) >= combine_score(50, 50, 50));
        assert!(combine_score(50, 80, 50) >= combine_score(50, 50, 50));
        assert!(combine_score(50, 50, 80) >= combine_score(50, 50, 50));
    }

    #[test]
    fn periodicity_edges() {
        // No interval history yet, or waking slower than once a second:
        // no points, however small the jitter reads.
        assert_eq!(periodicity_points(0, 0), 0);
        assert_eq!(periodicity_points(SCORE_PERIODIC_MAX_NS + 1, 0), 0);
        // Perfectly regular.
        assert_eq!(periodicity_points(1_000_000, 0), 100);
        // The first-sample seed (jitter = interval) reads as 100%
        // relative jitter: zero points until regularity accumulates.
        assert_eq!(periodicity_points(250_000, 250_000), 0);
        // Jitter beyond the interval clamps rather than underflowing.
        assert_eq!(periodicity_points(100, 100_000), 0);
        // Halfway.
        assert_eq!(periodicity_points(1_000_000, 500_000), 50);
    }

    /// The three calibration vectors the shadow threshold was chosen
    /// against, expressed as steady-state EWMA inputs. If the formula or
    /// weights change, these force the threshold discussion to reopen.
    #[test]
    fn workload_vectors_straddle_the_shadow_threshold() {
        let thresh = SCORE_SHADOW_THRESH as u64;

        // Spinner: every stop is slice exhaustion (vol 0), full slice
        // utilization (util_pts 0), never a real wakeup (per 0).
        let spinner = combine_score(0, 100 - 100, periodicity_points(0, 0));
        assert_eq!(spinner, 0);

        // Pingpong pair: always blocks voluntarily, burns ~2% of its
        // slice per run, wakes every ~200 us with ~10% jitter.
        let pingpong = combine_score(100, 100 - 2, periodicity_points(200_000, 20_000));
        assert!(pingpong >= 90, "pingpong scored {pingpong}");

        // Hackbench receiver: mixes voluntary blocks with slice
        // exhaustion while draining bursts, runs most of its slice, and
        // its wake intervals are bursty (jitter near the interval). The
        // load-bearing vector: the throughput middle class must stay
        // below the threshold or the preemption guard regression
        // (hackbench +52%) comes back.
        let receiver = combine_score(60, 100 - 70, periodicity_points(500_000, 400_000));
        assert!(
            receiver < thresh,
            "hackbench receiver scored {receiver}, must stay below {thresh}"
        );
        assert!(pingpong >= thresh && spinner < thresh);
    }

    #[test]
    fn tunables_defaults_sane() {
        let t = Tunables::default();
        assert!(t.steal_delay_ns > 0);
        assert!(t.credit_max_ns > 0);
        assert!(t.sample_shift < 16);
    }
}
