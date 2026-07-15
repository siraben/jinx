//! `printValueAsXML` (value-to-xml.cc) and the XMLWriter (xml-writer.cc).
//! Byte-exact with C++ Nix: indented, attribute values escaped for
//! `" < > & \n`, attributes emitted in sorted (lexicographic) order.

use std::collections::HashSet;

use jinx_syntax::pos::PosIdx;
use jinx_syntax::symbol::Symbol;

use crate::error::{ErrId, ErrKind};
use crate::print::fmt_f64_g6;
use crate::value::{Tag, VRef, Value};
use crate::vm::{attrs_entries, list_elems, path_bytes, str_bytes, thunk_code, val, VM};

struct Writer {
    out: Vec<u8>,
    depth: usize,
}

impl Writer {
    fn indent(&mut self) {
        for _ in 0..self.depth * 2 {
            self.out.push(b' ');
        }
    }
    fn write_attrs(&mut self, attrs: &mut [(Vec<u8>, Vec<u8>)]) {
        attrs.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, value) in attrs.iter() {
            self.out.push(b' ');
            self.out.extend_from_slice(name);
            self.out.extend_from_slice(b"=\"");
            escape(&mut self.out, value);
            self.out.push(b'"');
        }
    }
    fn open(&mut self, name: &str, mut attrs: Vec<(Vec<u8>, Vec<u8>)>) {
        self.indent();
        self.out.push(b'<');
        self.out.extend_from_slice(name.as_bytes());
        self.write_attrs(&mut attrs);
        self.out.push(b'>');
        self.out.push(b'\n');
        self.depth += 1;
    }
    fn close(&mut self, name: &str) {
        self.depth -= 1;
        self.indent();
        self.out.extend_from_slice(b"</");
        self.out.extend_from_slice(name.as_bytes());
        self.out.push(b'>');
        self.out.push(b'\n');
    }
    fn empty(&mut self, name: &str, mut attrs: Vec<(Vec<u8>, Vec<u8>)>) {
        self.indent();
        self.out.push(b'<');
        self.out.extend_from_slice(name.as_bytes());
        self.write_attrs(&mut attrs);
        self.out.extend_from_slice(b" />");
        self.out.push(b'\n');
    }
}

fn escape(out: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"&quot;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'&' => out.extend_from_slice(b"&amp;"),
            b'\n' => out.extend_from_slice(b"&#xA;"),
            _ => out.push(c),
        }
    }
}

fn attr(name: &str, value: &[u8]) -> (Vec<u8>, Vec<u8>) {
    (name.as_bytes().to_vec(), value.to_vec())
}

/// Produce the full XML document for `cell`, appending accumulated string
/// context ids to `ctx`.
pub fn value_to_xml(
    vm: &mut VM,
    cell: VRef,
    strict: bool,
    location: bool,
    out: &mut Vec<u8>,
    ctx: &mut Vec<u32>,
) -> Result<(), ErrId> {
    let mut w = Writer {
        out: Vec::new(),
        depth: 0,
    };
    w.out
        .extend_from_slice(b"<?xml version='1.0' encoding='utf-8'?>\n");
    w.open("expr", vec![]);
    let mut drvs_seen: HashSet<Vec<u8>> = HashSet::new();
    print_xml(vm, cell, strict, location, &mut w, ctx, &mut drvs_seen)?;
    w.close("expr");
    out.extend_from_slice(&w.out);
    Ok(())
}

fn merge_ctx(dst: &mut Vec<u32>, src: &[u32]) {
    for id in src {
        if !dst.contains(id) {
            dst.push(*id);
        }
    }
}

fn is_derivation(vm: &mut VM, v: &Value) -> bool {
    if v.tag() != Tag::Attrs {
        return false;
    }
    if let Some(a) = crate::vm::attrs_get(v, vm.syms.type_) {
        if vm.force(a.val, PosIdx(a.pos)).is_ok() {
            let tv = val(a.val);
            return tv.is_string() && str_bytes(&tv) == b"derivation";
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn print_xml(
    vm: &mut VM,
    cell: VRef,
    strict: bool,
    location: bool,
    w: &mut Writer,
    ctx: &mut Vec<u32>,
    drvs_seen: &mut HashSet<Vec<u8>>,
) -> Result<(), ErrId> {
    if strict {
        vm.force(cell, PosIdx(0))?;
    }
    let v = val(cell);
    match v.tag() {
        Tag::Int => w.empty("int", vec![attr("value", v.as_int().to_string().as_bytes())]),
        Tag::Float => w.empty(
            "float",
            vec![attr("value", fmt_f64_g6(v.as_float()).as_bytes())],
        ),
        Tag::True => w.empty("bool", vec![attr("value", b"true")]),
        Tag::False => w.empty("bool", vec![attr("value", b"false")]),
        Tag::String | Tag::SmallString => {
            let ids = vm.read_str_ctx(&v);
            merge_ctx(ctx, &ids);
            w.empty("string", vec![attr("value", str_bytes(&v))]);
        }
        Tag::Path => w.empty("path", vec![attr("value", path_bytes(&v))]),
        Tag::Null => w.empty("null", vec![]),
        Tag::List => {
            let elems = list_elems(&v).to_vec();
            w.open("list", vec![]);
            for el in &elems {
                print_xml(vm, *el, strict, location, w, ctx, drvs_seen)?;
            }
            w.close("list");
        }
        Tag::Attrs => {
            if is_derivation(vm, &v) {
                print_derivation(vm, cell, strict, location, w, ctx, drvs_seen)?;
            } else {
                w.open("attrs", vec![]);
                show_attrs(vm, &v, strict, location, w, ctx, drvs_seen)?;
                w.close("attrs");
            }
        }
        Tag::Closure | Tag::Closure0 | Tag::Closure1 => {
            print_function(vm, &v, location, w)?;
        }
        Tag::PrimOp | Tag::PrimOpApp => w.empty("unevaluated", vec![]),
        Tag::Thunk | Tag::Thunk0 | Tag::Thunk1 | Tag::Failed | Tag::Blackhole | Tag::Blackhole0 | Tag::Blackhole1 => {
            w.empty("unevaluated", vec![])
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn show_attrs(
    vm: &mut VM,
    v: &Value,
    strict: bool,
    location: bool,
    w: &mut Writer,
    ctx: &mut Vec<u32>,
    drvs_seen: &mut HashSet<Vec<u8>>,
) -> Result<(), ErrId> {
    // Lexicographic order by attribute name.
    let mut sorted: Vec<(Vec<u8>, VRef, u32)> = attrs_entries(v)
        .iter()
        .map(|a| (vm.symbols.resolve(Symbol(a.sym)).to_vec(), a.val, a.pos))
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, vc, apos) in &sorted {
        let mut xa = vec![attr("name", name)];
        if location {
            pos_to_xml(vm, PosIdx(*apos), &mut xa);
        }
        w.open("attr", xa);
        print_xml(vm, *vc, strict, location, w, ctx, drvs_seen)?;
        w.close("attr");
    }
    Ok(())
}

fn pos_to_xml(vm: &VM, pos: PosIdx, xa: &mut Vec<(Vec<u8>, Vec<u8>)>) {
    if let Some(p) = vm.positions.lookup(pos) {
        // C++ posToXML only writes `path` when the origin is a real file
        // (`std::get_if<SourcePath>`); for `-E`/stdin origins it emits just
        // line/column. Match that.
        let is_file = matches!(
            vm.positions.origin_of(pos),
            Some(jinx_syntax::pos::Origin::Path { .. })
        );
        let s = p.to_string();
        // p renders as "path:line:column" in jinx; split conservatively.
        if let Some((path, rest)) = s.rsplit_once(':').and_then(|(a, c)| {
            a.rsplit_once(':').map(|(p, l)| (p, (l, c)))
        }) {
            if is_file {
                xa.push(attr("path", path.as_bytes()));
            }
            xa.push(attr("line", rest.0.as_bytes()));
            xa.push(attr("column", rest.1.as_bytes()));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn print_derivation(
    vm: &mut VM,
    cell: VRef,
    strict: bool,
    location: bool,
    w: &mut Writer,
    ctx: &mut Vec<u32>,
    drvs_seen: &mut HashSet<Vec<u8>>,
) -> Result<(), ErrId> {
    let v = val(cell);
    let mut xa: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut drv_path: Vec<u8> = Vec::new();
    if let Some(a) = crate::vm::attrs_get(&v, vm.syms.drv_path) {
        if strict {
            let _ = vm.force(a.val, PosIdx(a.pos));
        }
        let av = val(a.val);
        if av.is_string() {
            drv_path = str_bytes(&av).to_vec();
            xa.push(attr("drvPath", &drv_path));
        }
    }
    if let Some(a) = crate::vm::attrs_get(&v, vm.syms.out_path) {
        if strict {
            let _ = vm.force(a.val, PosIdx(a.pos));
        }
        let av = val(a.val);
        if av.is_string() {
            xa.push(attr("outPath", str_bytes(&av)));
        }
    }
    w.open("derivation", xa);
    if !drv_path.is_empty() && drvs_seen.insert(drv_path.clone()) {
        let v = val(cell);
        show_attrs(vm, &v, strict, location, w, ctx, drvs_seen)?;
    } else {
        w.empty("repeated", vec![]);
    }
    w.close("derivation");
    Ok(())
}

fn print_function(vm: &mut VM, v: &Value, location: bool, w: &mut Writer) -> Result<(), ErrId> {
    let (code, _) = thunk_code(v);
    let chunk = code.chunk();
    let lambda = match &chunk.lambda {
        Some(l) => l,
        None => {
            w.empty("unevaluated", vec![]);
            return Ok(());
        }
    };
    let mut fattrs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    if location {
        pos_to_xml(vm, chunk.pos, &mut fattrs);
    }
    w.open("function", fattrs);
    match &lambda.formals {
        Some(formals) => {
            let mut sa: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            if lambda.arg.0 != 0 {
                sa.push(attr("name", vm.symbols.resolve(lambda.arg)));
            }
            if formals.ellipsis {
                sa.push(attr("ellipsis", b"1"));
            }
            w.open("attrspat", sa);
            // Formals in lexicographic order by name.
            let mut names: Vec<Vec<u8>> = formals
                .formals
                .iter()
                .map(|f| vm.symbols.resolve(f.name).to_vec())
                .collect();
            names.sort();
            for n in &names {
                w.empty("attr", vec![attr("name", n)]);
            }
            w.close("attrspat");
        }
        None => {
            w.empty("varpat", vec![attr("name", vm.symbols.resolve(lambda.arg))]);
        }
    }
    w.close("function");
    Ok(())
}

/// `builtins.toXML`: strict, no location, result string carries the
/// accumulated context.
pub fn prim_to_xml_impl(vm: &mut VM, cell: VRef) -> Result<(Vec<u8>, Vec<u32>), ErrId> {
    let mut out = Vec::new();
    let mut ctx = Vec::new();
    value_to_xml(vm, cell, true, false, &mut out, &mut ctx)?;
    let _ = ErrKind::Eval;
    Ok((out, ctx))
}
