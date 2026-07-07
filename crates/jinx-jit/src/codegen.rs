//! Chunk -> CLIF -> native-code compilation.
//!
//! The compiled function has ABI `extern "C" fn(*mut VM, u64 fi) -> u64` (see
//! `jinx_eval::jit`). It shares the interpreter's operand stack (`vm.stack`, a
//! `#[repr(C)]` `Stack`): cheap ops (constants, local/upvalue loads, pops,
//! jumps, branches, return, slide) are lowered inline against that stack;
//! everything else calls a `jinx_*` runtime helper that reproduces the
//! interpreter op exactly. One CLIF block is emitted per op index so jumps map
//! directly onto block edges.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, AbiParam, Block, InstBuilder, MemFlags, Value};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use jinx_eval::chunk::{Chunk, CodeRef, Op, Program};
use jinx_eval::jit::JitHook;
use jinx_eval::stack::{STACK_LEN_OFF, STACK_PTR_OFF};
use jinx_eval::value::Tag;

const I64: types::Type = types::I64;
const I32: types::Type = types::I32;

/// The Cranelift backend: owns the JIT module (code memory) for the process's
/// lifetime. Single-threaded, matching the evaluator.
pub struct Compiler {
    module: JITModule,
    ctx: cranelift_codegen::Context,
    fbc: cranelift_frontend::FunctionBuilderContext,
    /// Imported runtime-helper functions, by name.
    helpers: HashMap<&'static str, FuncId>,
}

impl Compiler {
    pub fn new() -> Self {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").unwrap();
        flags.set("use_colocated_libcalls", "false").unwrap();
        flags.set("is_pic", "false").unwrap();
        let isa = cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
            .expect("host ISA")
            .finish(settings::Flags::new(flags))
            .expect("ISA finish");

        let mut builder =
            JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        for (name, addr) in crate::rt::symbols() {
            builder.symbol(name, addr);
        }
        let mut module = JITModule::new(builder);
        let ctx = module.make_context();

        // Declare every runtime helper as an imported function (all return I64).
        let mut helpers = HashMap::new();
        for (name, params) in crate::rt::signatures() {
            let mut sig = module.make_signature();
            for t in params {
                sig.params.push(AbiParam::new(t));
            }
            sig.returns.push(AbiParam::new(I64));
            let id = module
                .declare_function(name, Linkage::Import, &sig)
                .expect("declare helper");
            helpers.insert(name, id);
        }

        Compiler {
            module,
            ctx,
            fbc: cranelift_frontend::FunctionBuilderContext::new(),
            helpers,
        }
    }
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

impl JitHook for Compiler {
    fn compile(&mut self, code: &'static CodeRef) -> Option<*const ()> {
        let chunk: &Chunk = code.chunk();
        let prog: &Program = code.prog();
        self.compile_chunk(chunk, prog)
    }
}

/// Which arithmetic a `Call` (or `ConcatStrings`) can be specialized to.
#[derive(Clone, Copy, PartialEq)]
enum Arith {
    Add,
    Sub,
    Lt,
}

/// Results of the pre-pass: which ops to specialize / elide. Every
/// specialization is *guarded at runtime* (function-pointer identity for
/// arith calls, `Int` tag checks for operands), so an imprecise guess only
/// loses the fast path — it can never miscompile.
#[derive(Default)]
struct Analysis {
    /// `Call` ip -> (arith kind, the primop const cell pointer to guard on).
    arith_call: HashMap<usize, (Arith, i64)>,
    /// `ConcatStrings` ips that are 2-part numeric `+` (guarded int add).
    concat_add: HashSet<usize>,
    /// `Force` ips whose operand is a constant (already WHNF) — no-op, elide.
    elide_force: HashSet<usize>,
}

/// Recognize a constant cell as an inlinable binary arithmetic primop.
fn const_arith(prog: &Program, ci: u32, arity: u32) -> Option<Arith> {
    let v = jinx_eval::vm::val(prog.consts[ci as usize]);
    if v.tag() != Tag::PrimOp {
        return None;
    }
    let def = jinx_eval::vm::primop_of(&v);
    if def.arity as u32 != arity {
        return None;
    }
    match def.name {
        "__add" => Some(Arith::Add),
        "__sub" => Some(Arith::Sub),
        "__lessThan" => Some(Arith::Lt),
        _ => None,
    }
}

/// Stack effect `(pops, pushes)` and, when it pushes exactly one statically
/// known constant, its const index.
fn stack_effect(op: Op, prog: &Program) -> (usize, usize, Option<u32>) {
    match op {
        Op::Const(i) => (0, 1, Some(i)),
        Op::GetLocal(_)
        | Op::GetUpval(_)
        | Op::ResolveWith(_)
        | Op::AllocCell
        | Op::CurPos
        | Op::MakeThunk(_)
        | Op::MakeClosure(_) => (0, 1, None),
        Op::Force
        | Op::ForceBool(_)
        | Op::ForceAttrs(_)
        | Op::ForceList(_)
        | Op::RecOverrides(_)
        | Op::Select(_)
        | Op::SelectForce(_)
        | Op::PopWith => (0, 0, None),
        Op::Pop | Op::StoreLocal(_) | Op::PushWith | Op::SelectDyn(_) => (1, 0, None),
        Op::Not => (1, 1, None),
        Op::Eq | Op::NEq | Op::Update | Op::ConcatLists | Op::AssertEq(_) => (2, 1, None),
        Op::DynAttr => (2, 0, None),
        Op::MakeList(n) => (n as usize, 1, None),
        Op::MakeAttrs(d) => (prog.attrs_descs[d as usize].names.len(), 1, None),
        Op::ConcatStrings(d) => (prog.concat_descs[d as usize].poss.len(), 1, None),
        Op::HasAttrPath(d) => {
            let ndyn = prog.haspath_descs[d as usize]
                .comps
                .iter()
                .filter(|c| c.is_none())
                .count();
            (1 + ndyn, 1, None)
        }
        Op::Call(k) => (k as usize + 1, 1, None),
        Op::Slide(n) => (n as usize + 1, 1, None),
        // Block terminators: never reached before block end in the simulation.
        Op::Jump(_)
        | Op::JumpIfFalse(_)
        | Op::JumpIfTrue(_)
        | Op::Ret
        | Op::AssertFail(_)
        | Op::SelectOr { .. }
        | Op::SelectDynOr { .. } => (0, 0, None),
    }
}

fn analyze(chunk: &Chunk, prog: &Program) -> Analysis {
    let ops = &chunk.ops;
    let n = ops.len();
    let mut leader = vec![false; n];
    leader[0] = true;
    let mark = |t: usize, l: &mut Vec<bool>| {
        if t < n {
            l[t] = true;
        }
    };
    for (ip, op) in ops.iter().enumerate() {
        match *op {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                mark(t as usize, &mut leader);
                mark(ip + 1, &mut leader);
            }
            Op::SelectOr { target, .. } | Op::SelectDynOr { target } => {
                mark(target as usize, &mut leader);
                mark(ip + 1, &mut leader);
            }
            Op::Ret | Op::AssertFail(_) => mark(ip + 1, &mut leader),
            _ => {}
        }
    }

    let mut a = Analysis::default();
    let mut sym: Vec<Option<u32>> = Vec::new();
    let mut desync = false;
    for ip in 0..n {
        if leader[ip] {
            sym.clear();
            desync = false;
        }
        match ops[ip] {
            Op::Force if !desync => {
                if let Some(Some(ci)) = sym.last().copied() {
                    let v = jinx_eval::vm::val(prog.consts[ci as usize]);
                    if !matches!(v.tag(), Tag::Thunk | Tag::Blackhole | Tag::Failed) {
                        a.elide_force.insert(ip);
                    }
                }
            }
            Op::Call(k) if !desync && sym.len() > k as usize => {
                if let Some(ci) = sym[sym.len() - 1 - k as usize] {
                    if let Some(kind) = const_arith(prog, ci, k) {
                        a.arith_call
                            .insert(ip, (kind, prog.consts[ci as usize].as_ptr() as i64));
                    }
                }
            }
            Op::ConcatStrings(d) => {
                let desc = &prog.concat_descs[d as usize];
                if desc.poss.len() == 2 && !desc.force_string {
                    a.concat_add.insert(ip);
                }
            }
            _ => {}
        }
        // Apply the stack effect to the symbolic stack.
        let (pop, push, kcconst) = stack_effect(ops[ip], prog);
        if pop > sym.len() {
            desync = true;
            sym.clear();
        } else {
            sym.truncate(sym.len() - pop);
        }
        if push == 1 && kcconst.is_some() {
            sym.push(kcconst);
        } else {
            for _ in 0..push {
                sym.push(None);
            }
        }
    }
    a
}

/// Per-function translation state.
struct Tr<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    refs: &'a HashMap<&'static str, cranelift_codegen::ir::FuncRef>,
    flags: MemFlags,
    sa: Variable,   // address of vm.stack (Stack, repr(C))
    base: Variable, // locals_base
    upv: Variable,  // pointer to upvals[0]
    vm: Value,
    fi: Value,
    an: &'a Analysis,
}

impl Tr<'_, '_> {
    #[inline]
    fn sa(&mut self) -> Value {
        self.b.use_var(self.sa)
    }
    fn load_len(&mut self) -> Value {
        let sa = self.sa();
        self.b.ins().load(I64, self.flags, sa, STACK_LEN_OFF)
    }
    fn store_len(&mut self, v: Value) {
        let sa = self.sa();
        self.b.ins().store(self.flags, v, sa, STACK_LEN_OFF);
    }
    fn load_ptr(&mut self) -> Value {
        let sa = self.sa();
        self.b.ins().load(I64, self.flags, sa, STACK_PTR_OFF)
    }
    /// Address of slot `idx` (an I64 value) in the operand buffer.
    fn slot_addr(&mut self, idx: Value) -> Value {
        let ptr = self.load_ptr();
        let off = self.b.ins().ishl_imm(idx, 3);
        self.b.ins().iadd(ptr, off)
    }
    fn push(&mut self, cell: Value) {
        let len = self.load_len();
        let ea = self.slot_addr(len);
        self.b.ins().store(self.flags, cell, ea, 0);
        let l1 = self.b.ins().iadd_imm(len, 1);
        self.store_len(l1);
    }
    fn pop(&mut self) -> Value {
        let len = self.load_len();
        let nlen = self.b.ins().iadd_imm(len, -1);
        self.store_len(nlen);
        let ea = self.slot_addr(nlen);
        self.b.ins().load(I64, self.flags, ea, 0)
    }
    /// Load the tag byte of a value cell.
    fn tag_of(&mut self, cell: Value) -> Value {
        self.b.ins().load(types::I8, self.flags, cell, 0)
    }
    /// Load a cell's second word (`w1`): the immediate i64 payload for ints.
    fn w1(&mut self, cell: Value) -> Value {
        self.b.ins().load(I64, self.flags, cell, 8)
    }
    /// Address of the operand slot at `len + delta`.
    fn slot_off(&mut self, len: Value, delta: i64) -> Value {
        let idx = self.b.ins().iadd_imm(len, delta);
        self.slot_addr(idx)
    }
    fn is_int(&mut self, cell: Value) -> Value {
        let t = self.tag_of(cell);
        self.b.ins().icmp_imm(IntCC::Equal, t, Tag::Int as i64)
    }
    fn iconst32(&mut self, v: u32) -> Value {
        self.b.ins().iconst(I32, v as i64)
    }
    fn call(&mut self, name: &str, args: &[Value]) -> Value {
        let fref = self.refs[name];
        let inst = self.b.ins().call(fref, args);
        self.b.inst_results(inst)[0]
    }
    /// Branch to `err_block` (passing the status) if `st`'s error bit is set,
    /// else fall through to `next`.
    fn err_check(&mut self, st: Value, err_block: Block, next: Block) {
        let is_err = self.b.ins().icmp_imm(IntCC::SignedLessThan, st, 0);
        self.b.ins().brif(is_err, err_block, &[st.into()], next, &[]);
    }
}

impl Compiler {
    fn compile_chunk(&mut self, chunk: &Chunk, prog: &Program) -> Option<*const ()> {
        let ops = &chunk.ops;
        let n = ops.len();
        if n == 0 {
            return None;
        }
        let has_getlocal = ops.iter().any(|o| matches!(o, Op::GetLocal(_)));
        let has_getupval = ops.iter().any(|o| matches!(o, Op::GetUpval(_)));
        let analysis = analyze(chunk, prog);

        self.module.clear_context(&mut self.ctx);
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(I64)); // vm
        sig.params.push(AbiParam::new(I64)); // fi
        sig.returns.push(AbiParam::new(I64));
        self.ctx.func.signature = sig.clone();

        let mut builder = FunctionBuilder::new(&mut self.ctx.func, &mut self.fbc);

        // Resolve helper FuncRefs for this function.
        let mut refs = HashMap::new();
        for (name, id) in &self.helpers {
            refs.insert(*name, self.module.declare_func_in_func(*id, builder.func));
        }

        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        let op_blocks: Vec<Block> = (0..n).map(|_| builder.create_block()).collect();
        let err_block = builder.create_block();
        builder.append_block_param(err_block, I64);

        let sa = Variable::from_u32(0);
        let base = Variable::from_u32(1);
        let upv = Variable::from_u32(2);
        builder.declare_var(sa, I64);
        builder.declare_var(base, I64);
        builder.declare_var(upv, I64);

        // --- entry block: frame setup ---
        builder.switch_to_block(entry);
        let vm = builder.block_params(entry)[0];
        let fi = builder.block_params(entry)[1];
        {
            let mh = builder.ins().iconst(I64, chunk.max_height as i64);
            let f = refs["jinx_setup"];
            let c = builder.ins().call(f, &[vm, fi, mh]);
            let saval = builder.inst_results(c)[0];
            builder.def_var(sa, saval);
        }
        if has_getlocal {
            let f = refs["jinx_base"];
            let c = builder.ins().call(f, &[vm, fi]);
            let v = builder.inst_results(c)[0];
            builder.def_var(base, v);
        }
        if has_getupval {
            let f = refs["jinx_upvals"];
            let c = builder.ins().call(f, &[vm, fi]);
            let v = builder.inst_results(c)[0];
            builder.def_var(upv, v);
        }
        builder.ins().jump(op_blocks[0], &[]);

        let flags = MemFlags::trusted();
        let mut tr = Tr {
            b: &mut builder,
            refs: &refs,
            flags,
            sa,
            base,
            upv,
            vm,
            fi,
            an: &analysis,
        };

        for ip in 0..n {
            tr.b.switch_to_block(op_blocks[ip]);
            let terminated = translate_op(&mut tr, ops[ip], ip, chunk, prog, &op_blocks, err_block);
            if !terminated {
                if ip + 1 >= n {
                    // A non-terminating op as the last instruction is malformed
                    // (chunks end in Ret); bail to the interpreter.
                    return None;
                }
                tr.b.ins().jump(op_blocks[ip + 1], &[]);
            }
        }

        // --- error return block ---
        builder.switch_to_block(err_block);
        let e = builder.block_params(err_block)[0];
        builder.ins().return_(&[e]);

        builder.seal_all_blocks();
        builder.finalize();

        let func_id = self.module.declare_anonymous_function(&sig).ok()?;
        self.module.define_function(func_id, &mut self.ctx).ok()?;
        self.module.clear_context(&mut self.ctx);
        self.module.finalize_definitions().ok()?;
        let ptr = self.module.get_finalized_function(func_id);
        Some(ptr as *const ())
    }
}

/// Translate one op into the current block. Returns `true` if the op emitted
/// its own block terminator (so the driver must not add a fallthrough jump).
fn translate_op(
    tr: &mut Tr,
    op: Op,
    ip: usize,
    chunk: &Chunk,
    prog: &Program,
    blocks: &[Block],
    err_block: Block,
) -> bool {
    let pos = chunk.pos_at(ip).0;
    let next = || blocks[ip + 1];

    // Helper: erroring call that branches to err_block on failure, else next.
    macro_rules! erroring {
        ($name:literal, $args:expr) => {{
            let st = tr.call($name, $args);
            let nb = next();
            tr.err_check(st, err_block, nb);
            true
        }};
    }

    match op {
        // ---- inline: constants & loads ----
        Op::Const(i) => {
            let vref = prog.consts[i as usize].as_ptr() as i64;
            let cell = tr.b.ins().iconst(I64, vref);
            tr.push(cell);
            false
        }
        Op::GetLocal(s) => {
            let base = tr.b.use_var(tr.base);
            let idx = tr.b.ins().iadd_imm(base, s as i64);
            let ea = tr.slot_addr(idx);
            let cell = tr.b.ins().load(I64, tr.flags, ea, 0);
            tr.push(cell);
            false
        }
        Op::GetUpval(i) => {
            let upv = tr.b.use_var(tr.upv);
            let cell = tr.b.ins().load(I64, tr.flags, upv, (i as i32) * 8);
            tr.push(cell);
            false
        }
        Op::Pop => {
            let len = tr.load_len();
            let nlen = tr.b.ins().iadd_imm(len, -1);
            tr.store_len(nlen);
            false
        }
        Op::Slide(k) => {
            // top = pop; truncate(len-1-k); push(top)
            let len = tr.load_len();
            let top_idx = tr.b.ins().iadd_imm(len, -1);
            let ea = tr.slot_addr(top_idx);
            let top = tr.b.ins().load(I64, tr.flags, ea, 0);
            let dst = tr.b.ins().iadd_imm(len, -(1 + k as i64));
            let dea = tr.slot_addr(dst);
            tr.b.ins().store(tr.flags, top, dea, 0);
            let nlen = tr.b.ins().iadd_imm(dst, 1);
            tr.store_len(nlen);
            false
        }

        // ---- inline: control flow ----
        Op::Jump(t) => {
            tr.b.ins().jump(blocks[t as usize], &[]);
            true
        }
        Op::JumpIfFalse(t) => {
            let cell = tr.pop();
            let tag = tr.tag_of(cell);
            let is = tr.b.ins().icmp_imm(IntCC::Equal, tag, Tag::False as i64);
            tr.b.ins().brif(is, blocks[t as usize], &[], next(), &[]);
            true
        }
        Op::JumpIfTrue(t) => {
            let cell = tr.pop();
            let tag = tr.tag_of(cell);
            let is = tr.b.ins().icmp_imm(IntCC::Equal, tag, Tag::True as i64);
            tr.b.ins().brif(is, blocks[t as usize], &[], next(), &[]);
            true
        }
        Op::Ret => {
            let cell = tr.pop();
            tr.b.ins().return_(&[cell]);
            true
        }

        // ---- helper: force family ----
        Op::Force => {
            if tr.an.elide_force.contains(&ip) {
                // Operand is a constant (already WHNF): forcing is a no-op.
                return false;
            }
            let p = tr.iconst32(pos);
            erroring!("jinx_force_top", &[tr.vm, p])
        }
        Op::ForceBool(c) => {
            let p = tr.iconst32(pos);
            let cc = tr.iconst32(c);
            erroring!("jinx_force_bool_top", &[tr.vm, p, cc])
        }
        Op::ForceAttrs(c) => {
            let p = tr.iconst32(pos);
            let cc = tr.iconst32(c);
            erroring!("jinx_force_attrs_top", &[tr.vm, p, cc])
        }
        Op::ForceList(c) => {
            let p = tr.iconst32(pos);
            let cc = tr.iconst32(c);
            erroring!("jinx_force_list_top", &[tr.vm, p, cc])
        }

        // ---- helper: variables / allocation ----
        Op::ResolveWith(sym) => {
            let s = tr.iconst32(sym);
            let p = tr.iconst32(pos);
            erroring!("jinx_resolve_with", &[tr.vm, tr.fi, s, p])
        }
        Op::AllocCell => erroring!("jinx_alloc_cell", &[tr.vm]),
        Op::StoreLocal(s) => {
            let sl = tr.iconst32(s);
            erroring!("jinx_store_local", &[tr.vm, tr.fi, sl])
        }
        Op::MakeThunk(cid) => {
            let c = tr.iconst32(cid);
            let z = tr.iconst32(0);
            erroring!("jinx_make_thunk", &[tr.vm, tr.fi, c, z])
        }
        Op::MakeClosure(cid) => {
            let c = tr.iconst32(cid);
            let o = tr.iconst32(1);
            erroring!("jinx_make_thunk", &[tr.vm, tr.fi, c, o])
        }
        Op::MakeList(k) => {
            let kk = tr.iconst32(k);
            erroring!("jinx_make_list", &[tr.vm, kk])
        }
        Op::MakeAttrs(d) => {
            let dd = tr.iconst32(d);
            erroring!("jinx_make_attrs", &[tr.vm, tr.fi, dd])
        }
        Op::DynAttr => {
            let p = tr.iconst32(pos);
            erroring!("jinx_dyn_attr", &[tr.vm, p])
        }
        Op::RecOverrides(rd) => {
            let r = tr.iconst32(rd);
            let p = tr.iconst32(pos);
            erroring!("jinx_rec_overrides", &[tr.vm, tr.fi, r, p])
        }

        // ---- helper: operators ----
        Op::Eq => {
            let p = tr.iconst32(pos);
            let z = tr.iconst32(0);
            erroring!("jinx_eq", &[tr.vm, p, z])
        }
        Op::NEq => {
            let p = tr.iconst32(pos);
            let o = tr.iconst32(1);
            erroring!("jinx_eq", &[tr.vm, p, o])
        }
        Op::Not => erroring!("jinx_not", &[tr.vm]),
        Op::Update => erroring!("jinx_update", &[tr.vm]),
        Op::ConcatLists => erroring!("jinx_concat_lists", &[tr.vm]),
        Op::ConcatStrings(d) => {
            if tr.an.concat_add.contains(&ip) {
                emit_concat_add(tr, ip, d, chunk, blocks, err_block);
                return true;
            }
            let dd = tr.iconst32(d);
            erroring!("jinx_concat_strings", &[tr.vm, tr.fi, dd])
        }

        // ---- helper: selection ----
        Op::Select(sym) => {
            let s = tr.iconst32(sym);
            let p = tr.iconst32(pos);
            erroring!("jinx_select", &[tr.vm, s, p])
        }
        Op::SelectForce(t) => {
            let tt = tr.iconst32(t);
            erroring!("jinx_select_force", &[tr.vm, tr.fi, tt])
        }
        Op::SelectOr { sym, target } => {
            let s = tr.iconst32(sym);
            let p = tr.iconst32(pos);
            let st = tr.call("jinx_select_or", &[tr.vm, s, p]);
            emit_or(tr, st, target as usize, ip, blocks, err_block);
            true
        }
        Op::SelectDyn(t) => {
            let tt = tr.iconst32(t);
            let p = tr.iconst32(pos);
            erroring!("jinx_select_dyn", &[tr.vm, tr.fi, tt, p])
        }
        Op::SelectDynOr { target } => {
            let p = tr.iconst32(pos);
            let st = tr.call("jinx_select_dyn_or", &[tr.vm, p]);
            emit_or(tr, st, target as usize, ip, blocks, err_block);
            true
        }
        Op::HasAttrPath(d) => {
            let dd = tr.iconst32(d);
            let p = tr.iconst32(pos);
            erroring!("jinx_has_attr_path", &[tr.vm, tr.fi, dd, p])
        }

        // ---- helper: call & misc ----
        Op::Call(k) => {
            if let Some(&(kind, cp)) = tr.an.arith_call.get(&ip) {
                emit_arith_call(tr, kind, cp, ip, pos, blocks, err_block);
                return true;
            }
            let kk = tr.iconst32(k);
            let p = tr.iconst32(pos);
            erroring!("jinx_call", &[tr.vm, kk, p])
        }
        Op::CurPos => {
            let p = tr.iconst32(pos);
            erroring!("jinx_cur_pos", &[tr.vm, p])
        }
        Op::AssertFail(t) => {
            // Always an error: return the status directly.
            let tt = tr.iconst32(t);
            let p = tr.iconst32(pos);
            let st = tr.call("jinx_assert_fail", &[tr.vm, tr.fi, tt, p]);
            tr.b.ins().return_(&[st]);
            true
        }
        Op::AssertEq(t) => {
            // 0 = equal (fall through to AssertFail), else error.
            let tt = tr.iconst32(t);
            let p = tr.iconst32(pos);
            erroring!("jinx_assert_eq", &[tr.vm, tr.fi, tt, p])
        }
        Op::PushWith => erroring!("jinx_push_with", &[tr.vm, tr.fi]),
        Op::PopWith => erroring!("jinx_pop_with", &[tr.vm, tr.fi]),
    }
}

/// Emit the branch sequence for an `...Or` helper whose result is 0 (fell
/// through to the next op) or 1 (take the `or`-branch at `target`), with the
/// error bit escaping to `err_block`.
fn emit_or(
    tr: &mut Tr,
    st: Value,
    target: usize,
    ip: usize,
    blocks: &[Block],
    err_block: Block,
) {
    let cont = tr.b.create_block();
    let is_err = tr.b.ins().icmp_imm(IntCC::SignedLessThan, st, 0);
    tr.b.ins().brif(is_err, err_block, &[st.into()], cont, &[]);
    tr.b.switch_to_block(cont);
    // st == 1 -> take the or-branch; st == 0 -> fall through.
    tr.b
        .ins()
        .brif(st, blocks[target], &[], blocks[ip + 1], &[]);
}

/// Signed-overflow predicate for `x <op> y == r`.
fn overflow(tr: &mut Tr, kind: Arith, x: Value, y: Value, r: Value) -> Value {
    // add: (x^r)&(y^r) < 0 ; sub: (x^y)&(x^r) < 0.
    let (p, q) = match kind {
        Arith::Add => (tr.b.ins().bxor(x, r), tr.b.ins().bxor(y, r)),
        Arith::Sub => (tr.b.ins().bxor(x, y), tr.b.ins().bxor(x, r)),
        Arith::Lt => unreachable!(),
    };
    let a = tr.b.ins().band(p, q);
    tr.b.ins().icmp_imm(IntCC::SignedLessThan, a, 0)
}

/// Finish an inline arithmetic op: pop `pops` operands, store `result` at the
/// new top, and continue to `next`.
fn finish_arith(tr: &mut Tr, len: Value, result: Value, pops: i64, next: Block) {
    let dst = tr.slot_off(len, -pops);
    tr.b.ins().store(tr.flags, result, dst, 0);
    let nlen = tr.b.ins().iadd_imm(len, -(pops - 1));
    tr.store_len(nlen);
    tr.b.ins().jump(next, &[]);
}

/// Specialize a binary arithmetic primop `Call(2)`: if the function is the
/// expected primop cell and both operands are already `Int`, do the arithmetic
/// inline; otherwise fall back to the generic `jinx_call`.
fn emit_arith_call(
    tr: &mut Tr,
    kind: Arith,
    const_ptr: i64,
    ip: usize,
    pos: u32,
    blocks: &[Block],
    err_block: Block,
) {
    let next = blocks[ip + 1];
    let len = tr.load_len();
    // Guard: the function operand (at len-3) is exactly the expected primop.
    let ea_fun = tr.slot_off(len, -3);
    let fun = tr.b.ins().load(I64, tr.flags, ea_fun, 0);
    let cpv = tr.b.ins().iconst(I64, const_ptr);
    let g = tr.b.ins().icmp(IntCC::Equal, fun, cpv);
    let fast = tr.b.create_block();
    let slow = tr.b.create_block();
    tr.b.ins().brif(g, fast, &[], slow, &[]);

    // fast: both operands already Int?
    tr.b.switch_to_block(fast);
    let ea0 = tr.slot_off(len, -2);
    let a0 = tr.b.ins().load(I64, tr.flags, ea0, 0);
    let ea1 = tr.slot_off(len, -1);
    let a1 = tr.b.ins().load(I64, tr.flags, ea1, 0);
    let i0 = tr.is_int(a0);
    let i1 = tr.is_int(a1);
    let both = tr.b.ins().band(i0, i1);
    let intb = tr.b.create_block();
    tr.b.ins().brif(both, intb, &[], slow, &[]);

    // intb: perform the arithmetic.
    tr.b.switch_to_block(intb);
    let x = tr.w1(a0);
    let y = tr.w1(a1);
    match kind {
        Arith::Add | Arith::Sub => {
            let r = if kind == Arith::Add {
                tr.b.ins().iadd(x, y)
            } else {
                tr.b.ins().isub(x, y)
            };
            let ov = overflow(tr, kind, x, y, r);
            let done = tr.b.create_block();
            tr.b.ins().brif(ov, slow, &[], done, &[]);
            tr.b.switch_to_block(done);
            let result = tr.call("jinx_alloc_int", &[tr.vm, r]);
            finish_arith(tr, len, result, 3, next);
        }
        Arith::Lt => {
            let rb = tr.b.ins().icmp(IntCC::SignedLessThan, x, y);
            let rbi = tr.b.ins().uextend(I32, rb);
            let result = tr.call("jinx_alloc_bool", &[tr.vm, rbi]);
            finish_arith(tr, len, result, 3, next);
        }
    }

    // slow: the generic call.
    tr.b.switch_to_block(slow);
    let kk = tr.iconst32(2);
    let p = tr.iconst32(pos);
    let st = tr.call("jinx_call", &[tr.vm, kk, p]);
    tr.err_check(st, err_block, next);
}

/// Specialize a 2-part numeric `+` (`ConcatStrings`): if both operands are
/// already `Int`, add inline (checked); otherwise fall back to the generic
/// `jinx_concat_strings` helper.
fn emit_concat_add(
    tr: &mut Tr,
    ip: usize,
    d: u32,
    _chunk: &Chunk,
    blocks: &[Block],
    err_block: Block,
) {
    let next = blocks[ip + 1];
    let len = tr.load_len();
    let ea0 = tr.slot_off(len, -2);
    let a0 = tr.b.ins().load(I64, tr.flags, ea0, 0);
    let ea1 = tr.slot_off(len, -1);
    let a1 = tr.b.ins().load(I64, tr.flags, ea1, 0);
    let i0 = tr.is_int(a0);
    let i1 = tr.is_int(a1);
    let both = tr.b.ins().band(i0, i1);
    let intb = tr.b.create_block();
    let slow = tr.b.create_block();
    tr.b.ins().brif(both, intb, &[], slow, &[]);

    tr.b.switch_to_block(intb);
    let x = tr.w1(a0);
    let y = tr.w1(a1);
    let r = tr.b.ins().iadd(x, y);
    let ov = overflow(tr, Arith::Add, x, y, r);
    let done = tr.b.create_block();
    tr.b.ins().brif(ov, slow, &[], done, &[]);
    tr.b.switch_to_block(done);
    let result = tr.call("jinx_alloc_int", &[tr.vm, r]);
    finish_arith(tr, len, result, 2, next);

    tr.b.switch_to_block(slow);
    let dd = tr.iconst32(d);
    let st = tr.call("jinx_concat_strings", &[tr.vm, tr.fi, dd]);
    tr.err_check(st, err_block, next);
}
