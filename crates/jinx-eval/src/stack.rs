//! The VM operand stack, as a `#[repr(C)]` growable buffer with a *stable,
//! documented* field layout so JIT-compiled code can inline pushes/pops by
//! reading `ptr`/`len`/`cap` at fixed offsets (unlike `Vec`, whose layout is
//! unspecified). Behaves like `Vec<VRef>` for the interpreter via `Deref`.
//!
//! Layout (offsets baked into the JIT):
//!   +0  ptr: *mut VRef   (STACK_PTR_OFF)
//!   +8  len: usize       (STACK_LEN_OFF)
//!   +16 cap: usize       (STACK_CAP_OFF)

use std::alloc::{self, Layout};
use std::ops::{Deref, DerefMut};

use crate::value::VRef;

pub const STACK_PTR_OFF: i32 = 0;
pub const STACK_LEN_OFF: i32 = 8;
pub const STACK_CAP_OFF: i32 = 16;

#[repr(C)]
pub struct Stack {
    ptr: *mut VRef,
    len: usize,
    cap: usize,
}

impl Stack {
    pub fn with_capacity(cap: usize) -> Self {
        let mut s = Stack {
            ptr: std::ptr::NonNull::<VRef>::dangling().as_ptr(),
            len: 0,
            cap: 0,
        };
        if cap > 0 {
            s.grow_to(cap);
        }
        s
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn push(&mut self, v: VRef) {
        if self.len == self.cap {
            self.grow_one();
        }
        // SAFETY: len < cap after the grow check.
        unsafe {
            self.ptr.add(self.len).write(v);
        }
        self.len += 1;
    }

    #[inline]
    pub fn pop(&mut self) -> Option<VRef> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: len was > 0; element is initialized.
        Some(unsafe { self.ptr.add(self.len).read() })
    }

    #[inline]
    pub fn truncate(&mut self, new_len: usize) {
        if new_len < self.len {
            self.len = new_len;
        }
    }

    /// Ensure room for at least `additional` more elements without
    /// reallocating (used by the JIT before a run of inline pushes).
    pub fn reserve(&mut self, additional: usize) {
        let need = self.len + additional;
        if need > self.cap {
            let new_cap = need.max(self.cap * 2).max(8);
            self.grow_to(new_cap);
        }
    }

    #[cold]
    #[inline(never)]
    fn grow_one(&mut self) {
        let new_cap = (self.cap * 2).max(8);
        self.grow_to(new_cap);
    }

    fn grow_to(&mut self, new_cap: usize) {
        debug_assert!(new_cap > self.cap);
        let new_layout = Layout::array::<VRef>(new_cap).expect("stack layout");
        let new_ptr = if self.cap == 0 {
            // SAFETY: non-zero size (VRef is 8 bytes, new_cap > 0).
            unsafe { alloc::alloc(new_layout) as *mut VRef }
        } else {
            let old_layout = Layout::array::<VRef>(self.cap).unwrap();
            // SAFETY: ptr was allocated with old_layout.
            unsafe {
                alloc::realloc(self.ptr as *mut u8, old_layout, new_layout.size()) as *mut VRef
            }
        };
        if new_ptr.is_null() {
            alloc::handle_alloc_error(new_layout);
        }
        self.ptr = new_ptr;
        self.cap = new_cap;
    }
}

impl Deref for Stack {
    type Target = [VRef];
    #[inline]
    fn deref(&self) -> &[VRef] {
        // SAFETY: first `len` elements are initialized.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for Stack {
    #[inline]
    fn deref_mut(&mut self) -> &mut [VRef] {
        // SAFETY: first `len` elements are initialized.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        if self.cap != 0 {
            let layout = Layout::array::<VRef>(self.cap).unwrap();
            // SAFETY: ptr came from alloc with this layout.
            unsafe { alloc::dealloc(self.ptr as *mut u8, layout) }
        }
    }
}

// The operand stack is only ever touched on the single evaluation thread.
unsafe impl Send for Stack {}
