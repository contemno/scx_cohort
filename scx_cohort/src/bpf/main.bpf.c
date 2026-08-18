/* Copyright (c) scx_cohort authors.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 *
 * scx_cohort BPF component: the per-wakeup fast path. Every decision here
 * is made from map lookups only; the userspace daemon does the slow
 * thinking and writes its conclusions into the maps this code reads.
 *
 * All types and constants shared with userspace come from intf.h, which is
 * generated from the scx_cohort_common Rust crate — do not define shared
 * state here. Kernel-only state (cpumask kptrs, per-LLC vtime) lives in
 * BPF-local structs below.
 *
 * Placement model: each LLC (CCD) has one vtime-ordered DSQ whose id is
 * the LLC id. A task's cohort has a home LLC; select_cpu never looks for
 * idle CPUs outside it, and dispatch steals across LLCs only through a
 * priced gate. Cohorts form from tgid grouping and fork lineage here and
 * are severed at exec (a new program image starts a fresh cohort);
 * wake-edge driven merges/splits arrive with the daemon in later phases.
 */
#include <scx/common.bpf.h>
#include "intf.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

/* Set by userspace between open and load. */
const volatile u64 slice_ns = 5000000ULL;
const volatile u32 nr_cpus = 1;
const volatile u32 nr_llcs = 1;
const volatile u32 cpu_llc_id[MAX_CPUS];

/* Live-tunable; rewritten by the daemon at any time. */
struct Tunables tunables = {
	.steal_min		= 2,
	.steal_delay_ns		= 500000,
	.credit_max_ns		= 4000000,
	.credit_wake_freq_min	= 50,
	.credit_runtime_max_ns	= 2000000,
	.preempt_min_ns		= 20000,
	.sample_shift		= 3,
};

/* Cohort id allocator (ids start at 1) and round-robin home assignment. */
static u64 cohort_seq;
static u32 rr_ctr;

static inline bool vtime_before(u64 a, u64 b)
{
	return (s64)(a - b) < 0;
}

/*
 * Kernel-only per-LLC state. The DSQ with id == LLC id is created in
 * ops.init; the cpumask is built there from cpu_llc_id.
 */
struct llc_ctx {
	struct bpf_cpumask __kptr *cpumask;
	u64 vtime_now;
	/*
	 * Approximate enqueue time of the DSQ head: refreshed when a task
	 * is enqueued into an empty DSQ and when the DSQ is consumed.
	 * Feeds the steal gate's wait-time condition.
	 */
	u64 head_enq_ts;
};

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__type(key, u32);
	__type(value, struct llc_ctx);
	__uint(max_entries, MAX_LLCS);
} llcs SEC(".maps");

/*
 * Per-CPU scratch state: cpumask for intersections, wake-edge sampling,
 * and what this CPU is currently running as seen by remote preemption
 * decisions. curr_interactive goes stale while the CPU idles, which is
 * harmless: an idle CPU is found by the idle search before enqueue ever
 * considers preempting it.
 */
struct scratch_ctx {
	struct bpf_cpumask __kptr *mask;
	u32 sample_ctr;
	/* The running task fits the interactive signature (set in running). */
	u32 curr_interactive;
	/* Last time enqueue preempted this CPU; rate-limits the IPIs. */
	u64 last_preempt_ts;
};

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__type(key, u32);
	__type(value, struct scratch_ctx);
	__uint(max_entries, 1);
} scratch_stor SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_TASK_STORAGE);
	__uint(map_flags, BPF_F_NO_PREALLOC);
	__type(key, int);
	__type(value, struct TaskCtx);
} task_ctxs SEC(".maps");

/*
 * Cohort state is split into two maps so each side owns whole values
 * exclusively: userspace map updates replace entire values, which would
 * race BPF-side atomic arithmetic if they shared a struct.
 *
 * cohort_policy: created here (BPF_NOEXIST) when a cohort first appears;
 * every later write (moves, pins) comes from the daemon.
 */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, u64);
	__type(value, struct CohortPolicy);
	__uint(max_entries, MAX_COHORTS);
} cohort_policy SEC(".maps");

/* cohort_counters: written only here (atomically); the daemon reads. */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, u64);
	__type(value, struct CohortCounters);
	__uint(max_entries, MAX_COHORTS);
} cohort_counters SEC(".maps");

/*
 * Weight-scaled runtime actually executed per LLC (as opposed to cohort
 * demand, which is charged to the cohort wherever its tasks ran). The
 * daemon diffs this for the balancer's imbalance signal, so spilled and
 * stolen work counts where it runs.
 */
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__type(key, u32);
	__type(value, u64);
	__uint(max_entries, MAX_LLCS);
} llc_load SEC(".maps");

/*
 * tgid → cohort id. Entries persist until the daemon garbage-collects
 * tgids with no live tasks; sized generously so the interim leak is
 * harmless.
 */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, u32);
	__type(value, u64);
	__uint(max_entries, 65536);
} tgid_cohort SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__type(key, u32);
	__type(value, u64);
	__uint(max_entries, NR_STATS);
} stats SEC(".maps");

/*
 * pid → TaskStat. Task storage cannot be iterated from userspace, so the
 * daemon's per-tick view of tasks (duty cycles, spill candidates, comm
 * rule matching) lives in this ordinary hash instead.
 */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, u32);
	__type(value, struct TaskStat);
	__uint(max_entries, 65536);
} task_stats SEC(".maps");

/*
 * (waker_tgid, wakee_tgid) → sampled wake count. The daemon reads and
 * decays these to discover relationships lineage misses (merges) and to
 * see a cohort's internal structure (splits). LRU: cold edges fall out on
 * their own.
 */
struct {
	__uint(type, BPF_MAP_TYPE_LRU_HASH);
	__type(key, struct WakeEdgeKey);
	__type(value, u64);
	__uint(max_entries, WAKE_EDGE_SLOTS);
} wake_edges SEC(".maps");

/*
 * pid → spill target LLC. Written only by the daemon: members of an
 * oversized cohort that it chose to run remotely. Sticky by construction —
 * the same pids stay listed until demand fits the home LLC again.
 */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, u32);
	__type(value, u32);
	__uint(max_entries, 8192);
} spill_tasks SEC(".maps");

static void stat_inc(u32 idx)
{
	u64 *cnt = bpf_map_lookup_elem(&stats, &idx);

	if (cnt)
		(*cnt)++;
}

static struct TaskCtx *lookup_task_ctx(struct task_struct *p)
{
	struct TaskCtx *tctx = bpf_task_storage_get(&task_ctxs, p, 0, 0);

	if (!tctx)
		stat_inc(STAT_TCTX_ERR);
	return tctx;
}

static struct llc_ctx *lookup_llc_ctx(u32 llc)
{
	return bpf_map_lookup_elem(&llcs, &llc);
}

static u32 safe_llc(u32 llc)
{
	return llc < nr_llcs ? llc : 0;
}

/*
 * The interactive signature: woken often, running briefly (render
 * threads, compositors, IPC pairs). Gates both the vtime credit and
 * wakeup preemption.
 */
static bool task_interactive(const struct TaskCtx *tctx)
{
	return tctx->wake_freq >= tunables.credit_wake_freq_min &&
	       tctx->avg_runtime_ns > 0 &&
	       tctx->avg_runtime_ns <= tunables.credit_runtime_max_ns;
}

static u32 llc_of_cpu(s32 cpu)
{
	if (cpu >= 0 && cpu < MAX_CPUS)
		return safe_llc(cpu_llc_id[cpu]);
	return 0;
}

/*
 * Resolve @p's cohort through tgid_cohort — the map the daemon rewrites
 * to apply merges and splits — refreshing the task-storage cache, so
 * membership changes take effect on the very next wakeup.
 */
static u64 resolve_cohort(struct task_struct *p, struct TaskCtx *tctx)
{
	u32 tgid = p->tgid;
	u64 *idp = bpf_map_lookup_elem(&tgid_cohort, &tgid);

	if (idp) {
		if (tctx)
			tctx->cohort_id = *idp;
		return *idp;
	}
	return tctx ? tctx->cohort_id : COHORT_INVALID;
}

/*
 * The LLC this task should be placed on: its cohort's home, or the spill
 * target if the daemon marked it spilled.
 */
static u32 task_home_llc(struct task_struct *p, struct TaskCtx *tctx)
{
	struct CohortPolicy *policy;
	u32 pid = p->pid;
	u32 *spill;
	u32 home;
	u64 id;

	if (!tctx)
		return llc_of_cpu(scx_bpf_task_cpu(p));

	spill = bpf_map_lookup_elem(&spill_tasks, &pid);
	if (spill) {
		tctx->is_spilled = 1;
		tctx->spill_llc = safe_llc(*spill);
		tctx->home_llc = tctx->spill_llc;
		return tctx->spill_llc;
	}
	tctx->is_spilled = 0;

	id = resolve_cohort(p, tctx);
	policy = bpf_map_lookup_elem(&cohort_policy, &id);
	if (!policy)
		home = llc_of_cpu(scx_bpf_task_cpu(p));
	else
		home = safe_llc(policy->home_llc);

	/* Cached for stopping's home-vs-away runtime accounting. */
	tctx->home_llc = home;
	return home;
}

/*
 * Sampled recording of one waker→wakee edge. Cost is one LRU hash update
 * every 2^sample_shift cross-process wakeups per CPU.
 */
static void record_wake_edge(struct task_struct *p)
{
	struct task_struct *waker = (void *)bpf_get_current_task_btf();
	struct scratch_ctx *sctx;
	const u32 zero = 0;

	if (!waker || waker->tgid == p->tgid)
		return;

	sctx = bpf_map_lookup_elem(&scratch_stor, &zero);
	if (!sctx)
		return;
	if (++sctx->sample_ctr & ((1 << (tunables.sample_shift & 15)) - 1))
		return;

	{
		struct WakeEdgeKey key = {
			.waker_tgid = waker->tgid,
			.wakee_tgid = p->tgid,
		};
		u64 *cnt = bpf_map_lookup_elem(&wake_edges, &key);

		if (cnt) {
			__sync_fetch_and_add(cnt, 1);
		} else {
			u64 one = 1;

			bpf_map_update_elem(&wake_edges, &key, &one, BPF_ANY);
		}
	}
}

/*
 * Resolve the LLC whose DSQ @p must be enqueued to. Differs from the home
 * LLC only for tasks whose CPU affinity excludes the home entirely: a
 * per-LLC DSQ is consumed only by that LLC's CPUs, so enqueueing such a
 * task at home would strand it.
 */
static u32 task_effective_llc(struct task_struct *p, struct TaskCtx *tctx)
{
	u32 home = task_home_llc(p, tctx);
	struct llc_ctx *llcx = lookup_llc_ctx(home);
	const struct cpumask *mask;

	if (!llcx)
		return home;
	mask = cast_mask(llcx->cpumask);
	if (mask && !bpf_cpumask_intersects(mask, p->cpus_ptr))
		return llc_of_cpu(scx_bpf_task_cpu(p));
	return home;
}

s32 BPF_STRUCT_OPS(cohort_select_cpu, struct task_struct *p, s32 prev_cpu,
		   u64 wake_flags)
{
	const struct cpumask *idle_smt, *home_mask;
	struct TaskCtx *tctx;
	struct scratch_ctx *sctx;
	struct bpf_cpumask *scratch;
	struct llc_ctx *llcx;
	const u32 zero = 0;
	bool prev_in_home;
	u32 home;
	s32 cpu;

	record_wake_edge(p);

	if (p->nr_cpus_allowed == 1)
		return prev_cpu;

	tctx = lookup_task_ctx(p);
	home = task_home_llc(p, tctx);
	llcx = lookup_llc_ctx(home);
	if (!llcx)
		return prev_cpu;
	home_mask = cast_mask(llcx->cpumask);
	if (!home_mask || !bpf_cpumask_intersects(home_mask, p->cpus_ptr))
		return prev_cpu;

	prev_in_home = bpf_cpumask_test_cpu(prev_cpu, home_mask);

	/*
	 * Synchronous handoff: the waker is about to sleep. If it belongs
	 * to the same cohort, runs inside the home LLC, and its local DSQ
	 * is empty, run the wakee right where the shared cache lines are.
	 */
	if (wake_flags & SCX_WAKE_SYNC) {
		struct task_struct *waker = (void *)bpf_get_current_task_btf();
		struct TaskCtx *wtctx = waker ? bpf_task_storage_get(&task_ctxs, waker, 0, 0) : NULL;

		if (tctx && wtctx && tctx->cohort_id == wtctx->cohort_id) {
			cpu = bpf_get_smp_processor_id();
			if (bpf_cpumask_test_cpu(cpu, home_mask) &&
			    bpf_cpumask_test_cpu(cpu, p->cpus_ptr) &&
			    !scx_bpf_dsq_nr_queued(SCX_DSQ_LOCAL_ON | cpu)) {
				stat_inc(STAT_SYNC_LOCAL);
				scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, slice_ns, 0);
				return cpu;
			}
		}
	}

	/*
	 * prev_cpu with its whole core idle: cache-warm and contention-free,
	 * the best possible seat.
	 */
	if (prev_in_home) {
		idle_smt = scx_bpf_get_idle_smtmask();
		if (bpf_cpumask_test_cpu(prev_cpu, idle_smt) &&
		    scx_bpf_test_and_clear_cpu_idle(prev_cpu)) {
			scx_bpf_put_idle_cpumask(idle_smt);
			stat_inc(STAT_PREV_IDLE);
			scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, slice_ns, 0);
			return prev_cpu;
		}
		scx_bpf_put_idle_cpumask(idle_smt);
	}

	/* Idle search is bounded to home ∩ p->cpus_ptr from here on. */
	sctx = bpf_map_lookup_elem(&scratch_stor, &zero);
	if (!sctx)
		return prev_in_home ? prev_cpu : (s32)bpf_cpumask_first(home_mask);
	scratch = sctx->mask;
	if (!scratch)
		return prev_in_home ? prev_cpu : (s32)bpf_cpumask_first(home_mask);
	bpf_cpumask_and(scratch, home_mask, p->cpus_ptr);

	/* Any fully idle core in the home LLC. */
	cpu = scx_bpf_pick_idle_cpu(cast_mask(scratch), SCX_PICK_IDLE_CORE);
	if (cpu >= 0) {
		stat_inc(STAT_IDLE_CORE);
		scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, slice_ns, 0);
		return cpu;
	}

	/* prev_cpu idle at all (SMT sibling busy). */
	if (prev_in_home && scx_bpf_test_and_clear_cpu_idle(prev_cpu)) {
		stat_inc(STAT_IDLE_SMT);
		scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, slice_ns, 0);
		return prev_cpu;
	}

	/* Any idle SMT sibling in the home LLC. */
	cpu = scx_bpf_pick_idle_cpu(cast_mask(scratch), 0);
	if (cpu >= 0) {
		stat_inc(STAT_IDLE_SMT);
		scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, slice_ns, 0);
		return cpu;
	}

	/*
	 * Home LLC fully busy. Clamp into the home LLC and let enqueue
	 * queue the task there. Deliberately NOT falling through to a
	 * machine-wide idle search: that reflex is exactly what this
	 * scheduler exists to remove.
	 */
	stat_inc(STAT_HOME_MISS_CLAMP);
	if (prev_in_home)
		return prev_cpu;
	return (s32)bpf_cpumask_any_distribute(cast_mask(scratch));
}

void BPF_STRUCT_OPS(cohort_enqueue, struct task_struct *p, u64 enq_flags)
{
	struct TaskCtx *tctx = lookup_task_ctx(p);
	u32 llc = task_effective_llc(p, tctx);
	struct llc_ctx *llcx = lookup_llc_ctx(llc);
	u64 vtime_now = llcx ? llcx->vtime_now : 0;
	u64 vtime = tctx ? tctx->vtime : vtime_now;

	/*
	 * Bound how much vtime credit a long sleeper can accumulate: one
	 * slice behind the LLC's frontier at most.
	 */
	if (vtime_before(vtime, vtime_now - slice_ns))
		vtime = vtime_now - slice_ns;

	/*
	 * Tasks with the interactive signature jump the queue within their
	 * CCD by a bounded credit. The clamp above plus this fixed bound
	 * keep the unfairness finite, so batch tasks cannot be starved
	 * past the watchdog's patience.
	 */
	if (tctx && task_interactive(tctx)) {
		vtime -= tunables.credit_max_ns;

		/*
		 * Queue-jumping only helps once some CPU dispatches; with
		 * every home CPU held by long-slice batch tasks the wakee
		 * still waits out someone's slice (measured as pingpong
		 * p50 doubling under per-CPU spinner load). Preempt the
		 * wakee's cache-warm prev_cpu instead — but only on a real
		 * wakeup, only if that CPU currently runs a task that is
		 * NOT itself interactive (batch work can be displaced;
		 * peers cannot, or all-interactive workloads like
		 * hackbench would preempt each other on every message),
		 * and at most once per victim CPU per preempt_min_ns.
		 */
		if ((enq_flags & SCX_ENQ_WAKEUP) && llcx) {
			const struct cpumask *hmask = cast_mask(llcx->cpumask);
			s32 prev = scx_bpf_task_cpu(p);

			if (hmask && prev >= 0 &&
			    bpf_cpumask_test_cpu(prev, hmask) &&
			    bpf_cpumask_test_cpu(prev, p->cpus_ptr)) {
				const u32 zero = 0;
				struct scratch_ctx *vs =
					bpf_map_lookup_percpu_elem(&scratch_stor,
								   &zero, prev);
				u64 pnow = scx_bpf_now();

				if (vs && !vs->curr_interactive &&
				    pnow - vs->last_preempt_ts >
					    tunables.preempt_min_ns) {
					/* Benign cross-CPU race: worst case
					 * is one extra preempt. */
					vs->last_preempt_ts = pnow;
					stat_inc(STAT_PREEMPT);
					scx_bpf_dsq_insert(p,
						SCX_DSQ_LOCAL_ON | (u32)prev,
						slice_ns,
						enq_flags | SCX_ENQ_PREEMPT);
					return;
				}
			}
		}
	}

	if (llcx && !scx_bpf_dsq_nr_queued(llc))
		llcx->head_enq_ts = scx_bpf_now();

	stat_inc(tctx && tctx->is_spilled ? STAT_ENQ_SPILL : STAT_ENQ_HOME);
	scx_bpf_dsq_insert_vtime(p, llc, slice_ns, vtime, enq_flags);
}

void BPF_STRUCT_OPS(cohort_dispatch, s32 cpu, struct task_struct *prev)
{
	u32 my_llc = llc_of_cpu(cpu);
	struct llc_ctx *llcx = lookup_llc_ctx(my_llc);
	u64 now;
	u32 i;

	if (scx_bpf_dsq_move_to_local(my_llc, 0)) {
		if (llcx)
			llcx->head_enq_ts = scx_bpf_now();
		return;
	}

	/*
	 * Own LLC is empty. Crossing the fabric is a priced decision:
	 * steal only from an LLC whose DSQ is deep AND whose head has
	 * waited long enough that fairness beats locality. Both conditions
	 * are required: depth alone is normal operation on a busy CCD, not
	 * starvation — with depth as an alternative trigger, two near-full
	 * CCDs trade queue heads on nearly every dispatch (measured at
	 * ~400k cross-CCD migrations/sec on hackbench-class loads),
	 * un-placing the load spill just placed. Wait time is the actual
	 * signal that the victim CCD can't drain its own queue; a standing
	 * overload is spill's job to shed, not this gate's. Stolen tasks
	 * keep their cohort and drift home on their next wakeup.
	 */
	now = scx_bpf_now();
	bpf_for(i, 0, nr_llcs) {
		struct llc_ctx *fx;
		s32 queued;

		if (i == my_llc)
			continue;
		fx = lookup_llc_ctx(i);
		if (!fx)
			continue;
		queued = scx_bpf_dsq_nr_queued(i);
		if (queued <= 0)
			continue;
		if ((u64)queued > tunables.steal_min &&
		    now - fx->head_enq_ts > tunables.steal_delay_ns) {
			if (scx_bpf_dsq_move_to_local(i, 0)) {
				fx->head_enq_ts = now;
				stat_inc(STAT_STEAL);
				return;
			}
		}
	}
}

void BPF_STRUCT_OPS(cohort_runnable, struct task_struct *p, u64 enq_flags)
{
	struct TaskCtx *tctx = lookup_task_ctx(p);
	u64 now, interval;

	if (!tctx)
		return;

	now = scx_bpf_now();
	if (tctx->last_wake_at) {
		interval = now - tctx->last_wake_at;
		if (interval > 0) {
			u64 freq = 1000000000ULL / interval;

			tctx->wake_freq = (tctx->wake_freq * 3 + freq) / 4;
		}
	}
	tctx->last_wake_at = now;
}

void BPF_STRUCT_OPS(cohort_running, struct task_struct *p)
{
	struct TaskCtx *tctx = lookup_task_ctx(p);
	u32 llc = llc_of_cpu(scx_bpf_task_cpu(p));
	struct llc_ctx *llcx = lookup_llc_ctx(llc);
	struct scratch_ctx *sctx;
	const u32 zero = 0;

	if (!tctx)
		return;

	tctx->last_run_at = scx_bpf_now();
	if (llcx && vtime_before(llcx->vtime_now, tctx->vtime))
		llcx->vtime_now = tctx->vtime;

	/* Publish what this CPU runs, for remote preemption decisions. */
	sctx = bpf_map_lookup_elem(&scratch_stor, &zero);
	if (sctx)
		sctx->curr_interactive = task_interactive(tctx);
}

void BPF_STRUCT_OPS(cohort_stopping, struct task_struct *p, bool runnable)
{
	struct CohortCounters *counters;
	struct TaskCtx *tctx = lookup_task_ctx(p);
	u64 delta, scaled;
	u32 run_llc;
	u64 *lload;

	if (!tctx)
		return;

	delta = scx_bpf_now() - tctx->last_run_at;
	scaled = delta * p->scx.weight / 100;
	tctx->vtime += delta * 100 / p->scx.weight;
	tctx->avg_runtime_ns = (tctx->avg_runtime_ns * 3 + delta) / 4;

	counters = bpf_map_lookup_elem(&cohort_counters, &tctx->cohort_id);
	if (counters)
		__sync_fetch_and_add(&counters->load_sum, scaled);

	/* Executed load is charged to the LLC that actually ran the task. */
	run_llc = llc_of_cpu(scx_bpf_task_cpu(p));
	lload = bpf_map_lookup_elem(&llc_load, &run_llc);
	if (lload)
		__sync_fetch_and_add(lload, scaled);

	{
		u32 pid = p->pid;
		struct TaskStat *ts = bpf_map_lookup_elem(&task_stats, &pid);

		if (ts) {
			ts->runtime_sum += delta;
			if (run_llc == tctx->home_llc)
				ts->runtime_home_sum += delta;
			/* Keeps the daemon's view current across merges. */
			ts->cohort_id = tctx->cohort_id;
		}
	}
}

/* Mint a fresh cohort with a round-robin home LLC. */
static u64 new_cohort(void)
{
	struct CohortPolicy policy = {
		.home_llc = __sync_fetch_and_add(&rr_ctr, 1) % nr_llcs,
	};
	struct CohortCounters counters = {};
	u64 id = __sync_fetch_and_add(&cohort_seq, 1) + 1;

	bpf_map_update_elem(&cohort_policy, &id, &policy, BPF_NOEXIST);
	bpf_map_update_elem(&cohort_counters, &id, &counters, BPF_NOEXIST);
	return id;
}

/*
 * Assign @p to a cohort: existing tgid mapping first, then fork lineage
 * (current is the forking parent when args->fork is set), then a fresh
 * cohort.
 */
static u64 assign_cohort(struct task_struct *p, bool fork)
{
	u32 tgid = p->tgid;
	u64 *idp, id = COHORT_INVALID;

	idp = bpf_map_lookup_elem(&tgid_cohort, &tgid);
	if (idp)
		return *idp;

	if (fork) {
		struct task_struct *parent = (void *)bpf_get_current_task_btf();
		struct TaskCtx *ptctx =
			parent ? bpf_task_storage_get(&task_ctxs, parent, 0, 0) : NULL;

		if (ptctx)
			id = ptctx->cohort_id;
	}

	if (id == COHORT_INVALID)
		id = new_cohort();

	bpf_map_update_elem(&tgid_cohort, &tgid, &id, BPF_ANY);
	return id;
}

/*
 * exec is a program-identity boundary. Fork lineage groups the tree a
 * program builds for itself (Chrome's zygote forking renderers), but
 * letting membership survive exec chains every unrelated binary a shell
 * or launcher spawns into its ancestor's cohort — everything descending
 * from one terminal collapses into a single machine-wide cohort homed on
 * one CCD. Sever here: the exec'ing task leaves the lineage cohort and
 * starts a fresh one, exactly like a fresh top-level task. Related
 * processes that are launched via fork+exec (game ↔ wineserver, browser
 * ↔ GPU process) are re-merged by the daemon from their observed wake
 * edges within about a second of them actually talking.
 */
SEC("tp_btf/sched_process_exec")
int BPF_PROG(cohort_exec_sever, struct task_struct *p, pid_t old_pid,
	     struct linux_binprm *bprm)
{
	struct CohortCounters *counters;
	struct CohortPolicy *policy;
	struct TaskCtx *tctx;
	u32 tgid = p->tgid;
	u64 id;

	tctx = bpf_task_storage_get(&task_ctxs, p, 0, 0);
	if (!tctx)
		return 0;

	/* Leave the lineage cohort; the daemon GCs it if now empty. */
	id = resolve_cohort(p, tctx);
	counters = bpf_map_lookup_elem(&cohort_counters, &id);
	if (counters)
		__sync_fetch_and_sub(&counters->nr_tasks, 1);

	id = new_cohort();
	bpf_map_update_elem(&tgid_cohort, &tgid, &id, BPF_ANY);
	tctx->cohort_id = id;
	counters = bpf_map_lookup_elem(&cohort_counters, &id);
	if (counters)
		__sync_fetch_and_add(&counters->nr_tasks, 1);

	/*
	 * A new program starts at its new home's vtime frontier, exactly as
	 * init_task would place it; carrying the old cohort's vtime across
	 * could park it far behind or ahead of the new LLC's clock.
	 */
	policy = bpf_map_lookup_elem(&cohort_policy, &id);
	if (policy) {
		struct llc_ctx *llcx = lookup_llc_ctx(safe_llc(policy->home_llc));

		if (llcx)
			tctx->vtime = llcx->vtime_now;
	}

	{
		u32 pid = p->pid;
		struct TaskStat *ts = bpf_map_lookup_elem(&task_stats, &pid);

		/* exec replaced comm; keep the daemon's rule matching current. */
		if (ts) {
			ts->cohort_id = id;
			__builtin_memcpy(ts->comm, p->comm, sizeof(ts->comm));
		}
		/* Any spill mark belonged to the old cohort's plan. */
		bpf_map_delete_elem(&spill_tasks, &pid);
	}

	stat_inc(STAT_EXEC_SEVER);
	return 0;
}

s32 BPF_STRUCT_OPS(cohort_init_task, struct task_struct *p,
		   struct scx_init_task_args *args)
{
	struct CohortCounters *counters;
	struct CohortPolicy *policy;
	struct TaskCtx *tctx;
	u64 id;

	tctx = bpf_task_storage_get(&task_ctxs, p, 0,
				    BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (!tctx)
		return -ENOMEM;

	id = assign_cohort(p, args->fork);
	tctx->cohort_id = id;

	counters = bpf_map_lookup_elem(&cohort_counters, &id);
	if (counters)
		__sync_fetch_and_add(&counters->nr_tasks, 1);

	policy = bpf_map_lookup_elem(&cohort_policy, &id);
	if (policy) {
		struct llc_ctx *llcx = lookup_llc_ctx(safe_llc(policy->home_llc));

		tctx->vtime = llcx ? llcx->vtime_now : 0;
	}

	{
		u32 pid = p->pid;
		struct TaskStat ts = {
			.tgid = p->tgid,
			.cohort_id = id,
		};

		__builtin_memcpy(ts.comm, p->comm, sizeof(ts.comm));
		bpf_map_update_elem(&task_stats, &pid, &ts, BPF_ANY);
	}

	return 0;
}

void BPF_STRUCT_OPS(cohort_exit_task, struct task_struct *p,
		    struct scx_exit_task_args *args)
{
	struct TaskCtx *tctx = bpf_task_storage_get(&task_ctxs, p, 0, 0);
	struct CohortCounters *counters;
	u64 id;

	if (!tctx)
		return;

	/*
	 * Resolve through tgid_cohort so that after a daemon-applied merge
	 * the decrement lands on the cohort that absorbed our count.
	 */
	id = resolve_cohort(p, tctx);
	counters = bpf_map_lookup_elem(&cohort_counters, &id);
	if (counters)
		__sync_fetch_and_sub(&counters->nr_tasks, 1);

	{
		u32 pid = p->pid;

		bpf_map_delete_elem(&task_stats, &pid);
		bpf_map_delete_elem(&spill_tasks, &pid);
	}
	/*
	 * Empty cohorts and stale tgid_cohort entries are garbage-collected
	 * by the daemon, which can iterate the maps; BPF cannot.
	 */
}

s32 BPF_STRUCT_OPS_SLEEPABLE(cohort_init)
{
	u32 cpu, llc;
	s32 ret;

	bpf_for(llc, 0, nr_llcs) {
		struct bpf_cpumask *mask, *old;
		struct llc_ctx *llcx = lookup_llc_ctx(llc);

		if (!llcx)
			return -ENOENT;

		ret = scx_bpf_create_dsq(llc, -1);
		if (ret)
			return ret;

		/*
		 * Build the mask fully on the owned pointer before publishing
		 * it. This callback is sleepable, so a pointer read back out
		 * of the map is untrusted (no RCU marking) and the verifier
		 * rejects mutating kfuncs on it; the create→set→xchg order
		 * never needs one.
		 */
		mask = bpf_cpumask_create();
		if (!mask)
			return -ENOMEM;
		bpf_for(cpu, 0, nr_cpus) {
			if (cpu >= MAX_CPUS)
				break;
			if (safe_llc(cpu_llc_id[cpu]) == llc)
				bpf_cpumask_set_cpu(cpu, mask);
		}
		old = bpf_kptr_xchg(&llcx->cpumask, mask);
		if (old)
			bpf_cpumask_release(old);
	}

	/* Per-CPU scratch masks for select_cpu's intersections. */
	bpf_for(cpu, 0, nr_cpus) {
		struct scratch_ctx *sctx;
		struct bpf_cpumask *mask, *old;
		const u32 zero = 0;

		sctx = bpf_map_lookup_percpu_elem(&scratch_stor, &zero, cpu);
		if (!sctx)
			continue;
		mask = bpf_cpumask_create();
		if (!mask)
			return -ENOMEM;
		old = bpf_kptr_xchg(&sctx->mask, mask);
		if (old)
			bpf_cpumask_release(old);
	}

	return 0;
}

void BPF_STRUCT_OPS(cohort_exit, struct scx_exit_info *ei)
{
	UEI_RECORD(uei, ei);
}

SCX_OPS_DEFINE(cohort_ops,
	       .select_cpu		= (void *)cohort_select_cpu,
	       .enqueue			= (void *)cohort_enqueue,
	       .dispatch		= (void *)cohort_dispatch,
	       .runnable		= (void *)cohort_runnable,
	       .running			= (void *)cohort_running,
	       .stopping		= (void *)cohort_stopping,
	       .init_task		= (void *)cohort_init_task,
	       .exit_task		= (void *)cohort_exit_task,
	       .init			= (void *)cohort_init,
	       .exit			= (void *)cohort_exit,
	       .name			= "cohort");
