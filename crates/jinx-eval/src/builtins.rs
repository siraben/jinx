//! Primops and global constants (ported from primops.cc; error message
//! strings verbatim). Unimplemented builtins (store/fetch/regex/xml/...)
//! are registered as stubs that fail with a distinctive message when
//! *called*, so name resolution and `builtins ? x` behave correctly.

use std::path::{Path, PathBuf};

use jinx_store::hash::{hash_string, Hash, HashAlgorithm, HashFormat};
use jinx_syntax::pos::{Origin, PosIdx, NO_POS};
use jinx_syntax::symbol::Symbol;

use crate::chunk::{Chunk, Op, Program};
use crate::error::{ErrId, ErrKind};
use crate::immortal;
use crate::print;
use crate::value::{Attr, Tag, VRef, Value};
use crate::vm::{
    attrs_entries, attrs_get, canon_path, list_elems, path_bytes, str_bytes, str_ctx, val, VM,
    PrimOpDef,
};

type R = Result<Value, ErrId>;

// ---------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------

/// Synthetic "apply" chunks used for lazy applications (C++ `mkApp`):
/// chunk n-1 applies upval0 to upvals 1..=n.
pub fn make_apply_prog() -> &'static Program {
    let mut prog = Program {
        chunks: Vec::new(),
        consts: Vec::new(),
        attrs_descs: Vec::new(),
        rec_descs: Vec::new(),
        concat_descs: Vec::new(),
        haspath_descs: Vec::new(),
        texts: Vec::new(),
        refs: Vec::new(),
    };
    for n in 1..=2u32 {
        let mut ops = vec![Op::GetUpval(0)];
        for k in 0..n {
            ops.push(Op::GetUpval(k + 1));
        }
        ops.push(Op::Call(n));
        ops.push(Op::Force);
        ops.push(Op::Ret);
        prog.chunks.push(Chunk {
            ops,
            ..Default::default()
        });
    }
    prog.leak()
}

impl VM {
    /// Lazy application thunk: forces to `f a1 .. an`.
    pub fn new_apply_thunk(&mut self, f: VRef, args: &[VRef]) -> Value {
        let prog = *self
            .apply_prog
            .get_or_insert_with(make_apply_prog);
        let code = prog.code_ref((args.len() - 1) as u32) as *const _ as *const ();
        let mut upvals = Vec::with_capacity(args.len() + 1);
        upvals.push(f);
        upvals.extend_from_slice(args);
        self.gc_check();
        self.heap.new_thunk(Tag::Thunk, code, &upvals)
    }
}

struct Reg {
    name: &'static str,
    arity: u8,
    func: fn(&mut VM, &'static PrimOpDef, &[VRef], PosIdx) -> R,
}

fn unimplemented(vm: &mut VM, def: &'static PrimOpDef, _args: &[VRef], pos: PosIdx) -> R {
    Err(vm.new_err(
        ErrKind::Eval,
        format!("the '{}' builtin is not implemented by jinx yet", def.display()),
        pos,
    ))
}

const UNIMPLEMENTED: &[(&str, u8)] = &[
    ("fetchFinalTree", 1),
    ("fetchGit", 1),
    ("fetchMercurial", 1),
    ("fetchTarball", 1),
    ("fetchTree", 1),
    ("__exec", 1),
    ("__fetchClosure", 1),
    ("__fetchurl", 1),
    ("__filterSource", 2),
    ("__forceLazyFetcherAttr", 1),
    ("__importNative", 2),
    ("__outputOf", 2),
    ("__path", 1),
    ("__storePath", 1),
    ("__toFile", 2),
    ("__toXML", 1),
    // flakes (experimental): present in `builtins` so failures are clearly
    // "not implemented" rather than "attribute missing".
    ("parseFlakeRef", 1),
    ("flakeRefToString", 1),
    ("getFlake", 1),
];

pub fn register_globals(vm: &mut VM) {
    let mut regs: Vec<Reg> = vec![
        Reg { name: "abort", arity: 1, func: prim_abort },
        Reg { name: "throw", arity: 1, func: prim_throw },
        Reg { name: "break", arity: 1, func: prim_break },
        Reg { name: "import", arity: 1, func: prim_import },
        Reg { name: "scopedImport", arity: 2, func: prim_scoped_import },
        Reg { name: "toString", arity: 1, func: prim_to_string },
        Reg { name: "baseNameOf", arity: 1, func: prim_base_name_of },
        Reg { name: "dirOf", arity: 1, func: prim_dir_of },
        Reg { name: "isNull", arity: 1, func: prim_is_null },
        Reg { name: "map", arity: 2, func: prim_map },
        Reg { name: "removeAttrs", arity: 2, func: prim_remove_attrs },
        Reg { name: "__toPath", arity: 1, func: prim_to_path },
        Reg { name: "__add", arity: 2, func: prim_add },
        Reg { name: "__sub", arity: 2, func: prim_sub },
        Reg { name: "__mul", arity: 2, func: prim_mul },
        Reg { name: "__div", arity: 2, func: prim_div },
        Reg { name: "__bitAnd", arity: 2, func: prim_bit_and },
        Reg { name: "__bitOr", arity: 2, func: prim_bit_or },
        Reg { name: "__bitXor", arity: 2, func: prim_bit_xor },
        Reg { name: "__lessThan", arity: 2, func: prim_less_than },
        Reg { name: "__ceil", arity: 1, func: prim_ceil },
        Reg { name: "__floor", arity: 1, func: prim_floor },
        Reg { name: "__all", arity: 2, func: prim_all },
        Reg { name: "__any", arity: 2, func: prim_any },
        Reg { name: "__filter", arity: 2, func: prim_filter },
        Reg { name: "__elem", arity: 2, func: prim_elem },
        Reg { name: "__elemAt", arity: 2, func: prim_elem_at },
        Reg { name: "__head", arity: 1, func: prim_head },
        Reg { name: "__tail", arity: 1, func: prim_tail },
        Reg { name: "__length", arity: 1, func: prim_length },
        Reg { name: "__concatLists", arity: 1, func: prim_concat_lists },
        Reg { name: "__concatMap", arity: 2, func: prim_concat_map },
        Reg { name: "__foldl'", arity: 3, func: prim_foldl_strict },
        Reg { name: "__genList", arity: 2, func: prim_gen_list },
        Reg { name: "__sort", arity: 2, func: prim_sort },
        Reg { name: "__partition", arity: 2, func: prim_partition },
        Reg { name: "__groupBy", arity: 2, func: prim_group_by },
        Reg { name: "__zipAttrsWith", arity: 2, func: prim_zip_attrs_with },
        Reg { name: "__attrNames", arity: 1, func: prim_attr_names },
        Reg { name: "__attrValues", arity: 1, func: prim_attr_values },
        Reg { name: "__getAttr", arity: 2, func: prim_get_attr },
        Reg { name: "__hasAttr", arity: 2, func: prim_has_attr },
        Reg { name: "__listToAttrs", arity: 1, func: prim_list_to_attrs },
        Reg { name: "__mapAttrs", arity: 2, func: prim_map_attrs },
        Reg { name: "__intersectAttrs", arity: 2, func: prim_intersect_attrs },
        Reg { name: "__catAttrs", arity: 2, func: prim_cat_attrs },
        Reg { name: "__functionArgs", arity: 1, func: prim_function_args },
        Reg { name: "__genericClosure", arity: 1, func: prim_generic_closure },
        Reg { name: "__unsafeGetAttrPos", arity: 2, func: prim_unsafe_get_attr_pos },
        Reg { name: "__stringLength", arity: 1, func: prim_string_length },
        Reg { name: "__substring", arity: 3, func: prim_substring },
        Reg { name: "__concatStringsSep", arity: 2, func: prim_concat_strings_sep },
        Reg { name: "__replaceStrings", arity: 3, func: prim_replace_strings },
        Reg { name: "__splitVersion", arity: 1, func: prim_split_version },
        Reg { name: "__compareVersions", arity: 2, func: prim_compare_versions },
        Reg { name: "__parseDrvName", arity: 1, func: prim_parse_drv_name },
        Reg { name: "__seq", arity: 2, func: prim_seq },
        Reg { name: "__deepSeq", arity: 2, func: prim_deep_seq },
        Reg { name: "__trace", arity: 2, func: prim_trace },
        Reg { name: "__traceVerbose", arity: 2, func: prim_seq_second },
        Reg { name: "__addErrorContext", arity: 2, func: prim_add_error_context },
        Reg { name: "__warn", arity: 2, func: prim_warn },
        Reg { name: "__tryEval", arity: 1, func: prim_try_eval },
        Reg { name: "__toJSON", arity: 1, func: prim_to_json },
        Reg { name: "__fromJSON", arity: 1, func: prim_from_json },
        Reg { name: "__getEnv", arity: 1, func: prim_get_env },
        Reg { name: "__pathExists", arity: 1, func: prim_path_exists },
        Reg { name: "__readFile", arity: 1, func: prim_read_file },
        Reg { name: "__readDir", arity: 1, func: prim_read_dir },
        Reg { name: "__readFileType", arity: 1, func: prim_read_file_type },
        Reg { name: "__findFile", arity: 2, func: prim_find_file },
        Reg { name: "__typeOf", arity: 1, func: prim_type_of },
        Reg { name: "__isAttrs", arity: 1, func: prim_is_attrs },
        Reg { name: "__isBool", arity: 1, func: prim_is_bool },
        Reg { name: "__isFloat", arity: 1, func: prim_is_float },
        Reg { name: "__isFunction", arity: 1, func: prim_is_function },
        Reg { name: "__isInt", arity: 1, func: prim_is_int },
        Reg { name: "__isList", arity: 1, func: prim_is_list },
        Reg { name: "__isPath", arity: 1, func: prim_is_path },
        Reg { name: "__isString", arity: 1, func: prim_is_string },
        Reg { name: "__hashString", arity: 2, func: prim_hash_string },
        Reg { name: "__hashFile", arity: 2, func: prim_hash_file },
        Reg { name: "__convertHash", arity: 1, func: prim_convert_hash },
        Reg { name: "__unsafeDiscardStringContext", arity: 1, func: prim_discard_context },
        Reg { name: "__unsafeDiscardOutputDependency", arity: 1, func: prim_discard_output_dependency },
        Reg { name: "__addDrvOutputDependencies", arity: 1, func: prim_add_drv_output_dependencies },
        Reg { name: "__appendContext", arity: 2, func: prim_append_context },
        Reg { name: "derivationStrict", arity: 1, func: prim_derivation_strict },
        Reg { name: "placeholder", arity: 1, func: prim_placeholder },
        Reg { name: "__match", arity: 2, func: prim_match },
        Reg { name: "__split", arity: 2, func: prim_split },
        Reg { name: "fromTOML", arity: 1, func: prim_from_toml },
        Reg { name: "__getContext", arity: 1, func: prim_get_context },
        Reg { name: "__hasContext", arity: 1, func: prim_has_context },
    ];
    for (name, arity) in UNIMPLEMENTED {
        regs.push(Reg {
            name,
            arity: *arity,
            func: unimplemented,
        });
    }

    // Leak primop defs; build immortal cells.
    let mut builtin_entries: Vec<(Vec<u8>, VRef)> = Vec::new();
    for r in regs {
        let def: &'static PrimOpDef = Box::leak(Box::new(PrimOpDef {
            name: r.name,
            arity: r.arity,
            func: r.func,
        }));
        let cell = immortal::cell(Value::make(Tag::PrimOp, def as *const _ as u64));
        let sym = vm.symbols.create(r.name.as_bytes());
        vm.globals.insert(sym, cell);
        builtin_entries.push((def.display().as_bytes().to_vec(), cell));
    }

    // Constants.
    let add_const = |vm: &mut VM, name: &str, cell: VRef, entries: &mut Vec<(Vec<u8>, VRef)>| {
        let sym = vm.symbols.create(name.as_bytes());
        vm.globals.insert(sym, cell);
        let display = name.strip_prefix("__").unwrap_or(name);
        entries.push((display.as_bytes().to_vec(), cell));
    };

    let true_cell = vm.true_cell;
    let false_cell = vm.false_cell;
    let null_cell = vm.null_cell;
    add_const(vm, "true", true_cell, &mut builtin_entries);
    add_const(vm, "false", false_cell, &mut builtin_entries);
    add_const(vm, "null", null_cell, &mut builtin_entries);

    let cs = immortal::cell(immortal::string(&vm.current_system.clone()));
    add_const(vm, "__currentSystem", cs, &mut builtin_entries);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let ct = immortal::cell(Value::int(now));
    add_const(vm, "__currentTime", ct, &mut builtin_entries);
    let nv = immortal::cell(immortal::string(b"2.36.0"));
    add_const(vm, "__nixVersion", nv, &mut builtin_entries);
    let lv = immortal::cell(Value::int(6));
    add_const(vm, "__langVersion", lv, &mut builtin_entries);
    let sd = immortal::cell(immortal::string(&vm.store_dir.clone()));
    add_const(vm, "__storeDir", sd, &mut builtin_entries);

    // __nixPath: list of { path; prefix; } from the search path.
    let path_sym = vm.symbols.create(b"path");
    let prefix_sym = vm.symbols.create(b"prefix");
    let mut nix_path_elems: Vec<VRef> = Vec::new();
    for (prefix, path) in vm.search_path.clone() {
        let pc = immortal::cell(immortal::string(&path));
        let prc = immortal::cell(immortal::string(&prefix));
        let mut entries = [
            Attr { sym: path_sym.0, pos: 0, val: pc },
            Attr { sym: prefix_sym.0, pos: 0, val: prc },
        ];
        entries.sort_by_key(|a| a.sym);
        nix_path_elems.push(immortal::cell(immortal::bindings(&entries)));
    }
    let np = immortal::cell(immortal::list(&nix_path_elems));
    add_const(vm, "__nixPath", np, &mut builtin_entries);

    // `derivation` is a Nix-level wrapper around derivationStrict
    // (primops/derivation.nix); its cell is patched after compilation so
    // the builtins set can already reference it.
    let derivation_cell = vm.alloc_cell(Value::make(Tag::Blackhole, 0));
    vm.perm_roots.push(derivation_cell);
    let dsym = vm.symbols.create(b"derivation");
    vm.globals.insert(dsym, derivation_cell);
    builtin_entries.push((b"derivation".to_vec(), derivation_cell));

    // The `builtins` attrset (self-referential).
    let builtins_cell = immortal::cell(Value::null());
    builtin_entries.push((b"builtins".to_vec(), builtins_cell));
    let mut entries: Vec<Attr> = builtin_entries
        .into_iter()
        .map(|(name, cell)| Attr {
            sym: vm.symbols.create(&name).0,
            pos: 0,
            val: cell,
        })
        .collect();
    entries.sort_by_key(|a| a.sym);
    entries.dedup_by_key(|a| a.sym);

    let bv = immortal::bindings(&entries);
    crate::vm::set(builtins_cell, bv);
    let bsym = vm.symbols.create(b"builtins");
    vm.globals.insert(bsym, builtins_cell);

    // Compile the derivation wrapper now that globals are in place.
    let src = DERIVATION_NIX;
    let mut warnings = Vec::new();
    let parsed = jinx_syntax::parse_and_bind_with(
        src,
        Origin::String {
            source: src.to_vec(),
        },
        "/",
        None,
        &mut vm.positions,
        &mut vm.symbols,
        &mut warnings,
    )
    .expect("derivation.nix parses");
    let prog = crate::compile::compile_program(
        &parsed.0,
        parsed.1,
        &vm.symbols,
        &vm.globals,
        vm.empty_list_cell,
    );
    let cell = vm.run_program(prog).expect("derivation.nix evaluates");
    crate::vm::set(derivation_cell, val(cell));
}

/// primops/derivation.nix, minus comments.
const DERIVATION_NIX: &[u8] = br#"
drvAttrs@{
  outputs ? [ "out" ],
  ...
}:

let

  strict = derivationStrict drvAttrs;

  commonAttrs =
    drvAttrs
    // (builtins.listToAttrs outputsList)
    // {
      all = map (x: x.value) outputsList;
      inherit drvAttrs;
    };

  outputToAttrListElement = outputName: {
    name = outputName;
    value = commonAttrs // {
      outPath = strict.${outputName};
      drvPath = strict.drvPath;
      type = "derivation";
      inherit outputName;
    };
  };

  outputsList = map outputToAttrListElement outputs;

in
(builtins.head outputsList).value
"#;

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

fn mk_string(vm: &mut VM, s: &[u8]) -> Value {
    vm.new_string_value(s, std::ptr::null_mut())
}

fn mk_string_ctx(vm: &mut VM, s: &[u8], ctx: &[u32]) -> Value {
    let cp = vm.make_ctx(ctx);
    vm.new_string_value(s, cp)
}

/// Root a fresh value in a cell registered in temp_roots; returns the cell.
fn temp_cell(vm: &mut VM, v: Value) -> VRef {
    let c = vm.alloc_cell(v);
    vm.temp_roots.push(c);
    c
}

fn force_fun(vm: &mut VM, cell: VRef, pos: PosIdx, ctx: &str) -> Result<(), ErrId> {
    vm.force(cell, pos).map_err(|e| {
        vm.add_trace(e, pos, ctx);
        e
    })?;
    let v = val(cell);
    let is_fun = matches!(v.tag(), Tag::Closure | Tag::PrimOp | Tag::PrimOpApp)
        || (v.tag() == Tag::Attrs && attrs_get(&v, vm.syms.functor).is_some());
    if !is_fun {
        let printed = print::print_value_err(vm, &v);
        let msg = format!(
            "expected a function but found {}: {}",
            vm.show_type(&v),
            printed
        );
        let e = vm.new_err(ErrKind::Type, msg, pos);
        vm.add_trace(e, pos, ctx);
        return Err(e);
    }
    Ok(())
}

/// C++ CompareValues (used by lessThan, sort, genericClosure).
fn compare_values(vm: &mut VM, a: VRef, b: VRef, pos: PosIdx, ctx: &str) -> Result<bool, ErrId> {
    vm.force(a, pos)?;
    vm.force(b, pos)?;
    let (va, vb) = (val(a), val(b));
    match (va.tag(), vb.tag()) {
        (Tag::Float, Tag::Int) => return Ok(va.as_float() < vb.as_int() as f64),
        (Tag::Int, Tag::Float) => return Ok((va.as_int() as f64) < vb.as_float()),
        _ => {}
    }
    if va.tag() != vb.tag() {
        let (pa, pb) = (
            print::print_value_err(vm, &va),
            print::print_value_err(vm, &vb),
        );
        let msg = format!(
            "cannot compare {} with {}; values are {} and {}",
            vm.show_type(&va),
            vm.show_type(&vb),
            pa,
            pb
        );
        let e = vm.new_err(ErrKind::Eval, msg, pos);
        if !ctx.is_empty() {
            vm.add_trace(e, pos, ctx);
        }
        return Err(e);
    }
    match va.tag() {
        Tag::Int => Ok(va.as_int() < vb.as_int()),
        Tag::Float => Ok(va.as_float() < vb.as_float()),
        Tag::String => Ok(str_bytes(&va) < str_bytes(&vb)),
        Tag::Path => Ok(path_bytes(&va) < path_bytes(&vb)),
        Tag::List => {
            let (ea, eb) = (list_elems(&va), list_elems(&vb));
            for i in 0.. {
                if i == eb.len() {
                    return Ok(false);
                }
                if i == ea.len() {
                    return Ok(true);
                }
                if !vm.eq_values(ea[i], eb[i], pos, ctx, false)? {
                    return compare_values(vm, ea[i], eb[i], pos, "while comparing two list elements");
                }
            }
            unreachable!()
        }
        _ => {
            let (pa, pb) = (
                print::print_value_err(vm, &va),
                print::print_value_err(vm, &vb),
            );
            let msg = format!(
                "cannot compare {} with {}; values of that type are incomparable; values are {} and {}",
                vm.show_type(&va),
                vm.show_type(&vb),
                pa,
                pb
            );
            let e = vm.new_err(ErrKind::Eval, msg, pos);
            if !ctx.is_empty() {
                vm.add_trace(e, pos, ctx);
            }
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------
// control / errors
// ---------------------------------------------------------------------

fn prim_abort(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let (s, _) = vm.coerce_to_string(
        args[0],
        pos,
        "while evaluating the error message passed to builtins.abort",
        false,
        false,
        true,
    )?;
    let msg = format!(
        "evaluation aborted with the following error message: '{}'",
        String::from_utf8_lossy(&s)
    );
    Err(vm.new_err(ErrKind::Abort, msg, pos))
}

fn prim_throw(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let (s, _) = vm.coerce_to_string(
        args[0],
        pos,
        "while evaluating the error message passed to builtins.throw",
        false,
        false,
        true,
    )?;
    Err(vm.new_err(ErrKind::Thrown, s, pos))
}

fn prim_break(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    // Without a debugger attached, `break` is the identity.
    vm.force(args[0], pos)?;
    Ok(val(args[0]))
}

fn prim_try_eval(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let mut entries: Vec<Attr>;
    match vm.force(args[0], pos) {
        Ok(()) => {
            entries = vec![
                Attr { sym: vm.syms.value.0, pos: 0, val: args[0] },
                Attr { sym: vm.syms.success.0, pos: 0, val: vm.true_cell },
            ];
        }
        Err(e) if vm.err_kind(e).catchable() => {
            entries = vec![
                Attr { sym: vm.syms.value.0, pos: 0, val: vm.false_cell },
                Attr { sym: vm.syms.success.0, pos: 0, val: vm.false_cell },
            ];
        }
        Err(e) => return Err(e),
    }
    entries.sort_by_key(|a| a.sym);
    Ok(vm.new_bindings_value(&entries))
}

fn prim_seq(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force(args[0], pos)?;
    vm.force(args[1], pos)?;
    Ok(val(args[1]))
}

fn prim_seq_second(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    // traceVerbose without --trace-verbose: just return e2.
    vm.force(args[1], pos)?;
    Ok(val(args[1]))
}

fn prim_deep_seq(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    print::deep_force(vm, args[0])?;
    vm.force(args[1], pos)?;
    Ok(val(args[1]))
}

fn prim_trace(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force(args[0], pos)?;
    let v = val(args[0]);
    let text = if v.tag() == Tag::String {
        str_bytes(&v).to_vec()
    } else {
        print::print_value_trace(vm, &v)
    };
    let mut out = b"trace: ".to_vec();
    out.extend_from_slice(&text);
    out.push(b'\n');
    use std::io::Write;
    let _ = std::io::stderr().write_all(&out);
    vm.force(args[1], pos)?;
    Ok(val(args[1]))
}

fn prim_warn(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let s = vm.force_string(
        args[0],
        pos,
        "while evaluating the first argument; the message passed to builtins.warn",
    )?;
    let mut out = b"evaluation warning: ".to_vec();
    out.extend_from_slice(&s);
    out.push(b'\n');
    use std::io::Write;
    let _ = std::io::stderr().write_all(&out);
    vm.force(args[1], pos)?;
    Ok(val(args[1]))
}

fn prim_add_error_context(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    match vm.force(args[1], pos) {
        Ok(()) => Ok(val(args[1])),
        Err(e) => {
            let (s, _) = vm.coerce_to_string(
                args[0],
                pos,
                "while evaluating the error message passed to builtins.addErrorContext",
                false,
                false,
                true,
            )?;
            vm.add_trace(e, NO_POS, String::from_utf8_lossy(&s).into_owned());
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------
// type tests
// ---------------------------------------------------------------------

fn type_test(vm: &mut VM, args: &[VRef], pos: PosIdx, f: impl Fn(&Value) -> bool) -> R {
    vm.force(args[0], pos)?;
    Ok(Value::bool(f(&val(args[0]))))
}

fn prim_is_null(vm: &mut VM, _d: &'static PrimOpDef, a: &[VRef], p: PosIdx) -> R {
    type_test(vm, a, p, |v| v.tag() == Tag::Null)
}

fn prim_is_attrs(vm: &mut VM, _d: &'static PrimOpDef, a: &[VRef], p: PosIdx) -> R {
    type_test(vm, a, p, |v| v.tag() == Tag::Attrs)
}

fn prim_is_bool(vm: &mut VM, _d: &'static PrimOpDef, a: &[VRef], p: PosIdx) -> R {
    type_test(vm, a, p, |v| matches!(v.tag(), Tag::True | Tag::False))
}

fn prim_is_float(vm: &mut VM, _d: &'static PrimOpDef, a: &[VRef], p: PosIdx) -> R {
    type_test(vm, a, p, |v| v.tag() == Tag::Float)
}

fn prim_is_function(vm: &mut VM, _d: &'static PrimOpDef, a: &[VRef], p: PosIdx) -> R {
    type_test(vm, a, p, |v| {
        matches!(v.tag(), Tag::Closure | Tag::PrimOp | Tag::PrimOpApp)
    })
}

fn prim_is_int(vm: &mut VM, _d: &'static PrimOpDef, a: &[VRef], p: PosIdx) -> R {
    type_test(vm, a, p, |v| v.tag() == Tag::Int)
}

fn prim_is_list(vm: &mut VM, _d: &'static PrimOpDef, a: &[VRef], p: PosIdx) -> R {
    type_test(vm, a, p, |v| v.tag() == Tag::List)
}

fn prim_is_path(vm: &mut VM, _d: &'static PrimOpDef, a: &[VRef], p: PosIdx) -> R {
    type_test(vm, a, p, |v| v.tag() == Tag::Path)
}

fn prim_is_string(vm: &mut VM, _d: &'static PrimOpDef, a: &[VRef], p: PosIdx) -> R {
    type_test(vm, a, p, |v| v.tag() == Tag::String)
}

fn prim_type_of(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force(args[0], pos)?;
    let t = match val(args[0]).tag() {
        Tag::Int => "int",
        Tag::True | Tag::False => "bool",
        Tag::String => "string",
        Tag::Path => "path",
        Tag::Null => "null",
        Tag::Attrs => "set",
        Tag::List => "list",
        Tag::Closure | Tag::PrimOp | Tag::PrimOpApp => "lambda",
        Tag::Float => "float",
        _ => unreachable!(),
    };
    Ok(mk_string(vm, t.as_bytes()))
}

// ---------------------------------------------------------------------
// arithmetic
// ---------------------------------------------------------------------

fn arith(
    vm: &mut VM,
    args: &[VRef],
    pos: PosIdx,
    what: (&str, &str, &str), // ("addition"/"the addition" ctx word, verb, op symbol)
    fi: fn(i64, i64) -> Option<i64>,
    ff: fn(f64, f64) -> f64,
) -> R {
    vm.force(args[0], pos)?;
    vm.force(args[1], pos)?;
    let (a, b) = (val(args[0]), val(args[1]));
    if a.tag() == Tag::Float || b.tag() == Tag::Float {
        let x = vm.force_float(
            args[0],
            pos,
            &format!("while evaluating the first argument of the {}", what.0),
        )?;
        let y = vm.force_float(
            args[1],
            pos,
            &format!("while evaluating the second argument of the {}", what.0),
        )?;
        Ok(Value::float(ff(x, y)))
    } else {
        let x = vm.force_int(
            args[0],
            pos,
            &format!("while evaluating the first argument of the {}", what.0),
        )?;
        let y = vm.force_int(
            args[1],
            pos,
            &format!("while evaluating the second argument of the {}", what.0),
        )?;
        match fi(x, y) {
            Some(r) => Ok(Value::int(r)),
            None => {
                let msg = format!("integer overflow in {} {} {} {}", what.1, x, what.2, y);
                Err(vm.new_err(ErrKind::Eval, msg, pos))
            }
        }
    }
}

fn prim_add(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    arith(vm, args, pos, ("addition", "adding", "+"), i64::checked_add, |a, b| a + b)
}

fn prim_sub(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    arith(vm, args, pos, ("subtraction", "subtracting", "-"), i64::checked_sub, |a, b| a - b)
}

fn prim_mul(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    arith(vm, args, pos, ("multiplication", "multiplying", "*"), i64::checked_mul, |a, b| a * b)
}

fn prim_div(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force(args[0], pos)?;
    vm.force(args[1], pos)?;
    let f2 = vm.force_float(
        args[1],
        pos,
        "while evaluating the second operand of the division",
    )?;
    if f2 == 0.0 {
        return Err(vm.new_err(ErrKind::Eval, "division by zero", pos));
    }
    let (a, b) = (val(args[0]), val(args[1]));
    if a.tag() == Tag::Float || b.tag() == Tag::Float {
        let f1 = vm.force_float(
            args[0],
            pos,
            "while evaluating the first operand of the division",
        )?;
        Ok(Value::float(f1 / f2))
    } else {
        let i1 = vm.force_int(
            args[0],
            pos,
            "while evaluating the first operand of the division",
        )?;
        let i2 = vm.force_int(
            args[1],
            pos,
            "while evaluating the second operand of the division",
        )?;
        match i1.checked_div(i2) {
            Some(r) => Ok(Value::int(r)),
            None => {
                let msg = format!("integer overflow in dividing {} / {}", i1, i2);
                Err(vm.new_err(ErrKind::Eval, msg, pos))
            }
        }
    }
}

fn prim_bit_and(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let a = vm.force_int(args[0], pos, "while evaluating the first argument passed to builtins.bitAnd")?;
    let b = vm.force_int(args[1], pos, "while evaluating the second argument passed to builtins.bitAnd")?;
    Ok(Value::int(a & b))
}

fn prim_bit_or(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let a = vm.force_int(args[0], pos, "while evaluating the first argument passed to builtins.bitOr")?;
    let b = vm.force_int(args[1], pos, "while evaluating the second argument passed to builtins.bitOr")?;
    Ok(Value::int(a | b))
}

fn prim_bit_xor(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let a = vm.force_int(args[0], pos, "while evaluating the first argument passed to builtins.bitXor")?;
    let b = vm.force_int(args[1], pos, "while evaluating the second argument passed to builtins.bitXor")?;
    Ok(Value::int(a ^ b))
}

fn prim_less_than(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force(args[0], pos)?;
    vm.force(args[1], pos)?;
    Ok(Value::bool(compare_values(vm, args[0], args[1], NO_POS, "")?))
}

fn floor_ceil(vm: &mut VM, args: &[VRef], pos: PosIdx, name: &str, f: fn(f64) -> f64) -> R {
    let value = vm.force_float(
        args[0],
        pos,
        &format!("while evaluating the first argument passed to builtins.{name}"),
    )?;
    let r = f(value);
    let is_int = val(args[0]).tag() == Tag::Int;
    let int_min = i64::MIN as f64;
    if r >= int_min && r < -int_min {
        if is_int {
            return Ok(Value::int(val(args[0]).as_int()));
        }
        Ok(Value::int(r as i64))
    } else {
        let msg = format!("NixFloat argument {} is not in the range of NixInt", fmt_float_msg(value));
        Err(vm.new_err(ErrKind::Eval, msg, pos))
    }
}

fn fmt_float_msg(f: f64) -> String {
    print::fmt_f64_g6(f)
}

fn prim_floor(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    floor_ceil(vm, args, pos, "floor", f64::floor)
}

fn prim_ceil(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    floor_ceil(vm, args, pos, "ceil", f64::ceil)
}

// ---------------------------------------------------------------------
// strings
// ---------------------------------------------------------------------

fn prim_to_string(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let (s, ctx) = vm.coerce_to_string(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.toString",
        true,
        false,
        true,
    )?;
    Ok(mk_string_ctx(vm, &s, &ctx))
}

fn prim_string_length(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let (s, _) = vm.coerce_to_string(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.stringLength",
        false,
        false,
        true,
    )?;
    Ok(Value::int(s.len() as i64))
}

fn prim_substring(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let start = vm.force_int(
        args[0],
        pos,
        "while evaluating the first argument (the start offset) passed to builtins.substring",
    )?;
    if start < 0 {
        return Err(vm.new_err(ErrKind::Eval, "negative start position in 'substring'", pos));
    }
    let len = vm.force_int(
        args[1],
        pos,
        "while evaluating the second argument (the substring length) passed to builtins.substring",
    )?;
    // Special case: skip coercion if the length is 0.
    if len == 0 {
        vm.force(args[2], pos)?;
        if val(args[2]).tag() == Tag::String {
            let v = val(args[2]);
            let ctxp = str_ctx(&v);
            return Ok(vm.new_string_value(b"", ctxp));
        }
    }
    let (s, ctx) = vm.coerce_to_string(
        args[2],
        pos,
        "while evaluating the third argument (the string) passed to builtins.substring",
        false,
        false,
        true,
    )?;
    let start = start as usize;
    let out: &[u8] = if start >= s.len() {
        b""
    } else {
        let end = if len < 0 {
            s.len()
        } else {
            (start + len as usize).min(s.len())
        };
        &s[start..end]
    };
    Ok(mk_string_ctx(vm, out, &ctx))
}

fn prim_concat_strings_sep(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let sep = vm.force_string(
        args[0],
        pos,
        "while evaluating the first argument (the separator string) passed to builtins.concatStringsSep",
    )?;
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument (the list of strings to concat) passed to builtins.concatStringsSep",
    )?;
    let elems = list_elems(&val(args[1]));
    let mut out: Vec<u8> = Vec::new();
    let mut ctx: Vec<u32> = Vec::new();
    for (i, &el) in elems.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(&sep);
        }
        let (part, pctx) = vm.coerce_to_string(
            el,
            pos,
            "while evaluating one element of the list of strings to concat passed to builtins.concatStringsSep",
            false,
            false,
            true,
        )?;
        out.extend_from_slice(&part);
        for c in pctx {
            if !ctx.contains(&c) {
                ctx.push(c);
            }
        }
    }
    Ok(mk_string_ctx(vm, &out, &ctx))
}

fn prim_replace_strings(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.replaceStrings",
    )?;
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.replaceStrings",
    )?;
    let from_cells = list_elems(&val(args[0])).to_vec();
    let to_cells = list_elems(&val(args[1])).to_vec();
    if from_cells.len() != to_cells.len() {
        return Err(vm.new_err(
            ErrKind::Eval,
            "'from' and 'to' arguments passed to builtins.replaceStrings have different lengths",
            pos,
        ));
    }
    let mut froms: Vec<Vec<u8>> = Vec::with_capacity(from_cells.len());
    for &c in &from_cells {
        froms.push(vm.force_string(
            c,
            pos,
            "while evaluating one of the strings to replace passed to builtins.replaceStrings",
        )?);
    }
    // Replacement strings are evaluated lazily, only when actually used.
    let mut tos: Vec<Option<(Vec<u8>, Vec<u32>)>> = vec![None; to_cells.len()];
    let get_to = |vm: &mut VM,
                      tos: &mut Vec<Option<(Vec<u8>, Vec<u32>)>>,
                      i: usize|
     -> Result<(Vec<u8>, Vec<u32>), ErrId> {
        if let Some(t) = &tos[i] {
            return Ok(t.clone());
        }
        let c = to_cells[i];
        let s = vm.force_string(
            c,
            pos,
            "while evaluating one of the replacement strings passed to builtins.replaceStrings",
        )?;
        let v = val(c);
        let mut ids: Vec<u32> = Vec::new();
        let cp = str_ctx(&v);
        if !cp.is_null() {
            // SAFETY: ctx objects hold u32 ids.
            unsafe {
                let len = crate::value::header_len(*cp);
                ids.extend_from_slice(std::slice::from_raw_parts(cp.add(1) as *const u32, len));
            }
        }
        tos[i] = Some((s.clone(), ids.clone()));
        Ok((s, ids))
    };
    let s = vm.force_string(
        args[2],
        pos,
        "while evaluating the third argument passed to builtins.replaceStrings",
    )?;
    let mut out: Vec<u8> = Vec::new();
    let mut ctx: Vec<u32> = Vec::new();
    let mut p = 0usize;
    while p <= s.len() {
        let mut found = false;
        for (i, from) in froms.iter().enumerate() {
            if s[p..].starts_with(from.as_slice()) {
                found = true;
                let (to, to_ctx) = get_to(vm, &mut tos, i)?;
                out.extend_from_slice(&to);
                for c in to_ctx {
                    if !ctx.contains(&c) {
                        ctx.push(c);
                    }
                }
                if from.is_empty() {
                    if p < s.len() {
                        out.push(s[p]);
                    }
                    p += 1;
                } else {
                    p += from.len();
                }
                break;
            }
        }
        if !found {
            if p < s.len() {
                out.push(s[p]);
            }
            p += 1;
        }
    }
    Ok(mk_string_ctx(vm, &out, &ctx))
}

fn prim_discard_context(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let (s, _) = vm.coerce_to_string(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.unsafeDiscardStringContext",
        false,
        false,
        true,
    )?;
    Ok(mk_string(vm, &s))
}

fn prim_get_context(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    use crate::context::ContextElem;
    use std::collections::{BTreeMap, BTreeSet};
    vm.force_string(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.getContext",
    )?;
    #[derive(Default)]
    struct Info {
        path: bool,
        all_outputs: bool,
        outputs: BTreeSet<Vec<u8>>,
    }
    // Keyed by store-path base name (C++ orders a std::map<StorePath,...>).
    let mut infos: BTreeMap<Vec<u8>, Info> = BTreeMap::new();
    for id in vm.read_str_ctx(&val(args[0])) {
        match vm.ctx_elem(id) {
            ContextElem::Opaque { path } => infos.entry(path).or_default().path = true,
            ContextElem::DrvDeep { drv_path } => {
                infos.entry(drv_path).or_default().all_outputs = true
            }
            ContextElem::Built { drv_path, output } => {
                infos.entry(drv_path).or_default().outputs.insert(output);
            }
        }
    }
    let store = vm.store();
    let scope = vm.temp_scope();
    let mut entries: Vec<Attr> = Vec::with_capacity(infos.len());
    for (base, info) in &infos {
        // Inner attrset { allOutputs?; outputs?; path?; }.
        let mut inner: Vec<Attr> = Vec::new();
        if info.all_outputs {
            inner.push(Attr {
                sym: vm.symbols.create(b"allOutputs").0,
                pos: 0,
                val: vm.true_cell,
            });
        }
        if !info.outputs.is_empty() {
            let mut outs: Vec<VRef> = Vec::with_capacity(info.outputs.len());
            for o in &info.outputs {
                let sv = mk_string(vm, o);
                outs.push(temp_cell(vm, sv));
            }
            let lv = vm.new_list_value(&outs);
            inner.push(Attr {
                sym: vm.symbols.create(b"outputs").0,
                pos: 0,
                val: temp_cell(vm, lv),
            });
        }
        if info.path {
            inner.push(Attr {
                sym: vm.symbols.create(b"path").0,
                pos: 0,
                val: vm.true_cell,
            });
        }
        inner.sort_by_key(|a| a.sym);
        let iv = vm.new_bindings_value(&inner);
        let ic = temp_cell(vm, iv);
        // `base` is a store-path base name; reconstruct the full path key.
        let sp = match jinx_store::store_path::StorePath::new(&String::from_utf8_lossy(base)) {
            Ok(sp) => sp,
            Err(_) => continue,
        };
        let key = store.print_store_path(&sp);
        entries.push(Attr {
            sym: vm.symbols.create(key.as_bytes()).0,
            pos: 0,
            val: ic,
        });
    }
    entries.sort_by_key(|a| a.sym);
    entries.dedup_by_key(|a| a.sym);
    let v = vm.new_bindings_value(&entries);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_has_context(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let _ = vm.force_string(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.hasContext",
    )?;
    Ok(Value::bool(!str_ctx(&val(args[0])).is_null()))
}

/// isDerivation on a store-path string: ends in `.drv`.
fn ctx_is_derivation(name: &[u8]) -> bool {
    name.ends_with(b".drv")
}

fn prim_append_context(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    use crate::context::ContextElem;
    vm.force_string(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.appendContext",
    )?;
    let orig = str_bytes(&val(args[0])).to_vec();
    let mut ids = vm.read_str_ctx(&val(args[0]));
    vm.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.appendContext",
    )?;
    let store = vm.store();
    let entries = attrs_entries(&val(args[1])).to_vec();
    for a in &entries {
        let name = vm.symbols.resolve(Symbol(a.sym)).to_vec();
        let name_str = String::from_utf8_lossy(&name).into_owned();
        let sp = match store.parse_store_path(&name_str) {
            Ok(sp) => sp,
            Err(_) => {
                let e = vm.new_err(
                    ErrKind::Eval,
                    format!("context key '{name_str}' is not a store path"),
                    PosIdx(a.pos),
                );
                return Err(e);
            }
        };
        let base = sp.to_string().as_bytes().to_vec();
        vm.force_attrs(a.val, PosIdx(a.pos), "while evaluating the value of a string context")?;
        let av = val(a.val);
        if let Some(pa) = attrs_get(&av, vm.symbols.create(b"path")) {
            let b = vm.force_bool(
                pa.val,
                PosIdx(pa.pos),
                "while evaluating the `path` attribute of a string context",
            )?;
            if b {
                ids.push(vm.intern_elem(&ContextElem::Opaque { path: base.clone() }));
            }
        }
        let av = val(a.val);
        if let Some(aa) = attrs_get(&av, vm.symbols.create(b"allOutputs")) {
            let b = vm.force_bool(
                aa.val,
                PosIdx(aa.pos),
                "while evaluating the `allOutputs` attribute of a string context",
            )?;
            if b {
                if !ctx_is_derivation(&base) {
                    let e = vm.new_err(
                        ErrKind::Eval,
                        format!(
                            "tried to add all-outputs context of {name_str}, which is not a derivation, to a string"
                        ),
                        PosIdx(a.pos),
                    );
                    return Err(e);
                }
                ids.push(vm.intern_elem(&ContextElem::DrvDeep {
                    drv_path: base.clone(),
                }));
            }
        }
        let av = val(a.val);
        if let Some(oa) = attrs_get(&av, vm.symbols.create(b"outputs")) {
            vm.force_list(
                oa.val,
                PosIdx(oa.pos),
                "while evaluating the `outputs` attribute of a string context",
            )?;
            let outs = list_elems(&val(oa.val)).to_vec();
            if !outs.is_empty() && !ctx_is_derivation(&base) {
                let e = vm.new_err(
                    ErrKind::Eval,
                    format!(
                        "tried to add derivation output context of {name_str}, which is not a derivation, to a string"
                    ),
                    PosIdx(a.pos),
                );
                return Err(e);
            }
            for o in &outs {
                let out = vm.force_string_no_ctx(
                    *o,
                    PosIdx(oa.pos),
                    "while evaluating an output name within a string context",
                )?;
                ids.push(vm.intern_elem(&ContextElem::Built {
                    drv_path: base.clone(),
                    output: out,
                }));
            }
        }
    }
    Ok(vm.new_string_ctx(&orig, &dedup_ids(&ids)))
}

fn prim_discard_output_dependency(
    vm: &mut VM,
    _d: &'static PrimOpDef,
    args: &[VRef],
    pos: PosIdx,
) -> R {
    use crate::context::ContextElem;
    let (s, ids) = vm.coerce_to_string(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.unsafeDiscardOutputDependency",
        false,
        false,
        true,
    )?;
    let mut out: Vec<u32> = Vec::with_capacity(ids.len());
    for id in ids {
        let e = vm.ctx_elem(id);
        let ne = match e {
            ContextElem::DrvDeep { drv_path } => ContextElem::Opaque { path: drv_path },
            other => other,
        };
        out.push(vm.intern_elem(&ne));
    }
    Ok(vm.new_string_ctx(&s, &dedup_ids(&out)))
}

fn prim_add_drv_output_dependencies(
    vm: &mut VM,
    _d: &'static PrimOpDef,
    args: &[VRef],
    pos: PosIdx,
) -> R {
    use crate::context::ContextElem;
    let (s, ids) = vm.coerce_to_string(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.addDrvOutputDependencies",
        false,
        false,
        true,
    )?;
    if ids.len() != 1 {
        let e = vm.new_err(
            ErrKind::Eval,
            format!(
                "context of string '{}' must have exactly one element, but has {}",
                String::from_utf8_lossy(&s),
                ids.len()
            ),
            pos,
        );
        return Err(e);
    }
    let store = vm.store();
    let ne = match vm.ctx_elem(ids[0]) {
        ContextElem::Opaque { path } => {
            if !ctx_is_derivation(&path) {
                let printed = jinx_store::store_path::StorePath::new(&String::from_utf8_lossy(&path))
                    .map(|sp| store.print_store_path(&sp))
                    .unwrap_or_else(|_| String::from_utf8_lossy(&path).into_owned());
                let e = vm.new_err(
                    ErrKind::Eval,
                    format!("path '{printed}' is not a derivation"),
                    pos,
                );
                return Err(e);
            }
            ContextElem::DrvDeep { drv_path: path }
        }
        ContextElem::Built { output, .. } => {
            let e = vm.new_err(
                ErrKind::Eval,
                format!(
                    "`addDrvOutputDependencies` can only act on derivations, not on a derivation output such as '{}'",
                    String::from_utf8_lossy(&output)
                ),
                pos,
            );
            return Err(e);
        }
        deep @ ContextElem::DrvDeep { .. } => deep,
    };
    let id = vm.intern_elem(&ne);
    Ok(vm.new_string_ctx(&s, &[id]))
}

/// Deduplicate context ids preserving first-seen order.
fn dedup_ids(ids: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::with_capacity(ids.len());
    for &id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

// ---------------------------------------------------------------------
// derivationStrict / placeholder
// ---------------------------------------------------------------------

/// `hashPlaceholder(name)` = `/` + nix32(sha256("nix-output:" + name)).
fn hash_placeholder(output_name: &[u8]) -> Vec<u8> {
    let mut clear = b"nix-output:".to_vec();
    clear.extend_from_slice(output_name);
    let h = hash_string(HashAlgorithm::Sha256, &clear);
    let mut out = vec![b'/'];
    out.extend_from_slice(h.to_string(HashFormat::Nix32, false).as_bytes());
    out
}

fn prim_placeholder(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let name = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.placeholder",
    )?;
    Ok(mk_string(vm, &hash_placeholder(&name)))
}

fn prim_derivation_strict(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.derivationStrict",
    )?;
    let av = val(args[0]);
    let name_attr = match attrs_get(&av, vm.syms.name) {
        Some(a) => a,
        None => {
            let e = vm.new_err(
                ErrKind::Type,
                "attribute 'name' missing",
                pos,
            );
            vm.add_trace(e, pos, "in the attrset passed as argument to builtins.derivationStrict");
            return Err(e);
        }
    };
    let name_pos = PosIdx(name_attr.pos);
    let drv_name = vm.force_string_no_ctx(
        name_attr.val,
        pos,
        "while evaluating the `name` attribute passed to builtins.derivationStrict",
    )?;
    let r = derivation_strict_internal(vm, &drv_name, args[0], pos);
    r.map_err(|e| {
        let loc = vm
            .positions
            .lookup(name_pos)
            .map(|p| p.to_string())
            .unwrap_or_else(|| "«none»".to_string());
        vm.add_trace(
            e,
            NO_POS,
            format!(
                "while evaluating derivation '{}'\n  whose name attribute is located at {}",
                String::from_utf8_lossy(&drv_name),
                loc
            ),
        );
        e
    })
}

fn derivation_strict_internal(
    vm: &mut VM,
    drv_name: &[u8],
    attrs_cell: VRef,
    pos: PosIdx,
) -> R {
    use jinx_store::derivation::{
        hash_derivation_modulo, Derivation, DerivationOutput, DrvError, DrvHashModulo,
    };
    use jinx_store::store_path::{
        ContentAddress, ContentAddressMethod, FileIngestionMethod, StorePath,
    };

    let drv_name_s = String::from_utf8_lossy(drv_name).into_owned();
    // checkDerivationName.
    if let Err(e) = jinx_store::store_path::check_name(&drv_name_s) {
        return Err(vm.new_err(
            ErrKind::Eval,
            format!("invalid derivation name: {}. Please pass a different 'name'.", e.0),
            pos,
        ));
    }

    // Special control attributes.
    let av = val(attrs_cell);
    let structured = match attrs_get(&av, vm.symbols.create(b"__structuredAttrs")) {
        Some(a) => vm.force_bool(
            a.val,
            pos,
            "while evaluating the `__structuredAttrs` attribute passed to builtins.derivationStrict",
        )?,
        None => false,
    };
    let av = val(attrs_cell);
    let ignore_nulls = match attrs_get(&av, vm.symbols.create(b"__ignoreNulls")) {
        Some(a) => vm.force_bool(
            a.val,
            pos,
            "while evaluating the `__ignoreNulls` attribute passed to builtins.derivationStrict",
        )?,
        None => false,
    };

    let mut drv = Derivation::default();
    drv.name = drv_name_s.clone();
    let mut context: Vec<u32> = Vec::new();
    let mut content_addressed = false;
    let mut is_impure = false;
    let mut output_hash: Option<Vec<u8>> = None;
    let mut output_hash_algo: Option<jinx_store::hash::HashAlgorithm> = None;
    let mut ingestion_method: Option<ContentAddressMethod> = None;
    let mut outputs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    outputs.insert("out".to_string());
    // Collected structuredAttrs JSON members (key -> serialized JSON bytes).
    let mut json_members: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

    // Iterate attrs in lexicographic (symbol string) order.
    let entries = attrs_entries(&val(attrs_cell)).to_vec();
    let mut ordered: Vec<(Vec<u8>, Attr)> = entries
        .iter()
        .map(|a| (vm.symbols.resolve(Symbol(a.sym)).to_vec(), *a))
        .collect();
    ordered.sort_by(|x, y| x.0.cmp(&y.0));

    let ign = vm.symbols.create(b"__ignoreNulls");
    for (key, a) in &ordered {
        if a.sym == ign.0 {
            continue;
        }
        let apos = PosIdx(a.pos);
        // __ignoreNulls: drop null-valued attrs entirely.
        if ignore_nulls {
            vm.force(a.val, apos)?;
            if val(a.val).tag() == Tag::Null {
                continue;
            }
        }
        match key.as_slice() {
            b"__structuredAttrs" => continue,
            b"__contentAddressed" => {
                content_addressed = vm.force_bool(
                    a.val,
                    apos,
                    "while evaluating the `__contentAddressed` attribute passed to builtins.derivationStrict",
                )?;
                if content_addressed && !vm.experimental.ca_derivations {
                    return Err(vm.new_err(
                        ErrKind::Eval,
                        "experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it",
                        pos,
                    ));
                }
                continue;
            }
            b"__impure" => {
                is_impure = vm.force_bool(
                    a.val,
                    apos,
                    "while evaluating the `__impure` attribute passed to builtins.derivationStrict",
                )?;
                if is_impure && !vm.experimental.impure_derivations {
                    return Err(vm.new_err(
                        ErrKind::Eval,
                        "experimental Nix feature 'impure-derivations' is disabled; add '--extra-experimental-features impure-derivations' to enable it",
                        pos,
                    ));
                }
                continue;
            }
            b"args" => {
                vm.force_list(
                    a.val,
                    apos,
                    "while evaluating the `args` attribute passed to builtins.derivationStrict",
                )?;
                let elems = list_elems(&val(a.val)).to_vec();
                for el in &elems {
                    let (s, cids) = vm.coerce_to_string(
                        *el,
                        apos,
                        "while evaluating an element of the argument list",
                        true,
                        true,
                        true,
                    )?;
                    merge_ctx(&mut context, &cids);
                    drv.args.push(s.into());
                }
                continue;
            }
            _ => {}
        }

        // A regular attribute.
        if structured {
            // Serialize the value into the structuredAttrs JSON.
            let mut jbuf = Vec::new();
            crate::json::to_json_ctx(vm, a.val, apos, &mut jbuf, &mut context)?;
            json_members.push((key.clone(), jbuf));
            // Warn that structuredAttrs disables these attributes.
            if let b"allowedReferences" | b"allowedRequisites" | b"disallowedReferences"
            | b"disallowedRequisites" | b"maxSize" | b"maxClosureSize" = key.as_slice()
            {
                let ks = String::from_utf8_lossy(key);
                emit_warning(&format!(
                    "In a derivation named '{drv_name_s}', 'structuredAttrs' disables the effect of the derivation attribute '{ks}'; use 'outputChecks.<output>.{ks}' instead"
                ));
            }
            // Also extract typed fields.
            match key.as_slice() {
                b"builder" => {
                    let (s, cids) = coerce_str_ctx(vm, a.val, apos)?;
                    merge_ctx(&mut context, &cids);
                    drv.builder = s.into();
                }
                b"system" => {
                    drv.platform =
                        vm.force_string_no_ctx(a.val, apos, "")?.into();
                }
                b"outputHash" => {
                    output_hash = Some(vm.force_string_no_ctx(a.val, apos, "")?);
                }
                b"outputHashAlgo" => {
                    let s = vm.force_string_no_ctx(a.val, apos, "")?;
                    output_hash_algo =
                        jinx_store::hash::HashAlgorithm::parse_opt(&String::from_utf8_lossy(&s));
                }
                b"outputHashMode" => {
                    let s = vm.force_string_no_ctx(a.val, apos, "")?;
                    ingestion_method = Some(handle_hash_mode(vm, &s, pos)?);
                }
                b"outputs" => {
                    vm.force_list(a.val, apos, "")?;
                    let elems = list_elems(&val(a.val)).to_vec();
                    let mut ss: Vec<Vec<u8>> = Vec::new();
                    for el in &elems {
                        ss.push(vm.force_string_no_ctx(*el, apos, "")?);
                    }
                    handle_outputs(vm, &ss, &mut outputs, pos)?;
                }
                _ => {}
            }
        } else {
            let (s, cids) = vm.coerce_to_string(a.val, apos, "", true, true, true)?;
            merge_ctx(&mut context, &cids);
            if key.as_slice() == b"__json" {
                drv.env.insert(b"__json".as_slice().into(), s.clone().into());
            } else {
                drv.env.insert(key.as_slice().into(), s.clone().into());
                match key.as_slice() {
                    b"builder" => drv.builder = s.into(),
                    b"system" => drv.platform = s.into(),
                    b"outputHash" => output_hash = Some(s),
                    b"outputHashAlgo" => {
                        output_hash_algo = jinx_store::hash::HashAlgorithm::parse_opt(
                            &String::from_utf8_lossy(&s),
                        )
                    }
                    b"outputHashMode" => ingestion_method = Some(handle_hash_mode(vm, &s, pos)?),
                    b"outputs" => {
                        let ss: Vec<Vec<u8>> = s
                            .split(|c| c.is_ascii_whitespace())
                            .filter(|x| !x.is_empty())
                            .map(|x| x.to_vec())
                            .collect();
                        handle_outputs(vm, &ss, &mut outputs, pos)?;
                    }
                    _ => {}
                }
            }
        }
    }

    // Finish structuredAttrs: build the __json env var.
    if structured {
        json_members.sort_by(|x, y| x.0.cmp(&y.0));
        let mut jbuf = vec![b'{'];
        for (i, (k, v)) in json_members.iter().enumerate() {
            if i > 0 {
                jbuf.push(b',');
            }
            crate::json::json_string_pub(&mut jbuf, k);
            jbuf.push(b':');
            jbuf.extend_from_slice(v);
        }
        jbuf.push(b'}');
        drv.env.insert(b"__json".as_slice().into(), jbuf.into());
    }

    // Context -> inputs. Context paths are store-path base names.
    let store = vm.store();
    let sp_from_base = |b: &[u8]| StorePath::new(&String::from_utf8_lossy(b));
    for id in &context {
        match vm.ctx_elem(*id) {
            crate::context::ContextElem::Built { drv_path, output } => {
                if let Ok(sp) = sp_from_base(&drv_path) {
                    drv.input_drvs
                        .map
                        .entry(sp)
                        .or_default()
                        .value
                        .insert(String::from_utf8_lossy(&output).into_owned());
                }
            }
            crate::context::ContextElem::Opaque { path } => {
                if let Ok(sp) = sp_from_base(&path) {
                    drv.input_srcs.insert(sp);
                }
            }
            crate::context::ContextElem::DrvDeep { drv_path } => {
                if let Ok(sp) = sp_from_base(&drv_path) {
                    // Depend on all outputs of the referenced derivation.
                    if let Some(d) = vm.built_drvs.get(&sp).cloned() {
                        drv.input_srcs.insert(sp.clone());
                        let outs: std::collections::BTreeSet<String> =
                            d.outputs.keys().cloned().collect();
                        drv.input_drvs.map.entry(sp).or_default().value = outs;
                    } else {
                        drv.input_srcs.insert(sp);
                    }
                }
            }
        }
    }

    // Required attributes.
    if drv.builder.is_empty() {
        return Err(vm.new_err(ErrKind::Eval, "required attribute 'builder' missing", pos));
    }
    if drv.platform.is_empty() {
        return Err(vm.new_err(ErrKind::Eval, "required attribute 'system' missing", pos));
    }
    if drv_name.ends_with(b".drv")
        && !(ingestion_method == Some(ContentAddressMethod::Text)
            && outputs.len() == 1
            && outputs.iter().next().map(|s| s.as_str()) == Some("out"))
    {
        return Err(vm.new_err(
            ErrKind::Eval,
            "derivation names are allowed to end in '.drv' only if they produce a single derivation file",
            pos,
        ));
    }

    // Output computation.
    if let Some(oh) = &output_hash {
        // Fixed-output.
        if outputs.len() != 1 || outputs.iter().next().map(|s| s.as_str()) != Some("out") {
            return Err(vm.new_err(
                ErrKind::Eval,
                "multiple outputs are not supported in fixed-output derivations",
                pos,
            ));
        }
        let h = jinx_store::hash::Hash::parse_any(&String::from_utf8_lossy(oh), output_hash_algo)
            .map_err(|e| vm.new_err(ErrKind::Eval, e.0, pos))?;
        let method = ingestion_method.unwrap_or(ContentAddressMethod::Flat);
        let ca = ContentAddress { method, hash: h };
        let dof = DerivationOutput::CAFixed { ca: ca.clone() };
        let p = dof
            .path(&store, &drv_name_s, "out")
            .map_err(|e| vm.new_err(ErrKind::Eval, e.0, pos))?
            .expect("CAFixed path");
        drv.env
            .insert(b"out".as_slice().into(), store.print_store_path(&p).into_bytes().into());
        drv.outputs.insert("out".to_string(), dof);
        let _ = FileIngestionMethod::Flat;
    } else if content_addressed || is_impure {
        if content_addressed && is_impure {
            return Err(vm.new_err(
                ErrKind::Eval,
                "derivation cannot be both content-addressed and impure",
                pos,
            ));
        }
        let ha = output_hash_algo.unwrap_or(jinx_store::hash::HashAlgorithm::Sha256);
        let method = ingestion_method.unwrap_or(ContentAddressMethod::NixArchive);
        for o in &outputs {
            drv.env
                .insert(o.as_bytes().into(), hash_placeholder(o.as_bytes()).into());
            drv.outputs.insert(
                o.clone(),
                if is_impure {
                    DerivationOutput::Impure {
                        method,
                        hash_algo: ha,
                    }
                } else {
                    DerivationOutput::CAFloating {
                        method,
                        hash_algo: ha,
                    }
                },
            );
        }
    } else {
        // Input-addressed: two-pass.
        for o in &outputs {
            drv.env.insert(o.as_bytes().into(), Vec::new().into());
            drv.outputs.insert(o.clone(), DerivationOutput::Deferred);
        }
        // Compute hashDerivationModulo with masked outputs.
        let mut memo = std::mem::take(&mut vm.drv_hashes);
        let built = std::mem::take(&mut vm.built_drvs);
        let hash_res = {
            let mut resolver = |p: &StorePath| -> Result<Derivation, DrvError> {
                built
                    .get(p)
                    .cloned()
                    .ok_or_else(|| DrvError(format!("derivation '{}' is not known", p.to_string())))
            };
            hash_derivation_modulo(&store, &mut memo, &mut resolver, &drv, true)
        };
        vm.drv_hashes = memo;
        vm.built_drvs = built;
        let modulo = hash_res.map_err(|e| vm.new_err(ErrKind::Eval, e.0, pos))?;
        let drv_hash = match modulo {
            DrvHashModulo::DrvHash(h) => h,
            // Deferred: leave outputs as Deferred (dynamic deps). Not exercised.
            _ => {
                return Err(vm.new_err(
                    ErrKind::Eval,
                    "jinx: deferred/dynamic derivation outputs are not supported",
                    pos,
                ))
            }
        };
        for o in &outputs {
            let p = store
                .make_output_path(o, &drv_hash, &drv_name_s)
                .map_err(|e| vm.new_err(ErrKind::Eval, e.0, pos))?;
            drv.env
                .insert(o.as_bytes().into(), store.print_store_path(&p).into_bytes().into());
            drv.outputs
                .insert(o.clone(), DerivationOutput::InputAddressed { path: p });
        }
    }

    // drvPath (readonly: compute, never write).
    let drv_path = drv
        .compute_store_path(&store)
        .map_err(|e| vm.new_err(ErrKind::Eval, e.0, pos))?;

    // Register the derivation's hash and body for later resolution.
    {
        let mut memo = std::mem::take(&mut vm.drv_hashes);
        let built = std::mem::take(&mut vm.built_drvs);
        let h = {
            let mut resolver = |p: &StorePath| -> Result<Derivation, DrvError> {
                built
                    .get(p)
                    .cloned()
                    .ok_or_else(|| DrvError(format!("derivation '{}' is not known", p.to_string())))
            };
            hash_derivation_modulo(&store, &mut memo, &mut resolver, &drv, false)
        };
        vm.drv_hashes = memo;
        vm.built_drvs = built;
        if let Ok(h) = h {
            vm.drv_hashes.insert(drv_path.clone(), h);
        }
    }
    vm.built_drvs.insert(drv_path.clone(), drv.clone());

    // Build the result attrset.
    let drv_path_s = store.print_store_path(&drv_path);
    let drv_base = drv_path.to_string().as_bytes().to_vec();
    let scope = vm.temp_scope();
    let mut result: Vec<Attr> = Vec::with_capacity(1 + drv.outputs.len());
    // drvPath attr.
    let dp_ctx = vm.intern_elem(&crate::context::ContextElem::DrvDeep {
        drv_path: drv_base.clone(),
    });
    let dp_val = vm.new_string_ctx(drv_path_s.as_bytes(), &[dp_ctx]);
    result.push(Attr {
        sym: vm.syms.drv_path.0,
        pos: 0,
        val: temp_cell(vm, dp_val),
    });
    // Output attrs.
    let out_infos: Vec<(String, Vec<u8>)> = drv
        .outputs
        .iter()
        .map(|(name, o)| {
            let s = match o.path(&store, &drv_name_s, name) {
                Ok(Some(p)) => store.print_store_path(&p).into_bytes(),
                _ => hash_placeholder(name.as_bytes()),
            };
            (name.clone(), s)
        })
        .collect();
    for (name, s) in &out_infos {
        let oc = vm.intern_elem(&crate::context::ContextElem::Built {
            drv_path: drv_base.clone(),
            output: name.as_bytes().to_vec(),
        });
        let ov = vm.new_string_ctx(s, &[oc]);
        result.push(Attr {
            sym: vm.symbols.create(name.as_bytes()).0,
            pos: 0,
            val: temp_cell(vm, ov),
        });
    }
    result.sort_by_key(|a| a.sym);
    result.dedup_by_key(|a| a.sym);
    let v = vm.new_bindings_value(&result);
    vm.temp_end(scope);
    Ok(v)
}

// ---------------------------------------------------------------------
// fromTOML
// ---------------------------------------------------------------------

fn toml_subsecond(n: u32) -> String {
    if n == 0 {
        return String::new();
    }
    let s = format!("{n:09}");
    let nanos_part = n % 1000;
    let micros_part = (n / 1000) % 1000;
    let prec = if nanos_part != 0 {
        9
    } else if micros_part != 0 {
        6
    } else {
        3
    };
    s[..prec].to_string()
}

fn toml_normalize_datetime(dt: &toml::value::Datetime) -> String {
    use toml::value::Offset;
    let mut out = String::new();
    if let Some(d) = &dt.date {
        out += &format!("{:04}-{:02}-{:02}", d.year, d.month, d.day);
    }
    if dt.date.is_some() && dt.time.is_some() {
        out.push('T');
    }
    if let Some(t) = &dt.time {
        out += &format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second);
        let sub = toml_subsecond(t.nanosecond);
        if !sub.is_empty() {
            out.push('.');
            out += &sub;
        }
    }
    if let Some(off) = &dt.offset {
        match off {
            Offset::Z => out.push('Z'),
            Offset::Custom { minutes } => {
                let sign = if *minutes < 0 { '-' } else { '+' };
                let m = minutes.unsigned_abs();
                out += &format!("{sign}{:02}:{:02}", m / 60, m % 60);
            }
        }
    }
    out
}

fn toml_to_value(
    vm: &mut VM,
    t: &toml::Value,
    ts: bool,
    pos: PosIdx,
) -> Result<VRef, ErrId> {
    let v = match t {
        toml::Value::Integer(i) => Value::int(*i),
        toml::Value::Float(f) => Value::float(*f),
        toml::Value::Boolean(b) => Value::bool(*b),
        toml::Value::String(s) => mk_string(vm, s.as_bytes()),
        toml::Value::Datetime(dt) => {
            if !ts {
                return Err(vm.new_err(
                    ErrKind::Eval,
                    "while parsing TOML: Dates and times are not supported",
                    pos,
                ));
            }
            let norm = toml_normalize_datetime(dt);
            let tv = mk_string(vm, b"timestamp");
            let tc = temp_cell(vm, tv);
            let vv = mk_string(vm, norm.as_bytes());
            let vc = temp_cell(vm, vv);
            let mut entries = [
                Attr {
                    sym: vm.symbols.create(b"_type").0,
                    pos: 0,
                    val: tc,
                },
                Attr {
                    sym: vm.symbols.create(b"value").0,
                    pos: 0,
                    val: vc,
                },
            ];
            entries.sort_by_key(|a| a.sym);
            vm.new_bindings_value(&entries)
        }
        toml::Value::Array(a) => {
            let mut cells: Vec<VRef> = Vec::with_capacity(a.len());
            for el in a {
                cells.push(toml_to_value(vm, el, ts, pos)?);
            }
            vm.new_list_value(&cells)
        }
        toml::Value::Table(m) => {
            let mut entries: Vec<Attr> = Vec::with_capacity(m.len());
            for (k, val) in m {
                let vc = toml_to_value(vm, val, ts, pos)?;
                entries.push(Attr {
                    sym: vm.symbols.create(k.as_bytes()).0,
                    pos: 0,
                    val: vc,
                });
            }
            entries.sort_by_key(|a| a.sym);
            entries.dedup_by_key(|a| a.sym);
            vm.new_bindings_value(&entries)
        }
    };
    Ok(temp_cell(vm, v))
}

fn prim_from_toml(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let s = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.fromTOML",
    )?;
    let text = match std::str::from_utf8(&s) {
        Ok(t) => t,
        Err(_) => {
            return Err(vm.new_err(
                ErrKind::Eval,
                "while parsing TOML: invalid UTF-8",
                pos,
            ))
        }
    };
    let parsed: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return Err(vm.new_err(
                ErrKind::Eval,
                format!("while parsing TOML: {}", e.message()),
                pos,
            ))
        }
    };
    let ts = vm.experimental.parse_toml_timestamps;
    let scope = vm.temp_scope();
    let r = toml_to_value(vm, &parsed, ts, pos);
    let out = r.map(val);
    vm.temp_end(scope);
    out
}

// ---------------------------------------------------------------------
// regex: match / split
// ---------------------------------------------------------------------

fn get_regex(
    vm: &mut VM,
    re: &[u8],
    pos: PosIdx,
) -> Result<std::rc::Rc<crate::regex::Regex>, ErrId> {
    if let Some(r) = vm.regex_cache.get(re) {
        return Ok(r.clone());
    }
    match crate::regex::Regex::compile(re) {
        Ok(r) => {
            let rc = std::rc::Rc::new(r);
            vm.regex_cache.insert(re.to_vec(), rc.clone());
            Ok(rc)
        }
        Err(_) => Err(vm.new_err(
            ErrKind::Eval,
            format!(
                "invalid regular expression '{}'",
                String::from_utf8_lossy(re)
            ),
            pos,
        )),
    }
}

fn prim_match(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let re = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.match",
    )?;
    // Subject string: context allowed but discarded.
    let s = vm.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.match",
    )?;
    let rx = get_regex(vm, &re, pos)?;
    match rx.match_full(&s) {
        None => Ok(Value::null()),
        Some(groups) => {
            let scope = vm.temp_scope();
            let mut cells: Vec<VRef> = Vec::with_capacity(groups.len());
            for g in &groups {
                let c = match g {
                    Some((a, b)) => {
                        let sv = mk_string(vm, &s[*a..*b]);
                        temp_cell(vm, sv)
                    }
                    None => vm.null_cell,
                };
                cells.push(c);
            }
            let v = vm.new_list_value(&cells);
            vm.temp_end(scope);
            Ok(v)
        }
    }
}

fn prim_split(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let re = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.split",
    )?;
    let s = vm.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.split",
    )?;
    let rx = get_regex(vm, &re, pos)?;
    let matches = rx.find_iter(&s);
    if matches.is_empty() {
        // Return [ str ] preserving the original value (and its context).
        return Ok(vm.new_list_value(&[args[1]]));
    }
    let scope = vm.temp_scope();
    let mut cells: Vec<VRef> = Vec::with_capacity(2 * matches.len() + 1);
    let mut last = 0usize;
    for (mi, m) in matches.iter().enumerate() {
        // Non-matching prefix segment.
        let seg = mk_string(vm, &s[last..m.start]);
        cells.push(temp_cell(vm, seg));
        // Capture-group sublist.
        let mut groups: Vec<VRef> = Vec::with_capacity(m.groups.len());
        for g in &m.groups {
            let c = match g {
                Some((a, b)) => {
                    let sv = mk_string(vm, &s[*a..*b]);
                    temp_cell(vm, sv)
                }
                None => vm.null_cell,
            };
            groups.push(c);
        }
        let lv = vm.new_list_value(&groups);
        cells.push(temp_cell(vm, lv));
        last = m.end;
        // Trailing suffix after the last match.
        if mi + 1 == matches.len() {
            let seg = mk_string(vm, &s[last..]);
            cells.push(temp_cell(vm, seg));
        }
    }
    let v = vm.new_list_value(&cells);
    vm.temp_end(scope);
    Ok(v)
}

fn emit_warning(msg: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "warning: {msg}");
}

fn merge_ctx(dst: &mut Vec<u32>, src: &[u32]) {
    for &id in src {
        if !dst.contains(&id) {
            dst.push(id);
        }
    }
}

fn coerce_str_ctx(vm: &mut VM, cell: VRef, pos: PosIdx) -> Result<(Vec<u8>, Vec<u32>), ErrId> {
    vm.coerce_to_string(cell, pos, "", true, true, true)
}

fn handle_hash_mode(
    vm: &mut VM,
    s: &[u8],
    pos: PosIdx,
) -> Result<jinx_store::store_path::ContentAddressMethod, ErrId> {
    use jinx_store::store_path::ContentAddressMethod as M;
    let m = match s {
        b"recursive" | b"nar" => M::NixArchive,
        b"flat" => M::Flat,
        b"text" => M::Text,
        b"git" => M::Git,
        _ => {
            return Err(vm.new_err(
                ErrKind::Eval,
                format!(
                    "invalid value '{}' for 'outputHashMode' attribute",
                    String::from_utf8_lossy(s)
                ),
                pos,
            ))
        }
    };
    Ok(m)
}

fn handle_outputs(
    vm: &mut VM,
    ss: &[Vec<u8>],
    outputs: &mut std::collections::BTreeSet<String>,
    pos: PosIdx,
) -> Result<(), ErrId> {
    outputs.clear();
    for j in ss {
        let name = String::from_utf8_lossy(j).into_owned();
        if name == "drvPath" {
            return Err(vm.new_err(
                ErrKind::Eval,
                "invalid derivation output name 'drvPath'",
                pos,
            ));
        }
        if !outputs.insert(name.clone()) {
            return Err(vm.new_err(
                ErrKind::Eval,
                format!("duplicate derivation output '{name}'"),
                pos,
            ));
        }
    }
    if outputs.is_empty() {
        return Err(vm.new_err(
            ErrKind::Eval,
            "derivation cannot have an empty set of outputs",
            pos,
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// versions / names
// ---------------------------------------------------------------------

fn version_components(s: &[u8]) -> Vec<Vec<u8>> {
    let mut comps = Vec::new();
    let mut i = 0;
    while i < s.len() {
        while i < s.len() && (s[i] == b'.' || s[i] == b'-') {
            i += 1;
        }
        if i >= s.len() {
            break;
        }
        let start = i;
        if s[i].is_ascii_digit() {
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
        } else {
            while i < s.len() && !s[i].is_ascii_digit() && s[i] != b'.' && s[i] != b'-' {
                i += 1;
            }
        }
        comps.push(s[start..i].to_vec());
    }
    comps
}

fn component_cmp(c1: &[u8], c2: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let n1: Option<i64> = std::str::from_utf8(c1).ok().and_then(|s| s.parse().ok());
    let n2: Option<i64> = std::str::from_utf8(c2).ok().and_then(|s| s.parse().ok());
    match (n1, n2) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => {
            if c1.is_empty() && n2.is_some() {
                Less
            } else if c2.is_empty() && n1.is_some() {
                Greater
            } else if c1 == b"pre" && c2 != b"pre" {
                Less
            } else if c2 == b"pre" && c1 != b"pre" {
                Greater
            } else if n1.is_some() {
                Greater
            } else if n2.is_some() {
                Less
            } else {
                c1.cmp(c2)
            }
        }
    }
}

fn compare_versions_bytes(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let (ca, cb) = (version_components(a), version_components(b));
    let n = ca.len().max(cb.len());
    for i in 0..n {
        let empty: Vec<u8> = Vec::new();
        let x = ca.get(i).unwrap_or(&empty);
        let y = cb.get(i).unwrap_or(&empty);
        let o = component_cmp(x, y);
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}

fn prim_compare_versions(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let a = vm.force_string(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.compareVersions",
    )?;
    let b = vm.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.compareVersions",
    )?;
    let r = match compare_versions_bytes(&a, &b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::int(r))
}

fn prim_split_version(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let s = vm.force_string(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.splitVersion",
    )?;
    let comps = version_components(&s);
    let scope = vm.temp_scope();
    let mut cells: Vec<VRef> = Vec::with_capacity(comps.len());
    for c in &comps {
        let v = mk_string(vm, c);
        cells.push(temp_cell(vm, v));
    }
    let v = vm.new_list_value(&cells);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_parse_drv_name(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let s = vm.force_string(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.parseDrvName",
    )?;
    let mut name: &[u8] = &s;
    let mut version: &[u8] = b"";
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'-' && i + 1 < s.len() && !s[i + 1].is_ascii_alphabetic() {
            name = &s[..i];
            version = &s[i + 1..];
            break;
        }
        i += 1;
    }
    let scope = vm.temp_scope();
    let nv = mk_string(vm, name);
    let nc = temp_cell(vm, nv);
    let vv = mk_string(vm, version);
    let vc = temp_cell(vm, vv);
    let name_sym = vm.syms.name;
    let version_sym = vm.symbols.create(b"version");
    let mut entries = [
        Attr { sym: name_sym.0, pos: 0, val: nc },
        Attr { sym: version_sym.0, pos: 0, val: vc },
    ];
    entries.sort_by_key(|a| a.sym);
    let v = vm.new_bindings_value(&entries);
    vm.temp_end(scope);
    Ok(v)
}

// ---------------------------------------------------------------------
// lists
// ---------------------------------------------------------------------

fn prim_length(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.length",
    )?;
    Ok(Value::int(list_elems(&val(args[0])).len() as i64))
}

fn prim_head(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to 'builtins.head'",
    )?;
    let elems = list_elems(&val(args[0]));
    if elems.is_empty() {
        return Err(vm.new_err(ErrKind::Eval, "'builtins.head' called on an empty list", pos));
    }
    vm.force(elems[0], pos)?;
    Ok(val(elems[0]))
}

fn prim_tail(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to 'builtins.tail'",
    )?;
    let elems = list_elems(&val(args[0]));
    if elems.is_empty() {
        return Err(vm.new_err(ErrKind::Eval, "'builtins.tail' called on an empty list", pos));
    }
    Ok(vm.new_list_value(&elems[1..]))
}

fn prim_elem_at(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let n = vm.force_int(
        args[1],
        pos,
        "while evaluating the second argument passed to 'builtins.elemAt'",
    )?;
    vm.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to 'builtins.elemAt'",
    )?;
    let elems = list_elems(&val(args[0]));
    if n < 0 || n as usize >= elems.len() {
        let msg = format!(
            "'builtins.elemAt' called with index {} on a list of size {}",
            n,
            elems.len()
        );
        return Err(vm.new_err(ErrKind::Eval, msg, pos));
    }
    let c = elems[n as usize];
    vm.force(c, pos)?;
    Ok(val(c))
}

fn prim_elem(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.elem",
    )?;
    let elems = list_elems(&val(args[1]));
    for &el in elems {
        if vm.eq_values(
            args[0],
            el,
            pos,
            "while searching for the presence of the given element in the list",
            false,
        )? {
            return Ok(Value::bool(true));
        }
    }
    Ok(Value::bool(false))
}

fn prim_map(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.map",
    )?;
    let elems = list_elems(&val(args[1]));
    if elems.is_empty() {
        return Ok(val(args[1]));
    }
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.map",
    )?;
    let scope = vm.temp_scope();
    let mut out: Vec<VRef> = Vec::with_capacity(elems.len());
    let elems: Vec<VRef> = elems.to_vec();
    for el in elems {
        let t = vm.new_apply_thunk(args[0], &[el]);
        out.push(temp_cell(vm, t));
    }
    let v = vm.new_list_value(&out);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_filter(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.filter",
    )?;
    let elems = list_elems(&val(args[1])).to_vec();
    if elems.is_empty() {
        return Ok(val(args[1]));
    }
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.filter",
    )?;
    let mut out: Vec<VRef> = Vec::new();
    let mut same = true;
    for &el in &elems {
        let r = vm.call_function(args[0], &[el], pos)?;
        let rc = vm.alloc_cell(r);
        let scope = vm.temp_scope();
        vm.temp_roots.push(rc);
        let keep = vm.force_bool(
            rc,
            pos,
            "while evaluating the return value of the filtering function passed to builtins.filter",
        );
        vm.temp_end(scope);
        if keep? {
            out.push(el);
        } else {
            same = false;
        }
    }
    if same {
        Ok(val(args[1]))
    } else {
        // Elements stay rooted via args[1].
        Ok(vm.new_list_value(&out))
    }
}

fn call_bool(vm: &mut VM, f: VRef, arg: VRef, pos: PosIdx, ctx: &str) -> Result<bool, ErrId> {
    let r = vm.call_function(f, &[arg], pos)?;
    let rc = vm.alloc_cell(r);
    let scope = vm.temp_scope();
    vm.temp_roots.push(rc);
    let b = vm.force_bool(rc, pos, ctx);
    vm.temp_end(scope);
    b
}

fn any_all(vm: &mut VM, args: &[VRef], pos: PosIdx, any: bool) -> R {
    let name = if any { "any" } else { "all" };
    force_fun(
        vm,
        args[0],
        pos,
        &format!("while evaluating the first argument passed to builtins.{name}"),
    )?;
    vm.force_list(
        args[1],
        pos,
        &format!("while evaluating the second argument passed to builtins.{name}"),
    )?;
    let elems = list_elems(&val(args[1])).to_vec();
    for el in elems {
        let b = call_bool(
            vm,
            args[0],
            el,
            pos,
            &format!(
                "while evaluating the return value of the function passed to builtins.{name}"
            ),
        )?;
        if b == any {
            return Ok(Value::bool(any));
        }
    }
    Ok(Value::bool(!any))
}

fn prim_any(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    any_all(vm, args, pos, true)
}

fn prim_all(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    any_all(vm, args, pos, false)
}

fn prim_concat_lists(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.concatLists",
    )?;
    let lists = list_elems(&val(args[0])).to_vec();
    let mut out: Vec<VRef> = Vec::new();
    for l in &lists {
        vm.force_list(
            *l,
            pos,
            "while evaluating a value of the list passed to builtins.concatLists",
        )?;
        out.extend_from_slice(list_elems(&val(*l)));
    }
    // All elements rooted via args[0]'s sublists.
    Ok(vm.new_list_value(&out))
}

fn prim_concat_map(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.concatMap",
    )?;
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.concatMap",
    )?;
    let elems = list_elems(&val(args[1])).to_vec();
    let scope = vm.temp_scope();
    let mut results: Vec<VRef> = Vec::with_capacity(elems.len());
    for el in &elems {
        let r = vm.call_function(args[0], &[*el], pos)?;
        let rc = temp_cell(vm, r);
        vm.force_list(
            rc,
            pos,
            "while evaluating the return value of the function passed to builtins.concatMap",
        )?;
        results.push(rc);
    }
    let mut out: Vec<VRef> = Vec::new();
    for rc in &results {
        out.extend_from_slice(list_elems(&val(*rc)));
    }
    let v = vm.new_list_value(&out);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_foldl_strict(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.foldlStrict",
    )?;
    vm.force_list(
        args[2],
        pos,
        "while evaluating the third argument passed to builtins.foldlStrict",
    )?;
    let elems = list_elems(&val(args[2])).to_vec();
    if elems.is_empty() {
        vm.force(args[1], pos)?;
        return Ok(val(args[1]));
    }
    let scope = vm.temp_scope();
    let mut cur = args[1];
    for el in elems {
        let r = vm.call_function(args[0], &[cur, el], pos)?;
        cur = temp_cell(vm, r);
    }
    let r = vm.force(cur, pos);
    let out = val(cur);
    vm.temp_end(scope);
    r?;
    Ok(out)
}

fn prim_gen_list(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let len = vm.force_int(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.genList",
    )?;
    if len < 0 {
        let msg = format!("cannot create list of size {}", len);
        return Err(vm.new_err(ErrKind::Eval, msg, pos));
    }
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.genList",
    )?;
    let scope = vm.temp_scope();
    let mut out: Vec<VRef> = Vec::with_capacity(len as usize);
    for n in 0..len {
        let idx = temp_cell(vm, Value::int(n));
        let t = vm.new_apply_thunk(args[0], &[idx]);
        out.push(temp_cell(vm, t));
    }
    let v = vm.new_list_value(&out);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_sort(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.sort",
    )?;
    let elems = list_elems(&val(args[1])).to_vec();
    if elems.is_empty() {
        return Ok(val(args[1]));
    }
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.sort",
    )?;
    for &el in &elems {
        vm.force(el, pos)?;
    }
    // Is the comparator builtins.lessThan? Then bypass callFunction.
    let fv = val(args[0]);
    let is_less_than = fv.tag() == Tag::PrimOp && crate::vm::primop_of(&fv).name == "__lessThan";

    let mut items = elems;
    // Stable merge sort with a fallible comparator.
    let mut cmp = |vm: &mut VM, a: VRef, b: VRef| -> Result<bool, ErrId> {
        if is_less_than {
            compare_values(
                vm,
                a,
                b,
                NO_POS,
                "while evaluating the ordering function passed to builtins.sort",
            )
        } else {
            let r = vm.call_function(args[0], &[a, b], NO_POS)?;
            let rc = vm.alloc_cell(r);
            let scope = vm.temp_scope();
            vm.temp_roots.push(rc);
            let b = vm.force_bool(
                rc,
                pos,
                "while evaluating the return value of the sorting function passed to builtins.sort",
            );
            vm.temp_end(scope);
            b
        }
    };
    merge_sort(vm, &mut items, &mut cmp)?;
    // Elements rooted via args[1].
    Ok(vm.new_list_value(&items))
}

fn merge_sort(
    vm: &mut VM,
    items: &mut [VRef],
    less: &mut impl FnMut(&mut VM, VRef, VRef) -> Result<bool, ErrId>,
) -> Result<(), ErrId> {
    let n = items.len();
    if n <= 1 {
        return Ok(());
    }
    let mid = n / 2;
    let mut left = items[..mid].to_vec();
    let mut right = items[mid..].to_vec();
    merge_sort(vm, &mut left, less)?;
    merge_sort(vm, &mut right, less)?;
    let (mut i, mut j, mut k) = (0, 0, 0);
    while i < left.len() && j < right.len() {
        // stable: take left unless right < left
        if less(vm, right[j], left[i])? {
            items[k] = right[j];
            j += 1;
        } else {
            items[k] = left[i];
            i += 1;
        }
        k += 1;
    }
    while i < left.len() {
        items[k] = left[i];
        i += 1;
        k += 1;
    }
    while j < right.len() {
        items[k] = right[j];
        j += 1;
        k += 1;
    }
    Ok(())
}

fn prim_partition(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.partition",
    )?;
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.partition",
    )?;
    let elems = list_elems(&val(args[1])).to_vec();
    let mut right: Vec<VRef> = Vec::new();
    let mut wrong: Vec<VRef> = Vec::new();
    for el in &elems {
        let b = call_bool(
            vm,
            args[0],
            *el,
            pos,
            "while evaluating the return value of the partition function passed to builtins.partition",
        )?;
        if b {
            right.push(*el);
        } else {
            wrong.push(*el);
        }
    }
    let scope = vm.temp_scope();
    let rv = vm.new_list_value(&right);
    let rc = temp_cell(vm, rv);
    let wv = vm.new_list_value(&wrong);
    let wc = temp_cell(vm, wv);
    let right_sym = vm.symbols.create(b"right");
    let wrong_sym = vm.symbols.create(b"wrong");
    let mut entries = [
        Attr { sym: right_sym.0, pos: 0, val: rc },
        Attr { sym: wrong_sym.0, pos: 0, val: wc },
    ];
    entries.sort_by_key(|a| a.sym);
    let v = vm.new_bindings_value(&entries);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_group_by(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.groupBy",
    )?;
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.groupBy",
    )?;
    let elems = list_elems(&val(args[1])).to_vec();
    let mut groups: Vec<(Symbol, Vec<VRef>)> = Vec::new();
    for el in &elems {
        let r = vm.call_function(args[0], &[*el], pos)?;
        let rc = vm.alloc_cell(r);
        let scope = vm.temp_scope();
        vm.temp_roots.push(rc);
        let name = vm.force_string_no_ctx(
            rc,
            pos,
            "while evaluating the return value of the grouping function passed to builtins.groupBy",
        );
        vm.temp_end(scope);
        let name = name?;
        let sym = vm.symbols.create(&name);
        match groups.iter_mut().find(|(s, _)| *s == sym) {
            Some((_, v)) => v.push(*el),
            None => groups.push((sym, vec![*el])),
        }
    }
    groups.sort_by_key(|(s, _)| s.0);
    let scope = vm.temp_scope();
    let mut entries: Vec<Attr> = Vec::with_capacity(groups.len());
    for (sym, members) in &groups {
        let lv = vm.new_list_value(members);
        let lc = temp_cell(vm, lv);
        entries.push(Attr {
            sym: sym.0,
            pos: 0,
            val: lc,
        });
    }
    let v = vm.new_bindings_value(&entries);
    vm.temp_end(scope);
    Ok(v)
}

// ---------------------------------------------------------------------
// attrsets
// ---------------------------------------------------------------------

fn prim_attr_names(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.attrNames",
    )?;
    let mut names: Vec<Vec<u8>> = attrs_entries(&val(args[0]))
        .iter()
        .map(|a| vm.symbols.resolve(Symbol(a.sym)).to_vec())
        .collect();
    names.sort();
    let scope = vm.temp_scope();
    let mut cells: Vec<VRef> = Vec::with_capacity(names.len());
    for n in &names {
        let v = mk_string(vm, n);
        cells.push(temp_cell(vm, v));
    }
    let v = vm.new_list_value(&cells);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_attr_values(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.attrValues",
    )?;
    let mut pairs: Vec<(Vec<u8>, VRef)> = attrs_entries(&val(args[0]))
        .iter()
        .map(|a| (vm.symbols.resolve(Symbol(a.sym)).to_vec(), a.val))
        .collect();
    pairs.sort_by(|x, y| x.0.cmp(&y.0));
    let cells: Vec<VRef> = pairs.into_iter().map(|(_, c)| c).collect();
    Ok(vm.new_list_value(&cells))
}

fn prim_get_attr(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let name = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.getAttr",
    )?;
    vm.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.getAttr",
    )?;
    let sym = vm.symbols.create(&name);
    match attrs_get(&val(args[1]), sym) {
        Some(a) => {
            vm.force(a.val, pos)?;
            Ok(val(a.val))
        }
        None => {
            let msg = format!("attribute '{}' missing", String::from_utf8_lossy(&name));
            let e = vm.new_err(ErrKind::Type, msg, pos);
            vm.add_trace(e, NO_POS, "in the attribute set under consideration");
            Err(e)
        }
    }
}

fn prim_has_attr(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let name = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hasAttr",
    )?;
    vm.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.hasAttr",
    )?;
    let sym = vm.symbols.create(&name);
    Ok(Value::bool(attrs_get(&val(args[1]), sym).is_some()))
}

fn prim_remove_attrs(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.removeAttrs",
    )?;
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.removeAttrs",
    )?;
    let name_cells = list_elems(&val(args[1])).to_vec();
    let mut to_remove: Vec<u32> = Vec::with_capacity(name_cells.len());
    for c in name_cells {
        let name = vm.force_string(
            c,
            pos,
            "while evaluating the values of the second argument passed to builtins.removeAttrs",
        )?;
        to_remove.push(vm.symbols.create(&name).0);
    }
    let entries: Vec<Attr> = attrs_entries(&val(args[0]))
        .iter()
        .filter(|a| !to_remove.contains(&a.sym))
        .copied()
        .collect();
    if entries.len() == attrs_entries(&val(args[0])).len() {
        return Ok(val(args[0]));
    }
    Ok(vm.new_bindings_value(&entries))
}

fn prim_list_to_attrs(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.listToAttrs",
    )?;
    let elems = list_elems(&val(args[0])).to_vec();
    // (sym, list index, element)
    let mut named: Vec<(u32, usize, VRef)> = Vec::with_capacity(elems.len());
    for (n, el) in elems.iter().enumerate() {
        vm.force_attrs(
            *el,
            pos,
            "while evaluating an element of the list passed to builtins.listToAttrs",
        )?;
        let j = match attrs_get(&val(*el), vm.syms.name) {
            Some(a) => a,
            None => {
                let e = vm.new_err(ErrKind::Type, "attribute 'name' missing", pos);
                vm.add_trace(e, NO_POS, "in a {name=...; value=...;} pair");
                return Err(e);
            }
        };
        let name = vm.force_string_no_ctx(
            j.val,
            PosIdx(j.pos),
            "while evaluating the `name` attribute of an element of the list passed to builtins.listToAttrs",
        )?;
        named.push((vm.symbols.create(&name).0, n, *el));
    }
    named.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut entries: Vec<Attr> = Vec::with_capacity(named.len());
    let mut prev: Option<u32> = None;
    for (sym, _, el) in named {
        if prev == Some(sym) {
            continue;
        }
        prev = Some(sym);
        let j = match attrs_get(&val(el), vm.syms.value) {
            Some(a) => a,
            None => {
                let e = vm.new_err(ErrKind::Type, "attribute 'value' missing", pos);
                vm.add_trace(e, NO_POS, "in a {name=...; value=...;} pair");
                return Err(e);
            }
        };
        entries.push(Attr {
            sym,
            pos: j.pos,
            val: j.val,
        });
    }
    Ok(vm.new_bindings_value(&entries))
}

fn prim_map_attrs(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.mapAttrs",
    )?;
    let entries_in = attrs_entries(&val(args[1])).to_vec();
    let scope = vm.temp_scope();
    let mut entries: Vec<Attr> = Vec::with_capacity(entries_in.len());
    for a in &entries_in {
        let name_bytes = vm.symbols.resolve(Symbol(a.sym)).to_vec();
        let nv = mk_string(vm, &name_bytes);
        let nc = temp_cell(vm, nv);
        let t = vm.new_apply_thunk(args[0], &[nc, a.val]);
        let tc = temp_cell(vm, t);
        entries.push(Attr {
            sym: a.sym,
            pos: a.pos,
            val: tc,
        });
    }
    let v = vm.new_bindings_value(&entries);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_intersect_attrs(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.intersectAttrs",
    )?;
    vm.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.intersectAttrs",
    )?;
    let e1 = attrs_entries(&val(args[0]));
    let e2 = attrs_entries(&val(args[1]));
    let entries: Vec<Attr> = e2
        .iter()
        .filter(|a| e1.binary_search_by(|x| x.sym.cmp(&a.sym)).is_ok())
        .copied()
        .collect();
    Ok(vm.new_bindings_value(&entries))
}

fn prim_cat_attrs(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let name = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.catAttrs",
    )?;
    let sym = vm.symbols.create(&name);
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.catAttrs",
    )?;
    let elems = list_elems(&val(args[1])).to_vec();
    let mut out: Vec<VRef> = Vec::new();
    for el in elems {
        vm.force_attrs(
            el,
            pos,
            "while evaluating an element in the list passed to builtins.catAttrs",
        )?;
        if let Some(a) = attrs_get(&val(el), sym) {
            out.push(a.val);
        }
    }
    Ok(vm.new_list_value(&out))
}

fn prim_function_args(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force(args[0], pos)?;
    let v = val(args[0]);
    if v.tag() == Tag::Attrs {
        if let Some(f) = attrs_get(&v, vm.syms.functor) {
            let r = vm.call_function(f.val, &[args[0]], pos)?;
            let rc = vm.alloc_cell(r);
            let scope = vm.temp_scope();
            vm.temp_roots.push(rc);
            let def: &'static PrimOpDef = _d;
            let out = prim_function_args(vm, def, &[rc], pos);
            vm.temp_end(scope);
            return out;
        }
    }
    if !matches!(v.tag(), Tag::Closure | Tag::PrimOp | Tag::PrimOpApp) {
        return Err(vm.new_err(ErrKind::Type, "'functionArgs' requires a function", pos));
    }
    if v.tag() != Tag::Closure {
        return Ok(vm.new_bindings_value(&[]));
    }
    let (code, _) = crate::vm::thunk_code(&v);
    let chunk = code.chunk();
    let spec = chunk.lambda.as_ref().unwrap();
    let Some(formals) = &spec.formals else {
        return Ok(vm.new_bindings_value(&[]));
    };
    let mut entries: Vec<Attr> = formals
        .formals
        .iter()
        .map(|f| Attr {
            sym: f.name.0,
            pos: f.pos.0,
            val: if f.default.is_some() {
                vm.true_cell
            } else {
                vm.false_cell
            },
        })
        .collect();
    entries.sort_by_key(|a| a.sym);
    Ok(vm.new_bindings_value(&entries))
}

fn prim_unsafe_get_attr_pos(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let name = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.unsafeGetAttrPos",
    )?;
    vm.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.unsafeGetAttrPos",
    )?;
    let sym = vm.symbols.create(&name);
    match attrs_get(&val(args[1]), sym) {
        Some(a) => Ok(vm.mk_pos(PosIdx(a.pos))),
        None => Ok(Value::null()),
    }
}

fn prim_generic_closure(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_attrs(
        args[0],
        NO_POS,
        "while evaluating the first argument passed to builtins.genericClosure",
    )?;
    let start_set_sym = vm.symbols.create(b"startSet");
    let operator_sym = vm.symbols.create(b"operator");
    let key_sym = vm.symbols.create(b"key");
    let attrs = val(args[0]);
    let Some(start) = attrs_get(&attrs, start_set_sym) else {
        let e = vm.new_err(ErrKind::Type, "attribute 'startSet' missing", pos);
        vm.add_trace(
            e,
            NO_POS,
            "in the attrset passed as argument to builtins.genericClosure",
        );
        return Err(e);
    };
    vm.force_list(
        start.val,
        NO_POS,
        "while evaluating the 'startSet' attribute passed as argument to builtins.genericClosure",
    )?;
    if list_elems(&val(start.val)).is_empty() {
        return Ok(val(start.val));
    }
    let Some(op) = attrs_get(&attrs, operator_sym) else {
        let e = vm.new_err(ErrKind::Type, "attribute 'operator' missing", pos);
        vm.add_trace(
            e,
            NO_POS,
            "in the attrset passed as argument to builtins.genericClosure",
        );
        return Err(e);
    };
    force_fun(
        vm,
        op.val,
        NO_POS,
        "while evaluating the 'operator' attribute passed as argument to builtins.genericClosure",
    )?;

    let scope = vm.temp_scope();
    let mut work: std::collections::VecDeque<VRef> =
        list_elems(&val(start.val)).iter().copied().collect();
    let mut done_keys: Vec<VRef> = Vec::new();
    let mut res: Vec<VRef> = Vec::new();
    while let Some(e) = work.pop_front() {
        vm.force_attrs(e, NO_POS, "")?;
        let ev = val(e);
        let Some(key) = attrs_get(&ev, key_sym) else {
            let er = vm.new_err(ErrKind::Type, "attribute 'key' missing", pos);
            vm.add_trace(
                er,
                NO_POS,
                "in one of the attrsets generated by (or initially passed to) builtins.genericClosure",
            );
            vm.temp_end(scope);
            return Err(er);
        };
        vm.force(key.val, NO_POS)?;
        // Insert into done set (ordered by CompareValues).
        let mut lo = 0usize;
        let mut hi = done_keys.len();
        let mut is_dup = false;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if compare_values(vm, done_keys[mid], key.val, NO_POS, "")? {
                lo = mid + 1;
            } else if compare_values(vm, key.val, done_keys[mid], NO_POS, "")? {
                hi = mid;
            } else {
                is_dup = true;
                break;
            }
        }
        if is_dup {
            continue;
        }
        done_keys.insert(lo, key.val);
        vm.temp_roots.push(key.val);
        res.push(e);
        vm.temp_roots.push(e);

        // Call the operator and queue new elements.
        let r = vm.call_function(op.val, &[e], NO_POS)?;
        let rc = temp_cell(vm, r);
        vm.force_list(
            rc,
            NO_POS,
            "while evaluating the return value of the `operator` passed to builtins.genericClosure",
        )?;
        for &n in list_elems(&val(rc)) {
            work.push_back(n);
            vm.temp_roots.push(n);
        }
    }
    let v = vm.new_list_value(&res);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_zip_attrs_with(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.zipAttrsWith",
    )?;
    vm.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.zipAttrsWith",
    )?;
    let lists = list_elems(&val(args[1])).to_vec();
    let mut buckets: Vec<(u32, Vec<VRef>)> = Vec::new();
    for el in &lists {
        vm.force_attrs(
            *el,
            NO_POS,
            "while evaluating a value of the list passed as second argument to builtins.zipAttrsWith",
        )?;
        for a in attrs_entries(&val(*el)) {
            match buckets.iter_mut().find(|(s, _)| *s == a.sym) {
                Some((_, v)) => v.push(a.val),
                None => buckets.push((a.sym, vec![a.val])),
            }
        }
    }
    buckets.sort_by_key(|(s, _)| *s);
    let scope = vm.temp_scope();
    let mut entries: Vec<Attr> = Vec::with_capacity(buckets.len());
    for (sym, vals) in &buckets {
        let name_bytes = vm.symbols.resolve(Symbol(*sym)).to_vec();
        let nv = mk_string(vm, &name_bytes);
        let nc = temp_cell(vm, nv);
        let lv = vm.new_list_value(vals);
        let lc = temp_cell(vm, lv);
        let t = vm.new_apply_thunk(args[0], &[nc, lc]);
        let tc = temp_cell(vm, t);
        entries.push(Attr {
            sym: *sym,
            pos: 0,
            val: tc,
        });
    }
    let v = vm.new_bindings_value(&entries);
    vm.temp_end(scope);
    Ok(v)
}

// ---------------------------------------------------------------------
// paths / files / env
// ---------------------------------------------------------------------

fn coerce_to_path(vm: &mut VM, cell: VRef, pos: PosIdx, ctx: &str) -> Result<Vec<u8>, ErrId> {
    vm.force(cell, pos).map_err(|e| {
        vm.add_trace(e, pos, ctx);
        e
    })?;
    let v = val(cell);
    if v.tag() == Tag::Path {
        return Ok(path_bytes(&v).to_vec());
    }
    if v.tag() == Tag::Attrs {
        if let Some(f) = attrs_get(&v, vm.syms.to_string) {
            let r = vm.call_function(f.val, &[cell], pos)?;
            let rc = vm.alloc_cell(r);
            let scope = vm.temp_scope();
            vm.temp_roots.push(rc);
            let out = coerce_to_path(vm, rc, pos, ctx);
            vm.temp_end(scope);
            return out;
        }
    }
    let (s, _) = vm.coerce_to_string(cell, pos, ctx, false, false, true)?;
    if s.is_empty() || s[0] != b'/' {
        let msg = format!(
            "string '{}' doesn't represent an absolute path",
            String::from_utf8_lossy(&s)
        );
        let e = vm.new_err(ErrKind::Eval, msg, pos);
        vm.add_trace(e, pos, ctx);
        return Err(e);
    }
    Ok(canon_path(&s))
}

fn prim_to_path(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let p = coerce_to_path(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to 'builtins.toPath'",
    )?;
    Ok(mk_string(vm, &p))
}

fn prim_base_name_of(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let (s, ctx) = vm.coerce_to_string(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.baseNameOf",
        false,
        false,
        false,
    )?;
    // libutil baseNameOf: strips at most ONE trailing slash (and only if
    // it is not the leading character).
    if s.is_empty() {
        return Ok(mk_string_ctx(vm, b"", &ctx));
    }
    let mut last = s.len() - 1;
    if s[last] == b'/' && last > 0 {
        last -= 1;
    }
    let base = match s[..=last].iter().rposition(|&b| b == b'/') {
        Some(i) => &s[i + 1..=last],
        None => &s[..=last],
    };
    Ok(mk_string_ctx(vm, base, &ctx))
}

fn prim_dir_of(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force(args[0], pos)?;
    let v = val(args[0]);
    if v.tag() == Tag::Path {
        let p = path_bytes(&v);
        let dir = dir_of(p);
        return Ok(vm.new_path_value(&dir));
    }
    let (s, ctx) = vm.coerce_to_string(
        args[0],
        pos,
        "while evaluating the first argument passed to 'builtins.dirOf'",
        false,
        false,
        false,
    )?;
    let dir = dir_of(&s);
    Ok(mk_string_ctx(vm, &dir, &ctx))
}

fn dir_of(s: &[u8]) -> Vec<u8> {
    match s.iter().rposition(|&b| b == b'/') {
        Some(0) => b"/".to_vec(),
        Some(i) => s[..i].to_vec(),
        None => b".".to_vec(),
    }
}

fn prim_get_env(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let name = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.getEnv",
    )?;
    let value = if vm.pure_eval {
        Vec::new()
    } else {
        std::env::var_os(std::ffi::OsStr::new(
            std::str::from_utf8(&name).unwrap_or(""),
        ))
        .map(|v| v.to_string_lossy().into_owned().into_bytes())
        .unwrap_or_default()
    };
    Ok(mk_string(vm, &value))
}

fn prim_path_exists(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force(args[0], pos)?;
    let v = val(args[0]);
    // Determine the raw path text: trailing "/" or "/." force the target to
    // be a directory (the OS would fail ENOTDIR on files, like C++).
    let (raw, _) = if v.tag() == Tag::Path {
        (path_bytes(&v).to_vec(), ())
    } else {
        let (s, _) = vm.coerce_to_string(
            args[0],
            pos,
            "while realising the context of a path",
            false,
            false,
            false,
        )?;
        (s, ())
    };
    if raw.is_empty() || raw[0] != b'/' {
        let msg = format!(
            "string '{}' doesn't represent an absolute path",
            String::from_utf8_lossy(&raw)
        );
        let e = vm.new_err(ErrKind::Eval, msg, pos);
        vm.add_trace(e, pos, "while realising the context of a path");
        return Err(e);
    }
    let dir_required = matches!(raw.split(|&b| b == b'/').next_back(), Some(b"") | Some(b"."));
    let canon = canon_path(&raw);
    let path = PathBuf::from(String::from_utf8_lossy(&canon).into_owned());
    let exists = path.symlink_metadata().is_ok();
    let ok = if dir_required {
        path.metadata().map(|m| m.is_dir()).unwrap_or(false)
    } else {
        exists
    };
    Ok(Value::bool(ok))
}

fn prim_read_file(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let p = coerce_to_path(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.readFile",
    )?;
    let path = String::from_utf8_lossy(&p).into_owned();
    match std::fs::read(&path) {
        Ok(bytes) => Ok(mk_string(vm, &bytes)),
        Err(err) => {
            let msg = format!("reading file '{}': {}", path, io_msg(&err));
            Err(vm.new_err(ErrKind::Eval, msg, pos))
        }
    }
}

fn io_msg(err: &std::io::Error) -> String {
    err.to_string()
}

fn file_type_str(m: &std::fs::Metadata) -> &'static str {
    if m.file_type().is_symlink() {
        "symlink"
    } else if m.is_dir() {
        "directory"
    } else if m.is_file() {
        "regular"
    } else {
        "unknown"
    }
}

fn prim_read_dir(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let p = coerce_to_path(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.readDir",
    )?;
    let path = String::from_utf8_lossy(&p).into_owned();
    let rd = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(err) => {
            let msg = format!("reading directory '{}': {}", path, io_msg(&err));
            return Err(vm.new_err(ErrKind::Eval, msg, pos));
        }
    };
    let mut items: Vec<(Vec<u8>, &'static str)> = Vec::new();
    for ent in rd {
        let Ok(ent) = ent else { continue };
        let name = ent.file_name().to_string_lossy().into_owned().into_bytes();
        let t = ent
            .path()
            .symlink_metadata()
            .map(|m| file_type_str(&m))
            .unwrap_or("unknown");
        items.push((name, t));
    }
    let scope = vm.temp_scope();
    let mut entries: Vec<Attr> = Vec::with_capacity(items.len());
    for (name, t) in &items {
        let sym = vm.symbols.create(name);
        let tv = mk_string(vm, t.as_bytes());
        let tc = temp_cell(vm, tv);
        entries.push(Attr {
            sym: sym.0,
            pos: 0,
            val: tc,
        });
    }
    entries.sort_by_key(|a| a.sym);
    let v = vm.new_bindings_value(&entries);
    vm.temp_end(scope);
    Ok(v)
}

fn prim_read_file_type(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let p = coerce_to_path(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.readFileType",
    )?;
    let path = PathBuf::from(String::from_utf8_lossy(&p).into_owned());
    match path.symlink_metadata() {
        Ok(m) => Ok(mk_string(vm, file_type_str(&m).as_bytes())),
        Err(err) => {
            let msg = format!(
                "getting status of '{}': {}",
                path.display(),
                io_msg(&err)
            );
            Err(vm.new_err(ErrKind::Eval, msg, pos))
        }
    }
}

// ---------------------------------------------------------------------
// hashes
// ---------------------------------------------------------------------

fn parse_hash_algo(vm: &mut VM, s: &[u8], pos: PosIdx) -> Result<HashAlgorithm, ErrId> {
    let name = String::from_utf8_lossy(s).into_owned();
    HashAlgorithm::parse_opt(&name).ok_or_else(|| {
        vm.new_err(
            ErrKind::Eval,
            format!("unknown hash algorithm '{name}', expect 'md5', 'sha1', 'sha256', or 'sha512'"),
            pos,
        )
    })
}

fn prim_hash_string(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let algo_s = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hashString",
    )?;
    let algo = parse_hash_algo(vm, &algo_s, pos)?;
    let s = vm.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.hashString",
    )?;
    let h = hash_string(algo, &s);
    Ok(mk_string(vm, h.to_string(HashFormat::Base16, false).as_bytes()))
}

fn prim_hash_file(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let algo_s = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hashFile",
    )?;
    let algo = parse_hash_algo(vm, &algo_s, pos)?;
    let p = coerce_to_path(
        vm,
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.hashFile",
    )?;
    let path = String::from_utf8_lossy(&p).into_owned();
    let bytes = std::fs::read(&path).map_err(|err| {
        vm.new_err(
            ErrKind::Eval,
            format!("opening file '{}': {}", path, io_msg(&err)),
            pos,
        )
    })?;
    let h = hash_string(algo, &bytes);
    Ok(mk_string(vm, h.to_string(HashFormat::Base16, false).as_bytes()))
}

fn prim_convert_hash(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.convertHash",
    )?;
    let attrs = val(args[0]);
    let hash_sym = vm.symbols.create(b"hash");
    let algo_sym = vm.symbols.create(b"hashAlgo");
    let to_sym = vm.symbols.create(b"toHashFormat");
    let Some(h) = attrs_get(&attrs, hash_sym) else {
        let e = vm.new_err(ErrKind::Type, "attribute 'hash' missing", pos);
        vm.add_trace(
            e,
            NO_POS,
            "while locating the attribute 'hash' passed to builtins.convertHash",
        );
        return Err(e);
    };
    let hash_s = vm.force_string_no_ctx(
        h.val,
        pos,
        "while evaluating the attribute 'hash' passed to builtins.convertHash",
    )?;
    let algo = match attrs_get(&attrs, algo_sym) {
        Some(a) => {
            let s = vm.force_string_no_ctx(
                a.val,
                pos,
                "while evaluating the attribute 'hashAlgo' passed to builtins.convertHash",
            )?;
            Some(parse_hash_algo(vm, &s, pos)?)
        }
        None => None,
    };
    let to_format = match attrs_get(&attrs, to_sym) {
        Some(a) => {
            let s = vm.force_string_no_ctx(
                a.val,
                pos,
                "while evaluating the attribute 'toHashFormat' passed to builtins.convertHash",
            )?;
            match s.as_slice() {
                b"base16" => HashFormat::Base16,
                b"base32" => {
                    use std::io::Write;
                    let _ = std::io::stderr().write_all(
                        b"warning: \"base32\" is a deprecated alias for hash format \"nix32\".\n",
                    );
                    HashFormat::Nix32
                }
                b"nix32" => HashFormat::Nix32,
                b"base64" => HashFormat::Base64,
                b"sri" => HashFormat::Sri,
                _ => {
                    let msg = format!(
                        "hash format '{}' is unknown",
                        String::from_utf8_lossy(&s)
                    );
                    return Err(vm.new_err(ErrKind::Eval, msg, pos));
                }
            }
        }
        None => HashFormat::Sri,
    };
    let hs = String::from_utf8_lossy(&hash_s).into_owned();
    let hash = Hash::parse_any(&hs, algo).map_err(|e| {
        vm.new_err(ErrKind::Eval, e.0, pos)
    })?;
    let out = hash.to_string(to_format, to_format == HashFormat::Sri);
    Ok(mk_string(vm, out.as_bytes()))
}

// ---------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------

fn prim_to_json(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let mut out = Vec::new();
    let mut ctx = Vec::new();
    crate::json::to_json_ctx(vm, args[0], pos, &mut out, &mut ctx)?;
    Ok(mk_string_ctx(vm, &out, &ctx))
}

fn prim_from_json(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let s = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.fromJSON",
    )?;
    crate::json::from_json(vm, &s, pos)
}

// ---------------------------------------------------------------------
// import / findFile
// ---------------------------------------------------------------------

pub fn eval_file(vm: &mut VM, path: &Path, pos: PosIdx) -> Result<VRef, ErrId> {
    let resolved = resolve_expr_path(path);
    if let Some((_, cell)) = vm
        .file_cache
        .iter()
        .find(|(p, _)| p == &resolved)
    {
        let cell = *cell;
        vm.force(cell, pos)?;
        return Ok(cell);
    }
    let source = read_source(vm, &resolved, pos)?;
    let base_path = resolved
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".into());
    let origin = Origin::Path {
        path: resolved.to_string_lossy().into_owned(),
        source: source.clone(),
    };
    let home = std::env::var("HOME").ok();
    let mut warnings: Vec<Vec<u8>> = Vec::new();
    let parsed = jinx_syntax::parse_and_bind_with(
        &source,
        origin,
        &base_path,
        home.as_deref(),
        &mut vm.positions,
        &mut vm.symbols,
        &mut warnings,
    );
    for w in &warnings {
        use std::io::Write;
        let mut out = jinx_syntax::error::filter_ansi_escapes(w);
        out.push(b'\n');
        let _ = std::io::stderr().write_all(&out);
    }
    let (exprs, root) = match parsed {
        Ok(r) => r,
        Err(pe) => {
            let e = vm.new_err(ErrKind::Eval, pe.msg.clone(), pe.pos);
            return Err(e);
        }
    };
    let prog = crate::compile::compile_program(
        &exprs,
        root,
        &vm.symbols,
        &vm.globals,
        vm.empty_list_cell,
    );
    let cell = vm.run_program(prog).map_err(|e| {
        vm.add_trace(
            e,
            pos,
            format!("while evaluating the file '{}':", resolved.display()),
        );
        e
    })?;
    vm.file_cache.push((resolved, cell));
    Ok(cell)
}

fn read_source(vm: &mut VM, resolved: &Path, pos: PosIdx) -> Result<Vec<u8>, ErrId> {
    if let Some(src) = corepkgs_source(resolved) {
        return Ok(src.to_vec());
    }
    std::fs::read(resolved).map_err(|err| {
        vm.new_err(
            ErrKind::Eval,
            format!("opening file '{}': {}", resolved.display(), io_msg(&err)),
            pos,
        )
    })
}

/// Port of C++ resolveExprPath: follow the *final* component while it is a
/// symlink (rebasing relative targets against the LOGICAL parent, so
/// relative imports resolve through symlinks the way C++ does), then append
/// default.nix for directories. Ancestor symlinks are left in the path.
fn resolve_expr_path(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    if p.to_str() == Some("/__corepkgs__/fetchurl.nix") {
        return p;
    }
    for _ in 0..1024 {
        let Ok(md) = p.symlink_metadata() else { break };
        if !md.file_type().is_symlink() {
            break;
        }
        let Ok(target) = std::fs::read_link(&p) else { break };
        p = if target.is_absolute() {
            target
        } else {
            let parent = p.parent().unwrap_or(Path::new("/"));
            PathBuf::from(String::from_utf8_lossy(&crate::vm::canon_path(
                parent.join(target).to_string_lossy().as_bytes(),
            )).into_owned())
        };
    }
    if p.metadata().map(|m| m.is_dir()).unwrap_or(false) {
        p.join("default.nix")
    } else {
        p
    }
}

fn prim_import(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let p = coerce_to_path(
        vm,
        args[0],
        pos,
        "while evaluating the argument passed to builtins.import",
    )?;
    let path = PathBuf::from(String::from_utf8_lossy(&p).into_owned());
    let cell = eval_file(vm, &path, pos)?;
    Ok(val(cell))
}

fn prim_find_file(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.findFile",
    )?;
    let name = vm.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.findFile",
    )?;
    let entries = list_elems(&val(args[0])).to_vec();
    let prefix_sym = vm.symbols.create(b"prefix");
    let path_sym = vm.symbols.create(b"path");
    for el in entries {
        vm.force_attrs(
            el,
            pos,
            "while evaluating an element of the list passed to builtins.findFile",
        )?;
        let ev = val(el);
        let prefix = match attrs_get(&ev, prefix_sym) {
            Some(a) => vm.force_string_no_ctx(
                a.val,
                pos,
                "while evaluating the `prefix` attribute of an element of the list passed to builtins.findFile",
            )?,
            None => Vec::new(),
        };
        let Some(pa) = attrs_get(&ev, path_sym) else {
            continue;
        };
        let (path, _) = vm.coerce_to_string(
            pa.val,
            pos,
            "while evaluating the `path` attribute of an element of the list passed to builtins.findFile",
            false,
            false,
            true,
        )?;
        let suffix: Option<Vec<u8>> = if prefix.is_empty() {
            let mut s = b"/".to_vec();
            s.extend_from_slice(&name);
            Some(s)
        } else if name == prefix {
            Some(Vec::new())
        } else if name.starts_with(&prefix)
            && name.len() > prefix.len()
            && name[prefix.len()] == b'/'
        {
            Some(name[prefix.len()..].to_vec())
        } else {
            None
        };
        let Some(suffix) = suffix else { continue };
        let mut full = path.clone();
        full.extend_from_slice(&suffix);
        let full_str = String::from_utf8_lossy(&full).into_owned();
        let abs = if full_str.starts_with('/') {
            PathBuf::from(full_str)
        } else {
            std::env::current_dir().unwrap_or_default().join(full_str)
        };
        if abs.exists() {
            let canon = canon_path(abs.to_string_lossy().as_bytes());
            return Ok(vm.new_path_value(&canon));
        }
    }
    if name.starts_with(b"nix/") {
        let mut p = b"/__corepkgs__".to_vec();
        p.extend_from_slice(&name[3..]);
        return Ok(vm.new_path_value(&p));
    }
    let msg = format!(
        "file '{}' was not found in the Nix search path (add it using $NIX_PATH or -I)",
        String::from_utf8_lossy(&name)
    );
    Err(vm.new_err(ErrKind::Thrown, msg, pos))
}

/// Embedded corepkgs (fetchurl.nix), served for `<nix/...>` lookups.
fn corepkgs_source(path: &Path) -> Option<&'static [u8]> {
    match path.to_str()? {
        "/__corepkgs__/fetchurl.nix" => Some(FETCHURL_NIX),
        _ => None,
    }
}

const FETCHURL_NIX: &[u8] = include_bytes!("fetchurl.nix");

fn prim_scoped_import(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    vm.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.scopedImport",
    )?;
    let p = coerce_to_path(
        vm,
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.scopedImport",
    )?;
    let path = PathBuf::from(String::from_utf8_lossy(&p).into_owned());
    let resolved = resolve_expr_path(&path);
    let source = read_source(vm, &resolved, pos)?;
    let scope = attrs_entries(&val(args[0])).to_vec();
    // Scope cells become compile-time constants of a leaked program; they
    // must stay alive for the process lifetime.
    let extra: Vec<Symbol> = scope.iter().map(|a| Symbol(a.sym)).collect();
    for a in &scope {
        vm.perm_roots.push(a.val);
    }
    let base_path = resolved
        .parent()
        .map(|q| q.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".into());
    let origin = Origin::Path {
        path: resolved.to_string_lossy().into_owned(),
        source: source.clone(),
    };
    let home = std::env::var("HOME").ok();
    let mut warnings: Vec<Vec<u8>> = Vec::new();
    let parsed = jinx_syntax::parse_and_bind_scoped(
        &source,
        origin,
        &base_path,
        home.as_deref(),
        &mut vm.positions,
        &mut vm.symbols,
        &mut warnings,
        &extra,
    );
    let (exprs, root) = match parsed {
        Ok(r) => r,
        Err(pe) => {
            let e = vm.new_err(ErrKind::Eval, pe.msg.clone(), pe.pos);
            return Err(e);
        }
    };
    let mut globals2 = vm.globals.clone();
    for a in &scope {
        globals2.insert(Symbol(a.sym), a.val);
    }
    let prog = crate::compile::compile_program(
        &exprs,
        root,
        &vm.symbols,
        &globals2,
        vm.empty_list_cell,
    );
    let cell = vm.run_program(prog)?;
    Ok(val(cell))
}
