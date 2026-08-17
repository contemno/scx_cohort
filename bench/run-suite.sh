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
RESULTS_ROOT="$BENCH_DIR/results"
WORKLOADS="pingpong,pingpong_loaded,schbench,hackbench"
declare -A SCHED_CMDS=(
	[eevdf]=""
	[cohort]="$REPO_ROOT/target/release/scx_cohort"
)
SCHED_ORDER=(eevdf cohort)
declare -A CUSTOM_WORKLOADS=()

SCHED_PID=""
MONITOR_PID=""
declare -A SAVED_GOVERNORS=()

log()  { echo "[$(date +%H:%M:%S)] $*" >&2; }
die()  { log "FATAL: $*"; exit 1; }

usage() {
	cat >&2 <<EOF
usage: sudo $0 [options]
  --rounds N           rounds per scheduler (default $ROUNDS; use >=5 for stats)
  --workloads a,b,c    subset of: pingpong pingpong_loaded schbench hackbench
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

if [[ $USE_PERF == auto ]]; then
	if have perf && [[ $EUID -eq 0 ]] && \
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
	pingpong|pingpong_loaded) AVAILABLE+=("$w") ;;
	schbench)
		if have schbench; then AVAILABLE+=("$w")
		else log "SKIP schbench: not installed"; fi ;;
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
	stop_sched
	restore_governors
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------- workloads

if (( QUICK )); then
	PP_ARGS=(-p 2 -n 20000 -w 1000)
	PPL_ARGS=(-p 2 -n 20000 -w 1000 -b 4)
	SCHBENCH_ARGS=(-m 2 -t 4 -r 5)
	HACKBENCH_LOOPS=1000
else
	PP_ARGS=(-p 4 -n 300000 -w 10000)
	PPL_ARGS=(-p 4 -n 300000 -w 10000 -b "$(nproc)")
	SCHBENCH_ARGS=(-m 2 -t 8 -r 30)
	HACKBENCH_LOOPS=20000
fi

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
	local perf_out="$dir/$w.perf"

	case "$w" in
	pingpong)        cmd=("$PINGPONG" "${PP_ARGS[@]}") ;;
	pingpong_loaded) cmd=("$PINGPONG" "${PPL_ARGS[@]}") ;;
	schbench)        cmd=(schbench "${SCHBENCH_ARGS[@]}") ;;
	hackbench)
		if have hackbench; then
			cmd=(hackbench -g 8 -l "$HACKBENCH_LOOPS")
		else
			cmd=(perf bench sched messaging -g 8 -l "$HACKBENCH_LOOPS")
		fi ;;
	*)               cmd=(bash -c "${CUSTOM_WORKLOADS[$w]}") ;;
	esac

	log "  $w"
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
	elapsed=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.3f", b - a }')
	emit "$w" "$s" "$r" elapsed_sec "$elapsed"

	case "$w" in
	pingpong|pingpong_loaded) emit_results "$w" "$s" "$r" "$logfile" ;;
	schbench)                 parse_schbench "$w" "$s" "$r" "$logfile" ;;
	hackbench)
		awk '/^Time:/ { print $2; exit }
		     /Total time:/ { print $(NF - 1); exit }' "$logfile" | {
			read -r t || true
			[[ -n ${t:-} ]] && emit "$w" "$s" "$r" time_sec "$t"
		} ;;
	*)                        emit_results "$w" "$s" "$r" "$logfile" ;;
	esac

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
} > "$RUN_DIR/sysinfo.txt"

(( ENV_SETUP )) && set_governors
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
			awk '{
				if (match($0, /affinity=[ ]*[0-9.]+%/)) {
					v = substr($0, RSTART, RLENGTH)
					gsub(/[^0-9.]/, "", v)
					s += v; n++
				}
			     }
			     END { if (n) printf "%.1f\n", s / n }' \
				"$dir/scheduler.monitor" | {
				read -r a || true
				[[ -n ${a:-} ]] && emit _scheduler "$s" "$round" affinity_pct "$a"
			}
		fi
		sleep "$COOLDOWN"
	done
done

log "done: $RESULTS_CSV"
log "analyze with: python3 $BENCH_DIR/analyze.py $RESULTS_CSV"
