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
 * state here.
 *
 * Phase 1: single global vtime DSQ, default idle CPU selection. The
 * cohort/CCD machinery arrives in later phases.
 */
#include <scx/common.bpf.h>
#include "intf.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

#define DSQ_GLOBAL	0

/* Set by userspace between open and load. */
const volatile u64 slice_ns = 5000000ULL;

static u64 vtime_now;

static inline bool vtime_before(u64 a, u64 b)
{
	return (s64)(a - b) < 0;
}

struct {
	__uint(type, BPF_MAP_TYPE_TASK_STORAGE);
	__uint(map_flags, BPF_F_NO_PREALLOC);
	__type(key, int);
	__type(value, struct TaskCtx);
} task_ctxs SEC(".maps");

static struct TaskCtx *lookup_task_ctx(struct task_struct *p)
{
	return bpf_task_storage_get(&task_ctxs, p, 0, 0);
}

s32 BPF_STRUCT_OPS(cohort_select_cpu, struct task_struct *p, s32 prev_cpu,
		   u64 wake_flags)
{
	bool is_idle = false;
	s32 cpu;

	cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);
	if (is_idle)
		scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, slice_ns, 0);

	return cpu;
}

void BPF_STRUCT_OPS(cohort_enqueue, struct task_struct *p, u64 enq_flags)
{
	struct TaskCtx *tctx = lookup_task_ctx(p);
	u64 vtime = tctx ? tctx->vtime : vtime_now;

	/*
	 * Bound how much vtime credit a long sleeper can accumulate: one
	 * slice behind the frontier at most.
	 */
	if (vtime_before(vtime, vtime_now - slice_ns))
		vtime = vtime_now - slice_ns;

	scx_bpf_dsq_insert_vtime(p, DSQ_GLOBAL, slice_ns, vtime, enq_flags);
}

void BPF_STRUCT_OPS(cohort_dispatch, s32 cpu, struct task_struct *prev)
{
	scx_bpf_dsq_move_to_local(DSQ_GLOBAL, 0);
}

void BPF_STRUCT_OPS(cohort_running, struct task_struct *p)
{
	struct TaskCtx *tctx = lookup_task_ctx(p);

	if (!tctx)
		return;

	tctx->last_run_at = scx_bpf_now();
	if (vtime_before(vtime_now, tctx->vtime))
		vtime_now = tctx->vtime;
}

void BPF_STRUCT_OPS(cohort_stopping, struct task_struct *p, bool runnable)
{
	struct TaskCtx *tctx = lookup_task_ctx(p);
	u64 delta;

	if (!tctx)
		return;

	delta = scx_bpf_now() - tctx->last_run_at;
	tctx->vtime += delta * 100 / p->scx.weight;
}

s32 BPF_STRUCT_OPS(cohort_init_task, struct task_struct *p,
		   struct scx_init_task_args *args)
{
	struct TaskCtx *tctx;

	tctx = bpf_task_storage_get(&task_ctxs, p, 0,
				    BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (!tctx)
		return -ENOMEM;

	tctx->vtime = vtime_now;

	return 0;
}

s32 BPF_STRUCT_OPS_SLEEPABLE(cohort_init)
{
	return scx_bpf_create_dsq(DSQ_GLOBAL, -1);
}

void BPF_STRUCT_OPS(cohort_exit, struct scx_exit_info *ei)
{
	UEI_RECORD(uei, ei);
}

SCX_OPS_DEFINE(cohort_ops,
	       .select_cpu		= (void *)cohort_select_cpu,
	       .enqueue			= (void *)cohort_enqueue,
	       .dispatch		= (void *)cohort_dispatch,
	       .running			= (void *)cohort_running,
	       .stopping		= (void *)cohort_stopping,
	       .init_task		= (void *)cohort_init_task,
	       .init			= (void *)cohort_init,
	       .exit			= (void *)cohort_exit,
	       .name			= "cohort");
