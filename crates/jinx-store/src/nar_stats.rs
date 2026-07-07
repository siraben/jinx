//! Lightweight NAR-dump instrumentation, gated on `JINX_NAR_STATS=1`.
//!
//! When enabled, counts top-level dump calls, per-node-type counts, file
//! content bytes, total NAR bytes, and (reported by the eval crate) the
//! `filtered_path_cache` hit/miss ratio. The counters are process-global
//! atomics; a summary is printed to stderr at process exit via `atexit`.
//!
//! All increments are behind a cached `enabled()` check, so a non-instrumented
//! build path pays only a relaxed atomic load per top-level dump.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

static DUMP_CALLS: AtomicU64 = AtomicU64::new(0);
static NAR_BYTES: AtomicU64 = AtomicU64::new(0);
static FILE_COUNT: AtomicU64 = AtomicU64::new(0);
static FILE_BYTES: AtomicU64 = AtomicU64::new(0);
static DIR_COUNT: AtomicU64 = AtomicU64::new(0);
static SYMLINK_COUNT: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

/// Whether instrumentation is on, decided once from `JINX_NAR_STATS`.
#[inline]
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("JINX_NAR_STATS").is_some_and(|v| v != "0" && v != "");
        if on {
            register_atexit();
        }
        on
    })
}

fn register_atexit() {
    extern "C" fn print_hook() {
        print_summary();
    }
    // SAFETY: `atexit` registers an `extern "C"` function with no arguments and
    // no return value, exactly matching `print_hook`. `print_summary` only
    // reads process-global atomics and writes to stderr, both valid during
    // process teardown. Registered at most once (guarded by the OnceLock in
    // `enabled`).
    unsafe {
        libc::atexit(print_hook);
    }
}

#[inline]
pub fn record_dump_call() {
    if enabled() {
        DUMP_CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
pub fn record_nar_bytes(n: u64) {
    if enabled() {
        NAR_BYTES.fetch_add(n, Ordering::Relaxed);
    }
}

#[inline]
pub fn record_file(size: u64) {
    if enabled() {
        FILE_COUNT.fetch_add(1, Ordering::Relaxed);
        FILE_BYTES.fetch_add(size, Ordering::Relaxed);
    }
}

#[inline]
pub fn record_dir() {
    if enabled() {
        DIR_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
pub fn record_symlink() {
    if enabled() {
        SYMLINK_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Reported by `jinx-eval`'s `add_filtered_path` memo lookup.
#[inline]
pub fn record_cache_hit() {
    if enabled() {
        CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
pub fn record_cache_miss() {
    if enabled() {
        CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }
}

fn print_summary() {
    let dumps = DUMP_CALLS.load(Ordering::Relaxed);
    let nar_bytes = NAR_BYTES.load(Ordering::Relaxed);
    let files = FILE_COUNT.load(Ordering::Relaxed);
    let file_bytes = FILE_BYTES.load(Ordering::Relaxed);
    let dirs = DIR_COUNT.load(Ordering::Relaxed);
    let symlinks = SYMLINK_COUNT.load(Ordering::Relaxed);
    let hits = CACHE_HITS.load(Ordering::Relaxed);
    let misses = CACHE_MISSES.load(Ordering::Relaxed);
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    eprintln!("[jinx-nar-stats]");
    eprintln!("  dump calls        : {dumps}");
    eprintln!("  nar bytes         : {nar_bytes} ({:.1} MiB)", mib(nar_bytes));
    eprintln!("  regular files     : {files}");
    eprintln!("  file bytes        : {file_bytes} ({:.1} MiB)", mib(file_bytes));
    eprintln!("  directories       : {dirs}");
    eprintln!("  symlinks          : {symlinks}");
    eprintln!("  cache hits/misses : {hits} / {misses}");
}
