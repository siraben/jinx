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

use std::ptr::NonNull;

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockKind {
    Value,
    Data,
}

/// Out-of-line per-block metadata (kept out of the block itself so the block
/// payload stays fully usable and pointer math stays trivial).
pub struct BlockMeta {
    pub kind: BlockKind,
    /// Object-start bitmap: bit i set iff an object starts at granule i.
    pub starts: Box<[u8; BITMAP_BYTES]>,
    /// Mark bitmap: bit i set iff the object starting at granule i is live.
    /// (Only meaningful on start granules.)
    pub marks: Box<[u8; BITMAP_BYTES]>,
    /// Current bump offset (bytes) for the allocator.
    pub bump: usize,
}

impl BlockMeta {
    fn new(kind: BlockKind) -> Self {
        BlockMeta {
            kind,
            starts: Box::new([0u8; BITMAP_BYTES]),
            marks: Box::new([0u8; BITMAP_BYTES]),
            bump: 0,
        }
    }

    #[inline]
    pub fn set_start(&mut self, granule: usize) {
        self.starts[granule / 8] |= 1 << (granule % 8);
    }
    #[inline]
    pub fn is_start(&self, granule: usize) -> bool {
        self.starts[granule / 8] & (1 << (granule % 8)) != 0
    }
    #[inline]
    pub fn set_mark(&mut self, granule: usize) -> bool {
        let b = &mut self.marks[granule / 8];
        let bit = 1 << (granule % 8);
        let was = *b & bit != 0;
        *b |= bit;
        !was
    }
    #[inline]
    pub fn is_marked(&self, granule: usize) -> bool {
        self.marks[granule / 8] & (1 << (granule % 8)) != 0
    }
    #[inline]
    #[allow(dead_code)]
    pub fn clear_mark(&mut self, granule: usize) {
        self.marks[granule / 8] &= !(1 << (granule % 8));
    }
    pub fn clear_marks(&mut self) {
        self.marks.fill(0);
    }
    /// Any mark bit set among the first `granules` granules?
    #[allow(dead_code)]
    pub fn any_marked(&self, granules: usize) -> bool {
        let full = granules / 8;
        if self.marks[..full].iter().any(|&b| b != 0) {
            return true;
        }
        let rem = granules % 8;
        rem != 0 && self.marks[full] & ((1u8 << rem) - 1) != 0
    }
    /// Find the granule of the object containing `granule` (scan back for a
    /// start bit). Returns None if no start bit at or before it.
    pub fn find_start(&self, granule: usize) -> Option<usize> {
        let mut g = granule;
        loop {
            if self.is_start(g) {
                return Some(g);
            }
            if g == 0 {
                return None;
            }
            g -= 1;
        }
    }
}

/// A large object: its own allocation, header + payload.
pub struct LargeObject {
    pub ptr: NonNull<u8>,
    pub size: usize,
    pub marked: bool,
}

/// The raw block space: one big reservation divided into aligned blocks, a
/// flat table mapping block index to metadata, and a free-block pool.
pub struct BlockSpace {
    /// Aligned base of the block reservation.
    base: usize,
    /// Total reserved bytes (address space) of the block reservation.
    reserve: usize,
    /// Bytes of the reservation handed out to blocks so far.
    committed: usize,
    metas: Vec<Option<BlockMeta>>,
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
            metas: Vec::new(),
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
        let start = self.metas.len();
        for _ in 0..BLOCKS_PER_CHUNK {
            self.metas.push(None);
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
        debug_assert!(self.metas[idx].is_none());
        self.metas[idx] = Some(BlockMeta::new(kind));
        (self.base_of(idx), idx)
    }

    /// Return a fully-dead block to the free pool.
    pub fn release(&mut self, idx: usize) {
        self.metas[idx] = None;
        self.free.push(idx);
    }

    #[inline]
    pub fn base_of(&self, idx: usize) -> usize {
        self.base + (idx << BLOCK_SHIFT)
    }

    #[inline]
    pub fn meta(&self, idx: usize) -> Option<&BlockMeta> {
        self.metas[idx].as_ref()
    }
    #[inline]
    pub fn meta_mut(&mut self, idx: usize) -> Option<&mut BlockMeta> {
        self.metas[idx].as_mut()
    }

    pub fn live_block_indices(&self) -> Vec<usize> {
        (0..self.metas.len())
            .filter(|&i| self.metas[i].is_some())
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
        // SAFETY: idx < committed/BLOCK_SIZE == metas.len().
        let meta = unsafe { self.metas.get_unchecked(idx).as_ref()? };
        let boff = off & (BLOCK_SIZE - 1);
        if boff >= meta.bump {
            return None;
        }
        Some((idx, boff / GRANULE))
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
            marked: false,
        });
        ptr
    }
}
