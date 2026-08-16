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
mod config;
mod domain;
mod stats;
mod wake_graph;

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

use balancer::{plan_spills, Balancer, BalancerCfg, SpillCfg};
use domain::{CohortSnapshot, Decision, LlcLoad, TaskSnapshot};
use scx_cohort_common::{
    CohortCounters, CohortPolicy, TaskStat, Tunables, WakeEdgeKey, COHORT_PINNED, MAX_CPUS,
    MAX_LLCS, NR_STATS,
};
use wake_graph::{WakeGraph, WakeGraphCfg};

/// Cohort ids the daemon allocates for split-off components. The BPF
/// allocator counts up from 1; starting daemon ids here keeps the two id
/// spaces collision-free.
const DAEMON_COHORT_BASE: u64 = 1 << 32;

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

    /// Merge two cohorts when their mutual wake rate sustains this many
    /// wakes per second.
    #[clap(long, default_value = "300")]
    merge_wakes_per_sec: f64,

    /// Path to a TOML rules file (see config.rs for the format).
    #[clap(long)]
    config: Option<std::path::PathBuf>,

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
    /// llc → executed load at the previous tick, for per-tick deltas.
    prev_llc_load: HashMap<u32, u64>,
    /// Cohorts observed dead (no tgid references, no live tasks) last
    /// tick; deleted only on the second consecutive observation, so the
    /// GC can't race the BPF side's create-then-populate in init_task.
    dead_cohorts: std::collections::HashSet<u64>,
    /// Same two-tick treatment for tgid_cohort entries with no live tasks.
    dead_tgids: std::collections::HashSet<u32>,
    wake_graph: WakeGraph,
    /// Raw (directed) edge counters at the previous tick.
    prev_edges: HashMap<(u32, u32), u64>,
    /// Allocator for split-off cohort ids (offset from DAEMON_COHORT_BASE).
    daemon_seq: u64,
    sample_mult: u64,
    config: config::Config,
    spill_cfg: SpillCfg,
    /// The spill set as last written to the BPF map: pid → target LLC.
    current_spills: HashMap<u32, u32>,
    /// pid → runtime_sum at the previous tick, for duty deltas.
    prev_runtime: HashMap<u32, u64>,
    /// tgid → cgroup path, lazily read from procfs for rule matching.
    cgroup_cache: HashMap<u32, String>,
    /// cohort → residency override from matched rules.
    residency_overrides: HashMap<u64, u64>,
    nr_migrations: u64,
    nr_merges: u64,
    nr_splits: u64,
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
            prev_llc_load: HashMap::new(),
            dead_cohorts: std::collections::HashSet::new(),
            dead_tgids: std::collections::HashSet::new(),
            wake_graph: WakeGraph::new(WakeGraphCfg {
                merge_wakes_per_sec: opts.merge_wakes_per_sec,
                ..WakeGraphCfg::default()
            }),
            prev_edges: HashMap::new(),
            daemon_seq: 0,
            sample_mult: 1 << opts.tunables().sample_shift,
            config: match &opts.config {
                Some(path) => {
                    let cfg = config::Config::load(path)?;
                    info!("loaded {} rule(s) from {:?}", cfg.rules.len(), path);
                    cfg
                }
                None => config::Config::default(),
            },
            spill_cfg: SpillCfg {
                tick_ns: opts.interval_ms * 1_000_000,
                ..SpillCfg::default()
            },
            current_spills: HashMap::new(),
            prev_runtime: HashMap::new(),
            cgroup_cache: HashMap::new(),
            residency_overrides: HashMap::new(),
            nr_migrations: 0,
            nr_merges: 0,
            nr_splits: 0,
            stats_server,
        })
    }

    fn now_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Snapshot cohort state from the counters and policy maps, deriving
    /// exact membership from the task snapshot, and garbage-collect dead
    /// cohorts (no tgid references, no live tasks — two ticks in a row).
    ///
    /// The daemon never writes an existing counters value back: counters
    /// are BPF-owned, policy is daemon-owned, so no read-modify-write can
    /// race the other side's atomics.
    fn snapshot_cohorts(
        &mut self,
        tasks: &[TaskSnapshot],
        tgid_cohort: &HashMap<u32, u64>,
    ) -> Result<Vec<CohortSnapshot>> {
        let counters_map = &self.skel.maps.cohort_counters;
        let policy_map = &self.skel.maps.cohort_policy;
        let mut out = Vec::new();
        let mut dead = Vec::new();
        let mut seen = HashMap::new();
        let mut dead_now = std::collections::HashSet::new();

        let mut task_counts: HashMap<u64, u64> = HashMap::new();
        for t in tasks {
            *task_counts.entry(t.cohort_id).or_default() += 1;
        }
        let referenced: std::collections::HashSet<u64> =
            tgid_cohort.values().copied().collect();

        for key in counters_map.keys() {
            let Some(val) = counters_map.lookup(&key, MapFlags::ANY)? else {
                continue;
            };
            let Ok(counters) = CohortCounters::read_from_bytes(val.as_slice()) else {
                warn!("cohort counters size mismatch: {}", val.len());
                continue;
            };
            let id = u64::from_ne_bytes(key.as_slice().try_into()?);
            let nr_tasks = task_counts.get(&id).copied().unwrap_or(0);

            if nr_tasks == 0 && !referenced.contains(&id) {
                if self.dead_cohorts.contains(&id) {
                    dead.push(id);
                } else {
                    dead_now.insert(id);
                }
                continue;
            }

            let policy = match policy_map.lookup(&key, MapFlags::ANY)? {
                Some(v) => CohortPolicy::read_from_bytes(v.as_slice()).unwrap_or_default(),
                None => CohortPolicy::default(),
            };

            let prev = self
                .prev_load_sum
                .get(&id)
                .copied()
                .unwrap_or(counters.load_sum);
            seen.insert(id, counters.load_sum);
            out.push(CohortSnapshot {
                id,
                home_llc: policy.home_llc,
                pinned: policy.flags & COHORT_PINNED != 0,
                nr_tasks,
                load_ns: counters.load_sum.saturating_sub(prev),
                residency_ms: self.residency_overrides.get(&id).copied(),
            });
        }

        for id in &dead {
            let key = id.to_ne_bytes();
            let _ = counters_map.delete(&key);
            let _ = policy_map.delete(&key);
        }
        self.dead_cohorts = dead_now;

        self.prev_load_sum = seen;
        let live: std::collections::HashSet<u64> = out.iter().map(|c| c.id).collect();
        self.balancer.retain_cohorts(|id| live.contains(&id));
        Ok(out)
    }

    /// Drop tgid→cohort entries whose tgid has no live tasks (two ticks
    /// in a row, so a freshly inserted mapping can't be swept before its
    /// first task_stats entry appears).
    fn gc_tgid_map(&mut self, tgid_cohort: &HashMap<u32, u64>, tasks: &[TaskSnapshot]) {
        let live_tgids: std::collections::HashSet<u32> = tasks.iter().map(|t| t.tgid).collect();
        let mut dead_now = std::collections::HashSet::new();
        let tgid_map = &self.skel.maps.tgid_cohort;

        for &tgid in tgid_cohort.keys() {
            if live_tgids.contains(&tgid) {
                continue;
            }
            if self.dead_tgids.contains(&tgid) {
                let _ = tgid_map.delete(&tgid.to_ne_bytes());
            } else {
                dead_now.insert(tgid);
            }
        }
        self.dead_tgids = dead_now;
    }

    /// Per-tick deltas of the sampled wake-edge counters. LRU eviction can
    /// reset a counter; a decrease is treated as a fresh count.
    fn snapshot_edges(&mut self) -> Result<Vec<(u32, u32, u64)>> {
        let map = &self.skel.maps.wake_edges;
        let mut cur = HashMap::new();
        let mut deltas = Vec::new();

        for key in map.keys() {
            let Some(val) = map.lookup(&key, MapFlags::ANY)? else {
                continue;
            };
            let Ok(edge) = WakeEdgeKey::read_from_bytes(key.as_slice()) else {
                continue;
            };
            let count = u64::from_ne_bytes(val.as_slice().try_into()?);
            let k = (edge.waker_tgid, edge.wakee_tgid);
            let prev = self.prev_edges.get(&k).copied().unwrap_or(0);
            let delta = if count >= prev { count - prev } else { count };
            if delta > 0 {
                deltas.push((k.0, k.1, delta));
            }
            cur.insert(k, count);
        }
        self.prev_edges = cur;
        Ok(deltas)
    }

    /// tgid → cohort straight from the map the BPF side resolves through.
    fn snapshot_tgid_cohort(&self) -> Result<HashMap<u32, u64>> {
        let map = &self.skel.maps.tgid_cohort;
        let mut out = HashMap::new();
        for key in map.keys() {
            let Some(val) = map.lookup(&key, MapFlags::ANY)? else {
                continue;
            };
            out.insert(
                u32::from_ne_bytes(key.as_slice().try_into()?),
                u64::from_ne_bytes(val.as_slice().try_into()?),
            );
        }
        Ok(out)
    }

    /// Snapshot the pid-keyed task_stats map, computing per-tick duty
    /// deltas from the monotonic runtime sums.
    fn snapshot_tasks(&mut self) -> Result<Vec<TaskSnapshot>> {
        let map = &self.skel.maps.task_stats;
        let mut out = Vec::new();
        let mut seen = HashMap::new();

        for key in map.keys() {
            let Some(val) = map.lookup(&key, MapFlags::ANY)? else {
                continue;
            };
            let Ok(ts) = TaskStat::read_from_bytes(val.as_slice()) else {
                continue;
            };
            let pid = u32::from_ne_bytes(key.as_slice().try_into()?);
            let prev = self.prev_runtime.get(&pid).copied().unwrap_or(ts.runtime_sum);
            seen.insert(pid, ts.runtime_sum);

            let comm_len = ts.comm.iter().position(|&b| b == 0).unwrap_or(ts.comm.len());
            out.push(TaskSnapshot {
                pid,
                tgid: ts.tgid,
                cohort_id: ts.cohort_id,
                duty_ns: ts.runtime_sum.saturating_sub(prev),
                comm: String::from_utf8_lossy(&ts.comm[..comm_len]).into_owned(),
            });
        }
        self.prev_runtime = seen;
        Ok(out)
    }

    fn cgroup_of(&mut self, tgid: u32) -> String {
        if let Some(path) = self.cgroup_cache.get(&tgid) {
            return path.clone();
        }
        // Use the cgroup v2 line ("0::/user.slice/...") specifically: on
        // hybrid v1+v2 hosts the v1 controller lines come first and would
        // otherwise win. Strip the leading slash so globs like
        // "user.slice/*" match naturally.
        let path = std::fs::read_to_string(format!("/proc/{tgid}/cgroup"))
            .ok()
            .and_then(|s| {
                s.lines().find_map(|l| {
                    l.strip_prefix("0::")
                        .map(|p| p.trim_start_matches('/').to_string())
                })
            })
            .unwrap_or_default();
        self.cgroup_cache.insert(tgid, path.clone());
        path
    }

    /// Apply TOML rules: pin matched cohorts and collect per-cohort
    /// residency overrides for the balancer.
    fn apply_rules(&mut self, tasks: &[TaskSnapshot]) -> Result<()> {
        if self.config.rules.is_empty() {
            return Ok(());
        }

        let live_tgids: std::collections::HashSet<u32> = tasks.iter().map(|t| t.tgid).collect();
        self.cgroup_cache.retain(|tgid, _| live_tgids.contains(tgid));

        let mut overrides = HashMap::new();
        let mut pins: HashMap<u64, u32> = HashMap::new();
        for t in tasks {
            let cgroup = self.cgroup_of(t.tgid);
            // Copy the matched rule's effects out immediately so the
            // borrow of self.config doesn't outlive this iteration.
            let Some((pin, residency)) = self
                .config
                .match_task(&t.comm, &cgroup)
                .map(|r| (r.pin_ccd, r.min_ccd_residency_ms))
            else {
                continue;
            };
            if let Some(ms) = residency {
                let e = overrides.entry(t.cohort_id).or_insert(ms);
                *e = (*e).max(ms);
            }
            if let Some(ccd) = pin {
                pins.insert(t.cohort_id, ccd);
            }
        }
        self.residency_overrides = overrides;

        let policy_map = &self.skel.maps.cohort_policy;
        for (cohort, ccd) in pins {
            let key = cohort.to_ne_bytes();
            let Some(val) = policy_map.lookup(&key, MapFlags::ANY)? else {
                continue;
            };
            let Ok(mut policy) = CohortPolicy::read_from_bytes(val.as_slice()) else {
                continue;
            };
            let want_home = ccd.min(self.llc_cpus.len() as u32 - 1);
            if policy.flags & COHORT_PINNED == 0 || policy.home_llc != want_home {
                debug!("pinning cohort {} to LLC {}", cohort, want_home);
                policy.flags |= COHORT_PINNED;
                policy.home_llc = want_home;
                policy_map.update(&key, policy.as_bytes(), MapFlags::EXIST)?;
            }
        }
        Ok(())
    }

    /// Sync the desired spill set into the BPF map.
    fn apply_spills(&mut self, desired: HashMap<u32, u32>) -> Result<()> {
        let map = &self.skel.maps.spill_tasks;
        for (pid, llc) in &desired {
            if self.current_spills.get(pid) != Some(llc) {
                map.update(&pid.to_ne_bytes(), &llc.to_ne_bytes(), MapFlags::ANY)?;
            }
        }
        for pid in self.current_spills.keys() {
            if !desired.contains_key(pid) {
                let _ = map.delete(&pid.to_ne_bytes());
            }
        }
        self.current_spills = desired;
        Ok(())
    }

    fn apply_merge(&mut self, plan: wake_graph::MergePlan) -> Result<()> {
        let counters_map = &self.skel.maps.cohort_counters;
        let policy_map = &self.skel.maps.cohort_policy;
        let into_key = plan.into.to_ne_bytes();
        let from_key = plan.from.to_ne_bytes();

        if counters_map.lookup(&into_key, MapFlags::ANY)?.is_none()
            || counters_map.lookup(&from_key, MapFlags::ANY)?.is_none()
        {
            return Ok(());
        }

        info!("merging cohort {} into {}", plan.from, plan.into);

        // Membership transfers through tgid_cohort alone: the BPF side
        // resolves through it on every wakeup, exit decrements land on
        // the absorbing cohort, and the daemon derives member counts from
        // task_stats — no counter needs rewriting.
        let tgid_map = &self.skel.maps.tgid_cohort;
        for key in tgid_map.keys() {
            let Some(val) = tgid_map.lookup(&key, MapFlags::ANY)? else {
                continue;
            };
            if u64::from_ne_bytes(val.as_slice().try_into()?) == plan.from {
                tgid_map.update(&key, &plan.into.to_ne_bytes(), MapFlags::EXIST)?;
            }
        }

        let _ = counters_map.delete(&from_key);
        let _ = policy_map.delete(&from_key);
        self.prev_load_sum.remove(&plan.from);
        self.nr_merges += 1;
        Ok(())
    }

    fn apply_split(&mut self, plan: wake_graph::SplitPlan) -> Result<()> {
        let counters_map = &self.skel.maps.cohort_counters;
        let policy_map = &self.skel.maps.cohort_policy;
        let old_key = plan.cohort.to_ne_bytes();
        let Some(old_val) = policy_map.lookup(&old_key, MapFlags::ANY)? else {
            return Ok(());
        };
        let Ok(old_policy) = CohortPolicy::read_from_bytes(old_val.as_slice()) else {
            return Ok(());
        };

        let new_id = DAEMON_COHORT_BASE + self.daemon_seq;
        self.daemon_seq += 1;

        info!(
            "splitting {} tgids out of cohort {} into {}",
            plan.off_tgids.len(),
            plan.cohort,
            new_id
        );

        // The new cohort starts at the same home; if the machine is
        // imbalanced afterwards, the balancer moves it — placement policy
        // stays in one place. Member counts are daemon-derived, so only
        // fresh entries are written; nothing existing is rewritten.
        let new_policy = CohortPolicy {
            home_llc: old_policy.home_llc,
            flags: 0,
        };
        policy_map.update(
            &new_id.to_ne_bytes(),
            new_policy.as_bytes(),
            MapFlags::NO_EXIST,
        )?;
        counters_map.update(
            &new_id.to_ne_bytes(),
            CohortCounters::default().as_bytes(),
            MapFlags::NO_EXIST,
        )?;

        let tgid_map = &self.skel.maps.tgid_cohort;
        for tgid in &plan.off_tgids {
            let key = tgid.to_ne_bytes();
            if tgid_map.lookup(&key, MapFlags::ANY)?.is_some() {
                tgid_map.update(&key, &new_id.to_ne_bytes(), MapFlags::EXIST)?;
            }
        }

        self.nr_splits += 1;
        Ok(())
    }

    fn discover_relationships(
        &mut self,
        cohorts: &[CohortSnapshot],
        tasks: &[TaskSnapshot],
        tgid_cohort: &HashMap<u32, u64>,
    ) -> Result<()> {
        let dt_s = self.interval.as_secs_f64();
        let deltas = self.snapshot_edges()?;
        self.wake_graph.observe(&deltas, dt_s, self.sample_mult);

        let cohort_sizes: HashMap<u64, u64> =
            cohorts.iter().map(|c| (c.id, c.nr_tasks)).collect();

        for plan in self.wake_graph.merges(tgid_cohort, &cohort_sizes) {
            self.apply_merge(plan)?;
        }

        let mut tgid_tasks: HashMap<u32, u64> = HashMap::new();
        for t in tasks {
            *tgid_tasks.entry(t.tgid).or_default() += 1;
        }
        let mut cohort_tgids: HashMap<u64, Vec<(u32, u64)>> = HashMap::new();
        for (&tgid, &cohort) in tgid_cohort {
            if cohort_sizes.contains_key(&cohort) {
                cohort_tgids.entry(cohort).or_default().push((
                    tgid,
                    tgid_tasks.get(&tgid).copied().unwrap_or(0),
                ));
            }
        }
        for plan in self.wake_graph.splits(&cohort_tgids) {
            self.apply_split(plan)?;
        }
        Ok(())
    }

    /// Per-LLC executed load: what actually ran where this tick, from the
    /// BPF-side llc_load counters. Spilled and stolen work counts at the
    /// LLC that ran it, so the imbalance signal converges.
    fn llc_loads(&mut self) -> Result<Vec<LlcLoad>> {
        let tick_ns = self.interval.as_nanos() as u64;
        let map = &self.skel.maps.llc_load;
        let mut out = Vec::new();

        for (&llc, &cpus) in &self.llc_cpus {
            let cur = map
                .lookup(&llc.to_ne_bytes(), MapFlags::ANY)?
                .map(|v| u64::from_ne_bytes(v.as_slice().try_into().unwrap_or([0; 8])))
                .unwrap_or(0);
            let prev = self.prev_llc_load.get(&llc).copied().unwrap_or(cur);
            self.prev_llc_load.insert(llc, cur);
            out.push(LlcLoad {
                llc,
                load_ns: cur.saturating_sub(prev),
                capacity_ns: cpus * tick_ns,
            });
        }
        Ok(out)
    }

    fn apply(&mut self, decisions: Vec<Decision>) -> Result<()> {
        let policy_map = &self.skel.maps.cohort_policy;
        for decision in decisions {
            match decision {
                Decision::MoveCohort { id, to } => {
                    let key = id.to_ne_bytes();
                    let Some(val) = policy_map.lookup(&key, MapFlags::ANY)? else {
                        continue;
                    };
                    let Ok(mut policy) = CohortPolicy::read_from_bytes(val.as_slice()) else {
                        continue;
                    };
                    debug!("moving cohort {} from LLC {} to {}", id, policy.home_llc, to);
                    policy.home_llc = to;
                    policy_map.update(&key, policy.as_bytes(), MapFlags::EXIST)?;
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

        stats::Metrics {
            nr_cohorts: cohorts.len() as u64,
            nr_tasks: cohorts.iter().map(|c| c.nr_tasks).sum(),
            // Recomputed from counters (cumulatively here, per-interval in
            // Metrics::delta) — see stats::affinity_pct for the formula.
            affinity_hit_pct: stats::affinity_pct(
                c[STAT_SYNC_LOCAL as usize]
                    + c[STAT_PREV_IDLE as usize]
                    + c[STAT_IDLE_CORE as usize]
                    + c[STAT_IDLE_SMT as usize],
                c[STAT_ENQ_HOME as usize],
                c[STAT_ENQ_SPILL as usize],
                c[STAT_STEAL as usize],
            ),
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
            nr_merges: self.nr_merges,
            nr_splits: self.nr_splits,
            nr_spilled: self.current_spills.len() as u64,
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
        let tasks = self.snapshot_tasks()?;
        let tgid_cohort = self.snapshot_tgid_cohort()?;
        self.apply_rules(&tasks)?;
        let cohorts = self.snapshot_cohorts(&tasks, &tgid_cohort)?;
        self.gc_tgid_map(&tgid_cohort, &tasks);
        let llcs = self.llc_loads()?;
        let decisions = self.balancer.plan(self.now_ms(), &llcs, &cohorts);
        self.apply(decisions)?;
        let desired = plan_spills(
            &self.spill_cfg,
            &llcs,
            &cohorts,
            &tasks,
            &self.current_spills,
        );
        self.apply_spills(desired)?;
        self.discover_relationships(&cohorts, &tasks, &tgid_cohort)?;
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
