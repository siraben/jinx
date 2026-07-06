//! AST mirroring C++ `Expr*` (nixexpr.hh) 1:1, arena-allocated.

use std::collections::BTreeMap;

use crate::pos::PosIdx;
use crate::symbol::Symbol;

/// Index into the expression arena.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExprId(pub u32);

/// One element of an attribute path: either a static symbol or a dynamic
/// expression (`AttrName` in C++).
#[derive(Clone, Copy, Debug)]
pub struct AttrName {
    pub symbol: Symbol,       // Symbol(0) if dynamic
    pub expr: Option<ExprId>, // Some(..) if dynamic
}

impl AttrName {
    pub fn sym(s: Symbol) -> Self {
        AttrName {
            symbol: s,
            expr: None,
        }
    }

    pub fn dynamic(e: ExprId) -> Self {
        AttrName {
            symbol: Symbol(0),
            expr: Some(e),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttrDefKind {
    /// `attr = expr;`
    Plain,
    /// `inherit attr1 attrn;`
    Inherited,
    /// `inherit (expr) attr1 attrn;`
    InheritedFrom,
}

#[derive(Clone, Copy, Debug)]
pub struct AttrDef {
    pub kind: AttrDefKind,
    pub e: ExprId,
    pub pos: PosIdx,
}

#[derive(Clone, Copy, Debug)]
pub struct DynamicAttrDef {
    pub name_expr: ExprId,
    pub value_expr: ExprId,
    pub pos: PosIdx,
}

/// `ExprAttrs`. `attrs` is ordered by symbol id (creation order), like the
/// C++ `std::map<Symbol, AttrDef>`.
#[derive(Default, Debug)]
pub struct ExprAttrs {
    pub recursive: bool,
    pub pos: PosIdx,
    pub attrs: BTreeMap<Symbol, AttrDef>,
    pub dynamic_attrs: Vec<DynamicAttrDef>,
    pub inherit_from_exprs: Vec<ExprId>,
}

#[derive(Clone, Copy, Debug)]
pub struct Formal {
    pub pos: PosIdx,
    pub name: Symbol,
    pub def: Option<ExprId>,
}

#[derive(Clone, Debug, Default)]
pub struct Formals {
    /// Sorted by (name, pos) once validated (see `validateFormals`).
    pub formals: Vec<Formal>,
    pub ellipsis: bool,
}

#[derive(Debug)]
pub struct ExprLambda {
    pub pos: PosIdx,
    pub name: Symbol,
    pub arg: Symbol, // Symbol(0) if none
    pub formals: Option<Formals>,
    pub body: ExprId,
}

#[derive(Debug)]
pub enum Expr {
    Int(i64),
    Float(f64),
    String(Vec<u8>),
    /// Canonicalized absolute path (`ExprPath`).
    Path(Vec<u8>),
    Var {
        pos: PosIdx,
        name: Symbol,
    },
    /// `ExprInheritFrom`: pseudo-variable referring to
    /// `ExprAttrs::inherit_from_exprs[displ]`.
    InheritFrom {
        pos: PosIdx,
        displ: u32,
    },
    Select {
        pos: PosIdx,
        e: ExprId,
        attrpath: Vec<AttrName>,
        def: Option<ExprId>,
    },
    OpHasAttr {
        e: ExprId,
        attrpath: Vec<AttrName>,
    },
    Attrs(ExprAttrs),
    List(Vec<ExprId>),
    Lambda(ExprLambda),
    Call {
        pos: PosIdx,
        fun: ExprId,
        args: Vec<ExprId>,
        /// Set while parsing a "cursed or" (`expr_simple OR_KW`); used to
        /// emit the deprecation warning. See NixOS/nix#11118.
        cursed_or_end_pos: Option<PosIdx>,
    },
    Let {
        attrs: ExprId, // always Expr::Attrs
        body: ExprId,
    },
    With {
        pos: PosIdx,
        attrs: ExprId,
        body: ExprId,
    },
    If {
        pos: PosIdx,
        cond: ExprId,
        then: ExprId,
        else_: ExprId,
    },
    Assert {
        pos: PosIdx,
        cond: ExprId,
        body: ExprId,
    },
    OpNot(ExprId),
    OpEq(ExprId, ExprId),
    OpNEq(ExprId, ExprId),
    OpAnd(PosIdx, ExprId, ExprId),
    OpOr(PosIdx, ExprId, ExprId),
    OpImpl(PosIdx, ExprId, ExprId),
    OpUpdate(PosIdx, ExprId, ExprId),
    OpConcatLists(PosIdx, ExprId, ExprId),
    ConcatStrings {
        pos: PosIdx,
        force_string: bool,
        es: Vec<(PosIdx, ExprId)>,
    },
    /// `__curPos`
    CurPos(PosIdx),
}

/// Expression arena (C++ `Exprs`).
#[derive(Default)]
pub struct Exprs {
    arena: Vec<Expr>,
}

impl Exprs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, e: Expr) -> ExprId {
        self.arena.push(e);
        ExprId((self.arena.len() - 1) as u32)
    }

    pub fn get(&self, id: ExprId) -> &Expr {
        &self.arena[id.0 as usize]
    }

    pub fn get_mut(&mut self, id: ExprId) -> &mut Expr {
        &mut self.arena[id.0 as usize]
    }

    pub fn attrs(&self, id: ExprId) -> &ExprAttrs {
        match self.get(id) {
            Expr::Attrs(a) => a,
            _ => panic!("expected ExprAttrs"),
        }
    }

    pub fn attrs_mut(&mut self, id: ExprId) -> &mut ExprAttrs {
        match self.get_mut(id) {
            Expr::Attrs(a) => a,
            _ => panic!("expected ExprAttrs"),
        }
    }
}
