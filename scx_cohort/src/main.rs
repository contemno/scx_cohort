// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! scx_cohort userspace daemon: loads the BPF component, discovers the CPU
//! topology, and (in later phases) runs the cohort graph and load balancer.

mod bpf_skel;
pub use bpf_skel::*;

use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use libbpf_rs::OpenObject;
use log::info;
use scx_utils::scx_ops_attach;
use scx_utils::scx_ops_load;
use scx_utils::scx_ops_open;
use scx_utils::uei_exited;
use scx_utils::uei_report;
use scx_utils::Topology;
use scx_utils::UserExitInfo;

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

    /// Increase verbosity (-v: debug, -vv: trace).
    #[clap(short = 'v', long, action = clap::ArgAction::Count)]
    verbose: u8,
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

        skel.maps.rodata_data.as_mut().unwrap().slice_ns = opts.slice_us * 1000;

        let mut skel = scx_ops_load!(skel, cohort_ops, uei)?;
        let struct_ops = scx_ops_attach!(skel, cohort_ops)?;
        info!("scx_cohort attached");

        Ok(Self {
            skel,
            _struct_ops: struct_ops,
            interval: Duration::from_millis(opts.interval_ms),
        })
    }

    fn run(&mut self, shutdown: Arc<AtomicBool>) -> Result<UserExitInfo> {
        while !shutdown.load(Ordering::Relaxed) && !uei_exited!(&self.skel, uei) {
            std::thread::sleep(self.interval);
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
    lcfg.set_time_offset_to_local()
        .expect("set local time offset")
        .set_time_level(simplelog::LevelFilter::Error)
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

    let mut open_object = MaybeUninit::uninit();
    let mut sched = Scheduler::init(&opts, &mut open_object)?;
    let uei = sched.run(shutdown)?;
    uei.report()?;
    Ok(())
}
