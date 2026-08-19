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
pub const NR_STATS: u32 = 12;

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
    pub _pad: u32,
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
    pub _pad: u32,
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
    pub _pad: u32,
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
            _pad: 0,
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
        assert_eq!(size_of::<TaskCtx>(), 64);
        assert_eq!(align_of::<TaskCtx>(), 8);
        assert_eq!(size_of::<CohortPolicy>(), 8);
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
    fn tunables_defaults_sane() {
        let t = Tunables::default();
        assert!(t.steal_delay_ns > 0);
        assert!(t.credit_max_ns > 0);
        assert!(t.sample_shift < 16);
    }
}
