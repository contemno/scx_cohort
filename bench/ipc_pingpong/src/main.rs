// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! ipc_pingpong: a microbenchmark of the exact failure mode scx_cohort
//! exists to fix. Pairs of processes exchange messages through a shared
//! memory buffer with futex wakeups — the communication shape of Chrome's
//! Mojo IPC and Wine's fsync — and every message write/read walks the
//! buffer cache line by cache line, so ownership of those lines ping-pongs
//! between the two processes. If the scheduler places the pair on one CCD
//! the lines move through the shared L3; if it splits them across CCDs
//! every line crosses the Infinity Fabric and round-trip latency roughly
//! triples. The `-b` flag adds background CPU load, which is what provokes
//! the default scheduler's machine-wide balancing into splitting pairs.
//!
//! Output: human-readable "# ..." lines plus machine-parseable
//! "RESULT <metric> <value>" lines consumed by bench/run-suite.sh.
//!
//! The processes are real fork()ed processes, not threads: cross-process
//! wakeups are the case the cohort design targets, and fork lineage is one
//! of its grouping signals. That is why this lives on libc rather than
//! std's process API — the pairs must share the MAP_SHARED channel mapping
//! by inheritance, exactly like Chrome's zygote or Wine's server.

use std::io::Write as _;
use std::mem::size_of;
use std::process::exit;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

const CACHE_LINE: usize = 64;
/// 512Ki samples * 4 bytes = 2 MiB per pair; iterations beyond the cap are
/// subsampled evenly rather than truncated.
const MAX_SAMPLES: usize = 512 * 1024;

/// One futex word alone on its cache line, so the two wakeup directions
/// don't false-share.
#[repr(C, align(64))]
struct FutexLine(AtomicU32);

/// Channel header; the payload follows it in the same shared mapping.
#[repr(C)]
struct Channel {
    ping: FutexLine,
    pong: FutexLine,
}

#[repr(C)]
struct PairResult {
    nsamples: u64,
    elapsed_ns: u64,
    iters: u64,
    samples: [u32; MAX_SAMPLES],
}

#[derive(Clone, Copy)]
struct Opts {
    pairs: usize,
    iters: u64,
    warmup: u64,
    payload: usize,
    background: usize,
}

fn futex(word: &AtomicU32, op: libc::c_int, val: u32) -> libc::c_long {
    // The channel is MAP_SHARED across fork(), i.e. distinct mms, so the
    // futexes must be shared (no FUTEX_PRIVATE_FLAG).
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            op,
            val,
            ptr::null::<libc::timespec>(),
            ptr::null::<u32>(),
            0u32,
        )
    }
}

fn futex_wake(word: &AtomicU32) {
    futex(word, libc::FUTEX_WAKE, 1);
}

fn futex_wait_until(word: &AtomicU32, want: u32) {
    loop {
        let cur = word.load(Ordering::Acquire);
        if cur == want {
            return;
        }
        futex(word, libc::FUTEX_WAIT, cur);
    }
}

/// Touch every cache line of the payload so its ownership follows the
/// message. Volatile keeps the walk from being elided; the returned sum
/// keeps the reads live.
fn payload_write(buf: *mut u8, len: usize, stamp: u64) {
    let mut off = 0;
    while off < len {
        unsafe { ptr::write_volatile(buf.add(off) as *mut u64, stamp + off as u64) };
        off += CACHE_LINE;
    }
}

fn payload_read(buf: *const u8, len: usize) -> u64 {
    let mut sum = 0u64;
    let mut off = 0;
    while off < len {
        sum = sum.wrapping_add(unsafe { ptr::read_volatile(buf.add(off) as *const u64) });
        off += CACHE_LINE;
    }
    sum
}

fn map_shared(len: usize) -> *mut u8 {
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        die("mmap failed");
    }
    p as *mut u8
}

fn die(msg: &str) -> ! {
    eprintln!("ipc_pingpong: {msg}");
    exit(1);
}

/// Fork; the child dies with the parent instead of lingering as a spinning
/// orphan, and never returns to the caller's stack.
fn spawn(child: impl FnOnce()) -> libc::pid_t {
    // Safety: the parent is single-threaded throughout, so fork() is safe.
    let pid = unsafe { libc::fork() };
    match pid.cmp(&0) {
        std::cmp::Ordering::Less => die("fork failed"),
        std::cmp::Ordering::Equal => {
            unsafe {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                if libc::getppid() == 1 {
                    exit(0);
                }
            }
            child();
            exit(0);
        }
        std::cmp::Ordering::Greater => pid,
    }
}

fn run_server(ch: &Channel, payload: *mut u8, o: &Opts) {
    let total = o.warmup + o.iters;
    let mut sink = 0u64;

    for i in 1..=total {
        futex_wait_until(&ch.ping.0, i as u32);
        sink = sink.wrapping_add(payload_read(payload, o.payload));
        payload_write(payload, o.payload, !i);
        ch.pong.0.store(i as u32, Ordering::Release);
        futex_wake(&ch.pong.0);
    }
    std::hint::black_box(sink);
}

fn run_client(ch: &Channel, payload: *mut u8, res: &mut PairResult, o: &Opts) {
    let stride = sample_stride(o.iters);
    let mut sink = 0u64;
    let mut start = Instant::now();

    for i in 1..=(o.warmup + o.iters) {
        let measured = i > o.warmup;
        if i == o.warmup + 1 {
            start = Instant::now();
        }
        let t0 = if measured { Some(Instant::now()) } else { None };

        payload_write(payload, o.payload, i);
        ch.ping.0.store(i as u32, Ordering::Release);
        futex_wake(&ch.ping.0);
        futex_wait_until(&ch.pong.0, i as u32);
        sink = sink.wrapping_add(payload_read(payload, o.payload));

        if let Some(t0) = t0 {
            let dt = t0.elapsed().as_nanos().min(u32::MAX as u128) as u32;
            let k = i - o.warmup - 1;
            if k.is_multiple_of(stride) && (res.nsamples as usize) < MAX_SAMPLES {
                res.samples[res.nsamples as usize] = dt;
                res.nsamples += 1;
            }
        }
    }
    res.elapsed_ns = start.elapsed().as_nanos() as u64;
    res.iters = o.iters;
    std::hint::black_box(sink);
}

/// Evenly subsample when the iteration count exceeds the sample buffer.
fn sample_stride(iters: u64) -> u64 {
    iters.div_ceil(MAX_SAMPLES as u64)
}

fn percentile(sorted: &[u32], p: f64) -> u32 {
    sorted[(p / 100.0 * (sorted.len() - 1) as f64) as usize]
}

fn usage() -> ! {
    eprintln!(
        "usage: ipc_pingpong [-p pairs] [-n iters] [-w warmup] [-s payload_bytes] [-b background_spinners]\n\
         \x20 -p  communicating process pairs (default 1)\n\
         \x20 -n  measured round trips per pair (default 200000)\n\
         \x20 -w  warmup round trips per pair (default 5000)\n\
         \x20 -s  message payload bytes, walked line-by-line (default 4096)\n\
         \x20 -b  background busy-loop processes to load the machine (default 0)"
    );
    exit(2);
}

fn parse_opts() -> Opts {
    let mut o = Opts {
        pairs: 1,
        iters: 200000,
        warmup: 5000,
        payload: 4096,
        background: 0,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = |flag: &str| -> u64 {
            args.next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| die(&format!("{flag} needs a numeric argument")))
        };
        match flag.as_str() {
            "-p" => o.pairs = val("-p") as usize,
            "-n" => o.iters = val("-n"),
            "-w" => o.warmup = val("-w"),
            "-s" => o.payload = val("-s") as usize,
            "-b" => o.background = val("-b") as usize,
            _ => usage(),
        }
    }
    if o.pairs < 1 || o.iters < 1 || o.payload < CACHE_LINE {
        usage();
    }
    o.payload = o.payload.next_multiple_of(CACHE_LINE);
    o
}

fn main() {
    let o = parse_opts();

    let ch_sz = (size_of::<Channel>() + o.payload).next_multiple_of(CACHE_LINE);
    let chs = map_shared(ch_sz * o.pairs);
    let results = map_shared(size_of::<PairResult>() * o.pairs) as *mut PairResult;

    let spinners: Vec<libc::pid_t> = (0..o.background)
        .map(|_| {
            spawn(|| {
                let mut x = 0u64;
                loop {
                    x = std::hint::black_box(x.wrapping_add(1));
                }
            })
        })
        .collect();

    let mut workers = Vec::with_capacity(o.pairs * 2);
    for i in 0..o.pairs {
        let base = unsafe { chs.add(ch_sz * i) };
        let ch = unsafe { &*(base as *const Channel) };
        let payload = unsafe { base.add(size_of::<Channel>()) };

        workers.push(spawn(|| run_server(ch, payload, &o)));
        workers.push(spawn(|| {
            let res = unsafe { &mut *results.add(i) };
            run_client(ch, payload, res, &o);
        }));
    }

    let mut failed = false;
    for pid in &workers {
        let mut status = 0;
        if unsafe { libc::waitpid(*pid, &mut status, 0) } < 0 {
            die("waitpid failed");
        }
        if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
            failed = true;
        }
    }
    for pid in &spinners {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
            libc::waitpid(*pid, ptr::null_mut(), 0);
        }
    }
    if failed {
        die("a worker process failed");
    }

    let mut all = Vec::new();
    let mut tput = 0.0f64;
    for i in 0..o.pairs {
        let res = unsafe { &*results.add(i) };
        all.extend_from_slice(&res.samples[..res.nsamples as usize]);
        if res.elapsed_ns > 0 {
            tput += res.iters as f64 * 1e9 / res.elapsed_ns as f64;
        }
    }
    all.sort_unstable();
    let mean = all.iter().map(|&x| x as f64).sum::<f64>() / all.len() as f64;

    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "# ipc_pingpong pairs={} iters={} warmup={} payload={} background={} samples={}",
        o.pairs,
        o.iters,
        o.warmup,
        o.payload,
        o.background,
        all.len()
    );
    let _ = writeln!(out, "RESULT rtt_p50_ns {}", percentile(&all, 50.0));
    let _ = writeln!(out, "RESULT rtt_p90_ns {}", percentile(&all, 90.0));
    let _ = writeln!(out, "RESULT rtt_p99_ns {}", percentile(&all, 99.0));
    let _ = writeln!(out, "RESULT rtt_p999_ns {}", percentile(&all, 99.9));
    let _ = writeln!(out, "RESULT rtt_max_ns {}", all[all.len() - 1]);
    let _ = writeln!(out, "RESULT rtt_mean_ns {mean:.0}");
    let _ = writeln!(out, "RESULT throughput_msgs_per_sec {tput:.0}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_index_the_sorted_tail() {
        let v: Vec<u32> = (1..=1000).collect();
        assert_eq!(percentile(&v, 50.0), 500);
        assert_eq!(percentile(&v, 99.0), 990);
        assert_eq!(percentile(&v, 99.9), 999);
        assert_eq!(percentile(&[42], 99.9), 42);
    }

    #[test]
    fn stride_subsamples_into_the_buffer() {
        assert_eq!(sample_stride(1), 1);
        assert_eq!(sample_stride(MAX_SAMPLES as u64), 1);
        assert_eq!(sample_stride(MAX_SAMPLES as u64 + 1), 2);
        // Worst case still fits: ceil(iters / stride) <= MAX_SAMPLES.
        for iters in [1, 12345, 3 * MAX_SAMPLES as u64 - 1] {
            let stride = sample_stride(iters);
            assert!(iters.div_ceil(stride) <= MAX_SAMPLES as u64);
        }
    }

    #[test]
    fn futex_lines_do_not_share_a_cache_line() {
        assert_eq!(std::mem::offset_of!(Channel, pong) % CACHE_LINE, 0);
        assert!(std::mem::offset_of!(Channel, pong) >= CACHE_LINE);
    }
}
