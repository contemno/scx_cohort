// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.
//
// ipc_pingpong: a microbenchmark of the exact failure mode scx_cohort
// exists to fix. Pairs of processes exchange messages through a shared
// memory buffer with futex wakeups — the communication shape of Chrome's
// Mojo IPC and Wine's fsync — and every message write/read walks the
// buffer cache line by cache line, so ownership of those lines ping-pongs
// between the two processes. If the scheduler places the pair on one CCD
// the lines move through the shared L3; if it splits them across CCDs
// every line crosses the Infinity Fabric and round-trip latency roughly
// triples. The -b flag adds background CPU load, which is what provokes
// the default scheduler's machine-wide balancing into splitting pairs.
//
// Output: human-readable "# ..." lines plus machine-parseable
// "RESULT <metric> <value>" lines consumed by run-suite.sh.
//
// Build: cc -O2 -o ipc_pingpong ipc_pingpong.c   (no dependencies)

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <signal.h>
#include <time.h>
#include <unistd.h>

#define CACHE_LINE 64
// 512Ki samples * 4 bytes = 2 MiB per pair; iterations beyond the cap are
// subsampled evenly rather than truncated.
#define MAX_SAMPLES (512 * 1024)

static long futex(_Atomic uint32_t *uaddr, int op, uint32_t val)
{
	// The buffer is MAP_SHARED across fork(), i.e. distinct mms, so the
	// futexes must be shared (no FUTEX_PRIVATE_FLAG).
	return syscall(SYS_futex, uaddr, op, val, NULL, NULL, 0);
}

static void futex_wake(_Atomic uint32_t *f)
{
	futex(f, FUTEX_WAKE, 1);
}

static void futex_wait_until(_Atomic uint32_t *f, uint32_t want)
{
	uint32_t cur;

	while ((cur = atomic_load_explicit(f, memory_order_acquire)) != want)
		futex(f, FUTEX_WAIT, cur);
}

static uint64_t now_ns(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

struct channel {
	_Atomic uint32_t ping __attribute__((aligned(CACHE_LINE)));
	_Atomic uint32_t pong __attribute__((aligned(CACHE_LINE)));
	char payload[] __attribute__((aligned(CACHE_LINE)));
};

struct pair_result {
	uint64_t nsamples;
	uint64_t elapsed_ns;
	uint64_t iters;
	uint32_t samples[MAX_SAMPLES];
};

struct opts {
	int pairs;
	long iters;
	long warmup;
	long payload;
	int background;
};

// Touch every cache line of the payload so its ownership follows the
// message. The volatile reads/writes stop the compiler from eliding the
// walk; the checksum return keeps the reads live.
static uint64_t payload_write(char *buf, long len, uint64_t stamp)
{
	for (long off = 0; off < len; off += CACHE_LINE)
		*(volatile uint64_t *)(buf + off) = stamp + off;
	return stamp;
}

static uint64_t payload_read(const char *buf, long len)
{
	uint64_t sum = 0;

	for (long off = 0; off < len; off += CACHE_LINE)
		sum += *(volatile const uint64_t *)(buf + off);
	return sum;
}

static void die(const char *msg)
{
	perror(msg);
	exit(1);
}

static void child_setup(void)
{
	// Die with the parent instead of lingering as a spinning orphan.
	prctl(PR_SET_PDEATHSIG, SIGKILL);
	if (getppid() == 1)
		exit(0);
}

static void run_server(struct channel *ch, const struct opts *o)
{
	long total = o->warmup + o->iters;
	uint64_t sink = 0;

	for (long i = 1; i <= total; i++) {
		futex_wait_until(&ch->ping, (uint32_t)i);
		sink += payload_read(ch->payload, o->payload);
		payload_write(ch->payload, o->payload, ~(uint64_t)i);
		atomic_store_explicit(&ch->pong, (uint32_t)i,
				      memory_order_release);
		futex_wake(&ch->pong);
	}
	// Keep `sink` observable so the reads cannot be optimized out.
	if (sink == 0xdeadbeef)
		fputc(0, stderr);
}

static void run_client(struct channel *ch, struct pair_result *res,
		       const struct opts *o)
{
	// Subsample evenly when iters exceeds the sample buffer.
	long stride = (o->iters + MAX_SAMPLES - 1) / MAX_SAMPLES;
	uint64_t sink = 0, start = 0;

	for (long i = 1; i <= o->warmup + o->iters; i++) {
		int measured = i > o->warmup;
		uint64_t t0 = 0;

		if (i == o->warmup + 1)
			start = now_ns();
		if (measured)
			t0 = now_ns();

		payload_write(ch->payload, o->payload, (uint64_t)i);
		atomic_store_explicit(&ch->ping, (uint32_t)i,
				      memory_order_release);
		futex_wake(&ch->ping);
		futex_wait_until(&ch->pong, (uint32_t)i);
		sink += payload_read(ch->payload, o->payload);

		if (measured) {
			uint64_t dt = now_ns() - t0;
			long k = i - o->warmup - 1;

			if (k % stride == 0 && res->nsamples < MAX_SAMPLES)
				res->samples[res->nsamples++] =
					dt > UINT32_MAX ? UINT32_MAX
							: (uint32_t)dt;
		}
	}
	res->elapsed_ns = now_ns() - start;
	res->iters = o->iters;
	if (sink == 0xdeadbeef)
		fputc(0, stderr);
}

static int cmp_u32(const void *a, const void *b)
{
	uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;

	return x < y ? -1 : x > y;
}

static uint32_t percentile(const uint32_t *sorted, size_t n, double p)
{
	size_t idx = (size_t)(p / 100.0 * (double)(n - 1));

	return sorted[idx];
}

static void usage(const char *argv0)
{
	fprintf(stderr,
		"usage: %s [-p pairs] [-n iters] [-w warmup] [-s payload_bytes] [-b background_spinners]\n"
		"  -p  communicating process pairs (default 1)\n"
		"  -n  measured round trips per pair (default 200000)\n"
		"  -w  warmup round trips per pair (default 5000)\n"
		"  -s  message payload bytes, walked line-by-line (default 4096)\n"
		"  -b  background busy-loop processes to load the machine (default 0)\n",
		argv0);
	exit(2);
}

int main(int argc, char **argv)
{
	struct opts o = {
		.pairs = 1,
		.iters = 200000,
		.warmup = 5000,
		.payload = 4096,
		.background = 0,
	};
	int c;

	while ((c = getopt(argc, argv, "p:n:w:s:b:h")) != -1) {
		switch (c) {
		case 'p': o.pairs = atoi(optarg); break;
		case 'n': o.iters = atol(optarg); break;
		case 'w': o.warmup = atol(optarg); break;
		case 's': o.payload = atol(optarg); break;
		case 'b': o.background = atoi(optarg); break;
		default: usage(argv[0]);
		}
	}
	if (o.pairs < 1 || o.iters < 1 || o.warmup < 0 || o.payload < CACHE_LINE ||
	    o.background < 0)
		usage(argv[0]);
	o.payload = (o.payload + CACHE_LINE - 1) & ~(long)(CACHE_LINE - 1);

	size_t ch_sz = (sizeof(struct channel) + (size_t)o.payload +
			CACHE_LINE - 1) & ~(size_t)(CACHE_LINE - 1);
	struct channel *chs = mmap(NULL, ch_sz * o.pairs,
				   PROT_READ | PROT_WRITE,
				   MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	struct pair_result *results = mmap(NULL,
					   sizeof(struct pair_result) * o.pairs,
					   PROT_READ | PROT_WRITE,
					   MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	if (chs == MAP_FAILED || results == MAP_FAILED)
		die("mmap");

	pid_t *spinners = calloc(o.background, sizeof(pid_t));
	pid_t *workers = calloc((size_t)o.pairs * 2, sizeof(pid_t));
	int nworkers = 0;

	for (int i = 0; i < o.background; i++) {
		pid_t pid = fork();

		if (pid < 0)
			die("fork");
		if (pid == 0) {
			child_setup();
			for (volatile uint64_t x = 0;; x++)
				;
		}
		spinners[i] = pid;
	}

	for (int i = 0; i < o.pairs; i++) {
		struct channel *ch = (struct channel *)((char *)chs + ch_sz * i);
		pid_t pid = fork();

		if (pid < 0)
			die("fork");
		if (pid == 0) {
			child_setup();
			run_server(ch, &o);
			exit(0);
		}
		workers[nworkers++] = pid;

		pid = fork();
		if (pid < 0)
			die("fork");
		if (pid == 0) {
			child_setup();
			run_client(ch, &results[i], &o);
			exit(0);
		}
		workers[nworkers++] = pid;
	}

	int failed = 0;
	for (int i = 0; i < nworkers; i++) {
		int status;

		if (waitpid(workers[i], &status, 0) < 0)
			die("waitpid");
		if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
			failed = 1;
	}
	for (int i = 0; i < o.background; i++)
		kill(spinners[i], SIGKILL);
	for (int i = 0; i < o.background; i++)
		waitpid(spinners[i], NULL, 0);
	if (failed) {
		fprintf(stderr, "a worker process failed\n");
		return 1;
	}

	size_t total = 0;
	for (int i = 0; i < o.pairs; i++)
		total += results[i].nsamples;
	uint32_t *all = malloc(total * sizeof(uint32_t));
	if (!all)
		die("malloc");
	size_t k = 0;
	double tput = 0.0, mean = 0.0;
	for (int i = 0; i < o.pairs; i++) {
		memcpy(all + k, results[i].samples,
		       results[i].nsamples * sizeof(uint32_t));
		k += results[i].nsamples;
		if (results[i].elapsed_ns)
			tput += (double)results[i].iters * 1e9 /
				(double)results[i].elapsed_ns;
	}
	qsort(all, total, sizeof(uint32_t), cmp_u32);
	for (size_t i = 0; i < total; i++)
		mean += all[i];
	mean /= (double)total;

	printf("# ipc_pingpong pairs=%d iters=%ld warmup=%ld payload=%ld background=%d samples=%zu\n",
	       o.pairs, o.iters, o.warmup, o.payload, o.background, total);
	printf("RESULT rtt_p50_ns %u\n", percentile(all, total, 50.0));
	printf("RESULT rtt_p90_ns %u\n", percentile(all, total, 90.0));
	printf("RESULT rtt_p99_ns %u\n", percentile(all, total, 99.0));
	printf("RESULT rtt_p999_ns %u\n", percentile(all, total, 99.9));
	printf("RESULT rtt_max_ns %u\n", all[total - 1]);
	printf("RESULT rtt_mean_ns %.0f\n", mean);
	printf("RESULT throughput_msgs_per_sec %.0f\n", tput);
	return 0;
}
