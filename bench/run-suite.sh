#!/usr/bin/env bash
# Copyright (c) scx_cohort authors.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.
#
# A/B benchmark harness: runs a workload suite under each configured
# scheduler (EEVDF baseline, scx_cohort, and optionally other scx
# schedulers), interleaved across rounds to decorrelate thermal and
# background drift, and writes one CSV that bench/analyze.py turns into a
# comparison with significance tests.
#
#   sudo bench/run-suite.sh                          # eevdf vs scx_cohort
#   sudo bench/run-suite.sh --rounds 10 --workloads pingpong_loaded
#   sudo bench/run-suite.sh --sched lavd=/usr/bin/scx_lavd
#   python3 bench/analyze.py bench/results/<ts>/results.csv

set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$BENCH_DIR")"
SCX_SYSFS=/sys/kernel/sched_ext

ROUNDS=5
COOLDOWN=3
QUICK=0
ENV_SETUP=1
USE_PERF=auto
USE_CCD=auto
RESULTS_ROOT="$BENCH_DIR/results"
WORKLOADS="pingpong,pingpong_loaded,pingpong_real,schbench,schbench_loaded,hackbench"
declare -A SCHED_CMDS=(
	[eevdf]=""
	[cohort]="$REPO_ROOT/target/release/scx_cohort"
)
SCHED_ORDER=(eevdf cohort)
declare -A CUSTOM_WORKLOADS=()

SCHED_PID=""
MONITOR_PID=""
CCD_PID=""
CCD_PROG_FILE=""
CCD_COUNT=0
declare -A SAVED_GOVERNORS=()

log()  { echo "[$(date +%H:%M:%S)] $*" >&2; }
die()  { log "FATAL: $*"; exit 1; }

usage() {
	cat >&2 <<EOF
usage: sudo $0 [options]
  --rounds N           rounds per scheduler (default $ROUNDS; use >=5 for stats)
  --workloads a,b,c    subset of: pingpong pingpong_loaded pingpong_real
                       pingpong_spill schbench schbench_loaded hackbench
                       plus any --custom names (default: all available)
  --sched name=cmd     add/override a scheduler (e.g. lavd=/usr/bin/scx_lavd);
                       'eevdf' is the empty command. Repeatable.
  --schedulers a,b     which schedulers to run (default: eevdf,cohort)
  --custom name:cmd    add a custom workload; if its stdout has no
                       "RESULT <metric> <value>" lines, wall time is recorded
                       as elapsed_sec. Repeatable.
  --cooldown SEC       idle seconds between runs (default $COOLDOWN)
  --results DIR        results root (default bench/results)
  --no-env-setup       don't pin the cpufreq governor to performance
  --no-perf            don't wrap runs in "perf stat" migration counting
  --no-ccd             don't classify migrations by CCD with bpftrace
  --quick              tiny workload sizes, for smoke-testing the harness
EOF
	exit 2
}

while [[ $# -gt 0 ]]; do
	case "$1" in
	--rounds) ROUNDS="$2"; shift 2 ;;
	--workloads) WORKLOADS="$2"; shift 2 ;;
	--sched)
		[[ "$2" == *=* ]] || die "--sched needs name=command"
		name="${2%%=*}"
		SCHED_CMDS[$name]="${2#*=}"
		shift 2 ;;
	--schedulers)
		IFS=',' read -r -a SCHED_ORDER <<< "$2"; shift 2 ;;
	--custom)
		[[ "$2" == *:* ]] || die "--custom needs name:command"
		CUSTOM_WORKLOADS["${2%%:*}"]="${2#*:}"
		shift 2 ;;
	--cooldown) COOLDOWN="$2"; shift 2 ;;
	--results) RESULTS_ROOT="$2"; shift 2 ;;
	--no-env-setup) ENV_SETUP=0; shift ;;
	--no-perf) USE_PERF=0; shift ;;
	--no-ccd) USE_CCD=0; shift ;;
	--quick) QUICK=1; shift ;;
	-h|--help) usage ;;
	*) die "unknown option $1 (see --help)" ;;
	esac
done

for s in "${SCHED_ORDER[@]}"; do
	[[ -v "SCHED_CMDS[$s]" ]] || die "scheduler '$s' has no command; add --sched $s=..."
done

# ---------------------------------------------------------------- preflight

need_root=0
for s in "${SCHED_ORDER[@]}"; do
	[[ "$s" != eevdf ]] && need_root=1
done
if (( need_root )); then
	[[ $EUID -eq 0 ]] || die "attaching a sched_ext scheduler needs root"
	[[ -d $SCX_SYSFS ]] || die "no $SCX_SYSFS: kernel lacks CONFIG_SCHED_CLASS_EXT"
fi

if [[ -v "SCHED_CMDS[cohort]" ]]; then
	for s in "${SCHED_ORDER[@]}"; do
		if [[ "$s" == cohort && ! -x "${SCHED_CMDS[cohort]%% *}" ]]; then
			die "scx_cohort binary missing; run: cargo build --release"
		fi
	done
fi

have() { command -v "$1" >/dev/null 2>&1; }

# The microbenchmark is a workspace crate; a no-op rebuild is instant.
PINGPONG="$REPO_ROOT/target/release/ipc_pingpong"
if have cargo; then
	cargo build --release -q -p ipc_pingpong \
		--manifest-path "$REPO_ROOT/Cargo.toml" || die "cargo build failed"
fi
[[ -x $PINGPONG ]] || die "ipc_pingpong missing; run: cargo build --release -p ipc_pingpong"

# A raw migration count can't tell a cheap hop between two cores of one
# CCD from the fabric crossing this scheduler exists to avoid, and under
# sched_ext the two schedulers don't even pay the same price per event.
# Classify instead: bin every sched_migrate_task by whether it stayed
# inside one last-level cache. The CPU->LLC map comes from sysfs, so no
# topology is hardcoded.
llc_list_for_cpu() { # cpu -> "0-7,16-23"
	local cpu="$1" d
	for d in /sys/devices/system/cpu/cpu"$cpu"/cache/index*; do
		[[ -r $d/level && -r $d/shared_cpu_list ]] || continue
		[[ $(<"$d/level") == 3 ]] || continue
		cat "$d/shared_cpu_list"
		return 0
	done
	return 1
}

build_ccd_prog() {
	local d cpu list n=0
	local -A ids=()
	CCD_PROG_FILE=$(mktemp) || return 1
	{
		echo "BEGIN {"
		for d in /sys/devices/system/cpu/cpu[0-9]*; do
			cpu="${d##*/cpu}"
			list=$(llc_list_for_cpu "$cpu") || return 1
			[[ -n $list ]] || return 1
			if [[ -z ${ids[$list]+set} ]]; then
				ids[$list]=$n
				n=$((n + 1))
			fi
			echo "	@ccd[$cpu] = ${ids[$list]};"
		done
		echo '	printf("ready\n");'
		echo "}"
		cat <<-'EOF'
		tracepoint:sched:sched_migrate_task {
			$s = @ccd[args->orig_cpu];
			$d = @ccd[args->dest_cpu];
			@mig[$s == $d ? "same" : "cross"] = count();
		}
		END { clear(@ccd); }
		EOF
	} > "$CCD_PROG_FILE"
	CCD_COUNT=$n
	(( n > 1 ))   # a single-LLC machine has nothing to classify
}

start_ccd_trace() { # outfile
	local out="$1" i
	bpftrace "$CCD_PROG_FILE" > "$out" 2>&1 &
	CCD_PID=$!
	for i in $(seq 100); do
		grep -q '^ready$' "$out" 2>/dev/null && return 0
		kill -0 "$CCD_PID" 2>/dev/null || { CCD_PID=""; return 1; }
		sleep 0.1
	done
	stop_ccd_trace
	return 1
}

stop_ccd_trace() {
	[[ -n $CCD_PID ]] || return 0
	# SIGINT is what makes bpftrace print its maps and exit cleanly.
	kill -INT "$CCD_PID" 2>/dev/null || true
	wait "$CCD_PID" 2>/dev/null || true
	CCD_PID=""
}

if [[ $USE_CCD == auto ]]; then
	# Dry-run the generated program rather than a generic probe, so a bad
	# topology map fails here instead of halfway through a suite.
	if have bpftrace && [[ $EUID -eq 0 ]] && build_ccd_prog && \
	   bpftrace -d "$CCD_PROG_FILE" >/dev/null 2>&1; then
		USE_CCD=1
		log "migrations classified per CCD ($CCD_COUNT LLC domains)"
	else
		USE_CCD=0
		log "SKIP per-CCD migration classification (needs root + bpftrace + >1 LLC)"
	fi
fi

if [[ $USE_PERF == auto ]]; then
	if (( USE_CCD )); then
		# bpftrace already totals the same tracepoint; counting it twice
		# would only add overhead to both schedulers.
		USE_PERF=0
	elif have perf && [[ $EUID -eq 0 ]] && \
	   perf stat -a -e sched:sched_migrate_task -- true >/dev/null 2>&1; then
		USE_PERF=1
	else
		USE_PERF=0
	fi
fi

# Filter requested workloads down to what is runnable, loudly.
AVAILABLE=()
IFS=',' read -r -a requested <<< "$WORKLOADS"
for w in "${requested[@]}" "${!CUSTOM_WORKLOADS[@]}"; do
	case "$w" in
	pingpong|pingpong_loaded|pingpong_real|pingpong_spill) AVAILABLE+=("$w") ;;
	schbench)
		if have schbench; then AVAILABLE+=("$w")
		else log "SKIP schbench: not installed"; fi ;;
	schbench_loaded)
		if have schbench && have hackbench; then AVAILABLE+=("$w")
		else log "SKIP schbench_loaded: needs both schbench and hackbench"; fi ;;
	hackbench)
		if have hackbench || have perf; then AVAILABLE+=("$w")
		else log "SKIP hackbench: neither hackbench nor perf installed"; fi ;;
	*)
		if [[ -v "CUSTOM_WORKLOADS[$w]" ]]; then AVAILABLE+=("$w")
		else die "unknown workload '$w'"; fi ;;
	esac
done
# De-duplicate, preserving order.
declare -A seen=()
RUN_WORKLOADS=()
for w in "${AVAILABLE[@]}"; do
	[[ -v "seen[$w]" ]] || RUN_WORKLOADS+=("$w")
	seen[$w]=1
done
[[ ${#RUN_WORKLOADS[@]} -gt 0 ]] || die "no runnable workloads"

# ------------------------------------------------------------- environment

set_governors() {
	local cpu gov
	for cpu in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_governor; do
		[[ -w $cpu ]] || return 0
		gov=$(<"$cpu")
		SAVED_GOVERNORS[$cpu]="$gov"
		echo performance > "$cpu" 2>/dev/null || true
	done
	[[ ${#SAVED_GOVERNORS[@]} -gt 0 ]] && log "cpufreq governor pinned to performance"
}

restore_governors() {
	local cpu
	for cpu in "${!SAVED_GOVERNORS[@]}"; do
		echo "${SAVED_GOVERNORS[$cpu]}" > "$cpu" 2>/dev/null || true
	done
}

scx_state() {
	[[ -r $SCX_SYSFS/state ]] && cat "$SCX_SYSFS/state" || echo disabled
}

stop_sched() {
	if [[ -n $MONITOR_PID ]]; then
		kill "$MONITOR_PID" 2>/dev/null || true
		wait "$MONITOR_PID" 2>/dev/null || true
		MONITOR_PID=""
	fi
	if [[ -n $SCHED_PID ]]; then
		kill -INT "$SCHED_PID" 2>/dev/null || true
		for _ in $(seq 50); do
			kill -0 "$SCHED_PID" 2>/dev/null || break
			sleep 0.2
		done
		kill -KILL "$SCHED_PID" 2>/dev/null || true
		wait "$SCHED_PID" 2>/dev/null || true
		SCHED_PID=""
	fi
}

start_sched() {
	local name="$1" logfile="$2"
	local cmd="${SCHED_CMDS[$name]}"

	if [[ -z $cmd ]]; then
		[[ $(scx_state) == disabled ]] || \
			die "a sched_ext scheduler is already attached; detach it for the $name baseline"
		return
	fi

	[[ $(scx_state) == disabled ]] || die "another sched_ext scheduler is attached"
	# shellcheck disable=SC2086
	$cmd > "$logfile" 2>&1 &
	SCHED_PID=$!
	for _ in $(seq 100); do
		[[ $(scx_state) == enabled ]] && break
		kill -0 "$SCHED_PID" 2>/dev/null || \
			{ tail -5 "$logfile" >&2; die "$name exited during attach"; }
		sleep 0.1
	done
	[[ $(scx_state) == enabled ]] || { tail -5 "$logfile" >&2; die "$name failed to attach"; }
	log "  attached: $(cat "$SCX_SYSFS/root/ops" 2>/dev/null || echo '?')"

	# scx_cohort serves its own metrics; sample them during the run so
	# the CSV records the affinity the scheduler achieved.
	if [[ $name == cohort ]]; then
		"${cmd%% *}" --monitor 1 > "${logfile%.log}.monitor" 2>/dev/null &
		MONITOR_PID=$!
	fi
}

cleanup() {
	stop_ccd_trace
	stop_sched
	restore_governors
	[[ -n $CCD_PROG_FILE ]] && rm -f "$CCD_PROG_FILE"
	return 0
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------- workloads

# pingpong_real is the one to quote for a realistic claim: the pairs think
# between messages (so they aren't pinned runnable, and the shared payload
# falls out of L1/L2 to L3 where the CCD boundary costs something) and the
# background load execs into its own scheduling group. pingpong_loaded
# keeps the flat-out duty cycle for continuity with older runs.
# pingpong_spill deliberately leaves the loaders forked, so they inherit
# the pairs' lineage: on a lineage-grouping scheduler that oversubscribes
# one cohort far past a CCD, which measures the spill path rather than
# placement. Read its numbers as a stress test, not as a workload.
if (( QUICK )); then
	PP_ARGS=(-p 2 -n 20000 -w 1000)
	PPL_ARGS=(-p 2 -n 20000 -w 1000 -b 4)
	PPR_ARGS=(-p 2 -n 4000 -w 500 -t 50 -S 1024 -b 4)
	PPS_ARGS=(-p 2 -n 20000 -w 1000 -b 4 -f)
	SCHBENCH_ARGS=(-m 2 -t 4 -r 5)
	HACKBENCH_LOOPS=1000
else
	PP_ARGS=(-p 4 -n 300000 -w 10000)
	PPL_ARGS=(-p 4 -n 300000 -w 10000 -b "$(nproc)")
	PPR_ARGS=(-p 4 -n 60000 -w 5000 -t 50 -S 1024 -b "$(nproc)")
	PPS_ARGS=(-p 4 -n 300000 -w 10000 -b "$(nproc)" -f)
	SCHBENCH_ARGS=(-m 2 -t 8 -r 30)
	HACKBENCH_LOOPS=20000
fi

# schbench under fork-heavy pressure: wakeup latency measured while the
# machine is saturated, which is the interactive-under-load case the design
# exists for and the one nothing else here covers. Every other workload
# measures throughput or latency alone; this measures latency *during*
# throughput, and it is where a desktop stall shows up as a number instead
# of as a feeling. hackbench loops so the pressure outlasts the
# measurement, its output is discarded, and only schbench's percentiles are
# recorded. Both tools are scheduler-neutral.
schbench_loaded_cmd() {
	printf '%s\n' \
		"( while :; do hackbench -g 8 -l $HACKBENCH_LOOPS >/dev/null 2>&1 || break; done ) &" \
		'hb=$!' \
		"schbench ${SCHBENCH_ARGS[*]}" \
		'rc=$?' \
		'kill "$hb" 2>/dev/null || true' \
		'# Killing the loop leaves whichever hackbench it had already' \
		'# started, and a survivor would silently load the *next*' \
		'# workload. Take the tree out by name and wait for it to go.' \
		'pkill -x hackbench 2>/dev/null || true' \
		'for _ in $(seq 100); do' \
		'	pgrep -x hackbench >/dev/null 2>&1 || break' \
		'	sleep 0.1' \
		'done' \
		'wait 2>/dev/null' \
		'exit $rc'
}

emit() { # workload sched round metric value
	echo "$1,$2,$3,$4,$5" >> "$RESULTS_CSV"
}

# Emit every "RESULT <metric> <value>" line from a log.
emit_results() { # workload sched round logfile
	local metric value
	while read -r _ metric value _; do
		emit "$1" "$2" "$3" "$metric" "$value"
	done < <(grep '^RESULT ' "$4" || true)
}

# bpftrace prints "@mig[same]: N" / "@mig[cross]: N" on exit. Absent keys
# mean zero events of that kind, not a failed trace.
emit_ccd_migrations() { # workload sched round elapsed outfile
	local same cross
	same=$(awk -F': *' '/^@mig\[same\]/ { print $2 }' "$5")
	cross=$(awk -F': *' '/^@mig\[cross\]/ { print $2 }' "$5")
	awk -v s="${same:-0}" -v c="${cross:-0}" -v el="$4" \
		'BEGIN { if (el > 0) printf "%.1f %.1f %.1f\n", (s + c) / el, c / el, s / el }' | {
		read -r total cross_rate same_rate || return 0
		[[ -n ${total:-} ]] || return 0
		emit "$1" "$2" "$3" migrations_per_sec "$total"
		emit "$1" "$2" "$3" cross_ccd_migrations_per_sec "$cross_rate"
		emit "$1" "$2" "$3" same_ccd_migrations_per_sec "$same_rate"
	}
}

parse_schbench() { # workload sched round logfile
	# Newer schbench prints several percentile sections; keep the wakeup
	# latencies. Older versions print one "Latency percentiles" block.
	# POSIX awk only (Ubuntu's default awk is mawk).
	awk '
		/percentiles/ {
			line = tolower($0)
			sec = (line ~ /wakeup/)  ? "wakeup" : \
			      (line ~ /request/) ? "request" : \
			      (line ~ /rps/)     ? "rps" : "latency"
		}
		sec == "wakeup" || sec == "latency" {
			for (i = 1; i < NF; i++) {
				f = $i
				sub(/^\*/, "", f)
				if (f !~ /^[0-9]+\.[0-9]+th:$/)
					continue
				sub(/th:$/, "", f)
				p = f + 0
				if (p == 50)   print "wakeup_p50_usec", $(i + 1)
				if (p == 99)   print "wakeup_p99_usec", $(i + 1)
				if (p == 99.9) print "wakeup_p999_usec", $(i + 1)
			}
		}
	' "$4" | while read -r metric value; do
		emit "$1" "$2" "$3" "$metric" "$value"
	done
}

run_workload() { # workload sched round rundir
	local w="$1" s="$2" r="$3" dir="$4"
	local logfile="$dir/$w.log" cmd=() t0 t1 elapsed
	local perf_out="$dir/$w.perf" ccd_out="$dir/$w.ccd"

	case "$w" in
	pingpong)        cmd=("$PINGPONG" "${PP_ARGS[@]}") ;;
	pingpong_loaded) cmd=("$PINGPONG" "${PPL_ARGS[@]}") ;;
	pingpong_real)   cmd=("$PINGPONG" "${PPR_ARGS[@]}") ;;
	pingpong_spill)  cmd=("$PINGPONG" "${PPS_ARGS[@]}") ;;
	schbench)        cmd=(schbench "${SCHBENCH_ARGS[@]}") ;;
	schbench_loaded) cmd=(bash -c "$(schbench_loaded_cmd)") ;;
	hackbench)
		if have hackbench; then
			cmd=(hackbench -g 8 -l "$HACKBENCH_LOOPS")
		else
			cmd=(perf bench sched messaging -g 8 -l "$HACKBENCH_LOOPS")
		fi ;;
	*)               cmd=(bash -c "${CUSTOM_WORKLOADS[$w]}") ;;
	esac

	log "  $w"
	if (( USE_CCD )); then
		start_ccd_trace "$ccd_out" || die "bpftrace migration trace failed to start"
	fi
	t0=$(date +%s.%N)
	if (( USE_PERF )); then
		perf stat -a -e sched:sched_migrate_task -x, -o "$perf_out" \
			-- "${cmd[@]}" > "$logfile" 2>&1 || \
			die "$w failed under $s (see $logfile)"
	else
		"${cmd[@]}" > "$logfile" 2>&1 || \
			die "$w failed under $s (see $logfile)"
	fi
	t1=$(date +%s.%N)
	if (( USE_CCD )); then
		stop_ccd_trace
	fi
	elapsed=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.3f", b - a }')
	emit "$w" "$s" "$r" elapsed_sec "$elapsed"

	case "$w" in
	pingpong|pingpong_loaded|pingpong_real|pingpong_spill)
		emit_results "$w" "$s" "$r" "$logfile" ;;
	schbench|schbench_loaded) parse_schbench "$w" "$s" "$r" "$logfile" ;;
	hackbench)
		awk '/^Time:/ { print $2; exit }
		     /Total time:/ { print $(NF - 1); exit }' "$logfile" | {
			read -r t || true
			[[ -n ${t:-} ]] && emit "$w" "$s" "$r" time_sec "$t"
		} ;;
	*)                        emit_results "$w" "$s" "$r" "$logfile" ;;
	esac

	if (( USE_CCD )) && [[ -s $ccd_out ]]; then
		emit_ccd_migrations "$w" "$s" "$r" "$elapsed" "$ccd_out"
	fi

	if (( USE_PERF )) && [[ -s $perf_out ]]; then
		awk -F, -v el="$elapsed" '
			$3 == "sched:sched_migrate_task" && $1 ~ /^[0-9]+$/ {
				printf "%.1f\n", $1 / el; exit
			}' "$perf_out" | {
			read -r m || true
			[[ -n ${m:-} ]] && emit "$w" "$s" "$r" migrations_per_sec "$m"
		}
	fi
}

# ------------------------------------------------------------------ main

TS=$(date +%Y%m%d-%H%M%S)
RUN_DIR="$RESULTS_ROOT/$TS"
RESULTS_CSV="$RUN_DIR/results.csv"
mkdir -p "$RUN_DIR"
echo "workload,scheduler,round,metric,value" > "$RESULTS_CSV"

{
	echo "date: $(date -u +%FT%TZ)"
	echo "kernel: $(uname -r)"
	echo "cpu: $(awk -F: '/model name/ { print $2; exit }' /proc/cpuinfo)"
	echo "nproc: $(nproc)"
	echo "git: $(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo '?')"
	echo "boost: $(cat /sys/devices/system/cpu/cpufreq/boost 2>/dev/null || echo 'n/a')"
	echo "amd_pstate: $(cat /sys/devices/system/cpu/amd_pstate/status 2>/dev/null || echo 'n/a')"
	echo "governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo 'n/a')"
	echo "schedulers: ${SCHED_ORDER[*]}"
	echo "workloads: ${RUN_WORKLOADS[*]}"
	echo "rounds: $ROUNDS  quick: $QUICK  perf: $USE_PERF"
	echo "llc_domains: $CCD_COUNT"
	if (( USE_CCD )); then
		echo "migration_counter: bpftrace (classified per CCD)"
	elif (( USE_PERF )); then
		echo "migration_counter: perf stat (total only, not CCD-classified)"
	else
		echo "migration_counter: none"
	fi
} > "$RUN_DIR/sysinfo.txt"

if (( ENV_SETUP )); then
	set_governors
fi
log "results -> $RUN_DIR"

for round in $(seq 1 "$ROUNDS"); do
	# ABBA ordering: reverse the scheduler order on even rounds so slow
	# drift (thermals, background daemons) cancels instead of biasing
	# whichever scheduler always runs second.
	order=("${SCHED_ORDER[@]}")
	if (( round % 2 == 0 )); then
		rev=()
		for ((i = ${#order[@]} - 1; i >= 0; i--)); do rev+=("${order[i]}"); done
		order=("${rev[@]}")
	fi

	for s in "${order[@]}"; do
		log "round $round/$ROUNDS: scheduler=$s"
		dir="$RUN_DIR/r$round-$s"
		mkdir -p "$dir"
		start_sched "$s" "$dir/scheduler.log"
		sleep 1   # let cohort discovery settle before measuring
		for w in "${RUN_WORKLOADS[@]}"; do
			run_workload "$w" "$s" "$round" "$dir"
		done
		stop_sched
		if [[ -s "$dir/scheduler.monitor" ]]; then
			for metric in affinity plan; do
				awk -v key="$metric" '{
					if (match($0, key "=[ ]*[0-9.]+%")) {
						v = substr($0, RSTART, RLENGTH)
						gsub(/[^0-9.]/, "", v)
						s += v; n++
					}
				     }
				     END { if (n) printf "%.1f\n", s / n }' \
					"$dir/scheduler.monitor" | {
					read -r a || true
					[[ -n ${a:-} ]] && \
						emit _scheduler "$s" "$round" "${metric}_pct" "$a"
				}
			done
		fi
		sleep "$COOLDOWN"
	done
done

log "done: $RESULTS_CSV"
log "analyze with: python3 $BENCH_DIR/analyze.py $RESULTS_CSV"
