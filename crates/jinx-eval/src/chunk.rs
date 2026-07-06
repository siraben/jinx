//! Compiled code: chunks of ops with side tables, grouped into leaked
//! (immortal) `Program`s. Thunk/closure data objects store a pointer to a
//! `CodeRef` (program + chunk index) as their code word.
//!
//! The "bytecode" is a `Vec<Op>` of a small fixed-size enum with u32
//! operands (rather than a packed byte stream); the JIT milestone consumes
//! `Chunk` directly, so the encoding is an internal detail.

use jinx_syntax::pos::PosIdx;
use jinx_syntax::symbol::Symbol;

use crate::value::VRef;

/// How a child chunk's upvalue is materialized by its parent at
/// thunk/closure-creation time.
#[derive(Clone, Copy, Debug)]
pub enum Cap {
    /// Parent frame local (absolute operand-stack slot relative to
    /// `locals_base`).
    Local(u32),
    /// Parent upvalue index (into the parent's full upval array, including
    /// its with-chain prefix).
    Upval(u32),
}

/// Error-context strings attached by `ForceBool` / `ForceAttrs` /
/// `ForceList` ops (ported verbatim from eval.cc).
pub const CTX_STRINGS: &[&str] = &[
    "while evaluating a branch condition",                        // 0
    "in the condition of the assert statement",                   // 1
    "in the argument of the not operator",                        // 2
    "in the left operand of the AND (&&) operator",               // 3
    "in the right operand of the AND (&&) operator",              // 4
    "in the left operand of the OR (||) operator",                // 5
    "in the right operand of the OR (||) operator",               // 6
    "in the left operand of the IMPL (->) operator",              // 7
    "in the right operand of the IMPL (->) operator",             // 8
    "in the left operand of the update (//) operator",            // 9
    "in the right operand of the update (//) operator",           // 10
    "while selecting an attribute",                               // 11
    "while evaluating one of the elements to concatenate",        // 12
];

#[derive(Clone, Copy, Debug)]
pub enum Op {
    /// Push constant cell `consts[i]`.
    Const(u32),
    /// Push the cell at operand-stack slot `locals_base + i` (no force).
    GetLocal(u32),
    /// Push upvalue cell `i` of the current frame.
    GetUpval(u32),
    /// Look `sym` up in the runtime with-chain; push the attr cell.
    ResolveWith(u32),
    /// Force the top-of-stack cell in place.
    Force,
    /// Force TOS; type-check Boolean with context string `CTX_STRINGS[i]`.
    ForceBool(u32),
    /// Force TOS; type-check set with context string `CTX_STRINGS[i]`.
    ForceAttrs(u32),
    /// Force TOS; type-check list with context string `CTX_STRINGS[i]`.
    ForceList(u32),
    Pop,
    /// Push a fresh placeholder cell (Blackhole) for a recursive binding.
    AllocCell,
    /// Pop a cell; copy its value into the cell at slot `locals_base + i`.
    StoreLocal(u32),
    /// Create a thunk over `chunks[i]`, capturing per its spec; push it.
    MakeThunk(u32),
    /// Same but a closure (lambda value).
    MakeClosure(u32),
    /// Pop `n` cells; push a list of them.
    MakeList(u32),
    /// Pop `attrs_descs[i].names.len()` cells; push bindings (attrset).
    MakeAttrs(u32),
    /// Stack: [attrs, name, value] -> [attrs']; dynamic attr insertion.
    DynAttr,
    /// `__overrides` handling for rec sets: operand = rec_descs index.
    RecOverrides(u32),
    Jump(u32),
    /// Pop a (known-Boolean) cell; jump if false.
    JumpIfFalse(u32),
    /// Pop a (known-Boolean) cell; jump if true.
    JumpIfTrue(u32),
    /// Pop bool, push its negation.
    Not,
    /// Pop b, a; push bool a == b (deep equality).
    Eq,
    NEq,
    /// Stack: [right(e2), left(e1)] -> [e1 // e2] (both already forced sets).
    Update,
    /// Stack: [l1, l2] -> [l1 ++ l2] (both already forced lists).
    ConcatLists,
    /// Pop `concat_descs[i].poss.len()` values; string/path/arith concat.
    ConcatStrings(u32),
    /// Force TOS as attrs; select `sym`, error if missing; push attr cell.
    Select(u32),
    /// Force TOS (the final selected value) at the last-selected attribute's
    /// definition position, adding a "while evaluating the attribute '<path>'"
    /// frame on error. Operand = `texts` index of the selection-path string.
    SelectForce(u32),
    /// Like Select but on missing/non-attrs pop and jump to `target`.
    SelectOr { sym: u32, target: u32 },
    /// Stack: [v, name] -> [v.<name>]; dynamic component.
    SelectDyn,
    SelectDynOr { target: u32 },
    /// `e ? path` test; operand = haspath_descs index. Dynamic components
    /// have been pushed (in path order) above the subject value.
    HasAttrPath(u32),
    /// Pop `n` argument cells (TOS = last arg) and a function cell below
    /// them; apply; push result.
    Call(u32),
    /// Return TOS as the chunk's result.
    Ret,
    /// Push the `__curPos` attrset for this op's position.
    CurPos,
    /// Assertion failure: operand = texts index of the condition source.
    AssertFail(u32),
    /// `assert a == b` failed: pop rhs and lhs, run `assertEqValues` to
    /// produce a detailed inequality error (operand = condition texts index).
    /// Falls through to a following `AssertFail` if the values compare equal.
    AssertEq(u32),
    /// Pop a cell, push it onto the frame's with-chain.
    PushWith,
    PopWith,
    /// Remove the `n` entries directly below TOS (scope exit).
    Slide(u32),
}

/// Static attrset shape: names with their definition positions, in symbol
/// order (the order values are pushed).
pub struct AttrsDesc {
    pub names: Vec<(Symbol, PosIdx)>,
    pub pos: PosIdx,
}

/// Info for `RecOverrides`.
pub struct RecDesc {
    /// attrs_descs index describing the static shape.
    pub attrs_desc: u32,
    /// Frame slot of the first rec binding cell (bindings are contiguous,
    /// in desc order).
    pub locals_start: u32,
    /// Which desc index is `__overrides`.
    pub overrides_idx: u32,
    /// Which bindings are `inherit`ed (C++ skips thunk-wrapping for them).
    pub pos: PosIdx,
}

/// `ExprConcatStrings` shape.
pub struct ConcatDesc {
    pub force_string: bool,
    /// Per-part positions.
    pub poss: Vec<PosIdx>,
    pub pos: PosIdx,
}

/// `e ? a.b.c` path: static symbols, or None for a dynamic component
/// (whose name value has been pushed on the stack, in path order).
pub struct HasPathDesc {
    pub comps: Vec<Option<Symbol>>,
}

pub struct FormalSpec {
    pub name: Symbol,
    pub pos: PosIdx,
    /// Chunk compiled for the default expression (captures resolve against
    /// the lambda frame), if any.
    pub default: Option<u32>,
}

pub struct FormalsSpec {
    /// Sorted by (name, pos), matching the C++ displacement order.
    pub formals: Vec<FormalSpec>,
    pub ellipsis: bool,
}

pub struct LambdaSpec {
    /// `Symbol(0)` if there is no @-pattern / simple argument name.
    pub arg: Symbol,
    pub formals: Option<FormalsSpec>,
}

#[derive(Default)]
pub struct Chunk {
    pub ops: Vec<Op>,
    /// (op index, source position), sorted by op index; the position of an
    /// op is the entry with the largest index <= it.
    pub poss: Vec<(u32, PosIdx)>,
    /// Number of with-chain entries captured as the first upvalues
    /// (outermost first).
    pub with_captures: u32,
    /// Non-with captures, appended after the with prefix.
    pub captures: Vec<Cap>,
    /// Present iff this chunk is a lambda body.
    pub lambda: Option<LambdaSpec>,
    /// Lambda name (display), or Symbol(0).
    pub name: Symbol,
    /// Lambda / chunk origin position.
    pub pos: PosIdx,
}

impl Chunk {
    pub fn pos_at(&self, ip: usize) -> PosIdx {
        match self.poss.binary_search_by(|e| e.0.cmp(&(ip as u32))) {
            Ok(i) => self.poss[i].1,
            Err(0) => self.pos,
            Err(i) => self.poss[i - 1].1,
        }
    }
}

pub struct Program {
    pub chunks: Vec<Chunk>,
    pub consts: Vec<VRef>,
    pub attrs_descs: Vec<AttrsDesc>,
    pub rec_descs: Vec<RecDesc>,
    pub concat_descs: Vec<ConcatDesc>,
    pub haspath_descs: Vec<HasPathDesc>,
    /// Source texts (assert conditions).
    pub texts: Vec<Vec<u8>>,
    /// One per chunk; thunk data objects point at these.
    pub refs: Vec<CodeRef>,
}

/// Immortal handle stored as the code word of thunk/closure data objects.
pub struct CodeRef {
    pub prog: *const Program,
    pub chunk: u32,
}

impl CodeRef {
    #[inline]
    pub fn prog(&self) -> &'static Program {
        // SAFETY: programs are leaked and immortal.
        unsafe { &*self.prog }
    }

    #[inline]
    pub fn chunk(&self) -> &'static Chunk {
        &self.prog().chunks[self.chunk as usize]
    }
}

impl Program {
    /// Leak the program, wiring up its `CodeRef`s. Returns the immortal
    /// reference and the code ref for `chunk 0` (the entry chunk).
    pub fn leak(mut self) -> &'static Program {
        let n = self.chunks.len();
        self.refs = Vec::with_capacity(n);
        let p = Box::leak(Box::new(self));
        let raw = p as *const Program;
        for i in 0..n {
            p.refs.push(CodeRef {
                prog: raw,
                chunk: i as u32,
            });
        }
        p
    }

    #[inline]
    pub fn code_ref(&'static self, chunk: u32) -> &'static CodeRef {
        &self.refs[chunk as usize]
    }
}
