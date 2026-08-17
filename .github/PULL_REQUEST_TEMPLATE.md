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
- [ ] `make lint` and `make test` pass locally
- [ ] Required CI checks reported green (a required check that never *ran* is not a pass)

## Re-sync with base

<!-- A clean diff against a STALE base hides regressions. See CONTRIBUTING.md. -->

- [ ] Rebased on the latest `dev`
- [ ] Checked `git log <branch-point>..origin/dev -- <changed files>`; if another PR
      touched these files, reconciled *intent* against the current `dev` version

## Docs moved (Done = docs updated)

- [ ] Updated the matching docs (`README.md`, `DESIGN.md`, `bench/README.md`, or
      `--help`/config docs) for this change, **or** N/A because: ___

## Project invariants

- [ ] Rust stays the single source of truth for the BPF↔userspace ABI: shared
      structs/constants change only in `scx_cohort_common` (`intf.h` is
      cbindgen-generated, never hand-edited), and the layout tests move with them
- [ ] The BPF per-wakeup hot path still makes decisions from map lookups only
      (no new unbounded work on the wakeup/dispatch path)
- [ ] The scheduler remains safely detachable — daemon exit, Ctrl-C, panic, or
      the sched_ext watchdog still falls back cleanly to EEVDF
