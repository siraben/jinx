//! Value representation: 16-byte cells (two u64 words) in value blocks, with
//! variable-sized payloads (strings, lists, bindings, thunks, ...) in data
//! blocks. Non-moving; thunks are forced by overwriting their cell in place.
//!
//! Cell layout:
//!   w0: [ tag: u8 | unused ]
//!   w1: payload (immediate i64/f64, or pointer to a data object)
//!
//! Data objects start with a one-word `ObjHeader`: [ kind: u8 | len: u56 ].
//! `len` is the element count whose unit depends on the kind (bytes for
//! strings/paths, entries for lists/bindings/upvalue arrays).

use std::ptr::NonNull;

/// Handle to a heap value cell.
pub type VRef = NonNull<Value>;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Value {
    pub w0: u64,
    pub w1: u64,
}

pub const VALUE_SIZE: usize = 16;

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tag {
    Null = 0,
    False = 1,
    True = 2,
    Int = 3,
    Float = 4,
    String = 5,
    Path = 6,
    Attrs = 7,
    List = 8,
    Thunk = 9,
    Closure = 10,
    PrimOp = 11,
    PrimOpApp = 12,
    Blackhole = 13,
    /// Cached evaluation failure; w1 = index into the VM's error table.
    Failed = 14,
}

pub const NUM_TAGS: u8 = 15;

/// Data-object kinds (first byte of ObjHeader).
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjKind {
    /// len = byte length. Layout: header, ctx: *mut Obj (Ctx) or null, bytes...
    Str = 0,
    /// len = element count. Layout: header, elems: [*mut Value]...
    List = 1,
    /// len = entry count. Layout: header, entries: [(sym u32, pos u32, *mut Value)]...
    Bindings = 2,
    /// len = element count. Layout: header, elems: [*mut Value]...
    Upvals = 3,
    /// len = element count. Layout: header, elems: [u32 context-elem ids]... (padded)
    Ctx = 4,
    /// len = byte length. Layout: header, accessor: u64, bytes...
    Path = 5,
    /// len = captured arg count. Layout: header, code: *const () [immortal],
    /// elems: [*mut Value]...   (thunks and closures share this shape)
    Thunk = 6,
    /// len = applied arg count. Layout: header, prim: *const () [immortal],
    /// elems: [*mut Value]...
    PrimApp = 7,
}

#[inline]
pub fn header(kind: ObjKind, len: usize) -> u64 {
    debug_assert!(len < (1u64 << 56) as usize);
    (kind as u64) | ((len as u64) << 8)
}

#[inline]
pub fn header_kind(h: u64) -> ObjKind {
    // SAFETY: headers are only written via `header()`.
    unsafe { std::mem::transmute::<u8, ObjKind>((h & 0xff) as u8) }
}

#[inline]
pub fn header_len(h: u64) -> usize {
    (h >> 8) as usize
}

/// Total size in bytes (unrounded) of a data object given its header.
pub fn obj_size_bytes(h: u64) -> usize {
    let len = header_len(h);
    match header_kind(h) {
        ObjKind::Str => 8 + 8 + len,            // header + ctx ptr + bytes
        ObjKind::List | ObjKind::Upvals => 8 + len * 8,
        ObjKind::Bindings => 8 + len * 16,      // (sym,pos,ptr)
        ObjKind::Ctx => 8 + len.div_ceil(2) * 8, // u32s padded to words
        ObjKind::Path => 8 + 8 + len,           // header + accessor + bytes
        ObjKind::Thunk | ObjKind::PrimApp => 8 + 8 + len * 8, // header + code/prim + elems
    }
}

impl Value {
    #[inline]
    pub fn tag(&self) -> Tag {
        // SAFETY: w0's low byte is always a valid Tag.
        unsafe { std::mem::transmute::<u8, Tag>((self.w0 & 0xff) as u8) }
    }

    #[inline]
    pub fn make(tag: Tag, w1: u64) -> Value {
        Value {
            w0: tag as u64,
            w1,
        }
    }

    #[inline]
    pub fn null() -> Value {
        Value::make(Tag::Null, 0)
    }
    #[inline]
    pub fn bool(b: bool) -> Value {
        Value::make(if b { Tag::True } else { Tag::False }, 0)
    }
    #[inline]
    pub fn int(i: i64) -> Value {
        Value::make(Tag::Int, i as u64)
    }
    #[inline]
    pub fn float(f: f64) -> Value {
        Value::make(Tag::Float, f.to_bits())
    }

    #[inline]
    pub fn as_int(&self) -> i64 {
        debug_assert_eq!(self.tag(), Tag::Int);
        self.w1 as i64
    }
    #[inline]
    pub fn as_float(&self) -> f64 {
        debug_assert_eq!(self.tag(), Tag::Float);
        f64::from_bits(self.w1)
    }
    #[inline]
    pub fn as_bool(&self) -> bool {
        debug_assert!(matches!(self.tag(), Tag::True | Tag::False));
        self.tag() == Tag::True
    }

    /// Pointer payload (data object), for pointer-tagged values.
    #[inline]
    pub fn ptr(&self) -> *mut u64 {
        self.w1 as *mut u64
    }

    /// Does this value's payload point into the GC heap?
    #[inline]
    pub fn has_heap_payload(&self) -> bool {
        matches!(
            self.tag(),
            Tag::String
                | Tag::Path
                | Tag::Attrs
                | Tag::List
                | Tag::Thunk
                | Tag::Closure
                | Tag::PrimOpApp
        )
    }
}

// ---- typed views over data objects (unsafe, invariant-carrying) ----

/// String payload: (bytes, context object ptr or null).
///
/// # Safety
/// `p` must point to a live `ObjKind::Str` object.
pub unsafe fn str_parts<'a>(p: *const u64) -> (&'a [u8], *mut u64) {
    let h = *p;
    debug_assert_eq!(header_kind(h), ObjKind::Str);
    let len = header_len(h);
    let ctx = *p.add(1) as *mut u64;
    let bytes = std::slice::from_raw_parts(p.add(2) as *const u8, len);
    (bytes, ctx)
}

/// # Safety
/// `p` must point to a live `ObjKind::List` or `ObjKind::Upvals` object.
pub unsafe fn elems<'a>(p: *const u64) -> &'a [VRef] {
    let h = *p;
    debug_assert!(matches!(
        header_kind(h),
        ObjKind::List | ObjKind::Upvals
    ));
    std::slice::from_raw_parts(p.add(1) as *const VRef, header_len(h))
}

/// Bindings entry: interned symbol, source position, value cell.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Attr {
    pub sym: u32,
    pub pos: u32,
    pub val: VRef,
}

/// # Safety
/// `p` must point to a live `ObjKind::Bindings` object.
pub unsafe fn bindings<'a>(p: *const u64) -> &'a [Attr] {
    let h = *p;
    debug_assert_eq!(header_kind(h), ObjKind::Bindings);
    std::slice::from_raw_parts(p.add(1) as *const Attr, header_len(h))
}

/// # Safety
/// `p` must point to a live `ObjKind::Path` object.
pub unsafe fn path_parts<'a>(p: *const u64) -> (u64, &'a [u8]) {
    let h = *p;
    debug_assert_eq!(header_kind(h), ObjKind::Path);
    let accessor = *p.add(1);
    let bytes = std::slice::from_raw_parts(p.add(2) as *const u8, header_len(h));
    (accessor, bytes)
}

/// # Safety
/// `p` must point to a live `ObjKind::Thunk` or `ObjKind::PrimApp` object.
pub unsafe fn code_and_elems<'a>(p: *const u64) -> (*const (), &'a [VRef]) {
    let h = *p;
    debug_assert!(matches!(
        header_kind(h),
        ObjKind::Thunk | ObjKind::PrimApp
    ));
    let code = *p.add(1) as *const ();
    let elems = std::slice::from_raw_parts(p.add(2) as *const VRef, header_len(h));
    (code, elems)
}
