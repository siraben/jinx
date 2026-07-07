//! `extern "C"` runtime helpers that compiled chunks call back into. Each
//! helper replicates one interpreter op *exactly* (same VM methods, same
//! positions baked as constants by the compiler), so a compiled chunk is
//! bit-for-bit equivalent to interpreting it.
//!
//! Return conventions (see `jinx_eval::jit`):
//!   * status-only helpers return 0 on success, `ERR_FLAG | errid` on error;
//!   * value-producing helpers return a `VRef` (non-null) or `ERR_FLAG|errid`;
//!   * predicate helpers document their success encoding inline.

use jinx_eval::jit::ERR_FLAG;

// The helpers are added incrementally as ops are lowered (step 2+). This
// module currently exposes the registration table used by the compiler.

/// One registered runtime symbol: (name, address).
pub type Sym = (&'static str, *const u8);

/// All runtime helper symbols to register with the `JITBuilder`.
pub fn symbols() -> Vec<Sym> {
    Vec::new()
}

#[allow(dead_code)]
const _: u64 = ERR_FLAG; // keep the import live until helpers land
