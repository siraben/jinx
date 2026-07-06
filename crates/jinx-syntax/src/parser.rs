//! Recursive-descent parser producing the same AST (and byte-identical
//! `--parse` output / error messages) as nix's bison grammar (parser.y +
//! parser-state.hh).
//!
//! Error messages replicate bison's `parse.error detailed` behaviour,
//! including its quirks: expected-token lists are computed from the LALR
//! automaton state *after* default reductions, so many contexts report a
//! "masked" subset (e.g. `{ 1 = x; }` says "expecting 'inherit'"), and no
//! list is printed when more than 4 tokens would be expected. The lists
//! hard-coded below were derived from the automaton and validated against
//! the C++ implementation.

use crate::ast::*;
use crate::error::ParseError;
use crate::lexer::{token_name, Lexer, TokKind, Token};
use crate::pos::{OriginId, PosIdx, PosTable, NO_POS};
use crate::symbol::{Symbol, SymbolTable};

#[allow(clippy::too_many_arguments)]
pub fn parse(
    source: &[u8],
    origin: OriginId,
    exprs: &mut Exprs,
    symbols: &mut SymbolTable,
    positions: &mut PosTable,
    base_path: &str,
    home: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<ExprId, ParseError> {
    let mut p = Parser {
        lexer: Lexer::new(source, origin),
        src: source,
        peeked: Vec::new(),
        exprs,
        symbols,
        positions,
        origin,
        base_path: base_path.to_string(),
        home: home.map(|s| s.to_string()),
        warnings,
    };
    let (e, _) = p.parse_expr()?;
    let t = p.peek()?.clone();
    if !matches!(t.kind, TokKind::Eof) {
        return Err(p.err_unexpected(&t, &["end of file"]));
    }
    Ok(e)
}

/// Where a binds list appears; determines the masked expected-token list
/// bison reports for a token that can't continue the list.
#[derive(Clone, Copy, PartialEq)]
enum BindsCtx {
    /// `{ ... }` at expression level (`'{' binds1 '}'`): "expecting 'inherit'"
    Brace,
    /// `rec { ... }` / `let { ... }` (`'{' binds '}'`): "expecting 'inherit' or '}'"
    RecBrace,
    /// `let ... in`: "expecting 'in' or 'inherit'"
    Let,
}

type PExpr = (ExprId, u32); // expression + begin offset of its production

struct Parser<'a> {
    lexer: Lexer<'a>,
    src: &'a [u8],
    peeked: Vec<Token>,
    exprs: &'a mut Exprs,
    symbols: &'a mut SymbolTable,
    positions: &'a mut PosTable,
    origin: OriginId,
    base_path: String,
    home: Option<String>,
    warnings: &'a mut Vec<String>,
}

impl<'a> Parser<'a> {
    // ---------- token plumbing ----------

    fn fill(&mut self, n: usize) -> Result<(), ParseError> {
        while self.peeked.len() < n {
            let t = self.lexer.next_token(self.positions)?;
            self.peeked.push(t);
        }
        Ok(())
    }

    fn peek(&mut self) -> Result<&Token, ParseError> {
        self.fill(1)?;
        Ok(&self.peeked[0])
    }

    fn peek2(&mut self) -> Result<&Token, ParseError> {
        self.fill(2)?;
        Ok(&self.peeked[1])
    }

    fn next(&mut self) -> Result<Token, ParseError> {
        self.fill(1)?;
        Ok(self.peeked.remove(0))
    }

    fn at(&self, offset: u32) -> PosIdx {
        self.positions.add(self.origin, offset)
    }

    fn err_unexpected(&self, t: &Token, expecting: &[&str]) -> ParseError {
        let mut msg = format!("syntax error, unexpected {}", token_name(&t.kind));
        if !expecting.is_empty() {
            msg.push_str(", expecting ");
            msg.push_str(&expecting.join(" or "));
        }
        ParseError::new(msg, self.at(t.begin))
    }

    fn is_char(t: &Token, c: u8) -> bool {
        matches!(t.kind, TokKind::Char(x) if x == c)
    }

    fn expect_char(&mut self, c: u8, expecting: &[&str]) -> Result<Token, ParseError> {
        let t = self.peek()?.clone();
        if Self::is_char(&t, c) {
            self.next()
        } else {
            Err(self.err_unexpected(&t, expecting))
        }
    }

    // ---------- expr_function ----------

    fn parse_expr(&mut self) -> Result<PExpr, ParseError> {
        let t = self.peek()?.clone();
        match &t.kind {
            TokKind::Id => {
                let t2 = self.peek2()?.clone();
                if Self::is_char(&t2, b':') {
                    // ID ':' expr_function
                    let id = self.next()?;
                    self.next()?; // ':'
                    let arg = self.symbols.create(&id.text);
                    let (body, _) = self.parse_expr()?;
                    let e = self.exprs.add(Expr::Lambda(ExprLambda {
                        pos: self.at(id.begin),
                        name: Symbol(0),
                        arg,
                        formals: None,
                        body,
                    }));
                    return Ok((e, id.begin));
                }
                if Self::is_char(&t2, b'@') {
                    // ID '@' formal_set ':' expr_function
                    let id = self.next()?;
                    self.next()?; // '@'
                    let arg = self.symbols.create(&id.text);
                    let t3 = self.peek()?.clone();
                    if !Self::is_char(&t3, b'{') {
                        return Err(self.err_unexpected(&t3, &["'{'"]));
                    }
                    self.next()?;
                    let mut formals = self.parse_formals()?;
                    let t4 = self.peek()?.clone();
                    if !Self::is_char(&t4, b':') {
                        return Err(self.err_unexpected(&t4, &["':'"]));
                    }
                    self.next()?;
                    self.validate_formals(&mut formals, self.at(id.begin), arg)?;
                    let (body, _) = self.parse_expr()?;
                    let e = self.exprs.add(Expr::Lambda(ExprLambda {
                        pos: self.at(id.begin),
                        name: Symbol(0),
                        arg,
                        formals: Some(formals),
                        body,
                    }));
                    return Ok((e, id.begin));
                }
                self.parse_expr_if()
            }
            TokKind::Assert => {
                let kw = self.next()?;
                let (cond, _) = self.parse_expr()?;
                let t2 = self.peek()?.clone();
                if !Self::is_char(&t2, b';') {
                    return Err(self.err_unexpected(&t2, &["';'"]));
                }
                self.next()?;
                let (body, _) = self.parse_expr()?;
                let e = self.exprs.add(Expr::Assert {
                    pos: self.at(kw.begin),
                    cond,
                    body,
                });
                Ok((e, kw.begin))
            }
            TokKind::With => {
                let kw = self.next()?;
                let (attrs, _) = self.parse_expr()?;
                let t2 = self.peek()?.clone();
                if !Self::is_char(&t2, b';') {
                    return Err(self.err_unexpected(&t2, &["';'"]));
                }
                self.next()?;
                let (body, _) = self.parse_expr()?;
                let e = self.exprs.add(Expr::With {
                    pos: self.at(kw.begin),
                    attrs,
                    body,
                });
                Ok((e, kw.begin))
            }
            TokKind::Let => {
                let t2 = self.peek2()?.clone();
                if Self::is_char(&t2, b'{') {
                    // Old-style `let { ..., body = ... }`, an expr_simple:
                    // continue the operator pipeline from it.
                    let simple = self.parse_simple()?;
                    return self.continue_from_simple(simple);
                }
                let kw = self.next()?;
                let attrs = self.parse_binds(BindsCtx::Let)?;
                debug_assert!(matches!(self.peek()?.kind, TokKind::In));
                self.next()?; // 'in'
                if !self.exprs.attrs(attrs).dynamic_attrs.is_empty() {
                    return Err(ParseError::new(
                        "dynamic attributes not allowed in let",
                        self.at(kw.begin),
                    ));
                }
                let (body, _) = self.parse_expr()?;
                let e = self.exprs.add(Expr::Let { attrs, body });
                Ok((e, kw.begin))
            }
            TokKind::Char(b'{') => {
                // Ambiguous: formal_set (lambda) or attrset.
                self.parse_brace_at_function_level()
            }
            _ => self.parse_expr_if(),
        }
    }

    fn parse_expr_if(&mut self) -> Result<PExpr, ParseError> {
        let t = self.peek()?.clone();
        if matches!(t.kind, TokKind::If) {
            let kw = self.next()?;
            let (cond, _) = self.parse_expr()?;
            let t2 = self.peek()?.clone();
            if !matches!(t2.kind, TokKind::Then) {
                return Err(self.err_unexpected(&t2, &["'then'"]));
            }
            self.next()?;
            let (then, _) = self.parse_expr()?;
            let t3 = self.peek()?.clone();
            if !matches!(t3.kind, TokKind::Else) {
                return Err(self.err_unexpected(&t3, &["'else'"]));
            }
            self.next()?;
            let (else_, _) = self.parse_expr()?;
            let e = self.exprs.add(Expr::If {
                pos: self.at(kw.begin),
                cond,
                then,
                else_,
            });
            return Ok((e, kw.begin));
        }
        self.parse_op(0, None)
    }

    /// Continue the expr_op / expr_app / expr_select chain from an already
    /// parsed expr_simple (used by the `{`-at-function-level and old-let
    /// paths of parse_expr).
    fn continue_from_simple(&mut self, simple: PExpr) -> Result<PExpr, ParseError> {
        let sel = self.parse_select_from(simple)?;
        let app = self.parse_app_from(sel)?;
        self.parse_op(0, Some(app))
    }

    // ---------- operators ----------

    fn parse_op(&mut self, min_prec: u8, first: Option<PExpr>) -> Result<PExpr, ParseError> {
        let (mut e, begin) = match first {
            Some(x) => x,
            None => {
                let t = self.peek()?.clone();
                match t.kind {
                    TokKind::Char(b'!') => {
                        let op = self.next()?;
                        // %prec NOT (level 7): operand binds ops of prec >= 8
                        let (inner, _) = self.parse_op(8, None)?;
                        (self.exprs.add(Expr::OpNot(inner)), op.begin)
                    }
                    TokKind::Char(b'-') => {
                        let op = self.next()?;
                        // %prec NEGATE (level 12): operand binds prec >= 13
                        let (inner, _) = self.parse_op(13, None)?;
                        let sub = self.var_noPos("__sub");
                        let zero = self.exprs.add(Expr::Int(0));
                        let e = self.exprs.add(Expr::Call {
                            pos: self.at(op.begin),
                            fun: sub,
                            args: vec![zero, inner],
                        cursed_or_end_pos: None,
                        });
                        (e, op.begin)
                    }
                    _ => self.parse_app_from_none()?,
                }
            }
        };
        // consumed nonassoc levels in this invocation
        let mut nonassoc_seen = [false; 16];
        loop {
            let t = self.peek()?.clone();
            let Some((prec, assoc)) = op_info(&t.kind) else {
                break;
            };
            if prec < min_prec {
                break;
            }
            if assoc == Assoc::None && nonassoc_seen[prec as usize] {
                return Err(self.err_unexpected(&t, &[]));
            }
            let op = self.next()?;
            if assoc == Assoc::None {
                nonassoc_seen[prec as usize] = true;
            }
            if matches!(op.kind, TokKind::Char(b'?')) {
                let attrpath = self.parse_attrpath()?;
                e = self.exprs.add(Expr::OpHasAttr { e, attrpath });
                continue;
            }
            let rhs_min = match assoc {
                Assoc::Left | Assoc::None => prec + 1,
                Assoc::Right => prec,
            };
            let (rhs, rhs_begin) = self.parse_op(rhs_min, None)?;
            let op_pos = self.at(op.begin);
            e = match op.kind {
                TokKind::Eq => self.exprs.add(Expr::OpEq(e, rhs)),
                TokKind::NEq => self.exprs.add(Expr::OpNEq(e, rhs)),
                TokKind::Char(b'<') => {
                    let lt = self.var_noPos("__lessThan");
                    self.exprs.add(Expr::Call {
                        pos: op_pos,
                        fun: lt,
                        args: vec![e, rhs],
                        cursed_or_end_pos: None,
                    })
                }
                TokKind::Leq => {
                    let lt = self.var_noPos("__lessThan");
                    let call = self.exprs.add(Expr::Call {
                        pos: op_pos,
                        fun: lt,
                        args: vec![rhs, e],
                        cursed_or_end_pos: None,
                    });
                    self.exprs.add(Expr::OpNot(call))
                }
                TokKind::Char(b'>') => {
                    let lt = self.var_noPos("__lessThan");
                    self.exprs.add(Expr::Call {
                        pos: op_pos,
                        fun: lt,
                        args: vec![rhs, e],
                        cursed_or_end_pos: None,
                    })
                }
                TokKind::Geq => {
                    let lt = self.var_noPos("__lessThan");
                    let call = self.exprs.add(Expr::Call {
                        pos: op_pos,
                        fun: lt,
                        args: vec![e, rhs],
                        cursed_or_end_pos: None,
                    });
                    self.exprs.add(Expr::OpNot(call))
                }
                TokKind::And => self.exprs.add(Expr::OpAnd(op_pos, e, rhs)),
                TokKind::Or => self.exprs.add(Expr::OpOr(op_pos, e, rhs)),
                TokKind::Impl => self.exprs.add(Expr::OpImpl(op_pos, e, rhs)),
                TokKind::Update => self.exprs.add(Expr::OpUpdate(op_pos, e, rhs)),
                TokKind::Char(b'+') => {
                    let es = vec![(self.at(begin), e), (self.at(rhs_begin), rhs)];
                    self.exprs.add(Expr::ConcatStrings {
                        pos: op_pos,
                        force_string: false,
                        es,
                    })
                }
                TokKind::Char(b'-') => {
                    let f = self.var_noPos("__sub");
                    self.exprs.add(Expr::Call {
                        pos: op_pos,
                        fun: f,
                        args: vec![e, rhs],
                        cursed_or_end_pos: None,
                    })
                }
                TokKind::Char(b'*') => {
                    let f = self.var_noPos("__mul");
                    self.exprs.add(Expr::Call {
                        pos: op_pos,
                        fun: f,
                        args: vec![e, rhs],
                        cursed_or_end_pos: None,
                    })
                }
                TokKind::Char(b'/') => {
                    let f = self.var_noPos("__div");
                    self.exprs.add(Expr::Call {
                        pos: op_pos,
                        fun: f,
                        args: vec![e, rhs],
                        cursed_or_end_pos: None,
                    })
                }
                TokKind::Concat => self.exprs.add(Expr::OpConcatLists(op_pos, e, rhs)),
                _ => unreachable!(),
            };
        }
        Ok((e, begin))
    }

    // ---------- application / selection ----------

    fn parse_app_from_none(&mut self) -> Result<PExpr, ParseError> {
        let s = self.parse_select(None)?;
        self.parse_app_from(s)
    }

    fn parse_app_from(&mut self, first: PExpr) -> Result<PExpr, ParseError> {
        let (mut e, begin) = first;
        // `expr_app : expr_select { $$->resetCursedOr(); }`: a bare
        // expr_select flowing into expr_app is no longer cursed.
        self.reset_cursed_or(e);
        loop {
            let t = self.peek()?.clone();
            if !starts_expr_select(&t.kind) {
                break;
            }
            let (arg, _) = self.parse_select(None)?;
            self.warn_if_cursed_or(arg);
            // makeCall: append to an existing ExprCall
            if let Expr::Call { args, .. } = self.exprs.get_mut(e) {
                args.push(arg);
            } else {
                e = self.exprs.add(Expr::Call {
                    pos: self.at(begin),
                    fun: e,
                    args: vec![arg],
                    cursed_or_end_pos: None,
                });
            }
        }
        Ok((e, begin))
    }

    fn parse_select(&mut self, first: Option<PExpr>) -> Result<PExpr, ParseError> {
        let simple = match first {
            Some(x) => x,
            None => self.parse_simple()?,
        };
        self.parse_select_from(simple)
    }

    fn parse_select_from(&mut self, simple: PExpr) -> Result<PExpr, ParseError> {
        let (e, begin) = simple;
        let t = self.peek()?.clone();
        if Self::is_char(&t, b'.') {
            self.next()?;
            let attrpath = self.parse_attrpath()?;
            let t2 = self.peek()?.clone();
            let def = if matches!(t2.kind, TokKind::OrKw) {
                self.next()?;
                let (d, _) = self.parse_select(None)?;
                self.warn_if_cursed_or(d);
                Some(d)
            } else {
                None
            };
            let sel = self.exprs.add(Expr::Select {
                pos: self.at(begin),
                e,
                attrpath,
                def,
            });
            return Ok((sel, begin));
        }
        if matches!(t.kind, TokKind::OrKw) {
            // "cursed or": expr_simple OR_KW
            let or_tok = self.next()?;
            let or_sym = self.symbols.create(b"or");
            let orvar = self.exprs.add(Expr::Var {
                pos: self.at(begin),
                name: or_sym,
            });
            let call = self.exprs.add(Expr::Call {
                pos: self.at(begin),
                fun: e,
                args: vec![orvar],
                cursed_or_end_pos: Some(self.at(or_tok.end)),
            });
            return Ok((call, begin));
        }
        Ok((e, begin))
    }

    // ---------- expr_simple ----------

    fn parse_simple(&mut self) -> Result<PExpr, ParseError> {
        let t = self.peek()?.clone();
        match &t.kind {
            TokKind::Id => {
                let t = self.next()?;
                let e = if t.text == b"__curPos" {
                    self.exprs.add(Expr::CurPos(self.at(t.begin)))
                } else {
                    let sym = self.symbols.create(&t.text);
                    self.exprs.add(Expr::Var {
                        pos: self.at(t.begin),
                        name: sym,
                    })
                };
                Ok((e, t.begin))
            }
            TokKind::Int(n) => {
                let n = *n;
                let t = self.next()?;
                Ok((self.exprs.add(Expr::Int(n)), t.begin))
            }
            TokKind::Float(f) => {
                let f = *f;
                let t = self.next()?;
                Ok((self.exprs.add(Expr::Float(f)), t.begin))
            }
            TokKind::Char(b'"') => {
                let open = self.next()?;
                let e = match self.parse_string_tail()? {
                    StringParse::Plain(s) => self.exprs.add(Expr::String(s)),
                    StringParse::Interp(e) => e,
                };
                Ok((e, open.begin))
            }
            TokKind::IndStringOpen => {
                let open = self.next()?;
                let e = self.parse_ind_string_tail(open.begin)?;
                Ok((e, open.begin))
            }
            TokKind::Path | TokKind::HPath => self.parse_path(),
            TokKind::SPath => {
                let t = self.next()?;
                let inner = t.text[1..t.text.len() - 1].to_vec();
                let find_file = self.var_noPos("__findFile");
                let nix_path = self.var_noPos("__nixPath");
                let s = self.exprs.add(Expr::String(inner));
                let e = self.exprs.add(Expr::Call {
                    pos: self.at(t.begin),
                    fun: find_file,
                    args: vec![nix_path, s],
                        cursed_or_end_pos: None,
                });
                Ok((e, t.begin))
            }
            TokKind::Uri => {
                let t = self.next()?;
                Ok((self.exprs.add(Expr::String(t.text.clone())), t.begin))
            }
            TokKind::Char(b'(') => {
                let open = self.next()?;
                let (e, _) = self.parse_expr()?;
                let t2 = self.peek()?.clone();
                if !Self::is_char(&t2, b')') {
                    return Err(self.err_unexpected(&t2, &["')'"]));
                }
                self.next()?;
                Ok((e, open.begin))
            }
            TokKind::Let => {
                // LET '{' binds '}': old-style let, desugared to
                // (rec { ..., body = ... }).body
                let kw = self.next()?;
                let t2 = self.peek()?.clone();
                if !Self::is_char(&t2, b'{') {
                    return Err(self.err_unexpected(&t2, &["'{'"]));
                }
                self.next()?;
                let attrs = self.parse_binds(BindsCtx::RecBrace)?;
                debug_assert!(Self::is_char(self.peek()?, b'}'));
                self.next()?;
                {
                    let a = self.exprs.attrs_mut(attrs);
                    a.recursive = true;
                }
                let pos = self.at(kw.begin);
                self.exprs.attrs_mut(attrs).pos = pos;
                let body_sym = self.symbols.create(b"body");
                let e = self.exprs.add(Expr::Select {
                    pos: NO_POS,
                    e: attrs,
                    attrpath: vec![AttrName::sym(body_sym)],
                    def: None,
                });
                Ok((e, kw.begin))
            }
            TokKind::Rec => {
                let kw = self.next()?;
                let t2 = self.peek()?.clone();
                if !Self::is_char(&t2, b'{') {
                    return Err(self.err_unexpected(&t2, &["'{'"]));
                }
                self.next()?;
                let attrs = self.parse_binds(BindsCtx::RecBrace)?;
                debug_assert!(Self::is_char(self.peek()?, b'}'));
                self.next()?;
                let pos = self.at(kw.begin);
                {
                    let a = self.exprs.attrs_mut(attrs);
                    a.recursive = true;
                    a.pos = pos;
                }
                Ok((attrs, kw.begin))
            }
            TokKind::Char(b'{') => {
                // Pure attrset context (no formals possible here).
                let open = self.next()?;
                let t2 = self.peek()?.clone();
                if Self::is_char(&t2, b'}') {
                    self.next()?;
                    let pos = self.at(open.begin);
                    let e = self.exprs.add(Expr::Attrs(ExprAttrs {
                        pos,
                        ..Default::default()
                    }));
                    return Ok((e, open.begin));
                }
                let attrs = self.parse_binds(BindsCtx::Brace)?;
                debug_assert!(Self::is_char(self.peek()?, b'}'));
                self.next()?;
                let pos = self.at(open.begin);
                self.exprs.attrs_mut(attrs).pos = pos;
                Ok((attrs, open.begin))
            }
            TokKind::Char(b'[') => {
                let open = self.next()?;
                let mut elems = Vec::new();
                loop {
                    let t2 = self.peek()?.clone();
                    if Self::is_char(&t2, b']') {
                        self.next()?;
                        break;
                    }
                    if !starts_expr_select(&t2.kind) {
                        return Err(self.err_unexpected(&t2, &[]));
                    }
                    let (e, _) = self.parse_select(None)?;
                    self.warn_if_cursed_or(e);
                    elems.push(e);
                }
                Ok((self.exprs.add(Expr::List(elems)), open.begin))
            }
            _ => Err(self.err_unexpected(&t, &[])),
        }
    }

    // ---------- strings ----------

    fn parse_string_tail(&mut self) -> Result<StringParse, ParseError> {
        let mut parts: Vec<(u32, StrPart)> = Vec::new();
        let mut has_interp = false;
        loop {
            let t = self.peek()?.clone();
            match t.kind {
                TokKind::Str { .. } => {
                    let t = self.next()?;
                    parts.push((t.begin, StrPart::Str(t.text)));
                }
                TokKind::DollarCurly => {
                    let open = self.next()?;
                    let (e, _) = self.parse_expr()?;
                    let t2 = self.peek()?.clone();
                    if !Self::is_char(&t2, b'}') {
                        return Err(self.err_unexpected(&t2, &["'}'"]));
                    }
                    self.next()?;
                    parts.push((open.begin, StrPart::Expr(e)));
                    has_interp = true;
                }
                TokKind::Char(b'"') => {
                    self.next()?;
                    break;
                }
                TokKind::Eof => {
                    return Err(self.err_unexpected(&t, &["'\"'"]));
                }
                _ => unreachable!("bad token in string"),
            }
        }
        if !has_interp {
            let s = match parts.pop() {
                Some((_, StrPart::Str(s))) => s,
                None => Vec::new(),
                _ => unreachable!(),
            };
            return Ok(StringParse::Plain(s));
        }
        let concat_begin = parts[0].0;
        let es: Vec<(PosIdx, ExprId)> = parts
            .into_iter()
            .map(|(b, p)| {
                let e = match p {
                    StrPart::Str(s) => self.exprs.add(Expr::String(s)),
                    StrPart::Expr(e) => e,
                };
                (self.at(b), e)
            })
            .collect();
        let pos = self.at(concat_begin);
        Ok(StringParse::Interp(self.exprs.add(Expr::ConcatStrings {
            pos,
            force_string: true,
            es,
        })))
    }

    fn parse_ind_string_tail(&mut self, open_begin: u32) -> Result<ExprId, ParseError> {
        let mut parts: Vec<(PosIdx, IndPart)> = Vec::new();
        loop {
            let t = self.peek()?.clone();
            match t.kind {
                TokKind::IndStr { has_indent } => {
                    let t = self.next()?;
                    parts.push((self.at(t.begin), IndPart::Str(t.text, has_indent)));
                }
                TokKind::DollarCurly => {
                    let open = self.next()?;
                    let (e, _) = self.parse_expr()?;
                    let t2 = self.peek()?.clone();
                    if !Self::is_char(&t2, b'}') {
                        return Err(self.err_unexpected(&t2, &["'}'"]));
                    }
                    self.next()?;
                    parts.push((self.at(open.begin), IndPart::Expr(e)));
                }
                TokKind::IndStringClose => {
                    self.next()?;
                    break;
                }
                TokKind::Eof => {
                    return Err(self.err_unexpected(
                        &t,
                        &["indented string", "'${'", "end of an indented string"],
                    ));
                }
                _ => unreachable!("bad token in indented string"),
            }
        }
        Ok(self.strip_indentation(self.at(open_begin), parts))
    }

    /// Port of `ParserState::stripIndentation`.
    fn strip_indentation(&mut self, pos: PosIdx, es: Vec<(PosIdx, IndPart)>) -> ExprId {
        if es.is_empty() {
            return self.exprs.add(Expr::String(Vec::new()));
        }

        // Figure out the minimum indentation.
        let mut at_start_of_line = true;
        let mut min_indent: usize = 1_000_000;
        let mut cur_indent: usize = 0;
        for (_, part) in &es {
            match part {
                IndPart::Expr(_) | IndPart::Str(_, false) => {
                    if at_start_of_line {
                        at_start_of_line = false;
                        if cur_indent < min_indent {
                            min_indent = cur_indent;
                        }
                    }
                }
                IndPart::Str(s, true) => {
                    for &c in s {
                        if at_start_of_line {
                            if c == b' ' {
                                cur_indent += 1;
                            } else if c == b'\n' {
                                cur_indent = 0;
                            } else {
                                at_start_of_line = false;
                                if cur_indent < min_indent {
                                    min_indent = cur_indent;
                                }
                            }
                        } else if c == b'\n' {
                            at_start_of_line = true;
                            cur_indent = 0;
                        }
                    }
                }
            }
        }

        // Strip spaces from each line.
        let mut es2: Vec<(PosIdx, ExprId)> = Vec::new();
        let mut at_start_of_line = true;
        let mut cur_dropped: usize = 0;
        let mut n = es.len();
        for (part_pos, part) in es {
            match part {
                IndPart::Expr(e) => {
                    at_start_of_line = false;
                    cur_dropped = 0;
                    es2.push((part_pos, e));
                }
                IndPart::Str(s, _) => {
                    let mut s2: Vec<u8> = Vec::with_capacity(s.len());
                    for &c in &s {
                        if at_start_of_line {
                            if c == b' ' {
                                if cur_dropped >= min_indent {
                                    s2.push(c);
                                }
                                cur_dropped += 1;
                            } else if c == b'\n' {
                                cur_dropped = 0;
                                s2.push(c);
                            } else {
                                at_start_of_line = false;
                                cur_dropped = 0;
                                s2.push(c);
                            }
                        } else {
                            s2.push(c);
                            if c == b'\n' {
                                at_start_of_line = true;
                            }
                        }
                    }
                    // Remove the last line if it is empty and consists only
                    // of spaces.
                    if n == 1 {
                        if let Some(p) = s2.iter().rposition(|&c| c == b'\n') {
                            if s2[p + 1..].iter().all(|&c| c == b' ') {
                                s2.truncate(p + 1);
                            }
                        }
                    }
                    if !s2.is_empty() {
                        let e = self.exprs.add(Expr::String(s2));
                        es2.push((part_pos, e));
                    }
                }
            }
            n -= 1;
        }

        if es2.is_empty() {
            return self.exprs.add(Expr::String(Vec::new()));
        }
        if es2.len() == 1 && matches!(self.exprs.get(es2[0].1), Expr::String(_)) {
            return es2[0].1;
        }
        self.exprs.add(Expr::ConcatStrings {
            pos,
            force_string: true,
            es: es2,
        })
    }

    // ---------- paths ----------

    fn parse_path(&mut self) -> Result<PExpr, ParseError> {
        let t = self.next()?;
        let begin = t.begin;
        let path_start = match t.kind {
            TokKind::Path => {
                let literal = &t.text;
                let mut path = if literal[0] == b'/' {
                    canon_path(literal, b"")
                } else {
                    canon_path(literal, self.base_path.as_bytes())
                };
                if literal.len() > 1 && literal[literal.len() - 1] == b'/' {
                    path.push(b'/');
                }
                self.exprs.add(Expr::Path(path))
            }
            TokKind::HPath => {
                let mut path: Vec<u8> = self
                    .home
                    .as_deref()
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec();
                path.extend_from_slice(&t.text[1..]);
                self.exprs.add(Expr::Path(path))
            }
            _ => unreachable!(),
        };
        // path_start PATH_END | path_start string_parts_interpolated PATH_END
        let t2 = self.peek()?.clone();
        if matches!(t2.kind, TokKind::PathEnd) {
            self.next()?;
            return Ok((path_start, begin));
        }
        let mut es: Vec<(PosIdx, ExprId)> = vec![(self.at(begin), path_start)];
        loop {
            let t = self.peek()?.clone();
            match t.kind {
                TokKind::Str { .. } => {
                    let t = self.next()?;
                    let e = self.exprs.add(Expr::String(t.text));
                    es.push((self.at(t.begin), e));
                }
                TokKind::DollarCurly => {
                    let open = self.next()?;
                    let (e, _) = self.parse_expr()?;
                    let t2 = self.peek()?.clone();
                    if !Self::is_char(&t2, b'}') {
                        return Err(self.err_unexpected(&t2, &["'}'"]));
                    }
                    self.next()?;
                    es.push((self.at(open.begin), e));
                }
                TokKind::PathEnd => {
                    self.next()?;
                    break;
                }
                _ => unreachable!("bad token in path"),
            }
        }
        let pos = self.at(begin);
        let e = self.exprs.add(Expr::ConcatStrings {
            pos,
            force_string: false,
            es,
        });
        Ok((e, begin))
    }

    // ---------- attrpaths / attr names ----------

    fn parse_attrpath(&mut self) -> Result<Vec<AttrName>, ParseError> {
        let mut path = vec![self.parse_attr_name()?];
        loop {
            let t = self.peek()?.clone();
            if Self::is_char(&t, b'.') {
                self.next()?;
                path.push(self.parse_attr_name()?);
            } else {
                break;
            }
        }
        Ok(path)
    }

    fn parse_attr_name(&mut self) -> Result<AttrName, ParseError> {
        let t = self.peek()?.clone();
        match t.kind {
            TokKind::Id => {
                let t = self.next()?;
                Ok(AttrName::sym(self.symbols.create(&t.text)))
            }
            TokKind::OrKw => {
                self.next()?;
                Ok(AttrName::sym(self.symbols.create(b"or")))
            }
            TokKind::Char(b'"') => {
                self.next()?;
                match self.parse_string_tail()? {
                    StringParse::Plain(s) => Ok(AttrName::sym(self.symbols.create(&s))),
                    StringParse::Interp(e) => Ok(AttrName::dynamic(e)),
                }
            }
            TokKind::DollarCurly => {
                self.next()?;
                let (e, _) = self.parse_expr()?;
                let t2 = self.peek()?.clone();
                if !Self::is_char(&t2, b'}') {
                    return Err(self.err_unexpected(&t2, &["'}'"]));
                }
                self.next()?;
                // A `${"literal"}` is treated as a static name.
                if let Expr::String(s) = self.exprs.get(e) {
                    let s = s.clone();
                    Ok(AttrName::sym(self.symbols.create(&s)))
                } else {
                    Ok(AttrName::dynamic(e))
                }
            }
            _ => Err(self.err_unexpected(&t, &["identifier", "'or'", "'${'", "'\"'"])),
        }
    }

    // ---------- binds ----------

    fn binds_err(&self, t: &Token, ctx: BindsCtx) -> ParseError {
        match ctx {
            BindsCtx::Brace => self.err_unexpected(t, &["'inherit'"]),
            BindsCtx::RecBrace => self.err_unexpected(t, &["'inherit'", "'}'"]),
            BindsCtx::Let => self.err_unexpected(t, &["'in'", "'inherit'"]),
        }
    }

    /// Parse a (possibly empty) list of bindings. Stops before the
    /// terminator ('}' for brace contexts, 'in' for let), which the caller
    /// consumes.
    fn parse_binds(&mut self, ctx: BindsCtx) -> Result<ExprId, ParseError> {
        let attrs = self.exprs.add(Expr::Attrs(ExprAttrs::default()));
        loop {
            let t = self.peek()?.clone();
            match t.kind {
                TokKind::Inherit => self.parse_inherit(attrs)?,
                TokKind::Id | TokKind::OrKw | TokKind::Char(b'"') | TokKind::DollarCurly => {
                    self.parse_binding(attrs)?;
                }
                TokKind::Char(b'}') if ctx != BindsCtx::Let => break,
                TokKind::In if ctx == BindsCtx::Let => break,
                _ => return Err(self.binds_err(&t, ctx)),
            }
        }
        Ok(attrs)
    }

    fn parse_binding(&mut self, attrs: ExprId) -> Result<(), ParseError> {
        let begin = self.peek()?.begin;
        let attrpath = self.parse_attrpath()?;
        let t = self.peek()?.clone();
        if !Self::is_char(&t, b'=') {
            return Err(self.err_unexpected(&t, &["'.'", "'='"]));
        }
        self.next()?;
        let (e, _) = self.parse_expr()?;
        let t2 = self.peek()?.clone();
        if !Self::is_char(&t2, b';') {
            return Err(self.err_unexpected(&t2, &["';'"]));
        }
        self.next()?;
        self.add_attr(attrs, attrpath, begin, e)
    }

    fn parse_inherit(&mut self, attrs: ExprId) -> Result<(), ParseError> {
        self.next()?; // 'inherit'
        let t = self.peek()?.clone();
        let from = if Self::is_char(&t, b'(') {
            self.next()?;
            let (e, e_begin) = self.parse_expr()?;
            let t2 = self.peek()?.clone();
            if !Self::is_char(&t2, b')') {
                return Err(self.err_unexpected(&t2, &["')'"]));
            }
            self.next()?;
            let displ = self.exprs.attrs(attrs).inherit_from_exprs.len() as u32;
            self.exprs.attrs_mut(attrs).inherit_from_exprs.push(e);
            let pos = self.at(e_begin);
            Some(self.exprs.add(Expr::InheritFrom { pos, displ }))
        } else {
            None
        };
        loop {
            let t = self.peek()?.clone();
            let (sym, i_begin) = match t.kind {
                TokKind::Id => {
                    let t = self.next()?;
                    (self.symbols.create(&t.text), t.begin)
                }
                TokKind::OrKw => {
                    let t = self.next()?;
                    (self.symbols.create(b"or"), t.begin)
                }
                TokKind::Char(b'"') => {
                    let t = self.next()?;
                    match self.parse_string_tail()? {
                        StringParse::Plain(s) => (self.symbols.create(&s), t.begin),
                        StringParse::Interp(_) => {
                            return Err(ParseError::new(
                                "dynamic attributes not allowed in inherit",
                                self.at(t.begin),
                            ));
                        }
                    }
                }
                TokKind::DollarCurly => {
                    let open = self.next()?;
                    let (e, _) = self.parse_expr()?;
                    let t2 = self.peek()?.clone();
                    if !Self::is_char(&t2, b'}') {
                        return Err(self.err_unexpected(&t2, &["'}'"]));
                    }
                    self.next()?;
                    if let Expr::String(s) = self.exprs.get(e) {
                        let s = s.clone();
                        (self.symbols.create(&s), open.begin)
                    } else {
                        return Err(ParseError::new(
                            "dynamic attributes not allowed in inherit",
                            self.at(open.begin),
                        ));
                    }
                }
                TokKind::Char(b';') => {
                    self.next()?;
                    break;
                }
                _ => return Err(self.err_unexpected(&t, &[])),
            };
            let i_pos = self.at(i_begin);
            if let Some(prev) = self.exprs.attrs(attrs).attrs.get(&sym) {
                return Err(self.dup_attr_sym(sym, i_pos, prev.pos));
            }
            let def = match from {
                None => {
                    let v = self.exprs.add(Expr::Var {
                        pos: i_pos,
                        name: sym,
                    });
                    AttrDef {
                        kind: AttrDefKind::Inherited,
                        e: v,
                        pos: i_pos,
                    }
                }
                Some(from) => {
                    let sel = self.exprs.add(Expr::Select {
                        pos: i_pos,
                        e: from,
                        attrpath: vec![AttrName::sym(sym)],
                        def: None,
                    });
                    AttrDef {
                        kind: AttrDefKind::InheritedFrom,
                        e: sel,
                        pos: i_pos,
                    }
                }
            };
            self.exprs.attrs_mut(attrs).attrs.insert(sym, def);
        }
        Ok(())
    }

    // ---------- addAttr (parser-state.hh) ----------

    fn dup_attr_sym(&self, sym: Symbol, pos: PosIdx, prev_pos: PosIdx) -> ParseError {
        let name = crate::show::print_identifier_str(self.symbols.resolve(sym));
        let prev = self
            .positions
            .lookup(prev_pos)
            .map(|p| p.to_string())
            .unwrap_or_else(|| "«none»".into());
        ParseError::new(
            format!("attribute '{name}' already defined at {prev}"),
            pos,
        )
    }

    fn dup_attr_path(&self, path: &[AttrName], pos: PosIdx, prev_pos: PosIdx) -> ParseError {
        let shown = crate::show::show_attr_selection_path(self.exprs, self.symbols, path);
        let prev = self
            .positions
            .lookup(prev_pos)
            .map(|p| p.to_string())
            .unwrap_or_else(|| "«none»".into());
        ParseError::new(
            format!("attribute '{shown}' already defined at {prev}"),
            pos,
        )
    }

    fn add_attr(
        &mut self,
        attrs: ExprId,
        mut attr_path: Vec<AttrName>,
        loc_begin: u32,
        e: ExprId,
    ) -> Result<(), ParseError> {
        assert!(!attr_path.is_empty());
        let pos = self.at(loc_begin);
        let mut cur = attrs;
        let mut i = 0;
        while i + 1 < attr_path.len() {
            let an = attr_path[i];
            if an.symbol.is_set() {
                if let Some(j) = self.exprs.attrs(cur).attrs.get(&an.symbol).copied() {
                    if matches!(self.exprs.get(j.e), Expr::Attrs(_)) {
                        cur = j.e;
                    } else {
                        attr_path.truncate(i + 1);
                        return Err(self.dup_attr_path(&attr_path, pos, j.pos));
                    }
                } else {
                    let nested = self.exprs.add(Expr::Attrs(ExprAttrs::default()));
                    self.exprs.attrs_mut(cur).attrs.insert(
                        an.symbol,
                        AttrDef {
                            kind: AttrDefKind::Plain,
                            e: nested,
                            pos,
                        },
                    );
                    cur = nested;
                }
            } else {
                let nested = self.exprs.add(Expr::Attrs(ExprAttrs::default()));
                self.exprs.attrs_mut(cur).dynamic_attrs.push(DynamicAttrDef {
                    name_expr: an.expr.unwrap(),
                    value_expr: nested,
                    pos,
                });
                cur = nested;
            }
            i += 1;
        }
        let last = attr_path[i];
        if last.symbol.is_set() {
            self.add_attr_def(
                cur,
                &mut attr_path,
                last.symbol,
                AttrDef {
                    kind: AttrDefKind::Plain,
                    e,
                    pos,
                },
            )
        } else {
            self.exprs.attrs_mut(cur).dynamic_attrs.push(DynamicAttrDef {
                name_expr: last.expr.unwrap(),
                value_expr: e,
                pos,
            });
            Ok(())
        }
    }

    /// The merging overload of addAttr. `attr_path` already contains
    /// `symbol` as its last element (used for error messages).
    fn add_attr_def(
        &mut self,
        attrs: ExprId,
        attr_path: &mut Vec<AttrName>,
        symbol: Symbol,
        def: AttrDef,
    ) -> Result<(), ParseError> {
        let existing = self.exprs.attrs(attrs).attrs.get(&symbol).copied();
        if let Some(j) = existing {
            let ae_is_attrs = matches!(self.exprs.get(def.e), Expr::Attrs(_));
            let j_is_attrs = matches!(self.exprs.get(j.e), Expr::Attrs(_));
            if ae_is_attrs && j_is_attrs {
                let j_attrs = j.e;
                let ae = def.e;
                let j_if_len = self.exprs.attrs(j_attrs).inherit_from_exprs.len() as u32;
                let ae_attrs: Vec<(Symbol, AttrDef)> = {
                    let a = self.exprs.attrs_mut(ae);
                    std::mem::take(&mut a.attrs).into_iter().collect()
                };
                for (name, ad) in ae_attrs {
                    if ad.kind == AttrDefKind::InheritedFrom {
                        let sel_e = match self.exprs.get(ad.e) {
                            Expr::Select { e, .. } => *e,
                            _ => unreachable!(),
                        };
                        match self.exprs.get_mut(sel_e) {
                            Expr::InheritFrom { displ, .. } => *displ += j_if_len,
                            _ => unreachable!(),
                        }
                    }
                    attr_path.push(AttrName::sym(name));
                    let r = self.add_attr_def(j_attrs, attr_path, name, ad);
                    attr_path.pop();
                    r?;
                }
                let dyns = std::mem::take(&mut self.exprs.attrs_mut(ae).dynamic_attrs);
                self.exprs.attrs_mut(j_attrs).dynamic_attrs.extend(dyns);
                let ifr = std::mem::take(&mut self.exprs.attrs_mut(ae).inherit_from_exprs);
                self.exprs
                    .attrs_mut(j_attrs)
                    .inherit_from_exprs
                    .extend(ifr);
                Ok(())
            } else {
                Err(self.dup_attr_path(attr_path, def.pos, j.pos))
            }
        } else {
            self.exprs.attrs_mut(attrs).attrs.insert(symbol, def);
            Ok(())
        }
    }

    // ---------- formals & the ambiguous '{' ----------

    /// After `'{'` at expr_function level: either a formal_set (lambda) or
    /// an attrset. This replicates the LALR automaton's commitment points.
    fn parse_brace_at_function_level(&mut self) -> Result<PExpr, ParseError> {
        let open = self.next()?; // '{'
        let t = self.peek()?.clone();
        match t.kind {
            TokKind::Char(b'}') => {
                self.next()?;
                let t2 = self.peek()?.clone();
                if Self::is_char(&t2, b':') || Self::is_char(&t2, b'@') {
                    let formals = Formals::default();
                    return self.parse_lambda_after_formal_set(open.begin, formals);
                }
                let pos = self.at(open.begin);
                let e = self.exprs.add(Expr::Attrs(ExprAttrs {
                    pos,
                    ..Default::default()
                }));
                self.continue_from_simple((e, open.begin))
            }
            TokKind::Ellipsis => {
                self.next()?;
                let t2 = self.peek()?.clone();
                if !Self::is_char(&t2, b'}') {
                    return Err(self.err_unexpected(&t2, &["'}'"]));
                }
                self.next()?;
                let formals = Formals {
                    formals: vec![],
                    ellipsis: true,
                };
                self.parse_lambda_after_formal_set(open.begin, formals)
            }
            TokKind::Id => {
                let t2 = self.peek2()?.clone();
                if Self::is_char(&t2, b',') || Self::is_char(&t2, b'?') || Self::is_char(&t2, b'}')
                {
                    // Committed to formals.
                    let formals = self.parse_formals()?;
                    self.parse_lambda_after_formal_set(open.begin, formals)
                } else {
                    self.parse_brace_binds_tail(open.begin)
                }
            }
            TokKind::Inherit | TokKind::OrKw | TokKind::Char(b'"') | TokKind::DollarCurly => {
                self.parse_brace_binds_tail(open.begin)
            }
            _ => Err(self.err_unexpected(&t, &["'inherit'"])),
        }
    }

    fn parse_brace_binds_tail(&mut self, open_begin: u32) -> Result<PExpr, ParseError> {
        let attrs = self.parse_binds(BindsCtx::Brace)?;
        debug_assert!(Self::is_char(self.peek()?, b'}'));
        self.next()?;
        let pos = self.at(open_begin);
        self.exprs.attrs_mut(attrs).pos = pos;
        self.continue_from_simple((attrs, open_begin))
    }

    /// Parse the inside of a formal_set; the opening '{' has been consumed.
    /// Consumes the closing '}'.
    fn parse_formals(&mut self) -> Result<Formals, ParseError> {
        let mut formals = Formals::default();
        loop {
            let t = self.peek()?.clone();
            match t.kind {
                TokKind::Id => {
                    let id = self.next()?;
                    let name = self.symbols.create(&id.text);
                    let pos = self.at(id.begin);
                    let t2 = self.peek()?.clone();
                    let def = if Self::is_char(&t2, b'?') {
                        self.next()?;
                        let (d, _) = self.parse_expr()?;
                        Some(d)
                    } else {
                        None
                    };
                    formals.formals.push(Formal { pos, name, def });
                    let t3 = self.peek()?.clone();
                    if Self::is_char(&t3, b',') {
                        self.next()?;
                        continue;
                    }
                    if Self::is_char(&t3, b'}') {
                        self.next()?;
                        break;
                    }
                    return Err(self.err_unexpected(&t3, &["'}'", "','"]));
                }
                TokKind::Ellipsis => {
                    self.next()?;
                    formals.ellipsis = true;
                    let t2 = self.peek()?.clone();
                    if !Self::is_char(&t2, b'}') {
                        return Err(self.err_unexpected(&t2, &["'}'"]));
                    }
                    self.next()?;
                    break;
                }
                TokKind::Char(b'}') => {
                    self.next()?;
                    break;
                }
                _ => return Err(self.err_unexpected(&t, &["identifier", "'...'", "'}'"])),
            }
        }
        Ok(formals)
    }

    fn parse_lambda_after_formal_set(
        &mut self,
        begin: u32,
        mut formals: Formals,
    ) -> Result<PExpr, ParseError> {
        let t = self.peek()?.clone();
        if Self::is_char(&t, b':') {
            self.next()?;
            self.validate_formals(&mut formals, NO_POS, Symbol(0))?;
            let (body, _) = self.parse_expr()?;
            let e = self.exprs.add(Expr::Lambda(ExprLambda {
                pos: self.at(begin),
                name: Symbol(0),
                arg: Symbol(0),
                formals: Some(formals),
                body,
            }));
            return Ok((e, begin));
        }
        if Self::is_char(&t, b'@') {
            self.next()?;
            let t2 = self.peek()?.clone();
            if !matches!(t2.kind, TokKind::Id) {
                return Err(self.err_unexpected(&t2, &["identifier"]));
            }
            let id = self.next()?;
            let arg = self.symbols.create(&id.text);
            let t3 = self.peek()?.clone();
            if !Self::is_char(&t3, b':') {
                return Err(self.err_unexpected(&t3, &["':'"]));
            }
            self.next()?;
            self.validate_formals(&mut formals, self.at(begin), arg)?;
            let (body, _) = self.parse_expr()?;
            let e = self.exprs.add(Expr::Lambda(ExprLambda {
                pos: self.at(begin),
                name: Symbol(0),
                arg,
                formals: Some(formals),
                body,
            }));
            return Ok((e, begin));
        }
        Err(self.err_unexpected(&t, &["':'", "'@'"]))
    }

    /// Port of `ParserState::validateFormals`.
    fn validate_formals(
        &mut self,
        formals: &mut Formals,
        pos: PosIdx,
        arg: Symbol,
    ) -> Result<(), ParseError> {
        formals.formals.sort_by_key(|f| (f.name.0, f.pos.0));
        let mut duplicate: Option<(u32, u32)> = None;
        for i in 0..formals.formals.len().saturating_sub(1) {
            if formals.formals[i].name != formals.formals[i + 1].name {
                continue;
            }
            let this_dup = (formals.formals[i].name.0, formals.formals[i + 1].pos.0);
            duplicate = Some(match duplicate {
                Some(d) => d.min(this_dup),
                None => this_dup,
            });
        }
        if let Some((name, dpos)) = duplicate {
            let name = crate::show::print_identifier_str(self.symbols.resolve(Symbol(name)));
            return Err(ParseError::new(
                format!("duplicate formal function argument '{name}'"),
                PosIdx(dpos),
            ));
        }
        if arg.is_set() && formals.formals.iter().any(|f| f.name == arg) {
            let name = crate::show::print_identifier_str(self.symbols.resolve(arg));
            return Err(ParseError::new(
                format!("duplicate formal function argument '{name}'"),
                pos,
            ));
        }
        Ok(())
    }

    // ---------- helpers ----------

    /// ExprCall::resetCursedOr
    fn reset_cursed_or(&mut self, e: ExprId) {
        if let Expr::Call {
            cursed_or_end_pos, ..
        } = self.exprs.get_mut(e)
        {
            *cursed_or_end_pos = None;
        }
    }

    /// ExprCall::warnIfCursedOr
    fn warn_if_cursed_or(&mut self, e: ExprId) {
        let (pos, end) = match self.exprs.get(e) {
            Expr::Call {
                pos,
                cursed_or_end_pos: Some(end),
                ..
            } => (*pos, *end),
            _ => return,
        };
        let begin_off = self.positions.offset_of(pos).unwrap_or(0) as usize;
        let end_off = self.positions.offset_of(end).unwrap_or(0) as usize;
        let snippet = if begin_off <= end_off && end_off <= self.src.len() {
            String::from_utf8_lossy(&self.src[begin_off..end_off]).into_owned()
        } else {
            "could not read expression".into()
        };
        let at = self
            .positions
            .lookup(pos)
            .map(|p| p.to_string())
            .unwrap_or_else(|| "«none»".into());
        self.warnings.push(format!(
            "warning: at {at}: This expression uses `or` as an identifier in a way that will change in a future Nix release.\n\
             Wrap this entire expression in parentheses to preserve its current meaning:\n    \
             ({snippet})\n\
             Give feedback at https://github.com/NixOS/nix/pull/11121"
        ));
    }

    #[allow(non_snake_case)]
    fn var_noPos(&mut self, name: &str) -> ExprId {
        let sym = self.symbols.create(name.as_bytes());
        self.exprs.add(Expr::Var {
            pos: NO_POS,
            name: sym,
        })
    }
}

enum StringParse {
    Plain(Vec<u8>),
    Interp(ExprId),
}

enum StrPart {
    Str(Vec<u8>),
    Expr(ExprId),
}

enum IndPart {
    Str(Vec<u8>, bool),
    Expr(ExprId),
}

#[derive(PartialEq, Clone, Copy)]
enum Assoc {
    Left,
    Right,
    None,
}

/// Binary operator precedence, mirroring parser.y's precedence declarations
/// (higher binds tighter).
fn op_info(t: &TokKind) -> Option<(u8, Assoc)> {
    Some(match t {
        TokKind::Impl => (1, Assoc::Right),
        TokKind::Or => (2, Assoc::Left),
        TokKind::And => (3, Assoc::Left),
        TokKind::Eq | TokKind::NEq => (4, Assoc::None),
        TokKind::Char(b'<') | TokKind::Char(b'>') | TokKind::Leq | TokKind::Geq => {
            (5, Assoc::None)
        }
        TokKind::Update => (6, Assoc::Right),
        TokKind::Char(b'+') | TokKind::Char(b'-') => (8, Assoc::Left),
        TokKind::Char(b'*') | TokKind::Char(b'/') => (9, Assoc::Left),
        TokKind::Concat => (10, Assoc::Right),
        TokKind::Char(b'?') => (11, Assoc::None),
        _ => return None,
    })
}

fn starts_expr_select(t: &TokKind) -> bool {
    matches!(
        t,
        TokKind::Id
            | TokKind::Int(_)
            | TokKind::Float(_)
            | TokKind::Path
            | TokKind::HPath
            | TokKind::SPath
            | TokKind::Uri
            | TokKind::IndStringOpen
            | TokKind::Let
            | TokKind::Rec
            | TokKind::Char(b'"')
            | TokKind::Char(b'(')
            | TokKind::Char(b'{')
            | TokKind::Char(b'[')
    )
}

/// Lexical path canonicalization like `CanonPath(literal, base).abs()`:
/// resolve `.`, `..` and duplicate slashes against an absolute base
/// (empty base for absolute literals).
fn canon_path(literal: &[u8], base: &[u8]) -> Vec<u8> {
    let mut segs: Vec<&[u8]> = Vec::new();
    if literal.first() != Some(&b'/') {
        for seg in base.split(|&c| c == b'/') {
            push_seg(&mut segs, seg);
        }
    }
    for seg in literal.split(|&c| c == b'/') {
        push_seg(&mut segs, seg);
    }
    let mut out = Vec::new();
    if segs.is_empty() {
        out.push(b'/');
    } else {
        for s in segs {
            out.push(b'/');
            out.extend_from_slice(s);
        }
    }
    out
}

fn push_seg<'x>(segs: &mut Vec<&'x [u8]>, seg: &'x [u8]) {
    match seg {
        b"" | b"." => {}
        b".." => {
            segs.pop();
        }
        s => segs.push(s),
    }
}
