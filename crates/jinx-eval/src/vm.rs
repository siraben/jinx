//! The stack VM: frames over compiled chunks, in-place thunk forcing with
//! blackholing and error memoization, a runtime with-chain, and the C++
//! call/equality/coercion semantics ported from eval.cc.
//!
//! # GC discipline
//! `Heap` allocation never collects. Collections run only from
//! `VM::gc_check()`, which is called at op dispatch and at the head of the
//! `alloc_*`/`new_*` wrappers. Precise roots: the operand stack, frames
//! (thunk payloads + with-chains), `temp_roots`, and the import cache;
//! immortal globals/constants need no rooting. Native `VRef`/`Value`
//! locals in builtins are covered by the conservative stack scan, but
//! `Vec<VRef>` *contents* live on the Rust heap and are NOT scanned:
//! builtins accumulating cells in vectors must root them via
//! `TempRoots` (see `VM::temp_scope`).

use rustc_hash::FxHashMap;
use std::ptr::NonNull;

use jinx_syntax::pos::{PosIdx, PosTable};
use jinx_syntax::symbol::{Symbol, SymbolTable};

use crate::chunk::{Chunk, CodeRef, Op, CTX_STRINGS};
use crate::compile::SpecialSyms;
use crate::error::{best_matches, ErrId, ErrKind, EvalError, Trace};
use crate::heap::Heap;
use crate::immortal;
use crate::value::{self, Attr, Tag, VRef, Value};

#[inline]
pub fn val(c: VRef) -> Value {
    // SAFETY: cells handed to the VM are live.
    unsafe { *c.as_ptr() }
}

#[inline]
pub fn set(c: VRef, v: Value) {
    // SAFETY: cells handed to the VM are live.
    unsafe { *c.as_ptr() = v }
}

/// Typed views (thin unsafe wrappers; heap objects are non-moving).
#[inline]
pub fn str_bytes<'a>(v: &Value) -> &'a [u8] {
    debug_assert_eq!(v.tag(), Tag::String);
    // SAFETY: tag invariant.
    unsafe { value::str_parts(v.ptr() as *const u64).0 }
}

#[inline]
pub fn str_ctx(v: &Value) -> *mut u64 {
    // SAFETY: tag invariant.
    unsafe { value::str_parts(v.ptr() as *const u64).1 }
}

#[inline]
pub fn path_bytes<'a>(v: &Value) -> &'a [u8] {
    debug_assert_eq!(v.tag(), Tag::Path);
    // SAFETY: tag invariant.
    unsafe { value::path_parts(v.ptr() as *const u64).1 }
}

#[inline]
pub fn list_elems<'a>(v: &Value) -> &'a [VRef] {
    debug_assert_eq!(v.tag(), Tag::List);
    // SAFETY: tag invariant.
    unsafe { value::elems(v.ptr() as *const u64) }
}

#[inline]
pub fn attrs_entries<'a>(v: &Value) -> &'a [Attr] {
    debug_assert_eq!(v.tag(), Tag::Attrs);
    // SAFETY: tag invariant.
    unsafe { value::bindings(v.ptr() as *const u64) }
}

pub fn attrs_get(v: &Value, sym: Symbol) -> Option<Attr> {
    let es = attrs_entries(v);
    es.binary_search_by(|a| a.sym.cmp(&sym.0))
        .ok()
        .map(|i| es[i])
}

pub struct PrimOpDef {
    /// Registration name (may have "__" prefix).
    pub name: &'static str,
    pub arity: u8,
    pub func: fn(&mut VM, &'static PrimOpDef, &[VRef], PosIdx) -> Result<Value, ErrId>,
}

impl PrimOpDef {
    pub fn display(&self) -> &'static str {
        self.name.strip_prefix("__").unwrap_or(self.name)
    }
}

pub fn primop_of(v: &Value) -> &'static PrimOpDef {
    debug_assert_eq!(v.tag(), Tag::PrimOp);
    // SAFETY: PrimOp cells always carry a &'static PrimOpDef.
    unsafe { &*(v.w1 as *const PrimOpDef) }
}

/// (primop, applied arg cells) of a PrimOpApp value.
pub fn primapp_parts<'a>(v: &Value) -> (&'static PrimOpDef, &'a [VRef]) {
    debug_assert_eq!(v.tag(), Tag::PrimOpApp);
    // SAFETY: tag invariant.
    unsafe {
        let (code, elems) = value::code_and_elems(v.ptr() as *const u64);
        (&*(code as *const PrimOpDef), elems)
    }
}

pub fn thunk_code(v: &Value) -> (&'static CodeRef, &'static [VRef]) {
    debug_assert!(matches!(v.tag(), Tag::Thunk | Tag::Closure));
    // SAFETY: thunk/closure data objects carry a &'static CodeRef.
    unsafe {
        let (code, elems) = value::code_and_elems(v.ptr() as *const u64);
        (
            &*(code as *const CodeRef),
            std::slice::from_raw_parts(elems.as_ptr(), elems.len()),
        )
    }
}

pub struct Frame {
    pub code: &'static CodeRef,
    /// The thunk/closure value being run (roots the upvalue array).
    pub data: Value,
    pub locals_base: usize,
    pub with_local: Vec<VRef>,
}

impl Frame {
    fn upvals(&self) -> &'static [VRef] {
        if matches!(self.data.tag(), Tag::Thunk | Tag::Closure) {
            thunk_code(&self.data).1
        } else {
            &[]
        }
    }
}

pub struct VM {
    pub heap: Heap,
    pub stack: Vec<VRef>,
    pub frames: Vec<Frame>,
    pub temp_roots: Vec<VRef>,
    /// Permanent extra roots (mutable global cells like `derivation`,
    /// scopedImport scope cells referenced from leaked program constants).
    pub perm_roots: Vec<VRef>,
    pub symbols: SymbolTable,
    pub positions: PosTable,
    pub syms: SpecialSyms,
    pub errors: Vec<EvalError>,
    pub globals: FxHashMap<Symbol, VRef>,
    pub file_cache: Vec<(std::path::PathBuf, VRef)>,
    pub call_depth: usize,
    pub max_call_depth: usize,
    /// (prefix, path) entries, from -I and NIX_PATH.
    pub search_path: Vec<(Vec<u8>, Vec<u8>)>,
    pub true_cell: VRef,
    pub false_cell: VRef,
    pub null_cell: VRef,
    pub empty_list_cell: VRef,
    pub current_system: Vec<u8>,
    pub store_dir: Vec<u8>,
    pub pure_eval: bool,
    /// String context element table (M3 plumbing; unused producers in M2).
    pub ctx_elems: Vec<Vec<u8>>,
    /// Synthetic apply chunks for lazy applications (set at registration).
    pub apply_prog: Option<&'static crate::chunk::Program>,
}

/// RAII guard for `temp_roots`.
pub struct TempScope(usize);

impl VM {
    pub fn new(mut symbols: SymbolTable, positions: PosTable) -> Self {
        let syms = SpecialSyms::new(&mut symbols);
        VM {
            heap: Heap::new(),
            stack: Vec::with_capacity(1024),
            frames: Vec::with_capacity(64),
            temp_roots: Vec::new(),
            perm_roots: Vec::new(),
            symbols,
            positions,
            syms,
            errors: Vec::new(),
            globals: FxHashMap::default(),
            file_cache: Vec::new(),
            call_depth: 0,
            max_call_depth: 10000,
            search_path: Vec::new(),
            true_cell: immortal::cell(Value::bool(true)),
            false_cell: immortal::cell(Value::bool(false)),
            null_cell: immortal::cell(Value::null()),
            empty_list_cell: immortal::cell(immortal::list(&[])),
            current_system: b"aarch64-darwin".to_vec(),
            store_dir: b"/nix/store".to_vec(),
            pure_eval: false,
            ctx_elems: Vec::new(),
            apply_prog: None,
        }
    }

    // ---------------- GC ----------------

    #[inline]
    pub fn gc_check(&mut self) {
        if self.heap.should_gc() {
            self.gc();
        }
    }

    fn gc(&mut self) {
        let VM {
            heap,
            stack,
            frames,
            temp_roots,
            perm_roots,
            file_cache,
            ..
        } = self;
        heap.collect(
            |m| {
                for &c in stack.iter() {
                    m.mark_cell(c);
                }
                for f in frames.iter() {
                    m.mark_value(&f.data);
                    for &c in &f.with_local {
                        m.mark_cell(c);
                    }
                }
                for &c in temp_roots.iter() {
                    m.mark_cell(c);
                }
                for &c in perm_roots.iter() {
                    m.mark_cell(c);
                }
                for (_, c) in file_cache.iter() {
                    m.mark_cell(*c);
                }
            },
            true,
        );
    }

    pub fn temp_scope(&mut self) -> TempScope {
        TempScope(self.temp_roots.len())
    }

    pub fn temp_end(&mut self, s: TempScope) {
        self.temp_roots.truncate(s.0);
    }

    // ---------------- allocation wrappers ----------------

    pub fn alloc_cell(&mut self, v: Value) -> VRef {
        self.gc_check();
        self.heap.alloc_value(v)
    }

    pub fn new_string_value(&mut self, bytes: &[u8], ctx: *mut u64) -> Value {
        self.gc_check();
        self.heap.new_string(bytes, ctx)
    }

    pub fn new_path_value(&mut self, bytes: &[u8]) -> Value {
        self.gc_check();
        self.heap.new_path(0, bytes)
    }

    pub fn new_list_value(&mut self, items: &[VRef]) -> Value {
        self.gc_check();
        self.heap.new_list(items)
    }

    pub fn new_bindings_value(&mut self, entries: &[Attr]) -> Value {
        self.gc_check();
        self.heap.new_bindings(entries)
    }

    pub fn bool_cell(&self, b: bool) -> VRef {
        if b {
            self.true_cell
        } else {
            self.false_cell
        }
    }

    // ---------------- errors ----------------

    pub fn new_err(&mut self, kind: ErrKind, msg: impl Into<Vec<u8>>, pos: PosIdx) -> ErrId {
        self.errors.push(EvalError::new(kind, msg, pos));
        (self.errors.len() - 1) as ErrId
    }

    pub fn add_trace(&mut self, e: ErrId, pos: PosIdx, text: impl Into<String>) {
        self.errors[e as usize].traces.push(Trace {
            pos,
            text: text.into(),
        });
    }

    pub fn err_kind(&self, e: ErrId) -> ErrKind {
        self.errors[e as usize].kind
    }

    /// "an integer" / "a set" / ... (showType with article).
    pub fn show_type(&self, v: &Value) -> String {
        match v.tag() {
            Tag::Null => "null".into(),
            Tag::False | Tag::True => "a Boolean".into(),
            Tag::Int => "an integer".into(),
            Tag::Float => "a float".into(),
            Tag::String => {
                if str_ctx(v).is_null() {
                    "a string".into()
                } else {
                    "a string with context".into()
                }
            }
            Tag::Path => "a path".into(),
            Tag::Attrs => "a set".into(),
            Tag::List => "a list".into(),
            Tag::Closure => "a function".into(),
            Tag::PrimOp => format!("the built-in function '{}'", primop_of(v).display()),
            Tag::PrimOpApp => format!(
                "the partially applied built-in function '{}'",
                primapp_parts(v).0.display()
            ),
            Tag::Thunk | Tag::Blackhole => "a thunk".into(),
            Tag::Failed => "an error".into(),
        }
    }

    fn type_err(
        &mut self,
        v: &Value,
        expected: &str,
        pos: PosIdx,
        ctx: Option<&str>,
    ) -> ErrId {
        let printed = crate::print::print_value_err(self, v);
        let msg = format!(
            "expected {} but found {}: {}",
            expected,
            self.show_type(v),
            printed
        );
        let e = self.new_err(ErrKind::Type, msg, pos);
        if let Some(c) = ctx {
            self.add_trace(e, pos, c);
        }
        e
    }

    // ---------------- force ----------------

    pub fn force(&mut self, cell: VRef, pos: PosIdx) -> Result<(), ErrId> {
        loop {
            let v = val(cell);
            match v.tag() {
                Tag::Thunk => {
                    set(cell, Value::make(Tag::Blackhole, 0));
                    let (code, _) = thunk_code(&v);
                    match self.run_code(code, v) {
                        Ok(res) => {
                            set(cell, res);
                            // The chunk may itself return an (unforced)
                            // thunk value (e.g. a call result); keep going
                            // until WHNF, like C++ forceValue on tApp.
                            if res.tag() == Tag::Thunk {
                                continue;
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            set(cell, Value::make(Tag::Failed, e as u64));
                            return Err(e);
                        }
                    }
                }
                Tag::Blackhole => {
                    return Err(self.new_err(
                        ErrKind::InfiniteRecursion,
                        "infinite recursion encountered",
                        pos,
                    ))
                }
                Tag::Failed => return Err(v.w1 as ErrId),
                _ => return Ok(()),
            }
        }
    }

    pub fn force_bool(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<bool, ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        match v.tag() {
            Tag::True => Ok(true),
            Tag::False => Ok(false),
            _ => Err(self.type_err(&v, "a Boolean", pos, Some(ctx))),
        }
    }

    pub fn force_attrs(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<(), ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        if v.tag() != Tag::Attrs {
            return Err(self.type_err(&v, "a set", pos, Some(ctx)));
        }
        Ok(())
    }

    pub fn force_list(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<(), ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        if v.tag() != Tag::List {
            return Err(self.type_err(&v, "a list", pos, Some(ctx)));
        }
        Ok(())
    }

    pub fn force_int(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<i64, ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        if v.tag() != Tag::Int {
            let e = self.type_err(&v, "an integer", pos, None);
            self.add_trace(e, pos, ctx);
            return Err(e);
        }
        Ok(v.as_int())
    }

    pub fn force_float(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<f64, ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        match v.tag() {
            Tag::Int => Ok(v.as_int() as f64),
            Tag::Float => Ok(v.as_float()),
            _ => {
                let e = self.type_err(&v, "a float", pos, None);
                self.add_trace(e, pos, ctx);
                Err(e)
            }
        }
    }

    /// forceString: returns owned bytes (heap strings are stable, but
    /// copying keeps borrows simple).
    pub fn force_string(
        &mut self,
        cell: VRef,
        pos: PosIdx,
        ctx: &str,
    ) -> Result<Vec<u8>, ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        if v.tag() != Tag::String {
            let e = self.type_err(&v, "a string", pos, None);
            self.add_trace(e, pos, ctx);
            return Err(e);
        }
        Ok(str_bytes(&v).to_vec())
    }

    pub fn force_string_no_ctx(
        &mut self,
        cell: VRef,
        pos: PosIdx,
        ctx: &str,
    ) -> Result<Vec<u8>, ErrId> {
        let s = self.force_string(cell, pos, ctx)?;
        let v = val(cell);
        if !str_ctx(&v).is_null() {
            let msg = format!(
                "the string '{}' is not allowed to refer to a store path",
                String::from_utf8_lossy(&s)
            );
            let e = self.new_err(ErrKind::Eval, msg, pos);
            self.add_trace(e, pos, ctx);
            return Err(e);
        }
        Ok(s)
    }

    // ---------------- running code ----------------

    /// Push a frame for `code` (with upvalue payload `data`), run it, pop.
    pub fn run_code(&mut self, code: &'static CodeRef, data: Value) -> Result<Value, ErrId> {
        let base = self.stack.len();
        self.frames.push(Frame {
            code,
            data,
            locals_base: base,
            with_local: Vec::new(),
        });
        let r = self.run_top_frame();
        self.frames.pop();
        let out = r.map(val);
        self.stack.truncate(base);
        out
    }

    /// Entry point: evaluate chunk 0 of a leaked program.
    pub fn run_program(&mut self, prog: &'static crate::chunk::Program) -> Result<VRef, ErrId> {
        let code = prog.code_ref(0);
        let v = self.run_code(code, Value::null())?;
        Ok(self.alloc_cell(v))
    }

    fn run_top_frame(&mut self) -> Result<VRef, ErrId> {
        let fi = self.frames.len() - 1;
        let chunk: &'static Chunk = self.frames[fi].code.chunk();
        let mut ip = 0usize;
        macro_rules! pos {
            () => {
                chunk.pos_at(ip)
            };
        }
        loop {
            self.gc_check();
            let op = chunk.ops[ip];
            match op {
                Op::Const(i) => {
                    let c = self.frames[fi].code.prog().consts[i as usize];
                    self.stack.push(c);
                }
                Op::GetLocal(s) => {
                    let b = self.frames[fi].locals_base;
                    let c = self.stack[b + s as usize];
                    self.stack.push(c);
                }
                Op::GetUpval(i) => {
                    let c = self.frames[fi].upvals()[i as usize];
                    self.stack.push(c);
                }
                Op::ResolveWith(sym) => {
                    let c = self.resolve_with(fi, Symbol(sym), pos!())?;
                    self.stack.push(c);
                }
                Op::Force => {
                    let c = *self.stack.last().unwrap();
                    self.force(c, pos!())?;
                }
                Op::ForceBool(ctx) => {
                    let c = *self.stack.last().unwrap();
                    self.force_bool(c, pos!(), CTX_STRINGS[ctx as usize])?;
                }
                Op::ForceAttrs(ctx) => {
                    let c = *self.stack.last().unwrap();
                    self.force_attrs(c, pos!(), CTX_STRINGS[ctx as usize])?;
                }
                Op::ForceList(ctx) => {
                    let c = *self.stack.last().unwrap();
                    self.force_list(c, pos!(), CTX_STRINGS[ctx as usize])?;
                }
                Op::Pop => {
                    self.stack.pop();
                }
                Op::AllocCell => {
                    let c = self.alloc_cell(Value::make(Tag::Blackhole, 0));
                    self.stack.push(c);
                }
                Op::StoreLocal(s) => {
                    let c = self.stack.pop().unwrap();
                    let b = self.frames[fi].locals_base;
                    let dst = self.stack[b + s as usize];
                    set(dst, val(c));
                }
                Op::MakeThunk(cid) => {
                    let c = self.make_thunk(fi, cid, Tag::Thunk);
                    self.stack.push(c);
                }
                Op::MakeClosure(cid) => {
                    let c = self.make_thunk(fi, cid, Tag::Closure);
                    self.stack.push(c);
                }
                Op::MakeList(n) => {
                    let n = n as usize;
                    let start = self.stack.len() - n;
                    self.gc_check();
                    let v = self.heap.new_list(&self.stack[start..]);
                    let c = self.heap.alloc_value(v);
                    self.stack.truncate(start);
                    self.stack.push(c);
                }
                Op::MakeAttrs(d) => {
                    let desc = &self.frames[fi].code.prog().attrs_descs[d as usize];
                    let n = desc.names.len();
                    let start = self.stack.len() - n;
                    let entries: Vec<Attr> = desc
                        .names
                        .iter()
                        .zip(&self.stack[start..])
                        .map(|(&(sym, pos), &cell)| Attr {
                            sym: sym.0,
                            pos: pos.0,
                            val: cell,
                        })
                        .collect();
                    self.gc_check();
                    let v = self.heap.new_bindings(&entries);
                    let c = self.heap.alloc_value(v);
                    self.stack.truncate(start);
                    self.stack.push(c);
                }
                Op::DynAttr => {
                    self.op_dyn_attr(pos!())?;
                }
                Op::RecOverrides(rd) => {
                    self.op_rec_overrides(fi, rd, pos!())?;
                }
                Op::Jump(t) => {
                    ip = t as usize;
                    continue;
                }
                Op::JumpIfFalse(t) => {
                    let c = self.stack.pop().unwrap();
                    if val(c).tag() == Tag::False {
                        ip = t as usize;
                        continue;
                    }
                }
                Op::JumpIfTrue(t) => {
                    let c = self.stack.pop().unwrap();
                    if val(c).tag() == Tag::True {
                        ip = t as usize;
                        continue;
                    }
                }
                Op::Not => {
                    let c = self.stack.pop().unwrap();
                    let b = val(c).tag() == Tag::True;
                    self.stack.push(self.bool_cell(!b));
                }
                Op::Eq | Op::NEq => {
                    let b = self.stack.pop().unwrap();
                    let a = self.stack.pop().unwrap();
                    let ctx = if matches!(op, Op::Eq) {
                        "while testing two values for equality"
                    } else {
                        "while testing two values for inequality"
                    };
                    let r = self.eq_values(a, b, pos!(), ctx, true)?;
                    let r = if matches!(op, Op::Eq) { r } else { !r };
                    self.stack.push(self.bool_cell(r));
                }
                Op::Update => {
                    self.op_update()?;
                }
                Op::ConcatLists => {
                    let b = self.stack.pop().unwrap();
                    let a = self.stack.pop().unwrap();
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
                        // a and b stay reachable from native locals;
                        // elements are reachable through them.
                        self.new_list_value(&items)
                    };
                    let c = self.alloc_cell(v);
                    self.stack.push(c);
                }
                Op::ConcatStrings(d) => {
                    self.op_concat_strings(fi, d)?;
                }
                Op::Select(sym) => {
                    let c = *self.stack.last().unwrap();
                    self.force_attrs(c, pos!(), "while selecting an attribute")?;
                    let v = val(c);
                    let found = attrs_get(&v, Symbol(sym));
                    match found {
                        Some(a) => {
                            *self.stack.last_mut().unwrap() = a.val;
                        }
                        None => return Err(self.missing_attr_err(&v, Symbol(sym), pos!())),
                    }
                }
                Op::SelectOr { sym, target } => {
                    let c = *self.stack.last().unwrap();
                    self.force(c, pos!())?;
                    let v = val(c);
                    let found = if v.tag() == Tag::Attrs {
                        attrs_get(&v, Symbol(sym))
                    } else {
                        None
                    };
                    match found {
                        Some(a) => *self.stack.last_mut().unwrap() = a.val,
                        None => {
                            self.stack.pop();
                            ip = target as usize;
                            continue;
                        }
                    }
                }
                Op::SelectDyn => {
                    let name = self.stack.pop().unwrap();
                    let nb = self.force_string_no_ctx(
                        name,
                        pos!(),
                        "while evaluating an attribute name",
                    )?;
                    let sym = self.symbols.create(&nb);
                    let c = *self.stack.last().unwrap();
                    self.force_attrs(c, pos!(), "while selecting an attribute")?;
                    let v = val(c);
                    match attrs_get(&v, sym) {
                        Some(a) => *self.stack.last_mut().unwrap() = a.val,
                        None => return Err(self.missing_attr_err(&v, sym, pos!())),
                    }
                }
                Op::SelectDynOr { target } => {
                    let name = self.stack.pop().unwrap();
                    let nb = self.force_string_no_ctx(
                        name,
                        pos!(),
                        "while evaluating an attribute name",
                    )?;
                    let sym = self.symbols.create(&nb);
                    let c = *self.stack.last().unwrap();
                    self.force(c, pos!())?;
                    let v = val(c);
                    let found = if v.tag() == Tag::Attrs {
                        attrs_get(&v, sym)
                    } else {
                        None
                    };
                    match found {
                        Some(a) => *self.stack.last_mut().unwrap() = a.val,
                        None => {
                            self.stack.pop();
                            ip = target as usize;
                            continue;
                        }
                    }
                }
                Op::HasAttrPath(d) => {
                    self.op_has_attr_path(fi, d, pos!())?;
                }
                Op::Call(n) => {
                    let n = n as usize;
                    let args_start = self.stack.len() - n;
                    let fun = self.stack[args_start - 1];
                    let args: Vec<VRef> = self.stack[args_start..].to_vec();
                    let v = self.call_function(fun, &args, pos!())?;
                    self.stack.truncate(args_start - 1);
                    let c = self.alloc_cell(v);
                    self.stack.push(c);
                }
                Op::Ret => {
                    return Ok(self.stack.pop().unwrap());
                }
                Op::CurPos => {
                    let v = self.mk_pos(pos!());
                    let c = self.alloc_cell(v);
                    self.stack.push(c);
                }
                Op::AssertFail(t) => {
                    let text = &self.frames[fi].code.prog().texts[t as usize];
                    let msg = {
                        let mut m = b"assertion '".to_vec();
                        m.extend_from_slice(text);
                        m.extend_from_slice(b"' failed");
                        m
                    };
                    return Err(self.new_err(ErrKind::Assertion, msg, pos!()));
                }
                Op::PushWith => {
                    let c = self.stack.pop().unwrap();
                    self.frames[fi].with_local.push(c);
                }
                Op::PopWith => {
                    self.frames[fi].with_local.pop();
                }
                Op::Slide(n) => {
                    let top = self.stack.pop().unwrap();
                    let len = self.stack.len();
                    self.stack.truncate(len - n as usize);
                    self.stack.push(top);
                }
            }
            ip += 1;
        }
    }

    fn missing_attr_err(&mut self, attrs: &Value, sym: Symbol, pos: PosIdx) -> ErrId {
        let name = String::from_utf8_lossy(self.symbols.resolve(sym)).into_owned();
        let cands: Vec<String> = attrs_entries(attrs)
            .iter()
            .map(|a| String::from_utf8_lossy(self.symbols.resolve(Symbol(a.sym))).into_owned())
            .collect();
        let sugg = best_matches(cands.into_iter(), &name);
        let e = self.new_err(ErrKind::Eval, format!("attribute '{name}' missing"), pos);
        self.errors[e as usize].suggestions = sugg;
        e
    }

    // ---------------- ops with more logic ----------------

    fn op_dyn_attr(&mut self, pos: PosIdx) -> Result<(), ErrId> {
        let value = self.stack.pop().unwrap();
        let name = self.stack.pop().unwrap();
        self.force(name, pos)?;
        if val(name).tag() == Tag::Null {
            return Ok(());
        }
        let nb = self.force_string_no_ctx(
            name,
            pos,
            "while evaluating the name of a dynamic attribute",
        )?;
        let sym = self.symbols.create(&nb);
        let attrs_cell = *self.stack.last().unwrap();
        let av = val(attrs_cell);
        if let Some(existing) = attrs_get(&av, sym) {
            let at = self
                .positions
                .lookup(PosIdx(existing.pos))
                .map(|p| p.to_string())
                .unwrap_or_else(|| "«none»".into());
            let msg = format!(
                "dynamic attribute '{}' already defined at {}",
                String::from_utf8_lossy(&nb),
                at
            );
            return Err(self.new_err(ErrKind::Eval, msg, pos));
        }
        let mut entries: Vec<Attr> = attrs_entries(&av).to_vec();
        let idx = entries.partition_point(|a| a.sym < sym.0);
        entries.insert(
            idx,
            Attr {
                sym: sym.0,
                pos: pos.0,
                val: value,
            },
        );
        // `value` and old entries stay rooted via the operand stack (value
        // was popped but remains in a native local; conservative scan).
        let v = self.new_bindings_value(&entries);
        set(attrs_cell, v);
        Ok(())
    }

    fn op_rec_overrides(&mut self, fi: usize, rd: u32, pos: PosIdx) -> Result<(), ErrId> {
        let prog = self.frames[fi].code.prog();
        let rdesc = &prog.rec_descs[rd as usize];
        let desc = &prog.attrs_descs[rdesc.attrs_desc as usize];
        let attrs_cell = *self.stack.last().unwrap();
        let av = val(attrs_cell);
        let ov_attr = attrs_entries(&av)[rdesc.overrides_idx as usize];
        self.force_attrs(
            ov_attr.val,
            pos,
            "while evaluating the `__overrides` attribute",
        )?;
        let ov = val(ov_attr.val);
        let base = self.frames[fi].locals_base + rdesc.locals_start as usize;
        let mut entries: Vec<Attr> = attrs_entries(&av).to_vec();
        for o in attrs_entries(&ov) {
            if let Some(k) = desc.names.iter().position(|(s, _)| s.0 == o.sym) {
                // Overwrite the rec binding cell so references through the
                // rec scope see the override.
                let cell = self.stack[base + k];
                set(cell, val(o.val));
                entries[k] = Attr {
                    sym: o.sym,
                    pos: o.pos,
                    val: cell,
                };
            } else {
                let idx = entries.partition_point(|a| a.sym < o.sym);
                entries.insert(idx, *o);
            }
        }
        let v = self.new_bindings_value(&entries);
        set(attrs_cell, v);
        Ok(())
    }

    fn op_update(&mut self) -> Result<(), ErrId> {
        let left = self.stack.pop().unwrap();
        let right = self.stack.pop().unwrap();
        let (lv, rv) = (val(left), val(right));
        let (le, re) = (attrs_entries(&lv), attrs_entries(&rv));
        let v = if le.is_empty() {
            rv
        } else if re.is_empty() {
            lv
        } else {
            let mut entries: Vec<Attr> = Vec::with_capacity(le.len() + re.len());
            let (mut i, mut j) = (0, 0);
            while i < le.len() && j < re.len() {
                if le[i].sym == re[j].sym {
                    entries.push(re[j]);
                    i += 1;
                    j += 1;
                } else if le[i].sym < re[j].sym {
                    entries.push(le[i]);
                    i += 1;
                } else {
                    entries.push(re[j]);
                    j += 1;
                }
            }
            entries.extend_from_slice(&le[i..]);
            entries.extend_from_slice(&re[j..]);
            self.new_bindings_value(&entries)
        };
        let c = self.alloc_cell(v);
        self.stack.push(c);
        Ok(())
    }

    fn op_has_attr_path(&mut self, fi: usize, d: u32, pos: PosIdx) -> Result<(), ErrId> {
        let desc = &self.frames[fi].code.prog().haspath_descs[d as usize];
        let ndyn = desc.comps.iter().filter(|c| c.is_none()).count();
        let dyn_start = self.stack.len() - ndyn;
        let subj = self.stack[dyn_start - 1];
        let mut dyn_idx = 0usize;
        let mut cur = subj;
        let mut result = true;
        for comp in &desc.comps {
            self.force(cur, pos)?;
            let sym = match comp {
                Some(s) => *s,
                None => {
                    let name_cell = self.stack[dyn_start + dyn_idx];
                    dyn_idx += 1;
                    let nb = self.force_string_no_ctx(
                        name_cell,
                        pos,
                        "while evaluating an attribute name",
                    )?;
                    self.symbols.create(&nb)
                }
            };
            let v = val(cur);
            match (v.tag() == Tag::Attrs).then(|| attrs_get(&v, sym)).flatten() {
                Some(a) => cur = a.val,
                None => {
                    result = false;
                    break;
                }
            }
        }
        self.stack.truncate(dyn_start - 1);
        self.stack.push(self.bool_cell(result));
        Ok(())
    }

    fn op_concat_strings(&mut self, fi: usize, d: u32) -> Result<(), ErrId> {
        let desc = &self.frames[fi].code.prog().concat_descs[d as usize];
        let n = desc.poss.len();
        let force_string = desc.force_string;
        let pos = desc.pos;
        let poss: Vec<PosIdx> = desc.poss.clone();
        let start = self.stack.len() - n;

        #[derive(PartialEq, Clone, Copy)]
        enum Mode {
            Unset,
            Int,
            Float,
            Str(Tag), // first value's tag (String or Path or other-coerced)
        }
        let mut mode = if force_string {
            Mode::Str(Tag::String)
        } else {
            Mode::Unset
        };
        let mut acc_i: i64 = 0;
        let mut acc_f: f64 = 0.0;
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let mut ctx: Vec<u32> = Vec::new();
        let mut first = !force_string;

        for k in 0..n {
            let cell = self.stack[start + k];
            let i_pos = poss[k];
            self.force(cell, i_pos)?;
            let v = val(cell);
            if first {
                mode = match v.tag() {
                    Tag::Int => Mode::Int,
                    Tag::Float => Mode::Float,
                    t => Mode::Str(t),
                };
            }
            match mode {
                Mode::Int => match v.tag() {
                    Tag::Int => {
                        let rhs = v.as_int();
                        match acc_i.checked_add(rhs) {
                            Some(s) => acc_i = s,
                            None => {
                                let msg = format!(
                                    "integer overflow in adding {} + {}",
                                    acc_i, rhs
                                );
                                return Err(self.new_err(ErrKind::Eval, msg, i_pos));
                            }
                        }
                    }
                    Tag::Float => {
                        mode = Mode::Float;
                        acc_f = acc_i as f64 + v.as_float();
                    }
                    _ => {
                        let msg =
                            format!("cannot add {} to an integer", self.show_type(&v));
                        return Err(self.new_err(ErrKind::Eval, msg, i_pos));
                    }
                },
                Mode::Float => match v.tag() {
                    Tag::Int => acc_f += v.as_int() as f64,
                    Tag::Float => acc_f += v.as_float(),
                    _ => {
                        let msg = format!("cannot add {} to a float", self.show_type(&v));
                        return Err(self.new_err(ErrKind::Eval, msg, i_pos));
                    }
                },
                Mode::Str(first_tag) => {
                    let (part, pctx) = self.coerce_to_string(
                        cell,
                        i_pos,
                        "while evaluating a path segment",
                        false,
                        first_tag == Tag::String,
                        !first,
                    )?;
                    parts.push(part);
                    for c in pctx {
                        if !ctx.contains(&c) {
                            ctx.push(c);
                        }
                    }
                }
                Mode::Unset => unreachable!(),
            }
            first = false;
        }

        let v = match mode {
            Mode::Int => Value::int(acc_i),
            Mode::Float => Value::float(acc_f),
            Mode::Str(Tag::Path) => {
                if !ctx.is_empty() {
                    return Err(self.new_err(
                        ErrKind::Eval,
                        "a string that refers to a store path cannot be appended to a path",
                        pos,
                    ));
                }
                let joined: Vec<u8> = parts.concat();
                let canon = canon_path(&joined);
                self.new_path_value(&canon)
            }
            Mode::Str(_) => {
                let joined: Vec<u8> = parts.concat();
                let cp = self.make_ctx(&ctx);
                self.new_string_value(&joined, cp)
            }
            Mode::Unset => unreachable!(),
        };
        let c = self.alloc_cell(v);
        self.stack.truncate(start);
        self.stack.push(c);
        Ok(())
    }

    pub fn make_ctx(&mut self, ids: &[u32]) -> *mut u64 {
        if ids.is_empty() {
            return std::ptr::null_mut();
        }
        self.gc_check();
        self.heap.new_ctx(ids)
    }

    // ---------------- thunks / with ----------------

    fn make_thunk(&mut self, fi: usize, cid: u32, tag: Tag) -> VRef {
        let prog = self.frames[fi].code.prog();
        let child: &Chunk = &prog.chunks[cid as usize];
        let cur_chunk = self.frames[fi].code.chunk();
        let mut upvals: Vec<VRef> =
            Vec::with_capacity(child.with_captures as usize + child.captures.len());
        if child.with_captures > 0 {
            let f_upvals = self.frames[fi].upvals();
            let inherited = cur_chunk.with_captures as usize;
            upvals.extend_from_slice(&f_upvals[..inherited]);
            upvals.extend_from_slice(&self.frames[fi].with_local);
            debug_assert_eq!(upvals.len(), child.with_captures as usize);
        }
        let base = self.frames[fi].locals_base;
        for cap in &child.captures {
            match cap {
                crate::chunk::Cap::Local(s) => upvals.push(self.stack[base + *s as usize]),
                crate::chunk::Cap::Upval(i) => {
                    upvals.push(self.frames[fi].upvals()[*i as usize])
                }
            }
        }
        self.gc_check();
        let code = prog.code_ref(cid) as *const CodeRef as *const ();
        let v = self.heap.new_thunk(tag, code, &upvals);
        self.heap.alloc_value(v)
    }

    fn resolve_with(&mut self, fi: usize, sym: Symbol, pos: PosIdx) -> Result<VRef, ErrId> {
        // Innermost first: local with entries (last pushed first), then the
        // captured prefix in reverse (it is stored outermost-first).
        let n_local = self.frames[fi].with_local.len();
        let wc = self.frames[fi].code.chunk().with_captures as usize;
        for k in (0..n_local + wc).rev() {
            let cell = if k >= wc {
                self.frames[fi].with_local[k - wc]
            } else {
                self.frames[fi].upvals()[k]
            };
            self.force_attrs(
                cell,
                pos,
                "while evaluating the first subexpression of a with expression",
            )?;
            let v = val(cell);
            if let Some(a) = attrs_get(&v, sym) {
                return Ok(a.val);
            }
        }
        let name = String::from_utf8_lossy(self.symbols.resolve(sym)).into_owned();
        Err(self.new_err(
            ErrKind::UndefinedVar,
            format!("undefined variable '{name}'"),
            pos,
        ))
    }

    // ---------------- calls ----------------

    pub fn call_function(
        &mut self,
        fun: VRef,
        args: &[VRef],
        pos: PosIdx,
    ) -> Result<Value, ErrId> {
        self.call_depth += 1;
        let r = self.call_function_inner(fun, args, pos);
        self.call_depth -= 1;
        r
    }

    fn depth_check(&mut self, pos: PosIdx) -> Result<(), ErrId> {
        if self.call_depth > self.max_call_depth {
            return Err(self.new_err(
                ErrKind::StackOverflow,
                "stack overflow; max-call-depth exceeded",
                pos,
            ));
        }
        Ok(())
    }

    fn call_function_inner(
        &mut self,
        fun: VRef,
        args: &[VRef],
        pos: PosIdx,
    ) -> Result<Value, ErrId> {
        self.depth_check(pos)?;
        self.force(fun, pos)?;
        let mut vcur = val(fun);
        let mut i = 0usize;

        while i < args.len() {
            match vcur.tag() {
                Tag::Closure => {
                    vcur = self.call_closure(vcur, args[i], pos)?;
                    i += 1;
                }
                Tag::PrimOp => {
                    let def = primop_of(&vcur);
                    let needed = def.arity as usize;
                    let remaining = args.len() - i;
                    if remaining < needed {
                        // Not enough arguments: build a PrimOpApp chain.
                        self.gc_check();
                        let v = self
                            .heap
                            .new_primapp(vcur.w1 as *const (), &args[i..]);
                        return Ok(v);
                    }
                    let f = def.func;
                    vcur = f(self, def, &args[i..i + needed], pos)?;
                    i += needed;
                }
                Tag::PrimOpApp => {
                    let (def, done) = primapp_parts(&vcur);
                    let needed = def.arity as usize - done.len();
                    let remaining = args.len() - i;
                    if remaining < needed {
                        let mut all: Vec<VRef> = done.to_vec();
                        all.extend_from_slice(&args[i..]);
                        // `done` cells stay rooted via vcur (native local).
                        self.gc_check();
                        let prim = def as *const PrimOpDef as *const ();
                        let v = self.heap.new_primapp(prim, &all);
                        return Ok(v);
                    }
                    let mut all: Vec<VRef> = done.to_vec();
                    all.extend_from_slice(&args[i..i + needed]);
                    let scope = self.temp_scope();
                    self.temp_roots.extend_from_slice(&all);
                    let f = def.func;
                    let r = f(self, def, &all, pos);
                    self.temp_end(scope);
                    vcur = r?;
                    i += needed;
                }
                Tag::Attrs => {
                    let functor = attrs_get(&vcur, self.syms.functor);
                    match functor {
                        Some(f) => {
                            let self_cell = self.alloc_cell(vcur);
                            let scope = self.temp_scope();
                            self.temp_roots.push(self_cell);
                            let r = self
                                .call_function(f.val, &[self_cell, args[i]], PosIdx(f.pos));
                            self.temp_end(scope);
                            let v = r.map_err(|e| {
                                self.add_trace(
                                    e,
                                    pos,
                                    "while calling a functor (an attribute set with a '__functor' attribute)",
                                );
                                e
                            })?;
                            vcur = v;
                            i += 1;
                        }
                        None => return Err(self.not_a_function_err(&vcur, pos)),
                    }
                }
                _ => return Err(self.not_a_function_err(&vcur, pos)),
            }
            // `vcur` may need forcing between applications (e.g. a lambda
            // body returning a thunk value cannot happen — run_code returns
            // WHNF-or-thunk copies; force via a temp cell when required).
            if i < args.len() && vcur.tag() == Tag::Thunk {
                let c = self.alloc_cell(vcur);
                let scope = self.temp_scope();
                self.temp_roots.push(c);
                let r = self.force(c, pos);
                self.temp_end(scope);
                r?;
                vcur = val(c);
            }
        }
        Ok(vcur)
    }

    fn not_a_function_err(&mut self, v: &Value, pos: PosIdx) -> ErrId {
        let printed = crate::print::print_value_err(self, v);
        let msg = format!(
            "attempt to call something which is not a function but {}: {}",
            self.show_type(v),
            printed
        );
        self.new_err(ErrKind::Type, msg, pos)
    }

    /// Apply a closure to one argument (the C++ lambda branch of
    /// callFunction).
    fn call_closure(&mut self, vcur: Value, arg: VRef, pos: PosIdx) -> Result<Value, ErrId> {
        let (code, _) = thunk_code(&vcur);
        let chunk = code.chunk();
        let spec = chunk.lambda.as_ref().expect("closure without lambda spec");
        let base = self.stack.len();

        let lambda_name: String = if chunk.name.is_set() {
            format!(
                "'{}'",
                String::from_utf8_lossy(self.symbols.resolve(chunk.name))
            )
        } else {
            "anonymous lambda".into()
        };

        let mut pending_defaults: Vec<(usize, u32)> = Vec::new(); // (stack idx, chunk)

        if let Some(formals) = &spec.formals {
            self.force_attrs(
                arg,
                chunk.pos,
                "while evaluating the value passed for the lambda argument",
            )
            .map_err(|e| {
                if pos.is_set() {
                    self.add_trace(e, pos, "from call site");
                }
                e
            })?;
            if spec.arg.is_set() {
                self.stack.push(arg);
            }
            let attrs = val(arg);
            let mut attrs_used = 0usize;
            for f in &formals.formals {
                match attrs_get(&attrs, f.name) {
                    Some(a) => {
                        attrs_used += 1;
                        self.stack.push(a.val);
                    }
                    None => match f.default {
                        Some(cid) => {
                            let c = self.alloc_cell(Value::make(Tag::Blackhole, 0));
                            pending_defaults.push((self.stack.len(), cid));
                            self.stack.push(c);
                        }
                        None => {
                            let name = lambda_name.clone();
                            let fname =
                                String::from_utf8_lossy(self.symbols.resolve(f.name)).into_owned();
                            let e = self.new_err(
                                ErrKind::Type,
                                format!(
                                    "function {} called without required argument '{}'",
                                    name, fname
                                ),
                                chunk.pos,
                            );
                            if pos.is_set() {
                                self.add_trace(e, pos, "from call site");
                            }
                            self.stack.truncate(base);
                            return Err(e);
                        }
                    },
                }
            }
            if !formals.ellipsis && attrs_used != attrs_entries(&attrs).len() {
                for a in attrs_entries(&attrs) {
                    if !formals.formals.iter().any(|f| f.name.0 == a.sym) {
                        let name = lambda_name.clone();
                        let aname = String::from_utf8_lossy(self.symbols.resolve(Symbol(a.sym)))
                            .into_owned();
                        let cands: Vec<String> = formals
                            .formals
                            .iter()
                            .map(|f| {
                                String::from_utf8_lossy(self.symbols.resolve(f.name)).into_owned()
                            })
                            .collect();
                        let sugg = best_matches(cands.into_iter(), &aname);
                        let e = self.new_err(
                            ErrKind::Type,
                            format!(
                                "function {} called with unexpected argument '{}'",
                                name, aname
                            ),
                            chunk.pos,
                        );
                        self.errors[e as usize].suggestions = sugg;
                        if pos.is_set() {
                            self.add_trace(e, pos, "from call site");
                        }
                        self.stack.truncate(base);
                        return Err(e);
                    }
                }
                unreachable!();
            }
        } else {
            self.stack.push(arg);
        }

        // Frame for the body; fill deferred defaults now that all formal
        // slots exist.
        self.frames.push(Frame {
            code,
            data: vcur,
            locals_base: base,
            with_local: Vec::new(),
        });
        let fi = self.frames.len() - 1;
        for (slot_idx, cid) in pending_defaults {
            let t = self.make_thunk(fi, cid, Tag::Thunk);
            let dst = self.stack[slot_idx];
            set(dst, val(t));
        }
        let r = self.run_top_frame();
        self.frames.pop();
        let out = r.map(val);
        self.stack.truncate(base);
        out
    }

    // ---------------- equality ----------------

    pub fn eq_values(
        &mut self,
        a: VRef,
        b: VRef,
        pos: PosIdx,
        ctx: &str,
        top: bool,
    ) -> Result<bool, ErrId> {
        self.call_depth += 1;
        let r = self.eq_values_inner(a, b, pos, ctx, top);
        self.call_depth -= 1;
        r
    }

    fn eq_values_inner(
        &mut self,
        a: VRef,
        b: VRef,
        pos: PosIdx,
        ctx: &str,
        top: bool,
    ) -> Result<bool, ErrId> {
        self.depth_check(pos)?;
        self.force(a, pos)?;
        self.force(b, pos)?;

        // Pointer-equality fast path — but not at the top level, where C++
        // compares freshly evaluated temporaries (so `f == f` is false for
        // functions).
        if !top && a == b {
            return Ok(true);
        }

        let (va, vb) = (val(a), val(b));

        // int/float cross-type equality.
        match (va.tag(), vb.tag()) {
            (Tag::Int, Tag::Float) => return Ok(va.as_int() as f64 == vb.as_float()),
            (Tag::Float, Tag::Int) => return Ok(va.as_float() == vb.as_int() as f64),
            _ => {}
        }
        let same_type = match (va.tag(), vb.tag()) {
            (Tag::True | Tag::False, Tag::True | Tag::False) => true,
            (x, y) => x == y,
        };
        if !same_type {
            return Ok(false);
        }

        match va.tag() {
            Tag::Int => Ok(va.as_int() == vb.as_int()),
            Tag::Float => Ok(va.as_float() == vb.as_float()),
            Tag::True | Tag::False => Ok(va.tag() == vb.tag()),
            Tag::Null => Ok(true),
            Tag::String => Ok(str_bytes(&va) == str_bytes(&vb)),
            Tag::Path => Ok(path_bytes(&va) == path_bytes(&vb)),
            Tag::List => {
                let (ea, eb) = (list_elems(&va), list_elems(&vb));
                if ea.len() != eb.len() {
                    return Ok(false);
                }
                for k in 0..ea.len() {
                    if !self.eq_values(ea[k], eb[k], pos, ctx, false)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Tag::Attrs => {
                // Derivations compare by outPath.
                if self.is_derivation(&va)? && self.is_derivation(&vb)? {
                    let i = attrs_get(&va, self.syms.out_path);
                    let j = attrs_get(&vb, self.syms.out_path);
                    if let (Some(i), Some(j)) = (i, j) {
                        return self.eq_values(i.val, j.val, pos, ctx, false);
                    }
                }
                let (ea, eb) = (attrs_entries(&va), attrs_entries(&vb));
                if ea.len() != eb.len() {
                    return Ok(false);
                }
                for k in 0..ea.len() {
                    if ea[k].sym != eb[k].sym
                        || !self.eq_values(ea[k].val, eb[k].val, pos, ctx, false)?
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            // Functions are incomparable.
            Tag::Closure | Tag::PrimOp | Tag::PrimOpApp => Ok(false),
            Tag::Thunk | Tag::Blackhole | Tag::Failed => unreachable!("forced"),
        }
    }

    pub fn is_derivation(&mut self, v: &Value) -> Result<bool, ErrId> {
        if v.tag() != Tag::Attrs {
            return Ok(false);
        }
        let Some(t) = attrs_get(v, self.syms.type_) else {
            return Ok(false);
        };
        self.force(t.val, PosIdx(t.pos))?;
        let tv = val(t.val);
        Ok(tv.tag() == Tag::String && str_bytes(&tv) == b"derivation")
    }

    // ---------------- coercion ----------------

    /// Port of EvalState::coerceToString. Returns bytes + context ids.
    pub fn coerce_to_string(
        &mut self,
        cell: VRef,
        pos: PosIdx,
        ctx: &str,
        coerce_more: bool,
        copy_to_store: bool,
        canonicalize_path: bool,
    ) -> Result<(Vec<u8>, Vec<u32>), ErrId> {
        self.call_depth += 1;
        let r =
            self.coerce_inner(cell, pos, ctx, coerce_more, copy_to_store, canonicalize_path);
        self.call_depth -= 1;
        r
    }

    fn coerce_inner(
        &mut self,
        cell: VRef,
        pos: PosIdx,
        ctx: &str,
        coerce_more: bool,
        copy_to_store: bool,
        canonicalize_path: bool,
    ) -> Result<(Vec<u8>, Vec<u32>), ErrId> {
        self.depth_check(pos)?;
        self.force(cell, pos)?;
        let v = val(cell);
        match v.tag() {
            Tag::String => {
                let mut ctxs = Vec::new();
                let cp = str_ctx(&v);
                if !cp.is_null() {
                    // SAFETY: ctx objects hold u32 ids.
                    unsafe {
                        let len = value::header_len(*cp);
                        let ids = std::slice::from_raw_parts(cp.add(1) as *const u32, len);
                        ctxs.extend_from_slice(ids);
                    }
                }
                Ok((str_bytes(&v).to_vec(), ctxs))
            }
            Tag::Path => {
                if copy_to_store {
                    let e = self.new_err(
                        ErrKind::Eval,
                        format!(
                            "cannot copy '{}' to the Nix store: store operations are not implemented in jinx M2",
                            String::from_utf8_lossy(path_bytes(&v))
                        ),
                        pos,
                    );
                    self.add_trace(e, pos, ctx);
                    Err(e)
                } else {
                    // canonicalizePath=false preserves literal trailing
                    // slashes; our path payload is stored as-is either way.
                    let _ = canonicalize_path;
                    Ok((path_bytes(&v).to_vec(), Vec::new()))
                }
            }
            Tag::Attrs => {
                if let Some(f) = attrs_get(&v, self.syms.to_string) {
                    let r = self.call_function(f.val, &[cell], pos)?;
                    let rc = self.alloc_cell(r);
                    let scope = self.temp_scope();
                    self.temp_roots.push(rc);
                    let out = self.coerce_to_string(
                        rc,
                        pos,
                        "while evaluating the result of the `__toString` attribute",
                        coerce_more,
                        copy_to_store,
                        canonicalize_path,
                    );
                    self.temp_end(scope);
                    return out;
                }
                if let Some(op) = attrs_get(&v, self.syms.out_path) {
                    return self.coerce_to_string(
                        op.val,
                        pos,
                        ctx,
                        coerce_more,
                        copy_to_store,
                        canonicalize_path,
                    );
                }
                Err(self.cannot_coerce_err(&v, pos, ctx))
            }
            _ if coerce_more => match v.tag() {
                Tag::True => Ok((b"1".to_vec(), Vec::new())),
                Tag::False | Tag::Null => Ok((Vec::new(), Vec::new())),
                Tag::Int => Ok((v.as_int().to_string().into_bytes(), Vec::new())),
                Tag::Float => Ok((format!("{:.6}", v.as_float()).into_bytes(), Vec::new())),
                Tag::List => {
                    let elems = list_elems(&v);
                    let mut out: Vec<u8> = Vec::new();
                    let mut ctxs: Vec<u32> = Vec::new();
                    for (k, &el) in elems.iter().enumerate() {
                        let (part, pctx) = self
                            .coerce_to_string(
                                el,
                                pos,
                                "while evaluating one element of the list",
                                coerce_more,
                                copy_to_store,
                                canonicalize_path,
                            )
                            .map_err(|e| {
                                self.add_trace(e, pos, ctx);
                                e
                            })?;
                        out.extend_from_slice(&part);
                        for c in pctx {
                            if !ctxs.contains(&c) {
                                ctxs.push(c);
                            }
                        }
                        let elv = val(el);
                        let el_empty_list =
                            elv.tag() == Tag::List && list_elems(&elv).is_empty();
                        if k + 1 < elems.len() && !el_empty_list {
                            out.push(b' ');
                        }
                    }
                    Ok((out, ctxs))
                }
                _ => Err(self.cannot_coerce_err(&v, pos, ctx)),
            },
            _ => Err(self.cannot_coerce_err(&v, pos, ctx)),
        }
    }

    fn cannot_coerce_err(&mut self, v: &Value, pos: PosIdx, ctx: &str) -> ErrId {
        let printed = crate::print::print_value_err(self, v);
        let msg = format!(
            "cannot coerce {} to a string: {}",
            self.show_type(v),
            printed
        );
        let e = self.new_err(ErrKind::Type, msg, pos);
        self.add_trace(e, pos, ctx);
        e
    }

    // ---------------- misc value builders ----------------

    /// `__curPos` / unsafeGetAttrPos-style position attrsets.
    pub fn mk_pos(&mut self, pos: PosIdx) -> Value {
        let is_path = matches!(
            self.positions.origin_of(pos),
            Some(jinx_syntax::pos::Origin::Path { .. })
        );
        if !is_path {
            return Value::null();
        }
        let p = self.positions.lookup(pos).unwrap();
        let file = match self.positions.origin_of(pos) {
            Some(jinx_syntax::pos::Origin::Path { path, .. }) => path.clone(),
            _ => unreachable!(),
        };
        let scope = self.temp_scope();
        let fv = self.new_string_value(file.as_bytes(), std::ptr::null_mut());
        let fc = self.alloc_cell(fv);
        self.temp_roots.push(fc);
        let lc = self.alloc_cell(Value::int(p.line as i64));
        self.temp_roots.push(lc);
        let cc = self.alloc_cell(Value::int(p.column as i64));
        self.temp_roots.push(cc);
        let mut entries = [
            Attr {
                sym: self.syms.file.0,
                pos: 0,
                val: fc,
            },
            Attr {
                sym: self.syms.line.0,
                pos: 0,
                val: lc,
            },
            Attr {
                sym: self.syms.column.0,
                pos: 0,
                val: cc,
            },
        ];
        entries.sort_by_key(|a| a.sym);
        let v = self.new_bindings_value(&entries);
        self.temp_end(scope);
        v
    }
}

/// Lexical path canonicalization matching C++ `CanonPath` (absolute paths;
/// collapses `//`, `.` and `..`; no trailing slash).
pub fn canon_path(p: &[u8]) -> Vec<u8> {
    let mut comps: Vec<&[u8]> = Vec::new();
    for comp in p.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                comps.pop();
            }
            c => comps.push(c),
        }
    }
    let mut out = Vec::with_capacity(p.len());
    if comps.is_empty() {
        out.push(b'/');
        return out;
    }
    for c in comps {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    out
}

/// Convenience for builtins: make an immortal-safe VRef from a raw pointer.
pub fn vref(p: *mut Value) -> VRef {
    NonNull::new(p).unwrap()
}
