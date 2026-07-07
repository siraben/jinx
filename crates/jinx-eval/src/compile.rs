//! AST -> Chunk compiler with flat closures (Tvix-style upvalue capture; no
//! env-pointer chains). Mirrors the evaluation order and error contexts of
//! C++ eval.cc, including the `maybeThunk` optimizations (variables and
//! constants are not wrapped in thunks).
//!
//! Scoping model:
//! - Each `FnState` compiles one chunk (top level, thunk, or lambda body).
//! - Locals are absolute operand-stack slots (relative to the frame's
//!   `locals_base`); the compiler simulates the stack height so `let`/`rec`
//!   binding cells can live interleaved with expression operands.
//! - Cross-chunk references become upvalues, materialized by the parent at
//!   thunk/closure creation from its own locals/upvalues (`Cap`).
//! - `with` is a runtime chain: each frame's local with-entries plus a
//!   captured prefix (`Chunk::with_captures` upvalues, outermost first).
//!   Lexical hits never consult it; only statically-unresolved variables
//!   compile to `ResolveWith`.

use rustc_hash::FxHashMap;

use jinx_syntax::ast::*;
use jinx_syntax::pos::{PosIdx, NO_POS};
use jinx_syntax::symbol::{Symbol, SymbolTable};

use crate::chunk::*;
use crate::immortal;
use crate::value::{VRef, Value};

/// Interned symbols the compiler/VM need by id.
pub struct SpecialSyms {
    pub overrides: Symbol,
    pub functor: Symbol,
    pub out_path: Symbol,
    pub type_: Symbol,
    pub value: Symbol,
    pub success: Symbol,
    pub file: Symbol,
    pub line: Symbol,
    pub column: Symbol,
    pub name: Symbol,
    pub to_string: Symbol,
    pub drv_path: Symbol,
}

impl SpecialSyms {
    pub fn new(symbols: &mut SymbolTable) -> Self {
        SpecialSyms {
            overrides: symbols.create(b"__overrides"),
            functor: symbols.create(b"__functor"),
            out_path: symbols.create(b"outPath"),
            type_: symbols.create(b"type"),
            value: symbols.create(b"value"),
            success: symbols.create(b"success"),
            file: symbols.create(b"file"),
            line: symbols.create(b"line"),
            column: symbols.create(b"column"),
            name: symbols.create(b"name"),
            to_string: symbols.create(b"__toString"),
            drv_path: symbols.create(b"drvPath"),
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Hash, Debug)]
enum Key {
    Sym(Symbol),
    /// inherit-from slot: (owning ExprAttrs node, displacement).
    From(u32, u32),
}

#[derive(PartialEq, Eq, Clone, Hash)]
enum ConstKey {
    Int(i64),
    Float(u64),
    Str(Vec<u8>),
    Path(Vec<u8>),
    Global(Symbol),
    EmptyList,
    Bool(bool),
}

enum Resolved {
    Local(u32),
    Upval(u32),
    /// Const index of the global's immortal cell.
    Global(u32),
    /// Not statically bound: consult the with-chain.
    With,
}

struct FnState {
    chunk: Chunk,
    /// Reserved index in `prog.chunks`.
    chunk_idx: u32,
    /// (key, absolute slot); innermost bindings last.
    locals: Vec<(Key, u32)>,
    /// Named (non-with) captures; final upval index = with_captures + i.
    upvals: Vec<(Key, Cap)>,
    /// Simulated operand stack height.
    height: u32,
    /// Peak `height` reached (recorded into `Chunk::max_height` for the JIT).
    max_height: u32,
    /// Lexical `with` count within this chunk at the current point.
    with_local: u32,
}

impl FnState {
    fn with_total(&self) -> u32 {
        self.chunk.with_captures + self.with_local
    }
}

pub struct Compiler<'a> {
    exprs: &'a Exprs,
    symbols: &'a SymbolTable,
    globals: &'a FxHashMap<Symbol, VRef>,
    prog: Program,
    const_map: FxHashMap<ConstKey, u32>,
    states: Vec<FnState>,
    /// Owning attrs node for `Expr::InheritFrom` displacements while
    /// compiling inherit-from definitions.
    from_owner: Option<u32>,
    empty_list_cell: VRef,
    /// Lambda naming: `ExprLambda::setName` (nixexpr.cc) sets a lambda's name
    /// from the attr/let binding it is directly bound to, recursing into the
    /// curried body chain. Keyed by `ExprId.0`.
    lambda_names: FxHashMap<u32, Symbol>,
}

pub fn compile_program(
    exprs: &Exprs,
    root: ExprId,
    symbols: &SymbolTable,
    globals: &FxHashMap<Symbol, VRef>,
    empty_list_cell: VRef,
) -> &'static Program {
    let mut c = Compiler {
        exprs,
        symbols,
        globals,
        prog: Program {
            chunks: Vec::new(),
            consts: Vec::new(),
            attrs_descs: Vec::new(),
            rec_descs: Vec::new(),
            concat_descs: Vec::new(),
            haspath_descs: Vec::new(),
            texts: Vec::new(),
            refs: Vec::new(),
            select_caches: Vec::new(),
        },
        const_map: FxHashMap::default(),
        states: Vec::new(),
        from_owner: None,
        empty_list_cell,
        lambda_names: FxHashMap::default(),
    };
    // Chunk 0 = entry.
    c.push_state(0, NO_POS, Symbol(0));
    c.compile_expr(root);
    c.emit(Op::Ret, NO_POS);
    let id = c.pop_state();
    debug_assert_eq!(id, 0);
    c.prog.leak()
}

impl<'a> Compiler<'a> {
    // ---------------- state / emission helpers ----------------

    fn push_state(&mut self, with_captures: u32, pos: PosIdx, name: Symbol) {
        let chunk = Chunk {
            with_captures,
            pos,
            name,
            ..Default::default()
        };
        // Reserve the chunk slot now so ids are stable (entry chunk = 0).
        self.prog.chunks.push(Chunk::default());
        let chunk_idx = (self.prog.chunks.len() - 1) as u32;
        self.states.push(FnState {
            chunk,
            chunk_idx,
            locals: Vec::new(),
            upvals: Vec::new(),
            height: 0,
            max_height: 0,
            with_local: 0,
        });
    }

    fn pop_state(&mut self) -> u32 {
        let mut st = self.states.pop().unwrap();
        st.chunk.captures = st.upvals.iter().map(|(_, c)| *c).collect();
        st.chunk.max_height = st.max_height;
        self.prog.chunks[st.chunk_idx as usize] = st.chunk;
        st.chunk_idx
    }

    fn st(&mut self) -> &mut FnState {
        self.states.last_mut().unwrap()
    }

    fn emit(&mut self, op: Op, pos: PosIdx) {
        let st = self.states.last_mut().unwrap();
        let idx = st.chunk.ops.len() as u32;
        if pos.is_set() && st.chunk.poss.last().map(|e| e.1) != Some(pos) {
            st.chunk.poss.push((idx, pos));
        }
        st.chunk.ops.push(op);
    }

    fn here(&self) -> u32 {
        self.states.last().unwrap().chunk.ops.len() as u32
    }

    /// Emit a jump-family op with a placeholder target; patch later.
    fn emit_jump(&mut self, mk: fn(u32) -> Op, pos: PosIdx) -> usize {
        self.emit(mk(u32::MAX), pos);
        self.states.last().unwrap().chunk.ops.len() - 1
    }

    fn patch_jump(&mut self, at: usize) {
        let target = self.here();
        let st = self.states.last_mut().unwrap();
        match &mut st.chunk.ops[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfTrue(t)
            | Op::SelectOr { target: t, .. }
            | Op::SelectDynOr { target: t } => *t = target,
            op => panic!("patching non-jump {op:?}"),
        }
    }

    fn bump(&mut self, delta: i64) {
        let st = self.states.last_mut().unwrap();
        st.height = (st.height as i64 + delta) as u32;
        st.max_height = st.max_height.max(st.height);
    }

    fn height(&self) -> u32 {
        self.states.last().unwrap().height
    }

    fn set_height(&mut self, h: u32) {
        let st = self.states.last_mut().unwrap();
        st.height = h;
        st.max_height = st.max_height.max(h);
    }

    // ---------------- constants ----------------

    fn const_idx(&mut self, key: ConstKey, mk: impl FnOnce() -> VRef) -> u32 {
        if let Some(&i) = self.const_map.get(&key) {
            return i;
        }
        let cell = mk();
        self.prog.consts.push(cell);
        let i = (self.prog.consts.len() - 1) as u32;
        self.const_map.insert(key, i);
        i
    }

    fn push_const(&mut self, key: ConstKey, mk: impl FnOnce() -> VRef, pos: PosIdx) {
        let i = self.const_idx(key, mk);
        self.emit(Op::Const(i), pos);
        self.bump(1);
    }

    fn push_literal(&mut self, e: &Expr, pos: PosIdx) {
        match e {
            Expr::Int(i) => {
                let i = *i;
                self.push_const(ConstKey::Int(i), || immortal::cell(Value::int(i)), pos)
            }
            Expr::Float(f) => {
                let f = *f;
                self.push_const(
                    ConstKey::Float(f.to_bits()),
                    || immortal::cell(Value::float(f)),
                    pos,
                )
            }
            Expr::String(s) => {
                let sv = s.clone();
                self.push_const(
                    ConstKey::Str(s.clone()),
                    || immortal::cell(immortal::string(&sv)),
                    pos,
                )
            }
            Expr::Path(p) => {
                let pv = p.clone();
                self.push_const(
                    ConstKey::Path(p.clone()),
                    || immortal::cell(immortal::path(&pv)),
                    pos,
                )
            }
            _ => unreachable!(),
        }
    }

    // ---------------- variable resolution ----------------

    /// Resolve `key` in the innermost state; `max_slot` (exclusive bound)
    /// filters locals of the innermost state only (used for `inherit`).
    fn resolve(&mut self, key: Key, max_slot: Option<u32>) -> Resolved {
        let top = self.states.len() - 1;
        self.resolve_at(top, key, max_slot)
    }

    fn resolve_at(&mut self, si: usize, key: Key, max_slot: Option<u32>) -> Resolved {
        // Innermost locals first.
        for &(k, slot) in self.states[si].locals.iter().rev() {
            if k == key {
                if let Some(m) = max_slot {
                    if slot >= m {
                        continue;
                    }
                }
                return Resolved::Local(slot);
            }
        }
        if si == 0 {
            if let Key::Sym(s) = key {
                if let Some(&cell) = self.globals.get(&s) {
                    let idx = self.const_idx(ConstKey::Global(s), || cell);
                    return Resolved::Global(idx);
                }
            }
            return Resolved::With;
        }
        // Check for an existing capture.
        if let Some(i) = self.states[si].upvals.iter().position(|(k, _)| *k == key) {
            let wc = self.states[si].chunk.with_captures;
            return Resolved::Upval(wc + i as u32);
        }
        match self.resolve_at(si - 1, key, None) {
            Resolved::Local(s) => self.add_upval(si, key, Cap::Local(s)),
            Resolved::Upval(i) => self.add_upval(si, key, Cap::Upval(i)),
            r => r,
        }
    }

    fn add_upval(&mut self, si: usize, key: Key, cap: Cap) -> Resolved {
        let st = &mut self.states[si];
        st.upvals.push((key, cap));
        Resolved::Upval(st.chunk.with_captures + (st.upvals.len() - 1) as u32)
    }

    // ---------------- expression compilation ----------------

    /// Evaluate `e`, leaving its (WHNF, per C++ eval discipline) result on
    /// the stack. Net height +1.
    fn compile_expr(&mut self, id: ExprId) {
        match self.exprs.get(id) {
            e @ (Expr::Int(_) | Expr::Float(_) | Expr::String(_) | Expr::Path(_)) => {
                self.push_literal(e, NO_POS)
            }
            Expr::Var { pos, name } => self.compile_var_eval(*name, *pos),
            Expr::InheritFrom { pos, displ } => {
                let owner = self.from_owner.expect("InheritFrom outside inherit(...)");
                match self.resolve(Key::From(owner, *displ), None) {
                    Resolved::Local(s) => self.emit(Op::GetLocal(s), *pos),
                    Resolved::Upval(i) => self.emit(Op::GetUpval(i), *pos),
                    _ => unreachable!("inherit-from slot always statically bound"),
                }
                self.bump(1);
                self.emit(Op::Force, *pos);
            }
            Expr::Select {
                pos,
                e,
                attrpath,
                def,
            } => self.compile_select(*e, attrpath, *def, *pos, id),
            Expr::OpHasAttr { e, attrpath } => {
                self.compile_expr(*e);
                let mut comps: Vec<Option<Symbol>> = Vec::with_capacity(attrpath.len());
                let mut ndyn = 0u32;
                for an in attrpath.iter() {
                    if let Some(de) = an.expr {
                        self.compile_expr(de);
                        ndyn += 1;
                        comps.push(None);
                    } else {
                        comps.push(Some(an.symbol));
                    }
                }
                self.prog.haspath_descs.push(HasPathDesc { comps });
                let d = (self.prog.haspath_descs.len() - 1) as u32;
                self.emit(Op::HasAttrPath(d), NO_POS);
                self.bump(-(1 + ndyn as i64) + 1);
            }
            Expr::Attrs(_) => self.compile_attrs(id, None),
            Expr::List(elems) => {
                let elems: Vec<ExprId> = elems.clone();
                for e in &elems {
                    self.compile_maybe_thunk(*e, None);
                }
                self.emit(Op::MakeList(elems.len() as u32), NO_POS);
                self.bump(-(elems.len() as i64) + 1);
            }
            Expr::Lambda(_) => {
                let cid = self.compile_lambda(id);
                self.emit(Op::MakeClosure(cid), self.lambda_pos(id));
                self.bump(1);
            }
            Expr::Call {
                pos, fun, args, ..
            } => {
                let (fun, args, pos) = (*fun, args.clone(), *pos);
                self.compile_expr(fun);
                for a in &args {
                    self.compile_maybe_thunk(*a, None);
                }
                self.emit(Op::Call(args.len() as u32), pos);
                self.bump(-(args.len() as i64));
            }
            Expr::Let { attrs, body } => {
                let (attrs, body) = (*attrs, *body);
                let n = self.compile_bindings_scope(attrs, true);
                self.compile_expr(body);
                if n > 0 {
                    self.emit(Op::Slide(n), NO_POS);
                    self.bump(-(n as i64));
                }
                self.pop_binding_scope(n);
            }
            Expr::With { pos, attrs, body } => {
                let (pos, attrs, body) = (*pos, *attrs, *body);
                self.compile_maybe_thunk(attrs, None);
                self.emit(Op::PushWith, pos);
                self.bump(-1);
                self.st().with_local += 1;
                self.compile_expr(body);
                self.st().with_local -= 1;
                self.emit(Op::PopWith, pos);
            }
            Expr::If {
                pos,
                cond,
                then,
                else_,
            } => {
                let (pos, cond, then, else_) = (*pos, *cond, *then, *else_);
                self.compile_expr(cond);
                self.emit(Op::ForceBool(0), pos);
                let jf = self.emit_jump(Op::JumpIfFalse, pos);
                self.bump(-1);
                let h = self.height();
                self.compile_expr(then);
                let jend = self.emit_jump(Op::Jump, NO_POS);
                self.patch_jump(jf);
                self.set_height(h);
                self.compile_expr(else_);
                self.patch_jump(jend);
            }
            Expr::Assert { pos, cond, body } => {
                let (pos, cond, body) = (*pos, *cond, *body);
                self.compile_expr(cond);
                self.emit(Op::ForceBool(1), pos);
                let jt = self.emit_jump(Op::JumpIfTrue, pos);
                self.bump(-1);
                let text = jinx_syntax::show::show(self.exprs, self.symbols, cond);
                self.prog.texts.push(text);
                let t = (self.prog.texts.len() - 1) as u32;
                // If the condition is `a == b`, mirror ExprAssert's special
                // case: re-evaluate both sides and run assertEqValues to build
                // a detailed inequality message before the generic failure.
                if let Expr::OpEq(a, b) = self.exprs.get(cond) {
                    let (a, b) = (*a, *b);
                    self.compile_expr(a);
                    self.compile_expr(b);
                    self.emit(Op::AssertEq(t), pos);
                    self.bump(-2);
                }
                self.emit(Op::AssertFail(t), pos);
                self.patch_jump(jt);
                self.compile_expr(body);
            }
            Expr::OpNot(e) => {
                let e = *e;
                // ExprOpNot::eval forces its argument at `e->getPos()` with
                // the "in the argument of the not operator" context.
                let npos = self.expr_pos(e);
                // Thunk the operand so that `ForceBool`'s error context wraps
                // its *evaluation* (matching C++ `evalBool`), not just the
                // final type check.
                self.compile_maybe_thunk(e, None);
                self.emit(Op::ForceBool(2), npos);
                self.emit(Op::Not, NO_POS);
            }
            Expr::OpEq(a, b) => {
                let (a, b) = (*a, *b);
                self.compile_expr(a);
                self.compile_expr(b);
                self.emit(Op::Eq, NO_POS);
                self.bump(-1);
            }
            Expr::OpNEq(a, b) => {
                let (a, b) = (*a, *b);
                self.compile_expr(a);
                self.compile_expr(b);
                self.emit(Op::NEq, NO_POS);
                self.bump(-1);
            }
            Expr::OpAnd(pos, a, b) => self.compile_bool_op(*pos, *a, *b, 3, 4, false, false),
            Expr::OpOr(pos, a, b) => self.compile_bool_op(*pos, *a, *b, 5, 6, true, true),
            Expr::OpImpl(pos, a, b) => self.compile_bool_op(*pos, *a, *b, 7, 8, false, true),
            Expr::OpUpdate(_pos, a, b) => {
                let (a, b) = (*a, *b);
                // C++ `evalForUpdate` wraps each operand's *evaluation* (via
                // `evalAttrs`) with its error context at the operand's own
                // position, evaluating the rightmost operand first. Thunk the
                // operands so eager errors are caught by `ForceAttrs`.
                let (pa, pb) = (self.expr_pos(a), self.expr_pos(b));
                self.compile_maybe_thunk(b, None);
                self.emit(Op::ForceAttrs(10), pb);
                self.compile_maybe_thunk(a, None);
                self.emit(Op::ForceAttrs(9), pa);
                self.emit(Op::Update, NO_POS);
                self.bump(-1);
            }
            Expr::OpConcatLists(pos, a, b) => {
                let (pos, a, b) = (*pos, *a, *b);
                self.compile_expr(a);
                self.emit(Op::ForceList(12), pos);
                self.compile_expr(b);
                self.emit(Op::ForceList(12), pos);
                self.emit(Op::ConcatLists, pos);
                self.bump(-1);
            }
            Expr::ConcatStrings {
                pos,
                force_string,
                es,
            } => {
                let (pos, force_string) = (*pos, *force_string);
                let es: Vec<(PosIdx, ExprId)> = es.clone();
                for (_, e) in &es {
                    self.compile_expr(*e);
                }
                self.prog.concat_descs.push(ConcatDesc {
                    force_string,
                    poss: es.iter().map(|(p, _)| *p).collect(),
                    pos,
                });
                let d = (self.prog.concat_descs.len() - 1) as u32;
                self.emit(Op::ConcatStrings(d), pos);
                self.bump(-(es.len() as i64) + 1);
            }
            Expr::CurPos(pos) => {
                self.emit(Op::CurPos, *pos);
                self.bump(1);
            }
        }
    }

    fn lambda_pos(&self, id: ExprId) -> PosIdx {
        match self.exprs.get(id) {
            Expr::Lambda(l) => l.pos,
            _ => NO_POS,
        }
    }

    /// Port of the per-node `Expr::getPos()` overrides (nixexpr.hh), used
    /// where C++ threads an expression's own position into a trace/force.
    fn expr_pos(&self, id: ExprId) -> PosIdx {
        match self.exprs.get(id) {
            Expr::Var { pos, .. } => *pos,
            Expr::Select { pos, .. } => *pos,
            Expr::OpHasAttr { e, .. } => self.expr_pos(*e),
            Expr::Attrs(a) => a.pos,
            Expr::List(elems) => elems.first().map(|e| self.expr_pos(*e)).unwrap_or(NO_POS),
            Expr::Lambda(l) => l.pos,
            Expr::Call { pos, .. } => *pos,
            Expr::If { pos, .. } => *pos,
            Expr::Assert { pos, .. } => *pos,
            Expr::OpNot(e) => self.expr_pos(*e),
            Expr::OpAnd(pos, ..)
            | Expr::OpOr(pos, ..)
            | Expr::OpImpl(pos, ..)
            | Expr::OpUpdate(pos, ..)
            | Expr::OpConcatLists(pos, ..) => *pos,
            Expr::ConcatStrings { pos, .. } => *pos,
            Expr::CurPos(pos) => *pos,
            _ => NO_POS,
        }
    }

    fn compile_bool_op(
        &mut self,
        pos: PosIdx,
        a: ExprId,
        b: ExprId,
        ctx_l: u32,
        ctx_r: u32,
        jump_on_true: bool,
        short_val: bool,
    ) {
        self.compile_expr(a);
        self.emit(Op::ForceBool(ctx_l), pos);
        let j = self.emit_jump(
            if jump_on_true {
                Op::JumpIfTrue
            } else {
                Op::JumpIfFalse
            },
            pos,
        );
        self.bump(-1);
        let h = self.height();
        self.compile_expr(b);
        self.emit(Op::ForceBool(ctx_r), pos);
        let jend = self.emit_jump(Op::Jump, NO_POS);
        self.patch_jump(j);
        self.set_height(h);
        self.push_const(
            ConstKey::Bool(short_val),
            || immortal::cell(Value::bool(short_val)),
            NO_POS,
        );
        self.patch_jump(jend);
    }

    fn compile_var_eval(&mut self, name: Symbol, pos: PosIdx) {
        match self.resolve(Key::Sym(name), None) {
            Resolved::Local(s) => {
                self.emit(Op::GetLocal(s), pos);
                self.bump(1);
                self.emit(Op::Force, pos);
            }
            Resolved::Upval(i) => {
                self.emit(Op::GetUpval(i), pos);
                self.bump(1);
                self.emit(Op::Force, pos);
            }
            Resolved::Global(c) => {
                self.emit(Op::Const(c), pos);
                self.bump(1);
                self.emit(Op::Force, pos);
            }
            Resolved::With => {
                self.emit(Op::ResolveWith(name.0), pos);
                self.bump(1);
                self.emit(Op::Force, pos);
            }
        }
    }

    /// `maybeThunk`: push a cell for `e` without evaluating it eagerly.
    /// Variables alias their binding cell (unless bound at slot >=
    /// `alias_floor` in the current state — a not-yet-filled recursive
    /// sibling), constants push their immortal cell, everything else
    /// becomes a thunk.
    fn compile_maybe_thunk(&mut self, id: ExprId, alias_floor: Option<u32>) {
        match self.exprs.get(id) {
            e @ (Expr::Int(_) | Expr::Float(_) | Expr::String(_) | Expr::Path(_)) => {
                self.push_literal(e, NO_POS)
            }
            Expr::List(elems) if elems.is_empty() => {
                let cell = self.empty_list_cell;
                self.push_const(ConstKey::EmptyList, || cell, NO_POS);
            }
            Expr::Var { pos, name } => {
                let (pos, name) = (*pos, *name);
                match self.resolve(Key::Sym(name), None) {
                    Resolved::Local(s) => {
                        let aliasable = alias_floor.is_none_or(|f| s < f);
                        if aliasable {
                            self.emit(Op::GetLocal(s), pos);
                            self.bump(1);
                        } else {
                            self.compile_thunk(id);
                        }
                    }
                    Resolved::Upval(i) => {
                        self.emit(Op::GetUpval(i), pos);
                        self.bump(1);
                    }
                    Resolved::Global(c) => {
                        self.emit(Op::Const(c), pos);
                        self.bump(1);
                    }
                    Resolved::With => self.compile_thunk(id),
                }
            }
            Expr::InheritFrom { pos, displ } => {
                let (pos, displ) = (*pos, *displ);
                let owner = self.from_owner.expect("InheritFrom outside inherit(...)");
                match self.resolve(Key::From(owner, displ), None) {
                    Resolved::Local(s) => self.emit(Op::GetLocal(s), pos),
                    Resolved::Upval(i) => self.emit(Op::GetUpval(i), pos),
                    _ => unreachable!(),
                }
                self.bump(1);
            }
            _ => self.compile_thunk(id),
        }
    }

    /// Compile `e` into its own chunk and emit MakeThunk.
    fn compile_thunk(&mut self, id: ExprId) {
        let wc = self.states.last().unwrap().with_total();
        self.push_state(wc, NO_POS, Symbol(0));
        self.compile_expr(id);
        self.emit(Op::Ret, NO_POS);
        let cid = self.pop_state();
        self.emit(Op::MakeThunk(cid), NO_POS);
        self.bump(1);
    }

    /// Emit a force-thunk that forwards to a single outer cell captured via
    /// `cap` (relative to the *current* frame).
    ///
    /// Used to *fill a recursive binding slot* (`let` / `rec { }`) whose value
    /// is a bare reference to an outer, mutable cell. Such a slot is filled by
    /// `StoreLocal`, which *copies the value* out of the referenced cell. A
    /// direct alias would therefore copy whatever the outer cell holds *at fill
    /// time* — which, when the binding scope is constructed while the outer cell
    /// is itself mid-force, is a transient `Blackhole`. That frozen blackhole is
    /// later re-forced and misreported as "infinite recursion". C++'s
    /// `maybeThunk` shares the outer `Value *` cell instead, so it never
    /// snapshots a transient state. jinx captures cells by value into fixed
    /// placeholder slots, so it cannot share; forwarding through a thunk defers
    /// the read (and force) of the outer cell until the slot is demanded, which
    /// is the observable equivalent.
    fn compile_forward_thunk(&mut self, cap: Cap, pos: PosIdx) {
        self.push_state(0, NO_POS, Symbol(0));
        // The single capture becomes upvalue 0 (with_captures == 0 here).
        self.st().upvals.push((Key::From(u32::MAX, u32::MAX), cap));
        self.emit(Op::GetUpval(0), pos);
        self.bump(1);
        self.emit(Op::Force, pos);
        self.emit(Op::Ret, NO_POS);
        let cid = self.pop_state();
        self.emit(Op::MakeThunk(cid), NO_POS);
        self.bump(1);
    }

    /// Fill a recursive binding slot with `id`. Like [`compile_maybe_thunk`],
    /// but tuned for the `StoreLocal` fill path: only *immutable* values
    /// (literals, the empty list, immortal globals) may be copied directly;
    /// every reference to another (mutable) binding cell is forwarded through a
    /// force-thunk (see [`compile_forward_thunk`]) so a transient blackhole is
    /// never snapshotted into the slot.
    fn compile_fill(&mut self, id: ExprId) {
        match self.exprs.get(id) {
            e @ (Expr::Int(_) | Expr::Float(_) | Expr::String(_) | Expr::Path(_)) => {
                self.push_literal(e, NO_POS)
            }
            Expr::List(elems) if elems.is_empty() => {
                let cell = self.empty_list_cell;
                self.push_const(ConstKey::EmptyList, || cell, NO_POS);
            }
            Expr::Var { pos, name } => {
                let (pos, name) = (*pos, *name);
                match self.resolve(Key::Sym(name), None) {
                    Resolved::Local(s) => self.compile_forward_thunk(Cap::Local(s), pos),
                    Resolved::Upval(i) => self.compile_forward_thunk(Cap::Upval(i), pos),
                    Resolved::Global(c) => {
                        self.emit(Op::Const(c), pos);
                        self.bump(1);
                    }
                    Resolved::With => self.compile_thunk(id),
                }
            }
            _ => self.compile_thunk(id),
        }
    }

    /// `inherit x;` inside a `let` / `rec { }`: fill the slot by forwarding to
    /// the *outer* binding of `x` (skipping the current group via `floor_slot`).
    /// Same rationale as [`compile_fill`] — never alias-copy a mutable cell.
    fn compile_inherited_fill(&mut self, sym: Symbol, pos: PosIdx, floor_slot: u32) {
        match self.resolve(Key::Sym(sym), Some(floor_slot)) {
            Resolved::Local(s) => self.compile_forward_thunk(Cap::Local(s), pos),
            Resolved::Upval(i) => self.compile_forward_thunk(Cap::Upval(i), pos),
            Resolved::Global(c) => {
                self.emit(Op::Const(c), pos);
                self.bump(1);
            }
            Resolved::With => self.compile_with_var_thunk(sym, pos),
        }
    }

    /// A thunk that resolves `name` through the with-chain lazily (the
    /// defeated-maybeThunk case for `inherit x` where x comes from `with`).
    fn compile_with_var_thunk(&mut self, name: Symbol, pos: PosIdx) {
        let wc = self.states.last().unwrap().with_total();
        self.push_state(wc, pos, Symbol(0));
        self.emit(Op::ResolveWith(name.0), pos);
        self.bump(1);
        self.emit(Op::Force, pos);
        self.emit(Op::Ret, NO_POS);
        let cid = self.pop_state();
        self.emit(Op::MakeThunk(cid), NO_POS);
        self.bump(1);
    }

    // ---------------- select ----------------

    fn compile_select(
        &mut self,
        e: ExprId,
        attrpath: &[AttrName],
        def: Option<ExprId>,
        pos: PosIdx,
        _id: ExprId,
    ) {
        let attrpath: Vec<AttrName> = attrpath.to_vec();
        self.compile_expr(e);
        if let Some(def) = def {
            let mut fails: Vec<usize> = Vec::new();
            for an in &attrpath {
                if let Some(de) = an.expr {
                    self.compile_expr(de);
                    self.bump(-1); // SelectDynOr pops the name
                    fails.push(self.emit_jump_seldyn(pos));
                } else {
                    fails.push(self.emit_jump_sel(an.symbol, pos));
                }
            }
            self.emit(Op::Force, pos);
            let jend = self.emit_jump(Op::Jump, NO_POS);
            let h = self.height() - 1;
            // Fail target: value popped.
            let target = self.here();
            for at in fails {
                self.patch_jump_to(at, target);
            }
            self.set_height(h);
            self.compile_expr(def);
            self.patch_jump(jend);
        } else {
            // Full selection-path text (C++ `showAttrSelectionPath`), shared by
            // every navigation op so that an error at any component reports the
            // whole path (e.g. `puppy."${key}"`).
            let text = self.attr_path_text(&attrpath);
            self.prog.texts.push(text);
            let t = (self.prog.texts.len() - 1) as u32;
            for an in &attrpath {
                if let Some(de) = an.expr {
                    // The dynamic name is forced at its own position (C++
                    // `getName` uses `name.expr->getPos()`).
                    let dpos = self.expr_pos(de);
                    self.compile_expr(de);
                    self.emit(Op::SelectDyn(t), dpos);
                    self.bump(-1);
                } else {
                    let cache = self.prog.select_caches.len() as u32;
                    self.prog
                        .select_caches
                        .push(std::cell::Cell::new(crate::chunk::SelectCache::default()));
                    self.emit(
                        Op::Select {
                            sym: an.symbol.0,
                            cache,
                        },
                        pos,
                    );
                }
            }
            self.emit(Op::SelectForce(t), pos);
        }
    }

    /// The `showAttrSelectionPath` string for an attr path: raw symbol names for
    /// static components, `"${<expr>}"` for dynamic ones (matching C++).
    fn attr_path_text(&self, attrpath: &[AttrName]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for (i, an) in attrpath.iter().enumerate() {
            if i > 0 {
                out.push(b'.');
            }
            if let Some(de) = an.expr {
                out.extend_from_slice(b"\"${");
                out.extend_from_slice(&jinx_syntax::show::show(self.exprs, self.symbols, de));
                out.extend_from_slice(b"}\"");
            } else {
                out.extend_from_slice(self.symbols.resolve(an.symbol));
            }
        }
        out
    }

    fn emit_jump_sel(&mut self, sym: Symbol, pos: PosIdx) -> usize {
        self.emit(
            Op::SelectOr {
                sym: sym.0,
                target: u32::MAX,
            },
            pos,
        );
        self.states.last().unwrap().chunk.ops.len() - 1
    }

    fn emit_jump_seldyn(&mut self, pos: PosIdx) -> usize {
        self.emit(Op::SelectDynOr { target: u32::MAX }, pos);
        self.states.last().unwrap().chunk.ops.len() - 1
    }

    fn patch_jump_to(&mut self, at: usize, target: u32) {
        let st = self.states.last_mut().unwrap();
        match &mut st.chunk.ops[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfTrue(t)
            | Op::SelectOr { target: t, .. }
            | Op::SelectDynOr { target: t } => *t = target,
            op => panic!("patching non-jump {op:?}"),
        }
    }

    // ---------------- lambdas ----------------

    /// Port of `ExprLambda::setName`: when a lambda is bound directly to an
    /// attr/let name, it (and its curried body chain) takes that name.
    fn register_lambda_name(&mut self, e: ExprId, sym: Symbol) {
        let mut cur = e;
        loop {
            match self.exprs.get(cur) {
                Expr::Lambda(l) => {
                    self.lambda_names.insert(cur.0, sym);
                    cur = l.body;
                }
                _ => break,
            }
        }
    }

    fn compile_lambda(&mut self, id: ExprId) -> u32 {
        let l = match self.exprs.get(id) {
            Expr::Lambda(l) => l,
            _ => unreachable!(),
        };
        let (pos, mut name, arg) = (l.pos, l.name, l.arg);
        if !name.is_set() {
            if let Some(s) = self.lambda_names.get(&id.0) {
                name = *s;
            }
        }
        let formals = l.formals.clone();
        let body = l.body;

        let wc = self.states.last().unwrap().with_total();
        self.push_state(wc, pos, name);

        // Locals: [@arg?] then formals in their (sorted) order.
        let mut slot = 0u32;
        if arg.is_set() {
            self.st().locals.push((Key::Sym(arg), slot));
            slot += 1;
        }
        let mut formal_specs: Vec<FormalSpec> = Vec::new();
        if let Some(f) = &formals {
            for fm in &f.formals {
                self.st().locals.push((Key::Sym(fm.name), slot));
                slot += 1;
            }
        }
        self.set_height(slot);

        // Compile defaults (maybeThunk in the lambda scope; aliases only to
        // earlier slots, mirroring boehm-zeroed env slots in C++).
        if let Some(f) = &formals {
            let base = if arg.is_set() { 1 } else { 0 };
            for (i, fm) in f.formals.iter().enumerate() {
                let default = fm.def.map(|d| {
                    let cur_slot = base + i as u32;
                    self.compile_default_chunk(d, cur_slot)
                });
                formal_specs.push(FormalSpec {
                    name: fm.name,
                    pos: fm.pos,
                    default,
                });
            }
        }

        self.st().chunk.lambda = Some(LambdaSpec {
            arg,
            formals: formals.as_ref().map(|f| FormalsSpec {
                formals: formal_specs,
                ellipsis: f.ellipsis,
            }),
        });

        self.compile_expr(body);
        self.emit(Op::Ret, NO_POS);
        self.pop_state()
    }

    /// Compile a formal's default expression into a chunk (created at call
    /// time by the VM, capturing lambda-frame slots).
    fn compile_default_chunk(&mut self, d: ExprId, _cur_slot: u32) -> u32 {
        let wc = self.states.last().unwrap().with_total();
        self.push_state(wc, NO_POS, Symbol(0));
        self.compile_expr(d);
        self.emit(Op::Ret, NO_POS);
        self.pop_state()
    }

    // ---------------- attrs / let ----------------

    /// Compile the binding scope of a `let` or a `rec { }`: pushes binding
    /// cells (and inherit-from thunks) as locals, registers them in the
    /// current state, and fills them. Returns the number of pushed slots.
    /// The caller must later emit `Slide(n)` and call `pop_binding_scope`.
    fn compile_bindings_scope(&mut self, attrs_id: ExprId, _is_let: bool) -> u32 {
        let a = self.exprs.attrs(attrs_id);
        let n_attrs = a.attrs.len() as u32;
        let n_from = a.inherit_from_exprs.len() as u32;
        let names: Vec<(Symbol, AttrDef)> = a.attrs.iter().map(|(s, d)| (*s, *d)).collect();
        let from_exprs: Vec<ExprId> = a.inherit_from_exprs.clone();
        let owner = attrs_id.0;

        let start = self.height();
        // Pass 1: placeholder cells for all bindings.
        for (i, (sym, _)) in names.iter().enumerate() {
            self.emit(Op::AllocCell, NO_POS);
            self.bump(1);
            self.st().locals.push((Key::Sym(*sym), start + i as u32));
        }
        // Inherit-from thunks (evaluated in the new scope, like C++ env2).
        for (i, fe) in from_exprs.iter().enumerate() {
            self.compile_maybe_thunk(*fe, None);
            let slot = self.height() - 1;
            self.st().locals.push((Key::From(owner, i as u32), slot));
        }
        // Pass 2: fill.
        self.fill_bindings(&names, owner, start);
        n_attrs + n_from
    }

    fn pop_binding_scope(&mut self, n: u32) {
        let st = self.states.last_mut().unwrap();
        let len = st.locals.len();
        st.locals.truncate(len - n as usize);
    }

    fn fill_bindings(&mut self, names: &[(Symbol, AttrDef)], owner: u32, start: u32) {
        for (sym, def) in names {
            if matches!(def.kind, AttrDefKind::Plain | AttrDefKind::InheritedFrom) {
                self.register_lambda_name(def.e, *sym);
            }
        }
        for (i, (sym, def)) in names.iter().enumerate() {
            let slot = start + i as u32;
            match def.kind {
                AttrDefKind::Inherited => {
                    // Evaluated in the *outer* env: skip the binding group.
                    self.compile_inherited_fill(*sym, def.pos, start);
                }
                AttrDefKind::InheritedFrom => {
                    let saved = self.from_owner.replace(owner);
                    self.compile_fill(def.e);
                    self.from_owner = saved;
                }
                AttrDefKind::Plain => {
                    self.compile_fill(def.e);
                }
            }
            self.emit(Op::StoreLocal(slot), def.pos);
            self.bump(-1);
        }
    }

    fn compile_attrs(&mut self, id: ExprId, _outer: Option<()>) {
        let a = self.exprs.attrs(id);
        let recursive = a.recursive;
        let pos = a.pos;
        let names: Vec<(Symbol, AttrDef)> = a.attrs.iter().map(|(s, d)| (*s, *d)).collect();
        let from_exprs: Vec<ExprId> = a.inherit_from_exprs.clone();
        let dynamics: Vec<DynamicAttrDef> = a.dynamic_attrs.clone();
        let owner = id.0;

        let desc = AttrsDesc {
            names: names.iter().map(|(s, d)| (*s, d.pos)).collect(),
            pos,
        };
        self.prog.attrs_descs.push(desc);
        let desc_id = (self.prog.attrs_descs.len() - 1) as u32;

        for (sym, def) in &names {
            if matches!(def.kind, AttrDefKind::Plain | AttrDefKind::InheritedFrom) {
                self.register_lambda_name(def.e, *sym);
            }
        }

        if recursive {
            let has_overrides = names.iter().any(|(s, _)| {
                self.symbols.resolve(*s) == b"__overrides"
            });
            let start = self.height();
            // Pass 1: placeholders.
            for (i, (sym, _)) in names.iter().enumerate() {
                self.emit(Op::AllocCell, NO_POS);
                self.bump(1);
                self.st().locals.push((Key::Sym(*sym), start + i as u32));
            }
            for (i, fe) in from_exprs.iter().enumerate() {
                self.compile_maybe_thunk(*fe, None);
                let slot = self.height() - 1;
                self.st().locals.push((Key::From(owner, i as u32), slot));
            }
            // Pass 2: fill. With __overrides, C++ always thunk-wraps
            // non-inherited attrs (no maybeThunk).
            for (i, (sym, def)) in names.iter().enumerate() {
                let slot = start + i as u32;
                match def.kind {
                    AttrDefKind::Inherited => self.compile_inherited_fill(*sym, def.pos, start),
                    AttrDefKind::InheritedFrom => {
                        let saved = self.from_owner.replace(owner);
                        if has_overrides {
                            self.compile_thunk(def.e);
                        } else {
                            self.compile_fill(def.e);
                        }
                        self.from_owner = saved;
                    }
                    AttrDefKind::Plain => {
                        if has_overrides {
                            self.compile_thunk(def.e);
                        } else {
                            self.compile_fill(def.e);
                        }
                    }
                }
                self.emit(Op::StoreLocal(slot), def.pos);
                self.bump(-1);
            }
            // Collect binding cells and build.
            for i in 0..names.len() {
                self.emit(Op::GetLocal(start + i as u32), NO_POS);
                self.bump(1);
            }
            self.emit(Op::MakeAttrs(desc_id), pos);
            self.bump(-(names.len() as i64) + 1);
            if has_overrides {
                let ov_idx = names
                    .iter()
                    .position(|(s, _)| self.symbols.resolve(*s) == b"__overrides")
                    .unwrap() as u32;
                self.prog.rec_descs.push(RecDesc {
                    attrs_desc: desc_id,
                    locals_start: start,
                    overrides_idx: ov_idx,
                    pos,
                });
                let rd = (self.prog.rec_descs.len() - 1) as u32;
                self.emit(Op::RecOverrides(rd), pos);
            }
            // Dynamic attrs (evaluated in the rec scope).
            for d in &dynamics {
                self.compile_expr(d.name_expr);
                self.compile_maybe_thunk(d.value_expr, None);
                self.emit(Op::DynAttr, d.pos);
                self.bump(-2);
            }
            let n = names.len() as u32 + from_exprs.len() as u32;
            if n > 0 {
                self.emit(Op::Slide(n), NO_POS);
                self.bump(-(n as i64));
            }
            self.pop_binding_scope(n);
        } else {
            let start_locals = self.states.last().unwrap().locals.len();
            // Inherit-from thunks in the enclosing scope.
            for (i, fe) in from_exprs.iter().enumerate() {
                self.compile_maybe_thunk(*fe, None);
                let slot = self.height() - 1;
                self.st().locals.push((Key::From(owner, i as u32), slot));
            }
            for (_sym, def) in &names {
                match def.kind {
                    AttrDefKind::InheritedFrom => {
                        let saved = self.from_owner.replace(owner);
                        self.compile_maybe_thunk(def.e, None);
                        self.from_owner = saved;
                    }
                    _ => self.compile_maybe_thunk(def.e, None),
                }
            }
            self.emit(Op::MakeAttrs(desc_id), pos);
            self.bump(-(names.len() as i64) + 1);
            for d in &dynamics {
                self.compile_expr(d.name_expr);
                self.compile_maybe_thunk(d.value_expr, None);
                self.emit(Op::DynAttr, d.pos);
                self.bump(-2);
            }
            let n_hidden = from_exprs.len() as u32;
            if n_hidden > 0 {
                self.emit(Op::Slide(n_hidden), NO_POS);
                self.bump(-(n_hidden as i64));
            }
            let st = self.states.last_mut().unwrap();
            st.locals.truncate(start_locals);
        }
    }
}
