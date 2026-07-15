//! Value representation: 16-byte cells (two u64 words) in value blocks, with
//! variable-sized payloads (strings, lists, bindings, thunks, ...) in data
//! blocks. Non-moving; thunks are forced by overwriting their cell in place.
//!
//! Cell layout:
//!   w0: [ tag: u8 | unused ] (or tag + packed representation metadata)
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

/// Maximum prefix retained by an inline list-tail view. A view points at the
/// original flat list payload and stores its start offset in `Value::w0`, so a
/// chain has no extra data objects and never grows in depth. Once the offset
/// reaches this bound, `builtins.tail` materializes the remaining elements and
/// resets the offset. `builtins.tail` additionally admits a view only when the
/// skipped element is pointer-free WHNF, so sharing cannot invisibly retain an
/// arbitrary element graph. The resulting extra retention is bounded to 16
/// value cells plus 16 pointers in the original list payload.
pub const MAX_LIST_TAIL_OFFSET: usize = 16;

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
    /// Capture-free (nullary) thunk packed into the cell: w1 = the immortal
    /// `*const CodeRef` directly -- no Thunk data object is allocated.
    ///
    /// The interpreter and JIT enumerate [`Value::needs_force`] tags
    /// explicitly; packed closures and inline strings are WHNF despite sharing
    /// the high range.
    Thunk0 = 15,
    /// Blackholed `Thunk0`; w1 = the `*const CodeRef` (for `determine_pos`).
    Blackhole0 = 16,
    /// One-capture thunk packed entirely into the value cell. The CodeRef
    /// pointer is compressed into w0's upper 56 bits and w1 is the capture.
    Thunk1 = 17,
    /// Blackholed `Thunk1`; retains code and capture while it is running.
    Blackhole1 = 18,
    /// Capture-free closure packed into the cell; w1 is an immortal CodeRef.
    Closure0 = 19,
    /// One-capture closure packed like `Thunk1`.
    Closure1 = 20,
    /// Context-free string of at most 14 bytes packed directly in the cell.
    /// Byte 1 of w0 is the length; bytes 2..16 contain the string bytes.
    SmallString = 21,
}

pub const NUM_TAGS: u8 = 22;

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
    /// Legacy thunk payload shape: header, code, captures. New generic thunks
    /// and closures store their immortal code pointer in `Value::w0` and use
    /// `Upvals` below, but the kind remains readable for heap compatibility
    /// and for tests that exercise object tracing directly.
    Thunk = 6,
    /// len = applied arg count. Layout: header, prim: *const () [immortal],
    /// elems: [*mut Value]...
    PrimApp = 7,
    /// len = local entry count. Layout: header, base bindings pointer,
    /// packed(total_len: u32, depth: u8), local entries.  Layers are used for
    /// small right operands of `//`; lookup walks newest-to-oldest and ordered
    /// iteration performs a bounded merge.
    BindingsLayer = 8,
    /// len = entry count. Layout: header, immortal `*const AttrsDesc`,
    /// values: [*mut Value]. Names and positions are shared with the compiled
    /// program instead of being duplicated in every runtime instance.
    BindingsStatic = 9,
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
        ObjKind::BindingsLayer => 24 + len * 16, // header + base + metadata + entries
        ObjKind::BindingsStatic => 16 + len * 8, // header + shape + value pointers
        ObjKind::Ctx => 8 + len.div_ceil(2) * 8, // u32s padded to words
        ObjKind::Path => 8 + 8 + len,           // header + accessor + bytes
        ObjKind::Thunk | ObjKind::PrimApp => 8 + 8 + len * 8, // header + code/prim + elems
    }
}

impl Value {
    /// Top bit of a generic thunk/closure cell. Code pointers occupy the other
    /// 55 metadata bits after 8-byte compression; this bit says that the
    /// Upvals payload is an owner's longer array and must be logically sliced
    /// to the child chunk's capture count.
    const SHARED_ENV_BIT: u64 = 1 << 63;

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

    /// CodeRef is 8-byte aligned, so w0's 56 spare bits represent 59-bit
    /// virtual addresses. Fail rather than truncate on an exotic target.
    #[inline]
    pub fn packed_code(tag: Tag, code: *const (), capture: VRef) -> Value {
        let p = code as usize as u64;
        debug_assert_eq!(p & 7, 0);
        let compressed = p >> 3;
        assert!(compressed < (1u64 << 56), "CodeRef pointer exceeds packed range");
        Value { w0: (compressed << 8) | tag as u64, w1: capture.as_ptr() as u64 }
    }

    #[inline]
    pub fn packed_env(tag: Tag, code: *const (), env: *mut u64) -> Value {
        debug_assert!(matches!(tag, Tag::Thunk | Tag::Closure));
        let p = code as usize as u64;
        debug_assert_eq!(p & 7, 0);
        let compressed = p >> 3;
        assert!(
            compressed < (1u64 << 55),
            "CodeRef pointer exceeds packed environment range"
        );
        Value {
            w0: (compressed << 8) | tag as u64,
            w1: env as u64,
        }
    }

    #[inline]
    pub fn shared_env(tag: Tag, code: *const (), owner: *mut u64) -> Value {
        let mut value = Self::packed_env(tag, code, owner);
        value.w0 |= Self::SHARED_ENV_BIT;
        value
    }

    #[inline]
    pub fn is_shared_env(&self) -> bool {
        matches!(self.tag(), Tag::Thunk | Tag::Closure | Tag::Blackhole)
            && self.w0 & Self::SHARED_ENV_BIT != 0
    }

    #[inline]
    pub fn unpacked_code(&self) -> *const () {
        let mut compressed = self.w0 >> 8;
        if matches!(self.tag(), Tag::Thunk | Tag::Closure | Tag::Blackhole) {
            compressed &= (1u64 << 55) - 1;
        }
        ((compressed << 3) as usize) as *const ()
    }

    #[inline]
    pub fn packed_capture(&self) -> Option<VRef> {
        if matches!(self.tag(), Tag::Thunk1 | Tag::Blackhole1 | Tag::Closure1) {
            NonNull::new(self.w1 as *mut Value)
        } else {
            None
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

    /// Return a zero-allocation view of this list without its first element.
    /// The upper 56 bits of `w0` hold the offset into the flat list payload.
    /// Views are bounded so keeping a short suffix cannot retain an unbounded
    /// prefix; callers copy when this returns `None`.
    #[inline]
    pub fn list_tail_view(&self) -> Option<Value> {
        debug_assert_eq!(self.tag(), Tag::List);
        let next = self.list_offset() + 1;
        (next <= MAX_LIST_TAIL_OFFSET).then_some(Value {
            w0: (next as u64) << 8 | Tag::List as u64,
            w1: self.w1,
        })
    }

    #[inline]
    pub fn list_offset(&self) -> usize {
        debug_assert_eq!(self.tag(), Tag::List);
        (self.w0 >> 8) as usize
    }

    /// Whether retaining this already-evaluated value cell can keep no mutable
    /// or GC-managed graph alive. Used to prevent an otherwise-dead skipped
    /// list element from becoming an unbounded retention edge through a tail
    /// view. These tags are immutable WHNF and carry only immediate data or an
    /// immortal code pointer.
    #[inline]
    pub fn is_pointer_free_whnf(&self) -> bool {
        matches!(
            self.tag(),
            Tag::Null
                | Tag::False
                | Tag::True
                | Tag::Int
                | Tag::Float
                | Tag::PrimOp
                | Tag::Closure0
                | Tag::SmallString
        )
    }

    #[inline]
    pub fn small_string(bytes: &[u8]) -> Option<Value> {
        if bytes.len() > 14 {
            return None;
        }
        let mut raw = [0u8; VALUE_SIZE];
        raw[0] = Tag::SmallString as u8;
        raw[1] = bytes.len() as u8;
        raw[2..2 + bytes.len()].copy_from_slice(bytes);
        Some(Value {
            w0: u64::from_ne_bytes(raw[..8].try_into().unwrap()),
            w1: u64::from_ne_bytes(raw[8..].try_into().unwrap()),
        })
    }

    #[inline]
    pub fn is_string(&self) -> bool {
        matches!(self.tag(), Tag::String | Tag::SmallString)
    }

    #[inline]
    pub fn small_string_bytes(&self) -> &[u8] {
        debug_assert_eq!(self.tag(), Tag::SmallString);
        let len = self.w0.to_ne_bytes()[1] as usize;
        debug_assert!(len <= 14);
        // SAFETY: Value is exactly 16 bytes and inline bytes occupy offsets
        // 2..16. The returned slice is tied to this Value reference.
        unsafe { std::slice::from_raw_parts((self as *const Value).cast::<u8>().add(2), len) }
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
                // A blackhole retains its thunk's data pointer (w1) so that
                // `determinePos` can recover the position of the expression
                // being computed for infinite-recursion diagnostics. It must
                // therefore be traced by the GC exactly like a Thunk. A bare
                // (w1 == 0) blackhole sentinel traces to null, which
                // `mark_data` ignores.
                | Tag::Blackhole
            // Packed forms and inline strings carry no data-object pointer.
            // Packed cell edges are handled explicitly by the marker.
        )
    }

    /// Thunk-like tags that `force` must handle (everything else is WHNF).
    #[inline]
    pub fn needs_force(&self) -> bool {
        matches!(
            self.tag(),
            Tag::Thunk | Tag::Thunk0 | Tag::Thunk1 | Tag::Blackhole | Tag::Blackhole0
                | Tag::Blackhole1 | Tag::Failed
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
/// `p` must point to a live `ObjKind::BindingsStatic` object. The descriptor
/// belongs to an immortal leaked Program and has exactly `header_len(p)` names.
pub unsafe fn bindings_static<'a>(
    p: *const u64,
) -> (&'static crate::chunk::AttrsDesc, &'a [VRef]) {
    let h = *p;
    debug_assert_eq!(header_kind(h), ObjKind::BindingsStatic);
    let desc = &*(*p.add(1) as *const crate::chunk::AttrsDesc);
    let len = header_len(h);
    debug_assert_eq!(desc.names.len(), len);
    (desc, std::slice::from_raw_parts(p.add(2) as *const VRef, len))
}

/// # Safety
/// `p` must point to a live `ObjKind::BindingsLayer` object.
pub unsafe fn bindings_layer<'a>(p: *const u64) -> (*mut u64, usize, u8, &'a [Attr]) {
    let h = *p;
    debug_assert_eq!(header_kind(h), ObjKind::BindingsLayer);
    let meta = *p.add(2);
    (
        *p.add(1) as *mut u64,
        meta as u32 as usize,
        ((meta >> 32) & 0xff) as u8,
        std::slice::from_raw_parts(p.add(3) as *const Attr, header_len(h)),
    )
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

#[cfg(test)]
mod small_string_tests {
    use super::*;

    #[test]
    fn inline_bytes_cover_boundaries_and_embedded_nul() {
        for bytes in [
            b"".as_slice(),
            b"a".as_slice(),
            b"a\0b".as_slice(),
            b"123456".as_slice(),
            b"1234567".as_slice(),
            b"12345678".as_slice(),
            b"12345678901234".as_slice(),
        ] {
            let value = Value::small_string(bytes).expect("at most 14 bytes");
            assert_eq!(value.tag(), Tag::SmallString);
            assert_eq!(value.small_string_bytes(), bytes);
            assert!(!value.has_heap_payload());
            assert!(!value.needs_force());
        }
        assert!(Value::small_string(b"123456789012345").is_none());
    }
}
