//! Value printers:
//! - `print_ambiguous`: what `nix-instantiate --eval [--strict]` writes to
//!   stdout (print-ambiguous.cc).
//! - `print_value`: the `ValuePrinter` used by `builtins.trace` and error
//!   messages (print.cc), sans ANSI colors.
//! - `deep_force`: `forceValueDeep` with a seen-set for cyclic structures.
//! - `fmt_f64_g6`: C++ `ostream << double` (`%.6g`) semantics via snprintf.

use std::collections::HashSet;

use jinx_syntax::pos::{PosIdx, NO_POS};
use jinx_syntax::symbol::Symbol;

use crate::error::{ErrId, ErrKind};
use crate::value::{Tag, VRef, Value};
use crate::vm::{
    attrs_entries, list_elems, path_bytes, primapp_parts, primop_of, str_bytes, thunk_code, val,
    VM,
};

/// `%.6g` like `std::ostream << double`.
pub fn fmt_f64_g6(f: f64) -> String {
    let mut buf = [0u8; 64];
    // SAFETY: buffer large enough for %g output.
    let n = unsafe {
        libc::snprintf(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            c"%.6g".as_ptr(),
            f,
        )
    };
    String::from_utf8_lossy(&buf[..n as usize]).into_owned()
}

/// print.cc printLiteralString.
pub fn print_literal_string(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
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
                out.push(b'$');
            }
            _ => out.push(c),
        }
        i += 1;
    }
    out.push(b'"');
}

fn is_reserved_keyword(s: &[u8]) -> bool {
    matches!(
        s,
        b"if" | b"then" | b"else" | b"assert" | b"with" | b"let" | b"in" | b"rec" | b"inherit"
    )
}

fn is_var_name(s: &[u8]) -> bool {
    if s.is_empty() || is_reserved_keyword(s) {
        return false;
    }
    let c = s[0];
    if c.is_ascii_digit() || c == b'-' || c == b'\'' {
        return false;
    }
    s.iter().all(|&i| {
        i.is_ascii_lowercase()
            || i.is_ascii_uppercase()
            || i.is_ascii_digit()
            || i == b'_'
            || i == b'-'
            || i == b'\''
    })
}

/// print.cc printAttributeName.
pub fn print_attribute_name(out: &mut Vec<u8>, name: &[u8]) {
    if is_var_name(name) {
        out.extend_from_slice(name);
    } else {
        print_literal_string(out, name);
    }
}

// ---------------- deep force (forceValueDeep) ----------------

pub fn deep_force(vm: &mut VM, cell: VRef) -> Result<(), ErrId> {
    let mut seen: HashSet<usize> = HashSet::new();
    deep_force_rec(vm, cell, &mut seen)
}

fn deep_force_rec(vm: &mut VM, cell: VRef, seen: &mut HashSet<usize>) -> Result<(), ErrId> {
    // C++ `forceValueDeep` accounts each level against the shared call-depth
    // counter (via `addCallDepth`), so a genuine stack overflow surfaces at
    // the same place — and with the same position — as function-call depth.
    vm.depth_check(NO_POS)?;
    vm.call_depth += 1;
    let r = deep_force_body(vm, cell, seen);
    vm.call_depth -= 1;
    r
}

fn deep_force_body(vm: &mut VM, cell: VRef, seen: &mut HashSet<usize>) -> Result<(), ErrId> {
    vm.force(cell, NO_POS)?;
    let v = val(cell);
    match v.tag() {
        Tag::Attrs => {
            // Only recursive container values can lead back into the graph.
            // Avoid hashing every scalar leaf reached by deepSeq.
            if !seen.insert(cell.as_ptr() as usize) {
                return Ok(());
            }
            for a in attrs_entries(&v) {
                deep_force_rec(vm, a.val, seen).map_err(|e| {
                    let name =
                        vm.symbols.resolve_str_lossy(Symbol(a.sym));
                    vm.add_trace(
                        e,
                        PosIdx(a.pos),
                        format!("while evaluating the attribute '{name}'"),
                    );
                    e
                })?;
            }
        }
        Tag::List => {
            if !seen.insert(cell.as_ptr() as usize) {
                return Ok(());
            }
            for (i, &el) in list_elems(&v).iter().enumerate() {
                deep_force_rec(vm, el, seen).map_err(|e| {
                    vm.add_trace(
                        e,
                        NO_POS,
                        format!("while evaluating list element at index {i}"),
                    );
                    e
                })?;
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------- printAmbiguous ----------------

pub fn print_ambiguous(vm: &mut VM, cell: VRef, out: &mut Vec<u8>) -> Result<(), ErrId> {
    let mut seen: HashSet<usize> = HashSet::new();
    print_ambiguous_rec(vm, cell, out, &mut seen, 0)
}

fn print_ambiguous_rec(
    vm: &mut VM,
    cell: VRef,
    out: &mut Vec<u8>,
    seen: &mut HashSet<usize>,
    depth: usize,
) -> Result<(), ErrId> {
    if depth > vm.max_call_depth {
        return Err(vm.new_err(
            ErrKind::StackOverflow,
            "stack overflow; max-call-depth exceeded",
            NO_POS,
        ));
    }
    let v = val(cell);
    match v.tag() {
        Tag::Int => out.extend_from_slice(v.as_int().to_string().as_bytes()),
        Tag::Float => out.extend_from_slice(fmt_f64_g6(v.as_float()).as_bytes()),
        Tag::True => out.extend_from_slice(b"true"),
        Tag::False => out.extend_from_slice(b"false"),
        Tag::String | Tag::SmallString => print_literal_string(out, str_bytes(&v)),
        Tag::Path => out.extend_from_slice(path_bytes(&v)),
        Tag::Null => out.extend_from_slice(b"null"),
        Tag::Attrs => {
            let entries = attrs_entries(&v);
            // seen-key: the bindings data object.
            if !entries.is_empty() && !seen.insert(v.ptr() as usize) {
                out.extend_from_slice("«repeated»".as_bytes());
            } else {
                out.extend_from_slice(b"{ ");
                let mut sorted: Vec<(Vec<u8>, VRef)> = entries
                    .iter()
                    .map(|a| (vm.symbols.resolve(Symbol(a.sym)).to_vec(), a.val))
                    .collect();
                sorted.sort_by(|x, y| x.0.cmp(&y.0));
                for (name, vc) in sorted {
                    // C++ streams SymbolStr, which prints via printIdentifier.
                    jinx_syntax::show::print_identifier(out, &name);
                    out.extend_from_slice(b" = ");
                    print_ambiguous_rec(vm, vc, out, seen, depth + 1)?;
                    out.extend_from_slice(b"; ");
                }
                out.push(b'}');
            }
        }
        Tag::List => {
            let elems = list_elems(&v);
            // seen-key: the value CELL (C++ uses &v).
            if !elems.is_empty() && !seen.insert(cell.as_ptr() as usize) {
                out.extend_from_slice("«repeated»".as_bytes());
            } else {
                out.extend_from_slice(b"[ ");
                for &el in elems {
                    print_ambiguous_rec(vm, el, out, seen, depth + 1)?;
                    out.push(b' ');
                }
                out.push(b']');
            }
        }
        Tag::Thunk | Tag::Thunk0 | Tag::Thunk1 => out.extend_from_slice(b"<CODE>"),
        Tag::Blackhole | Tag::Blackhole0 | Tag::Blackhole1 => {
            out.extend_from_slice("«potential infinite recursion»".as_bytes())
        }
        Tag::Failed => out.extend_from_slice(b"<CODE>"),
        Tag::Closure | Tag::Closure0 | Tag::Closure1 => out.extend_from_slice(b"<LAMBDA>"),
        Tag::PrimOp => out.extend_from_slice(b"<PRIMOP>"),
        Tag::PrimOpApp => out.extend_from_slice(b"<PRIMOP-APP>"),
    }
    Ok(())
}

// ---------------- ValuePrinter (trace / error messages) ----------------

pub struct PrintOptions {
    pub force: bool,
    pub max_depth: usize,
    pub max_attrs: usize,
    pub max_list_items: usize,
    pub max_string_length: usize,
}

impl Default for PrintOptions {
    fn default() -> Self {
        PrintOptions {
            force: false,
            max_depth: usize::MAX,
            max_attrs: usize::MAX,
            max_list_items: usize::MAX,
            max_string_length: usize::MAX,
        }
    }
}

/// errorPrintOptions (print-options.hh), minus ansi colors.
pub fn error_print_options() -> PrintOptions {
    PrintOptions {
        force: false,
        max_depth: 10,
        max_attrs: 10,
        max_list_items: 10,
        max_string_length: 1024,
    }
}

/// Print a value the way error messages embed it.
pub fn print_value_err(vm: &VM, v: &Value) -> String {
    let mut out = Vec::new();
    let mut st = PrinterState {
        seen: HashSet::new(),
        total_attrs: 0,
        total_list_items: 0,
    };
    print_value_rec(vm, v, &mut out, &error_print_options(), &mut st, 0);
    String::from_utf8_lossy(&out).into_owned()
}

/// `builtins.trace` non-string printing (default PrintOptions).
pub fn print_value_trace(vm: &VM, v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    let mut st = PrinterState {
        seen: HashSet::new(),
        total_attrs: 0,
        total_list_items: 0,
    };
    print_value_rec(vm, v, &mut out, &PrintOptions::default(), &mut st, 0);
    out
}

struct PrinterState {
    seen: HashSet<usize>,
    total_attrs: usize,
    total_list_items: usize,
}

fn pluralize(out: &mut Vec<u8>, n: usize, single: &str, plural: &str) {
    if n == 1 {
        out.extend_from_slice(format!("{n} {single}").as_bytes());
    } else {
        out.extend_from_slice(format!("{n} {plural}").as_bytes());
    }
}

fn print_elided(out: &mut Vec<u8>, n: usize, single: &str, plural: &str) {
    out.extend_from_slice("«".as_bytes());
    pluralize(out, n, single, plural);
    out.extend_from_slice(" elided»".as_bytes());
}

fn print_value_rec(
    vm: &VM,
    v: &Value,
    out: &mut Vec<u8>,
    opts: &PrintOptions,
    st: &mut PrinterState,
    depth: usize,
) {
    match v.tag() {
        Tag::Int => out.extend_from_slice(v.as_int().to_string().as_bytes()),
        Tag::Float => out.extend_from_slice(fmt_f64_g6(v.as_float()).as_bytes()),
        Tag::True => out.extend_from_slice(b"true"),
        Tag::False => out.extend_from_slice(b"false"),
        Tag::Null => out.extend_from_slice(b"null"),
        Tag::String | Tag::SmallString => {
            let s = str_bytes(v);
            if s.len() > opts.max_string_length {
                let mut trunc = s[..opts.max_string_length].to_vec();
                // printLiteralString prints '"' then elided note.
                let mut tmp = Vec::new();
                print_literal_string(&mut tmp, &trunc);
                tmp.pop(); // closing quote
                out.extend_from_slice(&tmp);
                out.extend_from_slice(b"\" ");
                print_elided(out, s.len() - opts.max_string_length, "byte", "bytes");
                trunc.clear();
            } else {
                print_literal_string(out, s);
            }
        }
        Tag::Path => out.extend_from_slice(path_bytes(v)),
        Tag::Attrs => {
            let entries = attrs_entries(v);
            if !st.seen.insert(v.ptr() as usize) {
                out.extend_from_slice("«repeated»".as_bytes());
                return;
            }
            if depth < opts.max_depth {
                out.push(b'{');
                let mut sorted: Vec<(Vec<u8>, VRef)> = entries
                    .iter()
                    .map(|a| (vm.symbols.resolve(Symbol(a.sym)).to_vec(), a.val))
                    .collect();
                sorted.sort_by(|x, y| x.0.cmp(&y.0));
                let mut current = 0usize;
                for (name, vc) in &sorted {
                    out.push(b' ');
                    if st.total_attrs >= opts.max_attrs {
                        print_elided(out, sorted.len() - current, "attribute", "attributes");
                        break;
                    }
                    print_attribute_name(out, name);
                    out.extend_from_slice(b" = ");
                    print_value_rec(vm, &val(*vc), out, opts, st, depth + 1);
                    out.push(b';');
                    st.total_attrs += 1;
                    current += 1;
                }
                out.extend_from_slice(b" }");
            } else {
                out.extend_from_slice(b"{ ... }");
            }
        }
        Tag::List => {
            let elems = list_elems(v);
            if !elems.is_empty() && !st.seen.insert(v.ptr() as usize) {
                out.extend_from_slice("«repeated»".as_bytes());
                return;
            }
            if depth < opts.max_depth {
                out.push(b'[');
                let mut current = 0usize;
                for &el in elems {
                    out.push(b' ');
                    if st.total_list_items >= opts.max_list_items {
                        print_elided(out, elems.len() - current, "item", "items");
                        break;
                    }
                    print_value_rec(vm, &val(el), out, opts, st, depth + 1);
                    st.total_list_items += 1;
                    current += 1;
                }
                out.extend_from_slice(b" ]");
            } else {
                out.extend_from_slice(b"[ ... ]");
            }
        }
        Tag::Closure | Tag::Closure0 | Tag::Closure1 => {
            out.extend_from_slice("«lambda".as_bytes());
            let (code, _) = thunk_code(v);
            let chunk = code.chunk();
            if chunk.name.is_set() {
                out.push(b' ');
                out.extend_from_slice(vm.symbols.resolve(chunk.name));
            }
            if let Some(p) = vm.positions.lookup(chunk.pos) {
                out.extend_from_slice(format!(" @ {p}").as_bytes());
            }
            out.extend_from_slice("»".as_bytes());
        }
        Tag::PrimOp => {
            out.extend_from_slice(
                format!("«primop {}»", primop_of(v).display()).as_bytes(),
            );
        }
        Tag::PrimOpApp => {
            out.extend_from_slice(
                format!(
                    "«partially applied primop {}»",
                    primapp_parts(v).0.display()
                )
                .as_bytes(),
            );
        }
        Tag::Blackhole | Tag::Blackhole0 | Tag::Blackhole1 => {
            out.extend_from_slice("«potential infinite recursion»".as_bytes())
        }
        Tag::Thunk | Tag::Thunk0 | Tag::Thunk1 | Tag::Failed => {
            out.extend_from_slice("«thunk»".as_bytes())
        }
    }
}
