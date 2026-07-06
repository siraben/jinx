//! Block-structured heap memory: 32 KiB aligned blocks carved from mmap,
//! with per-block object-start and mark bitmaps (granule = 8 bytes).
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
pub const GRANULE: usize = 8;
/// Granules per block.
pub const BLOCK_GRANULES: usize = BLOCK_SIZE / GRANULE; // 4096
/// Bitmap bytes per block (1 bit per granule).
pub const BITMAP_BYTES: usize = BLOCK_GRANULES / 8; // 512

/// Objects at or above this size go to the large-object space.
pub const LARGE_OBJECT_MIN: usize = 8 * 1024;

/// How many blocks to reserve per mmap chunk (16 MiB chunks).
const BLOCKS_PER_CHUNK: usize = 512;

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
    pub fn clear_marks(&mut self) {
        self.marks.fill(0);
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

/// The raw block space: mmap'd chunks divided into aligned blocks, a registry
/// mapping block base addresses to metadata, and a free-block pool.
pub struct BlockSpace {
    /// Block base address -> index into `metas`.
    registry: rustc_hash::FxHashMap<usize, usize>,
    metas: Vec<Option<BlockMeta>>,
    bases: Vec<usize>,
    free: Vec<usize>, // indices of empty blocks available for reuse
    chunks: Vec<(NonNull<u8>, usize)>,
    pub large: Vec<LargeObject>,
    /// Bounds of all mapped chunks, for fast conservative filtering.
    lo: usize,
    hi: usize,
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
        BlockSpace {
            registry: rustc_hash::FxHashMap::default(),
            metas: Vec::new(),
            bases: Vec::new(),
            free: Vec::new(),
            chunks: Vec::new(),
            large: Vec::new(),
            lo: usize::MAX,
            hi: 0,
        }
    }

    fn map_chunk(&mut self) {
        let len = BLOCKS_PER_CHUNK * BLOCK_SIZE + BLOCK_ALIGN;
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
        assert!(raw != libc::MAP_FAILED, "jinx heap: mmap failed");
        let base = raw as usize;
        let aligned = (base + BLOCK_ALIGN - 1) & !(BLOCK_ALIGN - 1);
        for i in 0..BLOCKS_PER_CHUNK {
            let b = aligned + i * BLOCK_SIZE;
            let idx = self.metas.len();
            self.metas.push(None);
            self.bases.push(b);
            self.registry.insert(b, idx);
            self.free.push(idx);
        }
        self.lo = self.lo.min(aligned);
        self.hi = self.hi.max(aligned + BLOCKS_PER_CHUNK * BLOCK_SIZE);
        self.chunks
            .push((NonNull::new(raw as *mut u8).unwrap(), len));
        // Hand out low blocks first.
        self.free.reverse();
    }

    /// Take an empty block for `kind`; returns (base address, meta index).
    pub fn acquire(&mut self, kind: BlockKind) -> (usize, usize) {
        if self.free.is_empty() {
            self.map_chunk();
        }
        let idx = self.free.pop().unwrap();
        debug_assert!(self.metas[idx].is_none());
        self.metas[idx] = Some(BlockMeta::new(kind));
        (self.bases[idx], idx)
    }

    /// Return a fully-dead block to the free pool.
    pub fn release(&mut self, idx: usize) {
        self.metas[idx] = None;
        self.free.push(idx);
    }

    #[inline]
    pub fn base_of(&self, idx: usize) -> usize {
        self.bases[idx]
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
        if addr < self.lo || addr >= self.hi || addr % GRANULE != 0 {
            return None;
        }
        let base = addr & !(BLOCK_ALIGN - 1);
        let &idx = self.registry.get(&base)?;
        let meta = self.metas[idx].as_ref()?;
        let off = addr - base;
        if off >= meta.bump {
            return None;
        }
        Some((idx, off / GRANULE))
    }

    /// Locate a large object containing `addr`.
    pub fn locate_large(&self, addr: usize) -> Option<usize> {
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
        self.lo = self.lo.min(raw as usize);
        self.hi = self.hi.max(raw as usize + len);
        self.large.push(LargeObject {
            ptr,
            size: len,
            marked: false,
        });
        ptr
    }
}
