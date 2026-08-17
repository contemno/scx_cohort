<!--
PR base should be `dev` (feature → dev → main). The dev → main PR is the release
promotion. Keep the diff scoped so the right CI checks run.
-->

## What & why

<!-- What does this change, and why now? Link the issue. -->

Closes #

## How it was verified

<!-- Commands run, tests added, manual checks. "Trust me" is not verification. -->

- [ ] Tests added or updated for the new behavior (and for any reconciled conflict)
- [ ] `cargo fmt --all --check` and `cargo clippy --all-targets` pass locally
- [ ] Required CI checks reported green (a required check that never *ran* is not a pass)

## Re-sync with base

<!-- A clean diff against a STALE base hides regressions. See CONTRIBUTING.md. -->

- [ ] Rebased on the latest `dev`
- [ ] Checked `git log <branch-point>..origin/dev -- <changed files>`; if another PR
      touched these files, reconciled *intent* against the current `dev` version

## Docs moved (Done = docs updated)

- [ ] Updated README.md / DESIGN.md (and `--help` text, if flags changed) for this
      change, **or** N/A because: ___

## Project invariants

- [ ] Preserves the **fallback guarantee**: daemon exit, crash, or the sched_ext
      watchdog always detaches back to EEVDF, and no per-wakeup BPF path blocks
      or starves a runnable task — or the effect is called out above
- [ ] Any **BPF↔userspace shared layout** change goes through `scx_cohort_common`
      only, with the layout tests moving in the same commit (`intf.h` is
      generated — never hand-edited)
