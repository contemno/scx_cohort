// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! The load balancer: decides which whole cohorts, if any, change home
//! LLC this tick. Pure logic over [`crate::domain`] snapshots.
//!
//! Stability rules, per DESIGN.md §4.3: whole cohorts move (never split to
//! fix a number), imbalance must persist for `confirm_ticks` consecutive
//! ticks before anything moves, a cohort that just moved is immune for
//! `residency_ms`, and a cohort too big for one LLC is never relocated
//! (spill handles it instead). A tick may move up to
//! `max_moves_per_tick` cohorts: the confirmation gate still decides
//! whether *any* move happens, but a split hands several cohorts one home
//! at once and unwinding that a cohort at a time is too slow to be
//! useful.

use std::collections::{HashMap, HashSet};

use crate::domain::{CohortSnapshot, Decision, LlcLoad, TaskSnapshot};

#[derive(Debug, Clone)]
pub struct BalancerCfg {
    /// Act when the inter-LLC load gap exceeds this percentage of one
    /// LLC's capacity.
    pub imbalance_pct: u64,
    /// Consecutive over-threshold ticks required before moving.
    pub confirm_ticks: u32,
    /// A cohort that moved is immune from moving again for this long.
    pub residency_ms: u64,
    /// Most cohorts relocated in one tick. Splits give every new cohort
    /// the parent's home, so a one-per-tick ceiling (with two-tick
    /// confirmation, one move per 400 ms) needed seconds to unwind a
    /// seven-way split, and left one LLC saturated while the other idled
    /// for the duration. Moves are lazy — members drift over on their
    /// next wakeup — so a bounded batch costs far less than that stall.
    pub max_moves_per_tick: usize,
}

impl Default for BalancerCfg {
    fn default() -> Self {
        Self {
            imbalance_pct: 20,
            confirm_ticks: 2,
            residency_ms: 2000,
            max_moves_per_tick: 8,
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
        let threshold = min.capacity_ns * self.cfg.imbalance_pct / 100;

        if max.load_ns - min.load_ns <= threshold {
            self.streak = 0;
            return vec![];
        }
        self.streak += 1;
        if self.streak < self.cfg.confirm_ticks {
            return vec![];
        }

        // Projected (llc, load, capacity) in `llcs` order — same order, so
        // ties break exactly as they did when this planned one move. Each
        // pick is applied to the projection before the next, so a batch
        // converges on the balance point instead of every member of it
        // chasing the same stale gap and overshooting into a mirror
        // imbalance.
        let mut proj: Vec<(u32, u64, u64)> = llcs
            .iter()
            .map(|l| (l.llc, l.load_ns, l.capacity_ns))
            .collect();
        let mut chosen: HashSet<u64> = HashSet::new();
        let mut out = Vec::new();

        while out.len() < self.cfg.max_moves_per_tick {
            let (hi_llc, hi_load, _) = *proj.iter().max_by_key(|(_, load, _)| *load).unwrap();
            let (lo_llc, lo_load, lo_cap) = *proj.iter().min_by_key(|(_, load, _)| *load).unwrap();
            let gap = hi_load - lo_load;
            if gap <= lo_cap * self.cfg.imbalance_pct / 100 {
                break;
            }

            // Candidates live on the loaded side, are movable, are small
            // enough that one LLC can hold them (oversized cohorts are
            // spill's problem, not relocation's), and have not already
            // been picked earlier in this same batch.
            let best = cohorts
                .iter()
                .filter(|c| {
                    c.home_llc == hi_llc
                        && !c.pinned
                        && c.nr_tasks > 0
                        && c.load_ns > 0
                        && c.load_ns <= lo_cap
                        && !chosen.contains(&c.id)
                        && self.last_move_at.get(&c.id).is_none_or(|at| {
                            now_ms.saturating_sub(*at)
                                >= c.residency_ms.unwrap_or(self.cfg.residency_ms)
                        })
                })
                // Moving c changes the gap to |gap - 2*load|; pick the
                // cohort that repairs it best, and among equals the
                // smaller one (less cache state to re-warm on the other
                // CCD).
                .filter(|c| (gap as i64 - 2 * c.load_ns as i64).unsigned_abs() < gap)
                .min_by_key(|c| {
                    (
                        (gap as i64 - 2 * c.load_ns as i64).unsigned_abs(),
                        c.load_ns,
                    )
                });

            let Some(c) = best else {
                break;
            };

            chosen.insert(c.id);
            self.last_move_at.insert(c.id, now_ms);
            out.push(Decision::MoveCohort {
                id: c.id,
                to: lo_llc,
            });

            for e in proj.iter_mut() {
                if e.0 == hi_llc {
                    e.1 = e.1.saturating_sub(c.load_ns);
                } else if e.0 == lo_llc {
                    e.1 += c.load_ns;
                }
            }
        }

        if !out.is_empty() {
            self.streak = 0;
        }
        out
    }
}

/// Spill handling for cohorts whose demand exceeds one LLC (DESIGN.md
/// §4.3): cold members run remotely first, and when that isn't enough the
/// hottest CPU-bound members are exiled too, until home demand fits.
/// Selection is sticky — already-spilled tasks stay chosen while the
/// overload lasts, so the *same* threads stay remote instead of the whole
/// cohort churning across the boundary. Growth is bounded by what the
/// destination can absorb, and members that burned nothing are never
/// exiled: together those keep an oversized cohort from emptying its home
/// into an already-saturated neighbour.
#[derive(Debug, Clone)]
pub struct SpillCfg {
    /// A cohort spills when its load exceeds this percentage of its home
    /// LLC's capacity...
    pub high_pct: u64,
    /// ...and unspills entirely when it drops below this percentage
    /// (hysteresis gap).
    pub low_pct: u64,
    /// Members below this percentage of one CPU's tick time are "cold":
    /// they spill first, coldest-first, at near-zero cost. If shedding
    /// every cold member still leaves the home overloaded, hot members
    /// spill too — hottest-first, because a CPU-bound thread rarely
    /// sleeps on cohort mates and so pays the least for remoteness,
    /// while often-waking latency-sensitive threads (which sort colder)
    /// keep the home L3. An earlier revision never spilled hot members
    /// and let the steal gate carry the remainder; on uniformly-hot
    /// oversized cohorts (hackbench-class) that meant the other CCD
    /// stole an arbitrary queue head on every dispatch — measured as
    /// hundreds of thousands of cross-CCD migrations per second and an
    /// affinity rate below a coin flip.
    pub cold_max_pct: u64,
    /// One CPU's worth of runtime per tick, for the cold ceiling.
    pub tick_ns: u64,
}

impl Default for SpillCfg {
    fn default() -> Self {
        Self {
            high_pct: 95,
            low_pct: 75,
            cold_max_pct: 25,
            tick_ns: 200_000_000,
        }
    }
}

/// New spill state for one tick: the complete desired (pid → target LLC)
/// set. The caller diffs it against the current map contents.
pub fn plan_spills(
    cfg: &SpillCfg,
    llcs: &[LlcLoad],
    cohorts: &[CohortSnapshot],
    tasks: &[TaskSnapshot],
    current: &HashMap<u32, u32>,
) -> HashMap<u32, u32> {
    let mut desired: HashMap<u32, u32> = HashMap::new();
    if llcs.len() < 2 {
        return desired;
    }
    let capacity: HashMap<u32, u64> = llcs.iter().map(|l| (l.llc, l.capacity_ns)).collect();
    // Load this pass has already promised to each destination, so
    // several oversized cohorts spilling in one tick cannot each claim
    // the same headroom and collectively flood it.
    let mut planned: HashMap<u32, u64> = HashMap::new();

    for cohort in cohorts {
        let Some(&cap) = capacity.get(&cohort.home_llc) else {
            continue;
        };
        let high = cap * cfg.high_pct / 100;
        let low = cap * cfg.low_pct / 100;

        let already: HashSet<u32> = tasks
            .iter()
            .filter(|t| t.cohort_id == cohort.id && current.contains_key(&t.pid))
            .map(|t| t.pid)
            .collect();

        // Below the low mark everything comes home (drop from `desired`);
        // between the marks the existing spill set is kept as-is (sticky);
        // above the high mark the set grows by the coldest members.
        if cohort.load_ns < low {
            continue;
        }

        for pid in &already {
            desired.insert(*pid, *current.get(pid).unwrap());
        }

        // Cohort load counts spilled members too (demand is charged
        // wherever a task runs), so growth must be planned against what
        // is still at home — otherwise every tick re-spills the same
        // excess onto fresh victims and the whole cohort creeps across
        // the boundary.
        let spilled_duty: u64 = tasks
            .iter()
            .filter(|t| already.contains(&t.pid))
            .map(|t| t.duty_ns)
            .sum();
        let home_load = cohort.load_ns.saturating_sub(spilled_duty);
        if home_load < high {
            continue;
        }

        // Only an LLC with room absorbs the spill. Exiling into one
        // already past its own high mark trades a crowded home for a
        // crowded exile, and on a two-CCD part there is no third choice,
        // so an unchecked destination drains one side into the other
        // until half the machine idles behind a saturated queue. Measured
        // under hackbench as affinity falling to 7.5%, one LLC at 9% while
        // the other held above 100%. When nowhere has room the existing
        // set still stands; it just doesn't grow.
        let Some(dest) = llcs
            .iter()
            .filter(|l| l.llc != cohort.home_llc)
            .min_by_key(|l| l.load_ns + planned.get(&l.llc).copied().unwrap_or(0))
        else {
            continue;
        };
        let dest_load = dest.load_ns + planned.get(&dest.llc).copied().unwrap_or(0);
        let headroom = (dest.capacity_ns * cfg.high_pct / 100).saturating_sub(dest_load);
        if headroom == 0 {
            continue;
        }
        let target = dest.llc;

        // Grow the set until the remaining home demand fits under the
        // high mark: cold members first (coldest-first, near-zero cost to
        // run remotely), then — if the cold candidates run out — hot
        // members, hottest-first, so CPU-bound hogs are exiled and the
        // often-waking threads keep the home L3. Never promise the
        // destination more than it can hold.
        let excess = (home_load - high).min(headroom);
        let cold_ceiling = cfg.tick_ns * cfg.cold_max_pct / 100;
        // A first-sighting task's duty reads 0 (no previous sample to
        // diff), which would misfile arbitrarily hot tasks as the very
        // coldest; skip them for one tick until their duty is real.
        let (mut cold, mut hot): (Vec<&TaskSnapshot>, Vec<&TaskSnapshot>) = tasks
            .iter()
            .filter(|t| t.cohort_id == cohort.id && t.duty_known && !already.contains(&t.pid))
            .partition(|t| t.duty_ns < cold_ceiling);
        cold.sort_by_key(|t| t.duty_ns);
        hot.sort_by_key(|t| std::cmp::Reverse(t.duty_ns));

        let mut shed: u64 = 0;
        for t in cold.into_iter().chain(hot) {
            if shed >= excess {
                break;
            }
            // A member that burned nothing this tick sheds nothing by
            // leaving: exiling it cannot close the excess, and it still
            // pays the full price of a cold home L3 the moment it wakes.
            // An earlier revision credited these a nominal 1 ns apiece so
            // the loop was guaranteed to make progress, which meant a
            // cohort of mostly-idle members handed over every one of them
            // before a single hot thread moved. That is how one CCD came
            // to hold an entire cohort while the other emptied.
            if t.duty_ns == 0 {
                continue;
            }
            desired.insert(t.pid, target);
            shed += t.duty_ns;
        }
        *planned.entry(target).or_default() += shed;
    }
    desired
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
            residency_ms: None,
        }
    }

    fn task(pid: u32, cohort_id: u64, duty_ns: u64) -> TaskSnapshot {
        TaskSnapshot {
            pid,
            tgid: pid,
            cohort_id,
            duty_ns,
            duty_known: true,
            home_ns: 0,
            comm: String::new(),
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
    fn a_batch_converges_on_the_balance_point() {
        // Seven cohorts sharing one home is what a split leaves behind:
        // every new cohort inherits the parent's LLC. Unwinding that one
        // move per 400 ms took seconds, long enough to stall the desktop.
        let mut b = Balancer::new(BalancerCfg::default());
        let each = CAP / 10;
        let cohorts: Vec<_> = (1..=7).map(|id| cohort(id, 0, each)).collect();
        let llcs = [llc(0, each * 7, CAP), llc(1, 0, CAP)];
        b.plan(0, &llcs, &cohorts);
        let moves = b.plan(200, &llcs, &cohorts);

        assert!(moves.len() > 1, "batch moved only {}", moves.len());
        let ids: HashSet<u64> = moves
            .iter()
            .map(|d| {
                let Decision::MoveCohort { id, .. } = d;
                *id
            })
            .collect();
        assert_eq!(ids.len(), moves.len(), "a cohort moved twice in one batch");
        for d in &moves {
            let Decision::MoveCohort { to, .. } = d;
            assert_eq!(*to, 1);
        }

        // Land inside the threshold without overshooting into a mirrored
        // imbalance: that is what planning against the projection buys.
        let shifted = moves.len() as u64 * each;
        let gap = (each * 7 - shifted).abs_diff(shifted);
        assert!(
            gap <= CAP * 20 / 100,
            "batch left a gap of {gap} above the threshold"
        );
    }

    #[test]
    fn a_batch_respects_the_cap() {
        let mut b = Balancer::new(BalancerCfg {
            max_moves_per_tick: 2,
            ..BalancerCfg::default()
        });
        let each = CAP / 20;
        let cohorts: Vec<_> = (1..=16).map(|id| cohort(id, 0, each)).collect();
        let llcs = [llc(0, each * 16, CAP), llc(1, 0, CAP)];
        b.plan(0, &llcs, &cohorts);
        assert_eq!(b.plan(200, &llcs, &cohorts).len(), 2);
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

    #[test]
    fn rule_residency_override_respected() {
        let mut b = Balancer::new(BalancerCfg::default());
        let llcs = [llc(0, CAP, CAP), llc(1, CAP / 10, CAP)];
        let mut c = cohort(1, 0, CAP / 4);
        c.residency_ms = Some(10_000); // e.g. a game with extra hysteresis
        let cohorts = [c];
        b.plan(0, &llcs, &cohorts);
        assert!(!b.plan(200, &llcs, &cohorts).is_empty());
        // Default residency (2 s) has long passed, but the override holds.
        b.plan(3000, &llcs, &cohorts);
        assert_eq!(b.plan(3200, &llcs, &cohorts), vec![]);
        // The streak is long-confirmed, so the first plan after the
        // override window moves it again.
        assert!(!b.plan(10_300, &llcs, &cohorts).is_empty());
    }

    // ---- spill ----

    #[test]
    fn undersized_cohort_never_spills() {
        let llcs = [llc(0, CAP / 2, CAP), llc(1, 0, CAP)];
        let cohorts = [cohort(1, 0, CAP / 2)];
        let tasks: Vec<_> = (0..4).map(|i| task(i, 1, CAP / 8)).collect();
        let out = plan_spills(
            &SpillCfg::default(),
            &llcs,
            &cohorts,
            &tasks,
            &HashMap::new(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn oversized_cohort_spills_coldest_members() {
        // Demand just over the high mark: the excess (5% of capacity)
        // is coverable by cold members alone, so hot threads stay home.
        let llcs = [llc(0, CAP, CAP), llc(1, 0, CAP)];
        let cohorts = [cohort(1, 0, CAP)];
        let tasks = vec![
            task(1, 1, CAP / 3),
            task(2, 1, CAP / 3),
            task(3, 1, CAP / 3),
            task(10, 1, CAP / 50),
            task(11, 1, CAP / 50),
            task(12, 1, CAP / 100),
        ];
        let out = plan_spills(
            &SpillCfg::default(),
            &llcs,
            &cohorts,
            &tasks,
            &HashMap::new(),
        );
        assert!(!out.is_empty());
        // Only cold members spill, all to LLC 1, and the hot three stay.
        for pid in [1, 2, 3] {
            assert!(!out.contains_key(&pid), "hot thread {pid} was spilled");
        }
        for (&pid, &target) in &out {
            assert!(pid >= 10);
            assert_eq!(target, 1);
        }
    }

    #[test]
    fn hot_members_overflow_when_cold_insufficient() {
        // Demand 125% of home capacity but the cold members cover only a
        // sliver of the excess: hot members must spill too, and the spill
        // must stop once enough demand has been shed — never the whole
        // cohort.
        let llcs = [llc(0, CAP + CAP / 4, CAP), llc(1, 0, CAP)];
        let cohorts = [cohort(1, 0, CAP + CAP / 4)];
        let tasks = vec![
            task(1, 1, CAP / 3),
            task(2, 1, CAP / 3),
            task(3, 1, CAP / 3),
            task(10, 1, CAP / 100),
        ];
        let out = plan_spills(
            &SpillCfg::default(),
            &llcs,
            &cohorts,
            &tasks,
            &HashMap::new(),
        );
        // Cold member spills, plus exactly one hot member: excess is 30%
        // of capacity and each hot member sheds 33%.
        assert!(out.contains_key(&10), "cold member must spill first");
        let hot_spilled: Vec<u32> = [1, 2, 3]
            .into_iter()
            .filter(|pid| out.contains_key(pid))
            .collect();
        assert_eq!(hot_spilled.len(), 1, "exactly one hot member overflows");
        assert!(out.len() < tasks.len(), "the whole cohort must never spill");
        for target in out.values() {
            assert_eq!(*target, 1);
        }
    }

    #[test]
    fn hot_overflow_exiles_hogs_not_the_waking_thread() {
        // One CPU's tick worth of runtime; duty is capped by it in
        // practice. A render-style thread runs at half duty; the rest are
        // full-duty hogs (shader compilers, spinners). Hottest-first
        // overflow must exile hogs and keep the lighter thread home.
        let tick = CAP / 8;
        let mut tasks = vec![task(1, 1, tick / 2)];
        for pid in 2..=16 {
            tasks.push(task(pid, 1, tick));
        }
        // Demand double the home capacity, all of it hot.
        let llcs = [llc(0, CAP * 2, CAP), llc(1, 0, CAP)];
        let cohorts = [cohort(1, 0, CAP * 2)];
        let out = plan_spills(
            &SpillCfg::default(),
            &llcs,
            &cohorts,
            &tasks,
            &HashMap::new(),
        );
        assert!(
            !out.contains_key(&1),
            "the least-hot member must be the last exiled"
        );
        assert!(!out.is_empty());
        assert!(out.len() < tasks.len());
    }

    #[test]
    fn first_sighting_tasks_are_not_spill_candidates() {
        // Members whose duty has never been sampled read duty 0 and would
        // otherwise sort as the coldest possible picks.
        let llcs = [llc(0, CAP + CAP / 4, CAP), llc(1, 0, CAP)];
        let cohorts = [cohort(1, 0, CAP + CAP / 4)];
        let mut fresh = task(1, 1, 0);
        fresh.duty_known = false;
        let tasks = vec![fresh, task(2, 1, CAP / 2), task(3, 1, CAP / 2)];
        let out = plan_spills(
            &SpillCfg::default(),
            &llcs,
            &cohorts,
            &tasks,
            &HashMap::new(),
        );
        assert!(
            !out.contains_key(&1),
            "unsampled task spilled on its first sighting"
        );
        assert!(!out.is_empty(), "known-duty members must still spill");
    }

    #[test]
    fn idle_members_are_never_exiled() {
        // The production failure: a cohort whose members are nearly all
        // idle. Crediting each one 1 ns of progress against an excess
        // measured in hundreds of millions handed the whole cohort to the
        // other CCD, draining one LLC to single-digit utilisation while
        // the other held above 100%.
        let mut tasks = vec![task(1, 1, CAP / 3), task(2, 1, CAP / 3)];
        for pid in 100..400 {
            tasks.push(task(pid, 1, 0));
        }
        let llcs = [llc(0, CAP + CAP / 4, CAP), llc(1, 0, CAP)];
        let cohorts = [cohort(1, 0, CAP + CAP / 4)];
        let out = plan_spills(
            &SpillCfg::default(),
            &llcs,
            &cohorts,
            &tasks,
            &HashMap::new(),
        );

        assert!(!out.is_empty(), "duty-carrying members must still shed");
        for pid in 100..400 {
            assert!(!out.contains_key(&pid), "idle member {pid} was exiled");
        }
        assert!(
            out.len() <= 2,
            "only duty-carrying members spill, got {}",
            out.len()
        );
    }

    #[test]
    fn a_full_destination_blocks_growth_but_keeps_the_existing_set() {
        // Both LLCs past their high mark: there is nowhere better to put
        // anything, so the set holds instead of pushing more across.
        let llcs = [llc(0, CAP, CAP), llc(1, CAP, CAP)];
        let cohorts = [cohort(1, 0, CAP)];
        let tasks = vec![
            task(1, 1, CAP / 3),
            task(2, 1, CAP / 3),
            task(10, 1, CAP / 50),
        ];
        let current: HashMap<u32, u32> = [(10, 1)].into();
        let out = plan_spills(&SpillCfg::default(), &llcs, &cohorts, &tasks, &current);
        assert_eq!(out, current, "grew the spill set into a full LLC");
    }

    #[test]
    fn concurrent_spills_share_one_destination_budget() {
        // Two oversized cohorts spilling in the same tick must not each
        // claim the destination's full headroom.
        let tick = CAP / 8;
        let mut tasks: Vec<_> = (1..=8).map(|pid| task(pid, 1, tick)).collect();
        tasks.extend((11..=18).map(|pid| task(pid, 2, tick)));
        // LLC 1 is at 90% of capacity: room for a little, not for both.
        let llcs = [llc(0, CAP * 2, CAP), llc(1, CAP * 9 / 10, CAP)];
        let cohorts = [cohort(1, 0, CAP), cohort(2, 0, CAP)];
        let out = plan_spills(
            &SpillCfg::default(),
            &llcs,
            &cohorts,
            &tasks,
            &HashMap::new(),
        );

        let first: Vec<u32> = (1..=8).filter(|p| out.contains_key(p)).collect();
        let second: Vec<u32> = (11..=18).filter(|p| out.contains_key(p)).collect();
        assert!(!first.is_empty(), "the first cohort takes the headroom");
        assert!(
            second.is_empty(),
            "second cohort claimed headroom already spent: {second:?}"
        );
    }

    #[test]
    fn hot_overflow_is_sticky() {
        let llcs = [llc(0, CAP + CAP / 4, CAP), llc(1, 0, CAP)];
        let cohorts = [cohort(1, 0, CAP + CAP / 4)];
        // Uniformly hot: any spilled member is an arbitrary pick, which
        // is exactly when re-planning could churn the choice.
        let tasks: Vec<_> = (1..=8).map(|pid| task(pid, 1, CAP / 6)).collect();
        let cfg = SpillCfg::default();
        let first = plan_spills(&cfg, &llcs, &cohorts, &tasks, &HashMap::new());
        assert!(!first.is_empty());
        // The first plan shed enough; replanning under the same load must
        // neither churn the picks nor creep more members across (cohort
        // load still counts the spilled members, so a naive replan would
        // spill the same excess again every tick).
        let second = plan_spills(&cfg, &llcs, &cohorts, &tasks, &first);
        assert_eq!(second, first, "spill set must converge, not grow");
    }

    #[test]
    fn spill_selection_is_sticky() {
        let llcs = [llc(0, CAP + CAP / 4, CAP), llc(1, 0, CAP)];
        let cohorts = [cohort(1, 0, CAP + CAP / 4)];
        let tasks = vec![
            task(1, 1, CAP / 2),
            task(2, 1, CAP / 2),
            // Equal-duty cold members: without stickiness the choice
            // between them could churn tick to tick.
            task(10, 1, CAP / 50),
            task(11, 1, CAP / 50),
        ];
        let first = plan_spills(
            &SpillCfg::default(),
            &llcs,
            &cohorts,
            &tasks,
            &HashMap::new(),
        );
        let second = plan_spills(&SpillCfg::default(), &llcs, &cohorts, &tasks, &first);
        // Whatever was spilled stays spilled while overload persists.
        for pid in first.keys() {
            assert!(second.contains_key(pid), "spill set churned: lost {pid}");
        }
    }

    #[test]
    fn spill_set_holds_between_marks_and_clears_below_low() {
        let cfg = SpillCfg::default();
        let cohorts_mid = [cohort(1, 0, CAP * 85 / 100)]; // between low and high
        let cohorts_low = [cohort(1, 0, CAP / 2)]; // below low
        let llcs = [llc(0, CAP, CAP), llc(1, 0, CAP)];
        let tasks = vec![task(1, 1, CAP / 2), task(10, 1, CAP / 100)];
        let current: HashMap<u32, u32> = [(10, 1)].into();

        // Between the marks: kept exactly as-is, not grown.
        let mid = plan_spills(&cfg, &llcs, &cohorts_mid, &tasks, &current);
        assert_eq!(mid, current);

        // Below the low mark: everything comes home.
        let low = plan_spills(&cfg, &llcs, &cohorts_low, &tasks, &current);
        assert!(low.is_empty());
    }
}
