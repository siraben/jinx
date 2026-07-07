//! JIT interface seam. The Cranelift backend lives in the `jinx-jit` crate
//! (which depends on `jinx-eval`); this module defines the trait the VM calls
//! into and the ABI of compiled chunk entry points, plus the `extern "C"`
//! runtime helpers that compiled code calls back into.
//!
//! # Compiled-chunk ABI
//! A compiled chunk is an `extern "C" fn(vm: *mut VM, fi: u64) -> u64` that
//! evaluates frame `fi` exactly like [`VM::run_top_frame`] and returns:
//!   * a `VRef` (cell pointer, always non-null, bit 63 clear on the supported
//!     targets) on success, or
//!   * `ERR_FLAG | (errid as u64)` on error.
//! It operates on the *same* `vm.stack` / `vm.frames[fi]` as the interpreter,
//! so falling back to the interpreter for an uncompilable chunk is trivial.

use crate::chunk::CodeRef;

/// Signature of a compiled chunk entry point:
/// `(vm, fi, &mut vm.stack, locals_base, upvals_ptr) -> status`.
/// The caller (`VM::run_jit`) pre-reserves `locals_base + max_height`
/// operand-stack capacity and passes the frame constants directly, so the
/// compiled prologue makes no helper calls.
pub type JitEntry =
    extern "C" fn(*mut crate::vm::VM, u64, *mut crate::stack::Stack, u64, u64) -> u64;

/// High bit of a helper/entry return word: set means "error", low bits carry
/// the `ErrId`. `VRef` pointers on aarch64/x86-64 userspace never set it.
pub const ERR_FLAG: u64 = 1u64 << 63;

/// The Cranelift backend, installed into the VM when JIT is enabled.
pub trait JitHook {
    /// Compile the chunk referenced by `code` to a native entry point, or
    /// return `None` if it contains an op the backend does not lower (the
    /// chunk is then marked uncompilable and always interpreted). `code`
    /// gives access to both the `Chunk` and its owning `Program` (constants,
    /// descriptor tables).
    fn compile(&mut self, code: &'static CodeRef) -> Option<*const ()>;
}
