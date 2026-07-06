//! The GC'd value heap: bump allocation into 32 KiB blocks, non-moving
//! mark-sweep collection.
//!
//! Collection = clear marks -> mark from precise roots (VM structures,
//! supplied by the caller) + conservative scan of the native stack (covers
//! Rust builtin temporaries and, later, Cranelift JIT frames) -> sweep
//! (whole blocks with no survivors return to the free pool; partial blocks
//! are retained as-is and not re-bumped — mark-region style).
//!
//! Policy knobs (env): JINX_GC_OFF=1, JINX_GC_STRESS=1, JINX_GC_HEAP_MB=n,
//! JINX_GC_STATS=1.

use crate::mem::{BlockKind, BlockSpace, BLOCK_SIZE, GRANULE, LARGE_OBJECT_MIN};
use crate::value::{self, Attr, ObjKind, Tag, VRef, Value, VALUE_SIZE};
use std::ptr::NonNull;

const DEFAULT_MIN_TRIGGER: usize = 64 << 20; // 64 MiB
const STRESS_TRIGGER: usize = 4 << 10;

pub struct GcStats {
    pub collections: u64,
    pub total_pause: std::time::Duration,
    pub last_live_blocks: usize,
    pub peak_footprint: usize,
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
                total_pause: std::time::Duration::ZERO,
                last_live_blocks: 0,
                peak_footprint: 0,
            },
            stats_on: std::env::var_os("JINX_GC_STATS").is_some_and(|v| v != "0"),
            stack_base: current_stack_base(),
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
            self.min_trigger
        } else {
            self.min_trigger.max(self.retained * 2)
        }
    }

    // ---------------- allocation ----------------

    #[inline]
    pub fn alloc_value(&mut self, v: Value) -> VRef {
        let idx = match self.cur_value {
            Some(i) if self.space.meta(i).unwrap().bump + VALUE_SIZE <= BLOCK_SIZE => i,
            _ => {
                let (_, i) = self.space.acquire(BlockKind::Value);
                self.cur_value = Some(i);
                i
            }
        };
        let base = self.space.base_of(idx);
        let meta = self.space.meta_mut(idx).unwrap();
        let off = meta.bump;
        meta.bump = off + VALUE_SIZE;
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
            Some(i) if self.space.meta(i).unwrap().bump + rounded <= BLOCK_SIZE => i,
            _ => {
                let (_, i) = self.space.acquire(BlockKind::Data);
                self.cur_data = Some(i);
                i
            }
        };
        let base = self.space.base_of(idx);
        let meta = self.space.meta_mut(idx).unwrap();
        let off = meta.bump;
        meta.bump = off + rounded;
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

    pub fn new_ctx(&mut self, elems: &[u32]) -> *mut u64 {
        let p = self.alloc_data(ObjKind::Ctx, elems.len());
        // SAFETY: object sized for len u32s (word-padded).
        unsafe {
            std::ptr::copy_nonoverlapping(elems.as_ptr(), p.add(1) as *mut u32, elems.len());
        }
        p
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

    /// Run a full collection. `precise_roots` must mark every VM-reachable
    /// cell; the native stack is additionally scanned conservatively (unless
    /// `scan_stack` is false — used by deterministic unit tests).
    pub fn collect(&mut self, precise_roots: impl FnOnce(&mut Marker), scan_stack: bool) {
        let t0 = std::time::Instant::now();
        // Clear all marks.
        for idx in self.space.live_block_indices() {
            self.space.meta_mut(idx).unwrap().clear_marks();
        }
        for lo in &mut self.space.large {
            lo.marked = false;
        }

        let mut marker = Marker {
            space: &mut self.space,
            worklist: Vec::with_capacity(1024),
        };
        precise_roots(&mut marker);
        if scan_stack {
            let base = self.stack_base;
            spill_registers_and(|sp| {
                marker.scan_range(sp, base);
            });
        }
        marker.drain();

        // Sweep.
        let cur_v = self.cur_value;
        let cur_d = self.cur_data;
        let mut retained = 0usize;
        for idx in self.space.live_block_indices() {
            let meta = self.space.meta(idx).unwrap();
            let used_granules = meta.bump / GRANULE;
            let mut any_live = false;
            for g in 0..used_granules {
                if meta.is_start(g) && meta.is_marked(g) {
                    any_live = true;
                    break;
                }
            }
            if any_live || Some(idx) == cur_v || Some(idx) == cur_d {
                retained += BLOCK_SIZE;
            } else {
                if cfg!(debug_assertions) {
                    // Poison released blocks to catch use-after-free.
                    let base = self.space.base_of(idx);
                    // SAFETY: block is mapped and being released.
                    unsafe {
                        std::ptr::write_bytes(base as *mut u8, 0x5A, meta.bump);
                    }
                }
                self.space.release(idx);
            }
        }
        // Large objects.
        let mut i = 0;
        while i < self.space.large.len() {
            if self.space.large[i].marked {
                retained += self.space.large[i].size;
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
        self.stats.collections += 1;
        self.stats.total_pause += t0.elapsed();
        self.stats.last_live_blocks = retained / BLOCK_SIZE;
        self.stats.peak_footprint = self.stats.peak_footprint.max(retained);
    }

    pub fn footprint(&self) -> usize {
        self.retained + self.alloc_since_gc
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        if self.stats_on {
            eprintln!(
                "jinx gc: {} collections, {:?} total pause, retained {} blocks, peak footprint {} MiB",
                self.stats.collections,
                self.stats.total_pause,
                self.stats.last_live_blocks,
                self.stats.peak_footprint >> 20,
            );
        }
    }
}

/// Marking context handed to root providers.
pub struct Marker<'h> {
    space: &'h mut BlockSpace,
    worklist: Vec<VRef>,
}

impl Marker<'_> {
    /// Mark a value cell as live (precise root).
    #[inline]
    pub fn mark_cell(&mut self, v: VRef) {
        let addr = v.as_ptr() as usize;
        if let Some((idx, granule)) = self.space.locate(addr) {
            let meta = self.space.meta_mut(idx).unwrap();
            if meta.is_start(granule) && meta.set_mark(granule) {
                self.worklist.push(v);
            }
        }
    }

    /// Mark a value held by-copy in VM structures (frames, constants): its
    /// payload data object (if any) is marked directly, without needing the
    /// value to live in a heap cell.
    #[inline]
    pub fn mark_value(&mut self, v: &Value) {
        if v.has_heap_payload() {
            self.mark_data(v.ptr());
        }
    }

    /// Mark a data object (header pointer) live; trace its outgoing cells.
    fn mark_data(&mut self, p: *mut u64) {
        if p.is_null() {
            return;
        }
        let addr = p as usize;
        let newly = if let Some((idx, granule)) = self.space.locate(addr) {
            let meta = self.space.meta_mut(idx).unwrap();
            debug_assert!(meta.is_start(granule), "data ptr not at object start");
            meta.set_mark(granule)
        } else if let Some(li) = self.space.locate_large(addr) {
            let lo = &mut self.space.large[li];
            let newly = !lo.marked;
            lo.marked = true;
            newly
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
                        self.mark_data(ctx);
                    }
                }
                ObjKind::Ctx | ObjKind::Path => {}
                ObjKind::List | ObjKind::Upvals => {
                    for &e in value::elems(p) {
                        self.mark_cell(e);
                    }
                }
                ObjKind::Bindings => {
                    for a in value::bindings(p) {
                        self.mark_cell(a.val);
                    }
                }
                ObjKind::Thunk | ObjKind::PrimApp => {
                    let (_, elems) = value::code_and_elems(p);
                    for &e in elems {
                        self.mark_cell(e);
                    }
                }
            }
        }
    }

    /// Conservative scan: any word in [lo, hi) that resolves into the heap
    /// pins the containing object.
    fn scan_range(&mut self, lo: usize, hi: usize) {
        debug_assert!(lo <= hi);
        let mut a = lo & !(GRANULE - 1);
        while a + 8 <= hi {
            // SAFETY: scanning our own thread's stack memory.
            let word = unsafe { *(a as *const usize) };
            self.mark_ambiguous(word);
            a += 8;
        }
    }

    /// A possibly-pointer word from the stack: resolve interior pointers to
    /// object starts in both value and data blocks.
    fn mark_ambiguous(&mut self, word: usize) {
        // Fast reject before locate() (also rejects unaligned).
        if word % GRANULE != 0 {
            return;
        }
        if let Some((idx, granule)) = self.space.locate(word) {
            let meta = self.space.meta(idx).unwrap();
            match meta.kind {
                BlockKind::Value => {
                    // Cells are 16 bytes: resolve to the cell start granule.
                    let g = granule & !1;
                    if meta.is_start(g) {
                        let base = self.space.base_of(idx);
                        let cell = (base + g * GRANULE) as *mut Value;
                        // SAFETY: g is a valid allocated cell start.
                        self.mark_cell(unsafe { NonNull::new_unchecked(cell) });
                    }
                }
                BlockKind::Data => {
                    if let Some(g) = meta.find_start(granule) {
                        let base = self.space.base_of(idx);
                        self.mark_data((base + g * GRANULE) as *mut u64);
                    }
                }
            }
        } else if let Some(li) = self.space.locate_large(word) {
            let p = self.space.large[li].ptr.as_ptr() as *mut u64;
            self.mark_data(p);
        }
    }

    /// Process the worklist to a fixpoint.
    fn drain(&mut self) {
        while let Some(v) = self.worklist.pop() {
            // SAFETY: v was verified to be an allocated cell when pushed.
            let val = unsafe { *v.as_ptr() };
            if val.has_heap_payload() {
                self.mark_data(val.ptr());
            }
        }
    }
}

/// Highest scannable address of the current thread's stack.
fn current_stack_base() -> usize {
    // SAFETY: pthread_self is always valid; Darwin returns the stack top.
    unsafe {
        let me = libc::pthread_self();
        libc::pthread_get_stackaddr_np(me) as usize
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

/// Spill callee-saved registers into a stack buffer, then invoke `f` with the
/// current stack pointer. The buffer lives in this frame, so the [sp, base)
/// scan performed by `f` covers both the mutator's stack and the registers.
#[inline(never)]
fn spill_registers_and(f: impl FnOnce(usize)) {
    let mut buf = [0usize; 12];
    #[cfg(target_arch = "aarch64")]
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
}

impl Heap {
    #[cfg(test)]
    fn space_large_count(&self) -> usize {
        self.space.large.len()
    }
}
