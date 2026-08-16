// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! Plain value types the policy planners operate on. `main.rs` builds these
//! from BPF map snapshots and applies the returned decisions as map writes;
//! the planners themselves never touch the skeleton, so they are fully
//! unit-testable without a kernel.

/// Per-LLC load for one balancer tick.
#[derive(Debug, Clone, Copy)]
pub struct LlcLoad {
    pub llc: u32,
    /// Weight-scaled runtime consumed by cohorts homed here this tick.
    pub load_ns: u64,
    /// `cpus_in_llc * tick_duration`: the most runtime this LLC could
    /// have delivered this tick.
    pub capacity_ns: u64,
}

/// One cohort as seen at a balancer tick.
#[derive(Debug, Clone)]
pub struct CohortSnapshot {
    pub id: u64,
    pub home_llc: u32,
    pub pinned: bool,
    pub nr_tasks: u64,
    /// Weight-scaled runtime this tick (delta of the monotonic
    /// `CohortCtx::load_sum`).
    pub load_ns: u64,
}

/// A planner conclusion for `main.rs` to apply to the BPF maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Rewrite the cohort's `home_llc`; members drift over lazily on
    /// their next wakeup.
    MoveCohort { id: u64, to: u32 },
}
