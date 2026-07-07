//! `extern "C"` runtime helpers called by JIT-compiled chunks. Each helper
//! reproduces one interpreter op body from [`crate::vm::VM::run_top_frame`]
//! *exactly* (same VM methods, same positions — which the compiler bakes in as
//! constants), so a compiled chunk is bit-for-bit equivalent to interpreting
//! it. They live in `jinx-eval` because they need the VM's private op methods;
//! `jinx-jit` registers their addresses with Cranelift.
//!
//! ## Return encoding
//! * status-only helpers: `0` on success, `ERR_FLAG | errid` on error;
//! * "or"-helpers (`SelectOr` / `SelectDynOr`): `0` = fell through (value
//!   pushed), `1` = take the `or`-branch (subject popped), else error;
//! * [`jinx_enter`] returns the address of `vm.stack` and writes the frame's
//!   locals base and upvalue pointer through its out-params.

use crate::chunk::CTX_STRINGS;
use crate::error::ErrId;
use crate::jit::ERR_FLAG;
use crate::value::{Tag, VRef, Value};
use crate::vm::{attrs_get, list_elems, val, VM};
use jinx_syntax::pos::{PosIdx, NO_POS};
use jinx_syntax::symbol::Symbol;

#[inline]
fn err(e: ErrId) -> u64 {
    ERR_FLAG | (e as u64)
}

#[inline]
fn status(r: Result<(), ErrId>) -> u64 {
    match r {
        Ok(()) => 0,
        Err(e) => err(e),
    }
}

macro_rules! vm {
    ($vm:ident) => {
        // SAFETY: JIT passes a live `*mut VM`.
        unsafe { &mut *$vm }
    };
}

/// Frame entry: reserve operand-stack capacity for the whole frame's inline
/// pushes, and return the address of `vm.stack` (a `Stack` with a stable
/// `#[repr(C)]` layout) for the compiled code's inline pushes/pops.
pub extern "C" fn jinx_setup(vm: *mut VM, fi: u64, max_height: u64) -> u64 {
    let vm = vm!(vm);
    let base = vm.frames[fi as usize].locals_base;
    vm.stack.reserve_to(base + max_height as usize);
    (&mut vm.stack) as *mut crate::stack::Stack as u64
}

/// `locals_base` of frame `fi` (constant for the frame's lifetime).
pub extern "C" fn jinx_base(vm: *mut VM, fi: u64) -> u64 {
    let vm = vm!(vm);
    vm.frames[fi as usize].locals_base as u64
}

/// Pointer to the frame's upvalue array (`upvals[0]`), or a dangling pointer if
/// the frame has no upvalues (only queried by chunks that have `GetUpval`).
pub extern "C" fn jinx_upvals(vm: *mut VM, fi: u64) -> u64 {
    let vm = vm!(vm);
    vm.frames[fi as usize].upvals().as_ptr() as u64
}

// ---------------- force family ----------------

pub extern "C" fn jinx_force_top(vm: *mut VM, pos: u32) -> u64 {
    let vm = vm!(vm);
    let c = *vm.stack.last().unwrap();
    status(vm.force(c, PosIdx(pos)))
}

pub extern "C" fn jinx_force_bool_top(vm: *mut VM, pos: u32, ctx: u32) -> u64 {
    let vm = vm!(vm);
    let c = *vm.stack.last().unwrap();
    match vm.force_bool(c, PosIdx(pos), CTX_STRINGS[ctx as usize]) {
        Ok(_) => 0,
        Err(e) => err(e),
    }
}

pub extern "C" fn jinx_force_attrs_top(vm: *mut VM, pos: u32, ctx: u32) -> u64 {
    let vm = vm!(vm);
    let c = *vm.stack.last().unwrap();
    // The `//` operands (ctx 9/10) use evalAttrs semantics (error context on
    // any error); every other site uses forceAttrs (type-mismatch only).
    // Mirrors the interpreter's Op::ForceAttrs handler.
    let cs = CTX_STRINGS[ctx as usize];
    let r = if ctx == 9 || ctx == 10 {
        vm.eval_attrs(c, PosIdx(pos), cs)
    } else {
        vm.force_attrs(c, PosIdx(pos), cs)
    };
    status(r)
}

pub extern "C" fn jinx_force_list_top(vm: *mut VM, pos: u32, ctx: u32) -> u64 {
    let vm = vm!(vm);
    let c = *vm.stack.last().unwrap();
    status(vm.force_list(c, PosIdx(pos), CTX_STRINGS[ctx as usize]))
}

// ---------------- variables / locals ----------------

pub extern "C" fn jinx_resolve_with(vm: *mut VM, fi: u64, sym: u32, pos: u32) -> u64 {
    let vm = vm!(vm);
    match vm.resolve_with(fi as usize, Symbol(sym), PosIdx(pos)) {
        Ok(c) => {
            vm.stack.push(c);
            0
        }
        Err(e) => err(e),
    }
}

pub extern "C" fn jinx_alloc_cell(vm: *mut VM) -> u64 {
    let vm = vm!(vm);
    let c = vm.alloc_cell(Value::make(Tag::Blackhole, 0));
    vm.stack.push(c);
    0
}

/// Allocate a fresh cell holding integer `i` (result of inline int arithmetic),
/// matching the interpreter's `alloc_cell(Value::int(..))`. Returns the cell.
pub extern "C" fn jinx_alloc_int(vm: *mut VM, i: i64) -> u64 {
    let vm = vm!(vm);
    vm.alloc_cell(Value::int(i)).as_ptr() as u64
}

/// Allocate a fresh cell holding boolean `b` (result of inline `<`). Returns
/// the cell.
pub extern "C" fn jinx_alloc_bool(vm: *mut VM, b: u32) -> u64 {
    let vm = vm!(vm);
    vm.alloc_cell(Value::bool(b != 0)).as_ptr() as u64
}

pub extern "C" fn jinx_store_local(vm: *mut VM, fi: u64, slot: u32) -> u64 {
    let vm = vm!(vm);
    let c = vm.stack.pop().unwrap();
    let b = vm.frames[fi as usize].locals_base;
    let dst = vm.stack[b + slot as usize];
    vm.set_b(dst, val(c));
    0
}

// ---------------- allocation ops ----------------

pub extern "C" fn jinx_make_thunk(vm: *mut VM, fi: u64, cid: u32, is_closure: u32) -> u64 {
    let vm = vm!(vm);
    let tag = if is_closure != 0 { Tag::Closure } else { Tag::Thunk };
    let c = vm.make_thunk(fi as usize, cid, tag);
    vm.stack.push(c);
    0
}

pub extern "C" fn jinx_make_list(vm: *mut VM, n: u32) -> u64 {
    let vm = vm!(vm);
    let n = n as usize;
    let start = vm.stack.len() - n;
    vm.gc_check();
    let v = vm.heap.new_list(&vm.stack[start..]);
    let c = vm.heap.alloc_value(v);
    vm.stack.truncate(start);
    vm.stack.push(c);
    0
}

pub extern "C" fn jinx_make_attrs(vm: *mut VM, fi: u64, d: u32) -> u64 {
    let vm = vm!(vm);
    let desc = &vm.frames[fi as usize].code.prog().attrs_descs[d as usize];
    let n = desc.names.len();
    let start = vm.stack.len() - n;
    let entries: Vec<crate::value::Attr> = desc
        .names
        .iter()
        .zip(&vm.stack[start..])
        .map(|(&(sym, pos), &cell)| crate::value::Attr {
            sym: sym.0,
            pos: pos.0,
            val: cell,
        })
        .collect();
    vm.gc_check();
    let v = vm.heap.new_bindings(&entries);
    let c = vm.heap.alloc_value(v);
    vm.stack.truncate(start);
    vm.stack.push(c);
    0
}

pub extern "C" fn jinx_dyn_attr(vm: *mut VM, pos: u32) -> u64 {
    let vm = vm!(vm);
    status(vm.op_dyn_attr(PosIdx(pos)))
}

pub extern "C" fn jinx_rec_overrides(vm: *mut VM, fi: u64, rd: u32, pos: u32) -> u64 {
    let vm = vm!(vm);
    status(vm.op_rec_overrides(fi as usize, rd, PosIdx(pos)))
}

// ---------------- equality / not ----------------

pub extern "C" fn jinx_eq(vm: *mut VM, pos: u32, is_neq: u32) -> u64 {
    let vm = vm!(vm);
    let b = vm.stack.pop().unwrap();
    let a = vm.stack.pop().unwrap();
    let ctx = if is_neq == 0 {
        "while testing two values for equality"
    } else {
        "while testing two values for inequality"
    };
    match vm.eq_values(a, b, PosIdx(pos), ctx, true) {
        Ok(r) => {
            let r = if is_neq == 0 { r } else { !r };
            let cell = vm.bool_cell(r);
            vm.stack.push(cell);
            0
        }
        Err(e) => err(e),
    }
}

pub extern "C" fn jinx_not(vm: *mut VM) -> u64 {
    let vm = vm!(vm);
    let c = vm.stack.pop().unwrap();
    let b = val(c).tag() == Tag::True;
    let cell = vm.bool_cell(!b);
    vm.stack.push(cell);
    0
}

// ---------------- update / concat ----------------

pub extern "C" fn jinx_update(vm: *mut VM) -> u64 {
    let vm = vm!(vm);
    status(vm.op_update())
}

pub extern "C" fn jinx_concat_lists(vm: *mut VM) -> u64 {
    let vm = vm!(vm);
    let b = vm.stack.pop().unwrap();
    let a = vm.stack.pop().unwrap();
    let (va, vb) = (val(a), val(b));
    let (ea, eb) = (list_elems(&va), list_elems(&vb));
    let v = if ea.is_empty() && !eb.is_empty() {
        vb
    } else if eb.is_empty() {
        va
    } else {
        let mut items: Vec<VRef> = Vec::with_capacity(ea.len() + eb.len());
        items.extend_from_slice(ea);
        items.extend_from_slice(eb);
        vm.new_list_value(&items)
    };
    let c = vm.alloc_cell(v);
    vm.stack.push(c);
    0
}

pub extern "C" fn jinx_concat_strings(vm: *mut VM, fi: u64, d: u32) -> u64 {
    let vm = vm!(vm);
    status(vm.op_concat_strings(fi as usize, d))
}

// ---------------- selection ----------------

pub extern "C" fn jinx_select(vm: *mut VM, fi: u64, sym: u32, cache: u32, pos: u32) -> u64 {
    let vm = vm!(vm);
    let prog = vm.frames[fi as usize].code.prog();
    status(vm.op_select(Symbol(sym), cache, prog, PosIdx(pos)))
}

pub extern "C" fn jinx_select_force(vm: *mut VM, fi: u64, text: u32) -> u64 {
    let vm = vm!(vm);
    let fi = fi as usize;
    let c = *vm.stack.last().unwrap();
    let p = vm.last_select_pos;
    match vm.force(c, p) {
        Ok(()) => 0,
        Err(e) => {
            if p.is_set() && !vm.pos_is_derivation_internal(p) {
                let text = vm.frames[fi].code.prog().texts[text as usize].clone();
                vm.add_trace(
                    e,
                    p,
                    format!(
                        "while evaluating the attribute '{}'",
                        String::from_utf8_lossy(&text)
                    ),
                );
            }
            err(e)
        }
    }
}

/// Returns 0 = found (fell through), 1 = not found (subject popped, take jump).
pub extern "C" fn jinx_select_or(vm: *mut VM, sym: u32, pos: u32) -> u64 {
    let vm = vm!(vm);
    let c = *vm.stack.last().unwrap();
    if let Err(e) = vm.force(c, PosIdx(pos)) {
        return err(e);
    }
    let v = val(c);
    let found = if v.tag() == Tag::Attrs {
        attrs_get(&v, Symbol(sym))
    } else {
        None
    };
    match found {
        Some(a) => {
            vm.last_select_pos = PosIdx(a.pos);
            *vm.stack.last_mut().unwrap() = a.val;
            0
        }
        None => {
            vm.stack.pop();
            1
        }
    }
}

pub extern "C" fn jinx_select_dyn(vm: *mut VM, fi: u64, text: u32, pos: u32) -> u64 {
    let vm = vm!(vm);
    let fi = fi as usize;
    let sp = vm.last_select_pos;
    let dyn_pos = PosIdx(pos);
    let name = vm.stack.pop().unwrap();
    let step = |vm: &mut VM| -> Result<(), ErrId> {
        let nb = vm.force_string_no_ctx(name, dyn_pos, "while evaluating an attribute name")?;
        let sym = vm.symbols.create(&nb);
        let c = *vm.stack.last().unwrap();
        vm.force_attrs(c, dyn_pos, "while selecting an attribute")?;
        let v = val(c);
        match attrs_get(&v, sym) {
            Some(a) => {
                vm.last_select_pos = PosIdx(a.pos);
                *vm.stack.last_mut().unwrap() = a.val;
                Ok(())
            }
            None => Err(vm.missing_attr_err(&v, sym, dyn_pos)),
        }
    };
    if let Err(e) = step(vm) {
        if text != u32::MAX && sp.is_set() && !vm.pos_is_derivation_internal(sp) {
            let t = vm.frames[fi].code.prog().texts[text as usize].clone();
            vm.add_trace(
                e,
                sp,
                format!(
                    "while evaluating the attribute '{}'",
                    String::from_utf8_lossy(&t)
                ),
            );
        }
        return err(e);
    }
    0
}

/// Returns 0 = found (fell through), 1 = not found (subject popped, take jump).
pub extern "C" fn jinx_select_dyn_or(vm: *mut VM, pos: u32) -> u64 {
    let vm = vm!(vm);
    let dyn_pos = PosIdx(pos);
    let name = vm.stack.pop().unwrap();
    let nb = match vm.force_string_no_ctx(name, dyn_pos, "while evaluating an attribute name") {
        Ok(nb) => nb,
        Err(e) => return err(e),
    };
    let sym = vm.symbols.create(&nb);
    let c = *vm.stack.last().unwrap();
    if let Err(e) = vm.force(c, dyn_pos) {
        return err(e);
    }
    let v = val(c);
    let found = if v.tag() == Tag::Attrs {
        attrs_get(&v, sym)
    } else {
        None
    };
    match found {
        Some(a) => {
            vm.last_select_pos = PosIdx(a.pos);
            *vm.stack.last_mut().unwrap() = a.val;
            0
        }
        None => {
            vm.stack.pop();
            1
        }
    }
}

pub extern "C" fn jinx_has_attr_path(vm: *mut VM, fi: u64, d: u32, pos: u32) -> u64 {
    let vm = vm!(vm);
    status(vm.op_has_attr_path(fi as usize, d, PosIdx(pos)))
}

// ---------------- call ----------------

pub extern "C" fn jinx_call(vm: *mut VM, n: u32, pos: u32) -> u64 {
    let vm = vm!(vm);
    let n = n as usize;
    let args_start = vm.stack.len() - n;
    let fun = vm.stack[args_start - 1];
    // Mirror the interpreter's Op::Call: copy small arg lists into an inline
    // buffer instead of a heap Vec (args stay rooted via the operand stack).
    let mut buf = [std::mem::MaybeUninit::<VRef>::uninit(); 8];
    let mut heap_args: Vec<VRef> = Vec::new();
    let args: &[VRef] = if n <= 8 {
        // SAFETY: we copy exactly n initialized cells.
        unsafe {
            std::ptr::copy_nonoverlapping(
                vm.stack[args_start..].as_ptr(),
                buf.as_mut_ptr() as *mut VRef,
                n,
            );
            std::slice::from_raw_parts(buf.as_ptr() as *const VRef, n)
        }
    } else {
        heap_args.extend_from_slice(&vm.stack[args_start..]);
        &heap_args
    };
    let mut cpos = PosIdx(pos);
    if !cpos.is_set() {
        cpos = vm.force_pos;
        if !cpos.is_set() {
            cpos = vm.determine_pos(&val(fun), NO_POS);
        }
    }
    match vm.call_function(fun, args, cpos) {
        Ok(v) => {
            vm.stack.truncate(args_start - 1);
            let c = vm.alloc_cell(v);
            vm.stack.push(c);
            0
        }
        Err(e) => err(e),
    }
}

// ---------------- misc ----------------

pub extern "C" fn jinx_cur_pos(vm: *mut VM, pos: u32) -> u64 {
    let vm = vm!(vm);
    let v = vm.mk_pos(PosIdx(pos));
    let c = vm.alloc_cell(v);
    vm.stack.push(c);
    0
}

pub extern "C" fn jinx_assert_fail(vm: *mut VM, fi: u64, text: u32, pos: u32) -> u64 {
    let vm = vm!(vm);
    let t = &vm.frames[fi as usize].code.prog().texts[text as usize];
    let mut m = b"assertion '".to_vec();
    m.extend_from_slice(t);
    m.extend_from_slice(b"' failed");
    err(vm.new_err(crate::error::ErrKind::Assertion, m, PosIdx(pos)))
}

/// Returns 0 if the values compared equal (fall through to the following
/// `AssertFail`), else the detailed inequality error.
pub extern "C" fn jinx_assert_eq(vm: *mut VM, fi: u64, text: u32, pos: u32) -> u64 {
    let vm = vm!(vm);
    let fi = fi as usize;
    let rhs = vm.stack.pop().unwrap();
    let lhs = vm.stack.pop().unwrap();
    let apos = PosIdx(pos);
    if let Err(e) = vm.assert_eq_values(lhs, rhs, NO_POS, "in an equality assertion") {
        let t = vm.frames[fi].code.prog().texts[text as usize].clone();
        vm.add_trace(
            e,
            apos,
            format!(
                "while evaluating the condition of the assertion '{}'",
                String::from_utf8_lossy(&t)
            ),
        );
        return err(e);
    }
    0
}

pub extern "C" fn jinx_push_with(vm: *mut VM, fi: u64) -> u64 {
    let vm = vm!(vm);
    let c = vm.stack.pop().unwrap();
    vm.frames[fi as usize].with_local.push(c);
    0
}

pub extern "C" fn jinx_pop_with(vm: *mut VM, fi: u64) -> u64 {
    let vm = vm!(vm);
    vm.frames[fi as usize].with_local.pop();
    0
}
