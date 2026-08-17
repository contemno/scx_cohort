# Security policy

scx_cohort is a sched_ext scheduler: it loads BPF programs into the kernel and
runs a privileged userspace daemon. Bugs here can have kernel-level impact, so
please report them privately.

## Reporting a vulnerability

**Do not open a public issue.** Report it via a private GitHub security
advisory:

https://github.com/contemno/scx_cohort/security/advisories/new

Include the version or commit, kernel version, and a reproducer if you have
one. You'll get a response in the advisory thread; the fix will be developed
privately and credited to you unless you prefer otherwise.

## Scope

Of particular interest:

- anything reachable from an unprivileged task that can crash, wedge, or
  starve the scheduler (the design goal is that the worst case is a clean
  ejection back to EEVDF — a way to defeat that fallback is a vulnerability);
- memory-safety issues in the daemon's map-byte parsing
  (`scx_cohort_common` zerocopy types) or the BPF↔userspace ABI;
- privilege-escalation paths through the systemd service or the stats socket.
