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
pub const NR_STATS: u32 = 9;

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
}

/// Per-cohort state. Created by the BPF side on first sight of a cohort;
/// `home_llc` is owned by the daemon's load balancer thereafter.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct CohortCtx {
    /// The LLC this cohort's tasks are placed on by default.
    pub home_llc: u32,
    /// `COHORT_PINNED` etc.
    pub flags: u32,
    /// Live member count; the daemon garbage-collects at 0.
    pub nr_tasks: u64,
    /// Monotonic weight-scaled runtime sum; the daemon diffs per tick to
    /// compute cohort load.
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
    /// Kernel comm, NUL-padded.
    pub comm: [u8; 16],
}

/// Live-tunable knobs, resident in BPF .bss so the daemon can adjust them
/// without reloading the scheduler.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Tunables {
    /// `dispatch` may steal from the foreign LLC only when its DSQ holds
    /// more than this many tasks...
    pub steal_min: u64,
    /// ...or its head task has waited longer than this.
    pub steal_delay_ns: u64,
    /// Upper bound on the interactive vtime credit.
    pub credit_max_ns: u64,
    /// Minimum wake frequency (wakes/sec EWMA) for the interactive credit.
    pub credit_wake_freq_min: u64,
    /// Maximum average runtime for the interactive credit.
    pub credit_runtime_max_ns: u64,
    /// Wake-edge recording is sampled 1-in-2^sample_shift.
    pub sample_shift: u32,
    pub _pad: u32,
}

impl Default for Tunables {
    fn default() -> Self {
        Self {
            steal_min: 2,
            steal_delay_ns: 500_000,
            credit_max_ns: 4_000_000,
            credit_wake_freq_min: 50,
            credit_runtime_max_ns: 2_000_000,
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
        assert_eq!(size_of::<TaskCtx>(), 56);
        assert_eq!(align_of::<TaskCtx>(), 8);
        assert_eq!(size_of::<CohortCtx>(), 24);
        assert_eq!(align_of::<CohortCtx>(), 8);
        assert_eq!(size_of::<WakeEdgeKey>(), 8);
        assert_eq!(size_of::<TaskStat>(), 40);
        assert_eq!(align_of::<TaskStat>(), 8);
        assert_eq!(size_of::<Tunables>(), 48);
    }

    #[test]
    fn tunables_defaults_sane() {
        let t = Tunables::default();
        assert!(t.steal_delay_ns > 0);
        assert!(t.credit_max_ns > 0);
        assert!(t.sample_shift < 16);
    }
}
