//! Immortal (leaked) value cells and data objects for compile-time
//! constants and globals. The GC treats out-of-heap pointers as immortal:
//! `Marker::mark_cell`/`mark_data` ignore addresses it cannot locate, so
//! leaked cells and payloads need no rooting. Their *outgoing* references
//! are never traced either, so immortal objects must only reference other
//! immortal objects (a constant string/path/int never references anything).

use std::ptr::NonNull;
use std::sync::OnceLock;

use crate::value::{self, ObjKind, Tag, VRef, Value};

/// Leak a value cell.
pub fn cell(v: Value) -> VRef {
    NonNull::from(Box::leak(Box::new(v)))
}

/// Canonical cell for a frequently produced small integer. Integer identity
/// is not observable in Nix, so arithmetic/call results in this range can
/// share immutable cells just like booleans and compile-time constants do.
///
/// Keep the range deliberately small: it covers loop indices, lengths, and
/// recursive-call arguments without retaining arbitrary arithmetic values.
pub fn small_int_cell(i: i64) -> Option<VRef> {
    const MIN: i64 = -128;
    const MAX: i64 = 255;
    static CELLS: OnceLock<Box<[Value]>> = OnceLock::new();

    if !(MIN..=MAX).contains(&i) {
        return None;
    }
    let cells = CELLS.get_or_init(|| {
        (MIN..=MAX)
            .map(Value::int)
            .collect::<Vec<_>>()
            .into_boxed_slice()
    });
    Some(NonNull::from(&cells[(i - MIN) as usize]))
}

/// Leak a data object with the given header, payload words zeroed.
fn obj(kind: ObjKind, len: usize) -> *mut u64 {
    let bytes = value::obj_size_bytes(value::header(kind, len));
    let words = bytes.div_ceil(8);
    let mut v: Vec<u64> = vec![0; words];
    v[0] = value::header(kind, len);
    let b: &'static mut [u64] = Box::leak(v.into_boxed_slice());
    b.as_mut_ptr()
}

/// Immortal string value (no context).
pub fn string(bytes: &[u8]) -> Value {
    if let Some(value) = Value::small_string(bytes) {
        return value;
    }
    let p = obj(ObjKind::Str, bytes.len());
    // SAFETY: object sized for the payload.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(2) as *mut u8, bytes.len());
    }
    Value::make(Tag::String, p as u64)
}

/// Immortal path value.
pub fn path(bytes: &[u8]) -> Value {
    let p = obj(ObjKind::Path, bytes.len());
    // SAFETY: object sized for the payload.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(2) as *mut u8, bytes.len());
    }
    Value::make(Tag::Path, p as u64)
}

/// Immortal list of immortal cells.
pub fn list(items: &[VRef]) -> Value {
    let p = obj(ObjKind::List, items.len());
    // SAFETY: object sized for the elements.
    unsafe {
        std::ptr::copy_nonoverlapping(items.as_ptr(), p.add(1) as *mut VRef, items.len());
    }
    Value::make(Tag::List, p as u64)
}

/// Immortal bindings (entries must be sorted by symbol id and reference
/// immortal cells).
pub fn bindings(entries: &[value::Attr]) -> Value {
    debug_assert!(entries.windows(2).all(|w| w[0].sym < w[1].sym));
    let p = obj(ObjKind::Bindings, entries.len());
    // SAFETY: object sized for the entries.
    unsafe {
        std::ptr::copy_nonoverlapping(entries.as_ptr(), p.add(1) as *mut value::Attr, entries.len());
    }
    Value::make(Tag::Attrs, p as u64)
}
