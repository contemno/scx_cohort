// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! Optional TOML rules: overrides and experiments layered on top of the
//! automatic mechanisms (which the design stands on without any of this).
//!
//! ```toml
//! [[rule]]
//! match_comm = ["wineserver", "wine64-preloader"]
//! join_cohort_of = "parent"     # default anyway; shown for illustration
//!
//! [[rule]]
//! match_cgroup = "user.slice/*/app-steam*"
//! min_ccd_residency_ms = 5000   # extra migration hysteresis for games
//!
//! [[rule]]
//! match_comm = ["my-benchmark"]
//! pin_ccd = 0                   # nail the cohort to CCD 0
//! ```
//!
//! Rules are evaluated in file order; the first match wins.

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub options: OptionsTable,
    #[serde(default, rename = "rule")]
    pub rules: Vec<RuleFile>,
}

/// `[options]`: daemon knobs mirroring the CLI flags, so a systemd
/// service can be tuned without editing the unit. Explicitly passed CLI
/// flags take precedence over these; these take precedence over built-in
/// defaults.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionsTable {
    /// Scheduling slice in microseconds.
    pub slice_us: Option<u64>,
    /// Daemon tick interval in milliseconds.
    pub interval_ms: Option<u64>,
    /// Steal gate: minimum foreign queue depth.
    pub steal_min: Option<u64>,
    /// Steal gate: maximum head wait in microseconds.
    pub steal_delay_us: Option<u64>,
    /// Wakeup preemption: minimum interval per victim CPU, microseconds.
    pub preempt_min_us: Option<u64>,
    /// Balancer imbalance threshold, % of one CCD's capacity.
    pub imbalance_pct: Option<u64>,
    /// Post-move cohort immunity in milliseconds.
    pub residency_ms: Option<u64>,
    /// Sustained cross-cohort wake rate that merges cohorts.
    pub merge_wakes_per_sec: Option<f64>,
    /// SCHED_FIFO priority for the daemon; 0 disables the RT boost.
    pub rt_priority: Option<i32>,
    /// Drop ALL capabilities once the scheduler is attached. The daemon
    /// then cannot re-load BPF after a suspend/resume ejection, so it
    /// exits instead and relies on the service manager's
    /// Restart=on-failure to relaunch it with fresh privileges.
    pub drop_privs: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleFile {
    /// Exact comm names (as in /proc/<pid>/comm, max 15 chars).
    pub match_comm: Option<Vec<String>>,
    /// Glob matched against the task's cgroup path.
    pub match_cgroup: Option<String>,
    /// Only "parent" is meaningful (and is the default behavior); other
    /// values are rejected so typos don't silently no-op.
    pub join_cohort_of: Option<String>,
    /// Pin the matching task's cohort to this CCD.
    pub pin_ccd: Option<u32>,
    /// Extra migration hysteresis for the matching task's cohort.
    pub min_ccd_residency_ms: Option<u64>,
}

/// A compiled rule ready for matching.
#[derive(Debug)]
pub struct Rule {
    comms: Option<Vec<String>>,
    cgroup: Option<GlobMatcher>,
    pub pin_ccd: Option<u32>,
    pub min_ccd_residency_ms: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Config {
    pub options: OptionsTable,
    pub rules: Vec<Rule>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let file: ConfigFile = toml::from_str(text).context("failed to parse config")?;
        let mut rules = Vec::new();
        for (i, r) in file.rules.into_iter().enumerate() {
            if let Some(j) = &r.join_cohort_of {
                if j != "parent" {
                    bail!("rule {}: join_cohort_of only supports \"parent\"", i + 1);
                }
            }
            if r.match_comm.is_none() && r.match_cgroup.is_none() {
                bail!("rule {}: needs match_comm and/or match_cgroup", i + 1);
            }
            rules.push(Rule {
                comms: r.match_comm,
                cgroup: match &r.match_cgroup {
                    Some(g) => Some(
                        Glob::new(g)
                            .with_context(|| format!("rule {}: bad glob {g:?}", i + 1))?
                            .compile_matcher(),
                    ),
                    None => None,
                },
                pin_ccd: r.pin_ccd,
                min_ccd_residency_ms: r.min_ccd_residency_ms,
            });
        }
        Ok(Self {
            options: file.options,
            rules,
        })
    }

    pub fn load(path: &std::path::Path) -> Result<Self> {
        Self::parse(&std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?)
    }

    /// First rule matching this task, if any. All present criteria of a
    /// rule must match (AND).
    pub fn match_task(&self, comm: &str, cgroup: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| {
            let comm_ok = r
                .comms
                .as_ref()
                .is_none_or(|cs| cs.iter().any(|c| c == comm));
            let cgroup_ok = r.cgroup.as_ref().is_none_or(|g| g.is_match(cgroup));
            comm_ok && cgroup_ok
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[rule]]
match_comm = ["wineserver", "wine64-preloader"]
join_cohort_of = "parent"

[[rule]]
match_cgroup = "user.slice/*/app-steam*"
min_ccd_residency_ms = 5000

[[rule]]
match_comm = ["my-benchmark"]
pin_ccd = 0
"#;

    #[test]
    fn comm_exact_match() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert!(cfg.match_task("wineserver", "").is_some());
        assert!(cfg.match_task("wine64-preloader", "").is_some());
        // Substrings and prefixes do not match.
        assert!(cfg.match_task("wineserver2", "").is_none());
        assert!(cfg.match_task("wine", "").is_none());
    }

    #[test]
    fn cgroup_glob_match() {
        let cfg = Config::parse(SAMPLE).unwrap();
        let rule = cfg
            .match_task(
                "GameThread",
                "user.slice/user-1000.slice/app-steam-12345.scope",
            )
            .expect("glob should match");
        assert_eq!(rule.min_ccd_residency_ms, Some(5000));
        assert!(cfg
            .match_task("GameThread", "system.slice/sshd.service")
            .is_none());
    }

    #[test]
    fn pin_rule_carries_ccd() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(cfg.match_task("my-benchmark", "").unwrap().pin_ccd, Some(0));
    }

    #[test]
    fn first_match_wins() {
        let cfg = Config::parse(
            r#"
[[rule]]
match_comm = ["x"]
pin_ccd = 0

[[rule]]
match_comm = ["x"]
pin_ccd = 1
"#,
        )
        .unwrap();
        assert_eq!(cfg.match_task("x", "").unwrap().pin_ccd, Some(0));
    }

    #[test]
    fn and_semantics_within_a_rule() {
        let cfg = Config::parse(
            r#"
[[rule]]
match_comm = ["game"]
match_cgroup = "user.slice/*"
pin_ccd = 1
"#,
        )
        .unwrap();
        assert!(cfg.match_task("game", "user.slice/foo").is_some());
        assert!(cfg.match_task("game", "system.slice/foo").is_none());
        assert!(cfg.match_task("other", "user.slice/foo").is_none());
    }

    #[test]
    fn options_table_parses() {
        let cfg = Config::parse(
            "[options]\nslice_us = 3000\nrt_priority = 0\n\n[[rule]]\nmatch_comm = [\"a\"]\npin_ccd = 1",
        )
        .unwrap();
        assert_eq!(cfg.options.slice_us, Some(3000));
        assert_eq!(cfg.options.rt_priority, Some(0));
        assert!(cfg.options.interval_ms.is_none());
        assert_eq!(cfg.rules.len(), 1);
    }

    #[test]
    fn drop_privs_option_parses() {
        let cfg = Config::parse("[options]\ndrop_privs = true").unwrap();
        assert_eq!(cfg.options.drop_privs, Some(true));
        assert!(Config::parse("").unwrap().options.drop_privs.is_none());
    }

    #[test]
    fn options_absent_is_fine() {
        let cfg = Config::parse("[[rule]]\nmatch_comm = [\"a\"]\npin_ccd = 1").unwrap();
        assert!(cfg.options.slice_us.is_none());
    }

    #[test]
    fn unknown_option_key_rejected() {
        assert!(Config::parse("[options]\nslice_usec = 1").is_err());
    }

    #[test]
    fn bad_configs_rejected() {
        // Unknown join target.
        assert!(
            Config::parse("[[rule]]\nmatch_comm=[\"a\"]\njoin_cohort_of=\"grandparent\"").is_err()
        );
        // No matcher at all.
        assert!(Config::parse("[[rule]]\npin_ccd = 1").is_err());
        // Unknown key (typo protection).
        assert!(Config::parse("[[rule]]\nmatch_com = [\"a\"]").is_err());
    }
}
