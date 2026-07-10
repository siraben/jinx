//! The GC'd value heap: bump allocation into 32 KiB blocks over one large
//! contiguous reservation, non-moving **sticky-mark generational** collection.
//!
//! A collection marks from precise roots (VM structures, supplied by the
//! caller) plus a conservative scan of the native stack and callee-saved
//! registers (covers Rust builtin temporaries and Cranelift JIT frames), then
//! sweeps mark-region style (whole blocks with no survivors return to the free
//! pool; partial blocks are retained, not re-bumped). Minor (young-only)
//! collections additionally trace a remembered set of old cells mutated since
//! the last GC — logged by a write barrier at the single `vm::set_b` choke
//! point — and sweep only young blocks; majors run on the first GC, at a
//! 2x-retained watermark, every 8th collection under stress, or with
//! JINX_GC_GEN=0. The conservative scan requires collection to run on the same
//! thread that built the heap (see the debug `owner` assert in `collect_gen`).
//!
//! Policy knobs (env): JINX_GC_OFF=1, JINX_GC_STRESS=1, JINX_GC_HEAP_MB=n,
//! JINX_GC_STATS=1, JINX_GC_GEN=0 (disable generational), JINX_GC_YOUNG_MB=n
//! (young-collection trigger), JINX_GC_RESERVE_GB=n (reservation size).

use crate::mem::{BlockKind, BlockSpace, BLOCK_SIZE, GRANULE, LARGE_OBJECT_MIN};
use crate::value::{self, Attr, ObjKind, Tag, VRef, Value, VALUE_SIZE};
use std::ptr::NonNull;

// 1 GiB: a nixpkgs `-A firefox` eval peaks ~700 MB; collecting at 256 MiB
// cost a ~35 ms pause that freed almost nothing (measured round 2). ISO RSS
// is unaffected (retained*2 growth passes 1 GiB anyway after the first GC).
const DEFAULT_MIN_TRIGGER: usize = 1024 << 20; // 1 GiB
const STRESS_TRIGGER: usize = 4 << 10;
/// Major-collection growth watermark, as a percentage of the retained old
/// generation: the next major fires once the heap has grown to
/// `retained * GROW / 100`. 300 (3x) is the R2 default; the geometric-sum
/// accounting (total major mark work ~= P*a/(a-1) for peak live P and growth
/// factor a) makes 3x do ~1.5*P total mark work vs 2x's 2*P — deleting ~25% of
/// total major mark time relative to the old 2x watermark. Override with the
/// `JINX_GC_GROW` env var (percent; e.g. 400 for 4x). Clamped to >= 150 so a
/// pathological value can't make majors fire before the heap has grown at all.
const DEFAULT_GROW_PERCENT: usize = 300;
/// Floor for the *first* major's next-major watermark. The first major on a
/// tiny heap (e.g. under stress, or an eval that just crossed the min trigger)
/// leaves a small `retained`; without a floor the next major would fire almost
/// immediately (retained * 3 of a few MiB), re-tracing constantly. Anchor the
/// first watermark at least this far out so early majors are spaced sensibly.
/// (Stress is unaffected: its 4 KiB `min_trigger` gate still fires minors long
/// before this watermark, and the every-8th-collection promotion keeps majors
/// stress-covered.)
const FIRST_MAJOR_FLOOR: usize = DEFAULT_MIN_TRIGGER;
/// Seed size below which parallel marking's thread-spawn overhead isn't worth
/// it (minor collections trace little); above it, a major trace parallelizes.
const PAR_MARK_MIN: usize = 50_000;

pub struct GcStats {
    pub collections: u64,
    pub majors: u64,
    pub total_pause: std::time::Duration,
    pub max_pause: std::time::Duration,
    pub last_live_blocks: usize,
    pub peak_footprint: usize,
    pub barrier_hits: u64,
}

pub struct Heap {
    space: BlockSpace,
    /// Current bump blocks (meta indices), if any.
    cur_value: Option<usize>,
    cur_data: Option<usize>,
    /// Bytes allocated since the last collection.
    alloc_since_gc: usize,
    /// Footprint (bytes) of retained blocks + large objects after last GC.
    retained: usize,
    min_trigger: usize,
    gc_off: bool,
    stress: bool,
    pub stats: GcStats,
    stats_on: bool,
    /// Base (highest address) of the mutator stack for conservative scanning.
    stack_base: usize,
    /// The thread that constructed this heap. `stack_base` is that thread's
    /// stack top, so the conservative `scan_range(sp, stack_base)` is only
    /// valid if collection always runs on this same thread. Debug-asserted at
    /// the collection entry to catch a future allocation from a worker thread.
    #[cfg(debug_assertions)]
    owner: std::thread::ThreadId,

    // ---- generational (sticky mark bit) state ----
    /// Generational collection disabled (JINX_GC_GEN=0): every GC is major.
    gen_off: bool,
    /// Blocks acquired since the last collection (the young generation).
    young_blocks: Vec<usize>,
    /// Old cells overwritten by the mutator since the last collection; their
    /// mark bit was cleared by the write barrier, so a minor GC must treat
    /// them as roots (they may now be the only path to young objects).
    remset: Vec<VRef>,
    /// True once any collection has run (before that everything is young and
    /// the barrier has nothing to do).
    gc_ran: bool,
    /// Retained footprint at which the next collection is promoted to major.
    major_watermark: usize,
    /// Allocation volume between minor collections (JINX_GC_YOUNG_MB).
    young_trigger: usize,
    /// Marker threads for the parallel transitive-closure drain (1 = the
    /// original single-threaded marker). `JINX_GC_THREADS` overrides.
    mark_threads: usize,
    /// Seed size above which a trace parallelizes (`JINX_GC_PAR_MIN`; default
    /// [`PAR_MARK_MIN`]). Set to 0 to force parallel marking (test/stress).
    par_mark_min: usize,
    /// Major-growth watermark as a percent of retained (`JINX_GC_GROW`; default
    /// [`DEFAULT_GROW_PERCENT`] = 3x). See the constant for the rationale.
    grow_percent: usize,
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl Heap {
    pub fn new() -> Self {
        let stress = std::env::var_os("JINX_GC_STRESS").is_some_and(|v| v != "0");
        let gc_off = std::env::var_os("JINX_GC_OFF").is_some_and(|v| v != "0");
        let min_trigger = std::env::var("JINX_GC_HEAP_MB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map(|mb| mb << 20)
            .unwrap_or(if stress { STRESS_TRIGGER } else { DEFAULT_MIN_TRIGGER });
        Heap {
            space: BlockSpace::new(),
            cur_value: None,
            cur_data: None,
            alloc_since_gc: 0,
            retained: 0,
            min_trigger,
            gc_off,
            stress,
            stats: GcStats {
                collections: 0,
                majors: 0,
                total_pause: std::time::Duration::ZERO,
                max_pause: std::time::Duration::ZERO,
                last_live_blocks: 0,
                peak_footprint: 0,
                barrier_hits: 0,
            },
            stats_on: std::env::var_os("JINX_GC_STATS").is_some_and(|v| v != "0"),
            stack_base: current_stack_base(),
            #[cfg(debug_assertions)]
            owner: std::thread::current().id(),
            gen_off: std::env::var_os("JINX_GC_GEN").is_some_and(|v| v == "0"),
            young_blocks: Vec::new(),
            remset: Vec::new(),
            gc_ran: false,
            major_watermark: 0,
            // Allocation volume between minor collections. For a one-shot
            // batch evaluator, over-collecting a monotonically growing heap is
            // pure cost (minors leave old-gen floating garbage that bloats the
            // next major), so the production default tracks `min_trigger`
            // rather than a small REPL-style young gen. The stress gate is
            // unaffected: under JINX_GC_STRESS `trigger()` uses `min_trigger`
            // (4 KiB) directly, so minor collections still fire ~every 4 KiB
            // and keep the ~8x cheaper stress gate. `JINX_GC_YOUNG_MB`
            // overrides for interactive/incremental workloads.
            young_trigger: std::env::var("JINX_GC_YOUNG_MB")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .map(|mb| mb << 20)
                .unwrap_or(min_trigger),
            // Default cap of 4: parallel marking is memory-bandwidth bound and
            // saturates by ~2 threads (measured), so more threads only risk
            // contention on a loaded host. JINX_GC_THREADS overrides;
            // JINX_GC_THREADS=1 forces the original single-threaded marker.
            mark_threads: std::env::var("JINX_GC_THREADS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| n >= 1)
                .unwrap_or_else(|| {
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(1)
                        .min(4)
                }),
            par_mark_min: std::env::var("JINX_GC_PAR_MIN")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(PAR_MARK_MIN),
            grow_percent: std::env::var("JINX_GC_GROW")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(DEFAULT_GROW_PERCENT)
                .max(150),
        }
    }

    /// Mutator write barrier: called before overwriting the value cell `c`.
    /// If `c` survived a previous collection (its sticky mark bit is set), the
    /// new value it now holds may be the only path to young objects, so the
    /// cell is logged in the remembered set (and its mark cleared, which both
    /// dedups the log and forces the next minor GC to re-trace it).
    #[inline]
    pub fn write_barrier(&mut self, c: VRef) {
        if !self.gc_ran {
            return;
        }
        let addr = c.as_ptr() as usize;
        if let Some((idx, granule)) = self.space.locate(addr) {
            if self.space.is_marked(idx, granule) {
                self.space.clear_mark(idx, granule);
                self.remset.push(c);
                self.stats.barrier_hits += 1;
            }
        }
    }

    /// Should the caller run a collection before the next allocation?
    #[inline]
    pub fn should_gc(&self) -> bool {
        !self.gc_off && self.alloc_since_gc >= self.trigger()
    }

    #[inline]
    fn trigger(&self) -> usize {
        if self.stress {
            return self.min_trigger;
        }
        if self.gen_off {
            // Non-generational: every GC is major, so gate on the same growth
            // watermark policy (R2) rather than a hard-coded 2x.
            let grown = self.retained + self.retained * (self.grow_percent - 100) / 100;
            return self.min_trigger.max(grown);
        }
        if !self.gc_ran {
            // First collection: honor the min-trigger (avoids collecting at
            // all on evals that finish under it).
            return self.min_trigger.max(self.young_trigger);
        }
        self.young_trigger
    }

    // ---------------- allocation ----------------

    #[inline]
    pub fn alloc_value(&mut self, v: Value) -> VRef {
        let idx = match self.cur_value {
            Some(i) if self.space.meta(i).unwrap().bump() + VALUE_SIZE <= BLOCK_SIZE => i,
            _ => {
                let (_, i) = self.space.acquire(BlockKind::Value);
                self.young_blocks.push(i);
                self.cur_value = Some(i);
                i
            }
        };
        let base = self.space.base_of(idx);
        let mut meta = self.space.meta_mut(idx).unwrap();
        let off = meta.bump();
        meta.set_bump(off + VALUE_SIZE);
        meta.set_start(off / GRANULE);
        self.alloc_since_gc += VALUE_SIZE;
        let p = (base + off) as *mut Value;
        // SAFETY: freshly carved 16-byte cell inside a mapped block.
        unsafe {
            p.write(v);
            NonNull::new_unchecked(p)
        }
    }

    /// Allocate a raw data object; returns pointer to its header word.
    fn alloc_data(&mut self, kind: ObjKind, len: usize) -> *mut u64 {
        let size = value::obj_size_bytes(value::header(kind, len));
        let rounded = size.div_ceil(GRANULE) * GRANULE;
        self.alloc_since_gc += rounded;
        if rounded >= LARGE_OBJECT_MIN {
            let p = self.space.alloc_large(rounded).as_ptr() as *mut u64;
            // SAFETY: fresh mapping, header fits.
            unsafe { p.write(value::header(kind, len)) };
            return p;
        }
        let idx = match self.cur_data {
            Some(i) if self.space.meta(i).unwrap().bump() + rounded <= BLOCK_SIZE => i,
            _ => {
                let (_, i) = self.space.acquire(BlockKind::Data);
                self.young_blocks.push(i);
                self.cur_data = Some(i);
                i
            }
        };
        let base = self.space.base_of(idx);
        let mut meta = self.space.meta_mut(idx).unwrap();
        let off = meta.bump();
        meta.set_bump(off + rounded);
        meta.set_start(off / GRANULE);
        let p = (base + off) as *mut u64;
        // SAFETY: freshly carved region inside a mapped block.
        unsafe { p.write(value::header(kind, len)) };
        p
    }

    pub fn new_string(&mut self, bytes: &[u8], ctx: *mut u64) -> Value {
        let p = self.alloc_data(ObjKind::Str, bytes.len());
        // SAFETY: object sized for len bytes at offset 16.
        unsafe {
            p.add(1).write(ctx as u64);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(2) as *mut u8, bytes.len());
        }
        Value::make(Tag::String, p as u64)
    }

    pub fn new_path(&mut self, accessor: u64, bytes: &[u8]) -> Value {
        let p = self.alloc_data(ObjKind::Path, bytes.len());
        // SAFETY: object sized for len bytes at offset 16.
        unsafe {
            p.add(1).write(accessor);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(2) as *mut u8, bytes.len());
        }
        Value::make(Tag::Path, p as u64)
    }

    pub fn new_list(&mut self, items: &[VRef]) -> Value {
        let p = self.alloc_data(ObjKind::List, items.len());
        // SAFETY: object sized for len pointers.
        unsafe {
            std::ptr::copy_nonoverlapping(items.as_ptr(), p.add(1) as *mut VRef, items.len());
        }
        Value::make(Tag::List, p as u64)
    }

    /// `entries` must already be sorted by symbol id.
    pub fn new_bindings(&mut self, entries: &[Attr]) -> Value {
        debug_assert!(entries.windows(2).all(|w| w[0].sym < w[1].sym));
        let p = self.alloc_data(ObjKind::Bindings, entries.len());
        // SAFETY: object sized for len 16-byte entries.
        unsafe {
            std::ptr::copy_nonoverlapping(entries.as_ptr(), p.add(1) as *mut Attr, entries.len());
        }
        Value::make(Tag::Attrs, p as u64)
    }

    /// `//`: merge two sorted attr slices directly into a fresh bindings
    /// object (single pass, no intermediate Vec). Mirrors C++
    /// `ExprOpUpdate::eval`, which merges straight into the destination
    /// Bindings. Both inputs must be sorted by sym; on duplicate syms the
    /// right entry wins (same as the old temp-Vec merge).
    pub fn new_bindings_merge(&mut self, le: &[Attr], re: &[Attr]) -> Value {
        debug_assert!(le.windows(2).all(|w| w[0].sym < w[1].sym));
        debug_assert!(re.windows(2).all(|w| w[0].sym < w[1].sym));
        // Exact merged length. Fast paths: disjoint ranges need no scan.
        let dups = if le.last().unwrap().sym < re.first().unwrap().sym
            || re.last().unwrap().sym < le.first().unwrap().sym
        {
            0
        } else {
            let (mut i, mut j, mut d) = (0usize, 0usize, 0usize);
            while i < le.len() && j < re.len() {
                match le[i].sym.cmp(&re[j].sym) {
                    std::cmp::Ordering::Equal => {
                        d += 1;
                        i += 1;
                        j += 1;
                    }
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                }
            }
            d
        };
        let n = le.len() + re.len() - dups;
        let p = self.alloc_data(ObjKind::Bindings, n);
        // SAFETY: object sized for n 16-byte entries; merge writes exactly n.
        unsafe {
            let out = p.add(1) as *mut Attr;
            let (mut i, mut j, mut k) = (0usize, 0usize, 0usize);
            while i < le.len() && j < re.len() {
                match le[i].sym.cmp(&re[j].sym) {
                    std::cmp::Ordering::Equal => {
                        out.add(k).write(re[j]);
                        i += 1;
                        j += 1;
                    }
                    std::cmp::Ordering::Less => {
                        out.add(k).write(le[i]);
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        out.add(k).write(re[j]);
                        j += 1;
                    }
                }
                k += 1;
            }
            if i < le.len() {
                std::ptr::copy_nonoverlapping(le.as_ptr().add(i), out.add(k), le.len() - i);
                k += le.len() - i;
            }
            if j < re.len() {
                std::ptr::copy_nonoverlapping(re.as_ptr().add(j), out.add(k), re.len() - j);
                k += re.len() - j;
            }
            debug_assert_eq!(k, n);
        }
        Value::make(Tag::Attrs, p as u64)
    }

    pub fn new_ctx(&mut self, elems: &[u32]) -> *mut u64 {
        let p = self.alloc_data(ObjKind::Ctx, elems.len());
        // SAFETY: object sized for len u32s (word-padded).
        unsafe {
            std::ptr::copy_nonoverlapping(elems.as_ptr(), p.add(1) as *mut u32, elems.len());
        }
        p
    }

    /// Like `new_thunk`, but returns the object with `len` upval slots
    /// UNINITIALIZED; the caller must write all `len` slots before the next
    /// possible collection (i.e. before any further gc_check/alloc wrapper).
    /// Avoids the temp Vec + copy in the very hot `make_thunk` path.
    pub fn new_thunk_raw(&mut self, tag: Tag, code: *const (), len: usize) -> (Value, *mut VRef) {
        debug_assert!(matches!(tag, Tag::Thunk | Tag::Closure));
        let p = self.alloc_data(ObjKind::Thunk, len);
        // SAFETY: object sized for len pointers at offset 16.
        unsafe {
            p.add(1).write(code as u64);
            (Value::make(tag, p as u64), p.add(2) as *mut VRef)
        }
    }

    /// Like `new_bindings`, but with `len` UNINITIALIZED entries for the
    /// caller to fill (sorted by sym) before the next possible collection.
    pub fn new_bindings_raw(&mut self, len: usize) -> (Value, *mut Attr) {
        let p = self.alloc_data(ObjKind::Bindings, len);
        (Value::make(Tag::Attrs, p as u64), unsafe {
            p.add(1) as *mut Attr
        })
    }

    /// Concatenate two element slices directly into a fresh list object.
    pub fn new_list_concat(&mut self, a: &[VRef], b: &[VRef]) -> Value {
        let p = self.alloc_data(ObjKind::List, a.len() + b.len());
        // SAFETY: object sized for a+b pointers.
        unsafe {
            let out = p.add(1) as *mut VRef;
            std::ptr::copy_nonoverlapping(a.as_ptr(), out, a.len());
            std::ptr::copy_nonoverlapping(b.as_ptr(), out.add(a.len()), b.len());
        }
        Value::make(Tag::List, p as u64)
    }

    /// Thunks and closures: `code` is an immortal pointer (Chunk), `upvals`
    /// are captured cells. `tag` is Tag::Thunk or Tag::Closure.
    pub fn new_thunk(&mut self, tag: Tag, code: *const (), upvals: &[VRef]) -> Value {
        debug_assert!(matches!(tag, Tag::Thunk | Tag::Closure));
        let p = self.alloc_data(ObjKind::Thunk, upvals.len());
        // SAFETY: object sized for len pointers at offset 16.
        unsafe {
            p.add(1).write(code as u64);
            std::ptr::copy_nonoverlapping(upvals.as_ptr(), p.add(2) as *mut VRef, upvals.len());
        }
        Value::make(tag, p as u64)
    }

    pub fn new_primapp(&mut self, prim: *const (), args: &[VRef]) -> Value {
        let p = self.alloc_data(ObjKind::PrimApp, args.len());
        // SAFETY: object sized for len pointers at offset 16.
        unsafe {
            p.add(1).write(prim as u64);
            std::ptr::copy_nonoverlapping(args.as_ptr(), p.add(2) as *mut VRef, args.len());
        }
        Value::make(Tag::PrimOpApp, p as u64)
    }

    // ---------------- collection ----------------

    /// Run a full (major) collection. `precise_roots` must mark every
    /// VM-reachable cell; the native stack is additionally scanned
    /// conservatively (unless `scan_stack` is false — used by deterministic
    /// unit tests).
    pub fn collect(&mut self, precise_roots: impl FnOnce(&mut Marker), scan_stack: bool) {
        self.collect_gen(precise_roots, scan_stack, true)
    }

    /// Policy entry point: run a minor (young-generation) collection, or a
    /// major one when the old generation has grown past the watermark. Under
    /// JINX_GC_STRESS, every 8th collection is promoted to major so both
    /// paths are stress-covered.
    pub fn collect_auto(&mut self, precise_roots: impl FnOnce(&mut Marker), scan_stack: bool) {
        let major = self.gen_off
            || !self.gc_ran
            || self.retained >= self.major_watermark
            || (self.stress && self.stats.collections % 8 == 7);
        self.collect_gen(precise_roots, scan_stack, major)
    }

    fn collect_gen(
        &mut self,
        precise_roots: impl FnOnce(&mut Marker),
        scan_stack: bool,
        major: bool,
    ) {
        // The conservative stack scan uses `stack_base`, captured on the owning
        // thread at construction. Collecting from any other thread would scan a
        // bogus [sp, stack_base) range (miss real roots -> UAF, or read unmapped
        // memory -> SIGSEGV). Nothing spawns a heap-touching worker today; this
        // pins the invariant so a future one fails loudly in debug builds.
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            std::thread::current().id(),
            self.owner,
            "jinx GC ran on a different thread than the one that built the heap"
        );
        let t0 = std::time::Instant::now();
        if major {
            // Clear all marks: everything is young again.
            for idx in self.space.live_block_indices() {
                self.space.clear_marks(idx);
            }
            for lo in &self.space.large {
                lo.marked.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            // Every cell will be re-traced from scratch; the remset is moot.
            self.remset.clear();
        }

        // Root phase (single-threaded): seed the worklist with directly-reachable
        // cells from the remembered set, precise VM roots, and the conservative
        // native-stack scan.
        let remset = std::mem::take(&mut self.remset);
        let seed = {
            let mut marker = Marker {
                space: &self.space,
                worklist: Vec::with_capacity(1024),
            };
            // Remembered set first (minor only; empty on major): old cells whose
            // contents changed since the last GC. Marking them re-sets their mark
            // bit and traces whatever they point to now.
            for &c in &remset {
                marker.mark_cell(c);
            }
            precise_roots(&mut marker);
            if scan_stack {
                let base = self.stack_base;
                spill_registers_and(|sp| {
                    marker.scan_range(sp, base);
                });
            }
            std::mem::take(&mut marker.worklist)
        };
        // Transitive closure: parallelize a large trace (majors) across marker
        // threads; small traces (minors) stay single-threaded to avoid overhead.
        if self.mark_threads > 1 && seed.len() >= self.par_mark_min {
            parallel_drain(&self.space, seed, self.mark_threads);
        } else {
            single_drain(&self.space, seed);
        }

        // Sweep. Minor collections only visit blocks allocated since the last
        // collection (old blocks all contain sticky-marked survivors and are
        // retained by construction; their dead space is reclaimed at the next
        // major).
        let cur_v = self.cur_value;
        let cur_d = self.cur_data;
        let sweep_set: Vec<usize> = if major {
            self.young_blocks.clear();
            self.space.live_block_indices()
        } else {
            std::mem::take(&mut self.young_blocks)
        };
        let mut retained = if major { 0 } else { self.retained };
        for idx in sweep_set {
            let meta = self.space.meta(idx).unwrap();
            let bump = meta.bump();
            let used_granules = bump / GRANULE;
            let any_live = meta.any_marked(used_granules);
            if any_live || Some(idx) == cur_v || Some(idx) == cur_d {
                retained += BLOCK_SIZE;
            } else {
                if cfg!(debug_assertions) {
                    // Poison released blocks to catch use-after-free.
                    let base = self.space.base_of(idx);
                    // SAFETY: block is mapped and being released.
                    unsafe {
                        std::ptr::write_bytes(base as *mut u8, 0x5A, bump);
                    }
                }
                self.space.release(idx);
            }
        }
        // Large objects: minor sweeps only the un-aged (young) ones.
        let mut i = 0;
        while i < self.space.large.len() {
            let lo = &mut self.space.large[i];
            if lo.marked.load(std::sync::atomic::Ordering::Relaxed) || (!major && lo.aged) {
                if major {
                    retained += lo.size;
                } else if !lo.aged {
                    retained += lo.size; // young survivor, newly counted
                }
                lo.aged = true;
                i += 1;
            } else {
                let lo = self.space.large.swap_remove(i);
                // SAFETY: unreferenced mapping being returned to the OS.
                unsafe {
                    libc::munmap(lo.ptr.as_ptr() as *mut libc::c_void, lo.size);
                }
            }
        }

        self.retained = retained;
        self.alloc_since_gc = 0;
        self.gc_ran = true;
        if major {
            self.stats.majors += 1;
            // Next major once the old generation has grown by `grow_percent`
            // (R2: geometric growth >= 3x does less total major mark work than
            // the old 2x). Guarantee at least half the min trigger of absolute
            // growth so a small retained heap doesn't schedule majors too
            // tightly, and anchor the *first* major's watermark at the floor so
            // early majors (esp. on a tiny post-first-major heap) stay spaced.
            let grown = retained + retained * (self.grow_percent - 100) / 100;
            self.major_watermark = grown
                .max(retained + self.min_trigger / 2)
                .max(if self.stats.majors == 1 { FIRST_MAJOR_FLOOR } else { 0 });
        }
        let pause = t0.elapsed();
        self.stats.collections += 1;
        self.stats.total_pause += pause;
        self.stats.max_pause = self.stats.max_pause.max(pause);
        self.stats.last_live_blocks = retained / BLOCK_SIZE;
        self.stats.peak_footprint = self.stats.peak_footprint.max(retained);
        if self.stats_on {
            eprintln!(
                "jinx gc #{} ({}): pause {:?} (remset {}); retained {} MiB",
                self.stats.collections,
                if major { "major" } else { "minor" },
                pause,
                remset.len(),
                retained >> 20,
            );
        }
    }

    pub fn footprint(&self) -> usize {
        self.retained + self.alloc_since_gc
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        if self.stats_on {
            eprintln!(
                "jinx gc: {} collections ({} major), {:?} total pause, {:?} max pause, retained {} blocks, peak footprint {} MiB, {} barrier hits, committed {} MiB (free pool {} MiB), large {} MiB",
                self.stats.collections,
                self.stats.majors,
                self.stats.total_pause,
                self.stats.max_pause,
                self.stats.last_live_blocks,
                self.stats.peak_footprint >> 20,
                self.stats.barrier_hits,
                self.space.committed_bytes() >> 20,
                (self.space.free_blocks() * BLOCK_SIZE) >> 20,
                self.space.large.iter().map(|l| l.size).sum::<usize>() >> 20,
            );
        }
    }
}

// ---- shared tracing primitives (over a `&BlockSpace`; mark bits are atomic
// so these are safe to run from several marker threads at once) ------------

/// Mark a value cell live; if newly marked, `push` it for later tracing.
#[inline]
fn mark_cell_into<F: FnMut(VRef)>(space: &BlockSpace, v: VRef, push: &mut F) {
    let addr = v.as_ptr() as usize;
    if let Some((idx, granule)) = space.locate(addr) {
        // A VRef must resolve to a value-block cell start; a stale field read
        // from a conservatively-pinned dead object can point elsewhere.
        if space.kind_of(idx) == BlockKind::Value
            && space.is_start(idx, granule)
            && space.set_mark(idx, granule)
        {
            push(v);
        }
    }
}

/// Mark a data object live and trace its outgoing cells (pushing newly-marked
/// ones). Robustness against conservatively-pinned *dead* cells: only follow
/// genuine data-object starts, else a stale/reused word would be read as an
/// `ObjHeader` and walked with a bogus length.
fn trace_data<F: FnMut(VRef)>(space: &BlockSpace, p: *mut u64, push: &mut F) {
    if p.is_null() {
        return;
    }
    let addr = p as usize;
    let newly = if let Some((idx, granule)) = space.locate(addr) {
        if space.kind_of(idx) != BlockKind::Data || !space.is_start(idx, granule) {
            return;
        }
        space.set_mark(idx, granule)
    } else if let Some(li) = space.locate_large(addr) {
        if space.large[li].ptr.as_ptr() as usize != addr {
            return;
        }
        !space.large[li]
            .marked
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    } else {
        return; // immortal (symbol table, code, statics)
    };
    if !newly {
        return;
    }
    // SAFETY: p is a live data object header.
    unsafe {
        let h = *p;
        match value::header_kind(h) {
            ObjKind::Str => {
                let ctx = *p.add(1) as *mut u64;
                if !ctx.is_null() {
                    trace_data(space, ctx, push);
                }
            }
            ObjKind::Ctx | ObjKind::Path => {}
            ObjKind::List | ObjKind::Upvals => {
                for &e in value::elems(p) {
                    mark_cell_into(space, e, push);
                }
            }
            ObjKind::Bindings => {
                for a in value::bindings(p) {
                    mark_cell_into(space, a.val, push);
                }
            }
            ObjKind::Thunk | ObjKind::PrimApp => {
                let (_, elems) = value::code_and_elems(p);
                for &e in elems {
                    mark_cell_into(space, e, push);
                }
            }
        }
    }
}

/// Resolve an ambiguous stack word to object start(s) and mark them.
#[inline]
fn scan_word<F: FnMut(VRef)>(space: &BlockSpace, word: usize, push: &mut F) {
    if word % GRANULE != 0 {
        return;
    }
    if let Some((idx, granule)) = space.locate(word) {
        match space.kind_of(idx) {
            BlockKind::Value => {
                // Cells are 16 bytes: resolve to the cell start granule.
                let g = granule & !1;
                if space.is_start(idx, g) {
                    let base = space.base_of(idx);
                    let cell = (base + g * GRANULE) as *mut Value;
                    // SAFETY: g is a valid allocated cell start.
                    mark_cell_into(space, unsafe { NonNull::new_unchecked(cell) }, push);
                }
            }
            BlockKind::Data => {
                if let Some(g) = space.find_start(idx, granule) {
                    let base = space.base_of(idx);
                    trace_data(space, (base + g * GRANULE) as *mut u64, push);
                }
            }
        }
    } else if let Some(li) = space.locate_large(word) {
        let p = space.large[li].ptr.as_ptr() as *mut u64;
        trace_data(space, p, push);
    }
}

/// Marking context handed to root providers. Root marking is single-threaded;
/// it seeds `worklist` with the directly-reachable cells, and the drain (below,
/// optionally parallel) computes the transitive closure.
pub struct Marker<'h> {
    space: &'h BlockSpace,
    worklist: Vec<VRef>,
}

impl Marker<'_> {
    /// Mark a value cell as live (precise root).
    #[inline]
    pub fn mark_cell(&mut self, v: VRef) {
        let space = self.space;
        mark_cell_into(space, v, &mut |c| self.worklist.push(c));
    }

    /// Mark a value held by-copy in VM structures (frames, constants): its
    /// payload data object (if any) is traced directly.
    #[inline]
    pub fn mark_value(&mut self, v: &Value) {
        if v.has_heap_payload() {
            let space = self.space;
            trace_data(space, v.ptr(), &mut |c| self.worklist.push(c));
        }
    }

    /// Conservative scan: any word in [lo, hi) that resolves into the heap
    /// pins the containing object.
    fn scan_range(&mut self, lo: usize, hi: usize) {
        debug_assert!(lo <= hi);
        let space = self.space;
        let mut a = lo & !(GRANULE - 1);
        while a + 8 <= hi {
            // SAFETY: scanning our own thread's stack memory.
            let word = unsafe { *(a as *const usize) };
            scan_word(space, word, &mut |c| self.worklist.push(c));
            a += 8;
        }
    }
}

/// Single-threaded transitive-closure drain of the seed worklist.
fn single_drain(space: &BlockSpace, mut worklist: Vec<VRef>) {
    while let Some(v) = worklist.pop() {
        // SAFETY: v was verified to be an allocated cell when pushed.
        let val = unsafe { *v.as_ptr() };
        if val.has_heap_payload() {
            trace_data(space, val.ptr(), &mut |c| worklist.push(c));
        }
    }
}

/// A `VRef` that can cross marker threads. Safe during stop-the-world marking:
/// the heap is non-moving and paused, so a cell address is stable and only ever
/// read (mark bits mutate atomically).
#[derive(Clone, Copy)]
struct Task(VRef);
// SAFETY: see above — sending a cell address between markers is sound while the
// mutator is stopped and the heap is non-moving.
unsafe impl Send for Task {}

/// Parallel transitive-closure drain: `nthreads` work-stealing markers.
///
/// Correct because the mutator is stopped for the whole collection, so the only
/// cross-thread mutation is the atomic claim of mark bits / large-object flags
/// (each object is traced by exactly one marker), and the block-space structure
/// (metas, bump pointers, large vec) is immutable for the duration.
fn parallel_drain(space: &BlockSpace, seed: Vec<VRef>, nthreads: usize) {
    use crossbeam_deque::{Injector, Steal, Stealer, Worker};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let injector = Injector::<Task>::new();
    // Count of pushed-but-not-yet-completed tasks: hits 0 exactly when the
    // closure is complete (a task is counted before it becomes findable and
    // uncounted only after all its children are pushed).
    let in_flight = AtomicUsize::new(seed.len());
    for v in seed {
        injector.push(Task(v));
    }
    let workers: Vec<Worker<Task>> = (0..nthreads).map(|_| Worker::new_lifo()).collect();
    let stealers: Vec<Stealer<Task>> = workers.iter().map(|w| w.stealer()).collect();

    // The block space is only atomically mutated during marking and is
    // otherwise immutable, so a shared `&` may cross threads for this scope.
    struct SyncSpace<'a>(&'a BlockSpace);
    // SAFETY: no thread mutates the block-space structure during marking.
    unsafe impl Sync for SyncSpace<'_> {}
    let shared = SyncSpace(space);

    std::thread::scope(|scope| {
        for worker in workers {
            let injector = &injector;
            let stealers = &stealers;
            let in_flight = &in_flight;
            let shared = &shared;
            scope.spawn(move || {
                let space = shared.0;
                loop {
                    let task = worker.pop().or_else(|| loop {
                        match injector.steal_batch_and_pop(&worker) {
                            Steal::Success(t) => return Some(t),
                            Steal::Retry => continue,
                            Steal::Empty => {}
                        }
                        let mut retry = false;
                        for s in stealers.iter() {
                            match s.steal() {
                                Steal::Success(t) => return Some(t),
                                Steal::Retry => retry = true,
                                Steal::Empty => {}
                            }
                        }
                        if !retry {
                            return None;
                        }
                    });
                    match task {
                        Some(Task(v)) => {
                            // SAFETY: v was an allocated cell when pushed.
                            let val = unsafe { *v.as_ptr() };
                            if val.has_heap_payload() {
                                trace_data(space, val.ptr(), &mut |c| {
                                    in_flight.fetch_add(1, Ordering::Relaxed);
                                    worker.push(Task(c));
                                });
                            }
                            in_flight.fetch_sub(1, Ordering::Relaxed);
                        }
                        None => {
                            if in_flight.load(Ordering::Relaxed) == 0 {
                                break;
                            }
                            // No local/stealable work yet but the closure isn't
                            // done — yield rather than busy-spin so we don't burn
                            // a core (esp. on an oversubscribed host).
                            std::thread::yield_now();
                        }
                    }
                }
            });
        }
    });
}

/// Highest scannable address of the current thread's stack.
#[cfg(target_os = "macos")]
fn current_stack_base() -> usize {
    // SAFETY: pthread_self is always valid; Darwin returns the stack top.
    unsafe {
        let me = libc::pthread_self();
        libc::pthread_get_stackaddr_np(me) as usize
    }
}

/// Highest scannable address of the current thread's stack.
#[cfg(target_os = "linux")]
fn current_stack_base() -> usize {
    // SAFETY: pthread_getattr_np/pthread_attr_getstack are the documented way
    // to obtain the stack extent on Linux; `stackaddr` is the LOWEST address,
    // so the scannable top is `stackaddr + stacksize`.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        let rc = libc::pthread_getattr_np(libc::pthread_self(), &mut attr);
        assert_eq!(rc, 0, "pthread_getattr_np failed");
        let mut stackaddr: *mut libc::c_void = std::ptr::null_mut();
        let mut stacksize: libc::size_t = 0;
        let rc = libc::pthread_attr_getstack(&mut attr, &mut stackaddr, &mut stacksize);
        assert_eq!(rc, 0, "pthread_attr_getstack failed");
        libc::pthread_attr_destroy(&mut attr);
        stackaddr as usize + stacksize
    }
}

/// Dump all callee-saved general registers into `buf` (they may hold the only
/// reference to a heap object at the moment GC runs).
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
extern "C" fn dump_callee_saved(buf: *mut usize) {
    std::arch::naked_asm!(
        "stp x19, x20, [x0]",
        "stp x21, x22, [x0, #16]",
        "stp x23, x24, [x0, #32]",
        "stp x25, x26, [x0, #48]",
        "stp x27, x28, [x0, #64]",
        "stp x29, x30, [x0, #80]",
        "ret",
    )
}

/// Dump all callee-saved general registers into `buf` (they may hold the only
/// reference to a heap object at the moment GC runs).
///
/// System V x86-64: rbx, rbp, r12-r15 are callee-saved.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
extern "C" fn dump_callee_saved(buf: *mut usize) {
    std::arch::naked_asm!(
        "mov [rdi], rbx",
        "mov [rdi + 8], rbp",
        "mov [rdi + 16], r12",
        "mov [rdi + 24], r13",
        "mov [rdi + 32], r14",
        "mov [rdi + 40], r15",
        "ret",
    )
}

/// Spill callee-saved registers into a stack buffer, then invoke `f` with the
/// current stack pointer. The buffer lives in this frame, so the [sp, base)
/// scan performed by `f` covers both the mutator's stack and the registers.
#[inline(never)]
fn spill_registers_and(f: impl FnOnce(usize)) {
    // A mutator heap pointer may live ONLY in a callee-saved register at GC
    // time; without an arch-specific dump those registers are never scanned and
    // the object is freed underneath us. Rather than silently mis-collect on an
    // unsupported arch, fail to build there.
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    compile_error!(
        "conservative GC register spill (dump_callee_saved) is only implemented \
         for aarch64 and x86_64; add a variant before targeting another arch"
    );
    let mut buf = [0usize; 12];
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    dump_callee_saved(buf.as_mut_ptr());
    let sp = buf.as_ptr() as usize;
    f(sp);
    std::hint::black_box(&mut buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Attr, Tag, VRef, Value};

    fn mk_int(h: &mut Heap, i: i64) -> VRef {
        h.alloc_value(Value::int(i))
    }

    fn deep_list(h: &mut Heap, depth: usize) -> VRef {
        let mut cur = mk_int(h, depth as i64);
        for _ in 0..depth {
            let lv = h.new_list(&[cur]);
            cur = h.alloc_value(lv);
        }
        cur
    }

    /// Sum of the ints at the leaves of nested single-element lists.
    unsafe fn leaf_int(v: VRef) -> i64 {
        let mut cur = v;
        loop {
            let val = *cur.as_ptr();
            match val.tag() {
                Tag::Int => return val.as_int(),
                Tag::List => {
                    let es = value::elems(val.ptr() as *const u64);
                    cur = es[0];
                }
                t => panic!("unexpected tag {t:?}"),
            }
        }
    }

    #[test]
    fn precise_collect_retains_live_and_frees_dead() {
        let mut h = Heap::new();
        let live = deep_list(&mut h, 100);
        // Allocate a lot of garbage.
        for _ in 0..10_000 {
            let _ = deep_list(&mut h, 20);
        }
        let before = h.footprint();
        h.collect(|m| m.mark_cell(live), false);
        let after = h.footprint();
        assert!(after < before / 4, "sweep should reclaim garbage: {before} -> {after}");
        // The live structure survives intact.
        assert_eq!(unsafe { leaf_int(live) }, 100);
        // And more allocation after GC still works.
        let live2 = deep_list(&mut h, 50);
        h.collect(|m| { m.mark_cell(live); m.mark_cell(live2); }, false);
        assert_eq!(unsafe { leaf_int(live) }, 100);
        assert_eq!(unsafe { leaf_int(live2) }, 50);
    }

    #[test]
    fn conservative_stack_scan_pins_locals() {
        let mut h = Heap::new();
        let live = deep_list(&mut h, 64);
        for _ in 0..1_000 {
            let _ = deep_list(&mut h, 10);
        }
        // No precise roots at all: `live` must survive via the stack scan.
        h.collect(|_| {}, true);
        assert_eq!(unsafe { leaf_int(live) }, 64);
        std::hint::black_box(live);
    }

    #[test]
    fn bindings_and_strings_traced() {
        let mut h = Heap::new();
        let sval = h.new_string(b"hello world", std::ptr::null_mut());
        let sref = h.alloc_value(sval);
        let ival = mk_int(&mut h, 7);
        let b = h.new_bindings(&[
            Attr { sym: 1, pos: 0, val: sref },
            Attr { sym: 2, pos: 0, val: ival },
        ]);
        let broot = h.alloc_value(b);
        for _ in 0..5_000 {
            let g = h.new_string(b"garbage garbage garbage", std::ptr::null_mut());
            let _ = h.alloc_value(g);
        }
        h.collect(|m| m.mark_cell(broot), false);
        unsafe {
            let bv = *broot.as_ptr();
            let attrs = value::bindings(bv.ptr() as *const u64);
            assert_eq!(attrs.len(), 2);
            let s = *attrs[0].val.as_ptr();
            let (bytes, _) = value::str_parts(s.ptr() as *const u64);
            assert_eq!(bytes, b"hello world");
            assert_eq!((*attrs[1].val.as_ptr()).as_int(), 7);
        }
    }

    #[test]
    fn large_objects_swept() {
        let mut h = Heap::new();
        let big_live = {
            let data = vec![0xABu8; 100_000];
            let v = h.new_string(&data, std::ptr::null_mut());
            h.alloc_value(v)
        };
        for _ in 0..20 {
            let data = vec![0xCDu8; 50_000];
            let v = h.new_string(&data, std::ptr::null_mut());
            let _ = h.alloc_value(v);
        }
        assert_eq!(h.space_large_count(), 21);
        h.collect(|m| m.mark_cell(big_live), false);
        assert_eq!(h.space_large_count(), 1);
        unsafe {
            let s = *big_live.as_ptr();
            let (bytes, _) = value::str_parts(s.ptr() as *const u64);
            assert_eq!(bytes.len(), 100_000);
            assert!(bytes.iter().all(|&b| b == 0xAB));
        }
    }

    #[test]
    fn stress_bounded_footprint() {
        let mut h = Heap::new();
        let live = deep_list(&mut h, 32);
        for round in 0..200 {
            for _ in 0..500 {
                let _ = deep_list(&mut h, 8);
            }
            h.collect(|m| m.mark_cell(live), false);
            assert!(
                h.footprint() < 8 << 20,
                "footprint unbounded at round {round}: {}",
                h.footprint()
            );
        }
        assert_eq!(unsafe { leaf_int(live) }, 32);
    }

    /// Regression: the conservative native-stack scan can pin a *dead* value
    /// cell via a stale word, and such a cell may still hold a stale payload
    /// pointer whose data object was freed and its block reused — leaving the
    /// pointer landing at a non-object-start (interior) location, or on a value
    /// cell rather than a data object. The tracer must refuse to read such a
    /// location as an `ObjHeader` (previously it did, walking a bogus length —
    /// a use-after-free that segfaulted heavy `JINX_GC_STRESS` evals).
    #[test]
    fn tracer_skips_stale_non_object_start_payloads() {
        let mut h = Heap::new();
        // A live keeper so its value block is retained across the collection.
        let keeper = h.alloc_value(Value::int(7));

        // A real string data object; take an *interior* address (not a start).
        let s = h.new_string(b"0123456789abcdef", std::ptr::null_mut());
        let interior = s.ptr() as usize + 8;
        // A cell whose value claims to be a List pointing into the middle of the
        // string. Following it as a data object would read string bytes as a
        // header and iterate a bogus element count.
        let stale_interior = h.alloc_value(Value::make(Tag::List, interior as u64));

        // A cell whose value claims its payload is another *value cell* (a
        // pointer into a value block, never a valid data object).
        let stale_kind = h.alloc_value(Value::make(Tag::Attrs, keeper.as_ptr() as u64));

        // Pin all three as roots (as the conservative stack scan would) and
        // collect. This must complete without dereferencing the bogus payloads.
        h.collect(
            |m| {
                m.mark_cell(keeper);
                m.mark_cell(stale_interior);
                m.mark_cell(stale_kind);
            },
            false,
        );

        // The collector ran to completion and left the keeper intact.
        assert_eq!(unsafe { (*keeper.as_ptr()).as_int() }, 7);
    }
}

impl Heap {
    #[cfg(test)]
    fn space_large_count(&self) -> usize {
        self.space.large.len()
    }
}
