// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! The load balancer: decides which whole cohort, if any, changes home
//! LLC this tick. Pure logic over [`crate::domain`] snapshots.
//!
//! Stability rules, per DESIGN.md §4.3: whole cohorts move (never split to
//! fix a number), imbalance must persist for `confirm_ticks` consecutive
//! ticks before anything moves, a cohort that just moved is immune for
//! `residency_ms`, and a cohort too big for one LLC is never relocated
//! (spill handles it instead).

use std::collections::HashMap;

use crate::domain::{CohortSnapshot, Decision, LlcLoad};

#[derive(Debug, Clone)]
pub struct BalancerCfg {
    /// Act when the inter-LLC load gap exceeds this percentage of one
    /// LLC's capacity.
    pub imbalance_pct: u64,
    /// Consecutive over-threshold ticks required before moving.
    pub confirm_ticks: u32,
    /// A cohort that moved is immune from moving again for this long.
    pub residency_ms: u64,
}

impl Default for BalancerCfg {
    fn default() -> Self {
        Self {
            imbalance_pct: 20,
            confirm_ticks: 2,
            residency_ms: 2000,
        }
    }
}

#[derive(Debug, Default)]
pub struct Balancer {
    cfg: BalancerCfg,
    /// cohort id → monotonic ms timestamp of its last move.
    last_move_at: HashMap<u64, u64>,
    streak: u32,
}

impl Balancer {
    pub fn new(cfg: BalancerCfg) -> Self {
        Self {
            cfg,
            last_move_at: HashMap::new(),
            streak: 0,
        }
    }

    /// Forget move timestamps for cohorts that no longer exist.
    pub fn retain_cohorts(&mut self, live: impl Fn(u64) -> bool) {
        self.last_move_at.retain(|id, _| live(*id));
    }

    pub fn plan(
        &mut self,
        now_ms: u64,
        llcs: &[LlcLoad],
        cohorts: &[CohortSnapshot],
    ) -> Vec<Decision> {
        if llcs.len() < 2 {
            return vec![];
        }

        let max = llcs.iter().max_by_key(|l| l.load_ns).unwrap();
        let min = llcs.iter().min_by_key(|l| l.load_ns).unwrap();
        let gap = max.load_ns - min.load_ns;
        let threshold = min.capacity_ns * self.cfg.imbalance_pct / 100;

        if gap <= threshold {
            self.streak = 0;
            return vec![];
        }
        self.streak += 1;
        if self.streak < self.cfg.confirm_ticks {
            return vec![];
        }

        // Candidates live on the loaded side, are movable, and are small
        // enough that one LLC can hold them (oversized cohorts are spill's
        // problem, not relocation's).
        let best = cohorts
            .iter()
            .filter(|c| {
                c.home_llc == max.llc
                    && !c.pinned
                    && c.nr_tasks > 0
                    && c.load_ns > 0
                    && c.load_ns <= min.capacity_ns
                    && self
                        .last_move_at
                        .get(&c.id)
                        .is_none_or(|at| now_ms.saturating_sub(*at) >= self.cfg.residency_ms)
            })
            // Moving c changes the gap to |gap - 2*load|; pick the cohort
            // that repairs it best, and among equals the smaller one (less
            // cache state to re-warm on the other CCD).
            .filter(|c| (gap as i64 - 2 * c.load_ns as i64).unsigned_abs() < gap)
            .min_by_key(|c| ((gap as i64 - 2 * c.load_ns as i64).unsigned_abs(), c.load_ns));

        match best {
            Some(c) => {
                self.last_move_at.insert(c.id, now_ms);
                self.streak = 0;
                vec![Decision::MoveCohort { id: c.id, to: min.llc }]
            }
            None => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn llc(llc: u32, load_ns: u64, capacity_ns: u64) -> LlcLoad {
        LlcLoad {
            llc,
            load_ns,
            capacity_ns,
        }
    }

    fn cohort(id: u64, home_llc: u32, load_ns: u64) -> CohortSnapshot {
        CohortSnapshot {
            id,
            home_llc,
            pinned: false,
            nr_tasks: 4,
            load_ns,
        }
    }

    // 8 CPUs × 200 ms tick.
    const CAP: u64 = 8 * 200_000_000;

    #[test]
    fn no_move_below_threshold() {
        let mut b = Balancer::new(BalancerCfg::default());
        // Gap of 10% of capacity: under the 20% threshold.
        let llcs = [llc(0, CAP / 2 + CAP / 10, CAP), llc(1, CAP / 2, CAP)];
        let cohorts = [cohort(1, 0, CAP / 10)];
        for _ in 0..5 {
            assert_eq!(b.plan(0, &llcs, &cohorts), vec![]);
        }
    }

    #[test]
    fn two_tick_confirmation() {
        let mut b = Balancer::new(BalancerCfg::default());
        let llcs = [llc(0, CAP, CAP), llc(1, CAP / 10, CAP)];
        let cohorts = [cohort(1, 0, CAP / 4)];
        // First over-threshold tick: observe, don't act.
        assert_eq!(b.plan(0, &llcs, &cohorts), vec![]);
        // Second consecutive tick: act.
        assert_eq!(
            b.plan(200, &llcs, &cohorts),
            vec![Decision::MoveCohort { id: 1, to: 1 }]
        );
    }

    #[test]
    fn transient_spike_resets_streak() {
        let mut b = Balancer::new(BalancerCfg::default());
        let hot = [llc(0, CAP, CAP), llc(1, 0, CAP)];
        let calm = [llc(0, CAP / 2, CAP), llc(1, CAP / 2, CAP)];
        let cohorts = [cohort(1, 0, CAP / 4)];
        assert_eq!(b.plan(0, &hot, &cohorts), vec![]);
        assert_eq!(b.plan(200, &calm, &cohorts), vec![]);
        // Streak was reset; a fresh spike must confirm again.
        assert_eq!(b.plan(400, &hot, &cohorts), vec![]);
        assert!(!b.plan(600, &hot, &cohorts).is_empty());
    }

    #[test]
    fn picks_best_repairing_cohort() {
        let mut b = Balancer::new(BalancerCfg::default());
        // Gap = 60% of capacity; the ideal mover carries half the gap.
        let llcs = [llc(0, CAP * 8 / 10, CAP), llc(1, CAP * 2 / 10, CAP)];
        let gap = CAP * 6 / 10;
        let cohorts = [
            cohort(1, 0, CAP / 100), // too small to matter much
            cohort(2, 0, gap / 2),   // repairs perfectly
            cohort(3, 0, gap),       // overshoots to a mirror imbalance
        ];
        b.plan(0, &llcs, &cohorts);
        assert_eq!(
            b.plan(200, &llcs, &cohorts),
            vec![Decision::MoveCohort { id: 2, to: 1 }]
        );
    }

    #[test]
    fn residency_immunity() {
        let mut b = Balancer::new(BalancerCfg::default());
        let llcs = [llc(0, CAP, CAP), llc(1, CAP / 10, CAP)];
        let cohorts = [cohort(1, 0, CAP / 4)];
        b.plan(0, &llcs, &cohorts);
        assert!(!b.plan(200, &llcs, &cohorts).is_empty());
        // The cohort nominally still looks movable (snapshot says home 0),
        // but it moved 200 ms ago: immune until residency_ms elapses.
        b.plan(400, &llcs, &cohorts);
        assert_eq!(b.plan(600, &llcs, &cohorts), vec![]);
        // The imbalance streak is already confirmed, so the first plan
        // after the residency window moves it again.
        assert!(!b.plan(2500, &llcs, &cohorts).is_empty());
    }

    #[test]
    fn pinned_and_oversized_never_move() {
        let mut b = Balancer::new(BalancerCfg::default());
        let llcs = [llc(0, CAP * 2, CAP), llc(1, 0, CAP)];
        let mut pinned = cohort(1, 0, CAP / 4);
        pinned.pinned = true;
        // Demand exceeding one LLC's capacity: relocation can't help.
        let oversized = cohort(2, 0, CAP + CAP / 4);
        let cohorts = [pinned, oversized];
        b.plan(0, &llcs, &cohorts);
        assert_eq!(b.plan(200, &llcs, &cohorts), vec![]);
    }

    #[test]
    fn never_moves_to_make_things_worse() {
        let mut b = Balancer::new(BalancerCfg::default());
        // Gap is 30% of capacity, but the only candidate is so big that
        // moving it would mirror the imbalance worse than it is.
        let llcs = [llc(0, CAP * 65 / 100, CAP), llc(1, CAP * 35 / 100, CAP)];
        let cohorts = [cohort(1, 0, CAP * 60 / 100)];
        b.plan(0, &llcs, &cohorts);
        assert_eq!(b.plan(200, &llcs, &cohorts), vec![]);
    }
}
