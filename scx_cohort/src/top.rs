// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! `scx_cohort top`: a watch(1)-style view of cohorts and their processes,
//! served by the running scheduler's stats socket. Plain ANSI redraws — no
//! TUI dependencies, works over ssh.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::stats::ProcsSnapshot;

fn terminal_rows() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0
    {
        ws.ws_row as usize
    } else {
        40
    }
}

fn render(s: &ProcsSnapshot) {
    let mut lines = Vec::new();

    let llcs = s
        .llc_util_pct
        .iter()
        .map(|(id, util)| format!("CCD{id} {util:5.1}%"))
        .collect::<Vec<_>>()
        .join("  ");
    lines.push(format!(
        "scx_cohort  tasks {}  cohorts {}  affinity {:5.1}%  |  {}",
        s.nr_tasks,
        s.cohorts.len(),
        s.affinity_pct,
        llcs
    ));
    lines.push(String::new());
    lines.push(format!(
        "{:<10} {:<18} {:>4} {:>8} {:>7} {:>9}",
        "COHORT", "COMM", "CCD", "THREADS", "UTIL%", "AFFINITY%"
    ));

    // Cohorts by utilization, hottest first; processes likewise within.
    let mut cohorts: Vec<(&usize, &crate::stats::CohortRow)> = s.cohorts.iter().collect();
    cohorts.sort_by(|a, b| b.1.util_pct.total_cmp(&a.1.util_pct));

    for (id, c) in cohorts {
        lines.push(format!(
            "{:<10} {:<18} {:>3}{} {:>8} {:>7.1} {:>9.1}",
            id,
            "─ cohort ─",
            c.home_llc,
            if c.pinned != 0 { "*" } else { " " },
            c.tasks,
            c.util_pct,
            c.affinity_pct,
        ));
        let mut procs: Vec<&crate::stats::ProcRow> = c.procs.values().collect();
        procs.sort_by(|a, b| b.util_pct.total_cmp(&a.util_pct));
        for p in procs {
            lines.push(format!(
                "  {:<8} {:<18} {:>4} {:>5}{:>3} {:>7.1} {:>9.1}",
                p.tgid,
                if p.comm.len() > 18 { &p.comm[..18] } else { &p.comm },
                "",
                p.threads,
                if p.spilled > 0 {
                    format!("s{}", p.spilled)
                } else {
                    String::new()
                },
                p.util_pct,
                p.affinity_pct,
            ));
        }
    }

    let rows = terminal_rows().saturating_sub(1).max(10);
    print!("\x1b[2J\x1b[H{}", lines[..lines.len().min(rows)].join("\n"));
    println!();
}

pub fn run(intv: Duration, shutdown: Arc<AtomicBool>) -> Result<()> {
    scx_utils::monitor_stats::<ProcsSnapshot>(
        &[("target".into(), "procs".into())],
        intv,
        || shutdown.load(Ordering::Relaxed),
        |snapshot| {
            render(&snapshot);
            Ok(())
        },
    )
}
