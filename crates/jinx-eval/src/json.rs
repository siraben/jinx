//! toJSON / fromJSON, matching value-to-json.cc + nlohmann::json::dump()
//! byte-for-byte (notably float formatting: std::to_chars shortest
//! round-trip, with ".0" appended to integral results).

use jinx_syntax::pos::{PosIdx, NO_POS};
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

/// Höhrmann UTF-8 decoding DFA, exactly as embedded in `nlohmann::json`'s
/// `dump()` (the type map for bytes 0x00-0xFF followed by the state-transition
/// table). Used to reproduce nlohmann's `type_error.316` diagnostics.
#[rustfmt::skip]
const UTF8D: [u8; 400] = [
    // bytes 0x00..0xFF -> character class
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0, 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0, 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0, 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0, 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1, 9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,
    7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7, 7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    8,8,2,2,2,2,2,2,2,2,2,2,2,2,2,2, 2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
    10,3,3,3,3,3,3,3,3,3,3,3,3,4,3,3, 11,6,6,6,5,8,8,8,8,8,8,8,8,8,8,8,
    // state transitions
    0,12,24,36,60,96,84,12,12,12,48,72, 12,12,12,12,12,12,12,12,12,12,12,12,
    12,0,12,12,12,12,12,0,12,0,12,12, 12,24,12,12,12,12,12,24,12,24,12,12,
    12,12,12,12,12,12,12,24,12,12,12,12, 12,24,12,12,12,12,12,12,12,24,12,12,
    12,12,12,12,12,12,12,36,12,36,12,12, 12,36,12,12,12,12,12,36,12,36,12,12,
    12,36,12,12,12,12,12,12,12,12,12,12,
    // padding to 400 (unused)
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,
];

const UTF8_ACCEPT: u32 = 0;
const UTF8_REJECT: u32 = 12;

/// Run nlohmann's UTF-8 validation over `s`. Returns the index and value of the
/// byte that first drives the decoder into `UTF8_REJECT` (nlohmann's `invalid
/// UTF-8 byte at index i: 0xXX`), or the last byte's index for a truncated
/// trailing sequence (`incomplete UTF-8 string; last byte: 0xXX`).
fn utf8_error(s: &[u8]) -> Option<Utf8Error> {
    let mut state = UTF8_ACCEPT;
    for (i, &byte) in s.iter().enumerate() {
        let ty = UTF8D[byte as usize] as u32;
        state = UTF8D[(256 + state + ty) as usize] as u32;
        if state == UTF8_REJECT {
            return Some(Utf8Error::Invalid { index: i, byte });
        }
    }
    if state != UTF8_ACCEPT {
        // Truncated multi-byte sequence at end of string.
        let i = s.len().saturating_sub(1);
        return Some(Utf8Error::Incomplete { byte: s[i] });
    }
    None
}

enum Utf8Error {
    Invalid { index: usize, byte: u8 },
    Incomplete { byte: u8 },
}

impl Utf8Error {
    /// The `e.what()` text nlohmann's `type_error` produces.
    fn what(&self) -> String {
        match self {
            Utf8Error::Invalid { index, byte } => format!(
                "[json.exception.type_error.316] invalid UTF-8 byte at index {index}: 0x{byte:02X}"
            ),
            Utf8Error::Incomplete { byte } => format!(
                "[json.exception.type_error.316] incomplete UTF-8 string; last byte: 0x{byte:02X}"
            ),
        }
    }
}

/// nlohmann-style string escaping (control chars as \uXXXX), no validation.
fn json_escape(out: &mut Vec<u8>, s: &[u8]) {
    use jinx_store::escape_scan::next_json_escape;
    out.push(b'"');
    // Only '"', '\\' and control chars (< 0x20) need escaping; everything else
    // (including raw high bytes) is copied verbatim. Find the next byte needing
    // escaping (SIMD-accelerated on aarch64/x86_64, scalar elsewhere) and
    // bulk-copy the clean span before it.
    let mut start = 0;
    while start < s.len() {
        let i = start + next_json_escape(&s[start..]);
        if i >= s.len() {
            break;
        }
        let c = s[i];
        out.extend_from_slice(&s[start..i]);
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c => {
                // Remaining control chars (< 0x20): \u00XX.
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(c >> 4) as usize]);
                out.push(HEX[(c & 0xf) as usize]);
            }
        }
        start = i + 1;
    }
    out.extend_from_slice(&s[start..]);
    out.push(b'"');
}

/// Validate UTF-8 (exactly like `dump()`) then escape, returning the nlohmann
/// diagnostic on failure.
fn json_string(out: &mut Vec<u8>, s: &[u8]) -> Result<(), Utf8Error> {
    if let Some(e) = utf8_error(s) {
        return Err(e);
    }
    json_escape(out, s);
    Ok(())
}

/// Build the `JSONSerializationError` an invalid string triggers, matching
/// `value-to-json.cc` (`JSON serialization error: %s` with `e.what()`).
fn json_utf8_errid(vm: &mut VM, e: Utf8Error) -> ErrId {
    let msg = format!("JSON serialization error: {}", e.what());
    vm.new_err(ErrKind::Eval, msg, NO_POS)
}

/// Public wrapper for JSON string escaping (used to build `__json`); silently
/// tolerates invalid UTF-8 (the surrounding derivationStrict path validates
/// elsewhere).
pub fn json_string_pub(out: &mut Vec<u8>, s: &[u8]) {
    json_escape(out, s);
}

pub fn to_json(vm: &mut VM, cell: VRef, pos: PosIdx, out: &mut Vec<u8>) -> Result<(), ErrId> {
    let mut ctx = Vec::new();
    to_json_ctx(vm, cell, pos, out, &mut ctx)
}

pub fn to_json_ctx(
    vm: &mut VM,
    cell: VRef,
    pos: PosIdx,
    out: &mut Vec<u8>,
    ctx: &mut Vec<u32>,
) -> Result<(), ErrId> {
    // C++ `printValueAsJSON` calls `addCallDepth(pos)` per level, so a deep
    // structure overflows at max-call-depth rather than the host stack.
    vm.depth_check(pos)?;
    vm.call_depth += 1;
    let r = to_json_ctx_inner(vm, cell, pos, out, ctx);
    vm.call_depth -= 1;
    r
}

fn to_json_ctx_inner(
    vm: &mut VM,
    cell: VRef,
    pos: PosIdx,
    out: &mut Vec<u8>,
    ctx: &mut Vec<u32>,
) -> Result<(), ErrId> {
    vm.force(cell, pos)?;
    let v = val(cell);
    match v.tag() {
        Tag::Int => out.extend_from_slice(v.as_int().to_string().as_bytes()),
        Tag::Float => out.extend_from_slice(json_float(v.as_float()).as_bytes()),
        Tag::True => out.extend_from_slice(b"true"),
        Tag::False => out.extend_from_slice(b"false"),
        Tag::Null => out.extend_from_slice(b"null"),
        Tag::String => {
            if let Err(e) = json_string(out, str_bytes(&v)) {
                return Err(json_utf8_errid(vm, e));
            }
            for id in vm.read_str_ctx(&v) {
                if !ctx.contains(&id) {
                    ctx.push(id);
                }
            }
        }
        Tag::Path => {
            // C++ copies the path to the store here (copyToStore=true).
            let path = crate::vm::path_bytes(&v).to_vec();
            let (printed, id) = vm.copy_path_to_store(&path, pos)?;
            if let Err(e) = json_string(out, &printed) {
                return Err(json_utf8_errid(vm, e));
            }
            if !ctx.contains(&id) {
                ctx.push(id);
            }
        }
        Tag::Attrs => {
            // __toString / outPath handling.
            if let Some(f) = crate::vm::attrs_get(&v, vm.syms.to_string) {
                let r = vm.call_function(f.val, &[cell], pos)?;
                let rc = vm.alloc_cell(r);
                let scope = vm.temp_scope();
                vm.temp_roots.push(rc);
                let (s, sctx) = vm.coerce_to_string(
                    rc,
                    pos,
                    "while evaluating the result of the `__toString` attribute",
                    false,
                    false,
                    true,
                )?;
                vm.temp_end(scope);
                if let Err(e) = json_string(out, &s) {
                    return Err(json_utf8_errid(vm, e));
                }
                for id in sctx {
                    if !ctx.contains(&id) {
                        ctx.push(id);
                    }
                }
                return Ok(());
            }
            if let Some(op) = crate::vm::attrs_get(&v, vm.syms.out_path) {
                return to_json_ctx(vm, op.val, PosIdx(op.pos), out, ctx);
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
                if let Err(e) = json_string(out, name) {
                    return Err(json_utf8_errid(vm, e));
                }
                out.push(b':');
                to_json_ctx(vm, *vc, PosIdx(*apos), out, ctx).map_err(|e| {
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
                to_json_ctx(vm, el, pos, out, ctx).map_err(|e| {
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
            // C++ prim_fromJSON catches only JSONParseError (a *parse* failure)
            // and adds this frame; semantic errors from json_to_value (integer
            // overflow, null bytes) are NOT wrapped. The base message stays
            // serde's — nlohmann's exact wording isn't replicated.
            let e = vm.new_err(ErrKind::Eval, msg, pos);
            vm.add_trace(e, pos, "while decoding a JSON string");
            return Err(e);
        }
    };
    json_to_value(vm, &parsed)
}

/// Port of `forceNoNullByte` (eval.cc): reject strings containing NUL, with
/// the NUL rendered as `␀` (U+2400) in the message. The error carries no
/// position.
pub fn force_no_null_byte(vm: &mut VM, s: &[u8]) -> Result<(), ErrId> {
    if s.contains(&0) {
        let mut shown = Vec::with_capacity(s.len());
        for &b in s {
            if b == 0 {
                shown.extend_from_slice("␀".as_bytes());
            } else {
                shown.push(b);
            }
        }
        let mut msg = b"input string '".to_vec();
        msg.extend_from_slice(&shown);
        msg.extend_from_slice(
            b"' cannot be represented as Nix string because it contains null bytes",
        );
        return Err(vm.new_err(ErrKind::Eval, msg, jinx_syntax::pos::NO_POS));
    }
    Ok(())
}

fn json_to_value(vm: &mut VM, j: &serde_json::Value) -> Result<Value, ErrId> {
    Ok(match j {
        serde_json::Value::Null => Value::null(),
        serde_json::Value::Bool(b) => Value::bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::int(i)
            } else if let Some(u) = n.as_u64() {
                // Integer literal too large for a signed 64-bit Nix integer.
                return Err(vm.new_err(
                    ErrKind::Eval,
                    format!("unsigned json number {u} outside of Nix integer range"),
                    jinx_syntax::pos::NO_POS,
                ));
            } else {
                Value::float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => {
            force_no_null_byte(vm, s.as_bytes())?;
            vm.new_string_value(s.as_bytes(), std::ptr::null_mut())
        }
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
                force_no_null_byte(vm, k.as_bytes())?;
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
