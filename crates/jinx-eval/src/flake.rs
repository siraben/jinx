//! Flake evaluation: `builtins.getFlake` and the `call-flake.nix` bootstrap.
//!
//! Port of the evaluation-side flake machinery (`src/libflake/flake.cc`
//! `callFlake` + `src/libflake/call-flake.nix` + the `fetchFinalTree` internal
//! primop). jinx computes store paths read-only, so the fetched flake tree is
//! surfaced through the VM's virtual-store redirect (see [`VM::add_store_redirect`])
//! rather than a realised store path.

use std::path::{Path, PathBuf};

use jinx_fetch::attrs::{maybe_get_str, Attr as FAttr, Attrs};
use jinx_flake::registry;
use jinx_flake::FlakeRef;
use jinx_store::hash::{HashAlgorithm, HashFormat};
use jinx_store::store_path::{FileIngestionMethod, FixedOutputInfo, StoreDir, StoreReferences};

use crate::context::ContextElem;
use crate::error::{ErrId, ErrKind};
use crate::value::{Attr, Tag, Value, VRef};
use crate::vm::{attrs_entries, str_bytes, val, PrimOpDef, VM};
use jinx_syntax::pos::PosIdx;
use jinx_syntax::symbol::Symbol;
use jinx_syntax::Origin;

/// The `call-flake.nix` helper, vendored verbatim from upstream Nix
/// `src/libflake/call-flake.nix`. Provenance: the NixOS/nix source tree.
pub const CALL_FLAKE_NIX: &[u8] = include_bytes!("call-flake.nix");

/// Origin name Nix shows for the embedded `call-flake.nix` (the memory source
/// accessor's `setPathDisplay("«flakes-internal»", "")`).
pub const CALL_FLAKE_ORIGIN: &str = "«flakes-internal»/call-flake.nix";

/// The fetched flake source tree.
struct Fetched {
    /// The content-addressed store path (`/nix/store/<hash>-source`).
    store_path_printed: String,
    store_base: String,
    /// The real on-disk directory the store path is redirected to.
    real_dir: PathBuf,
    /// The original working directory of the flake (for git flakes this is the
    /// checkout, not the `git archive` export), used to read `flake.lock`.
    orig_dir: PathBuf,
    nar_hash_sri: String,
    rev: Option<String>,
    rev_count: Option<u64>,
    last_modified: Option<i64>,
    is_git: bool,
}

fn eval_err(vm: &mut VM, msg: impl Into<String>, pos: PosIdx) -> ErrId {
    vm.new_err(ErrKind::Eval, msg.into(), pos)
}

/// Entry point for `builtins.getFlake` and the `nix eval` installable path:
/// resolve `flakeref_str`, fetch its source, and evaluate the flake, returning
/// the flake `result` attrset (outputs `//` sourceInfo `//` { outPath, ... }).
pub fn get_flake(vm: &mut VM, flakeref_str: &str, pos: PosIdx) -> Result<VRef, ErrId> {
    if !vm.experimental.flakes {
        return Err(eval_err(
            vm,
            "experimental Nix feature 'flakes' is disabled; add '--extra-experimental-features flakes' to enable it",
            pos,
        ));
    }
    let (attrs, subdir) = resolve_flakeref(vm, flakeref_str, pos)?;
    let fetched = fetch_source(vm, &attrs, pos)?;

    // Register the virtual-store redirect so `import outPath/flake.nix` and all
    // relative reads under the store path resolve to the real fetched tree.
    vm.add_store_redirect(
        fetched.store_path_printed.clone().into_bytes(),
        fetched.real_dir.clone(),
    );

    // Read the flake's lock file (from the flake subdir of the original working
    // directory), or synthesize a single-root lock for a lockfile-less flake
    // (port of `readLockFile`). NOTE: jinx reads an existing `flake.lock`; it
    // does not implement `lockFlake`'s dynamic input resolution, so flakes whose
    // lock is not present on disk (e.g. computed in-memory) are unsupported.
    let mut lock_path = fetched.orig_dir.clone();
    if !subdir.is_empty() {
        lock_path.push(&subdir);
    }
    lock_path.push("flake.lock");
    let lock_str = match std::fs::read_to_string(&lock_path) {
        Ok(s) => s,
        Err(_) => jinx_flake::LockFile::empty().to_json().to_string(),
    };

    call_flake(vm, &fetched, &subdir, &lock_str, pos)
}

/// Parse and (for indirect refs) registry-resolve a flakeref into locked input
/// attributes plus a subdirectory, converting bare local paths inside a git
/// working tree into a `git` input (port of `parsePathFlakeRefWithFragment`).
fn resolve_flakeref(
    vm: &mut VM,
    flakeref_str: &str,
    pos: PosIdx,
) -> Result<(Attrs, String), ErrId> {
    let fref = FlakeRef::parse(flakeref_str)
        .map_err(|e| eval_err(vm, format!("cannot parse flake reference '{flakeref_str}': {e}"), pos))?;
    let mut attrs = fref.attrs.clone();
    let mut subdir = fref.subdir.clone();

    // Registry resolution for indirect refs. Registry-resolved `path:` targets
    // stay path inputs (only *bare* CLI paths are promoted to git below).
    if maybe_get_str(&attrs, "type") == Some("indirect") {
        let registries = registry::default_registries();
        let (resolved, extra) = registry::lookup_in_registries(&attrs, &registries)
            .map_err(|e| eval_err(vm, format!("{e}"), pos))?;
        attrs = resolved;
        if let Some(dir) = maybe_get_str(&extra, "dir") {
            subdir = dir.to_string();
        }
        if let Some(dir) = maybe_get_str(&attrs, "dir") {
            subdir = dir.to_string();
            attrs.remove("dir");
        }
        return Ok((attrs, subdir));
    }

    // Convert a *bare* local path inside a git checkout into a `git` input
    // (port of parsePathFlakeRefWithFragment's `.git` walk). Explicit `path:`
    // URIs are left untouched.
    let is_bare_path = !flakeref_str.starts_with("path:");
    if is_bare_path && maybe_get_str(&attrs, "type") == Some("path") {
        if let Some(path) = maybe_get_str(&attrs, "path").map(|s| s.to_string()) {
            if let Some((root, sub)) = detect_git_root(&path) {
                let mut g = Attrs::new();
                g.insert("type".into(), FAttr::Str("git".into()));
                g.insert("url".into(), FAttr::Str(format!("file://{root}")));
                attrs = g;
                if !sub.is_empty() {
                    subdir = sub;
                }
            }
        }
    }
    Ok((attrs, subdir))
}

/// Port of the `.git` walk in `parsePathFlakeRefWithFragment`: from `path`,
/// ascend until a `.git` is found, returning (repo-root, subdir-within-repo).
fn detect_git_root(path: &str) -> Option<(String, String)> {
    let mut cur = PathBuf::from(path);
    let mut parts: Vec<String> = Vec::new();
    loop {
        if cur.join(".git").exists() {
            let subdir = {
                let mut v = parts.clone();
                v.reverse();
                v.join("/")
            };
            return Some((cur.to_string_lossy().into_owned(), subdir));
        }
        let name = cur.file_name()?.to_string_lossy().into_owned();
        parts.push(name);
        cur = cur.parent()?.to_path_buf();
    }
}

/// Fetch a flake source tree (path or git+file). Other schemes error.
fn fetch_source(vm: &mut VM, attrs: &Attrs, pos: PosIdx) -> Result<Fetched, ErrId> {
    let store = vm.store();
    match maybe_get_str(attrs, "type") {
        Some("git") => fetch_git(vm, attrs, &store, pos),
        Some("path") => fetch_path(vm, attrs, &store, pos),
        Some(other) => Err(eval_err(
            vm,
            format!("the flake fetcher for inputs of type '{other}' is not implemented by jinx yet"),
            pos,
        )),
        None => Err(eval_err(vm, "flake input has no 'type'", pos)),
    }
}

fn nar_ca_store_path(
    store: &StoreDir,
    dir: &Path,
) -> Result<(jinx_store::store_path::StorePath, String), String> {
    let (nar_hash, _size) = jinx_store::nar::hash_path(dir, HashAlgorithm::Sha256)
        .map_err(|e| format!("hashing '{}': {e}", dir.display()))?;
    let sp = store
        .make_fixed_output_path(
            "source",
            &FixedOutputInfo {
                method: FileIngestionMethod::NixArchive,
                hash: nar_hash,
                references: StoreReferences::default(),
            },
        )
        .map_err(|e| e.0)?;
    Ok((sp, nar_hash.to_string(HashFormat::Sri, true)))
}

/// Fetch a `git+file://` local working tree (clean HEAD, like C++ Nix's git
/// fetcher when the workdir has no tracked modifications). The tree is exported
/// via `git archive` and content-addressed as NAR (matching the oracle exactly).
fn fetch_git(vm: &mut VM, attrs: &Attrs, store: &StoreDir, pos: PosIdx) -> Result<Fetched, ErrId> {
    let url = maybe_get_str(attrs, "url")
        .ok_or_else(|| eval_err(vm, "git input has no 'url'", pos))?
        .to_string();
    let local = url
        .strip_prefix("git+")
        .unwrap_or(&url)
        .strip_prefix("file://")
        .map(|s| s.to_string())
        .ok_or_else(|| {
            eval_err(
                vm,
                format!("the git fetcher only supports local 'file://' URLs, got '{url}'"),
                pos,
            )
        })?;

    let rev = match maybe_get_str(attrs, "rev") {
        Some(r) => r.to_string(),
        None => git_output(&local, &["rev-parse", "HEAD"])
            .map_err(|e| eval_err(vm, format!("determining git HEAD of '{local}': {e}"), pos))?,
    };

    // Export the tree to a temp dir and NAR content-address it. Hand the dir
    // to the VM immediately so it is removed when the VM drops, even if the
    // archive below fails -- otherwise every git-flake eval leaks a full
    // source-tree copy into $TMPDIR.
    let export = make_temp_dir();
    vm.own_temp_dir(export.clone());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "git -C {} archive --format=tar {} | tar -x -C {}",
            shell_quote(&local),
            shell_quote(&rev),
            shell_quote(&export.to_string_lossy())
        ))
        .status()
        .map_err(|e| eval_err(vm, format!("running git archive: {e}"), pos))?;
    if !status.success() {
        return Err(eval_err(
            vm,
            format!("git archive of '{local}' at '{rev}' failed"),
            pos,
        ));
    }

    let (sp, nar_hash_sri) = nar_ca_store_path(store, &export)
        .map_err(|e| eval_err(vm, e, pos))?;
    let rev_count = git_output(&local, &["rev-list", "--count", &rev])
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    let last_modified = git_output(&local, &["log", "-1", "--format=%ct", &rev])
        .ok()
        .and_then(|s| s.parse::<i64>().ok());

    Ok(Fetched {
        store_path_printed: store.print_store_path(&sp),
        store_base: sp.to_string().to_string(),
        real_dir: export,
        orig_dir: PathBuf::from(&local),
        nar_hash_sri,
        rev: Some(rev),
        rev_count,
        last_modified,
        is_git: true,
    })
}

/// Fetch a `path:` input: content-address the whole tree as NAR (no filtering),
/// redirecting to the directory itself.
fn fetch_path(vm: &mut VM, attrs: &Attrs, store: &StoreDir, pos: PosIdx) -> Result<Fetched, ErrId> {
    let path = maybe_get_str(attrs, "path")
        .ok_or_else(|| eval_err(vm, "path input has no 'path'", pos))?
        .to_string();
    let real = PathBuf::from(&path);
    if !real.is_absolute() {
        return Err(eval_err(vm, format!("path input '{path}' is not absolute"), pos));
    }
    let (sp, nar_hash_sri) = nar_ca_store_path(store, &real).map_err(|e| eval_err(vm, e, pos))?;
    let last_modified = dir_last_modified(&real);
    Ok(Fetched {
        store_path_printed: store.print_store_path(&sp),
        store_base: sp.to_string().to_string(),
        real_dir: real.clone(),
        orig_dir: real,
        nar_hash_sri,
        rev: None,
        rev_count: None,
        last_modified,
        is_git: false,
    })
}

/// The newest mtime in a tree (port of `dumpPathAndGetMtime`), used for a path
/// input's `lastModified`.
fn dir_last_modified(dir: &Path) -> Option<i64> {
    fn walk(p: &Path, best: &mut i64) {
        if let Ok(md) = p.symlink_metadata() {
            if let Ok(t) = md.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    let secs = d.as_secs() as i64;
                    if secs > *best {
                        *best = secs;
                    }
                }
            }
            if md.is_dir() {
                if let Ok(rd) = std::fs::read_dir(p) {
                    for e in rd.flatten() {
                        walk(&e.path(), best);
                    }
                }
            }
        }
    }
    let mut best = 0i64;
    walk(dir, &mut best);
    if best == 0 {
        None
    } else {
        Some(best)
    }
}

/// Build the sourceInfo attrset (port of `emitTreeAttrs`) for a fetched tree.
fn build_source_info(vm: &mut VM, f: &Fetched) -> VRef {
    let mut children: Vec<(Vec<u8>, VRef)> = Vec::new();

    // outPath: the store path string with an opaque store-path context element.
    let ctx_id = vm.intern_elem(&ContextElem::Opaque {
        path: f.store_base.as_bytes().to_vec(),
    });
    let out_v = vm.new_string_ctx(f.store_path_printed.as_bytes(), &[ctx_id]);
    children.push((b"outPath".to_vec(), root_cell(vm, out_v)));

    let nar_v = vm.new_string_value(f.nar_hash_sri.as_bytes(), std::ptr::null_mut());
    children.push((b"narHash".to_vec(), root_cell(vm, nar_v)));

    if f.is_git {
        children.push((b"submodules".to_vec(), vm.false_cell));
    }

    if let Some(rev) = &f.rev {
        let rev_v = vm.new_string_value(rev.as_bytes(), std::ptr::null_mut());
        children.push((b"rev".to_vec(), root_cell(vm, rev_v)));
        let short: String = rev.chars().take(7).collect();
        let sr_v = vm.new_string_value(short.as_bytes(), std::ptr::null_mut());
        children.push((b"shortRev".to_vec(), root_cell(vm, sr_v)));
    }
    if let Some(rc) = f.rev_count {
        let c = root_cell(vm, Value::int(rc as i64));
        children.push((b"revCount".to_vec(), c));
    }
    if let Some(lm) = f.last_modified {
        let c = root_cell(vm, Value::int(lm));
        children.push((b"lastModified".to_vec(), c));
        let date = format_git_date(lm);
        let dv = vm.new_string_value(date.as_bytes(), std::ptr::null_mut());
        children.push((b"lastModifiedDate".to_vec(), root_cell(vm, dv)));
    }

    mk_attrs(vm, children)
}

/// UTC `strftime("%Y%m%d%H%M%S")` of a unix timestamp (port of the
/// `lastModifiedDate` derivation in `emitTreeAttrs`).
fn format_git_date(secs: i64) -> String {
    // Civil-from-days algorithm (Howard Hinnant), UTC.
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}{:02}{:02}{:02}{:02}{:02}", y, m, d, hh, mm, ss)
}

/// Evaluate `call-flake.nix` with (lockFileStr, overrides, fetchTreeFinal).
fn call_flake(
    vm: &mut VM,
    f: &Fetched,
    subdir: &str,
    lock_str: &str,
    pos: PosIdx,
) -> Result<VRef, ErrId> {
    let scope = vm.temp_scope();

    let source_info = build_source_info(vm, f);
    vm.temp_roots.push(source_info);

    // overrides = { root = { sourceInfo = <sourceInfo>; dir = <subdir>; }; }
    let dir_v = vm.new_string_value(subdir.as_bytes(), std::ptr::null_mut());
    let dir_cell = root_cell(vm, dir_v);
    let override_entry = mk_attrs(
        vm,
        vec![
            (b"sourceInfo".to_vec(), source_info),
            (b"dir".to_vec(), dir_cell),
        ],
    );
    vm.temp_roots.push(override_entry);
    let overrides = mk_attrs(vm, vec![(b"root".to_vec(), override_entry)]);
    vm.temp_roots.push(overrides);

    let lock_v = vm.new_string_value(lock_str.as_bytes(), std::ptr::null_mut());
    let lock_cell = root_cell(vm, lock_v);

    let fetch_final = fetch_tree_final_cell(vm);

    let call_flake_fn = compiled_call_flake(vm)?;

    let result = vm.call_function(call_flake_fn, &[lock_cell, overrides, fetch_final], pos);
    // Root the result before the scope closes.
    match result {
        Ok(v) => {
            let c = vm.alloc_cell(v);
            vm.perm_roots.push(c);
            vm.temp_end(scope);
            Ok(c)
        }
        Err(e) => {
            vm.temp_end(scope);
            Err(e)
        }
    }
}

/// The compiled `call-flake.nix` lambda closure, cached on the VM.
fn compiled_call_flake(vm: &mut VM) -> Result<VRef, ErrId> {
    if let Some(c) = vm.call_flake_fn {
        return Ok(c);
    }
    let src = CALL_FLAKE_NIX.to_vec();
    let mut warnings = Vec::new();
    let parsed = jinx_syntax::parse_and_bind_with(
        &src,
        Origin::Path {
            path: CALL_FLAKE_ORIGIN.to_string(),
            source: src.clone(),
        },
        "/",
        None,
        &mut vm.positions,
        &mut vm.symbols,
        &mut warnings,
    )
    .map_err(|pe| vm.new_err(ErrKind::Eval, pe.msg.clone(), pe.pos))?;
    let prog = crate::compile::compile_program(
        &parsed.0,
        parsed.1,
        &vm.symbols,
        &vm.globals,
        vm.empty_list_cell,
    );
    let cell = vm.run_program(prog)?;
    vm.perm_roots.push(cell);
    vm.call_flake_fn = Some(cell);
    Ok(cell)
}

/// The `fetchFinalTree` internal primop cell (passed to call-flake.nix).
fn fetch_tree_final_cell(vm: &mut VM) -> VRef {
    if let Some(c) = vm.fetch_tree_final_fn {
        return c;
    }
    let def: &'static PrimOpDef = Box::leak(Box::new(PrimOpDef {
        name: "fetchFinalTree",
        arity: 1,
        func: prim_fetch_final_tree,
    }));
    let cell = crate::immortal::cell(Value::make(Tag::PrimOp, def as *const _ as u64));
    vm.perm_roots.push(cell);
    vm.fetch_tree_final_fn = Some(cell);
    cell
}

/// The `fetchFinalTree` primop: fetch a fully-locked input (attrset) and return
/// its sourceInfo. Used by call-flake.nix for non-override (locked) nodes.
fn prim_fetch_final_tree(
    vm: &mut VM,
    _d: &'static PrimOpDef,
    args: &[VRef],
    pos: PosIdx,
) -> Result<Value, ErrId> {
    vm.force(args[0], pos)?;
    let av = val(args[0]);
    if av.tag() != Tag::Attrs {
        return Err(eval_err(vm, "fetchFinalTree expects an attribute set", pos));
    }
    // Convert the Nix attrset to fetcher Attrs.
    let attrs = nix_attrs_to_fetch_attrs(vm, args[0], pos)?;
    let f = fetch_source(vm, &attrs, pos)?;
    vm.add_store_redirect(
        f.store_path_printed.clone().into_bytes(),
        f.real_dir.clone(),
    );
    let c = build_source_info(vm, &f);
    Ok(val(c))
}

/// Convert a forced Nix attrset value into fetcher [`Attrs`] (strings/ints/bools).
fn nix_attrs_to_fetch_attrs(vm: &mut VM, cell: VRef, pos: PosIdx) -> Result<Attrs, ErrId> {
    vm.force(cell, pos)?;
    let av = val(cell);
    let entries = attrs_entries(&av).to_vec();
    let mut out = Attrs::new();
    for a in &entries {
        let name = vm.symbols.resolve_str_lossy(Symbol(a.sym));
        vm.force(a.val, pos)?;
        let v = val(a.val);
        match v.tag() {
            Tag::String | Tag::SmallString => {
                out.insert(name, FAttr::Str(String::from_utf8_lossy(str_bytes(&v)).into_owned()));
            }
            Tag::Int => {
                let i = v.as_int();
                if i >= 0 {
                    out.insert(name, FAttr::Int(i as u64));
                }
            }
            Tag::True => {
                out.insert(name, FAttr::Bool(true));
            }
            Tag::False => {
                out.insert(name, FAttr::Bool(false));
            }
            _ => {}
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn root_cell(vm: &mut VM, v: Value) -> VRef {
    let c = vm.alloc_cell(v);
    vm.temp_roots.push(c);
    c
}

fn mk_attrs(vm: &mut VM, children: Vec<(Vec<u8>, VRef)>) -> VRef {
    let mut entries: Vec<Attr> = children
        .iter()
        .map(|(name, cell)| Attr {
            sym: vm.symbols.create(name).0,
            pos: 0,
            val: *cell,
        })
        .collect();
    entries.sort_by_key(|a| a.sym);
    let v = vm.new_bindings_value(&entries);
    root_cell(vm, v)
}

fn git_output(dir: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn make_temp_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("jinx-flake-{pid}-{n}"));
    let _ = std::fs::create_dir_all(&base);
    base
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
