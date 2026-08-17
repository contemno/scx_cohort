// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! Wake-edge analysis: turns the BPF side's sampled (waker_tgid,
//! wakee_tgid) counters into cohort merges and splits. Pure logic; the
//! caller feeds per-tick counter deltas and the current tgid→cohort
//! mapping, and applies the returned plans to the maps.
//!
//! Hysteresis in both directions: a merge needs the cross-cohort wake
//! rate to stay above threshold for `merge_confirm_ticks` consecutive
//! ticks (a burst can't join strangers), and a split needs a cohort's
//! internal wake graph to stay disconnected for `split_confirm_ticks`
//! ticks after edge rates have decayed (a momentary lull can't shatter a
//! real cohort).

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

/// An undirected edge key: tgids in sorted order.
fn ekey(a: u32, b: u32) -> (u32, u32) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

#[derive(Debug, Clone)]
pub struct WakeGraphCfg {
    /// Cross-cohort wake rate (wakes/sec, sample-compensated) that
    /// triggers a merge.
    pub merge_wakes_per_sec: f64,
    /// Consecutive over-threshold ticks required to merge.
    pub merge_confirm_ticks: u32,
    /// An intra-cohort edge below this rate no longer counts as
    /// connecting its endpoints.
    pub split_min_wakes_per_sec: f64,
    /// Consecutive disconnected ticks required to split.
    pub split_confirm_ticks: u32,
    /// Don't split off a component with fewer tasks than this.
    pub split_min_tasks: u64,
    /// EWMA time constant for edge rates, in seconds.
    pub rate_tau_s: f64,
}

impl Default for WakeGraphCfg {
    fn default() -> Self {
        Self {
            merge_wakes_per_sec: 300.0,
            merge_confirm_ticks: 3,
            split_min_wakes_per_sec: 10.0,
            split_confirm_ticks: 25, // ~5 s of quiet at a 200 ms tick
            split_min_tasks: 2,
            rate_tau_s: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergePlan {
    /// Cohort that absorbs the other (the one with more tasks).
    pub into: u64,
    pub from: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlan {
    pub cohort: u64,
    /// tgids of the component to split off into a fresh cohort.
    pub off_tgids: Vec<u32>,
}

#[derive(Debug, Default)]
struct EdgeState {
    /// EWMA of the wake rate.
    rate: f64,
    /// This tick's instantaneous rate. The merge confirmation counter
    /// advances only on ticks with real over-threshold traffic, so the
    /// EWMA's memory of a burst can't accumulate confirmation on its own.
    inst: f64,
    over_ticks: u32,
}

#[derive(Debug)]
pub struct WakeGraph {
    cfg: WakeGraphCfg,
    edges: HashMap<(u32, u32), EdgeState>,
    /// cohort → consecutive ticks its wake graph has been disconnected.
    split_streak: HashMap<u64, u32>,
}

impl WakeGraph {
    pub fn new(cfg: WakeGraphCfg) -> Self {
        Self {
            cfg,
            edges: HashMap::new(),
            split_streak: HashMap::new(),
        }
    }

    /// Fold one tick's sampled counter deltas into the edge-rate EWMAs.
    /// `sample_mult` compensates for BPF-side sampling (2^sample_shift).
    pub fn observe(&mut self, deltas: &[(u32, u32, u64)], dt_s: f64, sample_mult: u64) {
        if dt_s <= 0.0 {
            return;
        }
        let alpha = dt_s / (dt_s + self.cfg.rate_tau_s);

        // Decay every known edge, then blend in this tick's traffic.
        let mut fresh: HashMap<(u32, u32), u64> = HashMap::new();
        for &(a, b, count) in deltas {
            *fresh.entry(ekey(a, b)).or_default() += count * sample_mult;
        }

        for (key, st) in self.edges.iter_mut() {
            let inst = *fresh.get(key).unwrap_or(&0) as f64 / dt_s;
            st.inst = inst;
            st.rate += alpha * (inst - st.rate);
        }
        for (key, count) in fresh {
            if let Entry::Vacant(e) = self.edges.entry(key) {
                let inst = count as f64 / dt_s;
                e.insert(EdgeState {
                    rate: alpha * inst,
                    inst,
                    over_ticks: 0,
                });
            }
        }
        // Edges decayed to noise are dropped entirely.
        self.edges
            .retain(|_, st| st.rate >= self.cfg.split_min_wakes_per_sec / 10.0);
    }

    /// Cross-cohort edges sustaining the merge rate: returns at most one
    /// merge per (into, from) pair per call; the caller applies them and
    /// the next tick sees the updated mapping.
    pub fn merges(
        &mut self,
        tgid_cohort: &HashMap<u32, u64>,
        cohort_sizes: &HashMap<u64, u64>,
    ) -> Vec<MergePlan> {
        let mut out = Vec::new();
        let mut merged: HashSet<u64> = HashSet::new();

        for (&(a, b), st) in self.edges.iter_mut() {
            let (Some(&ca), Some(&cb)) = (tgid_cohort.get(&a), tgid_cohort.get(&b)) else {
                continue;
            };
            if ca == cb {
                st.over_ticks = 0;
                continue;
            }
            if st.rate < self.cfg.merge_wakes_per_sec || st.inst < self.cfg.merge_wakes_per_sec {
                st.over_ticks = 0;
                continue;
            }
            st.over_ticks += 1;
            if st.over_ticks < self.cfg.merge_confirm_ticks {
                continue;
            }
            // One merge per cohort per tick keeps application simple.
            if merged.contains(&ca) || merged.contains(&cb) {
                continue;
            }
            let (sa, sb) = (
                cohort_sizes.get(&ca).copied().unwrap_or(0),
                cohort_sizes.get(&cb).copied().unwrap_or(0),
            );
            let (into, from) = if sa >= sb { (ca, cb) } else { (cb, ca) };
            merged.insert(ca);
            merged.insert(cb);
            st.over_ticks = 0;
            out.push(MergePlan { into, from });
        }
        out
    }

    /// Cohorts whose internal wake graph has stayed disconnected: returns
    /// components to split off. `cohort_tgids` maps each cohort to its
    /// member tgids with their task counts.
    pub fn splits(&mut self, cohort_tgids: &HashMap<u64, Vec<(u32, u64)>>) -> Vec<SplitPlan> {
        let mut out = Vec::new();

        // Cohorts that no longer exist must not leak their streaks (ids
        // are never reused, so a stale entry would live forever).
        self.split_streak
            .retain(|cohort, _| cohort_tgids.contains_key(cohort));

        for (&cohort, members) in cohort_tgids {
            if members.len() < 2 {
                self.split_streak.remove(&cohort);
                continue;
            }

            // Union-find over member tgids using live edges.
            let mut parent: HashMap<u32, u32> = members.iter().map(|&(t, _)| (t, t)).collect();
            fn find(parent: &mut HashMap<u32, u32>, x: u32) -> u32 {
                let p = parent[&x];
                if p == x {
                    return x;
                }
                let root = find(parent, p);
                parent.insert(x, root);
                root
            }
            for (&(a, b), st) in self.edges.iter() {
                if st.rate < self.cfg.split_min_wakes_per_sec {
                    continue;
                }
                if parent.contains_key(&a) && parent.contains_key(&b) {
                    let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                    if ra != rb {
                        parent.insert(ra, rb);
                    }
                }
            }

            let mut components: HashMap<u32, Vec<(u32, u64)>> = HashMap::new();
            for &(t, n) in members {
                let root = find(&mut parent, t);
                components.entry(root).or_default().push((t, n));
            }

            if components.len() < 2 {
                self.split_streak.remove(&cohort);
                continue;
            }

            let streak = self.split_streak.entry(cohort).or_insert(0);
            *streak += 1;
            if *streak < self.cfg.split_confirm_ticks {
                continue;
            }

            // Split off every component except the largest (by tasks),
            // skipping ones under the footprint floor.
            let mut comps: Vec<Vec<(u32, u64)>> = components.into_values().collect();
            comps.sort_by_key(|c| std::cmp::Reverse(c.iter().map(|&(_, n)| n).sum::<u64>()));
            for comp in comps.into_iter().skip(1) {
                let tasks: u64 = comp.iter().map(|&(_, n)| n).sum();
                if tasks < self.cfg.split_min_tasks {
                    continue;
                }
                out.push(SplitPlan {
                    cohort,
                    off_tgids: comp.into_iter().map(|(t, _)| t).collect(),
                });
            }
            self.split_streak.remove(&cohort);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 0.2;

    fn cfg() -> WakeGraphCfg {
        WakeGraphCfg {
            merge_wakes_per_sec: 300.0,
            merge_confirm_ticks: 3,
            split_min_wakes_per_sec: 10.0,
            split_confirm_ticks: 5,
            split_min_tasks: 2,
            rate_tau_s: 0.5,
        }
    }

    /// 500 wakes/tick at DT=0.2 → 2500/s instantaneous, converging well
    /// above the 300/s merge threshold within a few ticks.
    const HOT: u64 = 500;

    #[test]
    fn sustained_rate_merges_into_bigger_cohort() {
        let mut g = WakeGraph::new(cfg());
        let mapping: HashMap<u32, u64> = [(100, 1), (200, 2)].into();
        let sizes: HashMap<u64, u64> = [(1, 24), (2, 2)].into();

        let mut merges = vec![];
        for _ in 0..10 {
            g.observe(&[(100, 200, HOT)], DT, 1);
            merges = g.merges(&mapping, &sizes);
            if !merges.is_empty() {
                break;
            }
        }
        // pipewire (cohort 2, small) joins the game (cohort 1, big).
        assert_eq!(merges, vec![MergePlan { into: 1, from: 2 }]);
    }

    #[test]
    fn burst_does_not_merge() {
        let mut g = WakeGraph::new(cfg());
        let mapping: HashMap<u32, u64> = [(100, 1), (200, 2)].into();
        let sizes: HashMap<u64, u64> = [(1, 4), (2, 4)].into();

        // Two hot ticks (below merge_confirm_ticks), then silence.
        for _ in 0..2 {
            g.observe(&[(100, 200, HOT)], DT, 1);
            assert!(g.merges(&mapping, &sizes).is_empty());
        }
        for _ in 0..20 {
            g.observe(&[], DT, 1);
            assert!(g.merges(&mapping, &sizes).is_empty());
        }
    }

    #[test]
    fn rates_decay_when_traffic_stops() {
        let mut g = WakeGraph::new(cfg());
        for _ in 0..10 {
            g.observe(&[(100, 200, HOT)], DT, 1);
        }
        let hot_rate = g.edges[&(100, 200)].rate;
        assert!(hot_rate > 300.0);
        for _ in 0..30 {
            g.observe(&[], DT, 1);
        }
        // Decayed to noise and dropped, or at least far below threshold.
        let cold = g.edges.get(&(100, 200)).map(|e| e.rate).unwrap_or(0.0);
        assert!(cold < 10.0, "rate failed to decay: {cold}");
    }

    #[test]
    fn sample_compensation_applies() {
        let mut g = WakeGraph::new(cfg());
        let mapping: HashMap<u32, u64> = [(100, 1), (200, 2)].into();
        let sizes: HashMap<u64, u64> = [(1, 4), (2, 2)].into();

        // 63 sampled wakes/tick at 8x sampling = ~2500/s effective.
        let mut merged = false;
        for _ in 0..10 {
            g.observe(&[(100, 200, HOT / 8)], DT, 8);
            merged |= !g.merges(&mapping, &sizes).is_empty();
        }
        assert!(merged);
    }

    #[test]
    fn disconnected_components_split_after_confirm() {
        let mut g = WakeGraph::new(cfg());
        // Cohort 1: tgids {10, 11} chat; {20, 21} chat; no cross edges.
        let members: HashMap<u64, Vec<(u32, u64)>> =
            [(1u64, vec![(10, 8), (11, 4), (20, 3), (21, 2)])].into();

        let mut splits = vec![];
        for _ in 0..cfg().split_confirm_ticks + 2 {
            g.observe(&[(10, 11, HOT), (20, 21, HOT)], DT, 1);
            splits = g.splits(&members);
            if !splits.is_empty() {
                break;
            }
        }
        assert_eq!(splits.len(), 1);
        let split = &splits[0];
        assert_eq!(split.cohort, 1);
        // The smaller component {20, 21} splits off.
        let mut off = split.off_tgids.clone();
        off.sort();
        assert_eq!(off, vec![20, 21]);
    }

    #[test]
    fn connected_cohort_never_splits() {
        let mut g = WakeGraph::new(cfg());
        // A bridge edge keeps everything one component.
        let members: HashMap<u64, Vec<(u32, u64)>> =
            [(1u64, vec![(10, 8), (11, 4), (20, 3)])].into();
        for _ in 0..30 {
            g.observe(&[(10, 11, HOT), (11, 20, HOT)], DT, 1);
            assert!(g.splits(&members).is_empty());
        }
    }

    #[test]
    fn momentary_disconnect_does_not_split() {
        let mut g = WakeGraph::new(cfg());
        let members: HashMap<u64, Vec<(u32, u64)>> = [(1u64, vec![(10, 8), (20, 4)])].into();

        // Disconnected for a few ticks (below confirm), then the bridge
        // returns: the streak must reset.
        for _ in 0..cfg().split_confirm_ticks - 1 {
            g.observe(&[], DT, 1);
            assert!(g.splits(&members).is_empty());
        }
        for _ in 0..3 {
            g.observe(&[(10, 20, HOT)], DT, 1);
            assert!(g.splits(&members).is_empty());
        }
        for _ in 0..cfg().split_confirm_ticks - 1 {
            g.observe(&[], DT, 1);
            assert!(g.splits(&members).is_empty());
        }
    }

    #[test]
    fn footprint_floor_blocks_tiny_splits() {
        let mut g = WakeGraph::new(cfg());
        // The disconnected component has only 1 task: below the floor.
        let members: HashMap<u64, Vec<(u32, u64)>> =
            [(1u64, vec![(10, 8), (11, 4), (20, 1)])].into();
        for _ in 0..cfg().split_confirm_ticks + 5 {
            g.observe(&[(10, 11, HOT)], DT, 1);
            assert!(g.splits(&members).is_empty());
        }
    }
}
