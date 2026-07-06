//! `Expr::show`, byte-exact port of nixexpr.cc + print.cc helpers.

use std::collections::BTreeMap;

use crate::ast::*;
use crate::symbol::{Symbol, SymbolTable};

pub fn show(exprs: &Exprs, symbols: &SymbolTable, root: ExprId) -> Vec<u8> {
    let mut out = Vec::new();
    show_expr(exprs, symbols, root, &mut out);
    out
}

fn is_reserved_keyword(s: &[u8]) -> bool {
    matches!(
        s,
        b"if" | b"then" | b"else" | b"assert" | b"with" | b"let" | b"in" | b"rec" | b"inherit"
    )
}

/// print.cc printLiteralString
pub fn print_literal_string(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for (i, &c) in s.iter().enumerate() {
        match c {
            b'"' | b'\\' => {
                out.push(b'\\');
                out.push(c);
            }
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'$' if s.get(i + 1) == Some(&b'{') => {
                out.push(b'\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push(b'"');
}

/// print.cc printIdentifier
pub fn print_identifier(out: &mut Vec<u8>, s: &[u8]) {
    if s.is_empty() {
        out.extend_from_slice(b"\"\"");
    } else if is_reserved_keyword(s) {
        out.push(b'"');
        out.extend_from_slice(s);
        out.push(b'"');
    } else {
        let c = s[0];
        let ok_start = c.is_ascii_alphabetic() || c == b'_';
        let ok_rest = s
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'\'' | b'-'));
        if ok_start && ok_rest {
            out.extend_from_slice(s);
        } else {
            print_literal_string(out, s);
        }
    }
}

pub fn print_identifier_str(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    print_identifier(&mut out, s);
    out
}

/// nixexpr.cc showAttrSelectionPath
pub fn show_attr_selection_path(
    exprs: &Exprs,
    symbols: &SymbolTable,
    path: &[AttrName],
) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, an) in path.iter().enumerate() {
        if i > 0 {
            out.push(b'.');
        }
        if an.symbol.is_set() {
            print_identifier(&mut out, symbols.resolve(an.symbol));
        } else {
            out.extend_from_slice(b"\"${");
            show_expr(exprs, symbols, an.expr.unwrap(), &mut out);
            out.extend_from_slice(b"}\"");
        }
    }
    out
}

/// C++ `std::ostream << double` (defaultfloat, precision 6), i.e. printf %g.
fn show_float(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e16 {
        // %g drops the trailing ".0"; mimic via the general path below too,
        // but fast-path integral values within precision.
        let mag = if v == 0.0 {
            0
        } else {
            v.abs().log10().floor() as i32
        };
        if mag < 6 {
            return format!("{}", v.trunc() as i64);
        }
    }
    // %.6g
    let mut s = format!("{v:.5e}"); // 6 significant digits
    // parse "d.dddddde<exp>"
    if let Some(epos) = s.find('e') {
        let exp: i32 = s[epos + 1..].parse().unwrap();
        let mantissa = s[..epos].to_string();
        if (-5..6).contains(&exp) {
            // fixed notation with 6 significant digits
            let prec = (5 - exp).max(0) as usize;
            s = format!("{v:.prec$}");
            if s.contains('.') {
                s = s.trim_end_matches('0').trim_end_matches('.').to_string();
            }
        } else {
            let m = mantissa.trim_end_matches('0').trim_end_matches('.');
            s = format!("{}e{}{:02}", m, if exp < 0 { "-" } else { "+" }, exp.abs());
        }
    }
    s
}

fn show_expr(exprs: &Exprs, symbols: &SymbolTable, id: ExprId, out: &mut Vec<u8>) {
    match exprs.get(id) {
        Expr::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Expr::Float(f) => out.extend_from_slice(show_float(*f).as_bytes()),
        Expr::String(s) => print_literal_string(out, s),
        Expr::Path(p) => out.extend_from_slice(p),
        Expr::Var { name, .. } => print_identifier(out, symbols.resolve(*name)),
        Expr::InheritFrom { .. } => {}
        Expr::Select {
            e, attrpath, def, ..
        } => {
            out.push(b'(');
            show_expr(exprs, symbols, *e, out);
            out.extend_from_slice(b").");
            out.extend_from_slice(&show_attr_selection_path(exprs, symbols, attrpath));
            if let Some(def) = def {
                out.extend_from_slice(b" or (");
                show_expr(exprs, symbols, *def, out);
                out.push(b')');
            }
        }
        Expr::OpHasAttr { e, attrpath } => {
            out.extend_from_slice(b"((");
            show_expr(exprs, symbols, *e, out);
            out.extend_from_slice(b") ? ");
            out.extend_from_slice(&show_attr_selection_path(exprs, symbols, attrpath));
            out.push(b')');
        }
        Expr::Attrs(a) => {
            if a.recursive {
                out.extend_from_slice(b"rec ");
            }
            out.extend_from_slice(b"{ ");
            show_bindings(exprs, symbols, a, out);
            out.push(b'}');
        }
        Expr::List(elems) => {
            out.extend_from_slice(b"[ ");
            for e in elems {
                out.push(b'(');
                show_expr(exprs, symbols, *e, out);
                out.extend_from_slice(b") ");
            }
            out.push(b']');
        }
        Expr::Lambda(l) => {
            out.push(b'(');
            if let Some(formals) = &l.formals {
                out.extend_from_slice(b"{ ");
                let mut first = true;
                // lexicographic order by name string
                let mut sorted: Vec<&Formal> = formals.formals.iter().collect();
                sorted.sort_by(|a, b| symbols.resolve(a.name).cmp(symbols.resolve(b.name)));
                for f in sorted {
                    if first {
                        first = false;
                    } else {
                        out.extend_from_slice(b", ");
                    }
                    print_identifier(out, symbols.resolve(f.name));
                    if let Some(def) = f.def {
                        out.extend_from_slice(b" ? ");
                        show_expr(exprs, symbols, def, out);
                    }
                }
                if formals.ellipsis {
                    if !first {
                        out.extend_from_slice(b", ");
                    }
                    out.extend_from_slice(b"...");
                }
                out.extend_from_slice(b" }");
                if l.arg.is_set() {
                    out.extend_from_slice(b" @ ");
                }
            }
            if l.arg.is_set() {
                print_identifier(out, symbols.resolve(l.arg));
            }
            out.extend_from_slice(b": ");
            show_expr(exprs, symbols, l.body, out);
            out.push(b')');
        }
        Expr::Call { fun, args, .. } => {
            out.push(b'(');
            show_expr(exprs, symbols, *fun, out);
            for a in args {
                out.push(b' ');
                show_expr(exprs, symbols, *a, out);
            }
            out.push(b')');
        }
        Expr::Let { attrs, body } => {
            out.extend_from_slice(b"(let ");
            let a = exprs.attrs(*attrs);
            show_bindings(exprs, symbols, a, out);
            out.extend_from_slice(b"in ");
            show_expr(exprs, symbols, *body, out);
            out.push(b')');
        }
        Expr::With { attrs, body, .. } => {
            out.extend_from_slice(b"(with ");
            show_expr(exprs, symbols, *attrs, out);
            out.extend_from_slice(b"; ");
            show_expr(exprs, symbols, *body, out);
            out.push(b')');
        }
        Expr::If {
            cond, then, else_, ..
        } => {
            out.extend_from_slice(b"(if ");
            show_expr(exprs, symbols, *cond, out);
            out.extend_from_slice(b" then ");
            show_expr(exprs, symbols, *then, out);
            out.extend_from_slice(b" else ");
            show_expr(exprs, symbols, *else_, out);
            out.push(b')');
        }
        Expr::Assert { cond, body, .. } => {
            out.extend_from_slice(b"assert ");
            show_expr(exprs, symbols, *cond, out);
            out.extend_from_slice(b"; ");
            show_expr(exprs, symbols, *body, out);
        }
        Expr::OpNot(e) => {
            out.extend_from_slice(b"(! ");
            show_expr(exprs, symbols, *e, out);
            out.push(b')');
        }
        Expr::OpEq(a, b) => bin_op(exprs, symbols, *a, "==", *b, out),
        Expr::OpNEq(a, b) => bin_op(exprs, symbols, *a, "!=", *b, out),
        Expr::OpAnd(_, a, b) => bin_op(exprs, symbols, *a, "&&", *b, out),
        Expr::OpOr(_, a, b) => bin_op(exprs, symbols, *a, "||", *b, out),
        Expr::OpImpl(_, a, b) => bin_op(exprs, symbols, *a, "->", *b, out),
        Expr::OpUpdate(_, a, b) => bin_op(exprs, symbols, *a, "//", *b, out),
        Expr::OpConcatLists(_, a, b) => bin_op(exprs, symbols, *a, "++", *b, out),
        Expr::ConcatStrings { es, .. } => {
            out.push(b'(');
            for (i, (_, e)) in es.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b" + ");
                }
                show_expr(exprs, symbols, *e, out);
            }
            out.push(b')');
        }
        Expr::CurPos(_) => out.extend_from_slice(b"__curPos"),
    }
}

fn bin_op(exprs: &Exprs, symbols: &SymbolTable, a: ExprId, op: &str, b: ExprId, out: &mut Vec<u8>) {
    out.push(b'(');
    show_expr(exprs, symbols, a, out);
    out.push(b' ');
    out.extend_from_slice(op.as_bytes());
    out.push(b' ');
    show_expr(exprs, symbols, b, out);
    out.push(b')');
}

/// ExprAttrs::showBindings
fn show_bindings(exprs: &Exprs, symbols: &SymbolTable, a: &ExprAttrs, out: &mut Vec<u8>) {
    let mut sorted: Vec<(&Symbol, &AttrDef)> = a.attrs.iter().collect();
    sorted.sort_by(|x, y| symbols.resolve(*x.0).cmp(symbols.resolve(*y.0)));

    let mut inherits: Vec<Symbol> = Vec::new();
    // grouped by the displacement of the `inherit (from)` expression
    let mut inherits_from: BTreeMap<u32, Vec<Symbol>> = BTreeMap::new();
    for (sym, def) in &sorted {
        match def.kind {
            AttrDefKind::Plain => {}
            AttrDefKind::Inherited => inherits.push(**sym),
            AttrDefKind::InheritedFrom => {
                let sel_e = match exprs.get(def.e) {
                    Expr::Select { e, .. } => *e,
                    _ => unreachable!(),
                };
                let displ = match exprs.get(sel_e) {
                    Expr::InheritFrom { displ, .. } => *displ,
                    _ => unreachable!(),
                };
                inherits_from.entry(displ).or_default().push(**sym);
            }
        }
    }
    if !inherits.is_empty() {
        out.extend_from_slice(b"inherit");
        for sym in &inherits {
            out.push(b' ');
            print_identifier(out, symbols.resolve(*sym));
        }
        out.extend_from_slice(b"; ");
    }
    for (from, syms) in &inherits_from {
        out.extend_from_slice(b"inherit (");
        show_expr(exprs, symbols, a.inherit_from_exprs[*from as usize], out);
        out.push(b')');
        for sym in syms {
            out.push(b' ');
            print_identifier(out, symbols.resolve(*sym));
        }
        out.extend_from_slice(b"; ");
    }
    for (sym, def) in &sorted {
        if def.kind == AttrDefKind::Plain {
            print_identifier(out, symbols.resolve(**sym));
            out.extend_from_slice(b" = ");
            show_expr(exprs, symbols, def.e, out);
            out.extend_from_slice(b"; ");
        }
    }
    for d in &a.dynamic_attrs {
        out.extend_from_slice(b"\"${");
        show_expr(exprs, symbols, d.name_expr, out);
        out.extend_from_slice(b"}\" = ");
        show_expr(exprs, symbols, d.value_expr, out);
        out.extend_from_slice(b"; ");
    }
}
