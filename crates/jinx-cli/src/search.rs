//! `jinx search <nixpkgs-path> <query>` — a `nix search`-style command with a
//! hot/cold evaluation cache (see [`crate::eval_cache`]).
//!
//! Cold (empty cache): evaluate the whole package set's `name`/`meta.description`
//! (exactly what a search matches against) and populate the cache in Nix's
//! SQLite schema. Hot (populated cache): read the metadata straight back and
//! skip evaluation entirely — the 10× speed-up `nix search` gets from its eval
//! cache.

use std::process::ExitCode;
use std::time::Instant;

use jinx_eval::vm::VM;
use jinx_syntax::pos::NO_POS;
use jinx_syntax::{PosTable, SymbolTable};

use crate::eval_cache::{Cache, Pkg};

// Walk the package set like `nix search`: descend `recurseForDerivations`
// sets and, for every derivation, emit `path \t FLAG \t name \t desc \n`
// (FLAG: F=eval failed, N=ok no description, D=ok with description).
const WALK: &str = r#"
let
  pkgs = import <nixpkgs> { };
  lib = pkgs.lib;
  recurse = x: builtins.isAttrs x
    && ((builtins.tryEval (x.recurseForDerivations or false)).value or false);
  esc = builtins.replaceStrings [ "\t" "\n" "\r" ] [ " " " " " " ];
  go = prefix: attrs:
    lib.concatStrings (map (name:
      let e = builtins.tryEval (attrs.${name});
          path = prefix + name;
      in if !e.success then path + "\tF\t\t\n"
         else let v = e.value; in
           if (builtins.tryEval (lib.isDerivation v)).value or false then
             let dn = builtins.tryEval (v.name or "");
                 dd = builtins.tryEval (v.meta.description or null);
                 nm = if dn.success then esc dn.value else "";
             in if dd.success && dd.value != null
                then path + "\tD\t" + nm + "\t" + (esc dd.value) + "\n"
                else path + "\tN\t" + nm + "\t\n"
           else if recurse v then go (path + ".") v
           else ""
    ) (builtins.attrNames attrs));
in go "" pkgs
"#;

pub fn run_search(args: Vec<String>) -> ExitCode {
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if positional.len() < 2 {
        eprintln!("usage: jinx search <nixpkgs-path> <query>");
        return ExitCode::FAILURE;
    }
    let nixpkgs = positional[0].clone();
    let query = positional[1].to_lowercase();

    let mut vm = VM::new(SymbolTable::new(), PosTable::new());
    vm.current_system = crate::current_system();
    if let Ok(sd) = std::env::var("NIX_STORE_DIR") {
        vm.store_dir = sd.into_bytes();
    }
    vm.store_mode = crate::select_store_mode(true); // readonly: no daemon writes
    vm.search_path
        .push((b"nixpkgs".to_vec(), nixpkgs.clone().into_bytes()));
    jinx_eval::builtins::register_globals(&mut vm); // import/map/builtins/…

    // Cache key: jinx's own fingerprint of (nixpkgs path, system). (Sharing
    // Nix's exact DB additionally needs LockedFlake::getFingerprint + Nix's
    // AttrCursor tree shape — a follow-up; the on-disk schema is compatible.)
    let key = {
        use jinx_store::hash::{hash_string, HashAlgorithm, HashFormat};
        let mut s = nixpkgs.clone().into_bytes();
        s.push(0);
        s.extend_from_slice(&vm.current_system);
        hash_string(HashAlgorithm::Sha256, &s).to_string(HashFormat::Base16, false)
    };
    let db_path = cache_dir().join(format!("{}.sqlite", &key[..32.min(key.len())]));
    let mut cache = match Cache::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("jinx search: cannot open cache '{}': {e}", db_path.display());
            return ExitCode::FAILURE;
        }
    };

    let t0 = Instant::now();
    let (pkgs, mode) = match cache.root() {
        Ok(Some(root)) => match cache.read_all(root) {
            Ok(p) => (p, "hot (cache)"),
            Err(e) => {
                eprintln!("jinx search: cache read failed: {e}");
                return ExitCode::FAILURE;
            }
        },
        _ => match evaluate(&mut vm) {
            Ok(p) => {
                if let Err(e) = cache.write_all(&p) {
                    eprintln!("jinx search: warning: cache write failed: {e}");
                }
                (p, "cold (evaluated)")
            }
            Err(msg) => {
                eprint!("{}", String::from_utf8_lossy(&msg));
                return ExitCode::FAILURE;
            }
        },
    };

    let mut hits = 0usize;
    for p in &pkgs {
        if p.failed {
            continue;
        }
        let d = p.desc.clone().unwrap_or_default();
        if p.path.to_lowercase().contains(&query)
            || p.name.to_lowercase().contains(&query)
            || d.to_lowercase().contains(&query)
        {
            hits += 1;
            let ver = p.name.rsplit_once('-').map(|(_, v)| v).unwrap_or("");
            println!("* {} ({})", p.path, ver);
            if !d.is_empty() {
                println!("  {}", d);
            }
        }
    }
    eprintln!(
        "jinx search: {mode}, {} packages, {hits} matches in {:.2}s",
        pkgs.len(),
        t0.elapsed().as_secs_f64()
    );
    ExitCode::SUCCESS
}

/// Cold path: evaluate the walk expression to the big TSV string, parse it.
fn evaluate(vm: &mut VM) -> Result<Vec<Pkg>, Vec<u8>> {
    let cell = crate::compile_expr_thunk(vm, WALK.as_bytes())?;
    let bytes = vm
        .force_string(cell, NO_POS, "while evaluating the package set")
        .map_err(|e| vm.errors[e as usize].render_with(&vm.positions, vm.show_trace))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split('\t');
        let (path, flag, name, desc) = (f.next(), f.next(), f.next(), f.next());
        let (Some(path), Some(flag)) = (path, flag) else { continue };
        match flag {
            "F" => out.push(Pkg { path: path.into(), name: String::new(), desc: None, failed: true }),
            "N" => out.push(Pkg { path: path.into(), name: name.unwrap_or("").into(), desc: None, failed: false }),
            "D" => out.push(Pkg {
                path: path.into(),
                name: name.unwrap_or("").into(),
                desc: Some(desc.unwrap_or("").into()),
                failed: false,
            }),
            _ => {}
        }
    }
    Ok(out)
}

fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache")
        });
    base.join("nix").join("jinx-search-v1")
}
