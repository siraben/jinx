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
        select_caches: Vec::new(),
    };
    for n in 1..=2u32 {
        let mut ops = vec![Op::GetUpval(0)];
        for k in 0..n {
            ops.push(Op::GetUpval(k + 1));
        }
        ops.push(Op::Call(n));
        ops.push(Op::Force);
        ops.push(Op::Ret);
        let mut chunk = Chunk {
            ops,
            max_height: n + 1,
            ..Default::default()
        };
        chunk.kind = crate::compile::classify_chunk(&chunk, &prog.chunks);
        prog.chunks.push(chunk);
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

// Builtins jinx recognizes but does not yet execute. Only names that are in the
// C++ `builtins` set *by default* belong here — i.e. not gated behind an
// experimental feature or the unsafe-native setting. `exec`/`importNative`
// (unsafe-native), `fetchClosure` (fetch-closure), and `outputOf`
// (dynamic-derivations) are absent from the default set, and
// `fetchFinalTree`/`forceLazyFetcherAttr` are internal fetcher helpers Nix
// never exposes — so none of them are registered, matching `builtins`
// attrname parity with the oracle (adversarial-review finding: phantom
// builtins broke `builtins ? exec` feature detection).
const UNIMPLEMENTED: &[(&str, u8)] = &[
    ("fetchGit", 1),
    ("fetchMercurial", 1),
    ("fetchTarball", 1),
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
        Reg { name: "__storePath", arity: 1, func: prim_store_path },
        Reg { name: "__filterSource", arity: 2, func: prim_filter_source },
        Reg { name: "__fetchurl", arity: 1, func: prim_fetchurl },
        Reg { name: "fetchTree", arity: 1, func: prim_fetch_tree },
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
        Reg { name: "__traceVerbose", arity: 2, func: prim_trace_verbose },
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
        Reg { name: "__toXML", arity: 1, func: prim_to_xml },
        Reg { name: "__path", arity: 1, func: prim_path },
        Reg { name: "__toFile", arity: 2, func: prim_to_file },
        Reg { name: "getFlake", arity: 1, func: prim_get_flake },
        Reg { name: "parseFlakeRef", arity: 1, func: prim_parse_flake_ref },
        Reg { name: "flakeRefToString", arity: 1, func: prim_flake_ref_to_string },
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
    // Match the oracle's version string. Must not compareVersions-newer than
    // real nix (nixpkgs branches on builtins.nixVersion); a bare "2.36.0" would
    // sort *after* the "…pre…" release the oracle reports.
    let nv = immortal::cell(immortal::string(b"2.36.0pre20260706_cff1f11"));
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
    vm.set_b(builtins_cell, bv);
    let bsym = vm.symbols.create(b"builtins");
    vm.globals.insert(bsym, builtins_cell);

    // Compile the derivation wrapper now that globals are in place. The source
    // is the verbatim upstream `primops/derivation.nix` (see
    // DERIVATION_NIX_FILE) with a leading newline prepended, exactly
    // reproducing how C++ embeds it via `R"__NIX_STR( ... )__NIX_STR"` (the
    // open-paren is followed by a newline). This keeps line numbers and source
    // excerpts in error traces byte-identical to Nix, which does not normalize
    // them (only eval-fail-derivation-name.postprocess masks the digits).
    let src = derivation_nix_source();
    let mut warnings = Vec::new();
    let parsed = jinx_syntax::parse_and_bind_with(
        &src,
        Origin::Path {
            path: DERIVATION_INTERNAL_ORIGIN.to_string(),
            source: src.clone(),
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
    vm.set_b(derivation_cell, val(cell));
}

/// The `derivation` builtin's Nix implementation, vendored verbatim from
/// upstream Nix `src/libexpr/primops/derivation.nix` (comments and doc-comment
/// included) so that error-trace positions and source excerpts match Nix
/// exactly. Provenance: copied from /path/to/nix/src/libexpr/primops/derivation.nix.
pub(crate) const DERIVATION_NIX_FILE: &[u8] = include_bytes!("derivation-internal.nix");

/// Origin name Nix uses for the embedded derivation wrapper.
pub(crate) const DERIVATION_INTERNAL_ORIGIN: &str = "«nix-internal»/derivation-internal.nix";

/// The wrapper source as Nix sees it: the verbatim file with a single leading
/// newline, reproducing the `R"__NIX_STR(\n...` raw-string embedding so that
/// all positions are shifted down by one line to match the compiled C++ binary.
pub(crate) fn derivation_nix_source() -> Vec<u8> {
    let mut v = Vec::with_capacity(DERIVATION_NIX_FILE.len() + 1);
    v.push(b'\n');
    v.extend_from_slice(DERIVATION_NIX_FILE);
    v
}

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
    // C++ compares `v1->type() != v2->type()`; `true` and `false` are both
    // nBool (same type), so they take the "incomparable" branch below, not this
    // one. jinx stores them as distinct tags, so normalize booleans here.
    let norm = |t: Tag| match t {
        Tag::True | Tag::False => Tag::True,
        other => other,
    };
    if norm(va.tag()) != norm(vb.tag()) {
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
                "cannot compare {} with {}; values of that type are incomparable (values are {} and {})",
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
        // C++ `coerceToString` default copyToStore=true: a path argument is
        // copied to the store and the store path appears in the message.
        false,
        true,
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
        // C++ `coerceToString` default copyToStore=true: a path argument is
        // copied to the store and the store path appears in the message.
        false,
        true,
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

fn prim_trace_verbose(vm: &mut VM, d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    // Only emit the trace when `--trace-verbose` is set (C++ swaps the impl
    // based on the `trace-verbose` setting); otherwise behave like `seq`.
    if vm.trace_verbose {
        prim_trace(vm, d, args, pos)
    } else {
        prim_seq_second(vm, d, args, pos)
    }
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
    if vm.abort_on_warn {
        // C++ throws a plain EvalBaseError (uncached, not tryEval-catchable)
        // after emitting the warning, to reveal the surrounding stack trace.
        return Err(vm.new_err(
            ErrKind::Eval,
            "aborting to reveal stack trace of warning, as abort-on-warn is set",
            pos,
        ));
    }
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
            vm.add_trace_always(e, NO_POS, String::from_utf8_lossy(&s).into_owned());
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
        // C++ `coerceToString` default copyToStore=true: a path is copied to
        // the store, so the length counts the /nix/store/... path.
        false,
        true,
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
        // C++ `coerceToString` default copyToStore=true: a path is copied to
        // the store and substring operates on the /nix/store/... path.
        false,
        true,
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
            // C++ `coerceToString` defaults: coerceMore=false, copyToStore=true,
            // canonicalizePath=true. A path element (e.g. `./ldexpl.c`) is copied
            // to the store and contributes string context, not stringified raw.
            false,
            true,
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
    // C++ threads the *original* string's full context through `context` first
    // (forceString(args[2], context, …)), then merges each used replacement's
    // context. The whole original context is retained regardless of which parts
    // are replaced.
    let mut ctx: Vec<u32> = vm.read_str_ctx(&val(args[2]));
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
        // C++ `coerceToString` default copyToStore=true: a path is copied to
        // the store before its context is discarded.
        false,
        true,
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
        // C++ `coerceToString` default copyToStore=true.
        false,
        true,
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
        // C++ `coerceToString` default copyToStore=true.
        false,
        true,
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
    let drv_name = vm
        .force_string_no_ctx(
            name_attr.val,
            pos,
            "while evaluating the `name` attribute passed to builtins.derivationStrict",
        )
        .map_err(|e| {
            vm.add_trace(e, name_pos, "while evaluating the derivation attribute 'name'");
            e
        })?;
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

/// Mutable derivation-construction state threaded through [`process_drv_attr`].
struct DrvAttrState<'a> {
    drv: &'a mut jinx_store::derivation::Derivation,
    context: &'a mut Vec<u32>,
    content_addressed: &'a mut bool,
    is_impure: &'a mut bool,
    output_hash: &'a mut Option<Vec<u8>>,
    output_hash_algo: &'a mut Option<jinx_store::hash::HashAlgorithm>,
    ingestion_method: &'a mut Option<jinx_store::store_path::ContentAddressMethod>,
    outputs: &'a mut std::collections::BTreeSet<String>,
    json_members: &'a mut Vec<(Vec<u8>, Vec<u8>)>,
}

/// Process a single derivation attribute. Ported from the body of the
/// per-attribute loop in C++ `derivationStrictInternal`; the caller attaches
/// the "while evaluating attribute '…' of derivation '…'" frame on error.
/// Inner forces use `pos` (the primop's `noPos`) and empty contexts.
fn process_drv_attr(
    vm: &mut VM,
    key: &[u8],
    a: &Attr,
    structured: bool,
    ignore_nulls: bool,
    pos: PosIdx,
    drv_name_s: &str,
    st: &mut DrvAttrState,
) -> Result<(), ErrId> {
    // __ignoreNulls: drop null-valued attrs entirely.
    if ignore_nulls {
        vm.force(a.val, pos)?;
        if val(a.val).tag() == Tag::Null {
            return Ok(());
        }
    }
    match key {
        // Only elided from the env when structured-attrs mode is *on* (C++
        // skips it solely inside the `if (jsonObject)` branch). With
        // `__structuredAttrs = false` it is a normal boolean attribute and is
        // passed to the builder as the env var `__structuredAttrs=""`.
        b"__structuredAttrs" if structured => return Ok(()),
        b"__contentAddressed" => {
            *st.content_addressed = vm.force_bool(a.val, pos, "")?;
            if *st.content_addressed && !vm.experimental.ca_derivations {
                return Err(vm.new_err(
                    ErrKind::Eval,
                    "experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it",
                    pos,
                ));
            }
            return Ok(());
        }
        b"__impure" => {
            *st.is_impure = vm.force_bool(a.val, pos, "")?;
            if *st.is_impure && !vm.experimental.impure_derivations {
                return Err(vm.new_err(
                    ErrKind::Eval,
                    "experimental Nix feature 'impure-derivations' is disabled; add '--extra-experimental-features impure-derivations' to enable it",
                    pos,
                ));
            }
            return Ok(());
        }
        b"args" => {
            vm.force_list(a.val, pos, "")?;
            let elems = list_elems(&val(a.val)).to_vec();
            for el in &elems {
                let (s, cids) = vm.coerce_to_string(
                    *el,
                    pos,
                    "while evaluating an element of the argument list",
                    true,
                    true,
                    true,
                )?;
                merge_ctx(st.context, &cids);
                st.drv.args.push(s.into());
            }
            return Ok(());
        }
        _ => {}
    }

    // A regular attribute.
    if structured {
        let mut jbuf = Vec::new();
        crate::json::to_json_ctx(vm, a.val, pos, &mut jbuf, st.context)?;
        st.json_members.push((key.to_vec(), jbuf));
        if let b"allowedReferences" | b"allowedRequisites" | b"disallowedReferences"
        | b"disallowedRequisites" | b"maxSize" | b"maxClosureSize" = key
        {
            let ks = String::from_utf8_lossy(key);
            emit_warning(&format!(
                "In a derivation named '{drv_name_s}', 'structuredAttrs' disables the effect of the derivation attribute '{ks}'; use 'outputChecks.<output>.{ks}' instead"
            ));
        }
        match key {
            b"builder" => {
                let (s, cids) = coerce_str_ctx(vm, a.val, pos)?;
                merge_ctx(st.context, &cids);
                st.drv.builder = s.into();
            }
            b"system" => {
                st.drv.platform = vm.force_string_no_ctx(a.val, pos, "")?.into();
            }
            b"outputHash" => {
                *st.output_hash = Some(vm.force_string_no_ctx(a.val, pos, "")?);
            }
            b"outputHashAlgo" => {
                let s = vm.force_string_no_ctx(a.val, pos, "")?;
                *st.output_hash_algo =
                    jinx_store::hash::HashAlgorithm::parse_opt(&String::from_utf8_lossy(&s));
            }
            b"outputHashMode" => {
                let s = vm.force_string_no_ctx(a.val, pos, "")?;
                *st.ingestion_method = Some(handle_hash_mode(vm, &s, pos)?);
            }
            b"outputs" => {
                vm.force_list(a.val, pos, "")?;
                let elems = list_elems(&val(a.val)).to_vec();
                let mut ss: Vec<Vec<u8>> = Vec::new();
                for el in &elems {
                    ss.push(vm.force_string_no_ctx(*el, pos, "")?);
                }
                handle_outputs(vm, &ss, st.outputs, pos)?;
            }
            _ => {}
        }
    } else {
        let (s, cids) = vm.coerce_to_string(a.val, pos, "", true, true, true)?;
        merge_ctx(st.context, &cids);
        if key == b"__json" {
            st.drv.env.insert(b"__json".as_slice().into(), s.clone().into());
        } else {
            st.drv.env.insert(key.into(), s.clone().into());
            match key {
                b"builder" => st.drv.builder = s.into(),
                b"system" => st.drv.platform = s.into(),
                b"outputHash" => *st.output_hash = Some(s),
                b"outputHashAlgo" => {
                    *st.output_hash_algo =
                        jinx_store::hash::HashAlgorithm::parse_opt(&String::from_utf8_lossy(&s))
                }
                b"outputHashMode" => *st.ingestion_method = Some(handle_hash_mode(vm, &s, pos)?),
                b"outputs" => {
                    let ss: Vec<Vec<u8>> = s
                        .split(|c| c.is_ascii_whitespace())
                        .filter(|x| !x.is_empty())
                        .map(|x| x.to_vec())
                        .collect();
                    handle_outputs(vm, &ss, st.outputs, pos)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
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
        let mut st = DrvAttrState {
            drv: &mut drv,
            context: &mut context,
            content_addressed: &mut content_addressed,
            is_impure: &mut is_impure,
            output_hash: &mut output_hash,
            output_hash_algo: &mut output_hash_algo,
            ingestion_method: &mut ingestion_method,
            outputs: &mut outputs,
            json_members: &mut json_members,
        };
        // C++ wraps the whole per-attribute body in a single
        // "while evaluating attribute '<key>' of derivation '<name>'" frame
        // (at the attribute's position); inner forces carry no position.
        process_drv_attr(vm, key, a, structured, ignore_nulls, pos, &drv_name_s, &mut st)
            .map_err(|e| {
                vm.add_trace(
                    e,
                    apos,
                    format!(
                        "while evaluating attribute '{}' of derivation '{}'",
                        String::from_utf8_lossy(key),
                        drv_name_s
                    ),
                );
                e
            })?;
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

    // Materialize the `.drv` in the store when a writable daemon backend is
    // selected (non-readonly). Port of `writeDerivation` in `derivationStrict`.
    // Under the dummy store this is a no-op (read-only path computation).
    if let Err(msg) = write_derivation_to_store(vm, &drv, &drv_path) {
        return Err(vm.new_err(ErrKind::Eval, msg, pos));
    }

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

fn prim_path(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    use jinx_store::hash::{Hash, HashAlgorithm};
    use jinx_store::store_path::{
        ContentAddressMethod, ContentAddressWithReferences, FileIngestionMethod, FixedOutputInfo,
        StoreReferences,
    };
    vm.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to 'builtins.path'",
    )?;
    let entries = attrs_entries(&val(args[0])).to_vec();
    let mut path_bytes_opt: Option<Vec<u8>> = None;
    let mut name: Option<Vec<u8>> = None;
    let mut method = FileIngestionMethod::NixArchive;
    let mut expected_hash: Option<Hash> = None;
    let mut context: Vec<u32> = Vec::new();
    let mut filter: Option<VRef> = None;
    for a in &entries {
        let key = vm.symbols.resolve(Symbol(a.sym)).to_vec();
        match key.as_slice() {
            b"path" => {
                let (s, cids) = vm.coerce_to_string(
                    a.val,
                    PosIdx(a.pos),
                    "while evaluating the 'path' attribute passed to 'builtins.path'",
                    false,
                    false,
                    true,
                )?;
                merge_ctx(&mut context, &cids);
                path_bytes_opt = Some(s);
            }
            b"name" => {
                name = Some(vm.force_string_no_ctx(
                    a.val,
                    PosIdx(a.pos),
                    "while evaluating the `name` attribute passed to builtins.path",
                )?);
            }
            b"filter" => {
                force_fun(
                    vm,
                    a.val,
                    PosIdx(a.pos),
                    "while evaluating the `filter` parameter passed to builtins.path",
                )?;
                filter = Some(a.val);
            }
            b"recursive" => {
                method = if vm.force_bool(
                    a.val,
                    PosIdx(a.pos),
                    "while evaluating the `recursive` attribute passed to builtins.path",
                )? {
                    FileIngestionMethod::NixArchive
                } else {
                    FileIngestionMethod::Flat
                };
            }
            b"sha256" => {
                let s = vm.force_string_no_ctx(
                    a.val,
                    PosIdx(a.pos),
                    "while evaluating the `sha256` attribute passed to builtins.path",
                )?;
                expected_hash = Some(
                    Hash::parse_any(&String::from_utf8_lossy(&s), Some(HashAlgorithm::Sha256))
                        .map_err(|e| vm.new_err(ErrKind::Eval, e.0, pos))?,
                );
            }
            _ => {
                return Err(vm.new_err(
                    ErrKind::Eval,
                    format!(
                        "unsupported argument '{}' to 'builtins.path'",
                        String::from_utf8_lossy(&key)
                    ),
                    PosIdx(a.pos),
                ));
            }
        }
    }
    let path = match path_bytes_opt {
        Some(p) => p,
        None => {
            return Err(vm.new_err(
                ErrKind::Eval,
                "missing required 'path' attribute in the first argument to 'builtins.path'",
                pos,
            ))
        }
    };
    let name = name.unwrap_or_else(|| {
        match path.iter().rposition(|&c| c == b'/') {
            Some(i) => path[i + 1..].to_vec(),
            None => path.clone(),
        }
    });
    let name_s = String::from_utf8_lossy(&name).into_owned();
    let store = vm.store();
    let sp = if let Some(h) = expected_hash {
        let ca = ContentAddressWithReferences::Fixed(FixedOutputInfo {
            method,
            hash: h,
            references: StoreReferences::default(),
        });
        let expected_sp = store
            .make_fixed_output_path_from_ca(&name_s, &ca)
            .map_err(|e| vm.new_err(ErrKind::Eval, e.0, pos))?;
        // Port of prim_path: hash the actual (possibly filtered) path and
        // require the resulting store path to match the one derived from the
        // declared hash. C++ skips this when the expected path is already valid
        // in the store; under jinx's readonly/dummy store it never is, so we
        // always verify. A mismatch is `store path mismatch ...`.
        use std::os::unix::ffi::OsStrExt;
        let logical = std::path::Path::new(std::ffi::OsStr::from_bytes(&path));
        let real = vm.redirect_fs(logical);
        let refset = jinx_store::store_path::StorePathSet::new();
        let dst = add_filtered_path(vm, &name_s, real.as_path(), filter, method, &refset)
            .map_err(|e| {
                vm.add_trace(
                    e,
                    pos,
                    format!("while adding path '{}'", String::from_utf8_lossy(&path)),
                );
                e
            })?;
        if dst != expected_sp {
            let e = vm.new_err(
                ErrKind::Eval,
                format!(
                    "store path mismatch in (possibly filtered) path added from '{}'",
                    String::from_utf8_lossy(&path)
                ),
                pos,
            );
            vm.add_trace(
                e,
                pos,
                format!("while adding path '{}'", String::from_utf8_lossy(&path)),
            );
            return Err(e);
        }
        expected_sp
    } else {
        // No expected hash: hash the (filtered) path content, adding it to the
        // store when daemon-backed. Port of `addPath` without `expectedHash`.
        let _ = ContentAddressMethod::Flat;
        use std::os::unix::ffi::OsStrExt;
        let logical = std::path::Path::new(std::ffi::OsStr::from_bytes(&path));
        let real = vm.redirect_fs(logical);
        let refs = jinx_store::store_path::StorePathSet::new();
        add_filtered_path(vm, &name_s, real.as_path(), filter, method, &refs)
            .map_err(|e| {
                vm.add_trace(e, pos, format!("while adding path '{}'", String::from_utf8_lossy(&path)));
                e
            })?
    };
    let printed = store.print_store_path(&sp);
    let id = vm.intern_elem(&crate::context::ContextElem::Opaque {
        path: sp.to_string().as_bytes().to_vec(),
    });
    Ok(vm.new_string_ctx(printed.as_bytes(), &[id]))
}

fn prim_to_xml(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    let _ = pos;
    let (out, ctx) = crate::xml::prim_to_xml_impl(vm, args[0])?;
    Ok(mk_string_ctx(vm, &out, &ctx))
}

/// Port of `Store::writeDerivation`: add the `.drv` ATerm to the store as a
/// text content-addressed object (only under [`StoreMode::Daemon`]). The store
/// path is already known (`drv_path`); the daemon must agree. Skips the add when
/// the path is already valid (fast path), then registers a temp root so the GC
/// won't remove it mid-run. Returns an error message on daemon failure.
fn write_derivation_to_store(
    vm: &mut VM,
    drv: &jinx_store::derivation::Derivation,
    drv_path: &jinx_store::store_path::StorePath,
) -> Result<(), String> {
    if vm.store_mode != crate::vm::StoreMode::Daemon {
        return Ok(());
    }
    let store = vm.store();
    let suffix = format!("{}.drv", drv.name);
    let contents = drv.unparse(&store, false, None).map_err(|e| e.0)?;
    let refs = drv.drv_references();
    let Some(d) = vm.daemon() else {
        return Ok(());
    };
    let valid = d.is_valid_path(drv_path).map_err(|e| e.to_string())?;
    if !valid {
        d.add_to_store_bytes(&suffix, "text:sha256", &refs, false, &contents)
            .map_err(|e| e.to_string())?;
    }
    d.add_temp_root(drv_path).map_err(|e| e.to_string())?;
    Ok(())
}

fn prim_to_file(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    use crate::context::ContextElem;
    use jinx_store::store_path::{StorePath, StorePathSet};
    let name = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.toFile",
    )?;
    let contents = vm.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.toFile",
    )?;
    let name_s = String::from_utf8_lossy(&name).into_owned();
    // Collect store-path references from the contents' context. Only opaque
    // store paths are allowed; derivations / their outputs are rejected (a
    // `toFile` result is a plain text object, it cannot depend on a build).
    let mut refs = StorePathSet::new();
    for id in vm.read_str_ctx(&val(args[1])) {
        match vm.ctx_elem(id) {
            ContextElem::Opaque { path } => {
                if let Ok(sp) = StorePath::new(&String::from_utf8_lossy(&path)) {
                    refs.insert(sp);
                }
            }
            other => {
                let store = vm.store();
                let printed = {
                    let bytes = other.encode();
                    // `!out!<base>` / `=<base>` -> print the base with the store dir.
                    match &other {
                        ContextElem::Built { drv_path, output } => {
                            let sp = StorePath::new(&String::from_utf8_lossy(drv_path));
                            let dp = sp
                                .map(|p| store.print_store_path(&p))
                                .unwrap_or_else(|_| String::from_utf8_lossy(drv_path).into_owned());
                            format!("!{}!{}", String::from_utf8_lossy(output), dp)
                        }
                        ContextElem::DrvDeep { drv_path } => {
                            let sp = StorePath::new(&String::from_utf8_lossy(drv_path));
                            let dp = sp
                                .map(|p| store.print_store_path(&p))
                                .unwrap_or_else(|_| String::from_utf8_lossy(drv_path).into_owned());
                            format!("={dp}")
                        }
                        ContextElem::Opaque { .. } => String::from_utf8_lossy(&bytes).into_owned(),
                    }
                };
                return Err(vm.new_err(
                    ErrKind::Eval,
                    format!(
                        "files created by 'builtins.toFile' may not reference derivations, but '{name_s}' references '{printed}'"
                    ),
                    pos,
                ));
            }
        }
    }
    let store = vm.store();
    let sp = store
        .make_text_path(&name_s, &contents, &refs)
        .map_err(|e| vm.new_err(ErrKind::Eval, e.0, pos))?;
    let printed = store.print_store_path(&sp);
    // Materialize the text object in the store when daemon-backed (port of
    // `addTextToStore` via `AddToStore` with the text CA method). No-op under
    // the dummy store.
    if let Err(msg) = add_text_to_store(vm, &name_s, &contents, &refs, &sp) {
        return Err(vm.new_err(ErrKind::Eval, msg, pos));
    }
    let id = vm.intern_elem(&ContextElem::Opaque {
        path: sp.to_string().as_bytes().to_vec(),
    });
    Ok(vm.new_string_ctx(printed.as_bytes(), &[id]))
}

/// Port of `Store::addTextToStore`: add `contents` as a `text:sha256`
/// content-addressed object with the given references (only under
/// [`StoreMode::Daemon`]; a no-op otherwise). `sp` is the expected path.
fn add_text_to_store(
    vm: &mut VM,
    name: &str,
    contents: &[u8],
    refs: &jinx_store::store_path::StorePathSet,
    sp: &jinx_store::store_path::StorePath,
) -> Result<(), String> {
    if vm.store_mode != crate::vm::StoreMode::Daemon {
        return Ok(());
    }
    let Some(d) = vm.daemon() else {
        return Ok(());
    };
    let valid = d.is_valid_path(sp).map_err(|e| e.to_string())?;
    if !valid {
        d.add_to_store_bytes(name, "text:sha256", refs, false, contents)
            .map_err(|e| e.to_string())?;
    }
    d.add_temp_root(sp).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------
// flakerefs (experimental: flakes)
// ---------------------------------------------------------------------

fn flakes_disabled(vm: &mut VM, pos: PosIdx) -> ErrId {
    vm.new_err(
        ErrKind::Eval,
        "experimental Nix feature 'flakes' is disabled; add '--extra-experimental-features flakes' to enable it",
        pos,
    )
}

/// 40-char lowercase hex git rev.
fn is_git_rev(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Parse a flakeref string into a sorted list of (key, value) string attrs.
/// Supports the `github:` scheme, `flake:`/indirect ids, and `path:`/absolute
/// paths — enough for `builtins.parseFlakeRef`.
fn parse_flake_ref(s: &str) -> Result<Vec<(String, String)>, String> {
    if let Some(hidx) = s.find('#') {
        return Err(format!(
            "unexpected fragment '{}' in flake reference '{}'",
            &s[hidx + 1..],
            s
        ));
    }
    // Split off the query string.
    let (body, query) = match s.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (s, None),
    };
    let mut qmap: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    if let Some(q) = query {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            if let Some((k, v)) = pair.split_once('=') {
                qmap.insert(k.to_string(), url_decode(v));
            }
        }
    }
    let mut attrs: Vec<(String, String)> = Vec::new();
    let dir = qmap.remove("dir");

    if let Some(rest) = body.strip_prefix("github:").or_else(|| body.strip_prefix("gitlab:").or_else(|| body.strip_prefix("sourcehut:"))) {
        let scheme = &body[..body.find(':').unwrap()];
        let segs: Vec<&str> = rest.splitn(3, '/').collect();
        if segs.len() < 2 {
            return Err(format!("'{s}' is not a valid flake reference"));
        }
        attrs.push(("type".into(), scheme.into()));
        attrs.push(("owner".into(), segs[0].into()));
        attrs.push(("repo".into(), segs[1].into()));
        if let Some(third) = segs.get(2) {
            if is_git_rev(third) {
                attrs.push(("rev".into(), (*third).into()));
            } else {
                attrs.push(("ref".into(), (*third).into()));
            }
        }
        for (k, v) in &qmap {
            attrs.push((k.clone(), v.clone()));
        }
    } else if let Some(rest) = body.strip_prefix("flake:") {
        parse_indirect(rest, &mut attrs)?;
    } else if let Some(rest) = body.strip_prefix("path:") {
        attrs.push(("type".into(), "path".into()));
        attrs.push(("path".into(), url_decode(rest)));
        for (k, v) in &qmap {
            attrs.push((k.clone(), v.clone()));
        }
    } else if body.starts_with('/') {
        attrs.push(("type".into(), "path".into()));
        attrs.push(("path".into(), body.into()));
    } else if !body.contains(':') {
        // Bare flake id: "nixpkgs" or "nixpkgs/ref".
        parse_indirect(body, &mut attrs)?;
    } else {
        return Err(format!("'{s}' is not a valid flake reference"));
    }

    if let Some(d) = dir {
        attrs.push(("dir".into(), d));
    }
    attrs.sort_by(|a, b| a.0.cmp(&b.0));
    attrs.dedup_by(|a, b| a.0 == b.0);
    Ok(attrs)
}

fn parse_indirect(rest: &str, attrs: &mut Vec<(String, String)>) -> Result<(), String> {
    let segs: Vec<&str> = rest.splitn(3, '/').collect();
    attrs.push(("type".into(), "indirect".into()));
    attrs.push(("id".into(), segs[0].into()));
    match segs.get(1) {
        Some(a) if is_git_rev(a) => attrs.push(("rev".into(), (*a).into())),
        Some(a) => {
            attrs.push(("ref".into(), (*a).into()));
            if let Some(rev) = segs.get(2) {
                attrs.push(("rev".into(), (*rev).into()));
            }
        }
        None => {}
    }
    Ok(())
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn prim_get_flake(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    if !vm.experimental.flakes {
        return Err(flakes_disabled(vm, pos));
    }
    let s = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.getFlake",
    )?;
    let flakeref = String::from_utf8_lossy(&s).into_owned();
    let cell = crate::flake::get_flake(vm, &flakeref, pos)?;
    Ok(val(cell))
}

fn prim_parse_flake_ref(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    if !vm.experimental.flakes {
        return Err(flakes_disabled(vm, pos));
    }
    let s = vm.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.parseFlakeRef",
    )?;
    let attrs = parse_flake_ref(&String::from_utf8_lossy(&s))
        .map_err(|m| vm.new_err(ErrKind::Eval, m, pos))?;
    let scope = vm.temp_scope();
    let mut entries: Vec<Attr> = Vec::with_capacity(attrs.len());
    for (k, v) in &attrs {
        let sv = mk_string(vm, v.as_bytes());
        let vc = temp_cell(vm, sv);
        entries.push(Attr {
            sym: vm.symbols.create(k.as_bytes()).0,
            pos: 0,
            val: vc,
        });
    }
    entries.sort_by_key(|a| a.sym);
    entries.dedup_by_key(|a| a.sym);
    let v = vm.new_bindings_value(&entries);
    vm.temp_end(scope);
    Ok(v)
}

/// Render flakeref attrs back to a string.
fn flake_ref_to_string(attrs: &std::collections::BTreeMap<String, FlakeVal>) -> Result<String, String> {
    let get_str = |k: &str| -> Option<&str> {
        match attrs.get(k) {
            Some(FlakeVal::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    };
    let ty = get_str("type").ok_or_else(|| "flake reference has no 'type' attribute".to_string())?;
    // Query params (sorted): everything not consumed by the scheme's path.
    let mut out = String::new();
    let mut query: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    match ty {
        "github" | "gitlab" | "sourcehut" => {
            let owner = get_str("owner").unwrap_or("");
            let repo = get_str("repo").unwrap_or("");
            out.push_str(ty);
            out.push(':');
            out.push_str(owner);
            out.push('/');
            out.push_str(repo);
            if let Some(r) = get_str("ref") {
                out.push('/');
                out.push_str(r);
            } else if let Some(r) = get_str("rev") {
                out.push('/');
                out.push_str(r);
            }
            if let Some(h) = get_str("host") {
                query.insert("host".into(), h.into());
            }
            if let Some(h) = get_str("narHash") {
                query.insert("narHash".into(), h.into());
            }
        }
        "path" => {
            out.push_str("path:");
            out.push_str(get_str("path").unwrap_or(""));
            if let Some(r) = get_str("rev") {
                query.insert("rev".into(), r.into());
            }
            if let Some(r) = get_str("ref") {
                query.insert("ref".into(), r.into());
            }
        }
        "indirect" => {
            out.push_str("flake:");
            out.push_str(get_str("id").unwrap_or(""));
            if let Some(r) = get_str("ref") {
                out.push('/');
                out.push_str(r);
            }
            if let Some(r) = get_str("rev") {
                out.push('/');
                out.push_str(r);
            }
        }
        "git" | "tarball" | "file" => {
            let url = get_str("url").unwrap_or("");
            out.push_str(url);
            if let Some(r) = get_str("ref") {
                query.insert("ref".into(), r.into());
            }
            if let Some(r) = get_str("rev") {
                query.insert("rev".into(), r.into());
            }
        }
        other => return Err(format!("don't know how to serialize flakeref of type '{other}'")),
    }
    if let Some(FlakeVal::Str(d)) = attrs.get("dir") {
        query.insert("dir".into(), d.clone());
    }
    if !query.is_empty() {
        out.push('?');
        let mut first = true;
        for (k, v) in &query {
            if !first {
                out.push('&');
            }
            first = false;
            out.push_str(k);
            out.push('=');
            out.push_str(v);
        }
    }
    Ok(out)
}

#[allow(dead_code)] // Int/Bool round-tripping is validated but unused by fixtures.
enum FlakeVal {
    Str(String),
    Int(i64),
    Bool(bool),
}

fn prim_flake_ref_to_string(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    if !vm.experimental.flakes {
        return Err(flakes_disabled(vm, pos));
    }
    vm.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.flakeRefToString",
    )?;
    let entries = attrs_entries(&val(args[0])).to_vec();
    let mut map: std::collections::BTreeMap<String, FlakeVal> = std::collections::BTreeMap::new();
    for a in &entries {
        let key = String::from_utf8_lossy(vm.symbols.resolve(Symbol(a.sym))).into_owned();
        vm.force(a.val, PosIdx(a.pos))?;
        let v = val(a.val);
        let fv = match v.tag() {
            Tag::Int => {
                let iv = v.as_int();
                if iv < 0 {
                    return Err(vm.new_err(
                        ErrKind::Eval,
                        format!("negative value given for flake ref attr {key}: {iv}"),
                        pos,
                    ));
                }
                FlakeVal::Int(iv)
            }
            Tag::True | Tag::False => FlakeVal::Bool(v.as_bool()),
            Tag::String => FlakeVal::Str(String::from_utf8_lossy(str_bytes(&v)).into_owned()),
            _ => {
                return Err(vm.new_err(
                    ErrKind::Eval,
                    format!(
                        "flake reference attribute sets may only contain integers, Booleans, and strings, but attribute '{key}' is {}",
                        vm.show_type(&v)
                    ),
                    pos,
                ))
            }
        };
        map.insert(key, fv);
    }
    let s = flake_ref_to_string(&map).map_err(|m| vm.new_err(ErrKind::Eval, m, pos))?;
    Ok(mk_string(vm, s.as_bytes()))
}

// ---------------------------------------------------------------------
// fromTOML
// ---------------------------------------------------------------------

/// Reproduce toml11's `parse_dec_integer` out-of-range diagnostic (as embedded
/// by C++ `builtins.fromTOML` under the pseudo-filename `fromTOML`). The `toml`
/// crate only reports "number too large/small to fit in target type", so we
/// re-scan the source for the first decimal integer literal that overflows a
/// signed 64-bit range and rebuild toml11's multi-line, caret-annotated
/// message. Returns the full `while parsing TOML: ...` text.
fn toml_integer_overflow_message(src: &str) -> Option<String> {
    for (li, line) in src.split('\n').enumerate() {
        let bytes = line.as_bytes();
        let n = bytes.len();
        let mut i = 0;
        while i < n {
            let prev_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'.');
            let starts_num = bytes[i].is_ascii_digit()
                || ((bytes[i] == b'+' || bytes[i] == b'-')
                    && i + 1 < n
                    && bytes[i + 1].is_ascii_digit());
            if prev_ok && starts_num {
                let start = i;
                let mut j = i;
                if bytes[j] == b'+' || bytes[j] == b'-' {
                    j += 1;
                }
                let digits_start = j;
                while j < n && (bytes[j].is_ascii_digit() || bytes[j] == b'_') {
                    j += 1;
                }
                // Only bare decimal integers (not floats, hex/oct/bin, dates).
                let is_other = j < n
                    && matches!(bytes[j], b'.' | b'e' | b'E' | b'x' | b'o' | b'b' | b':' | b'-');
                let has_digit = j > digits_start;
                if has_digit && !is_other {
                    let sign = &line[start..digits_start];
                    let digits: String =
                        line[digits_start..j].chars().filter(|&c| c != '_').collect();
                    if let Ok(v) = format!("{sign}{digits}").parse::<i128>() {
                        if v > i64::MAX as i128 || v < i64::MIN as i128 {
                            let ln = li + 1;
                            let lnstr = ln.to_string();
                            let num_gutter = format!(" {lnstr} |");
                            let sep_gutter = format!("{}|", " ".repeat(lnstr.len() + 2));
                            // 1-based column just past the literal.
                            let end_col = j + 1;
                            let caret_pad = " ".repeat(end_col - 1);
                            // toml11 colours its diagnostic (magenta bold) and
                            // ends with an ANSI reset on its own line; C++ Nix
                            // strips the escapes for a non-tty, leaving a
                            // trailing blank line. Emit the same escapes so
                            // jinx's `filter_ansi_escapes` reproduces it exactly.
                            return Some(format!(
                                "while parsing TOML: \x1b[35;1m[error] toml::parse_dec_integer: \
                                 too large integer: current max digits = 2^63\n \
                                 --> fromTOML\n\
                                 {sep_gutter}\n\
                                 {num_gutter} {line}\n\
                                 {sep_gutter} {caret_pad}^-- must be < 2^63\n\
                                 \x1b[0m"
                            ));
                        }
                    }
                }
                i = j;
                continue;
            }
            i += 1;
        }
    }
    None
}

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

fn toml_no_null_byte(vm: &mut VM, s: &[u8], pos: PosIdx) -> Result<(), ErrId> {
    if s.contains(&0) {
        // NUL is rendered as `␀` (U+2400), like C++ `forceNoNullByte`.
        let mut shown = Vec::with_capacity(s.len());
        for &b in s {
            if b == 0 {
                shown.extend_from_slice("␀".as_bytes());
            } else {
                shown.push(b);
            }
        }
        let mut msg = b"while parsing TOML: error: input string '".to_vec();
        msg.extend_from_slice(&shown);
        msg.extend_from_slice(
            b"' cannot be represented as Nix string because it contains null bytes",
        );
        return Err(vm.new_err(ErrKind::Eval, msg, pos));
    }
    Ok(())
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
        toml::Value::String(s) => {
            toml_no_null_byte(vm, s.as_bytes(), pos)?;
            mk_string(vm, s.as_bytes())
        }
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
                toml_no_null_byte(vm, k.as_bytes(), pos)?;
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
            let msg = e.message();
            // The `toml` crate collapses out-of-range integer literals to a
            // terse Rust message; C++ Nix surfaces toml11's multi-line
            // diagnostic. Re-synthesize it by locating the offending literal.
            let full = if msg.contains("too large to fit") || msg.contains("too small to fit")
            {
                toml_integer_overflow_message(text)
                    .unwrap_or_else(|| format!("while parsing TOML: {msg}"))
            } else {
                format!("while parsing TOML: {msg}")
            };
            return Err(vm.new_err(ErrKind::Eval, full, pos));
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
    // C++ forces each result at `result.determinePos(fn.determinePos(pos))`.
    let fn_pos = vm.determine_pos(&val(args[0]), pos);
    let scope = vm.temp_scope();
    let mut results: Vec<VRef> = Vec::with_capacity(elems.len());
    for el in &elems {
        let r = vm.call_function(args[0], &[*el], pos)?;
        let rc = temp_cell(vm, r);
        let rpos = vm.determine_pos(&val(rc), fn_pos);
        vm.force_list(
            rc,
            rpos,
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
    let mut syms: Vec<u32> = attrs_entries(&val(args[0])).iter().map(|a| a.sym).collect();
    syms.sort_by(|x, y| vm.symbols.resolve(Symbol(*x)).cmp(vm.symbols.resolve(Symbol(*y))));
    // The name Values are immortal (see `VM::symbol_string`), so they need no
    // temp rooting while the list is built.
    let mut cells: Vec<VRef> = Vec::with_capacity(syms.len());
    for s in &syms {
        cells.push(vm.symbol_string(Symbol(*s)));
    }
    Ok(vm.new_list_value(&cells))
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
    // Borrow the input payload directly instead of copying it. This is sound
    // because the bindings object is rooted by `args[1]` for the whole call,
    // the heap is non-moving, and bindings objects are never mutated in place
    // — every producer (`new_bindings`, `new_bindings_merge`, `new_bindings_raw`,
    // `immortal::bindings`) writes into a freshly-allocated object, and the loop
    // below only creates thunks (no user code, no in-place attr writes). So the
    // slice stays valid and unaliased across the allocating calls inside the
    // loop, making the old defensive `.to_vec()` unnecessary. `attrs_entries`
    // returns a slice with a free lifetime, so it does not borrow `vm`.
    let entries_in: &[Attr] = attrs_entries(&val(args[1]));
    let scope = vm.temp_scope();
    let mut entries = std::mem::take(&mut vm.scratch_attrs);
    entries.clear();
    entries.reserve(entries_in.len());
    for a in entries_in {
        let nc = vm.symbol_string(Symbol(a.sym));
        let t = vm.new_apply_thunk(args[0], &[nc, a.val]);
        let tc = temp_cell(vm, t);
        // C++ `attrs.alloc(i.name)` gives the result attributes no position.
        entries.push(Attr {
            sym: a.sym,
            pos: 0,
            val: tc,
        });
    }
    let v = vm.new_bindings_value(&entries);
    entries.clear();
    vm.scratch_attrs = entries;
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
    // Both attrsets are sorted by symbol id: keep e2's entries whose name is
    // also in e1. When the sizes are lopsided (the callPackage pattern
    // intersects ~10 formals against a ~20k-entry scope) iterate the small
    // side and binary-search the large one; otherwise a single linear merge.
    let mut entries: Vec<Attr> = Vec::with_capacity(e1.len().min(e2.len()));
    let (small_len, large_len) = (e1.len().min(e2.len()), e1.len().max(e2.len()));
    if small_len == 0 {
        // empty result
    } else if large_len / small_len >= 8 {
        if e1.len() <= e2.len() {
            // small = e1: keep the matching e2 entry.
            for a in e1 {
                if let Ok(k) = e2.binary_search_by(|x| x.sym.cmp(&a.sym)) {
                    entries.push(e2[k]);
                }
            }
        } else {
            // small = e2: keep e2's own entry on a hit in e1.
            for a in e2 {
                if e1.binary_search_by(|x| x.sym.cmp(&a.sym)).is_ok() {
                    entries.push(*a);
                }
            }
        }
    } else {
        let (mut i, mut j) = (0usize, 0usize);
        while i < e1.len() && j < e2.len() {
            match e1[i].sym.cmp(&e2[j].sym) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    entries.push(e2[j]);
                    i += 1;
                    j += 1;
                }
            }
        }
    }
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
            "while evaluating an element in the list passed as second argument to builtins.catAttrs",
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
    let mut done_elems: Vec<VRef> = Vec::new();
    let mut res: Vec<VRef> = Vec::new();
    while let Some(e) = work.pop_front() {
        // C++ adds "in genericClosure element %s" if forcing the element or
        // reading its 'key' fails.
        let elem_ctx = |vm: &mut VM, er: ErrId| {
            let printed = crate::print::print_value_err(vm, &val(e));
            vm.add_trace(er, NO_POS, format!("in genericClosure element {printed}"));
        };
        vm.force_attrs(e, NO_POS, "").map_err(|er| {
            elem_ctx(vm, er);
            er
        })?;
        let ev = val(e);
        let Some(key) = attrs_get(&ev, key_sym) else {
            let er = vm.new_err(ErrKind::Type, "attribute 'key' missing", NO_POS);
            elem_ctx(vm, er);
            vm.temp_end(scope);
            return Err(er);
        };
        vm.force(key.val, NO_POS)?;
        // Insert into done set (ordered by CompareValues). The new key is
        // compared *first* so an incomparability error reports the new
        // element's type before the existing one (matching std::map order).
        let mut lo = 0usize;
        let mut hi = done_keys.len();
        let mut is_dup = false;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let other_elem = done_elems[mid];
            let cmp = |vm: &mut VM, a: VRef, b: VRef| -> Result<bool, ErrId> {
                compare_values(vm, a, b, NO_POS, "").map_err(|er| {
                    // Pre-swapped for reverse printing: "with element",
                    // then "while comparing element".
                    let other_p = crate::print::print_value_err(vm, &val(other_elem));
                    let e_p = crate::print::print_value_err(vm, &val(e));
                    vm.add_trace(er, NO_POS, format!("with element {other_p}"));
                    vm.add_trace(er, NO_POS, format!("while comparing element {e_p}"));
                    er
                })
            };
            if cmp(vm, key.val, done_keys[mid])? {
                hi = mid;
            } else if cmp(vm, done_keys[mid], key.val)? {
                lo = mid + 1;
            } else {
                is_dup = true;
                break;
            }
        }
        if is_dup {
            continue;
        }
        done_keys.insert(lo, key.val);
        done_elems.insert(lo, e);
        vm.temp_roots.push(key.val);
        res.push(e);
        vm.temp_roots.push(e);

        // Call the operator and queue new elements. C++ wraps the call, the
        // return-value forceList, and forcing each element with a single
        // "while calling operator on genericClosure element %s" frame.
        let op_val = op.val;
        let queued: Result<Vec<VRef>, ErrId> = (|vm: &mut VM| {
            let r = vm.call_function(op_val, &[e], NO_POS)?;
            let rc = temp_cell(vm, r);
            vm.force_list(
                rc,
                NO_POS,
                "while evaluating the return value of the `operator` passed to builtins.genericClosure",
            )?;
            Ok(list_elems(&val(rc)).to_vec())
        })(vm);
        let new_elems = queued.map_err(|er| {
            let printed = crate::print::print_value_err(vm, &val(e));
            vm.add_trace(
                er,
                NO_POS,
                format!("while calling operator on genericClosure element {printed}"),
            );
            er
        })?;
        for n in new_elems {
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
    // Borrow the list payload directly (no `.to_vec()`): it is rooted by
    // `args[1]`, the heap is non-moving, and list objects are not mutated in
    // place, so the slice stays valid across the `force_attrs` calls below.
    // `list_elems` returns a free-lifetime slice, so it does not borrow `vm`.
    let lists: &[VRef] = list_elems(&val(args[1]));
    let mut buckets: Vec<(u32, Vec<VRef>)> = Vec::new();
    for el in lists {
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
    let mut entries = std::mem::take(&mut vm.scratch_attrs);
    entries.clear();
    entries.reserve(buckets.len());
    for (sym, vals) in &buckets {
        let nc = vm.symbol_string(Symbol(*sym));
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
    entries.clear();
    vm.scratch_attrs = entries;
    vm.temp_end(scope);
    Ok(v)
}

// ---------------------------------------------------------------------
// paths / files / env
// ---------------------------------------------------------------------

fn prim_fetch_tree(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    // Partial port: only the attribute-set validation errors are reproduced;
    // the actual fetch (and flake-ref string parsing) is not implemented.
    vm.force(args[0], pos)?;
    let av = val(args[0]);
    if av.tag() != Tag::Attrs {
        // String form: coerce to a URL and reproduce the tarball fetcher's
        // "file scheme relative path" diagnostic (the only string-form error
        // exercised by the suite).
        let (url, _) = vm.coerce_to_string(
            args[0],
            pos,
            "while evaluating the first argument passed to 'fetchTree'",
            false,
            false,
            false,
        )?;
        let us = String::from_utf8_lossy(&url).into_owned();
        if let Some(rest) = us.strip_prefix("file:") {
            let path = rest.strip_prefix("//").unwrap_or(rest);
            if !path.starts_with('/') {
                let msg = format!(
                    "tarball '{us}' must use an absolute path. The 'file' scheme does not support relative paths."
                );
                let e = vm.new_err(ErrKind::Eval, msg, pos);
                vm.add_trace(e, NO_POS, format!("while fetching the input '{us}'"));
                return Err(e);
            }
        }
        return Err(vm.new_err(
            ErrKind::Eval,
            "the 'fetchTree' builtin is not implemented by jinx yet",
            pos,
        ));
    }
    if av.tag() == Tag::Attrs {
        let type_sym = vm.symbols.create(b"type").0;
        let entries = attrs_entries(&av).to_vec();
        for a in &entries {
            if a.sym == type_sym {
                continue;
            }
            vm.force(a.val, PosIdx(a.pos))?;
            let v = val(a.val);
            if v.tag() == Tag::Int && v.as_int() < 0 {
                let name = String::from_utf8_lossy(vm.symbols.resolve(Symbol(a.sym))).into_owned();
                let msg = format!(
                    "negative value given for 'fetchTree' argument '{}': {}",
                    name,
                    v.as_int()
                );
                return Err(vm.new_err(ErrKind::Eval, msg, pos));
            }
        }
    }
    Err(vm.new_err(
        ErrKind::Eval,
        "the 'fetchTree' builtin is not implemented by jinx yet",
        pos,
    ))
}

fn prim_fetchurl(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    // Partial port of the fetchurl/fetchTree name-validation path; the actual
    // fetch is not implemented, but store-path-name errors are reproduced.
    vm.force(args[0], pos)?;
    let av = val(args[0]);
    let is_attrs = av.tag() == Tag::Attrs;
    let mut url: Option<Vec<u8>> = None;
    let mut name: Vec<u8> = Vec::new();
    let mut name_passed = false;
    if is_attrs {
        let entries = attrs_entries(&av).to_vec();
        for a in &entries {
            let n = vm.symbols.resolve(Symbol(a.sym)).to_vec();
            match n.as_slice() {
                b"url" => {
                    url = Some(vm.force_string_no_ctx(
                        a.val,
                        PosIdx(a.pos),
                        "while evaluating the url we should fetch",
                    )?)
                }
                b"sha256" => {
                    vm.force_string_no_ctx(
                        a.val,
                        PosIdx(a.pos),
                        "while evaluating the sha256 of the content we should fetch",
                    )?;
                }
                b"name" => {
                    name_passed = true;
                    name = vm.force_string_no_ctx(
                        a.val,
                        PosIdx(a.pos),
                        "while evaluating the name of the content we should fetch",
                    )?;
                }
                _ => {
                    let msg = format!(
                        "unsupported argument '{}' to 'fetchurl'",
                        String::from_utf8_lossy(&n)
                    );
                    return Err(vm.new_err(ErrKind::Eval, msg, pos));
                }
            }
        }
        if url.is_none() {
            return Err(vm.new_err(ErrKind::Eval, "'url' argument required", pos));
        }
    } else {
        url = Some(vm.force_string_no_ctx(
            args[0],
            pos,
            "while evaluating the url we should fetch",
        )?);
    }
    let url = url.unwrap();
    if name.is_empty() {
        // baseNameOf: the component after the last '/'.
        let trimmed = url.strip_suffix(b"/").unwrap_or(&url);
        name = match trimmed.iter().rposition(|&c| c == b'/') {
            Some(i) => trimmed[i + 1..].to_vec(),
            None => trimmed.to_vec(),
        };
    }
    if let Err(e) = jinx_store::store_path::check_name(&String::from_utf8_lossy(&name)) {
        let who = "fetchurl";
        let resolution = if name_passed {
            format!("Please change the value for the 'name' attribute passed to '{who}', so that it can create a valid store path.")
        } else if is_attrs {
            format!("Please add a valid 'name' attribute to the argument for '{who}', so that it can create a valid store path.")
        } else {
            format!("Please pass an attribute set with 'url' and 'name' attributes to '{who}',  so that it can create a valid store path.")
        };
        let msg = format!(
            "invalid store path name when fetching URL '{}': {}. {}",
            String::from_utf8_lossy(&url),
            e.0,
            resolution
        );
        return Err(vm.new_err(ErrKind::Eval, msg, pos));
    }
    Err(vm.new_err(
        ErrKind::Eval,
        "the 'fetchurl' builtin is not implemented by jinx yet",
        pos,
    ))
}

fn prim_store_path(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    // Only the coercion error path is exercised by the test suite; a genuine
    // store-path realisation is not implemented.
    let _p = coerce_to_path(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to 'builtins.storePath'",
    )?;
    Err(vm.new_err(
        ErrKind::Eval,
        "the 'storePath' builtin is not implemented by jinx yet",
        pos,
    ))
}

fn prim_filter_source(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    // C++ coerces the path (second argument) before anything else, then forces
    // the filter function, then walks the tree ("addPath"), invoking the filter
    // on each entry (port of `addPath` with `method = NixArchive`).
    let path = coerce_to_path(
        vm,
        args[1],
        pos,
        "while evaluating the second argument (the path to filter) passed to 'builtins.filterSource'",
    )?;
    force_fun(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.filterSource",
    )?;
    let filter = args[0];
    let path_str = String::from_utf8_lossy(&path).into_owned();
    let name = base_name_of(&path);
    let name_s = String::from_utf8_lossy(&name).into_owned();
    use std::os::unix::ffi::OsStrExt;
    let logical = std::path::Path::new(std::ffi::OsStr::from_bytes(&path));
    let real = vm.redirect_fs(logical);
    let refs = jinx_store::store_path::StorePathSet::new();
    let sp = add_filtered_path(
        vm,
        &name_s,
        real.as_path(),
        Some(filter),
        jinx_store::store_path::FileIngestionMethod::NixArchive,
        &refs,
    )
    .map_err(|e| {
        vm.add_trace(e, pos, format!("while adding path '{path_str}'"));
        e
    })?;
    let store = vm.store();
    let printed = store.print_store_path(&sp);
    let id = vm.intern_elem(&crate::context::ContextElem::Opaque {
        path: sp.to_string().as_bytes().to_vec(),
    });
    Ok(vm.new_string_ctx(printed.as_bytes(), &[id]))
}

/// `baseNameOf`: the path component after the last `/` (paths are canonicalized
/// by `coerce_to_path`, so there is no trailing slash to worry about).
fn base_name_of(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&c| c == b'/') {
        Some(i) => path[i + 1..].to_vec(),
        None => path.to_vec(),
    }
}

/// Port of `EvalState::callPathFilter`: call `filter path type` and force the
/// result to a Boolean. `type` is `"regular"`/`"directory"`/`"symlink"`/
/// `"unknown"` per the entry's lstat.
fn call_path_filter(vm: &mut VM, filter: VRef, path: &std::path::Path) -> Result<bool, ErrId> {
    let t = path
        .symlink_metadata()
        .map(|m| file_type_str(&m))
        .unwrap_or("unknown");
    use std::os::unix::ffi::OsStrExt;
    let pv = mk_string(vm, path.as_os_str().as_bytes());
    let pc = temp_cell(vm, pv);
    let tv = mk_string(vm, t.as_bytes());
    let tc = temp_cell(vm, tv);
    let scope = vm.temp_scope();
    vm.temp_roots.push(pc);
    vm.temp_roots.push(tc);
    let r = vm.call_function(filter, &[pc, tc], NO_POS);
    vm.temp_end(scope);
    let rv = r?;
    let rc = temp_cell(vm, rv);
    vm.force_bool(
        rc,
        NO_POS,
        "while evaluating the return value of the path filter function",
    )
}

/// Maximum recursion depth when comparing filter closures structurally. A
/// deeper structure conservatively compares UNEQUAL (recomputes the dump).
const FILTER_IDENT_MAX_DEPTH: u32 = 128;

/// Conservative structural identity for path-filter arguments, used as the
/// filter component of the filtered-dump memo key.
///
/// Soundness contract: a *false hit* would alias two distinct filters and
/// produce a wrong store path, so we only return `true` when we can prove the
/// filters equal WITHOUT evaluating anything. In particular thunks are never
/// forced — an unforced, non-pointer-equal thunk anywhere makes the comparison
/// UNEQUAL. A conservative miss merely recomputes the dump, which is always
/// safe.
fn filter_ident_eq(a: Option<VRef>, b: Option<VRef>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => value_ident_eq(x, y, 0),
        _ => false,
    }
}

fn value_ident_eq(a: VRef, b: VRef, depth: u32) -> bool {
    if a == b {
        // Same cell: identical by construction (short-circuits thunks too).
        return true;
    }
    if depth >= FILTER_IDENT_MAX_DEPTH {
        return false;
    }
    let va = val(a);
    let vb = val(b);
    let ta = va.tag();
    if ta != vb.tag() {
        return false;
    }
    match ta {
        Tag::Null | Tag::True | Tag::False => true,
        Tag::Int => va.as_int() == vb.as_int(),
        Tag::Float => va.as_float().to_bits() == vb.as_float().to_bits(),
        Tag::String => {
            // Require equal bytes and the same (or both-null) context object;
            // distinct-but-equal contexts conservatively compare unequal.
            str_bytes(&va) == str_bytes(&vb) && std::ptr::eq(str_ctx(&va), str_ctx(&vb))
        }
        Tag::Path => {
            // SAFETY: tag checked above; both are ObjKind::Path objects.
            let (acc_a, bytes_a) = unsafe { crate::value::path_parts(va.ptr() as *const u64) };
            let (acc_b, bytes_b) = unsafe { crate::value::path_parts(vb.ptr() as *const u64) };
            acc_a == acc_b && bytes_a == bytes_b
        }
        Tag::Closure => {
            let (code_a, ups_a) = crate::vm::thunk_code(&va);
            let (code_b, ups_b) = crate::vm::thunk_code(&vb);
            std::ptr::eq(code_a, code_b)
                && ups_a.len() == ups_b.len()
                && ups_a
                    .iter()
                    .zip(ups_b.iter())
                    .all(|(&x, &y)| value_ident_eq(x, y, depth + 1))
        }
        Tag::List => {
            let la = list_elems(&va);
            let lb = list_elems(&vb);
            la.len() == lb.len()
                && la
                    .iter()
                    .zip(lb.iter())
                    .all(|(&x, &y)| value_ident_eq(x, y, depth + 1))
        }
        Tag::Attrs => {
            let ba = attrs_entries(&va);
            let bb = attrs_entries(&vb);
            // `pos` must participate: `builtins.unsafeGetAttrPos` makes an
            // attribute's source position observable, so two attrsets that
            // agree on symbols+values but differ in positions are NOT
            // interchangeable inside a filter closure. Omitting this let the
            // filterSource memo reuse a cached path for a genuinely different
            // filter (adversarial-review finding: filter-pos-cache).
            ba.len() == bb.len()
                && ba.iter().zip(bb.iter()).all(|(x, y)| {
                    x.sym == y.sym
                        && x.pos == y.pos
                        && value_ident_eq(x.val, y.val, depth + 1)
                })
        }
        // Thunk / Blackhole / Failed / PrimOp / PrimOpApp: equal only if the
        // very same cell (already handled above). Never force; treat as unequal.
        _ => false,
    }
}

/// Port of `addPath` (without an expected hash): serialize the tree at
/// `real_path` (applying `filter` to each directory entry when given), compute
/// its content-addressed store path, and — under [`StoreMode::Daemon`] — add it
/// to the store. Returns the resulting [`StorePath`].
///
/// For the refs-empty case the (dump-hash, store-path) result is memoized in
/// `vm.filtered_path_cache`, keyed by the canonical real path, ingestion
/// method, name, and the structural identity of the filter closure.
fn add_filtered_path(
    vm: &mut VM,
    name: &str,
    real_path: &std::path::Path,
    filter: Option<VRef>,
    method: jinx_store::store_path::FileIngestionMethod,
    refs: &jinx_store::store_path::StorePathSet,
) -> Result<jinx_store::store_path::StorePath, ErrId> {
    use jinx_store::hash::HashAlgorithm;
    use jinx_store::store_path::{ContentAddressMethod, FileIngestionMethod, FixedOutputInfo, StoreReferences};

    // Memoize refs-empty results: the store path is then a pure function of the
    // dump, name and method, so identical (path, method, name, filter) inputs
    // yield the same path without re-dumping/re-hashing.
    let memo_key: Option<(Vec<u8>, u8, String)> = if refs.is_empty() {
        use std::os::unix::ffi::OsStrExt;
        let m: u8 = match method {
            FileIngestionMethod::Flat => 0,
            _ => 1,
        };
        Some((real_path.as_os_str().as_bytes().to_vec(), m, name.to_string()))
    } else {
        None
    };
    if let Some(key) = &memo_key {
        if let Some(entries) = vm.filtered_path_cache.get(key) {
            for (f, _hash, sp) in entries {
                if filter_ident_eq(*f, filter) {
                    return Ok(sp.clone());
                }
            }
        }
    }

    // Build the content-address "dump": raw contents for Flat, a (filtered) NAR
    // for the recursive method.
    let dump: Vec<u8> = match method {
        FileIngestionMethod::Flat => std::fs::read(real_path)
            .map_err(|e| vm.new_err(ErrKind::Eval, e.to_string(), NO_POS))?,
        _ => {
            let mut nar = Vec::new();
            match filter {
                None => jinx_store::nar::dump_path(real_path, &mut nar)
                    .map_err(|e| vm.new_err(ErrKind::Eval, e.to_string(), NO_POS))?,
                Some(f) => {
                    let mut pending: Option<ErrId> = None;
                    let res = {
                        let mut cb = |p: &std::path::Path| -> std::io::Result<bool> {
                            match call_path_filter(vm, f, p) {
                                Ok(b) => Ok(b),
                                Err(e) => {
                                    pending = Some(e);
                                    Err(std::io::Error::other("path filter error"))
                                }
                            }
                        };
                        jinx_store::nar::dump_path_filtered(real_path, &mut nar, &mut cb)
                    };
                    if let Some(e) = pending {
                        return Err(e);
                    }
                    res.map_err(|e| vm.new_err(ErrKind::Eval, e.to_string(), NO_POS))?;
                }
            }
            nar
        }
    };

    let hash = hash_string(HashAlgorithm::Sha256, &dump);
    let store = vm.store();
    let sp = store
        .make_fixed_output_path(
            name,
            &FixedOutputInfo {
                method,
                hash,
                references: StoreReferences {
                    others: refs.clone(),
                    self_ref: false,
                },
            },
        )
        .map_err(|e| vm.new_err(ErrKind::Eval, e.0, NO_POS))?;

    if vm.store_mode == crate::vm::StoreMode::Daemon {
        let cam_method = match method {
            FileIngestionMethod::Flat => ContentAddressMethod::Flat,
            _ => ContentAddressMethod::NixArchive,
        };
        let cam = jinx_store::daemon::cam_str(cam_method, HashAlgorithm::Sha256);
        let r: Result<(), String> = (|| {
            let Some(d) = vm.daemon() else { return Ok(()) };
            if !d.is_valid_path(&sp).map_err(|e| e.to_string())? {
                d.add_to_store_bytes(name, &cam, refs, false, &dump)
                    .map_err(|e| e.to_string())?;
            }
            d.add_temp_root(&sp).map_err(|e| e.to_string())?;
            Ok(())
        })();
        if let Err(msg) = r {
            return Err(vm.new_err(ErrKind::Eval, msg, NO_POS));
        }
    }

    if let Some(key) = memo_key {
        // Root the filter closure so its cell (and the upvalue cells reachable
        // from it) survive GC — the stored VRef is later compared structurally.
        if let Some(f) = filter {
            vm.perm_roots.push(f);
        }
        vm.filtered_path_cache
            .entry(key)
            .or_default()
            .push((filter, hash, sp.clone()));
    }

    Ok(sp)
}

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
        "while evaluating the first argument passed to builtins.toPath",
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
    let logical = PathBuf::from(String::from_utf8_lossy(&canon).into_owned());
    let path = vm.redirect_fs(&logical);
    let exists = path.symlink_metadata().is_ok();
    let ok = if dir_required {
        path.metadata().map(|m| m.is_dir()).unwrap_or(false)
    } else {
        exists
    };
    Ok(Value::bool(ok))
}

fn prim_read_file(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    realise_path_context(vm, args[0], pos)?;
    let p = coerce_to_path(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.readFile",
    )?;
    let path = String::from_utf8_lossy(&p).into_owned();
    let real = vm.redirect_fs(Path::new(&path));
    match std::fs::read(&real) {
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
    realise_path_context(vm, args[0], pos)?;
    let p = coerce_to_path(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.readDir",
    )?;
    let logical = String::from_utf8_lossy(&p).into_owned();
    let path = vm
        .redirect_fs(Path::new(&logical))
        .to_string_lossy()
        .into_owned();
    // Mirror C++ `realisePath` + `readDirectory`: a missing path yields
    // "path '%s' does not exist"; a non-directory yields "'%s' is not a
    // directory" (ENOTDIR from opendir on the symlink-resolved path).
    //
    // Successful listings are memoized by the resolved real path, since a
    // batch evaluator sees a stable filesystem (matches C++ caching directory
    // reads). Only success is cached; error paths re-probe.
    let items: Vec<(Vec<u8>, &'static str)> = if let Some(cached) = vm.read_dir_cache.get(&path) {
        cached.clone()
    } else {
        if std::fs::symlink_metadata(&path).is_err() {
            let msg = format!("path '{path}' does not exist");
            return Err(vm.new_err(ErrKind::Eval, msg, pos));
        }
        let target = std::fs::canonicalize(&path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.clone());
        let rd = match std::fs::read_dir(&target) {
            Ok(rd) => rd,
            Err(err) => {
                let msg = if err.raw_os_error() == Some(20) {
                    format!("'{target}' is not a directory")
                } else if err.kind() == std::io::ErrorKind::NotFound {
                    format!("path '{target}' does not exist")
                } else {
                    format!("reading directory '{}': {}", target, io_msg(&err))
                };
                return Err(vm.new_err(ErrKind::Eval, msg, pos));
            }
        };
        let mut items: Vec<(Vec<u8>, &'static str)> = Vec::new();
        for ent in rd {
            let Ok(ent) = ent else { continue };
            let name = ent.file_name().to_string_lossy().into_owned().into_bytes();
            // `DirEntry::file_type` reads dirent `d_type` (no extra syscall on
            // APFS); this matches C++ nix, which trusts d_type from
            // getdirentries. Fall back to lstat only when d_type is unknown.
            let t = match ent.file_type() {
                Ok(ft) if ft.is_symlink() => "symlink",
                Ok(ft) if ft.is_dir() => "directory",
                Ok(ft) if ft.is_file() => "regular",
                Ok(_) => "unknown",
                Err(_) => ent
                    .path()
                    .symlink_metadata()
                    .map(|m| file_type_str(&m))
                    .unwrap_or("unknown"),
            };
            items.push((name, t));
        }
        vm.read_dir_cache.insert(path.clone(), items.clone());
        items
    };
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
    realise_path_context(vm, args[0], pos)?;
    let p = coerce_to_path(
        vm,
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.readFileType",
    )?;
    let logical = PathBuf::from(String::from_utf8_lossy(&p).into_owned());
    let path = vm.redirect_fs(&logical);
    match path.symlink_metadata() {
        Ok(m) => Ok(mk_string(vm, file_type_str(&m).as_bytes())),
        Err(err) => {
            let msg = format!(
                "getting status of '{}': {}",
                logical.display(),
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
    // `blake3` is a recognized algorithm gated behind the `blake3-hashes`
    // experimental feature: C++ reports the feature-disabled *eval* error
    // (not the unknown-algorithm usage error) when it's off.
    if name == "blake3" && !vm.experimental.blake3_hashes {
        return Err(vm.new_err(
            ErrKind::Eval,
            "experimental Nix feature 'blake3-hashes' is disabled; add \
             '--extra-experimental-features blake3-hashes' to enable it",
            pos,
        ));
    }
    HashAlgorithm::parse_opt(&name).ok_or_else(|| {
        vm.new_err(
            ErrKind::Usage,
            format!("unknown hash algorithm '{name}', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"),
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
    let real = vm.redirect_fs(Path::new(&path));
    let bytes = std::fs::read(&real).map_err(|err| {
        let msg = if err.kind() == std::io::ErrorKind::NotFound {
            format!("path '{path}' does not exist")
        } else {
            format!("opening file '{}': {}", path, io_msg(&err))
        };
        vm.new_err(ErrKind::Eval, msg, pos)
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
    eval_file_traced(vm, path, pos, true)
}

/// Like [`eval_file`] but `add_trace` controls whether the
/// "while evaluating the file '…'" frame is attached on error. The top-level
/// nix-instantiate entry (`state.eval` on a parsed file) does *not* add it,
/// whereas `builtins.import` (`evalFile`) does.
pub fn eval_file_traced(
    vm: &mut VM,
    path: &Path,
    pos: PosIdx,
    add_trace: bool,
) -> Result<VRef, ErrId> {
    let resolved = resolve_expr_path_vm(vm, path);
    if let Some(&i) = vm.file_cache_idx.get(&resolved) {
        let cell = vm.file_cache[i].1;
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
        if add_trace {
            vm.add_trace(
                e,
                pos,
                format!("while evaluating the file '{}':", resolved.display()),
            );
        }
        e
    })?;
    vm.file_cache_idx.insert(resolved.clone(), vm.file_cache.len());
    vm.file_cache.push((resolved, cell));
    Ok(cell)
}

fn read_source(vm: &mut VM, resolved: &Path, pos: PosIdx) -> Result<Vec<u8>, ErrId> {
    if let Some(src) = corepkgs_source(resolved) {
        return Ok(src.to_vec());
    }
    let real = vm.redirect_fs(resolved);
    std::fs::read(&real).map_err(|err| {
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
///
/// All filesystem probes go through the VM's virtual-store redirect, so imports
/// of a computed flake store path are served from the real fetched tree. The
/// returned path stays *logical* (store-path form): symlink targets are rebased
/// onto the logical parent and the on-disk directory check redirects the probe.
///
/// Results are memoized in `vm.import_resolution_cache` (C++
/// `importResolutionCache`) since resolution is a pure function of the fixed
/// NIX_PATH config and the input path.
fn resolve_expr_path_vm(vm: &mut VM, path: &Path) -> PathBuf {
    if let Some(r) = vm.import_resolution_cache.get(path) {
        return r.clone();
    }
    let r = resolve_expr_path_vm_uncached(vm, path);
    vm.import_resolution_cache.insert(path.to_path_buf(), r.clone());
    r
}

fn resolve_expr_path_vm_uncached(vm: &VM, path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    if p.to_str() == Some("/__corepkgs__/fetchurl.nix") {
        return p;
    }
    for _ in 0..1024 {
        let real = vm.redirect_fs(&p);
        let Ok(md) = real.symlink_metadata() else { break };
        if !md.file_type().is_symlink() {
            break;
        }
        let Ok(target) = std::fs::read_link(&real) else { break };
        p = if target.is_absolute() {
            target
        } else {
            let parent = p.parent().unwrap_or(Path::new("/"));
            PathBuf::from(String::from_utf8_lossy(&crate::vm::canon_path(
                parent.join(target).to_string_lossy().as_bytes(),
            )).into_owned())
        };
    }
    if vm.redirect_fs(&p).metadata().map(|m| m.is_dir()).unwrap_or(false) {
        p.join("default.nix")
    } else {
        p
    }
}

/// Port of the build step of `EvalState::realiseContext` / `realisePath`: when
/// the value being read is a string whose context references a derivation output
/// (`Built`), build that output through the daemon (import-from-derivation) so
/// its (input-addressed) store path becomes valid on disk before we read it.
///
/// Only `Built` elements are acted on; `Opaque`/`DrvDeep` are left alone (their
/// validity is not enforced here, so virtual-store/flake redirects still work).
/// A no-op under the dummy store (no build backend).
fn realise_path_context(vm: &mut VM, cell: VRef, pos: PosIdx) -> Result<(), ErrId> {
    use crate::context::ContextElem;
    use jinx_store::daemon::{DerivedPath, OutputsSpec};
    use jinx_store::store_path::StorePath;

    if vm.store_mode != crate::vm::StoreMode::Daemon {
        return Ok(());
    }
    vm.force(cell, pos)?;
    let v = val(cell);
    // Paths carry no context. Strings expose theirs directly; a derivation (or
    // other attrset/functor) is coerced to its `outPath` string to recover the
    // `Built` context, matching C++ `coerceToPath` feeding `realiseContext`.
    let ids: Vec<u32> = match v.tag() {
        Tag::Path => return Ok(()),
        Tag::String => vm.read_str_ctx(&v),
        _ => match vm.coerce_to_string(
            cell,
            pos,
            "while realising the context of a path",
            false,
            false,
            true,
        ) {
            Ok((_, cids)) => cids,
            // Not coercible to a path-like string: let coerce_to_path report it.
            Err(_) => return Ok(()),
        },
    };
    if ids.is_empty() {
        return Ok(());
    }

    // Collect the derivation outputs to build (dedup by drv, union of outputs).
    let mut wanted: std::collections::BTreeMap<StorePath, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for id in ids {
        if let ContextElem::Built { drv_path, output } = vm.ctx_elem(id) {
            let drv_s = String::from_utf8_lossy(&drv_path).into_owned();
            if let Ok(sp) = StorePath::new(&drv_s) {
                wanted
                    .entry(sp)
                    .or_default()
                    .insert(String::from_utf8_lossy(&output).into_owned());
            }
        }
    }
    if wanted.is_empty() {
        return Ok(());
    }

    let reqs: Vec<DerivedPath> = wanted
        .into_iter()
        .map(|(drv_path, outs)| DerivedPath::Built {
            drv_path,
            outputs: OutputsSpec::Names(outs),
        })
        .collect();

    let r: Result<(), String> = (|| {
        let Some(d) = vm.daemon() else { return Ok(()) };
        d.build_paths(&reqs, 0).map_err(|e| e.to_string())
    })();
    if let Err(msg) = r {
        return Err(vm.new_err(ErrKind::Eval, msg, pos));
    }
    Ok(())
}

fn prim_import(vm: &mut VM, _d: &'static PrimOpDef, args: &[VRef], pos: PosIdx) -> R {
    realise_path_context(vm, args[0], pos)?;
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
    realise_path_context(vm, args[1], pos)?;
    let p = coerce_to_path(
        vm,
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.scopedImport",
    )?;
    let path = PathBuf::from(String::from_utf8_lossy(&p).into_owned());
    let resolved = resolve_expr_path_vm(vm, &path);
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
