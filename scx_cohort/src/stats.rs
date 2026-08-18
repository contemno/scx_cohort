// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! scx_stats metrics: served over the standard scx stats socket (readable
//! with `scxtop` or `scx_cohort --monitor`).

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use scx_stats::prelude::*;
use scx_stats_derive::stat_doc;
use scx_stats_derive::Stats;
use serde::Deserialize;
use serde::Serialize;

#[stat_doc]
#[derive(Clone, Debug, Default, Serialize, Deserialize, Stats)]
#[stat(_om_prefix = "l_", _om_label = "llc")]
pub struct LlcMetrics {
    #[stat(desc = "load homed on this LLC as a fraction of its capacity")]
    pub util: f64,
    #[stat(desc = "cohorts homed on this LLC")]
    pub nr_cohorts: u64,
}

#[stat_doc]
#[derive(Clone, Debug, Default, Serialize, Deserialize, Stats)]
#[stat(top)]
pub struct Metrics {
    #[stat(desc = "live cohorts")]
    pub nr_cohorts: u64,
    #[stat(desc = "live tasks under the scheduler")]
    pub nr_tasks: u64,
    #[stat(desc = "% of placements landing on the home CCD (the headline number)")]
    pub affinity_hit_pct: f64,
    #[stat(desc = "% of placements landing where planned, home or spill target")]
    pub plan_hit_pct: f64,

    #[stat(desc = "WAKE_SYNC handoffs to a same-cohort waker's CPU")]
    pub nr_sync_local: u64,
    #[stat(desc = "placements on an idle prev_cpu with a fully idle core")]
    pub nr_prev_idle: u64,
    #[stat(desc = "placements on a fully idle core in the home CCD")]
    pub nr_idle_core: u64,
    #[stat(desc = "placements on an idle SMT sibling in the home CCD")]
    pub nr_idle_smt: u64,
    #[stat(desc = "wakeups clamped into a busy home CCD")]
    pub nr_home_miss_clamp: u64,
    #[stat(desc = "enqueues into the home CCD's queue")]
    pub nr_enq_home: u64,
    #[stat(desc = "enqueues of spilled tasks onto the foreign CCD")]
    pub nr_enq_spill: u64,
    #[stat(desc = "tasks stolen across the fabric by an idle CCD")]
    pub nr_steals: u64,
    #[stat(desc = "task context lookup failures (should stay 0)")]
    pub nr_tctx_errors: u64,

    #[stat(desc = "cohorts started fresh at exec (lineage severed)")]
    pub nr_exec_severs: u64,
    #[stat(desc = "cohort home moves by the load balancer")]
    pub nr_migrations: u64,
    #[stat(desc = "cohort merges from wake-edge discovery")]
    pub nr_merges: u64,
    #[stat(desc = "cohort splits from wake-edge discovery")]
    pub nr_splits: u64,
    #[stat(desc = "tasks currently spilled off their home CCD (gauge)")]
    pub nr_spilled: u64,
    #[stat(desc = "per-LLC statistics")]
    pub llcs: BTreeMap<usize, LlcMetrics>,
}

impl Metrics {
    pub fn format<W: Write>(&self, w: &mut W) -> Result<()> {
        writeln!(
            w,
            "[scx_cohort] cohorts={:<4} tasks={:<5} affinity={:5.1}% plan={:5.1}% moves={} merges={} splits={} execs={} spilled={}",
            self.nr_cohorts,
            self.nr_tasks,
            self.affinity_hit_pct,
            self.plan_hit_pct,
            self.nr_migrations,
            self.nr_merges,
            self.nr_splits,
            self.nr_exec_severs,
            self.nr_spilled,
        )?;
        writeln!(
            w,
            "  place: sync={} prev={} core={} smt={} clamp={} enq={} | spill={} steal={} tctx_err={}",
            self.nr_sync_local,
            self.nr_prev_idle,
            self.nr_idle_core,
            self.nr_idle_smt,
            self.nr_home_miss_clamp,
            self.nr_enq_home,
            self.nr_enq_spill,
            self.nr_steals,
            self.nr_tctx_errors,
        )?;
        for (id, llc) in self.llcs.iter() {
            writeln!(
                w,
                "  LLC[{}] util={:5.1}% cohorts={}",
                id,
                llc.util * 100.0,
                llc.nr_cohorts
            )?;
        }
        Ok(())
    }

    fn delta(&self, rhs: &Self) -> Self {
        // saturating_sub: a transient counter-read failure upstream can
        // make a "cumulative" value regress; clamping beats wrapping.
        let mut d = Self {
            nr_sync_local: self.nr_sync_local.saturating_sub(rhs.nr_sync_local),
            nr_prev_idle: self.nr_prev_idle.saturating_sub(rhs.nr_prev_idle),
            nr_idle_core: self.nr_idle_core.saturating_sub(rhs.nr_idle_core),
            nr_idle_smt: self.nr_idle_smt.saturating_sub(rhs.nr_idle_smt),
            nr_home_miss_clamp: self
                .nr_home_miss_clamp
                .saturating_sub(rhs.nr_home_miss_clamp),
            nr_enq_home: self.nr_enq_home.saturating_sub(rhs.nr_enq_home),
            nr_enq_spill: self.nr_enq_spill.saturating_sub(rhs.nr_enq_spill),
            nr_steals: self.nr_steals.saturating_sub(rhs.nr_steals),
            nr_tctx_errors: self.nr_tctx_errors.saturating_sub(rhs.nr_tctx_errors),
            nr_exec_severs: self.nr_exec_severs.saturating_sub(rhs.nr_exec_severs),
            nr_migrations: self.nr_migrations.saturating_sub(rhs.nr_migrations),
            nr_merges: self.nr_merges.saturating_sub(rhs.nr_merges),
            nr_splits: self.nr_splits.saturating_sub(rhs.nr_splits),
            ..self.clone()
        };
        // Affinity over THIS interval, not since attach — a cumulative
        // percentage would bury fresh regressions under old history.
        d.affinity_hit_pct = affinity_pct(
            d.nr_sync_local + d.nr_prev_idle + d.nr_idle_core + d.nr_idle_smt,
            d.nr_enq_home,
            d.nr_enq_spill,
            d.nr_steals,
        );
        d.plan_hit_pct = plan_hit_pct(
            d.nr_sync_local + d.nr_prev_idle + d.nr_idle_core + d.nr_idle_smt,
            d.nr_enq_home,
            d.nr_enq_spill,
            d.nr_steals,
        );
        d
    }
}

/// The affinity hit rate: placements that ended up on the home CCD over
/// all placements. A stolen task was counted at enqueue as landing home
/// but actually ran away, so steals move from the numerator to the
/// denominator's "away" side rather than being double-counted.
pub fn affinity_pct(direct: u64, enq_home: u64, enq_spill: u64, steals: u64) -> f64 {
    let home = (direct + enq_home).saturating_sub(steals);
    let total = direct + enq_home + enq_spill;
    if total > 0 {
        home as f64 * 100.0 / total as f64
    } else {
        100.0
    }
}

/// Placement-plan adherence: home *and* spill-target placements both
/// count as hits — spilling is deliberate policy, not a miss — so this
/// stays diagnostic for oversized cohorts, where `affinity_pct` is
/// structurally capped by the share of members the plan itself runs
/// remotely. Steals are the only misses: work that ran somewhere no
/// plan put it.
pub fn plan_hit_pct(direct: u64, enq_home: u64, enq_spill: u64, steals: u64) -> f64 {
    let total = direct + enq_home + enq_spill;
    let on_plan = total.saturating_sub(steals);
    if total > 0 {
        on_plan as f64 * 100.0 / total as f64
    } else {
        100.0
    }
}

/// One process (thread group) in the procs view.
#[stat_doc]
#[derive(Clone, Debug, Default, Serialize, Deserialize, Stats)]
#[stat(_om_prefix = "pr_", _om_label = "process")]
pub struct ProcRow {
    #[stat(desc = "thread group id")]
    pub tgid: u64,
    #[stat(desc = "process comm")]
    pub comm: String,
    #[stat(desc = "live threads")]
    pub threads: u64,
    #[stat(desc = "threads currently spilled to the other CCD")]
    pub spilled: u64,
    #[stat(desc = "CPU utilization, % of one CPU")]
    pub util_pct: f64,
    #[stat(desc = "% of runtime executed on the home CCD")]
    pub affinity_pct: f64,
}

/// One cohort with its member processes.
#[stat_doc]
#[derive(Clone, Debug, Default, Serialize, Deserialize, Stats)]
#[stat(_om_prefix = "c_", _om_label = "cohort")]
pub struct CohortRow {
    #[stat(desc = "home CCD (LLC id)")]
    pub home_llc: u64,
    #[stat(desc = "pinned by a rule (1/0)")]
    pub pinned: u64,
    #[stat(desc = "live tasks")]
    pub tasks: u64,
    #[stat(desc = "CPU utilization, % of one CPU")]
    pub util_pct: f64,
    #[stat(desc = "% of member runtime executed on the home CCD")]
    pub affinity_pct: f64,
    #[stat(desc = "member processes, keyed by tgid")]
    pub procs: BTreeMap<usize, ProcRow>,
}

/// The `scx_cohort top` payload: everything per-cohort and per-process.
#[stat_doc]
#[derive(Clone, Debug, Default, Serialize, Deserialize, Stats)]
#[stat(top)]
pub struct ProcsSnapshot {
    #[stat(desc = "live tasks under the scheduler")]
    pub nr_tasks: u64,
    #[stat(desc = "global % of runtime executed on home CCDs")]
    pub affinity_pct: f64,
    #[stat(desc = "per-LLC utilization, % of LLC capacity")]
    pub llc_util_pct: BTreeMap<usize, f64>,
    #[stat(desc = "cohorts, keyed by cohort id")]
    pub cohorts: BTreeMap<usize, CohortRow>,
}

/// Pure assembly of the procs view from one tick's snapshots; unit-tested
/// without a kernel.
pub fn procs_snapshot(
    cohorts: &[crate::domain::CohortSnapshot],
    tasks: &[crate::domain::TaskSnapshot],
    llcs: &[crate::domain::LlcLoad],
    spills: &std::collections::HashMap<u32, u32>,
    tick: Duration,
) -> ProcsSnapshot {
    let tick_ns = tick.as_nanos().max(1) as u64;
    let pct = |num: u64, den: u64| {
        if den > 0 {
            num as f64 * 100.0 / den as f64
        } else {
            100.0
        }
    };

    let mut out = ProcsSnapshot {
        nr_tasks: tasks.len() as u64,
        ..Default::default()
    };

    for llc in llcs {
        out.llc_util_pct.insert(
            llc.llc as usize,
            llc.load_ns as f64 * 100.0 / llc.capacity_ns.max(1) as f64,
        );
    }

    let (total_duty, total_home) = tasks
        .iter()
        .fold((0u64, 0u64), |(d, h), t| (d + t.duty_ns, h + t.home_ns));
    out.affinity_pct = pct(total_home, total_duty);

    for c in cohorts {
        let mut row = CohortRow {
            home_llc: c.home_llc as u64,
            pinned: c.pinned as u64,
            ..Default::default()
        };
        let mut cohort_duty = 0u64;
        let mut cohort_home = 0u64;

        for t in tasks.iter().filter(|t| t.cohort_id == c.id) {
            cohort_duty += t.duty_ns;
            cohort_home += t.home_ns;
            row.tasks += 1;
            let proc_row = row.procs.entry(t.tgid as usize).or_insert_with(|| ProcRow {
                tgid: t.tgid as u64,
                comm: t.comm.clone(),
                ..Default::default()
            });
            proc_row.threads += 1;
            proc_row.spilled += spills.contains_key(&t.pid) as u64;
            proc_row.util_pct += t.duty_ns as f64 * 100.0 / tick_ns as f64;
            // Accumulate ns; converted to a percentage below.
            proc_row.affinity_pct += t.home_ns as f64;
        }
        for proc_row in row.procs.values_mut() {
            let duty_ns = proc_row.util_pct / 100.0 * tick_ns as f64;
            proc_row.affinity_pct = if duty_ns > 0.0 {
                (proc_row.affinity_pct / duty_ns * 100.0).min(100.0)
            } else {
                100.0
            };
        }
        row.util_pct = cohort_duty as f64 * 100.0 / tick_ns as f64;
        row.affinity_pct = pct(cohort_home, cohort_duty);
        out.cohorts.insert(c.id as usize, row);
    }
    out
}

/// Requests the scheduler loop answers over the stats channels.
#[derive(Debug)]
pub enum StatsReq {
    Metrics,
    Procs,
}

#[derive(Debug)]
pub enum StatsRes {
    Metrics(Metrics),
    Procs(ProcsSnapshot),
}

pub fn server_data() -> StatsServerData<StatsReq, StatsRes> {
    let open: Box<dyn StatsOpener<StatsReq, StatsRes>> = Box::new(move |(req_ch, res_ch)| {
        req_ch.send(StatsReq::Metrics)?;
        let StatsRes::Metrics(mut prev) = res_ch.recv()? else {
            anyhow::bail!("unexpected stats response");
        };

        let read: Box<dyn StatsReader<StatsReq, StatsRes>> =
            Box::new(move |_args, (req_ch, res_ch)| {
                req_ch.send(StatsReq::Metrics)?;
                let StatsRes::Metrics(cur) = res_ch.recv()? else {
                    anyhow::bail!("unexpected stats response");
                };
                let delta = cur.delta(&prev);
                prev = cur;
                delta.to_json()
            });

        Ok(read)
    });

    let procs_open: Box<dyn StatsOpener<StatsReq, StatsRes>> =
        Box::new(move |(_req_ch, _res_ch)| {
            let read: Box<dyn StatsReader<StatsReq, StatsRes>> =
                Box::new(move |_args, (req_ch, res_ch)| {
                    req_ch.send(StatsReq::Procs)?;
                    let StatsRes::Procs(snapshot) = res_ch.recv()? else {
                        anyhow::bail!("unexpected stats response");
                    };
                    snapshot.to_json()
                });
            Ok(read)
        });

    StatsServerData::new()
        .add_meta(LlcMetrics::meta())
        .add_meta(Metrics::meta())
        .add_meta(ProcRow::meta())
        .add_meta(CohortRow::meta())
        .add_meta(ProcsSnapshot::meta())
        .add_ops("top", StatsOps { open, close: None })
        .add_ops(
            "procs",
            StatsOps {
                open: procs_open,
                close: None,
            },
        )
}

pub fn monitor(intv: Duration) -> Result<()> {
    scx_utils::monitor_stats::<Metrics>(
        &[],
        intv,
        || crate::SHUTDOWN.load(Ordering::Relaxed),
        |metrics| metrics.format(&mut std::io::stdout()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CohortSnapshot, LlcLoad, TaskSnapshot};
    use std::collections::HashMap;

    #[test]
    fn plan_hits_count_spills_where_affinity_does_not() {
        // 40 direct + 40 home enqueues, 40 spill enqueues (an oversized
        // cohort running a third of its placements remotely by plan),
        // 12 steals.
        let (direct, home, spill, steals) = (40, 40, 40, 12);
        // Affinity charges the spilled share against the cohort even
        // though it is deliberate: (40+40-12)/120.
        assert!((affinity_pct(direct, home, spill, steals) - 56.666).abs() < 0.01);
        // Plan adherence treats only steals as misses: (120-12)/120.
        assert!((plan_hit_pct(direct, home, spill, steals) - 90.0).abs() < 0.01);
        // With no spills the two metrics agree.
        assert_eq!(
            affinity_pct(direct, home, 0, steals),
            plan_hit_pct(direct, home, 0, steals)
        );
        assert_eq!(plan_hit_pct(0, 0, 0, 0), 100.0);
    }

    #[test]
    fn procs_snapshot_aggregates_threads_and_affinity() {
        let tick = Duration::from_millis(200);
        let tick_ns = 200_000_000u64;
        let cohorts = [CohortSnapshot {
            id: 7,
            home_llc: 1,
            pinned: true,
            nr_tasks: 3,
            load_ns: tick_ns,
            residency_ms: None,
        }];
        // Two threads of tgid 100 (one fully home, one half home) and one
        // of tgid 200, fully away.
        let t = |pid, tgid, duty, home, comm: &str| TaskSnapshot {
            pid,
            tgid,
            cohort_id: 7,
            duty_ns: duty,
            duty_known: true,
            home_ns: home,
            comm: comm.into(),
        };
        let tasks = [
            t(101, 100, tick_ns / 2, tick_ns / 2, "game"),
            t(102, 100, tick_ns / 2, tick_ns / 4, "game"),
            t(201, 200, tick_ns / 4, 0, "helper"),
        ];
        let llcs = [
            LlcLoad {
                llc: 0,
                load_ns: 0,
                capacity_ns: tick_ns * 8,
            },
            LlcLoad {
                llc: 1,
                load_ns: tick_ns,
                capacity_ns: tick_ns * 8,
            },
        ];
        let spills: HashMap<u32, u32> = [(201, 0)].into();

        let s = procs_snapshot(&cohorts, &tasks, &llcs, &spills, tick);
        assert_eq!(s.nr_tasks, 3);
        let c = &s.cohorts[&7];
        assert_eq!(c.home_llc, 1);
        assert_eq!(c.pinned, 1);
        assert_eq!(c.tasks, 3);
        // Cohort: 1.25 CPUs of demand → 125%.
        assert!((c.util_pct - 125.0).abs() < 0.1);
        // Cohort affinity: (0.5 + 0.25 + 0) / 1.25 = 60%.
        assert!((c.affinity_pct - 60.0).abs() < 0.1);

        let game = &c.procs[&100];
        assert_eq!(game.threads, 2);
        assert_eq!(game.spilled, 0);
        assert!((game.util_pct - 100.0).abs() < 0.1);
        assert!((game.affinity_pct - 75.0).abs() < 0.1);

        let helper = &c.procs[&200];
        assert_eq!(helper.spilled, 1);
        assert!((helper.affinity_pct - 0.0).abs() < 0.1);
        // LLC 1 runs 1 tick of work on 8 CPUs → 12.5%.
        assert!((s.llc_util_pct[&1] - 12.5).abs() < 0.1);
    }
}
