// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! scx_cohort userspace daemon: loads the BPF component, discovers the CPU
//! topology, and runs the slow-thinking side of the scheduler — cohort
//! bookkeeping, the load balancer, and the metrics server. All policy
//! logic lives in the pure `balancer` (and later `wake_graph`/`config`)
//! modules; this file only shuttles bytes between BPF maps and those
//! planners.

mod balancer;
mod bpf_skel;
mod domain;
mod stats;

pub use bpf_skel::*;

use std::collections::{BTreeMap, HashMap};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use crossbeam::channel::RecvTimeoutError;
use libbpf_rs::{MapCore, MapFlags, OpenObject};
use log::{debug, info, warn};
use scx_stats::prelude::*;
use scx_utils::scx_ops_attach;
use scx_utils::scx_ops_load;
use scx_utils::scx_ops_open;
use scx_utils::uei_exited;
use scx_utils::uei_report;
use scx_utils::Topology;
use scx_utils::UserExitInfo;
use zerocopy::{FromBytes, IntoBytes};

use balancer::{Balancer, BalancerCfg};
use domain::{CohortSnapshot, Decision, LlcLoad};
use scx_cohort_common::{CohortCtx, Tunables, COHORT_PINNED, MAX_CPUS, MAX_LLCS, NR_STATS};

/// scx_cohort: A CCD-affine sched_ext scheduler.
///
/// Discovers groups of related tasks ("cohorts"), gives each cohort a home
/// CCD, schedules within that CCD by default, and crosses the fabric only
/// when the math clearly favors it.
#[derive(Debug, Parser)]
#[command(version)]
struct Opts {
    /// Scheduling slice duration in microseconds.
    #[clap(long, default_value = "5000")]
    slice_us: u64,

    /// Daemon tick interval in milliseconds.
    #[clap(long, default_value = "200")]
    interval_ms: u64,

    /// Steal from a foreign CCD only when its queue holds more than this
    /// many tasks...
    #[clap(long, default_value = "2")]
    steal_min: u64,

    /// ...or its head task has waited longer than this many microseconds.
    #[clap(long, default_value = "500")]
    steal_delay_us: u64,

    /// Move a cohort when the inter-CCD load gap exceeds this percentage
    /// of one CCD's capacity for two consecutive ticks.
    #[clap(long, default_value = "20")]
    imbalance_pct: u64,

    /// A cohort that just moved is immune from moving again for this many
    /// milliseconds.
    #[clap(long, default_value = "2000")]
    residency_ms: u64,

    /// Enable stats monitoring with the specified interval in seconds.
    #[clap(long)]
    stats: Option<f64>,

    /// Run in stats monitoring mode with the specified interval in
    /// seconds; a scheduler instance must be running.
    #[clap(long)]
    monitor: Option<f64>,

    /// Increase verbosity (-v: debug, -vv: trace).
    #[clap(short = 'v', long, action = clap::ArgAction::Count)]
    verbose: u8,
}

impl Opts {
    fn tunables(&self) -> Tunables {
        Tunables {
            steal_min: self.steal_min,
            steal_delay_ns: self.steal_delay_us * 1000,
            ..Tunables::default()
        }
    }
}

fn log_topology(topo: &Topology) {
    info!(
        "topology: {} node(s), {} LLC(s), {} core(s), {} CPU(s), SMT {}",
        topo.nodes.len(),
        topo.all_llcs.len(),
        topo.all_cores.len(),
        topo.all_cpus.len(),
        if topo.smt_enabled { "on" } else { "off" }
    );
    for node in topo.nodes.values() {
        for llc in node.llcs.values() {
            info!(
                "  node {} llc {}: {} cores, {} cpus [{}]",
                node.id,
                llc.id,
                llc.cores.len(),
                llc.all_cpus.len(),
                llc.all_cpus
                    .keys()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
    }
}

struct Scheduler<'a> {
    skel: BpfSkel<'a>,
    // Dropping the link detaches the scheduler; hold it for our lifetime.
    _struct_ops: libbpf_rs::Link,
    interval: Duration,
    started_at: Instant,
    balancer: Balancer,
    /// LLC id → CPU count, for capacity computation.
    llc_cpus: BTreeMap<u32, u64>,
    /// cohort id → load_sum at the previous tick, for per-tick deltas.
    prev_load_sum: HashMap<u64, u64>,
    nr_migrations: u64,
    stats_server: StatsServer<(), stats::Metrics>,
}

impl<'a> Scheduler<'a> {
    fn init(opts: &Opts, open_object: &'a mut MaybeUninit<OpenObject>) -> Result<Self> {
        let topo = Topology::new().context("failed to discover topology")?;
        log_topology(&topo);
        if topo.nodes.len() > 1 {
            bail!("multi-node (NUMA) systems are out of scope for scx_cohort v1");
        }

        let mut skel_builder = BpfSkelBuilder::default();
        skel_builder.obj_builder.debug(opts.verbose > 1);
        let open_opts: Option<libbpf_rs::libbpf_sys::bpf_object_open_opts> = None;
        let mut skel = scx_ops_open!(skel_builder, open_object, cohort_ops, open_opts)?;

        if topo.all_llcs.len() > MAX_LLCS as usize {
            bail!(
                "{} LLCs exceeds MAX_LLCS ({})",
                topo.all_llcs.len(),
                MAX_LLCS
            );
        }
        if topo.all_llcs.len() < 2 {
            info!("single LLC detected; cohort placement degrades to plain vtime scheduling");
        }

        {
            let rodata = skel.maps.rodata_data.as_mut().unwrap();
            rodata.slice_ns = opts.slice_us * 1000;
            rodata.nr_llcs = topo.all_llcs.len() as u32;
            rodata.nr_cpus = *topo.all_cpus.keys().max().unwrap_or(&0) as u32 + 1;
            if rodata.nr_cpus > MAX_CPUS {
                bail!(
                    "CPU id range {} exceeds MAX_CPUS ({})",
                    rodata.nr_cpus,
                    MAX_CPUS
                );
            }
            for cpu in topo.all_cpus.values() {
                rodata.cpu_llc_id[cpu.id] = cpu.llc_id as u32;
            }
        }

        {
            // The skeleton's Tunables type is generated from BPF BTF, which
            // itself came from the shared crate via intf.h; field-by-field
            // assignment keeps the compiler checking that round trip.
            let t = opts.tunables();
            let data = skel.maps.data_data.as_mut().unwrap();
            data.tunables.steal_min = t.steal_min;
            data.tunables.steal_delay_ns = t.steal_delay_ns;
            data.tunables.credit_max_ns = t.credit_max_ns;
            data.tunables.credit_wake_freq_min = t.credit_wake_freq_min;
            data.tunables.credit_runtime_max_ns = t.credit_runtime_max_ns;
            data.tunables.sample_shift = t.sample_shift;
        }

        let llc_cpus: BTreeMap<u32, u64> = topo
            .all_llcs
            .values()
            .map(|llc| (llc.id as u32, llc.all_cpus.len() as u64))
            .collect();

        let mut skel = scx_ops_load!(skel, cohort_ops, uei)?;
        let struct_ops = scx_ops_attach!(skel, cohort_ops)?;
        let stats_server = StatsServer::new(stats::server_data()).launch()?;
        info!("scx_cohort attached");

        Ok(Self {
            skel,
            _struct_ops: struct_ops,
            interval: Duration::from_millis(opts.interval_ms),
            started_at: Instant::now(),
            balancer: Balancer::new(BalancerCfg {
                imbalance_pct: opts.imbalance_pct,
                confirm_ticks: 2,
                residency_ms: opts.residency_ms,
            }),
            llc_cpus,
            prev_load_sum: HashMap::new(),
            nr_migrations: 0,
            stats_server,
        })
    }

    fn now_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Snapshot the cohort map, computing per-tick load deltas, and
    /// garbage-collect empty cohorts along the way.
    fn snapshot_cohorts(&mut self) -> Result<Vec<CohortSnapshot>> {
        let cohorts_map = &self.skel.maps.cohorts;
        let mut out = Vec::new();
        let mut dead = Vec::new();
        let mut seen = HashMap::new();

        for key in cohorts_map.keys() {
            let Some(val) = cohorts_map.lookup(&key, MapFlags::ANY)? else {
                continue;
            };
            let Ok(ctx) = CohortCtx::read_from_bytes(val.as_slice()) else {
                warn!("cohort value size mismatch: {}", val.len());
                continue;
            };
            let id = u64::from_ne_bytes(key.as_slice().try_into()?);

            if ctx.nr_tasks == 0 {
                dead.push(key.clone());
                continue;
            }

            let prev = self.prev_load_sum.get(&id).copied().unwrap_or(ctx.load_sum);
            seen.insert(id, ctx.load_sum);
            out.push(CohortSnapshot {
                id,
                home_llc: ctx.home_llc,
                pinned: ctx.flags & COHORT_PINNED != 0,
                nr_tasks: ctx.nr_tasks,
                load_ns: ctx.load_sum.saturating_sub(prev),
            });
        }

        for key in &dead {
            let _ = cohorts_map.delete(key);
        }
        self.gc_tgid_map()?;

        self.prev_load_sum = seen;
        let live: std::collections::HashSet<u64> = out.iter().map(|c| c.id).collect();
        self.balancer.retain_cohorts(|id| live.contains(&id));
        Ok(out)
    }

    /// Drop tgid→cohort entries whose cohort no longer exists, so a
    /// recycled tgid can't resurrect a dead cohort.
    fn gc_tgid_map(&self) -> Result<()> {
        let tgid_map = &self.skel.maps.tgid_cohort;
        let cohorts_map = &self.skel.maps.cohorts;
        let mut stale = Vec::new();

        for key in tgid_map.keys() {
            let Some(val) = tgid_map.lookup(&key, MapFlags::ANY)? else {
                continue;
            };
            let cohort_key: [u8; 8] = val.as_slice().try_into()?;
            if cohorts_map.lookup(&cohort_key, MapFlags::ANY)?.is_none() {
                stale.push(key);
            }
        }
        for key in &stale {
            let _ = tgid_map.delete(key);
        }
        Ok(())
    }

    fn llc_loads(&self, cohorts: &[CohortSnapshot]) -> Vec<LlcLoad> {
        let tick_ns = self.interval.as_nanos() as u64;
        self.llc_cpus
            .iter()
            .map(|(&llc, &cpus)| LlcLoad {
                llc,
                load_ns: cohorts
                    .iter()
                    .filter(|c| c.home_llc == llc)
                    .map(|c| c.load_ns)
                    .sum(),
                capacity_ns: cpus * tick_ns,
            })
            .collect()
    }

    fn apply(&mut self, decisions: Vec<Decision>) -> Result<()> {
        let cohorts_map = &self.skel.maps.cohorts;
        for decision in decisions {
            match decision {
                Decision::MoveCohort { id, to } => {
                    let key = id.to_ne_bytes();
                    let Some(val) = cohorts_map.lookup(&key, MapFlags::ANY)? else {
                        continue;
                    };
                    let Ok(mut ctx) = CohortCtx::read_from_bytes(val.as_slice()) else {
                        continue;
                    };
                    debug!("moving cohort {} from LLC {} to {}", id, ctx.home_llc, to);
                    ctx.home_llc = to;
                    cohorts_map.update(&key, ctx.as_bytes(), MapFlags::EXIST)?;
                    self.nr_migrations += 1;
                }
            }
        }
        Ok(())
    }

    fn read_stat_counters(&self) -> Result<[u64; NR_STATS as usize]> {
        let map = &self.skel.maps.stats;
        let mut out = [0u64; NR_STATS as usize];
        for (idx, slot) in out.iter_mut().enumerate() {
            let key = (idx as u32).to_ne_bytes();
            if let Some(percpu) = map.lookup_percpu(&key, MapFlags::ANY)? {
                *slot = percpu
                    .iter()
                    .map(|v| u64::from_ne_bytes(v.as_slice().try_into().unwrap_or([0; 8])))
                    .sum();
            }
        }
        Ok(out)
    }

    fn get_metrics(&self, cohorts: &[CohortSnapshot], llcs: &[LlcLoad]) -> stats::Metrics {
        use scx_cohort_common::*;

        let c = self.read_stat_counters().unwrap_or_default();
        let direct = c[STAT_SYNC_LOCAL as usize]
            + c[STAT_PREV_IDLE as usize]
            + c[STAT_IDLE_CORE as usize]
            + c[STAT_IDLE_SMT as usize];
        let home = direct + c[STAT_ENQ_HOME as usize];
        let away = c[STAT_ENQ_SPILL as usize] + c[STAT_STEAL as usize];
        let total = home + away;

        stats::Metrics {
            nr_cohorts: cohorts.len() as u64,
            nr_tasks: cohorts.iter().map(|c| c.nr_tasks).sum(),
            affinity_hit_pct: if total > 0 {
                home as f64 * 100.0 / total as f64
            } else {
                100.0
            },
            nr_sync_local: c[STAT_SYNC_LOCAL as usize],
            nr_prev_idle: c[STAT_PREV_IDLE as usize],
            nr_idle_core: c[STAT_IDLE_CORE as usize],
            nr_idle_smt: c[STAT_IDLE_SMT as usize],
            nr_home_miss_clamp: c[STAT_HOME_MISS_CLAMP as usize],
            nr_enq_home: c[STAT_ENQ_HOME as usize],
            nr_enq_spill: c[STAT_ENQ_SPILL as usize],
            nr_steals: c[STAT_STEAL as usize],
            nr_tctx_errors: c[STAT_TCTX_ERR as usize],
            nr_migrations: self.nr_migrations,
            llcs: llcs
                .iter()
                .map(|l| {
                    (
                        l.llc as usize,
                        stats::LlcMetrics {
                            util: l.load_ns as f64 / l.capacity_ns.max(1) as f64,
                            nr_cohorts: cohorts.iter().filter(|c| c.home_llc == l.llc).count()
                                as u64,
                        },
                    )
                })
                .collect(),
        }
    }

    fn tick(&mut self) -> Result<(Vec<CohortSnapshot>, Vec<LlcLoad>)> {
        let cohorts = self.snapshot_cohorts()?;
        let llcs = self.llc_loads(&cohorts);
        let decisions = self.balancer.plan(self.now_ms(), &llcs, &cohorts);
        self.apply(decisions)?;
        Ok((cohorts, llcs))
    }

    fn run(&mut self, shutdown: Arc<AtomicBool>) -> Result<UserExitInfo> {
        let (res_ch, req_ch) = self.stats_server.channels();
        let mut next_tick = Instant::now() + self.interval;
        let mut last_view = (Vec::new(), Vec::new());

        while !shutdown.load(Ordering::Relaxed) && !uei_exited!(&self.skel, uei) {
            let timeout = next_tick.saturating_duration_since(Instant::now());
            match req_ch.recv_timeout(timeout) {
                Ok(()) => {
                    let metrics = self.get_metrics(&last_view.0, &last_view.1);
                    res_ch.send(metrics)?;
                }
                Err(RecvTimeoutError::Timeout) => {
                    last_view = self.tick()?;
                    next_tick += self.interval;
                }
                Err(e) => Err(e)?,
            }
        }
        uei_report!(&self.skel, uei)
    }
}

fn main() -> Result<()> {
    let opts = Opts::parse();

    let level = match opts.verbose {
        0 => simplelog::LevelFilter::Info,
        1 => simplelog::LevelFilter::Debug,
        _ => simplelog::LevelFilter::Trace,
    };
    let mut lcfg = simplelog::ConfigBuilder::new();
    lcfg.set_time_level(simplelog::LevelFilter::Error)
        .set_location_level(simplelog::LevelFilter::Off)
        .set_target_level(simplelog::LevelFilter::Off)
        .set_thread_level(simplelog::LevelFilter::Off);
    simplelog::TermLogger::init(
        level,
        lcfg.build(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    )?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    ctrlc::set_handler(move || {
        shutdown_clone.store(true, Ordering::Relaxed);
    })
    .context("error setting Ctrl-C handler")?;

    if let Some(intv) = opts.monitor {
        return stats::monitor(Duration::from_secs_f64(intv), shutdown);
    }

    if let Some(intv) = opts.stats {
        let shutdown_copy = shutdown.clone();
        std::thread::spawn(move || {
            let _ = stats::monitor(Duration::from_secs_f64(intv), shutdown_copy);
        });
    }

    let mut open_object = MaybeUninit::uninit();
    let mut sched = Scheduler::init(&opts, &mut open_object)?;
    let uei = sched.run(shutdown)?;
    uei.report()?;
    Ok(())
}
