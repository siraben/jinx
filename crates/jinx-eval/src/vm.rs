//! The stack VM: frames over compiled chunks, in-place thunk forcing with
//! blackholing and error memoization, a runtime with-chain, and the C++
//! call/equality/coercion semantics ported from eval.cc.
//!
//! # GC discipline
//! `Heap` allocation never collects. Collections run only from
//! `VM::gc_check()`, which is called at op dispatch and at the head of the
//! `alloc_*`/`new_*` wrappers. Precise roots: the operand stack, frames
//! (thunk payloads + with-chains), `temp_roots`, and the import cache;
//! immortal globals/constants need no rooting. Native `VRef`/`Value`
//! locals in builtins are covered by the conservative stack scan, but
//! `Vec<VRef>` *contents* live on the Rust heap and are NOT scanned:
//! builtins accumulating cells in vectors must root them via
//! `TempRoots` (see `VM::temp_scope`).

use rustc_hash::FxHashMap;
use std::ptr::NonNull;

use jinx_syntax::pos::{PosIdx, PosTable, NO_POS};
use jinx_syntax::symbol::{Symbol, SymbolTable};

use crate::chunk::{Chunk, CodeRef, Op, CTX_STRINGS};
use crate::compile::SpecialSyms;
use crate::error::{best_matches, ErrId, ErrKind, EvalError, Trace};
use crate::heap::Heap;
use crate::immortal;
use crate::value::{self, Attr, Tag, VRef, Value};

/// Which store backend evaluation-time store effects (toFile, path adds,
/// derivation writes, IFD builds) use. Mirrors C++ `openStore` selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreMode {
    /// Read-only: compute store paths but never write. Used under
    /// `NIX_REMOTE=dummy://` and `--readonly-mode`.
    Dummy,
    /// Talk to a local `nix-daemon` over its Unix socket (lazily connected).
    Daemon,
}

#[inline]
pub fn val(c: VRef) -> Value {
    // SAFETY: cells handed to the VM are live.
    unsafe { *c.as_ptr() }
}

#[inline]
pub fn set(c: VRef, v: Value) {
    // SAFETY: cells handed to the VM are live.
    unsafe { *c.as_ptr() = v }
}

/// Typed views (thin unsafe wrappers; heap objects are non-moving).
#[inline]
pub fn str_bytes<'a>(v: &Value) -> &'a [u8] {
    debug_assert_eq!(v.tag(), Tag::String);
    // SAFETY: tag invariant.
    unsafe { value::str_parts(v.ptr() as *const u64).0 }
}

#[inline]
pub fn str_ctx(v: &Value) -> *mut u64 {
    // SAFETY: tag invariant.
    unsafe { value::str_parts(v.ptr() as *const u64).1 }
}

/// The string-context ids attached to a string `Value` (empty if none).
/// A context object stores `header_len` `u32` ids after its header word.
///
/// The slice borrows through `v`; callers needing to outlive the borrow copy it.
#[inline]
pub fn str_ctx_ids<'a>(v: &'a Value) -> &'a [u32] {
    let cp = str_ctx(v);
    if cp.is_null() {
        return &[];
    }
    // SAFETY: a non-null string-context object holds `header_len` u32 ids
    // immediately after the header word (see `Heap::new_string`).
    unsafe {
        let len = value::header_len(*cp);
        std::slice::from_raw_parts(cp.add(1) as *const u32, len)
    }
}

#[inline]
pub fn path_bytes<'a>(v: &Value) -> &'a [u8] {
    debug_assert_eq!(v.tag(), Tag::Path);
    // SAFETY: tag invariant.
    unsafe { value::path_parts(v.ptr() as *const u64).1 }
}

#[inline]
pub fn list_elems<'a>(v: &Value) -> &'a [VRef] {
    debug_assert_eq!(v.tag(), Tag::List);
    // SAFETY: tag invariant.
    unsafe { value::elems(v.ptr() as *const u64) }
}

#[inline]
pub fn attrs_entries<'a>(v: &Value) -> &'a [Attr] {
    debug_assert_eq!(v.tag(), Tag::Attrs);
    // SAFETY: tag invariant.
    unsafe { value::bindings(v.ptr() as *const u64) }
}

pub fn attrs_get(v: &Value, sym: Symbol) -> Option<Attr> {
    let es = attrs_entries(v);
    es.binary_search_by(|a| a.sym.cmp(&sym.0))
        .ok()
        .map(|i| es[i])
}

pub struct PrimOpDef {
    /// Registration name (may have "__" prefix).
    pub name: &'static str,
    pub arity: u8,
    pub func: fn(&mut VM, &'static PrimOpDef, &[VRef], PosIdx) -> Result<Value, ErrId>,
}

impl PrimOpDef {
    pub fn display(&self) -> &'static str {
        self.name.strip_prefix("__").unwrap_or(self.name)
    }
}

pub fn primop_of(v: &Value) -> &'static PrimOpDef {
    debug_assert_eq!(v.tag(), Tag::PrimOp);
    // SAFETY: PrimOp cells always carry a &'static PrimOpDef.
    unsafe { &*(v.w1 as *const PrimOpDef) }
}

/// (primop, applied arg cells) of a PrimOpApp value.
pub fn primapp_parts<'a>(v: &Value) -> (&'static PrimOpDef, &'a [VRef]) {
    debug_assert_eq!(v.tag(), Tag::PrimOpApp);
    // SAFETY: tag invariant.
    unsafe {
        let (code, elems) = value::code_and_elems(v.ptr() as *const u64);
        (&*(code as *const PrimOpDef), elems)
    }
}

pub fn thunk_code(v: &Value) -> (&'static CodeRef, &'static [VRef]) {
    debug_assert!(matches!(v.tag(), Tag::Thunk | Tag::Closure | Tag::Blackhole));
    // SAFETY: thunk/closure data objects carry a &'static CodeRef.
    unsafe {
        let (code, elems) = value::code_and_elems(v.ptr() as *const u64);
        (
            &*(code as *const CodeRef),
            std::slice::from_raw_parts(elems.as_ptr(), elems.len()),
        )
    }
}

pub struct Frame {
    pub code: &'static CodeRef,
    /// The thunk/closure value being run (roots the upvalue array).
    pub data: Value,
    pub locals_base: usize,
    pub with_local: Vec<VRef>,
}

impl Frame {
    pub(crate) fn upvals(&self) -> &'static [VRef] {
        if matches!(self.data.tag(), Tag::Thunk | Tag::Closure) {
            thunk_code(&self.data).1
        } else {
            &[]
        }
    }
}

pub struct VM {
    pub heap: Heap,
    pub stack: crate::stack::Stack,
    pub frames: Vec<Frame>,
    pub temp_roots: Vec<VRef>,
    /// Permanent extra roots (mutable global cells like `derivation`,
    /// scopedImport scope cells referenced from leaked program constants).
    pub perm_roots: Vec<VRef>,
    pub symbols: SymbolTable,
    /// Lazily-filled cache of immortal String Values, one per symbol id
    /// (indexed by `Symbol.0`). Attribute-name builtins (`attrNames`,
    /// `mapAttrs`, `zipAttrsWith`, …) hand the user a string Value carrying
    /// the attribute's name; that name is a constant, context-free string, so
    /// rather than re-`resolve` + copy + GC-allocate one per attribute per
    /// call we intern one immortal String Value per distinct symbol. The
    /// immortal heap is ignored by the collector (no rooting / write-barrier),
    /// attribute names never carry context, and Nix has no observable string
    /// identity, so sharing the same cell everywhere is sound. See
    /// [`VM::symbol_string`].
    pub sym_string_cache: Vec<Option<VRef>>,
    /// Reusable scratch buffer for attrset builtins that assemble an output
    /// `Vec<Attr>` (e.g. `mapAttrs`, `zipAttrsWith`) before handing it to
    /// `new_bindings_value`. Taken via `mem::take` for the duration of a call
    /// (so reentrancy just allocates a fresh buffer) and returned afterwards,
    /// amortizing the allocation across calls. Never a GC root: the value
    /// cells it references are separately rooted in `temp_roots`.
    pub scratch_attrs: Vec<Attr>,
    pub positions: PosTable,
    pub syms: SpecialSyms,
    pub errors: Vec<EvalError>,
    pub globals: FxHashMap<Symbol, VRef>,
    pub file_cache: Vec<(std::path::PathBuf, VRef)>,
    /// Index into `file_cache` keyed by resolved path (mirrors C++
    /// `fileEvalCache` being a map). The Vec is retained for GC rooting.
    pub file_cache_idx: FxHashMap<std::path::PathBuf, usize>,
    /// Port of C++ `EvalState::srcToStore` — memoize source-path -> store-path
    /// coercions (keyed by the source path bytes) so repeated `copyPathToStore`
    /// of the same path skips re-hashing the file/tree.
    pub src_to_store: FxHashMap<Vec<u8>, (Vec<u8>, u32)>,
    /// Memoize `filterSource` / `builtins.path` dumps for the refs-empty case,
    /// keyed by (canonical real path, method, name). Each bucket holds one
    /// entry per distinct filter argument: `(filter, nar-hash, store-path)`.
    /// Filters are compared by conservative *structural* identity (see
    /// `filter_ident_eq` in builtins.rs) — never by code pointer alone — so two
    /// closures with the same code but different captures never alias. Filter
    /// closures are kept alive via `perm_roots`, so the stored `VRef`s (and the
    /// upvalue cells reachable from them) remain valid for later comparison.
    pub filtered_path_cache: FxHashMap<
        (Vec<u8>, u8, String),
        Vec<(Option<VRef>, jinx_store::hash::Hash, jinx_store::store_path::StorePath)>,
    >,
    /// Memoize `resolveExprPath` results (mirrors C++ `importResolutionCache`):
    /// symlink-following and directory `default.nix` resolution is a pure
    /// function of the (fixed) NIX_PATH config and the input path.
    pub import_resolution_cache: FxHashMap<std::path::PathBuf, std::path::PathBuf>,
    /// Memoize `builtins.readDir` listings keyed by the resolved real path
    /// (mirrors C++ caching directory reads). Values are `(name, type)` pairs —
    /// pure filesystem data, no GC pointers. Invalidation-free: a batch
    /// evaluator sees a stable filesystem.
    pub read_dir_cache: FxHashMap<String, Vec<(Vec<u8>, &'static str)>>,
    pub call_depth: usize,
    pub max_call_depth: usize,
    /// (prefix, path) entries, from -I and NIX_PATH.
    pub search_path: Vec<(Vec<u8>, Vec<u8>)>,
    pub true_cell: VRef,
    pub false_cell: VRef,
    pub null_cell: VRef,
    pub empty_list_cell: VRef,
    pub current_system: Vec<u8>,
    pub store_dir: Vec<u8>,
    /// Which store backend evaluation-time store effects go to. Defaults to
    /// [`StoreMode::Dummy`] (read-only path computation only); the CLI selects
    /// [`StoreMode::Daemon`] like C++ `openStore` (see `main.rs`).
    pub store_mode: StoreMode,
    /// Lazily-opened worker-protocol connection (only when `store_mode` is
    /// [`StoreMode::Daemon`]). `None` until the first store effect needs it.
    pub daemon_conn: Option<Box<jinx_store::daemon::DaemonStore>>,
    /// Set once a daemon connection attempt has failed, so we don't retry every
    /// store effect.
    pub daemon_failed: bool,
    pub pure_eval: bool,
    /// String context element table: `ctx_elems[id]` is the wire encoding of
    /// a `NixStringContextElem` (e.g. `<basename>`, `=<drv>`, `!<out>!<drv>`).
    pub ctx_elems: Vec<Vec<u8>>,
    /// Dedup map for [`VM::intern_ctx`].
    pub ctx_intern: FxHashMap<Vec<u8>, u32>,
    /// Enabled experimental features (from nix.conf / --extra-experimental-features).
    pub experimental: crate::context::ExperimentalFeatures,
    /// Compiled-regex cache (`regexCache` in C++), keyed on the pattern bytes.
    pub regex_cache: FxHashMap<Vec<u8>, std::rc::Rc<crate::regex::Regex>>,
    /// hashDerivationModulo memo (`drvHashes` in C++).
    pub drv_hashes: jinx_store::derivation::DrvHashes,
    /// Derivations produced by `derivationStrict` this run, so that later
    /// derivations depending on them can be resolved (stands in for reading
    /// `.drv` files from a store).
    pub built_drvs:
        FxHashMap<jinx_store::store_path::StorePath, jinx_store::derivation::Derivation>,
    /// Synthetic apply chunks for lazy applications (set at registration).
    pub apply_prog: Option<&'static crate::chunk::Program>,
    /// Definition position of the most recently selected attribute (C++
    /// ExprSelect `pos2`), used by `SelectForce` for the final force and its
    /// "while evaluating the attribute" frame.
    pub last_select_pos: PosIdx,
    /// Whether the error renderer prints full traces (`--show-trace`).
    pub show_trace: bool,
    /// `builtins.traceVerbose` only prints when `--trace-verbose` is set.
    pub trace_verbose: bool,
    /// `--abort-on-warn` / `NIX_ABORT_ON_WARN`: `builtins.warn` throws after
    /// emitting the warning, to reveal the stack trace.
    pub abort_on_warn: bool,
    /// Position the current thunk was forced with. C++ `forceValue(v, pos)`
    /// threads `pos` into `callFunction` for `tApp` values; jinx's synthetic
    /// apply thunks (no call pos of their own) recover it from here.
    pub force_pos: PosIdx,
    /// Virtual-store redirects: maps a computed store-path prefix (e.g.
    /// `/nix/store/<hash>-source`) to the real on-disk directory the flake
    /// tree was fetched from. Because jinx computes store paths read-only
    /// (never writing to the store), filesystem access under a redirected
    /// prefix is served from the real directory instead. This stands in for
    /// C++ Nix's lazy trees / a realised store path.
    pub store_redirects: Vec<(Vec<u8>, std::path::PathBuf)>,
    /// Cached compiled `call-flake.nix` lambda (flake bootstrap).
    pub call_flake_fn: Option<VRef>,
    /// Cached `fetchFinalTree` internal primop cell.
    pub fetch_tree_final_fn: Option<VRef>,
    /// Cranelift JIT backend (installed by the CLI when `--jit=on`). `None`
    /// disables tiering entirely (pure interpreter).
    pub jit: Option<Box<dyn crate::jit::JitHook>>,
    /// Background compile queue (perf-jit experiment): when set, hot chunks
    /// are sent to a worker thread (as `&'static CodeRef` addresses) instead
    /// of being compiled synchronously on the eval thread.
    pub jit_bg: Option<std::sync::mpsc::Sender<usize>>,
    /// Invocation count at which a chunk is handed to the JIT (0 = every
    /// chunk on first run; overridable via `JINX_JIT_THRESHOLD`).
    pub jit_threshold: u32,
}

/// RAII guard for `temp_roots`.
pub struct TempScope(usize);

impl VM {
    pub fn new(mut symbols: SymbolTable, positions: PosTable) -> Self {
        let syms = SpecialSyms::new(&mut symbols);
        VM {
            heap: Heap::new(),
            stack: crate::stack::Stack::with_capacity(1024),
            frames: Vec::with_capacity(64),
            temp_roots: Vec::new(),
            perm_roots: Vec::new(),
            symbols,
            sym_string_cache: Vec::new(),
            scratch_attrs: Vec::new(),
            positions,
            syms,
            errors: Vec::new(),
            globals: FxHashMap::default(),
            file_cache: Vec::new(),
            file_cache_idx: FxHashMap::default(),
            src_to_store: FxHashMap::default(),
            filtered_path_cache: FxHashMap::default(),
            import_resolution_cache: FxHashMap::default(),
            read_dir_cache: FxHashMap::default(),
            call_depth: 0,
            max_call_depth: 10000,
            last_select_pos: NO_POS,
            show_trace: true,
            trace_verbose: false,
            abort_on_warn: false,
            force_pos: NO_POS,
            search_path: Vec::new(),
            true_cell: immortal::cell(Value::bool(true)),
            false_cell: immortal::cell(Value::bool(false)),
            null_cell: immortal::cell(Value::null()),
            empty_list_cell: immortal::cell(immortal::list(&[])),
            current_system: b"aarch64-darwin".to_vec(),
            store_dir: b"/nix/store".to_vec(),
            store_mode: StoreMode::Dummy,
            daemon_conn: None,
            daemon_failed: false,
            pure_eval: false,
            ctx_elems: Vec::new(),
            ctx_intern: FxHashMap::default(),
            experimental: crate::context::ExperimentalFeatures::default(),
            regex_cache: FxHashMap::default(),
            drv_hashes: jinx_store::derivation::DrvHashes::default(),
            built_drvs: FxHashMap::default(),
            apply_prog: None,
            store_redirects: Vec::new(),
            call_flake_fn: None,
            fetch_tree_final_fn: None,
            jit: None,
            jit_bg: None,
            // Only hand hot chunks to the JIT: a higher trip count avoids
            // compiling chunks that never dominate runtime, cutting Cranelift
            // compile overhead. Measured faster on real nixpkgs evals
            // (firefox ~6%, hello ~2%) while fib stays >1.5x vs jit-off.
            jit_threshold: 4000,
        }
    }

    /// Register a virtual-store redirect: filesystem access under `store_prefix`
    /// (an absolute store path) is served from `real_dir`.
    pub fn add_store_redirect(&mut self, store_prefix: Vec<u8>, real_dir: std::path::PathBuf) {
        self.store_redirects.push((store_prefix, real_dir));
    }

    /// Translate a logical path to a real on-disk path, applying any registered
    /// store redirect whose prefix matches. Returns the input unchanged when no
    /// redirect applies. Port of the effect of a realised/lazy store tree.
    pub fn redirect_fs(&self, logical: &std::path::Path) -> std::path::PathBuf {
        if self.store_redirects.is_empty() {
            return logical.to_path_buf();
        }
        let bytes = logical.to_string_lossy();
        let bytes = bytes.as_bytes();
        for (prefix, real) in &self.store_redirects {
            if bytes.len() >= prefix.len() && &bytes[..prefix.len()] == prefix.as_slice() {
                let rest = &bytes[prefix.len()..];
                // Only redirect exact matches or `<prefix>/...` sub-paths.
                if rest.is_empty() {
                    return real.clone();
                }
                if rest[0] == b'/' {
                    let mut out = real.clone();
                    out.push(std::path::Path::new(
                        std::str::from_utf8(&rest[1..]).unwrap_or(""),
                    ));
                    return out;
                }
            }
        }
        logical.to_path_buf()
    }

    // ---------------- GC ----------------

    #[inline]
    pub fn gc_check(&mut self) {
        if self.heap.should_gc() {
            self.gc();
        }
    }

    /// Overwrite a value cell WITH the generational write barrier. Every
    /// mutation of a possibly-old cell must go through this (thunk
    /// force/blackhole updates, rec backpatches, `__overrides`), otherwise a
    /// minor collection can miss the only old->young edge and free a live
    /// young object.
    #[inline]
    pub fn set_b(&mut self, c: VRef, v: Value) {
        self.heap.write_barrier(c);
        set(c, v);
    }

    fn gc(&mut self) {
        let VM {
            heap,
            stack,
            frames,
            temp_roots,
            perm_roots,
            file_cache,
            ..
        } = self;
        heap.collect_auto(
            |m| {
                for &c in stack.iter() {
                    m.mark_cell(c);
                }
                for f in frames.iter() {
                    m.mark_value(&f.data);
                    for &c in &f.with_local {
                        m.mark_cell(c);
                    }
                }
                for &c in temp_roots.iter() {
                    m.mark_cell(c);
                }
                for &c in perm_roots.iter() {
                    m.mark_cell(c);
                }
                for (_, c) in file_cache.iter() {
                    m.mark_cell(*c);
                }
            },
            true,
        );
    }

    pub fn temp_scope(&mut self) -> TempScope {
        TempScope(self.temp_roots.len())
    }

    pub fn temp_end(&mut self, s: TempScope) {
        self.temp_roots.truncate(s.0);
    }

    /// Return an immortal String Value cell holding the name of `sym`, with no
    /// string context. The cell is created on first use and cached by symbol
    /// id. Because the cell (and its payload) live on the immortal heap they
    /// are ignored by the collector — no rooting, no write-barrier, no GC risk
    /// — and stay valid for the lifetime of the VM. Callers may drop the
    /// returned `VRef` straight into a list/bindings without a temp root.
    pub fn symbol_string(&mut self, sym: Symbol) -> VRef {
        let idx = sym.0 as usize;
        if idx >= self.sym_string_cache.len() {
            self.sym_string_cache.resize(idx + 1, None);
        }
        if let Some(c) = self.sym_string_cache[idx] {
            return c;
        }
        // `resolve` borrow ends once the bytes are copied into the immortal
        // string, before we touch the cache again.
        let cell = immortal::cell(immortal::string(self.symbols.resolve(sym)));
        self.sym_string_cache[idx] = Some(cell);
        cell
    }

    // ---------------- allocation wrappers ----------------

    pub fn alloc_cell(&mut self, v: Value) -> VRef {
        self.gc_check();
        self.heap.alloc_value(v)
    }

    pub fn new_string_value(&mut self, bytes: &[u8], ctx: *mut u64) -> Value {
        self.gc_check();
        self.heap.new_string(bytes, ctx)
    }

    pub fn new_path_value(&mut self, bytes: &[u8]) -> Value {
        self.gc_check();
        self.heap.new_path(0, bytes)
    }

    pub fn new_list_value(&mut self, items: &[VRef]) -> Value {
        self.gc_check();
        self.heap.new_list(items)
    }

    pub fn new_bindings_value(&mut self, entries: &[Attr]) -> Value {
        self.gc_check();
        self.heap.new_bindings(entries)
    }

    pub fn bool_cell(&self, b: bool) -> VRef {
        if b {
            self.true_cell
        } else {
            self.false_cell
        }
    }

    // ---------------- errors ----------------

    pub fn new_err(&mut self, kind: ErrKind, msg: impl Into<Vec<u8>>, pos: PosIdx) -> ErrId {
        self.errors.push(EvalError::new(kind, msg, pos));
        (self.errors.len() - 1) as ErrId
    }

    /// Port of the `if (fn->addTrace) addErrorTrace(e, pos, "while calling
    /// the '%1%' builtin", fn->name)` in `EvalState::callFunction`. Every
    /// builtin adds this frame except `addErrorContext` (whose own frame is
    /// redundant with the error context it injects).
    /// Port of `Value::determinePos` for the value kinds that carry a
    /// position: attribute sets and lambdas. Everything else falls back.
    pub fn determine_pos(&self, v: &Value, fallback: PosIdx) -> PosIdx {
        match v.tag() {
            // Runtime attribute sets don't retain their definition position in
            // jinx's heap layout, so only the lambda case is handled here.
            Tag::Closure => {
                let (code, _) = thunk_code(v);
                code.chunk().pos
            }
            // A blackhole retains its thunk pointer (w1); its chunk carries the
            // position of the expression under evaluation. Bare sentinels
            // (w1 == 0) have no position and fall back.
            Tag::Blackhole if v.w1 != 0 => {
                let (code, _) = thunk_code(v);
                code.chunk().pos
            }
            // SAFETY: Blackhole0's w1 is always an immortal CodeRef.
            Tag::Blackhole0 => unsafe { (*(v.w1 as *const CodeRef)).chunk().pos },
            _ => fallback,
        }
    }

    pub fn add_primop_trace(&mut self, e: ErrId, def: &'static PrimOpDef, pos: PosIdx) {
        if def.name == "__addErrorContext" {
            return;
        }
        self.add_trace(
            e,
            pos,
            format!("while calling the '{}' builtin", def.display()),
        );
    }

    /// C++ skips the "while evaluating the attribute" frame when the attr's
    /// position lives in the internal `derivation-internal.nix`. jinx compiles
    /// that wrapper from an in-memory source; detect it by its origin name.
    pub fn pos_is_derivation_internal(&self, p: PosIdx) -> bool {
        matches!(
            self.positions.origin_of(p),
            Some(o) if o.display_name() == crate::builtins::DERIVATION_INTERNAL_ORIGIN
        )
    }

    pub fn add_trace(&mut self, e: ErrId, pos: PosIdx, text: impl Into<String>) {
        self.errors[e as usize].traces.push(Trace {
            pos,
            text: text.into(),
            always: false,
        });
    }

    /// Like [`add_trace`] but marks the frame as `TracePrint::Always`
    /// (`builtins.addErrorContext`): shown even when traces are truncated.
    pub fn add_trace_always(&mut self, e: ErrId, pos: PosIdx, text: impl Into<String>) {
        self.errors[e as usize].traces.push(Trace {
            pos,
            text: text.into(),
            always: true,
        });
    }

    pub fn err_kind(&self, e: ErrId) -> ErrKind {
        self.errors[e as usize].kind
    }

    /// "an integer" / "a set" / ... (showType with article).
    pub fn show_type(&self, v: &Value) -> String {
        match v.tag() {
            Tag::Null => "null".into(),
            Tag::False | Tag::True => "a Boolean".into(),
            Tag::Int => "an integer".into(),
            Tag::Float => "a float".into(),
            Tag::String => {
                if str_ctx(v).is_null() {
                    "a string".into()
                } else {
                    "a string with context".into()
                }
            }
            Tag::Path => "a path".into(),
            Tag::Attrs => "a set".into(),
            Tag::List => "a list".into(),
            Tag::Closure => "a function".into(),
            Tag::PrimOp => format!("the built-in function '{}'", primop_of(v).display()),
            Tag::PrimOpApp => format!(
                "the partially applied built-in function '{}'",
                primapp_parts(v).0.display()
            ),
            Tag::Thunk | Tag::Thunk0 | Tag::Blackhole | Tag::Blackhole0 => "a thunk".into(),
            Tag::Failed => "an error".into(),
        }
    }

    fn type_err(
        &mut self,
        v: &Value,
        expected: &str,
        pos: PosIdx,
        ctx: Option<&str>,
    ) -> ErrId {
        let printed = crate::print::print_value_err(self, v);
        let msg = format!(
            "expected {} but found {}: {}",
            expected,
            self.show_type(v),
            printed
        );
        let e = self.new_err(ErrKind::Type, msg, pos);
        if let Some(c) = ctx {
            self.add_trace(e, pos, c);
        }
        e
    }

    // ---------------- force ----------------

    pub fn force(&mut self, cell: VRef, pos: PosIdx) -> Result<(), ErrId> {
        loop {
            let v = val(cell);
            match v.tag() {
                Tag::Thunk | Tag::Thunk0 => {
                    // Retain the thunk's data pointer in the blackhole so that
                    // `determine_pos` can recover the position of the
                    // expression being computed (C++ `Value::determinePos`
                    // over the blackholed thunk). The GC traces Blackhole
                    // like Thunk (see `has_heap_payload`); a Thunk0 packs its
                    // immortal (untraced) CodeRef into w1, blackholed as
                    // Blackhole0.
                    let (code, elems): (&'static CodeRef, &'static [VRef]) =
                        if v.tag() == Tag::Thunk {
                            self.set_b(cell, Value::make(Tag::Blackhole, v.w1));
                            thunk_code(&v)
                        } else {
                            self.set_b(cell, Value::make(Tag::Blackhole0, v.w1));
                            // SAFETY: Thunk0's w1 is always an immortal CodeRef.
                            (unsafe { &*(v.w1 as *const CodeRef) }, &[])
                        };
                    use crate::chunk::ChunkKind;
                    let run = match code.chunk().kind {
                        // ---- frame-less trivial kinds (round-4) ----
                        ChunkKind::ConstReturn { idx } => {
                            Ok(val(code.prog().consts[idx as usize]))
                        }
                        ChunkKind::Forward { upval, pos: gpos } => {
                            let target = elems[upval as usize];
                            let tv = val(target);
                            if tv.tag() == Tag::Blackhole {
                                // Exact port of the framed behavior: the
                                // forwarding frame *is* the running frame, so
                                // a blackholed target matching our own thunk
                                // data is a direct self-reference (reported
                                // at the reference site `gpos`); anything
                                // else reports at the enclosing force
                                // position.
                                let bpos = if tv.w1 != 0 && tv.w1 == v.w1 {
                                    gpos
                                } else if pos.is_set() {
                                    pos
                                } else {
                                    gpos
                                };
                                Err(self.new_err(
                                    ErrKind::InfiniteRecursion,
                                    "infinite recursion encountered",
                                    bpos,
                                ))
                            } else {
                                self.force(target, gpos).map(|()| val(target))
                            }
                        }
                        ChunkKind::Straight => {
                            let saved_force_pos = self.force_pos;
                            self.force_pos = pos;
                            let r = self.force_straight(code, elems);
                            self.force_pos = saved_force_pos;
                            r
                        }
                        // ---- general chunks: full frame ----
                        ChunkKind::General => {
                            let saved_force_pos = self.force_pos;
                            self.force_pos = pos;
                            let run = self.run_code(code, v);
                            self.force_pos = saved_force_pos;
                            run
                        }
                    };
                    match run {
                        Ok(res) => {
                            self.set_b(cell, res);
                            // The chunk may itself return an (unforced)
                            // thunk value (e.g. a call result); keep going
                            // until WHNF, like C++ forceValue on tApp.
                            if matches!(res.tag(), Tag::Thunk | Tag::Thunk0) {
                                continue;
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            self.set_b(cell, Value::make(Tag::Failed, e as u64));
                            return Err(e);
                        }
                    }
                }
                Tag::Blackhole | Tag::Blackhole0 => {
                    // Re-forcing a blackhole is infinite recursion. The
                    // position C++ reports is the reference site that closes
                    // the cycle:
                    //   * Direct self-reference (a thunk forces *itself*, e.g.
                    //     `a = {} // a`): the offending reference is the current
                    //     `pos`. We detect this by comparing the blackhole's
                    //     retained thunk pointer (w1) against the thunk of the
                    //     currently running frame — they're equal iff the
                    //     running thunk is re-forcing its own cell.
                    //   * Indirect cycle (x -> y -> x): the reference site lives
                    //     in the *enclosing* thunk, whose force position jinx
                    //     tracks as `force_pos`.
                    let running = self.frames.last().map(|f| f.data.w1).unwrap_or(0);
                    let bpos = if v.w1 != 0 && v.w1 == running {
                        pos
                    } else if self.force_pos != NO_POS {
                        self.force_pos
                    } else {
                        pos
                    };
                    return Err(self.new_err(
                        ErrKind::InfiniteRecursion,
                        "infinite recursion encountered",
                        bpos,
                    ));
                }
                Tag::Failed => {
                    // Errors are memoised on the thunk, but re-forcing a failed
                    // value yields a *fresh* copy so that later `addTrace`
                    // (e.g. from `addErrorContext`) does not mutate the cached
                    // error (see eval-fail-memoised-error-trace-not-mutated).
                    let orig = v.w1 as ErrId;
                    let copy = self.errors[orig as usize].clone();
                    self.errors.push(copy);
                    return Err((self.errors.len() - 1) as ErrId);
                }
                _ => return Ok(()),
            }
        }
    }

    pub fn force_bool(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<bool, ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        match v.tag() {
            Tag::True => Ok(true),
            Tag::False => Ok(false),
            _ => Err(self.type_err(&v, "a Boolean", pos, Some(ctx))),
        }
    }

    pub fn force_attrs(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<(), ErrId> {
        // C++ forceAttrs (inline) does NOT wrap forceValue in a try/catch: the
        // `ctx` frame is attached via `.withTrace(pos, ctx)` ONLY on the type
        // mismatch. When forcing the value itself throws, that error propagates
        // unwrapped (unlike forceInt/forceString, which do add ctx on any error).
        self.force(cell, pos)?;
        let v = val(cell);
        if v.tag() != Tag::Attrs {
            // C++ forceAttrs uses `.withTrace(pos, ctx)` only — no position on
            // the base error itself.
            let e = self.type_err(&v, "a set", NO_POS, None);
            self.add_trace(e, pos, ctx);
            return Err(e);
        }
        Ok(())
    }

    /// Port of `EvalState::evalAttrs` (eval.cc): like [`force_attrs`] but the
    /// error context wraps the *whole evaluation* in a try/catch, so it is
    /// attached on ANY error (a throw / infinite recursion), not only a type
    /// mismatch. Used for the `//` operator's operands (`evalForUpdate`).
    pub fn eval_attrs(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<(), ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        if v.tag() != Tag::Attrs {
            let e = self.type_err(&v, "a set", NO_POS, None);
            self.add_trace(e, pos, ctx);
            return Err(e);
        }
        Ok(())
    }

    pub fn force_list(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<(), ErrId> {
        // See force_attrs: `ctx` is added only on the type mismatch, not when
        // forcing the argument throws (C++ inline forceList).
        self.force(cell, pos)?;
        let v = val(cell);
        if v.tag() != Tag::List {
            // C++ forceList uses `.withTrace(pos, ctx)` only — no position on
            // the base error itself.
            let e = self.type_err(&v, "a list", NO_POS, None);
            self.add_trace(e, pos, ctx);
            return Err(e);
        }
        Ok(())
    }

    pub fn force_int(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<i64, ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        if v.tag() != Tag::Int {
            let e = self.type_err(&v, "an integer", pos, None);
            self.add_trace(e, pos, ctx);
            return Err(e);
        }
        Ok(v.as_int())
    }

    pub fn force_float(&mut self, cell: VRef, pos: PosIdx, ctx: &str) -> Result<f64, ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        match v.tag() {
            Tag::Int => Ok(v.as_int() as f64),
            Tag::Float => Ok(v.as_float()),
            _ => {
                let e = self.type_err(&v, "a float", pos, None);
                self.add_trace(e, pos, ctx);
                Err(e)
            }
        }
    }

    /// forceString: returns owned bytes (heap strings are stable, but
    /// copying keeps borrows simple).
    pub fn force_string(
        &mut self,
        cell: VRef,
        pos: PosIdx,
        ctx: &str,
    ) -> Result<Vec<u8>, ErrId> {
        self.force(cell, pos).map_err(|e| {
            self.add_trace(e, pos, ctx);
            e
        })?;
        let v = val(cell);
        if v.tag() != Tag::String {
            let e = self.type_err(&v, "a string", pos, None);
            self.add_trace(e, pos, ctx);
            return Err(e);
        }
        Ok(str_bytes(&v).to_vec())
    }

    pub fn force_string_no_ctx(
        &mut self,
        cell: VRef,
        pos: PosIdx,
        ctx: &str,
    ) -> Result<Vec<u8>, ErrId> {
        let s = self.force_string(cell, pos, ctx)?;
        let v = val(cell);
        if !str_ctx(&v).is_null() {
            let msg = format!(
                "the string '{}' is not allowed to refer to a store path",
                String::from_utf8_lossy(&s)
            );
            let e = self.new_err(ErrKind::Eval, msg, pos);
            self.add_trace(e, pos, ctx);
            return Err(e);
        }
        Ok(s)
    }

    // ---------------- running code ----------------

    /// Push a frame for `code` (with upvalue payload `data`), run it, pop.
    pub fn run_code(&mut self, code: &'static CodeRef, data: Value) -> Result<Value, ErrId> {
        let base = self.stack.len();
        self.frames.push(Frame {
            code,
            data,
            locals_base: base,
            with_local: Vec::new(),
        });
        let r = self.run_top_frame();
        self.frames.pop();
        let out = r.map(val);
        self.stack.truncate(base);
        out
    }

    /// Entry point: evaluate chunk 0 of a leaked program.
    pub fn run_program(&mut self, prog: &'static crate::chunk::Program) -> Result<VRef, ErrId> {
        let code = prog.code_ref(0);
        let v = self.run_code(code, Value::null())?;
        Ok(self.alloc_cell(v))
    }

    /// Decide whether frame `fi`'s chunk should run JIT-compiled code,
    /// compiling it on demand once its invocation counter passes the
    /// threshold. Returns the native entry point if the chunk is (now)
    /// compiled, else `None` (interpret).
    fn jit_dispatch(&mut self, code: &'static CodeRef) -> Option<crate::jit::JitEntry> {
        use std::sync::atomic::Ordering;
        let chunk = code.chunk();
        let e = chunk.jit.entry.load(Ordering::Acquire);
        if !(e == crate::chunk::JIT_NONE
            || e == crate::chunk::JIT_UNCOMPILABLE
            || e == crate::chunk::JIT_QUEUED)
        {
            // SAFETY: only real entry pointers are stored (not the sentinels).
            return Some(unsafe { std::mem::transmute::<*mut (), crate::jit::JitEntry>(e) });
        }
        if e == crate::chunk::JIT_UNCOMPILABLE || e == crate::chunk::JIT_QUEUED {
            return None;
        }
        let n = chunk.jit.counter.get().wrapping_add(1);
        chunk.jit.counter.set(n);
        if (n as u64) <= self.jit_threshold as u64 {
            return None;
        }
        // Background tier: enqueue and keep interpreting until the compiled
        // entry is published by the worker.
        if let Some(tx) = &self.jit_bg {
            chunk.jit.entry.store(crate::chunk::JIT_QUEUED, Ordering::Relaxed);
            let _ = tx.send(code as *const CodeRef as usize);
            return None;
        }
        // Take the hook out to satisfy the borrow checker (it borrows nothing
        // from `self`), compile, then restore it.
        let mut hook = self.jit.take();
        let res = hook.as_mut().and_then(|h| h.compile(code));
        self.jit = hook;
        match res {
            Some(entry) => {
                chunk.jit.entry.store(entry as *mut (), Ordering::Release);
                // SAFETY: the backend returns a valid entry pointer.
                Some(unsafe { std::mem::transmute::<*const (), crate::jit::JitEntry>(entry) })
            }
            None => {
                chunk
                    .jit
                    .entry
                    .store(crate::chunk::JIT_UNCOMPILABLE, Ordering::Release);
                None
            }
        }
    }

    /// Invoke a compiled chunk entry for frame `fi` and decode its status word.
    /// Performs the frame setup the compiled prologue used to do via helper
    /// calls: reserve operand-stack capacity for all inline pushes, and pass
    /// the stack address / locals base / upvalue pointer as arguments.
    #[inline]
    fn run_jit(
        &mut self,
        entry: crate::jit::JitEntry,
        fi: usize,
        chunk: &'static Chunk,
    ) -> Result<VRef, ErrId> {
        let base = self.frames[fi].locals_base;
        let upv = self.frames[fi].upvals().as_ptr() as u64;
        self.stack.reserve_to(base + chunk.max_height as usize);
        let sa = (&mut self.stack) as *mut crate::stack::Stack;
        let r = entry(self as *mut VM, fi as u64, sa, base as u64, upv);
        if r & crate::jit::ERR_FLAG != 0 {
            Err((r & 0xffff_ffff) as ErrId)
        } else {
            // SAFETY: success returns the result cell pointer (non-null).
            Ok(unsafe { NonNull::new_unchecked(r as *mut Value) })
        }
    }

    fn run_top_frame(&mut self) -> Result<VRef, ErrId> {
        let fi = self.frames.len() - 1;
        let chunk: &'static Chunk = self.frames[fi].code.chunk();
        if self.jit.is_some() {
            let code = self.frames[fi].code;
            if let Some(entry) = self.jit_dispatch(code) {
                return self.run_jit(entry, fi, chunk);
            }
        }
        // Frame constants, hoisted out of the dispatch loop: the program is
        // leaked ('static), `locals_base` never changes for a live frame, and
        // the upvalue array is a data object rooted by the frame for its whole
        // lifetime. Nested calls push/pop frames *above* `fi` only.
        let prog: &'static crate::chunk::Program = self.frames[fi].code.prog();
        let base = self.frames[fi].locals_base;
        let upvals: &'static [VRef] = self.frames[fi].upvals();
        let mut ip = 0usize;
        macro_rules! pos {
            () => {
                chunk.pos_at(ip)
            };
        }
        loop {
            // No per-op gc_check: every allocation site (alloc_cell,
            // make_thunk, MakeList/MakeAttrs/ConcatLists, new_*_value
            // wrappers, op_update) performs its own check, which is where
            // C++/Boehm polls too. A missed poll only delays collection to
            // the next allocating op.
            let op = chunk.ops[ip];
            match op {
                Op::Const(i) => {
                    let c = prog.consts[i as usize];
                    self.stack.push(c);
                }
                Op::GetLocal(s) => {
                    let c = self.stack[base + s as usize];
                    self.stack.push(c);
                }
                Op::GetUpval(i) => {
                    let c = upvals[i as usize];
                    self.stack.push(c);
                }
                Op::ResolveWith(sym) => {
                    let c = self.resolve_with(fi, Symbol(sym), pos!())?;
                    self.stack.push(c);
                }
                Op::Force => {
                    let c = *self.stack.last().unwrap();
                    // Fast path: only thunk-like cells need the (out-of-line)
                    // force machinery; an already-WHNF value returns immediately.
                    // C++ `forceValue` inlines the `!isThunk` early-out too.
                    if val(c).needs_force() {
                        self.force(c, pos!())?;
                    }
                }
                Op::ForceBool(ctx) => {
                    let c = *self.stack.last().unwrap();
                    self.force_bool(c, pos!(), CTX_STRINGS[ctx as usize])?;
                }
                Op::ForceAttrs(ctx) => {
                    let c = *self.stack.last().unwrap();
                    // The `//` operator's operands (ctx 9/10) use evalAttrs
                    // semantics — the operand's error context is added on ANY
                    // error. Every other ForceAttrs site (`with`, selection,
                    // lambda arg) uses forceAttrs (type-mismatch only).
                    if ctx == 9 || ctx == 10 {
                        self.eval_attrs(c, pos!(), CTX_STRINGS[ctx as usize])?;
                    } else {
                        self.force_attrs(c, pos!(), CTX_STRINGS[ctx as usize])?;
                    }
                }
                Op::ForceList(ctx) => {
                    let c = *self.stack.last().unwrap();
                    self.force_list(c, pos!(), CTX_STRINGS[ctx as usize])?;
                }
                Op::Pop => {
                    self.stack.pop();
                }
                Op::AllocCell => {
                    let c = self.alloc_cell(Value::make(Tag::Blackhole, 0));
                    self.stack.push(c);
                }
                Op::StoreLocal(s) => {
                    let c = self.stack.pop().unwrap();
                    let dst = self.stack[base + s as usize];
                    self.set_b(dst, val(c));
                }
                Op::MakeThunk(cid) => {
                    let c = self.make_thunk(fi, cid, Tag::Thunk);
                    self.stack.push(c);
                }
                Op::MakeClosure(cid) => {
                    let c = self.make_thunk(fi, cid, Tag::Closure);
                    self.stack.push(c);
                }
                Op::MakeList(n) => {
                    let n = n as usize;
                    let start = self.stack.len() - n;
                    self.gc_check();
                    let v = self.heap.new_list(&self.stack[start..]);
                    let c = self.heap.alloc_value(v);
                    self.stack.truncate(start);
                    self.stack.push(c);
                }
                Op::MakeAttrs(d) => {
                    // gc_check BEFORE building: the entry cells are still
                    // rooted on the operand stack while we fill the object.
                    self.gc_check();
                    let desc = &prog.attrs_descs[d as usize];
                    let n = desc.names.len();
                    let start = self.stack.len() - n;
                    let (v, out) = self.heap.new_bindings_raw(n);
                    // SAFETY: `out` has n slots; desc.names and the popped
                    // stack range both have exactly n items.
                    unsafe {
                        for (k, (&(sym, pos), &cell)) in
                            desc.names.iter().zip(&self.stack[start..]).enumerate()
                        {
                            out.add(k).write(Attr {
                                sym: sym.0,
                                pos: pos.0,
                                val: cell,
                            });
                        }
                    }
                    let c = self.heap.alloc_value(v);
                    self.stack.truncate(start);
                    self.stack.push(c);
                }
                Op::DynAttr => {
                    self.op_dyn_attr(pos!())?;
                }
                Op::RecOverrides(rd) => {
                    self.op_rec_overrides(fi, rd, pos!())?;
                }
                Op::Jump(t) => {
                    ip = t as usize;
                    continue;
                }
                Op::JumpIfFalse(t) => {
                    let c = self.stack.pop().unwrap();
                    if val(c).tag() == Tag::False {
                        ip = t as usize;
                        continue;
                    }
                }
                Op::JumpIfTrue(t) => {
                    let c = self.stack.pop().unwrap();
                    if val(c).tag() == Tag::True {
                        ip = t as usize;
                        continue;
                    }
                }
                Op::Not => {
                    let c = self.stack.pop().unwrap();
                    let b = val(c).tag() == Tag::True;
                    self.stack.push(self.bool_cell(!b));
                }
                Op::Eq | Op::NEq => {
                    let b = self.stack.pop().unwrap();
                    let a = self.stack.pop().unwrap();
                    let ctx = if matches!(op, Op::Eq) {
                        "while testing two values for equality"
                    } else {
                        "while testing two values for inequality"
                    };
                    let r = self.eq_values(a, b, pos!(), ctx, true)?;
                    let r = if matches!(op, Op::Eq) { r } else { !r };
                    self.stack.push(self.bool_cell(r));
                }
                Op::Update => {
                    self.op_update()?;
                }
                Op::ConcatLists => {
                    let b = self.stack.pop().unwrap();
                    let a = self.stack.pop().unwrap();
                    let (va, vb) = (val(a), val(b));
                    let (ea, eb) = (list_elems(&va), list_elems(&vb));
                    let v = if ea.is_empty() && !eb.is_empty() {
                        vb
                    } else if eb.is_empty() {
                        va
                    } else {
                        // a and b stay reachable from native locals;
                        // elements are reachable through them. gc_check runs
                        // inside before the object is carved.
                        self.gc_check();
                        self.heap.new_list_concat(ea, eb)
                    };
                    let c = self.alloc_cell(v);
                    self.stack.push(c);
                }
                Op::ConcatStrings(d) => {
                    self.op_concat_strings(fi, d)?;
                }
                Op::Select { sym, cache } => {
                    self.op_select(Symbol(sym), cache, prog, pos!())?;
                }
                Op::SelectForce(t) => {
                    let c = *self.stack.last().unwrap();
                    let p = self.last_select_pos;
                    self.force(c, p).map_err(|e| {
                        if p.is_set() && !self.pos_is_derivation_internal(p) {
                            let text =
                                prog.texts[t as usize].clone();
                            self.add_trace(
                                e,
                                p,
                                format!(
                                    "while evaluating the attribute '{}'",
                                    String::from_utf8_lossy(&text)
                                ),
                            );
                        }
                        e
                    })?;
                }
                Op::SelectOr { sym, target } => {
                    let c = *self.stack.last().unwrap();
                    self.force(c, pos!())?;
                    let v = val(c);
                    let found = if v.tag() == Tag::Attrs {
                        attrs_get(&v, Symbol(sym))
                    } else {
                        None
                    };
                    match found {
                        Some(a) => {
                            self.last_select_pos = PosIdx(a.pos);
                            *self.stack.last_mut().unwrap() = a.val
                        }
                        None => {
                            self.stack.pop();
                            ip = target as usize;
                            continue;
                        }
                    }
                }
                Op::SelectDyn(t) => {
                    // Position of the last successfully selected attribute
                    // (C++ `pos2`); the "while evaluating the attribute" frame
                    // is attached here on any navigation error, before this op
                    // updates it on success.
                    let sp = self.last_select_pos;
                    let dyn_pos = pos!();
                    let name = self.stack.pop().unwrap();
                    let step = |vm: &mut Self| -> Result<(), ErrId> {
                        let nb = vm.force_string_no_ctx(
                            name,
                            dyn_pos,
                            "while evaluating an attribute name",
                        )?;
                        let sym = vm.symbols.create(&nb);
                        let c = *vm.stack.last().unwrap();
                        vm.force_attrs(c, dyn_pos, "while selecting an attribute")?;
                        let v = val(c);
                        match attrs_get(&v, sym) {
                            Some(a) => {
                                vm.last_select_pos = PosIdx(a.pos);
                                *vm.stack.last_mut().unwrap() = a.val;
                                Ok(())
                            }
                            None => Err(vm.missing_attr_err(&v, sym, dyn_pos)),
                        }
                    };
                    if let Err(e) = step(self) {
                        if t != u32::MAX && sp.is_set() && !self.pos_is_derivation_internal(sp) {
                            let text =
                                prog.texts[t as usize].clone();
                            self.add_trace(
                                e,
                                sp,
                                format!(
                                    "while evaluating the attribute '{}'",
                                    String::from_utf8_lossy(&text)
                                ),
                            );
                        }
                        return Err(e);
                    }
                }
                Op::SelectDynOr { target } => {
                    let name = self.stack.pop().unwrap();
                    let nb = self.force_string_no_ctx(
                        name,
                        pos!(),
                        "while evaluating an attribute name",
                    )?;
                    let sym = self.symbols.create(&nb);
                    let c = *self.stack.last().unwrap();
                    self.force(c, pos!())?;
                    let v = val(c);
                    let found = if v.tag() == Tag::Attrs {
                        attrs_get(&v, sym)
                    } else {
                        None
                    };
                    match found {
                        Some(a) => {
                            self.last_select_pos = PosIdx(a.pos);
                            *self.stack.last_mut().unwrap() = a.val
                        }
                        None => {
                            self.stack.pop();
                            ip = target as usize;
                            continue;
                        }
                    }
                }
                Op::HasAttrPath(d) => {
                    self.op_has_attr_path(fi, d, pos!())?;
                }
                Op::Call(n) => {
                    let n = n as usize;
                    let args_start = self.stack.len() - n;
                    let fun = self.stack[args_start - 1];
                    // Copy args out of the (reallocatable) operand stack into
                    // an inline buffer (the common case is 1-2 curried args),
                    // avoiding a heap Vec allocation per call. The values stay
                    // GC-rooted via the operand stack (truncate is after call).
                    let mut buf = [std::mem::MaybeUninit::<VRef>::uninit(); 8];
                    let mut heap_args: Vec<VRef> = Vec::new();
                    let args: &[VRef] = if n <= 8 {
                        // SAFETY: we copy exactly n initialized cells.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                self.stack[args_start..].as_ptr(),
                                buf.as_mut_ptr() as *mut VRef,
                                n,
                            );
                            std::slice::from_raw_parts(buf.as_ptr() as *const VRef, n)
                        }
                    } else {
                        heap_args.extend_from_slice(&self.stack[args_start..]);
                        &heap_args
                    };
                    // Synthetic apply thunks (map/genList/…) carry no call
                    // pos; C++ threads the enclosing `forceValue` pos into the
                    // `tApp` call. Recover it from `force_pos`, else fall back
                    // to the callee's `determinePos` (as `forceValueDeep`
                    // forces at the callee position with no explicit pos).
                    let mut cpos = pos!();
                    if !cpos.is_set() {
                        cpos = self.force_pos;
                        if !cpos.is_set() {
                            cpos = self.determine_pos(&val(fun), NO_POS);
                        }
                    }
                    let v = self.call_function(fun, args, cpos)?;
                    self.stack.truncate(args_start - 1);
                    let c = self.alloc_cell(v);
                    self.stack.push(c);
                }
                Op::Ret => {
                    return Ok(self.stack.pop().unwrap());
                }
                Op::CurPos => {
                    let v = self.mk_pos(pos!());
                    let c = self.alloc_cell(v);
                    self.stack.push(c);
                }
                Op::AssertFail(t) => {
                    let text = &prog.texts[t as usize];
                    let msg = {
                        let mut m = b"assertion '".to_vec();
                        m.extend_from_slice(text);
                        m.extend_from_slice(b"' failed");
                        m
                    };
                    return Err(self.new_err(ErrKind::Assertion, msg, pos!()));
                }
                Op::AssertEq(t) => {
                    // Stack: [.., lhs, rhs]. Port of ExprAssert's ExprOpEq path.
                    let rhs = self.stack.pop().unwrap();
                    let lhs = self.stack.pop().unwrap();
                    let apos = pos!();
                    if let Err(e) =
                        self.assert_eq_values(lhs, rhs, NO_POS, "in an equality assertion")
                    {
                        let text =
                            prog.texts[t as usize].clone();
                        self.add_trace(
                            e,
                            apos,
                            format!(
                                "while evaluating the condition of the assertion '{}'",
                                String::from_utf8_lossy(&text)
                            ),
                        );
                        return Err(e);
                    }
                    // Values compared equal: fall through to AssertFail.
                }
                Op::PushWith => {
                    let c = self.stack.pop().unwrap();
                    self.frames[fi].with_local.push(c);
                }
                Op::PopWith => {
                    self.frames[fi].with_local.pop();
                }
                Op::Slide(n) => {
                    let top = self.stack.pop().unwrap();
                    let len = self.stack.len();
                    self.stack.truncate(len - n as usize);
                    self.stack.push(top);
                }
            }
            ip += 1;
        }
    }

    /// `Op::Select`: force TOS as attrs and replace it with attribute `sym`,
    /// using the per-site inline cache to skip the binary search when the same
    /// attrset object recurs. Shared by the interpreter and the JIT's
    /// `jinx_select` helper.
    pub(crate) fn op_select(
        &mut self,
        sym: Symbol,
        cache_idx: u32,
        prog: &'static crate::chunk::Program,
        pos: PosIdx,
    ) -> Result<(), ErrId> {
        let cell = *self.stack.last().unwrap();
        let r = self.select_value(cell, sym, cache_idx, prog, pos)?;
        *self.stack.last_mut().unwrap() = r;
        Ok(())
    }

    /// Cell-in/cell-out core of `Op::Select` (shared with the frame-less
    /// `force_straight` path).
    pub(crate) fn select_value(
        &mut self,
        cell: VRef,
        sym: Symbol,
        cache_idx: u32,
        prog: &'static crate::chunk::Program,
        pos: PosIdx,
    ) -> Result<VRef, ErrId> {
        self.force_attrs(cell, pos, "while selecting an attribute")?;
        let v = val(cell);
        let attrs_ptr = v.ptr() as *const u64;
        let es = attrs_entries(&v);
        let cache = &prog.select_caches[cache_idx as usize];
        let c = cache.get();
        // Cache hit: same attrset object and the cached slot still names `sym`
        // (re-checked so an address reused by the GC can't cause a mislookup).
        let found = if c.attrs == attrs_ptr
            && (c.slot as usize) < es.len()
            && es[c.slot as usize].sym == sym.0
        {
            Some(es[c.slot as usize])
        } else {
            match es.binary_search_by(|a| a.sym.cmp(&sym.0)) {
                Ok(i) => {
                    cache.set(crate::chunk::SelectCache {
                        attrs: attrs_ptr,
                        slot: i as u32,
                    });
                    Some(es[i])
                }
                Err(_) => None,
            }
        };
        match found {
            Some(a) => {
                self.last_select_pos = PosIdx(a.pos);
                Ok(a.val)
            }
            None => Err(self.missing_attr_err(&v, sym, pos)),
        }
    }

    pub(crate) fn missing_attr_err(&mut self, attrs: &Value, sym: Symbol, pos: PosIdx) -> ErrId {
        let name = self.symbols.resolve_str_lossy(sym);
        let cands: Vec<String> = attrs_entries(attrs)
            .iter()
            .map(|a| self.symbols.resolve_str_lossy(Symbol(a.sym)))
            .collect();
        let sugg = best_matches(cands.into_iter(), &name);
        let e = self.new_err(ErrKind::Eval, format!("attribute '{name}' missing"), pos);
        self.errors[e as usize].suggestions = sugg;
        e
    }

    // ---------------- ops with more logic ----------------

    pub(crate) fn op_dyn_attr(&mut self, pos: PosIdx) -> Result<(), ErrId> {
        let value = self.stack.pop().unwrap();
        let name = self.stack.pop().unwrap();
        self.force(name, pos)?;
        if val(name).tag() == Tag::Null {
            return Ok(());
        }
        let nb = self.force_string_no_ctx(
            name,
            pos,
            "while evaluating the name of a dynamic attribute",
        )?;
        let sym = self.symbols.create(&nb);
        let attrs_cell = *self.stack.last().unwrap();
        let av = val(attrs_cell);
        if let Some(existing) = attrs_get(&av, sym) {
            let at = self
                .positions
                .lookup(PosIdx(existing.pos))
                .map(|p| p.to_string())
                .unwrap_or_else(|| "«none»".into());
            let msg = format!(
                "dynamic attribute '{}' already defined at {}",
                String::from_utf8_lossy(&nb),
                at
            );
            return Err(self.new_err(ErrKind::Eval, msg, pos));
        }
        let mut entries: Vec<Attr> = attrs_entries(&av).to_vec();
        let idx = entries.partition_point(|a| a.sym < sym.0);
        entries.insert(
            idx,
            Attr {
                sym: sym.0,
                pos: pos.0,
                val: value,
            },
        );
        // `value` and old entries stay rooted via the operand stack (value
        // was popped but remains in a native local; conservative scan).
        let v = self.new_bindings_value(&entries);
        self.set_b(attrs_cell, v);
        Ok(())
    }

    pub(crate) fn op_rec_overrides(&mut self, fi: usize, rd: u32, _pos: PosIdx) -> Result<(), ErrId> {
        let prog = self.frames[fi].code.prog();
        let rdesc = &prog.rec_descs[rd as usize];
        let desc = &prog.attrs_descs[rdesc.attrs_desc as usize];
        let attrs_cell = *self.stack.last().unwrap();
        let av = val(attrs_cell);
        let ov_attr = attrs_entries(&av)[rdesc.overrides_idx as usize];
        // C++ forces at `vOverrides->determinePos(noPos)` computed on the
        // (unforced) value — noPos for a non-attrs/non-lambda thunk.
        let opos = self.determine_pos(&val(ov_attr.val), NO_POS);
        self.force_attrs(
            ov_attr.val,
            opos,
            "while evaluating the `__overrides` attribute",
        )?;
        let ov = val(ov_attr.val);
        let base = self.frames[fi].locals_base + rdesc.locals_start as usize;
        let mut entries: Vec<Attr> = attrs_entries(&av).to_vec();
        for o in attrs_entries(&ov) {
            if let Some(k) = desc.names.iter().position(|(s, _)| s.0 == o.sym) {
                // Overwrite the rec binding cell so references through the
                // rec scope see the override.
                let cell = self.stack[base + k];
                self.set_b(cell, val(o.val));
                entries[k] = Attr {
                    sym: o.sym,
                    pos: o.pos,
                    val: cell,
                };
            } else {
                let idx = entries.partition_point(|a| a.sym < o.sym);
                entries.insert(idx, *o);
            }
        }
        let v = self.new_bindings_value(&entries);
        self.set_b(attrs_cell, v);
        Ok(())
    }

    pub(crate) fn op_update(&mut self) -> Result<(), ErrId> {
        // gc_check BEFORE popping: left/right must still be rooted on the VM
        // stack when a collection can run, since the merge below reads their
        // heap entries after allocating the destination object.
        self.gc_check();
        let left = self.stack.pop().unwrap();
        let right = self.stack.pop().unwrap();
        let (lv, rv) = (val(left), val(right));
        let (le, re) = (attrs_entries(&lv), attrs_entries(&rv));
        let v = if le.is_empty() {
            rv
        } else if re.is_empty() {
            lv
        } else {
            self.heap.new_bindings_merge(le, re)
        };
        let c = self.alloc_cell(v);
        self.stack.push(c);
        Ok(())
    }

    pub(crate) fn op_has_attr_path(&mut self, fi: usize, d: u32, pos: PosIdx) -> Result<(), ErrId> {
        let desc = &self.frames[fi].code.prog().haspath_descs[d as usize];
        let ndyn = desc.comps.iter().filter(|c| c.is_none()).count();
        let dyn_start = self.stack.len() - ndyn;
        let subj = self.stack[dyn_start - 1];
        let mut dyn_idx = 0usize;
        let mut cur = subj;
        let mut result = true;
        for comp in &desc.comps {
            self.force(cur, pos)?;
            let sym = match comp {
                Some(s) => *s,
                None => {
                    let name_cell = self.stack[dyn_start + dyn_idx];
                    dyn_idx += 1;
                    let nb = self.force_string_no_ctx(
                        name_cell,
                        pos,
                        "while evaluating an attribute name",
                    )?;
                    self.symbols.create(&nb)
                }
            };
            let v = val(cur);
            match (v.tag() == Tag::Attrs).then(|| attrs_get(&v, sym)).flatten() {
                Some(a) => cur = a.val,
                None => {
                    result = false;
                    break;
                }
            }
        }
        self.stack.truncate(dyn_start - 1);
        self.stack.push(self.bool_cell(result));
        Ok(())
    }

    pub(crate) fn op_concat_strings(&mut self, fi: usize, d: u32) -> Result<(), ErrId> {
        let desc = &self.frames[fi].code.prog().concat_descs[d as usize];
        let n = desc.poss.len();
        let force_string = desc.force_string;
        let pos = desc.pos;
        let poss: Vec<PosIdx> = desc.poss.clone();
        let start = self.stack.len() - n;

        #[derive(PartialEq, Clone, Copy)]
        enum Mode {
            Unset,
            Int,
            Float,
            Str(Tag), // first value's tag (String or Path or other-coerced)
        }
        let mut mode = if force_string {
            Mode::Str(Tag::String)
        } else {
            Mode::Unset
        };
        let mut acc_i: i64 = 0;
        let mut acc_f: f64 = 0.0;
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let mut ctx: Vec<u32> = Vec::new();
        let mut first = !force_string;

        for k in 0..n {
            let cell = self.stack[start + k];
            let i_pos = poss[k];
            self.force(cell, i_pos)?;
            let v = val(cell);
            if first {
                mode = match v.tag() {
                    Tag::Int => Mode::Int,
                    Tag::Float => Mode::Float,
                    t => Mode::Str(t),
                };
            }
            match mode {
                Mode::Int => match v.tag() {
                    Tag::Int => {
                        let rhs = v.as_int();
                        match acc_i.checked_add(rhs) {
                            Some(s) => acc_i = s,
                            None => {
                                let msg = format!(
                                    "integer overflow in adding {} + {}",
                                    acc_i, rhs
                                );
                                return Err(self.new_err(ErrKind::Eval, msg, i_pos));
                            }
                        }
                    }
                    Tag::Float => {
                        mode = Mode::Float;
                        acc_f = acc_i as f64 + v.as_float();
                    }
                    _ => {
                        let msg =
                            format!("cannot add {} to an integer", self.show_type(&v));
                        return Err(self.new_err(ErrKind::Eval, msg, i_pos));
                    }
                },
                Mode::Float => match v.tag() {
                    Tag::Int => acc_f += v.as_int() as f64,
                    Tag::Float => acc_f += v.as_float(),
                    _ => {
                        let msg = format!("cannot add {} to a float", self.show_type(&v));
                        return Err(self.new_err(ErrKind::Eval, msg, i_pos));
                    }
                },
                Mode::Str(first_tag) => {
                    let (part, pctx) = self.coerce_to_string(
                        cell,
                        i_pos,
                        "while evaluating a path segment",
                        false,
                        first_tag == Tag::String,
                        !first,
                    )?;
                    parts.push(part);
                    for c in pctx {
                        if !ctx.contains(&c) {
                            ctx.push(c);
                        }
                    }
                }
                Mode::Unset => unreachable!(),
            }
            first = false;
        }

        let v = match mode {
            Mode::Int => Value::int(acc_i),
            Mode::Float => Value::float(acc_f),
            Mode::Str(Tag::Path) => {
                if !ctx.is_empty() {
                    return Err(self.new_err(
                        ErrKind::Eval,
                        "a string that refers to a store path cannot be appended to a path",
                        pos,
                    ));
                }
                let joined: Vec<u8> = parts.concat();
                let canon = canon_path(&joined);
                self.new_path_value(&canon)
            }
            Mode::Str(_) => {
                let joined: Vec<u8> = parts.concat();
                let cp = self.make_ctx(&ctx);
                self.new_string_value(&joined, cp)
            }
            Mode::Unset => unreachable!(),
        };
        let c = self.alloc_cell(v);
        self.stack.truncate(start);
        self.stack.push(c);
        Ok(())
    }

    pub fn make_ctx(&mut self, ids: &[u32]) -> *mut u64 {
        if ids.is_empty() {
            return std::ptr::null_mut();
        }
        self.gc_check();
        self.heap.new_ctx(ids)
    }

    /// Intern a context-element wire encoding, returning its id.
    pub fn intern_ctx(&mut self, enc: Vec<u8>) -> u32 {
        if let Some(&id) = self.ctx_intern.get(&enc) {
            return id;
        }
        let id = self.ctx_elems.len() as u32;
        self.ctx_elems.push(enc.clone());
        self.ctx_intern.insert(enc, id);
        id
    }

    /// Intern a decoded context element.
    pub fn intern_elem(&mut self, e: &crate::context::ContextElem) -> u32 {
        self.intern_ctx(e.encode())
    }

    /// The wire encoding of context element `id`.
    pub fn ctx_enc(&self, id: u32) -> &[u8] {
        &self.ctx_elems[id as usize]
    }

    /// Decode context element `id`.
    pub fn ctx_elem(&self, id: u32) -> crate::context::ContextElem {
        crate::context::ContextElem::parse(self.ctx_enc(id))
    }

    /// A `StoreDir` for the current store directory.
    pub fn store(&self) -> jinx_store::store_path::StoreDir {
        jinx_store::store_path::StoreDir::new(String::from_utf8_lossy(&self.store_dir).into_owned())
    }

    /// The live daemon connection, lazily opened on first use. Returns `None`
    /// under [`StoreMode::Dummy`] or if the connection cannot be established
    /// (so callers transparently fall back to read-only path computation).
    pub fn daemon(&mut self) -> Option<&mut jinx_store::daemon::DaemonStore> {
        if self.store_mode != StoreMode::Daemon {
            return None;
        }
        if self.daemon_conn.is_none() && !self.daemon_failed {
            match jinx_store::daemon::DaemonStore::connect() {
                Ok(s) => self.daemon_conn = Some(Box::new(s)),
                Err(e) => {
                    self.daemon_failed = true;
                    eprintln!("warning: could not connect to Nix daemon: {e}");
                }
            }
        }
        self.daemon_conn.as_deref_mut()
    }

    /// Read the context-element ids of a string value (empty if none).
    pub fn read_str_ctx(&self, v: &Value) -> Vec<u32> {
        let cp = str_ctx(v);
        if cp.is_null() {
            return Vec::new();
        }
        // SAFETY: ctx objects hold u32 ids.
        unsafe {
            let len = value::header_len(*cp);
            std::slice::from_raw_parts(cp.add(1) as *const u32, len).to_vec()
        }
    }

    /// Build a string value carrying context ids.
    pub fn new_string_ctx(&mut self, bytes: &[u8], ids: &[u32]) -> Value {
        let cp = self.make_ctx(ids);
        self.new_string_value(bytes, cp)
    }

    // ---------------- thunks / with ----------------

    pub(crate) fn make_thunk(&mut self, fi: usize, cid: u32, tag: Tag) -> VRef {
        // gc_check FIRST: the upval sources (stack cells, frame upvals,
        // with_local) are all rooted, and no collection can run while we fill
        // the fresh object below.
        self.gc_check();
        let prog = self.frames[fi].code.prog();
        let child: &Chunk = &prog.chunks[cid as usize];
        let cur_chunk = self.frames[fi].code.chunk();
        let n = child.with_captures as usize + child.captures.len();
        let code = prog.code_ref(cid) as *const CodeRef as *const ();
        if n == 0 && tag == Tag::Thunk {
            // Capture-free thunk: pack the code ref straight into the cell
            // (16 bytes total instead of a 16-byte data object + cell).
            return self.heap.alloc_value(Value::make(Tag::Thunk0, code as u64));
        }
        let (v, out) = self.heap.new_thunk_raw(tag, code, n);
        let mut k = 0usize;
        // SAFETY: `out` has n slots; we write exactly n below.
        unsafe {
            if child.with_captures > 0 {
                let f_upvals = self.frames[fi].upvals();
                let inherited = cur_chunk.with_captures as usize;
                std::ptr::copy_nonoverlapping(f_upvals.as_ptr(), out, inherited);
                k = inherited;
                let wl = &self.frames[fi].with_local;
                std::ptr::copy_nonoverlapping(wl.as_ptr(), out.add(k), wl.len());
                k += wl.len();
                debug_assert_eq!(k, child.with_captures as usize);
            }
            let base = self.frames[fi].locals_base;
            for cap in &child.captures {
                let c = match cap {
                    crate::chunk::Cap::Local(s) => self.stack[base + *s as usize],
                    crate::chunk::Cap::Upval(i) => self.frames[fi].upvals()[*i as usize],
                };
                out.add(k).write(c);
                k += 1;
            }
            debug_assert_eq!(k, n);
        }
        self.heap.alloc_value(v)
    }

    pub(crate) fn resolve_with(&mut self, fi: usize, sym: Symbol, pos: PosIdx) -> Result<VRef, ErrId> {
        // Innermost first: local with entries (last pushed first), then the
        // captured prefix in reverse (it is stored outermost-first).
        let n_local = self.frames[fi].with_local.len();
        let wc = self.frames[fi].code.chunk().with_captures as usize;
        for k in (0..n_local + wc).rev() {
            let cell = if k >= wc {
                self.frames[fi].with_local[k - wc]
            } else {
                self.frames[fi].upvals()[k]
            };
            self.force_attrs(
                cell,
                pos,
                "while evaluating the first subexpression of a with expression",
            )?;
            let v = val(cell);
            if let Some(a) = attrs_get(&v, sym) {
                return Ok(a.val);
            }
        }
        let name = self.symbols.resolve_str_lossy(sym);
        Err(self.new_err(
            ErrKind::UndefinedVar,
            format!("undefined variable '{name}'"),
            pos,
        ))
    }

    /// Frame-less interpreter for [`crate::chunk::ChunkKind::Straight`]
    /// chunks: runs the body against a small native-stack scratch array
    /// instead of pushing a `Frame` (the conservative stack scan roots the
    /// scratch cells; the forced cell's blackhole roots `upvals`). The
    /// caller has already blackholed the cell and set `force_pos`.
    fn force_straight(
        &mut self,
        code: &'static CodeRef,
        upvals: &'static [VRef],
    ) -> Result<Value, ErrId> {
        let chunk = code.chunk();
        let prog = code.prog();
        let mut st = [NonNull::<Value>::dangling(); 8];
        let mut sp = 0usize;
        let mut ip = 0usize;
        loop {
            match chunk.ops[ip] {
                Op::Const(i) => {
                    st[sp] = prog.consts[i as usize];
                    sp += 1;
                }
                Op::GetUpval(i) => {
                    st[sp] = upvals[i as usize];
                    sp += 1;
                }
                Op::ResolveWith(sym) => {
                    let c = self.resolve_with_upvals(chunk, upvals, Symbol(sym), chunk.pos_at(ip))?;
                    st[sp] = c;
                    sp += 1;
                }
                Op::Force => {
                    let c = st[sp - 1];
                    if val(c).needs_force() {
                        self.force(c, chunk.pos_at(ip))?;
                    }
                }
                Op::MakeThunk(cid) => {
                    st[sp] = self.make_thunk_from_upvals(prog, cid, Tag::Thunk, upvals);
                    sp += 1;
                }
                Op::MakeClosure(cid) => {
                    st[sp] = self.make_thunk_from_upvals(prog, cid, Tag::Closure, upvals);
                    sp += 1;
                }
                Op::Select { sym, cache } => {
                    st[sp - 1] =
                        self.select_value(st[sp - 1], Symbol(sym), cache, prog, chunk.pos_at(ip))?;
                }
                Op::SelectForce(t) => {
                    let c = st[sp - 1];
                    let p = self.last_select_pos;
                    self.force(c, p).map_err(|e| {
                        if p.is_set() && !self.pos_is_derivation_internal(p) {
                            let text = prog.texts[t as usize].clone();
                            self.add_trace(
                                e,
                                p,
                                format!(
                                    "while evaluating the attribute '{}'",
                                    String::from_utf8_lossy(&text)
                                ),
                            );
                        }
                        e
                    })?;
                }
                Op::Call(n) => {
                    let n = n as usize;
                    let fun = st[sp - n - 1];
                    let mut cpos = chunk.pos_at(ip);
                    if !cpos.is_set() {
                        cpos = self.force_pos;
                        if !cpos.is_set() {
                            cpos = self.determine_pos(&val(fun), NO_POS);
                        }
                    }
                    let v = self.call_function(fun, &st[sp - n..sp], cpos)?;
                    sp -= n + 1;
                    let c = self.alloc_cell(v);
                    st[sp] = c;
                    sp += 1;
                }
                Op::Ret => return Ok(val(st[sp - 1])),
                _ => unreachable!("non-straight op in Straight chunk"),
            }
            ip += 1;
        }
    }

    /// `make_thunk` for the frame-less path: captures resolve against an
    /// upvalue array only (Straight chunks have no locals, and their
    /// children share the with-prefix — both enforced by `classify_chunk`).
    fn make_thunk_from_upvals(
        &mut self,
        prog: &'static crate::chunk::Program,
        cid: u32,
        tag: Tag,
        parent_upvals: &[VRef],
    ) -> VRef {
        self.gc_check();
        let child: &Chunk = &prog.chunks[cid as usize];
        let n = child.with_captures as usize + child.captures.len();
        let code = prog.code_ref(cid) as *const CodeRef as *const ();
        if n == 0 && tag == Tag::Thunk {
            // Capture-free thunk: pack the code ref straight into the cell
            // (16 bytes total instead of a 16-byte data object + cell).
            return self.heap.alloc_value(Value::make(Tag::Thunk0, code as u64));
        }
        let (v, out) = self.heap.new_thunk_raw(tag, code, n);
        // SAFETY: `out` has n slots; we write exactly n below.
        unsafe {
            let wc = child.with_captures as usize;
            std::ptr::copy_nonoverlapping(parent_upvals.as_ptr(), out, wc);
            let mut k = wc;
            for cap in &child.captures {
                let c = match cap {
                    crate::chunk::Cap::Upval(i) => parent_upvals[*i as usize],
                    crate::chunk::Cap::Local(_) => {
                        unreachable!("straight chunk child with local capture")
                    }
                };
                out.add(k).write(c);
                k += 1;
            }
            debug_assert_eq!(k, n);
        }
        self.heap.alloc_value(v)
    }

    /// `resolve_with` for the frame-less path: a Straight chunk has no
    /// runtime with-entries of its own, so only the captured prefix
    /// (outermost first) is searched.
    fn resolve_with_upvals(
        &mut self,
        chunk: &'static Chunk,
        upvals: &[VRef],
        sym: Symbol,
        pos: PosIdx,
    ) -> Result<VRef, ErrId> {
        let wc = chunk.with_captures as usize;
        for k in (0..wc).rev() {
            let cell = upvals[k];
            self.force_attrs(
                cell,
                pos,
                "while evaluating the first subexpression of a with expression",
            )?;
            let v = val(cell);
            if let Some(a) = attrs_get(&v, sym) {
                return Ok(a.val);
            }
        }
        let name = self.symbols.resolve_str_lossy(sym);
        Err(self.new_err(
            ErrKind::UndefinedVar,
            format!("undefined variable '{name}'"),
            pos,
        ))
    }

    // ---------------- calls ----------------

    pub fn call_function(
        &mut self,
        fun: VRef,
        args: &[VRef],
        pos: PosIdx,
    ) -> Result<Value, ErrId> {
        self.depth_check(pos)?;
        self.call_depth += 1;
        let r = self.call_function_inner(fun, args, pos);
        self.call_depth -= 1;
        r
    }

    /// Port of `addCallDepth`: must be called *before* incrementing
    /// `call_depth` for the current frame.
    pub(crate) fn depth_check(&mut self, pos: PosIdx) -> Result<(), ErrId> {
        if self.call_depth > self.max_call_depth {
            return Err(self.new_err(
                ErrKind::StackOverflow,
                "stack overflow; max-call-depth exceeded",
                pos,
            ));
        }
        Ok(())
    }

    fn call_function_inner(
        &mut self,
        fun: VRef,
        args: &[VRef],
        pos: PosIdx,
    ) -> Result<Value, ErrId> {
        self.force(fun, pos)?;
        let mut vcur = val(fun);
        let mut i = 0usize;

        while i < args.len() {
            match vcur.tag() {
                Tag::Closure => {
                    vcur = self.call_closure(vcur, args[i], pos)?;
                    i += 1;
                }
                Tag::PrimOp => {
                    let def = primop_of(&vcur);
                    let needed = def.arity as usize;
                    let remaining = args.len() - i;
                    if remaining < needed {
                        // Not enough arguments: build a PrimOpApp chain.
                        self.gc_check();
                        let v = self
                            .heap
                            .new_primapp(vcur.w1 as *const (), &args[i..]);
                        return Ok(v);
                    }
                    let f = def.func;
                    // C++ invokes primops with `vCur.determinePos(noPos)`,
                    // which is `noPos` for a bare primop. The call-site `pos`
                    // is only used for the "while calling the '…' builtin"
                    // frame.
                    vcur = f(self, def, &args[i..i + needed], NO_POS).map_err(|e| {
                        self.add_primop_trace(e, def, pos);
                        e
                    })?;
                    i += needed;
                }
                Tag::PrimOpApp => {
                    let (def, done) = primapp_parts(&vcur);
                    let needed = def.arity as usize - done.len();
                    let remaining = args.len() - i;
                    if remaining < needed {
                        let mut all: Vec<VRef> = done.to_vec();
                        all.extend_from_slice(&args[i..]);
                        // `done` cells stay rooted via vcur (native local).
                        self.gc_check();
                        let prim = def as *const PrimOpDef as *const ();
                        let v = self.heap.new_primapp(prim, &all);
                        return Ok(v);
                    }
                    let mut all: Vec<VRef> = done.to_vec();
                    all.extend_from_slice(&args[i..i + needed]);
                    let scope = self.temp_scope();
                    self.temp_roots.extend_from_slice(&all);
                    let f = def.func;
                    let r = f(self, def, &all, NO_POS);
                    self.temp_end(scope);
                    vcur = r.map_err(|e| {
                        self.add_primop_trace(e, def, pos);
                        e
                    })?;
                    i += needed;
                }
                Tag::Attrs => {
                    let functor = attrs_get(&vcur, self.syms.functor);
                    match functor {
                        Some(f) => {
                            let self_cell = self.alloc_cell(vcur);
                            let scope = self.temp_scope();
                            self.temp_roots.push(self_cell);
                            let r = self
                                .call_function(f.val, &[self_cell, args[i]], PosIdx(f.pos));
                            self.temp_end(scope);
                            let v = r.map_err(|e| {
                                self.add_trace(
                                    e,
                                    pos,
                                    "while calling a functor (an attribute set with a '__functor' attribute)",
                                );
                                e
                            })?;
                            vcur = v;
                            i += 1;
                        }
                        None => return Err(self.not_a_function_err(&vcur, pos)),
                    }
                }
                _ => return Err(self.not_a_function_err(&vcur, pos)),
            }
            // `vcur` may need forcing between applications (e.g. a lambda
            // body returning a thunk value cannot happen — run_code returns
            // WHNF-or-thunk copies; force via a temp cell when required).
            if i < args.len() && matches!(vcur.tag(), Tag::Thunk | Tag::Thunk0) {
                let c = self.alloc_cell(vcur);
                let scope = self.temp_scope();
                self.temp_roots.push(c);
                let r = self.force(c, pos);
                self.temp_end(scope);
                r?;
                vcur = val(c);
            }
        }
        Ok(vcur)
    }

    fn not_a_function_err(&mut self, v: &Value, pos: PosIdx) -> ErrId {
        let printed = crate::print::print_value_err(self, v);
        let msg = format!(
            "attempt to call something which is not a function but {}: {}",
            self.show_type(v),
            printed
        );
        self.new_err(ErrKind::Type, msg, pos)
    }

    /// Apply a closure to one argument (the C++ lambda branch of
    /// callFunction).
    /// Bare lambda name (`%1%` form) for "called without/with … argument"
    /// errors; `"anonymous lambda"` when unnamed. Cold path only.
    fn lambda_raw_name(&self, chunk: &Chunk) -> String {
        if chunk.name.is_set() {
            self.symbols.resolve_str_lossy(chunk.name)
        } else {
            "anonymous lambda".into()
        }
    }

    /// Trace form (`'name'`, or unquoted `anonymous lambda`) for the
    /// "while calling …" frame. Cold path only.
    fn lambda_trace_name(&self, chunk: &Chunk) -> String {
        if chunk.name.is_set() {
            format!("'{}'", String::from_utf8_lossy(self.symbols.resolve(chunk.name)))
        } else {
            "anonymous lambda".into()
        }
    }

    fn call_closure(&mut self, vcur: Value, arg: VRef, pos: PosIdx) -> Result<Value, ErrId> {
        let (code, _) = thunk_code(&vcur);
        let chunk = code.chunk();
        let spec = chunk.lambda.as_ref().expect("closure without lambda spec");
        let base = self.stack.len();

        // Display names (the bare "%1%" form and the quoted trace form) are
        // computed lazily at the cold error sites — allocating them on every
        // call showed up in profiles of real evaluations.

        let mut pending_defaults: Vec<(usize, u32)> = Vec::new(); // (stack idx, chunk)

        if let Some(formals) = &spec.formals {
            self.force_attrs(
                arg,
                chunk.pos,
                "while evaluating the value passed for the lambda argument",
            )
            .map_err(|e| {
                if pos.is_set() {
                    self.add_trace(e, pos, "from call site");
                }
                e
            })?;
            if spec.arg.is_set() {
                self.stack.push(arg);
            }
            let attrs = val(arg);
            let mut attrs_used = 0usize;
            for f in &formals.formals {
                match attrs_get(&attrs, f.name) {
                    Some(a) => {
                        attrs_used += 1;
                        self.stack.push(a.val);
                    }
                    None => match f.default {
                        Some(cid) => {
                            let prog = code.prog();
                            if let crate::chunk::ChunkKind::ConstReturn { idx } =
                                prog.chunks[cid as usize].kind
                            {
                                // Literal default (`? []`, `? null`, ...):
                                // alias the immortal const cell directly --
                                // no placeholder cell, no default thunk, no
                                // later force.
                                self.stack.push(prog.consts[idx as usize]);
                            } else {
                                let c = self.alloc_cell(Value::make(Tag::Blackhole, 0));
                                pending_defaults.push((self.stack.len(), cid));
                                self.stack.push(c);
                            }
                        }
                        None => {
                            let name = self.lambda_raw_name(chunk);
                            let fname =
                                self.symbols.resolve_str_lossy(f.name);
                            let e = self.new_err(
                                ErrKind::Type,
                                format!(
                                    "function '{}' called without required argument '{}'",
                                    name, fname
                                ),
                                chunk.pos,
                            );
                            if pos.is_set() {
                                self.add_trace(e, pos, "from call site");
                            }
                            self.stack.truncate(base);
                            return Err(e);
                        }
                    },
                }
            }
            if !formals.ellipsis && attrs_used != attrs_entries(&attrs).len() {
                for a in attrs_entries(&attrs) {
                    if !formals.formals.iter().any(|f| f.name.0 == a.sym) {
                        let name = self.lambda_raw_name(chunk);
                        let aname = self.symbols.resolve_str_lossy(Symbol(a.sym));
                        let cands: Vec<String> = formals
                            .formals
                            .iter()
                            .map(|f| {
                                self.symbols.resolve_str_lossy(f.name)
                            })
                            .collect();
                        let sugg = best_matches(cands.into_iter(), &aname);
                        let e = self.new_err(
                            ErrKind::Type,
                            format!(
                                "function '{}' called with unexpected argument '{}'",
                                name, aname
                            ),
                            chunk.pos,
                        );
                        self.errors[e as usize].suggestions = sugg;
                        if pos.is_set() {
                            self.add_trace(e, pos, "from call site");
                        }
                        self.stack.truncate(base);
                        return Err(e);
                    }
                }
                unreachable!();
            }
        } else {
            self.stack.push(arg);
        }

        // Frame for the body; fill deferred defaults now that all formal
        // slots exist.
        self.frames.push(Frame {
            code,
            data: vcur,
            locals_base: base,
            with_local: Vec::new(),
        });
        let fi = self.frames.len() - 1;
        for (slot_idx, cid) in pending_defaults {
            let t = self.make_thunk(fi, cid, Tag::Thunk);
            let dst = self.stack[slot_idx];
            self.set_b(dst, val(t));
        }
        let chunk_pos = chunk.pos;
        let r = self.run_top_frame();
        self.frames.pop();
        let out = match r {
            Ok(v) => Ok(val(v)),
            Err(e) => {
                // Port of callFunction's body-eval catch: "while calling
                // <name>" at the lambda pos, then "from call site" at the
                // application pos. C++ only adds these when `showTrace` is on.
                if self.show_trace {
                    let lambda_name = self.lambda_trace_name(chunk);
                    self.add_trace(e, chunk_pos, format!("while calling {lambda_name}"));
                    if pos.is_set() {
                        self.add_trace(e, pos, "from call site");
                    }
                }
                Err(e)
            }
        };
        self.stack.truncate(base);
        out
    }

    // ---------------- equality ----------------

    pub fn eq_values(
        &mut self,
        a: VRef,
        b: VRef,
        pos: PosIdx,
        ctx: &str,
        top: bool,
    ) -> Result<bool, ErrId> {
        self.depth_check(pos)?;
        self.call_depth += 1;
        let r = self.eq_values_inner(a, b, pos, ctx, top);
        self.call_depth -= 1;
        r
    }

    fn eq_values_inner(
        &mut self,
        a: VRef,
        b: VRef,
        pos: PosIdx,
        ctx: &str,
        top: bool,
    ) -> Result<bool, ErrId> {
        self.force(a, pos)?;
        self.force(b, pos)?;

        // Pointer-equality fast path — but not at the top level, where C++
        // compares freshly evaluated temporaries (so `f == f` is false for
        // functions).
        if !top && a == b {
            return Ok(true);
        }

        let (va, vb) = (val(a), val(b));

        // int/float cross-type equality.
        match (va.tag(), vb.tag()) {
            (Tag::Int, Tag::Float) => return Ok(va.as_int() as f64 == vb.as_float()),
            (Tag::Float, Tag::Int) => return Ok(va.as_float() == vb.as_int() as f64),
            _ => {}
        }
        let same_type = match (va.tag(), vb.tag()) {
            (Tag::True | Tag::False, Tag::True | Tag::False) => true,
            (x, y) => x == y,
        };
        if !same_type {
            return Ok(false);
        }

        match va.tag() {
            Tag::Int => Ok(va.as_int() == vb.as_int()),
            Tag::Float => Ok(va.as_float() == vb.as_float()),
            Tag::True | Tag::False => Ok(va.tag() == vb.tag()),
            Tag::Null => Ok(true),
            Tag::String => Ok(str_bytes(&va) == str_bytes(&vb)),
            Tag::Path => Ok(path_bytes(&va) == path_bytes(&vb)),
            Tag::List => {
                let (ea, eb) = (list_elems(&va), list_elems(&vb));
                if ea.len() != eb.len() {
                    return Ok(false);
                }
                for k in 0..ea.len() {
                    if !self.eq_values(ea[k], eb[k], pos, ctx, false)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Tag::Attrs => {
                // Derivations compare by outPath.
                if self.is_derivation(&va)? && self.is_derivation(&vb)? {
                    let i = attrs_get(&va, self.syms.out_path);
                    let j = attrs_get(&vb, self.syms.out_path);
                    if let (Some(i), Some(j)) = (i, j) {
                        return self.eq_values(i.val, j.val, pos, ctx, false);
                    }
                }
                let (ea, eb) = (attrs_entries(&va), attrs_entries(&vb));
                if ea.len() != eb.len() {
                    return Ok(false);
                }
                for k in 0..ea.len() {
                    if ea[k].sym != eb[k].sym
                        || !self.eq_values(ea[k].val, eb[k].val, pos, ctx, false)?
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            // Functions are incomparable.
            Tag::Closure | Tag::PrimOp | Tag::PrimOpApp => Ok(false),
            Tag::Thunk | Tag::Thunk0 | Tag::Blackhole | Tag::Blackhole0 | Tag::Failed => {
                unreachable!("forced")
            }
        }
    }

    /// Port of `EvalState::assertEqValues` (eval.cc): deep structural equality
    /// that, on the first difference, throws a detailed `AssertionError`
    /// describing exactly how the two values differ. Used by `assert a == b`.
    pub fn assert_eq_values(
        &mut self,
        a: VRef,
        b: VRef,
        pos: PosIdx,
        ctx: &str,
    ) -> Result<(), ErrId> {
        self.depth_check(pos)?;
        self.call_depth += 1;
        let r = self.assert_eq_values_inner(a, b, pos, ctx);
        self.call_depth -= 1;
        r
    }

    fn assert_eq_values_inner(
        &mut self,
        a: VRef,
        b: VRef,
        pos: PosIdx,
        ctx: &str,
    ) -> Result<(), ErrId> {
        self.force(a, pos)?;
        self.force(b, pos)?;
        if a == b {
            return Ok(());
        }
        let (va, vb) = (val(a), val(b));
        let pr = |vm: &Self, v: &Value| crate::print::print_value_err(vm, v);

        let a_num = matches!(va.tag(), Tag::Int | Tag::Float);
        let b_num = matches!(vb.tag(), Tag::Int | Tag::Float);

        // Special case type-compatibility between float and int.
        if a_num && b_num {
            let eq = match (va.tag(), vb.tag()) {
                (Tag::Int, Tag::Int) => va.as_int() == vb.as_int(),
                (Tag::Float, Tag::Float) => va.as_float() == vb.as_float(),
                (Tag::Int, Tag::Float) => va.as_int() as f64 == vb.as_float(),
                (Tag::Float, Tag::Int) => va.as_float() == vb.as_int() as f64,
                _ => unreachable!(),
            };
            if eq {
                return Ok(());
            }
            let msg = format!(
                "{} with value '{}' is not equal to {} with value '{}'",
                self.show_type(&va),
                pr(self, &va),
                self.show_type(&vb),
                pr(self, &vb),
            );
            return Err(self.new_err(ErrKind::Assertion, msg, NO_POS));
        }

        let same_type = match (va.tag(), vb.tag()) {
            (Tag::True | Tag::False, Tag::True | Tag::False) => true,
            (x, y) => x == y,
        };
        if !same_type {
            let msg = format!(
                "{} of value '{}' is not equal to {} of value '{}'",
                self.show_type(&va),
                pr(self, &va),
                self.show_type(&vb),
                pr(self, &vb),
            );
            return Err(self.new_err(ErrKind::Assertion, msg, NO_POS));
        }

        match va.tag() {
            Tag::True | Tag::False => {
                if va.tag() != vb.tag() {
                    let msg = format!(
                        "boolean '{}' is not equal to boolean '{}'",
                        pr(self, &va),
                        pr(self, &vb),
                    );
                    return Err(self.new_err(ErrKind::Assertion, msg, NO_POS));
                }
                Ok(())
            }
            Tag::String => {
                if str_bytes(&va) != str_bytes(&vb) {
                    let msg = format!(
                        "string '{}' is not equal to string '{}'",
                        pr(self, &va),
                        pr(self, &vb),
                    );
                    return Err(self.new_err(ErrKind::Assertion, msg, NO_POS));
                }
                Ok(())
            }
            Tag::Path => {
                if path_bytes(&va) != path_bytes(&vb) {
                    let msg = format!(
                        "path '{}' is not equal to path '{}'",
                        pr(self, &va),
                        pr(self, &vb),
                    );
                    return Err(self.new_err(ErrKind::Assertion, msg, NO_POS));
                }
                Ok(())
            }
            Tag::Null => Ok(()),
            Tag::List => {
                let (ea, eb) = (list_elems(&va), list_elems(&vb));
                if ea.len() != eb.len() {
                    let msg = format!(
                        "list of size '{}' is not equal to list of size '{}', left hand side is '{}', right hand side is '{}'",
                        ea.len(),
                        eb.len(),
                        pr(self, &va),
                        pr(self, &vb),
                    );
                    return Err(self.new_err(ErrKind::Assertion, msg, NO_POS));
                }
                let (ea, eb) = (ea.to_vec(), eb.to_vec());
                for n in 0..ea.len() {
                    self.assert_eq_values(ea[n], eb[n], pos, ctx).map_err(|e| {
                        self.add_trace(e, pos, format!("while comparing list element {n}"));
                        e
                    })?;
                }
                Ok(())
            }
            Tag::Attrs => {
                if self.is_derivation(&va)? && self.is_derivation(&vb)? {
                    let i = attrs_get(&va, self.syms.out_path);
                    let j = attrs_get(&vb, self.syms.out_path);
                    if let (Some(i), Some(j)) = (i, j) {
                        let (iv, jv) = (i.val, j.val);
                        return self.assert_eq_values(iv, jv, pos, ctx).map_err(|e| {
                            self.add_trace(
                                e,
                                pos,
                                "while comparing a derivation by its 'outPath' attribute",
                            );
                            e
                        });
                    }
                }
                let (ea, eb) = (attrs_entries(&va), attrs_entries(&vb));
                if ea.len() != eb.len() {
                    let msg = format!(
                        "attribute names of attribute set '{}' differs from attribute set '{}'",
                        pr(self, &va),
                        pr(self, &vb),
                    );
                    return Err(self.new_err(ErrKind::Assertion, msg, NO_POS));
                }
                let ea = ea.to_vec();
                let eb = eb.to_vec();
                for k in 0..ea.len() {
                    if ea[k].sym != eb[k].sym {
                        // Names differ: figure out which side is missing.
                        if attrs_get(&vb, Symbol(ea[k].sym)).is_none() {
                            let name = String::from_utf8_lossy(
                                self.symbols.resolve(Symbol(ea[k].sym)),
                            )
                            .into_owned();
                            let msg = format!(
                                "attribute name '{}' is contained in '{}', but not in '{}'",
                                name,
                                pr(self, &va),
                                pr(self, &vb),
                            );
                            return Err(self.new_err(ErrKind::Assertion, msg, NO_POS));
                        }
                        if attrs_get(&va, Symbol(eb[k].sym)).is_none() {
                            let name = String::from_utf8_lossy(
                                self.symbols.resolve(Symbol(eb[k].sym)),
                            )
                            .into_owned();
                            let msg = format!(
                                "attribute name '{}' is missing in '{}', but is contained in '{}'",
                                name,
                                pr(self, &va),
                                pr(self, &vb),
                            );
                            return Err(self.new_err(ErrKind::Assertion, msg, NO_POS));
                        }
                        unreachable!();
                    }
                    let (iv, jv) = (ea[k].val, eb[k].val);
                    let (ipos, jpos) = (PosIdx(ea[k].pos), PosIdx(eb[k].pos));
                    let name =
                        self.symbols.resolve_str_lossy(Symbol(ea[k].sym));
                    self.assert_eq_values(iv, jv, pos, ctx).map_err(|e| {
                        // Reversed order (push order): rhs, lhs, comparing.
                        if jpos.is_set() {
                            self.add_trace(e, jpos, "where right hand side is");
                        }
                        if ipos.is_set() {
                            self.add_trace(e, ipos, "where left hand side is");
                        }
                        self.add_trace(e, pos, format!("while comparing attribute '{name}'"));
                        e
                    })?;
                }
                Ok(())
            }
            Tag::Closure | Tag::PrimOp | Tag::PrimOpApp => Err(self.new_err(
                ErrKind::Assertion,
                "distinct functions and immediate comparisons of identical functions compare as unequal",
                NO_POS,
            )),
            Tag::Int | Tag::Float => {
                // Both numeric: handled by the int/float branch above.
                Ok(())
            }
            Tag::Thunk | Tag::Thunk0 | Tag::Blackhole | Tag::Blackhole0 | Tag::Failed => {
                unreachable!("forced")
            }
        }
    }

    pub fn is_derivation(&mut self, v: &Value) -> Result<bool, ErrId> {
        if v.tag() != Tag::Attrs {
            return Ok(false);
        }
        let Some(t) = attrs_get(v, self.syms.type_) else {
            return Ok(false);
        };
        self.force(t.val, PosIdx(t.pos))?;
        let tv = val(t.val);
        Ok(tv.tag() == Tag::String && str_bytes(&tv) == b"derivation")
    }

    // ---------------- coercion ----------------

    /// Port of EvalState::coerceToString. Returns bytes + context ids.
    /// Port of `copyPathToStore` under the readonly/dummy store: NAR-hash the
    /// path and compute its `source` store path (never writing), returning the
    /// printed store path plus an interned `Opaque` context id.
    pub fn copy_path_to_store(
        &mut self,
        path: &[u8],
        pos: PosIdx,
    ) -> Result<(Vec<u8>, u32), ErrId> {
        use jinx_store::hash::HashAlgorithm;
        use jinx_store::store_path::{FileIngestionMethod, FixedOutputInfo, StoreReferences};
        use std::os::unix::ffi::OsStrExt;
        if path.ends_with(b".drv") {
            let e = self.new_err(
                ErrKind::Eval,
                "file names are not allowed to end in '.drv'",
                pos,
            );
            return Err(e);
        }
        if let Some((printed, id)) = self.src_to_store.get(path) {
            return Ok((printed.clone(), *id));
        }
        let os = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
        // Read/NAR-hash the *real* on-disk location, applying any store
        // redirect (as every read builtin does). Coercing a path inside a
        // not-yet-materialized flake source (e.g. `src = ./.` in nixpkgs)
        // maps `/nix/store/<h>-source/...` to the extracted temp dir; without
        // this, hashing the logical path fails with "does not exist" on a cold
        // store. The resulting store path still derives its name from the
        // logical basename below, matching C++ fetchToStore(path.baseName()).
        let real = self.redirect_fs(os);
        let (hash, _sz) = match jinx_store::nar::hash_path(&real, HashAlgorithm::Sha256) {
            Ok(r) => r,
            Err(e) => {
                // C++ throws a bare `FileNotFound("path '%s' does not exist")`
                // for a missing path (no position, no surrounding trace).
                if e.kind() == std::io::ErrorKind::NotFound {
                    let msg = format!("path '{}' does not exist", String::from_utf8_lossy(path));
                    return Err(self.new_err(ErrKind::Eval, msg, NO_POS));
                }
                let msg = format!(
                    "getting attributes of path '{}': {}",
                    String::from_utf8_lossy(path),
                    e
                );
                return Err(self.new_err(ErrKind::Eval, msg, pos));
            }
        };
        let base = match path.iter().rposition(|&c| c == b'/') {
            Some(i) => &path[i + 1..],
            None => path,
        };
        let name = String::from_utf8_lossy(base).into_owned();
        let store = self.store();
        let sp = store
            .make_fixed_output_path(
                &name,
                &FixedOutputInfo {
                    method: FileIngestionMethod::NixArchive,
                    hash,
                    references: StoreReferences::default(),
                },
            )
            .map_err(|e| self.new_err(ErrKind::Eval, e.0, pos))?;
        let printed = store.print_store_path(&sp).into_bytes();
        let id = self.intern_elem(&crate::context::ContextElem::Opaque {
            path: sp.to_string().as_bytes().to_vec(),
        });
        self.src_to_store
            .insert(path.to_vec(), (printed.clone(), id));
        Ok((printed, id))
    }

    pub fn coerce_to_string(
        &mut self,
        cell: VRef,
        pos: PosIdx,
        ctx: &str,
        coerce_more: bool,
        copy_to_store: bool,
        canonicalize_path: bool,
    ) -> Result<(Vec<u8>, Vec<u32>), ErrId> {
        self.depth_check(pos)?;
        self.call_depth += 1;
        let r =
            self.coerce_inner(cell, pos, ctx, coerce_more, copy_to_store, canonicalize_path);
        self.call_depth -= 1;
        r
    }

    fn coerce_inner(
        &mut self,
        cell: VRef,
        pos: PosIdx,
        ctx: &str,
        coerce_more: bool,
        copy_to_store: bool,
        canonicalize_path: bool,
    ) -> Result<(Vec<u8>, Vec<u32>), ErrId> {
        self.force(cell, pos)?;
        let v = val(cell);
        match v.tag() {
            Tag::String => Ok((str_bytes(&v).to_vec(), str_ctx_ids(&v).to_vec())),
            Tag::Path => {
                if copy_to_store {
                    let path = path_bytes(&v).to_vec();
                    // C++ does not wrap copyPathToStore errors with `errorCtx`
                    // (the "path segment" frame), so neither do we.
                    let (printed, id) = self.copy_path_to_store(&path, pos)?;
                    Ok((printed, vec![id]))
                } else {
                    // canonicalizePath=false preserves literal trailing
                    // slashes; our path payload is stored as-is either way.
                    let _ = canonicalize_path;
                    Ok((path_bytes(&v).to_vec(), Vec::new()))
                }
            }
            Tag::Attrs => {
                if let Some(f) = attrs_get(&v, self.syms.to_string) {
                    let r = self.call_function(f.val, &[cell], pos)?;
                    let rc = self.alloc_cell(r);
                    let scope = self.temp_scope();
                    self.temp_roots.push(rc);
                    let out = self.coerce_to_string(
                        rc,
                        pos,
                        "while evaluating the result of the `__toString` attribute",
                        coerce_more,
                        copy_to_store,
                        canonicalize_path,
                    );
                    self.temp_end(scope);
                    return out;
                }
                if let Some(op) = attrs_get(&v, self.syms.out_path) {
                    return self.coerce_to_string(
                        op.val,
                        pos,
                        ctx,
                        coerce_more,
                        copy_to_store,
                        canonicalize_path,
                    );
                }
                Err(self.cannot_coerce_err(&v, pos, ctx))
            }
            _ if coerce_more => match v.tag() {
                Tag::True => Ok((b"1".to_vec(), Vec::new())),
                Tag::False | Tag::Null => Ok((Vec::new(), Vec::new())),
                Tag::Int => Ok((v.as_int().to_string().into_bytes(), Vec::new())),
                Tag::Float => Ok((format!("{:.6}", v.as_float()).into_bytes(), Vec::new())),
                Tag::List => {
                    let elems = list_elems(&v);
                    let mut out: Vec<u8> = Vec::new();
                    let mut ctxs: Vec<u32> = Vec::new();
                    for (k, &el) in elems.iter().enumerate() {
                        let (part, pctx) = self
                            .coerce_to_string(
                                el,
                                pos,
                                "while evaluating one element of the list",
                                coerce_more,
                                copy_to_store,
                                canonicalize_path,
                            )
                            .map_err(|e| {
                                self.add_trace(e, pos, ctx);
                                e
                            })?;
                        out.extend_from_slice(&part);
                        for c in pctx {
                            if !ctxs.contains(&c) {
                                ctxs.push(c);
                            }
                        }
                        let elv = val(el);
                        let el_empty_list =
                            elv.tag() == Tag::List && list_elems(&elv).is_empty();
                        if k + 1 < elems.len() && !el_empty_list {
                            out.push(b' ');
                        }
                    }
                    Ok((out, ctxs))
                }
                _ => Err(self.cannot_coerce_err(&v, pos, ctx)),
            },
            _ => Err(self.cannot_coerce_err(&v, pos, ctx)),
        }
    }

    fn cannot_coerce_err(&mut self, v: &Value, pos: PosIdx, ctx: &str) -> ErrId {
        let printed = crate::print::print_value_err(self, v);
        let msg = format!(
            "cannot coerce {} to a string: {}",
            self.show_type(v),
            printed
        );
        // C++ coerceToString uses `.withTrace(pos, ctx)` — the base error has
        // no position of its own.
        let e = self.new_err(ErrKind::Type, msg, NO_POS);
        self.add_trace(e, pos, ctx);
        e
    }

    // ---------------- misc value builders ----------------

    /// `__curPos` / unsafeGetAttrPos-style position attrsets.
    pub fn mk_pos(&mut self, pos: PosIdx) -> Value {
        let is_path = matches!(
            self.positions.origin_of(pos),
            Some(jinx_syntax::pos::Origin::Path { .. })
        );
        if !is_path {
            return Value::null();
        }
        let p = self.positions.lookup(pos).unwrap();
        let file = match self.positions.origin_of(pos) {
            Some(jinx_syntax::pos::Origin::Path { path, .. }) => path.clone(),
            _ => unreachable!(),
        };
        let scope = self.temp_scope();
        let fv = self.new_string_value(file.as_bytes(), std::ptr::null_mut());
        let fc = self.alloc_cell(fv);
        self.temp_roots.push(fc);
        let lc = self.alloc_cell(Value::int(p.line as i64));
        self.temp_roots.push(lc);
        let cc = self.alloc_cell(Value::int(p.column as i64));
        self.temp_roots.push(cc);
        let mut entries = [
            Attr {
                sym: self.syms.file.0,
                pos: 0,
                val: fc,
            },
            Attr {
                sym: self.syms.line.0,
                pos: 0,
                val: lc,
            },
            Attr {
                sym: self.syms.column.0,
                pos: 0,
                val: cc,
            },
        ];
        entries.sort_by_key(|a| a.sym);
        let v = self.new_bindings_value(&entries);
        self.temp_end(scope);
        v
    }
}

/// Lexical path canonicalization matching C++ `CanonPath` (absolute paths;
/// collapses `//`, `.` and `..`; no trailing slash).
pub fn canon_path(p: &[u8]) -> Vec<u8> {
    let mut comps: Vec<&[u8]> = Vec::new();
    for comp in p.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                comps.pop();
            }
            c => comps.push(c),
        }
    }
    let mut out = Vec::with_capacity(p.len());
    if comps.is_empty() {
        out.push(b'/');
        return out;
    }
    for c in comps {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    out
}

/// Convenience for builtins: make an immortal-safe VRef from a raw pointer.
pub fn vref(p: *mut Value) -> VRef {
    NonNull::new(p).unwrap()
}
