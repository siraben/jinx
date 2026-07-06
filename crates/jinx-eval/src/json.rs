//! toJSON / fromJSON, matching value-to-json.cc + nlohmann::json::dump()
//! byte-for-byte (notably float formatting: std::to_chars shortest
//! round-trip, with ".0" appended to integral results).

use jinx_syntax::pos::PosIdx;
use jinx_syntax::symbol::Symbol;

use crate::error::{ErrId, ErrKind};
use crate::value::{Tag, VRef, Value};
use crate::vm::{attrs_entries, list_elems, str_bytes, val, VM};

/// `std::to_chars(double)`: shortest round-trip digits, fixed vs scientific
/// whichever is shorter (fixed wins ties), printf-style exponent (e+05).
pub fn to_chars_shortest(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let neg = f < 0.0;
    let a = f.abs();
    // Rust's LowerExp is shortest round-trip: "d.dddde[-]X".
    let s = format!("{a:e}");
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let n = digits.len() as i32;

    // Fixed representation.
    let fixed = if exp >= n - 1 {
        // digits followed by zeros
        let mut t = digits.clone();
        t.extend(std::iter::repeat_n('0', (exp - (n - 1)) as usize));
        t
    } else if exp >= 0 {
        let (int_part, frac) = digits.split_at((exp + 1) as usize);
        format!("{int_part}.{frac}")
    } else {
        let zeros: String = std::iter::repeat_n('0', (-exp - 1) as usize).collect();
        format!("0.{zeros}{digits}")
    };

    // Scientific representation (printf: >= 2 exponent digits).
    let sci_mant = if digits.len() == 1 {
        digits.clone()
    } else {
        format!("{}.{}", &digits[..1], &digits[1..])
    };
    let sci = format!(
        "{}e{}{:02}",
        sci_mant,
        if exp < 0 { '-' } else { '+' },
        exp.abs()
    );

    let body = if fixed.len() <= sci.len() { fixed } else { sci };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// nlohmann dump() float serialization.
fn json_float(f: f64) -> String {
    if !f.is_finite() {
        return "null".into();
    }
    let s = to_chars_shortest(f);
    if s.contains('.') || s.contains('e') {
        s
    } else {
        format!("{s}.0")
    }
}

/// nlohmann-style string escaping (control chars as \uXXXX).
fn json_string(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c if c < 0x20 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            c => out.push(c),
        }
    }
    out.push(b'"');
}

pub fn to_json(vm: &mut VM, cell: VRef, pos: PosIdx, out: &mut Vec<u8>) -> Result<(), ErrId> {
    vm.force(cell, pos)?;
    let v = val(cell);
    match v.tag() {
        Tag::Int => out.extend_from_slice(v.as_int().to_string().as_bytes()),
        Tag::Float => out.extend_from_slice(json_float(v.as_float()).as_bytes()),
        Tag::True => out.extend_from_slice(b"true"),
        Tag::False => out.extend_from_slice(b"false"),
        Tag::Null => out.extend_from_slice(b"null"),
        Tag::String => json_string(out, str_bytes(&v)),
        Tag::Path => {
            // C++ copies the path to the store here (copyToStore=true).
            let e = vm.new_err(
                ErrKind::Eval,
                "cannot copy paths to the Nix store in jinx M2 (builtins.toJSON of a path)",
                pos,
            );
            return Err(e);
        }
        Tag::Attrs => {
            // __toString / outPath handling.
            if let Some(f) = crate::vm::attrs_get(&v, vm.syms.to_string) {
                let r = vm.call_function(f.val, &[cell], pos)?;
                let rc = vm.alloc_cell(r);
                let scope = vm.temp_scope();
                vm.temp_roots.push(rc);
                let (s, _) = vm.coerce_to_string(
                    rc,
                    pos,
                    "while evaluating the result of the `__toString` attribute",
                    false,
                    false,
                    true,
                )?;
                vm.temp_end(scope);
                json_string(out, &s);
                return Ok(());
            }
            if let Some(op) = crate::vm::attrs_get(&v, vm.syms.out_path) {
                return to_json(vm, op.val, PosIdx(op.pos), out);
            }
            let mut sorted: Vec<(Vec<u8>, VRef, u32)> = attrs_entries(&v)
                .iter()
                .map(|a| (vm.symbols.resolve(Symbol(a.sym)).to_vec(), a.val, a.pos))
                .collect();
            sorted.sort_by(|x, y| x.0.cmp(&y.0));
            out.push(b'{');
            for (i, (name, vc, apos)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                json_string(out, name);
                out.push(b':');
                to_json(vm, *vc, PosIdx(*apos), out).map_err(|e| {
                    vm.add_trace(
                        e,
                        PosIdx(*apos),
                        format!(
                            "while evaluating attribute '{}'",
                            String::from_utf8_lossy(name)
                        ),
                    );
                    e
                })?;
            }
            out.push(b'}');
        }
        Tag::List => {
            out.push(b'[');
            for (i, &el) in list_elems(&v).iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                to_json(vm, el, pos, out).map_err(|e| {
                    vm.add_trace(
                        e,
                        pos,
                        format!("while evaluating list element at index {i}"),
                    );
                    e
                })?;
            }
            out.push(b']');
        }
        t => {
            let msg = format!("cannot convert {} to JSON", vm.show_type(&v));
            let _ = t;
            return Err(vm.new_err(ErrKind::Type, msg, pos));
        }
    }
    Ok(())
}

pub fn from_json(vm: &mut VM, s: &[u8], pos: PosIdx) -> Result<Value, ErrId> {
    let parsed: serde_json::Value = match serde_json::from_slice(s) {
        Ok(v) => v,
        Err(err) => {
            let msg = format!(
                "failed to parse JSON string '{}': {}",
                String::from_utf8_lossy(s),
                err
            );
            return Err(vm.new_err(ErrKind::Eval, msg, pos));
        }
    };
    json_to_value(vm, &parsed)
}

fn json_to_value(vm: &mut VM, j: &serde_json::Value) -> Result<Value, ErrId> {
    Ok(match j {
        serde_json::Value::Null => Value::null(),
        serde_json::Value::Bool(b) => Value::bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::int(i)
            } else {
                Value::float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => vm.new_string_value(s.as_bytes(), std::ptr::null_mut()),
        serde_json::Value::Array(items) => {
            let scope = vm.temp_scope();
            let mut cells: Vec<VRef> = Vec::with_capacity(items.len());
            for it in items {
                let v = json_to_value(vm, it)?;
                let c = vm.alloc_cell(v);
                vm.temp_roots.push(c);
                cells.push(c);
            }
            let v = vm.new_list_value(&cells);
            vm.temp_end(scope);
            v
        }
        serde_json::Value::Object(map) => {
            let scope = vm.temp_scope();
            let mut entries: Vec<crate::value::Attr> = Vec::with_capacity(map.len());
            for (k, jv) in map {
                let sym = vm.symbols.create(k.as_bytes());
                let v = json_to_value(vm, jv)?;
                let c = vm.alloc_cell(v);
                vm.temp_roots.push(c);
                entries.push(crate::value::Attr {
                    sym: sym.0,
                    pos: 0,
                    val: c,
                });
            }
            entries.sort_by_key(|a| a.sym);
            entries.dedup_by_key(|a| a.sym);
            let v = vm.new_bindings_value(&entries);
            vm.temp_end(scope);
            v
        }
    })
}
