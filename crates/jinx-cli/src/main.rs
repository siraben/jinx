//! jinx: nix-instantiate-compatible CLI.
//! M1: `--parse -`. M2: `--eval [--strict] [-E expr | file | -]` with
//! --arg/--argstr/-A/-I/NIX_PATH, printing via printAmbiguous.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use jinx_eval::builtins;
use jinx_eval::error::{ErrId, ErrKind};
use jinx_eval::print;
use jinx_eval::value::{Tag, VRef};
use jinx_eval::vm::{attrs_get, list_elems, str_bytes, thunk_code, val, VM};
use jinx_syntax::lexer::{Lexer, TokKind};
use jinx_syntax::pos::{PosIdx, NO_POS};
use jinx_syntax::symbol::Symbol;
use jinx_syntax::{parse_and_bind, parse_and_bind_with, show, Origin, PosTable, SymbolTable};

#[derive(Clone, Copy, PartialEq)]
enum LintLevel {
    Allow,
    Warn,
    Fatal,
}

struct Options {
    parse_only: bool,
    eval: bool,
    strict: bool,
    xml: bool,
    json: bool,
    read_stdin: bool,
    from_args: bool,
    files: Vec<String>,
    attr_paths: Vec<String>,
    /// (name, expr-or-string, is_string)
    auto_args: Vec<(String, String, bool)>,
    include_paths: Vec<String>,
    lint_url: LintLevel,
    lint_abs: LintLevel,
    lint_short: LintLevel,
    pure_eval: bool,
    /// Experimental features requested on the command line.
    experimental: Vec<String>,
    /// XML output source locations (`--no-location` clears this).
    location: bool,
    /// `--max-call-depth` override (C++ default 10000).
    max_call_depth: Option<usize>,
    /// Whether to print full traces. The harness nix.conf enables show-trace
    /// by default; `--no-show-trace` turns it off (traces are truncated).
    show_trace: bool,
    /// `--trace-verbose`: enable `builtins.traceVerbose` output.
    trace_verbose: bool,
    /// `--abort-on-warn` / `NIX_ABORT_ON_WARN`: `builtins.warn` throws.
    abort_on_warn: bool,
}

fn parse_lint_level(v: &str) -> Result<LintLevel, String> {
    match v {
        "allow" => Ok(LintLevel::Allow),
        "warn" => Ok(LintLevel::Warn),
        "fatal" => Ok(LintLevel::Fatal),
        _ => Err(format!("unknown lint level '{v}'")),
    }
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        parse_only: false,
        eval: false,
        strict: false,
        xml: false,
        json: false,
        read_stdin: false,
        from_args: false,
        files: vec![],
        attr_paths: vec![],
        auto_args: vec![],
        include_paths: vec![],
        lint_url: LintLevel::Allow,
        lint_abs: LintLevel::Allow,
        lint_short: LintLevel::Allow,
        pure_eval: false,
        experimental: vec![],
        location: true,
        max_call_depth: None,
        show_trace: true,
        trace_verbose: false,
        // `NIX_ABORT_ON_WARN` (truthy) enables abort-on-warn like the setting.
        abort_on_warn: matches!(
            std::env::var("NIX_ABORT_ON_WARN").ok().as_deref(),
            Some("1") | Some("true") | Some("yes")
        ),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let need = |args: &[String], i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("flag '{flag}' requires an argument"))
    };
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--parse" | "--parse-only" => opts.parse_only = true,
            "--eval" | "--eval-only" => opts.eval = true,
            "--strict" => opts.strict = true,
            "--xml" => opts.xml = true,
            "--json" => opts.json = true,
            "--no-location" => opts.location = false,
            "--expr" | "-E" => opts.from_args = true,
            "--attr" | "-A" => {
                let v = need(&args, &mut i, a)?;
                opts.attr_paths.push(v);
            }
            "--arg" => {
                let n = need(&args, &mut i, a)?;
                let v = need(&args, &mut i, a)?;
                opts.auto_args.push((n, v, false));
            }
            "--argstr" => {
                let n = need(&args, &mut i, a)?;
                let v = need(&args, &mut i, a)?;
                opts.auto_args.push((n, v, true));
            }
            "-I" | "--include" => {
                let v = need(&args, &mut i, a)?;
                opts.include_paths.push(v);
            }
            "--impure" => opts.pure_eval = false,
            "--pure-eval" => opts.pure_eval = true,
            "--show-trace" => opts.show_trace = true,
            "--no-show-trace" => opts.show_trace = false,
            "--trace-verbose" => opts.trace_verbose = true,
            "--abort-on-warn" => opts.abort_on_warn = true,
            "--no-abort-on-warn" => opts.abort_on_warn = false,
            "--read-write-mode" | "--readonly-mode" | "--dry-run" | "--indirect" => {}
            "--extra-experimental-features" | "--experimental-features" => {
                let v = need(&args, &mut i, a)?;
                for f in v.split_whitespace() {
                    opts.experimental.push(f.to_string());
                }
            }
            "--option" => {
                let n = need(&args, &mut i, a)?;
                let v = need(&args, &mut i, a)?;
                if n == "experimental-features" || n == "extra-experimental-features" {
                    for f in v.split_whitespace() {
                        opts.experimental.push(f.to_string());
                    }
                }
            }
            "--add-root" => {
                let _ = need(&args, &mut i, a)?;
            }
            "--max-call-depth" => {
                let v = need(&args, &mut i, a)?;
                opts.max_call_depth = v.parse::<usize>().ok();
            }
            "--lint-url-literals" => {
                let v = need(&args, &mut i, a)?;
                opts.lint_url = parse_lint_level(&v)?;
            }
            "--lint-absolute-path-literals" => {
                let v = need(&args, &mut i, a)?;
                opts.lint_abs = parse_lint_level(&v)?;
            }
            "--lint-short-path-literals" => {
                let v = need(&args, &mut i, a)?;
                opts.lint_short = parse_lint_level(&v)?;
            }
            "-" => opts.read_stdin = true,
            s if s.starts_with('-') => {
                return Err(format!("unrecognised flag '{s}'"));
            }
            s => opts.files.push(s.to_string()),
        }
        i += 1;
    }
    Ok(opts)
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Evaluation can recurse deeply (max-call-depth 10000); run on a thread
    // with a large stack, like C++ Nix's stack growth.
    let child = std::thread::Builder::new()
        .stack_size(1 << 29)
        .spawn(move || run(opts))
        .expect("spawn eval thread");
    child.join().unwrap_or(ExitCode::FAILURE)
}

fn run(opts: Options) -> ExitCode {
    if opts.parse_only {
        return run_parse(&opts);
    }
    if !opts.eval {
        // Lint diagnostics fire at parse time regardless of the output mode.
        // Support fatal lint flags (e.g. `--lint-absolute-path-literals fatal`)
        // without `--eval`, matching nix-instantiate's default mode.
        let lint_on = opts.lint_url != LintLevel::Allow
            || opts.lint_abs != LintLevel::Allow
            || opts.lint_short != LintLevel::Allow;
        if lint_on {
            if let Some(file) = opts.files.first() {
                let path = if file.starts_with('/') {
                    PathBuf::from(file)
                } else {
                    PathBuf::from(cwd_string()).join(file)
                };
                if let Ok(source) = std::fs::read(&path) {
                    let mut vm = VM::new(SymbolTable::new(), PosTable::new());
                    let origin = Origin::Path {
                        path: path.to_string_lossy().into_owned(),
                        source: source.clone(),
                    };
                    if let Some(code) = lint_scan(&mut vm, &source, origin, &opts) {
                        return code;
                    }
                }
            }
        }
        // Default (instantiate) mode: nix-instantiate still evaluates the
        // expression (an error surfaces during evaluation). jinx has no
        // derivation-building backend, but evaluation-failure tests only need
        // the error, so route through the evaluator.
        if !opts.files.is_empty() || opts.read_stdin {
            return run_eval(opts);
        }
        eprintln!("error: only --parse and --eval are supported in this milestone");
        return ExitCode::FAILURE;
    }
    if opts.json {
        eprintln!("error: --json output is not implemented in jinx yet");
        return ExitCode::FAILURE;
    }
    run_eval(opts)
}

fn run_parse(opts: &Options) -> ExitCode {
    if !opts.read_stdin || !opts.files.is_empty() {
        eprintln!("error: only '--parse -' is supported in this milestone");
        return ExitCode::FAILURE;
    }
    let mut source = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut source) {
        eprintln!("error: reading stdin: {e}");
        return ExitCode::FAILURE;
    }
    let base_path = cwd_string();
    let home = std::env::var("HOME").ok();
    let mut positions = PosTable::new();
    let origin = Origin::Stdin {
        source: source.clone(),
    };
    let mut warnings = Vec::new();
    let result = parse_and_bind(
        &source,
        origin,
        &base_path,
        home.as_deref(),
        &mut positions,
        &mut warnings,
    );
    for w in &warnings {
        write_stderr_line(&jinx_syntax::error::filter_ansi_escapes(w));
    }
    match result {
        Ok(res) => {
            let mut out = show::show(&res.exprs, &res.symbols, res.root);
            out.push(b'\n');
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = lock.write_all(&out);
            let _ = lock.flush();
            ExitCode::SUCCESS
        }
        Err(e) => {
            write_stderr_line(&e.render(&positions));
            ExitCode::FAILURE
        }
    }
}

fn cwd_string() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into())
}

fn current_system() -> Vec<u8> {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    };
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    };
    format!("{arch}-{os}").into_bytes()
}

/// Parse a search-path entry ("prefix=path" or "path").
fn search_path_entry(s: &str) -> (Vec<u8>, Vec<u8>) {
    match s.split_once('=') {
        Some((prefix, path)) => (prefix.as_bytes().to_vec(), abs_path(path)),
        None => (Vec::new(), abs_path(s)),
    }
}

fn abs_path(p: &str) -> Vec<u8> {
    if p.starts_with('/') {
        p.as_bytes().to_vec()
    } else {
        let mut base = cwd_string().into_bytes();
        base.push(b'/');
        base.extend_from_slice(p.as_bytes());
        jinx_eval::vm::canon_path(&base)
    }
}

/// Collect experimental features from the nix.conf pointed to by NIX_CONF_DIR
/// (following `include`/`!include` directives), reading
/// `experimental-features` and `extra-experimental-features`.
fn nix_conf_experimental_features() -> Vec<String> {
    let mut out = Vec::new();
    let dir = match std::env::var("NIX_CONF_DIR") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => return out,
    };
    fn read_conf(path: &std::path::Path, out: &mut Vec<String>, depth: usize) {
        if depth > 10 {
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let base = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("include ") {
                read_conf(&base.join(rest.trim()), out, depth + 1);
                continue;
            }
            if let Some(rest) = line.strip_prefix("!include ") {
                read_conf(&base.join(rest.trim()), out, depth + 1);
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                if k == "experimental-features" || k == "extra-experimental-features" {
                    for f in v.split_whitespace() {
                        out.push(f.to_string());
                    }
                }
            }
        }
    }
    read_conf(&dir.join("nix.conf"), &mut out, 0);
    out
}

/// Instantiate-mode derivation validation (a subset of C++ `getDerivations` +
/// `PackageInfo::queryDrvPath`): if the value is a derivation, coerce its
/// `drvPath` to a store path and require it names a `.drv`.
fn instantiate_check(vm: &mut VM, v: VRef) -> Result<(), ErrId> {
    let vv = val(v);
    if vv.tag() != Tag::Attrs {
        return Ok(());
    }
    // A derivation is an attrset whose `type` is the string "derivation".
    let is_drv = match attrs_get(&vv, vm.syms.type_) {
        Some(a) => {
            vm.force(a.val, NO_POS)?;
            let tv = val(a.val);
            tv.tag() == Tag::String && str_bytes(&tv) == b"derivation"
        }
        None => false,
    };
    if !is_drv {
        return Ok(());
    }
    if let Some(a) = attrs_get(&vv, vm.syms.drv_path) {
        let dpos = PosIdx(a.pos);
        let (s, _ctx) = vm.coerce_to_string(
            a.val,
            dpos,
            "while evaluating the 'drvPath' attribute of a derivation",
            false,
            false,
            false,
        )?;
        if let Err(e) = require_derivation(vm, &s) {
            vm.add_trace(e, dpos, "while evaluating the 'drvPath' attribute of a derivation");
            return Err(e);
        }
    }
    Ok(())
}

/// Port of `StorePath::requireDerivation`: extract the store-path base name and
/// error if its name part does not end in `.drv`.
fn require_derivation(vm: &mut VM, path: &[u8]) -> Result<(), ErrId> {
    let store_dir = vm.store_dir.clone();
    // Strip the store dir prefix; if absent, coerceToStorePath would have
    // raised a different error already — nothing to validate here.
    let rest = if path.starts_with(&store_dir) && path.get(store_dir.len()) == Some(&b'/') {
        &path[store_dir.len() + 1..]
    } else {
        return Ok(());
    };
    let base: &[u8] = match rest.iter().position(|&c| c == b'/') {
        Some(i) => &rest[..i],
        None => rest,
    };
    // StorePath::name() = base name after the 32-char hash and its '-'.
    let name: &[u8] = if base.len() > 33 && base[32] == b'-' {
        &base[33..]
    } else {
        base
    };
    if !name.ends_with(b".drv") {
        let msg = format!(
            "store path '{}' is not a valid derivation path",
            String::from_utf8_lossy(base)
        );
        return Err(vm.new_err(ErrKind::Eval, msg, NO_POS));
    }
    Ok(())
}

fn run_eval(opts: Options) -> ExitCode {
    let symbols = SymbolTable::new();
    let positions = PosTable::new();
    let mut vm = VM::new(symbols, positions);
    vm.current_system = current_system();
    if let Ok(sd) = std::env::var("NIX_STORE_DIR") {
        vm.store_dir = sd.into_bytes();
    }
    vm.pure_eval = opts.pure_eval;
    if let Some(d) = opts.max_call_depth {
        vm.max_call_depth = d;
    }
    vm.show_trace = opts.show_trace;
    vm.trace_verbose = opts.trace_verbose;
    vm.abort_on_warn = opts.abort_on_warn;
    for f in nix_conf_experimental_features() {
        vm.experimental.enable(&f);
    }
    for f in &opts.experimental {
        vm.experimental.enable(f);
    }

    // Search path: -I entries first, then NIX_PATH.
    for e in &opts.include_paths {
        let ent = search_path_entry(e);
        vm.search_path.push(ent);
    }
    if let Ok(np) = std::env::var("NIX_PATH") {
        for e in np.split(':') {
            if !e.is_empty() {
                vm.search_path.push(search_path_entry(e));
            }
        }
    }

    builtins::register_globals(&mut vm);

    // Auto args (lazy thunks for --arg, strings for --argstr).
    let mut auto_args: Vec<(Symbol, VRef)> = Vec::new();
    for (name, value, is_string) in &opts.auto_args {
        let sym = vm.symbols.create(name.as_bytes());
        let cell = if *is_string {
            let v = vm.new_string_value(value.as_bytes(), std::ptr::null_mut());
            let c = vm.alloc_cell(v);
            vm.temp_roots.push(c);
            c
        } else {
            match compile_expr_thunk(&mut vm, value.as_bytes()) {
                Ok(c) => {
                    vm.temp_roots.push(c);
                    c
                }
                Err(rendered) => {
                    write_stderr_line(&rendered);
                    return ExitCode::FAILURE;
                }
            }
        };
        auto_args.push((sym, cell));
    }

    // Obtain the root value.
    let root: Result<VRef, ErrId> = if opts.read_stdin {
        let mut source = Vec::new();
        if let Err(e) = std::io::stdin().read_to_end(&mut source) {
            eprintln!("error: reading stdin: {e}");
            return ExitCode::FAILURE;
        }
        eval_source(
            &mut vm,
            &source,
            Origin::Stdin {
                source: source.clone(),
            },
            &opts,
            true,
        )
    } else if opts.from_args {
        let Some(expr) = opts.files.first() else {
            eprintln!("error: -E requires an expression");
            return ExitCode::FAILURE;
        };
        let source = expr.clone().into_bytes();
        eval_source(
            &mut vm,
            &source,
            Origin::String {
                source: source.clone(),
            },
            &opts,
            false,
        )
    } else {
        let file = opts
            .files
            .first()
            .cloned()
            .unwrap_or_else(|| "./default.nix".to_string());
        let path = if file.starts_with('/') {
            PathBuf::from(file)
        } else {
            PathBuf::from(cwd_string()).join(file)
        };
        // Lint scan on the file source.
        if let Ok(source) = std::fs::read(&path) {
            let origin = Origin::Path {
                path: path.to_string_lossy().into_owned(),
                source: source.clone(),
            };
            if let Some(code) = lint_scan(&mut vm, &source, origin, &opts) {
                return code;
            }
        }
        builtins::eval_file_traced(&mut vm, &path, NO_POS, false)
    };

    let root = match root {
        Ok(c) => c,
        Err(e) => {
            report_err(&vm, e);
            return ExitCode::FAILURE;
        }
    };

    // Process each attr path (default: [""]).
    let attr_paths = if opts.attr_paths.is_empty() {
        vec![String::new()]
    } else {
        opts.attr_paths.clone()
    };

    let stdout = std::io::stdout();
    let mut printed_gc_warning = false;
    for ap in &attr_paths {
        let v = match find_along_attr_path(&mut vm, root, ap, &auto_args) {
            Ok(v) => v,
            Err(e) => {
                report_err(&vm, e);
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = vm.force(v, NO_POS) {
            report_err(&vm, e);
            return ExitCode::FAILURE;
        }
        // Top-level auto-call (autoCallFunction; a no-op unless the value is a
        // lambda/functor whose arguments can be auto-supplied).
        let v = match auto_call(&mut vm, &auto_args, v) {
            Ok(c) => c,
            Err(e) => {
                report_err(&vm, e);
                return ExitCode::FAILURE;
            }
        };
        // Instantiate mode (no --eval): nix-instantiate collects derivations
        // and validates each `drvPath`. jinx has no build backend, but the
        // validation surfaces real errors (e.g. a manually-set, non-`.drv`
        // drvPath), which is what `non-eval-fail-bad-drvPath` exercises.
        if !opts.eval {
            if let Err(e) = instantiate_check(&mut vm, v) {
                report_err(&vm, e);
                return ExitCode::FAILURE;
            }
            // Collect derivations (port of `getDerivations`) and print each
            // `drvPath`, like nix-instantiate's default mode.
            let mut drv_paths: Vec<Vec<u8>> = Vec::new();
            if let Err(e) = get_derivations(&mut vm, v, true, &mut drv_paths) {
                report_err(&vm, e);
                return ExitCode::FAILURE;
            }
            // C++ prints the GC-root warning to stderr once per run in
            // read-only/no-add-root mode (not captured by the gate's stdout).
            if !printed_gc_warning {
                write_stderr_line(
                    b"warning: you did not specify '--add-root'; the result might be removed by the garbage collector",
                );
                printed_gc_warning = true;
            }
            let mut lock = stdout.lock();
            for p in &drv_paths {
                let _ = lock.write_all(p);
                let _ = lock.write_all(b"\n");
            }
            let _ = lock.flush();
            continue;
        }
        if opts.strict && !opts.xml {
            if let Err(e) = print::deep_force(&mut vm, v) {
                report_err(&vm, e);
                return ExitCode::FAILURE;
            }
        }
        let mut out = Vec::new();
        if opts.xml {
            let mut ctx = Vec::new();
            if let Err(e) =
                jinx_eval::xml::value_to_xml(&mut vm, v, opts.strict, opts.location, &mut out, &mut ctx)
            {
                report_err(&vm, e);
                return ExitCode::FAILURE;
            }
        } else {
            if let Err(e) = print::print_ambiguous(&mut vm, v, &mut out) {
                report_err(&vm, e);
                return ExitCode::FAILURE;
            }
            out.push(b'\n');
        }
        let mut lock = stdout.lock();
        let _ = lock.write_all(&out);
        let _ = lock.flush();
    }
    ExitCode::SUCCESS
}

fn eval_source(
    vm: &mut VM,
    source: &[u8],
    origin: Origin,
    opts: &Options,
    _stdin: bool,
) -> Result<VRef, ErrId> {
    if let Some(_code) = lint_scan_err(vm, source, origin.clone(), opts) {
        // Fatal lint: reported already; surface as generic error.
        return Err(vm.new_err(ErrKind::Eval, "lint error", NO_POS));
    }
    let base_path = cwd_string();
    let home = std::env::var("HOME").ok();
    let mut warnings = Vec::new();
    let parsed = parse_and_bind_with(
        source,
        origin,
        &base_path,
        home.as_deref(),
        &mut vm.positions,
        &mut vm.symbols,
        &mut warnings,
    );
    for w in &warnings {
        write_stderr_line(&jinx_syntax::error::filter_ansi_escapes(w));
    }
    let (exprs, root) = match parsed {
        Ok(r) => r,
        Err(pe) => {
            write_stderr_line(&pe.render(&vm.positions));
            std::process::exit(1);
        }
    };
    let prog = jinx_eval::compile::compile_program(
        &exprs,
        root,
        &vm.symbols,
        &vm.globals,
        vm.empty_list_cell,
    );
    vm.run_program(prog)
}

/// Compile `--arg` expression into a lazy thunk cell.
fn compile_expr_thunk(vm: &mut VM, source: &[u8]) -> Result<VRef, Vec<u8>> {
    let base_path = cwd_string();
    let home = std::env::var("HOME").ok();
    let mut warnings = Vec::new();
    let parsed = parse_and_bind_with(
        source,
        Origin::String {
            source: source.to_vec(),
        },
        &base_path,
        home.as_deref(),
        &mut vm.positions,
        &mut vm.symbols,
        &mut warnings,
    );
    let (exprs, root) = parsed.map_err(|pe| pe.render(&vm.positions))?;
    let prog = jinx_eval::compile::compile_program(
        &exprs,
        root,
        &vm.symbols,
        &vm.globals,
        vm.empty_list_cell,
    );
    let code = prog.code_ref(0) as *const _ as *const ();
    vm.gc_check();
    let v = vm.heap.new_thunk(Tag::Thunk, code, &[]);
    Ok(vm.alloc_cell(v))
}

/// Port of `getDerivations` (get-drvs.cc): collect the `drvPath`s reachable
/// from `v`. A derivation (`type == "derivation"`) yields its own drvPath;
/// otherwise `v` is an attrset/list recursed into — at the top level always,
/// nested only through attrs whose `recurseForDerivations == true`.
fn get_derivations(
    vm: &mut VM,
    v: VRef,
    top: bool,
    out: &mut Vec<Vec<u8>>,
) -> Result<(), ErrId> {
    vm.force(v, NO_POS)?;
    let vv = val(v);
    match vv.tag() {
        Tag::Attrs => {
            let is_drv = matches!(attrs_get(&vv, vm.syms.type_), Some(a) if {
                vm.force(a.val, NO_POS)?;
                let tv = val(a.val);
                tv.tag() == Tag::String && str_bytes(&tv) == b"derivation"
            });
            if is_drv {
                if let Some(a) = attrs_get(&vv, vm.syms.drv_path) {
                    let (s, _c) = vm.coerce_to_string(
                        a.val,
                        NO_POS,
                        "while evaluating the 'drvPath' attribute of a derivation",
                        false,
                        false,
                        false,
                    )?;
                    out.push(s);
                }
                return Ok(());
            }
            // Not a derivation: recurse at the top level, or when the set opts
            // in via `recurseForDerivations = true`.
            let recurse = top
                || matches!(attrs_get(&vv, vm.symbols.create(b"recurseForDerivations")), Some(a) if {
                    vm.force(a.val, NO_POS)?;
                    val(a.val).tag() == Tag::True
                });
            if !recurse {
                return Ok(());
            }
            // Attrs are stored sorted by symbol; C++ getDerivations orders by
            // attribute name. Re-sort by name bytes to match.
            let entries = jinx_eval::vm::attrs_entries(&vv).to_vec();
            let mut named: Vec<(Vec<u8>, VRef)> = entries
                .iter()
                .map(|a| (vm.symbols.resolve(Symbol(a.sym)).to_vec(), a.val))
                .collect();
            named.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, cell) in named {
                get_derivations(vm, cell, false, out)?;
            }
            Ok(())
        }
        Tag::List => {
            let elems = list_elems(&vv).to_vec();
            for e in elems {
                get_derivations(vm, e, false, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Port of autoCallFunction.
fn auto_call(vm: &mut VM, args: &[(Symbol, VRef)], fun: VRef) -> Result<VRef, ErrId> {
    vm.force(fun, NO_POS)?;
    let v = val(fun);
    if v.tag() == Tag::Attrs {
        if let Some(f) = attrs_get(&v, vm.syms.functor) {
            let r = vm.call_function(f.val, &[fun], NO_POS)?;
            let rc = vm.alloc_cell(r);
            vm.temp_roots.push(rc);
            vm.force(rc, NO_POS)?;
            return auto_call(vm, args, rc);
        }
    }
    if v.tag() != Tag::Closure {
        return Ok(fun);
    }
    let (code, _) = thunk_code(&v);
    let chunk = code.chunk();
    let Some(spec) = &chunk.lambda else {
        return Ok(fun);
    };
    let Some(formals) = &spec.formals else {
        return Ok(fun);
    };
    let mut entries: Vec<jinx_eval::value::Attr> = Vec::new();
    if formals.ellipsis {
        for (sym, cell) in args {
            entries.push(jinx_eval::value::Attr {
                sym: sym.0,
                pos: 0,
                val: *cell,
            });
        }
    } else {
        for f in &formals.formals {
            if let Some((_, cell)) = args.iter().find(|(s, _)| *s == f.name) {
                entries.push(jinx_eval::value::Attr {
                    sym: f.name.0,
                    pos: 0,
                    val: *cell,
                });
            } else if f.default.is_none() {
                let name = String::from_utf8_lossy(vm.symbols.resolve(f.name)).into_owned();
                let msg = format!(
                    "cannot evaluate a function that has an argument without a value ('{name}')\n\
Nix attempted to evaluate a function as a top level expression; in\n\
this case it must have its arguments supplied either by default\n\
values, or passed explicitly with '--arg' or '--argstr'. See\n\
https://nix.dev/manual/nix/stable/language/syntax.html#functions.",
                );
                return Err(vm.new_err(ErrKind::MissingArgument, msg, f.pos));
            }
        }
    }
    entries.sort_by_key(|a| a.sym);
    entries.dedup_by_key(|a| a.sym);
    let attrs = vm.new_bindings_value(&entries);
    let ac = vm.alloc_cell(attrs);
    vm.temp_roots.push(ac);
    let r = vm.call_function(fun, &[ac], NO_POS)?;
    let rc = vm.alloc_cell(r);
    vm.temp_roots.push(rc);
    Ok(rc)
}

/// Port of findAlongAttrPath (attr-path.cc), sufficient for the harness.
fn find_along_attr_path(
    vm: &mut VM,
    root: VRef,
    attr_path: &str,
    auto_args: &[(Symbol, VRef)],
) -> Result<VRef, ErrId> {
    let mut tokens: Vec<String> = Vec::new();
    if !attr_path.is_empty() {
        for t in attr_path.split('.') {
            tokens.push(t.to_string());
        }
    }
    let mut v = root;
    for t in &tokens {
        // Auto-call before each selection (C++ findAlongAttrPath always runs
        // autoCallFunction; with no `--arg`s it still applies a lambda whose
        // formals all have defaults / an ellipsis, e.g. nixpkgs' `import ./.`).
        v = auto_call(vm, auto_args, v)?;
        vm.force(v, NO_POS)?;
        let cur = val(v);
        if let Ok(n) = t.parse::<usize>() {
            if cur.tag() == Tag::List {
                let elems = list_elems(&cur);
                if n >= elems.len() {
                    let msg = format!(
                        "list index {} in selection path '{}' is out of range",
                        n, attr_path
                    );
                    return Err(vm.new_err(ErrKind::Eval, msg, NO_POS));
                }
                v = elems[n];
                continue;
            }
        }
        if cur.tag() != Tag::Attrs {
            let msg = format!(
                "the expression selected by the selection path '{}' should be a set but is {}",
                attr_path,
                vm.show_type(&cur)
            );
            return Err(vm.new_err(ErrKind::Type, msg, NO_POS));
        }
        let sym = vm.symbols.create(t.as_bytes());
        match attrs_get(&cur, sym) {
            Some(a) => v = a.val,
            None => {
                let msg = format!(
                    "attribute '{}' in selection path '{}' not found",
                    t, attr_path
                );
                return Err(vm.new_err(ErrKind::Eval, msg, NO_POS));
            }
        }
    }
    Ok(v)
}

fn report_err(vm: &VM, e: ErrId) {
    let err = &vm.errors[e as usize];
    write_stderr_line(&err.render_with(&vm.positions, vm.show_trace));
    // A UsageError triggers the arg-parser's help hint (main.cc), printed at
    // column 0 after the error block.
    if err.kind == ErrKind::Usage {
        write_stderr_line(b"Try 'nix-instantiate --help' for more information.");
    }
}

// ---------------- lint warnings ----------------

fn lint_scan(vm: &mut VM, source: &[u8], origin: Origin, opts: &Options) -> Option<ExitCode> {
    lint_scan_err(vm, source, origin, opts).map(|_| ExitCode::FAILURE)
}

/// Returns Some(()) if a fatal lint fired (error already printed).
fn lint_scan_err(vm: &mut VM, source: &[u8], origin: Origin, opts: &Options) -> Option<()> {
    if opts.lint_url == LintLevel::Allow
        && opts.lint_abs == LintLevel::Allow
        && opts.lint_short == LintLevel::Allow
    {
        return None;
    }
    let origin_id = vm.positions.add_origin(origin, source.len());
    let mut lexer = Lexer::new(source, origin_id);
    loop {
        let tok = match lexer.next_token(&vm.positions) {
            Ok(t) => t,
            Err(_) => break,
        };
        match tok.kind {
            TokKind::Eof => break,
            TokKind::Uri => {
                if opts.lint_url != LintLevel::Allow {
                    // C++ varies the wording by severity (parser.y).
                    let word = if opts.lint_url == LintLevel::Fatal {
                        "disallowed"
                    } else {
                        "discouraged"
                    };
                    let msg = format!(
                        "URL literals are {word}. Consider using a string literal \"{}\" instead (lint-url-literals)",
                        String::from_utf8_lossy(&tok.text)
                    );
                    if emit_lint(vm, &msg, tok.begin, origin_id, opts.lint_url) {
                        return Some(());
                    }
                }
            }
            TokKind::HPath => {
                if opts.lint_abs != LintLevel::Allow {
                    let msg = format!(
                        "home path literals are not portable. Consider replacing path literal '{}' by a string, relative path, or parameter (lint-absolute-path-literals)",
                        String::from_utf8_lossy(&tok.text)
                    );
                    if emit_lint(vm, &msg, tok.begin, origin_id, opts.lint_abs) {
                        return Some(());
                    }
                }
            }
            TokKind::Path => {
                let text = tok.text.clone();
                if text.starts_with(b"/") {
                    if opts.lint_abs != LintLevel::Allow {
                        let msg = format!(
                            "absolute path literals are not portable. Consider replacing path literal '{}' by a string, relative path, or parameter (lint-absolute-path-literals)",
                            String::from_utf8_lossy(&text)
                        );
                        if emit_lint(vm, &msg, tok.begin, origin_id, opts.lint_abs) {
                            return Some(());
                        }
                    }
                } else if !text.starts_with(b"./") && !text.starts_with(b"../") {
                    if opts.lint_short != LintLevel::Allow {
                        let msg = format!(
                            "relative path literal '{}' should be prefixed with '.' for clarity: './{}' (lint-short-path-literals)",
                            String::from_utf8_lossy(&text),
                            String::from_utf8_lossy(&text)
                        );
                        if emit_lint(vm, &msg, tok.begin, origin_id, opts.lint_short) {
                            return Some(());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Render a lint message; returns true when fatal.
fn emit_lint(
    vm: &VM,
    msg: &str,
    begin: u32,
    origin_id: jinx_syntax::pos::OriginId,
    level: LintLevel,
) -> bool {
    let pos = vm.positions.add(origin_id, begin);
    let rendered =
        jinx_syntax::ParseError::new(msg.as_bytes().to_vec(), pos).render(&vm.positions);
    let out = if level == LintLevel::Warn {
        rerender_as_warning(&rendered)
    } else {
        rendered
    };
    write_stderr_line(&out);
    level == LintLevel::Fatal
}

/// Turn an "error: ..."-rendered block into a "warning: ..." block
/// (prefix swap plus two extra columns of continuation indent).
fn rerender_as_warning(rendered: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rendered.len() + 16);
    for (i, line) in rendered.split(|&b| b == b'\n').enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        if i == 0 {
            if let Some(rest) = line.strip_prefix(b"error: ".as_slice()) {
                out.extend_from_slice(b"warning: ");
                out.extend_from_slice(rest);
                continue;
            }
        } else if !line.is_empty() {
            out.extend_from_slice(b"  ");
        }
        out.extend_from_slice(line);
    }
    out
}

fn write_stderr_line(bytes: &[u8]) {
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_all(bytes);
    let _ = lock.write_all(b"\n");
}
