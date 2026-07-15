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
/// Value-cell size (bytes): uniform, headerless, 2 granules. Matches
/// `value::VALUE_SIZE`; defined here so mem.rs stays free of a value.rs dep.
pub const CELL_SIZE: usize = 2 * GRANULE; // 16
/// Granules per block.
pub const BLOCK_GRANULES: usize = BLOCK_SIZE / GRANULE; // 4096
/// Bitmap bytes per block (1 bit per granule).
pub const BITMAP_BYTES: usize = BLOCK_GRANULES / 8; // 512
/// Immix-style allocation line for variable-sized data-object recycling.
/// 128 B gives 256 lines per block and bounds internal fragmentation while
/// keeping the sweep bitmap tiny enough to live on the collector stack.
pub const DATA_LINE_SIZE: usize = 128;
pub const DATA_LINES_PER_BLOCK: usize = BLOCK_SIZE / DATA_LINE_SIZE;

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
    /// B1 allocation bitmap arena, same indexing. For a *recyclable* value block
    /// it is a SNAPSHOT of the mark bitmap taken at the major sweep (bit set =
    /// live/occupied at recycle time = NOT free). The cell-recycling allocator
    /// (`next_free_cell`) reads THIS, never the live mark bitmap: the generational
    /// write barrier clears a live old cell's *mark* bit between collections
    /// (remset logging), so the mark bitmap does not stay a valid free/occupied
    /// map during the mutator phase — but `allocs` is immutable until the next
    /// major re-snapshots it, so a barrier-cleared live cell is never mistaken
    /// for free (the exact bug the snapshot fixes; Go's allocBits vs gcmarkBits).
    allocs: Vec<u8>,
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
            allocs: Vec::new(),
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
        self.allocs
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

    // ---- B1: value-block cell recycling (Go allocCache pattern) ----
    //
    // A recyclable value block is one that survived a *major* sweep with some
    // (but not all) of its 16-byte cells still live. Its free set is exactly the
    // complement of the mark bitmap among the cells below the high-water bump.
    // The allocator drains it by scanning forward with a monotone cell cursor
    // (Go's `freeindex`): we never revisit a cell behind the cursor within a
    // mutator epoch, so a handed-out cell can't be handed out twice even though
    // we never set its mark on allocation (marks are the collector's; they stay
    // = survivors until the next major recomputes them).
    //
    // Cells are 16 B = 2 granules; a cell starting at byte `c*16` occupies the
    // even granule `2*c`, so cell marks live only on even granules. We scan a
    // 64-bit word of the mark arena at a time (`allocCache = !marks_word`) and
    // ctz to the next free *even* granule.

    /// Snapshot the mark bitmap of value block `idx` into its allocation bitmap
    /// (`allocs[idx] = marks[idx]`). Called at major-sweep recycle classification
    /// so the recycling allocator has an immutable free-set the generational
    /// write barrier can't corrupt (see the `allocs` field doc).
    pub fn snapshot_alloc_bitmap(&mut self, idx: usize) {
        debug_assert_eq!(self.kinds[idx], KIND_VALUE);
        let r = Self::bm_range(idx);
        for k in 0..BITMAP_BYTES {
            self.allocs[r.start + k] = self.marks[r.start + k].load(Ordering::Relaxed);
        }
    }

    /// Next free 16-byte cell in value block `idx` at or after cell index
    /// `from_cell` (i.e. even granule `2*from_cell`), scanning below the block's
    /// high-water bump. Returns the byte offset of the cell within the block, or
    /// `None` if the block is drained. Reads the immutable **allocation bitmap**
    /// snapshot (not the live marks — the barrier mutates marks mid-mutator); a
    /// clear alloc bit means the cell was dead at recycle time. The monotone
    /// cursor guarantees each cell is handed out at most once per epoch.
    pub fn next_free_cell(&self, idx: usize, from_cell: usize) -> Option<usize> {
        debug_assert_eq!(self.kinds[idx], KIND_VALUE);
        let bump_cells = (self.bumps[idx] as usize) / CELL_SIZE;
        if from_cell >= bump_cells {
            return None;
        }
        let allocs = &self.allocs[Self::bm_range(idx)];
        let mut cell = from_cell;
        while cell < bump_cells {
            // The word of 64 granules (= 32 cells) containing granule 2*cell.
            let granule = cell * 2;
            let word_idx = granule / 64; // 8 alloc bytes per 64-granule word
            // Load the 8-byte alloc word (little-endian) at this word.
            let base = word_idx * 8;
            let mut word = 0u64;
            for k in 0..8 {
                word |= (allocs[base + k] as u64) << (k * 8);
            }
            // Keep only even-granule bits (cell starts); complement so 1 = free.
            const EVEN: u64 = 0x5555_5555_5555_5555;
            let bit_in_word = granule % 64;
            // Free even granules at or after our position within this word.
            let free = (!word) & EVEN & (!0u64 << bit_in_word);
            if free != 0 {
                let g = word_idx * 64 + free.trailing_zeros() as usize;
                let c = g / 2;
                if c >= bump_cells {
                    return None;
                }
                return Some(c * CELL_SIZE);
            }
            // Advance to the start of the next word.
            cell = ((word_idx + 1) * 64) / 2;
        }
        None
    }

    /// Set the start bit for a recycled cell (cell start bits are per-granule
    /// and persist across lives; re-setting is idempotent and keeps the
    /// invariant "an allocated cell has its start bit set" exact even if a prior
    /// occupant's start bit had somehow been cleared).
    #[inline]
    pub fn set_start_at(&mut self, idx: usize, byte_off: usize) {
        let granule = byte_off / GRANULE;
        let byte = idx * BITMAP_BYTES + granule / 8;
        self.starts[byte] |= 1 << (granule % 8);
    }

    /// After a completed major mark, remove dead data-object starts and return runs
    /// of complete 128-byte lines untouched by any live object. Objects that
    /// cross a line protect every line they touch. The caller may subsequently
    /// allocate within these runs without moving survivors.
    pub fn recyclable_data_runs(&mut self, idx: usize) -> Vec<(usize, usize)> {
        debug_assert_eq!(self.kinds[idx], KIND_DATA);
        let bump = self.bumps[idx] as usize;
        let base = self.base_of(idx);
        let mut occupied = [false; DATA_LINES_PER_BLOCK];
        let mut off = 0usize;
        while off < bump {
            let g = off / GRANULE;
            if !self.is_start(idx, g) {
                off += GRANULE;
                continue;
            }
            // SAFETY: start bits are installed only for allocated data
            // objects, whose header remains valid until this major sweep.
            let h = unsafe { *((base + off) as *const u64) };
            let rounded = crate::value::obj_size_bytes(h).div_ceil(GRANULE) * GRANULE;
            debug_assert!(rounded > 0 && off + rounded <= bump);
            if self.is_marked(idx, g) {
                let first = off / DATA_LINE_SIZE;
                let last = (off + rounded - 1) / DATA_LINE_SIZE;
                for line in &mut occupied[first..=last] {
                    *line = true;
                }
            } else {
                let byte = idx * BITMAP_BYTES + g / 8;
                self.starts[byte] &= !(1 << (g % 8));
            }
            off += rounded;
        }

        let mut runs = Vec::new();
        let mut line = 0usize;
        while line < DATA_LINES_PER_BLOCK {
            if occupied[line] {
                line += 1;
                continue;
            }
            let start = line;
            while line < DATA_LINES_PER_BLOCK && !occupied[line] {
                line += 1;
            }
            runs.push((start * DATA_LINE_SIZE, line * DATA_LINE_SIZE));
        }
        runs
    }

    /// Expand a data block's used extent after allocating from recycled lines
    /// that may lie above its former bump frontier.
    #[inline]
    pub fn raise_bump(&mut self, idx: usize, end: usize) {
        debug_assert!(end <= BLOCK_SIZE);
        self.bumps[idx] = self.bumps[idx].max(end as u32);
    }

    /// True iff block `idx` is a value block. Used by the sweeper to decide
    /// recyclability. `idx` must be a live block.
    #[inline]
    pub fn is_value_block(&self, idx: usize) -> bool {
        self.kinds[idx] == KIND_VALUE
    }

    #[inline]
    pub fn is_data_block(&self, idx: usize) -> bool {
        self.kinds[idx] == KIND_DATA
    }

    /// Bump offset (bytes) of block `idx`. Cheap direct load for the recycler.
    #[inline]
    pub fn bump_of(&self, idx: usize) -> usize {
        self.bumps[idx] as usize
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_free_cell_masking() {
        let mut sp = BlockSpace::new();
        let (_base, idx) = sp.acquire(BlockKind::Value);
        // Simulate 10 allocated cells: bump = 10*16.
        {
            let mut m = sp.meta_mut(idx).unwrap();
            m.set_bump(10 * CELL_SIZE);
            for c in 0..10 {
                m.set_start(c * 2); // start bits at even granules
            }
        }
        // Mark cells 0,1,2 and 7 live (granules 0,2,4,14).
        for &c in &[0usize, 1, 2, 7] {
            assert!(sp.set_mark(idx, c * 2));
        }
        sp.snapshot_alloc_bitmap(idx);
        // Free cells below bump: 3,4,5,6,8,9.
        let mut got = Vec::new();
        let mut cur = 0;
        while let Some(off) = sp.next_free_cell(idx, cur) {
            let c = off / CELL_SIZE;
            got.push(c);
            cur = c + 1;
        }
        assert_eq!(got, vec![3, 4, 5, 6, 8, 9]);
    }

    #[test]
    fn next_free_cell_cross_word() {
        let mut sp = BlockSpace::new();
        let (_b, idx) = sp.acquire(BlockKind::Value);
        // 40 cells; cell 32 lives in the 2nd 64-granule word (granule 64).
        {
            let mut m = sp.meta_mut(idx).unwrap();
            m.set_bump(40 * CELL_SIZE);
        }
        sp.set_mark(idx, 32 * 2); // granule 64
        sp.snapshot_alloc_bitmap(idx);
        // From cell 30, free cells are 30,31,33,34,...,39 (32 is live).
        let mut got = Vec::new();
        let mut cur = 30;
        while let Some(off) = sp.next_free_cell(idx, cur) {
            let c = off / CELL_SIZE;
            got.push(c);
            cur = c + 1;
        }
        assert_eq!(got, vec![30, 31, 33, 34, 35, 36, 37, 38, 39]);
    }

}
