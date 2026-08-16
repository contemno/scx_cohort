// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! scx_stats metrics: served over the standard scx stats socket (readable
//! with `scxtop` or `scx_cohort --monitor`).

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
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

    #[stat(desc = "cohort home moves by the load balancer")]
    pub nr_migrations: u64,
    #[stat(desc = "cohort merges from wake-edge discovery")]
    pub nr_merges: u64,
    #[stat(desc = "cohort splits from wake-edge discovery")]
    pub nr_splits: u64,
    #[stat(desc = "per-LLC statistics")]
    pub llcs: BTreeMap<usize, LlcMetrics>,
}

impl Metrics {
    pub fn format<W: Write>(&self, w: &mut W) -> Result<()> {
        writeln!(
            w,
            "[scx_cohort] cohorts={:<4} tasks={:<5} affinity={:5.1}% moves={} merges={} splits={}",
            self.nr_cohorts,
            self.nr_tasks,
            self.affinity_hit_pct,
            self.nr_migrations,
            self.nr_merges,
            self.nr_splits,
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
        Self {
            nr_sync_local: self.nr_sync_local - rhs.nr_sync_local,
            nr_prev_idle: self.nr_prev_idle - rhs.nr_prev_idle,
            nr_idle_core: self.nr_idle_core - rhs.nr_idle_core,
            nr_idle_smt: self.nr_idle_smt - rhs.nr_idle_smt,
            nr_home_miss_clamp: self.nr_home_miss_clamp - rhs.nr_home_miss_clamp,
            nr_enq_home: self.nr_enq_home - rhs.nr_enq_home,
            nr_enq_spill: self.nr_enq_spill - rhs.nr_enq_spill,
            nr_steals: self.nr_steals - rhs.nr_steals,
            nr_tctx_errors: self.nr_tctx_errors - rhs.nr_tctx_errors,
            nr_migrations: self.nr_migrations - rhs.nr_migrations,
            nr_merges: self.nr_merges - rhs.nr_merges,
            nr_splits: self.nr_splits - rhs.nr_splits,
            ..self.clone()
        }
    }
}

pub fn server_data() -> StatsServerData<(), Metrics> {
    let open: Box<dyn StatsOpener<(), Metrics>> = Box::new(move |(req_ch, res_ch)| {
        req_ch.send(())?;
        let mut prev = res_ch.recv()?;

        let read: Box<dyn StatsReader<(), Metrics>> = Box::new(move |_args, (req_ch, res_ch)| {
            req_ch.send(())?;
            let cur = res_ch.recv()?;
            let delta = cur.delta(&prev);
            prev = cur;
            delta.to_json()
        });

        Ok(read)
    });

    StatsServerData::new()
        .add_meta(LlcMetrics::meta())
        .add_meta(Metrics::meta())
        .add_ops("top", StatsOps { open, close: None })
}

pub fn monitor(intv: Duration, shutdown: Arc<AtomicBool>) -> Result<()> {
    scx_utils::monitor_stats::<Metrics>(
        &[],
        intv,
        || shutdown.load(Ordering::Relaxed),
        |metrics| metrics.format(&mut std::io::stdout()),
    )
}
