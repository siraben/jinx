//! Block-structured heap memory: 32 KiB aligned blocks carved from one large
//! contiguous reservation, with per-block object-start and mark bitmaps
//! (granule = 8 bytes).
//!
//! The whole block space is a single anonymous mmap reservation (lazily
//! committed by the OS on first touch), so `locate()` is a subtraction, a
//! shift and one array index — no hash lookup on the marking hot path.
//!
//! Two block kinds:
//! - Value blocks: uniform 16-byte cells (`value::Value`), headerless.
//! - Data blocks: variable-size objects, each starting with a one-word
//!   `ObjHeader` (kind + size in granules) used by the tracer and sweeper.
//!
//! Objects larger than `LARGE_OBJECT_MIN` get their own mmap ("large object"),
//! tracked individually.
//!
//! ## Metadata layout (R4: flat, no dependent loads on the mark/locate path)
//!
//! Per-block metadata is stored as *flat parallel arrays* indexed by block
//! number, not `Vec<Option<BlockMeta>>` with boxed bitmaps. A traced edge's
//! `locate` → `is_start` → `set_mark` chain used to cost: metas base → Option
//! branch → BlockMeta → Box deref → bitmap byte, twice (starts + marks). Now it
//! is subtract/shift plus one indexed load per array with no pointer chasing:
//! - `kinds[idx]`  — [`KIND_FREE`]/[`KIND_VALUE`]/[`KIND_DATA`] (1 byte)
//! - `bumps[idx]`  — bump offset in bytes (`u32`; a block is 32 KiB)
//! - `starts[idx*BITMAP_BYTES ..]` — object-start bitmap arena (contiguous)
//! - `marks[idx*BITMAP_BYTES ..]`  — mark bitmap arena (contiguous, atomic)
//!
//! `meta()`/`meta_mut()` return thin [`BlockRef`]/[`BlockRefMut`] views over
//! these arenas so the collector's call sites read unchanged.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

pub const BLOCK_SIZE: usize = 32 * 1024;
pub const BLOCK_ALIGN: usize = BLOCK_SIZE;
pub const BLOCK_SHIFT: usize = 15;
pub const GRANULE: usize = 8;
/// Granules per block.
pub const BLOCK_GRANULES: usize = BLOCK_SIZE / GRANULE; // 4096
/// Bitmap bytes per block (1 bit per granule).
pub const BITMAP_BYTES: usize = BLOCK_GRANULES / 8; // 512

/// Objects at or above this size go to the large-object space.
pub const LARGE_OBJECT_MIN: usize = 8 * 1024;

/// How many blocks to hand out per commit step (16 MiB).
const BLOCKS_PER_CHUNK: usize = 512;

/// Default reserved (not committed) address space for the block heap. Pages
/// are only dirtied on first touch, so this costs virtual address space, not
/// memory. Override with `JINX_GC_RESERVE_GB`; on mmap failure the reservation
/// is halved down to a 1 GiB floor before giving up.
const DEFAULT_RESERVE_BYTES: usize = 48 << 30; // 48 GiB
const MIN_RESERVE_BYTES: usize = 1 << 30; // 1 GiB floor

/// Block-kind tags stored in the flat `kinds` array. A free (unallocated)
/// block is `KIND_FREE`; the two live kinds match [`BlockKind`].
const KIND_FREE: u8 = 0;
const KIND_VALUE: u8 = 1;
const KIND_DATA: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockKind {
    Value,
    Data,
}

impl BlockKind {
    #[inline]
    fn tag(self) -> u8 {
        match self {
            BlockKind::Value => KIND_VALUE,
            BlockKind::Data => KIND_DATA,
        }
    }
    #[inline]
    fn from_tag(tag: u8) -> BlockKind {
        // Only ever called for an allocated block (KIND_VALUE/KIND_DATA).
        debug_assert!(tag == KIND_VALUE || tag == KIND_DATA);
        if tag == KIND_VALUE {
            BlockKind::Value
        } else {
            BlockKind::Data
        }
    }
}

/// A borrowed, read-only view of one block's flat metadata, for the cold
/// sweep path (bump extent + whole-block liveness scan). Returned by
/// `BlockSpace::meta`. Mark bits are atomic, so `any_marked` takes `&self`.
pub struct BlockRef<'a> {
    bump: u32,
    /// This block's slice of the shared `marks` arena (BITMAP_BYTES long).
    marks: &'a [AtomicU8],
}

/// A borrowed, mutable view of one block's flat metadata (bump + start bits),
/// for the cold allocator path. Returned by `BlockSpace::meta_mut`.
pub struct BlockRefMut<'a> {
    /// Mutable handle to this block's bump offset (in the shared `bumps` array).
    bump: &'a mut u32,
    /// This block's slice of the shared `starts` arena (BITMAP_BYTES long).
    starts: &'a mut [u8],
}

// Bitmap bit ops shared by both views (granule i lives at byte i/8, bit i%8).
#[inline]
fn bm_test(bytes: &[u8], granule: usize) -> bool {
    bytes[granule / 8] & (1 << (granule % 8)) != 0
}
#[inline]
fn bm_set(bytes: &mut [u8], granule: usize) {
    bytes[granule / 8] |= 1 << (granule % 8);
}
/// Any mark bit set among the first `granules` granules of `marks`?
fn any_marked(marks: &[AtomicU8], granules: usize) -> bool {
    let full = granules / 8;
    if marks[..full].iter().any(|b| b.load(Ordering::Relaxed) != 0) {
        return true;
    }
    let rem = granules % 8;
    rem != 0 && marks[full].load(Ordering::Relaxed) & ((1u8 << rem) - 1) != 0
}
/// Granule of the object containing `granule` (scan back for a start bit).
/// None if no start bit at or before it.
fn find_start(starts: &[u8], granule: usize) -> Option<usize> {
    let mut g = granule;
    loop {
        if bm_test(starts, g) {
            return Some(g);
        }
        if g == 0 {
            return None;
        }
        g -= 1;
    }
}

// The views cover the *cold* paths (allocator bump/start-bit writes, the
// per-block sweep scan). The mark/locate hot path uses the direct
// `BlockSpace::{kind_of,is_start,set_mark,...}` accessors instead (see below),
// which avoid constructing a view per traced edge.
impl BlockRef<'_> {
    #[inline]
    pub fn bump(&self) -> usize {
        self.bump as usize
    }
    /// Any object in the first `granules` granules still marked live?
    pub fn any_marked(&self, granules: usize) -> bool {
        any_marked(self.marks, granules)
    }
}

impl BlockRefMut<'_> {
    #[inline]
    pub fn bump(&self) -> usize {
        *self.bump as usize
    }
    #[inline]
    pub fn set_bump(&mut self, bytes: usize) {
        debug_assert!(bytes <= BLOCK_SIZE);
        *self.bump = bytes as u32;
    }
    #[inline]
    pub fn set_start(&mut self, granule: usize) {
        bm_set(self.starts, granule);
    }
}

/// A large object: its own allocation, header + payload.
pub struct LargeObject {
    pub ptr: NonNull<u8>,
    pub size: usize,
    /// Atomic so parallel markers can claim it race-free (see mark bitmaps).
    pub marked: AtomicBool,
    /// Survived at least one collection (old generation). Minor collections
    /// only sweep un-aged (young) large objects.
    pub aged: bool,
}

/// The raw block space: one big reservation divided into aligned blocks, flat
/// per-block metadata arrays, and a free-block pool.
pub struct BlockSpace {
    /// Aligned base of the block reservation.
    base: usize,
    /// Total reserved bytes (address space) of the block reservation.
    reserve: usize,
    /// Bytes of the reservation handed out to blocks so far.
    committed: usize,
    // ---- flat per-block metadata (indexed by block number) ----
    /// Block kind tag: KIND_FREE / KIND_VALUE / KIND_DATA.
    kinds: Vec<u8>,
    /// Bump offset (bytes) for the allocator. A block is 32 KiB, so `u32` fits.
    bumps: Vec<u32>,
    /// Object-start bitmap arena: block `i`'s bitmap is
    /// `starts[i*BITMAP_BYTES .. (i+1)*BITMAP_BYTES]`. Bit g set iff an object
    /// starts at granule g.
    starts: Vec<u8>,
    /// Mark bitmap arena, same indexing. Bit g set iff the object starting at
    /// granule g is live (only meaningful on start granules). Atomic so
    /// parallel markers can claim bits race-free during a stop-the-world mark.
    marks: Vec<AtomicU8>,
    free: Vec<usize>, // indices of empty blocks available for reuse
    pub large: Vec<LargeObject>,
    /// Bounds of large-object mappings, for fast conservative filtering.
    large_lo: usize,
    large_hi: usize,
}

// SAFETY: single-threaded evaluator; BlockSpace is never shared across threads.
unsafe impl Send for BlockSpace {}

impl Default for BlockSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockSpace {
    pub fn new() -> Self {
        // Reserve the whole block space up front. Anonymous mappings are
        // committed lazily per page on first write, so this is address space,
        // not RSS. (Boehm and most modern runtimes do the same.)
        let mut reserve = match std::env::var("JINX_GC_RESERVE_GB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            Some(gb) if gb > 0 => gb << 30,
            _ => DEFAULT_RESERVE_BYTES,
        };
        let (base, reserve) = loop {
            let len = reserve + BLOCK_ALIGN;
            // SAFETY: plain anonymous mapping.
            let raw = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            };
            if raw != libc::MAP_FAILED {
                let base = ((raw as usize) + BLOCK_ALIGN - 1) & !(BLOCK_ALIGN - 1);
                break (base, reserve);
            }
            // Graceful fallback: halve the reservation down to the floor.
            assert!(
                reserve > MIN_RESERVE_BYTES,
                "jinx heap: reservation mmap failed even at 1 GiB"
            );
            reserve = (reserve / 2).max(MIN_RESERVE_BYTES);
        };
        BlockSpace {
            base,
            reserve,
            committed: 0,
            kinds: Vec::new(),
            bumps: Vec::new(),
            starts: Vec::new(),
            marks: Vec::new(),
            free: Vec::new(),
            large: Vec::new(),
            large_lo: usize::MAX,
            large_hi: 0,
        }
    }

    fn grow(&mut self) {
        assert!(
            self.committed + BLOCKS_PER_CHUNK * BLOCK_SIZE <= self.reserve,
            "jinx heap: block-space reservation exhausted"
        );
        let start = self.kinds.len();
        self.kinds.resize(start + BLOCKS_PER_CHUNK, KIND_FREE);
        self.bumps.resize(start + BLOCKS_PER_CHUNK, 0);
        self.starts
            .resize((start + BLOCKS_PER_CHUNK) * BITMAP_BYTES, 0);
        // AtomicU8 isn't Clone, so grow the marks arena element-by-element.
        self.marks
            .reserve((start + BLOCKS_PER_CHUNK) * BITMAP_BYTES - self.marks.len());
        for _ in 0..BLOCKS_PER_CHUNK * BITMAP_BYTES {
            self.marks.push(AtomicU8::new(0));
        }
        // Hand out low blocks first.
        for i in (start..start + BLOCKS_PER_CHUNK).rev() {
            self.free.push(i);
        }
        self.committed += BLOCKS_PER_CHUNK * BLOCK_SIZE;
    }

    /// Take an empty block for `kind`; returns (base address, meta index).
    pub fn acquire(&mut self, kind: BlockKind) -> (usize, usize) {
        if self.free.is_empty() {
            self.grow();
        }
        let idx = self.free.pop().unwrap();
        debug_assert_eq!(self.kinds[idx], KIND_FREE);
        self.kinds[idx] = kind.tag();
        self.bumps[idx] = 0;
        // Fresh block: clear its start/mark bitmaps (a previously-released block
        // left them dirty).
        let lo = idx * BITMAP_BYTES;
        for b in &mut self.starts[lo..lo + BITMAP_BYTES] {
            *b = 0;
        }
        for m in &self.marks[lo..lo + BITMAP_BYTES] {
            m.store(0, Ordering::Relaxed);
        }
        (self.base_of(idx), idx)
    }

    /// Return a fully-dead block to the free pool.
    pub fn release(&mut self, idx: usize) {
        self.kinds[idx] = KIND_FREE;
        self.free.push(idx);
    }

    #[inline]
    pub fn base_of(&self, idx: usize) -> usize {
        self.base + (idx << BLOCK_SHIFT)
    }

    /// Slice bounds of block `idx`'s bitmap in the arenas.
    #[inline]
    fn bm_range(idx: usize) -> std::ops::Range<usize> {
        let lo = idx * BITMAP_BYTES;
        lo..lo + BITMAP_BYTES
    }

    #[inline]
    pub fn meta(&self, idx: usize) -> Option<BlockRef<'_>> {
        if self.kinds[idx] == KIND_FREE {
            return None;
        }
        Some(BlockRef {
            bump: self.bumps[idx],
            marks: &self.marks[Self::bm_range(idx)],
        })
    }
    #[inline]
    pub fn meta_mut(&mut self, idx: usize) -> Option<BlockRefMut<'_>> {
        if self.kinds[idx] == KIND_FREE {
            return None;
        }
        Some(BlockRefMut {
            bump: &mut self.bumps[idx],
            starts: &mut self.starts[Self::bm_range(idx)],
        })
    }

    /// Clear every mark bit of block `idx` (used on major collection).
    pub fn clear_marks(&self, idx: usize) {
        for m in &self.marks[Self::bm_range(idx)] {
            m.store(0, Ordering::Relaxed);
        }
    }

    // ---- direct hot-path accessors (R4) ----
    //
    // The tracer visits one edge per pointer and needs only kind + one start
    // bit + one mark bit. Building a `BlockRef` view (two bounds-checked slice
    // references) per edge measured as a net loss on this workload, so the hot
    // path uses these direct, unchecked flat-array loads instead: subtract/
    // shift/load with no dependent pointer chase (the whole point of R4). All
    // take an `idx` already validated by `locate` (idx < committed/BLOCK_SIZE
    // == kinds.len(), and each flat array is sized len*{1,BITMAP_BYTES}), and a
    // `granule < BLOCK_GRANULES`, so the arena index `idx*BITMAP_BYTES +
    // granule/8` is always in bounds.

    /// Kind of block `idx`. `idx` must be a live (allocated) block.
    #[inline]
    pub fn kind_of(&self, idx: usize) -> BlockKind {
        // SAFETY: idx validated by the caller's prior `locate`.
        BlockKind::from_tag(unsafe { *self.kinds.get_unchecked(idx) })
    }

    /// Is there an object start at (block `idx`, `granule`)?
    #[inline]
    pub fn is_start(&self, idx: usize, granule: usize) -> bool {
        let byte = idx * BITMAP_BYTES + granule / 8;
        // SAFETY: byte < kinds.len()*BITMAP_BYTES == starts.len().
        unsafe { *self.starts.get_unchecked(byte) & (1 << (granule % 8)) != 0 }
    }

    /// Atomically claim mark bit at (block `idx`, `granule`); true iff newly
    /// set (so exactly one marker traces the object even under parallel
    /// marking). `Relaxed` suffices: the mutator is paused (object contents
    /// already stable), so the only synchronization needed is the atomic claim
    /// of the bit itself.
    #[inline]
    pub fn set_mark(&self, idx: usize, granule: usize) -> bool {
        let byte = idx * BITMAP_BYTES + granule / 8;
        let bit = 1u8 << (granule % 8);
        // SAFETY: byte < marks.len() (see above).
        unsafe { self.marks.get_unchecked(byte) }.fetch_or(bit, Ordering::Relaxed) & bit == 0
    }

    /// Is the mark bit at (block `idx`, `granule`) set?
    #[inline]
    pub fn is_marked(&self, idx: usize, granule: usize) -> bool {
        let byte = idx * BITMAP_BYTES + granule / 8;
        // SAFETY: byte < marks.len().
        unsafe { self.marks.get_unchecked(byte) }.load(Ordering::Relaxed) & (1 << (granule % 8)) != 0
    }

    /// Clear the mark bit at (block `idx`, `granule`) (write barrier).
    #[inline]
    pub fn clear_mark(&self, idx: usize, granule: usize) {
        let byte = idx * BITMAP_BYTES + granule / 8;
        // SAFETY: byte < marks.len().
        unsafe { self.marks.get_unchecked(byte) }
            .fetch_and(!(1u8 << (granule % 8)), Ordering::Relaxed);
    }

    /// Granule of the object containing `granule` in block `idx` (scan back for
    /// a start bit). None if no start bit at or before it.
    #[inline]
    pub fn find_start(&self, idx: usize, granule: usize) -> Option<usize> {
        find_start(&self.starts[Self::bm_range(idx)], granule)
    }

    pub fn live_block_indices(&self) -> Vec<usize> {
        (0..self.kinds.len())
            .filter(|&i| self.kinds[i] != KIND_FREE)
            .collect()
    }

    /// Conservative filter: does `addr` fall inside a live block's used space?
    /// Returns (block index, granule) if so.
    #[inline]
    pub fn locate(&self, addr: usize) -> Option<(usize, usize)> {
        let off = addr.wrapping_sub(self.base);
        if off >= self.committed || addr % GRANULE != 0 {
            return None;
        }
        let idx = off >> BLOCK_SHIFT;
        // SAFETY: idx < committed/BLOCK_SIZE == kinds.len(). One indexed load,
        // no Option/Box deref (R4).
        if unsafe { *self.kinds.get_unchecked(idx) } == KIND_FREE {
            return None;
        }
        let boff = off & (BLOCK_SIZE - 1);
        // SAFETY: idx in bounds (see above).
        if boff >= unsafe { *self.bumps.get_unchecked(idx) } as usize {
            return None;
        }
        Some((idx, boff / GRANULE))
    }

    /// Total bytes of block space ever committed (touched at least once).
    pub fn committed_bytes(&self) -> usize {
        self.committed
    }

    /// Blocks currently in the free pool.
    pub fn free_blocks(&self) -> usize {
        self.free.len()
    }

    /// Locate a large object containing `addr`.
    pub fn locate_large(&self, addr: usize) -> Option<usize> {
        if addr < self.large_lo || addr >= self.large_hi {
            return None;
        }
        self.large
            .iter()
            .position(|lo| addr >= lo.ptr.as_ptr() as usize && addr < lo.ptr.as_ptr() as usize + lo.size)
    }

    pub fn alloc_large(&mut self, size: usize) -> NonNull<u8> {
        let len = (size + 4095) & !4095;
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert!(raw != libc::MAP_FAILED, "jinx heap: large mmap failed");
        let ptr = NonNull::new(raw as *mut u8).unwrap();
        self.large_lo = self.large_lo.min(raw as usize);
        self.large_hi = self.large_hi.max(raw as usize + len);
        self.large.push(LargeObject {
            ptr,
            size: len,
            marked: AtomicBool::new(false),
            aged: false,
        });
        ptr
    }
}
