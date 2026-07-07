//! Flake evaluation tests: `builtins.getFlake` + the call-flake bootstrap on a
//! locked multi-input fixture. The fixture (a composite flake whose `flake.lock`
//! pins a second local path flake as a parent-relative input) is copied to a
//! temp directory outside any git repo so it is fetched via the `path` scheme.

use std::path::{Path, PathBuf};

use jinx_eval::builtins;
use jinx_eval::value::Tag;
use jinx_eval::vm::{attrs_get, str_bytes, val, VM};
use jinx_syntax::pos::NO_POS;
use jinx_syntax::{PosTable, SymbolTable};

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

/// Evaluate `<flakeref>#<attr>` (single dotted attr) and return the string value.
fn eval_flake_attr(flakeref: String, attr: String) -> Result<String, String> {
    std::thread::Builder::new()
        .stack_size(1 << 29)
        .spawn(move || {
            let symbols = SymbolTable::new();
            let positions = PosTable::new();
            let mut vm = VM::new(symbols, positions);
            vm.experimental.flakes = true;
            builtins::register_globals(&mut vm);
            let cell = jinx_eval::flake::get_flake(&mut vm, &flakeref, NO_POS)
                .map_err(|e| String::from_utf8_lossy(&vm.errors[e as usize].msg).into_owned())?;
            let mut v = cell;
            for comp in attr.split('.') {
                vm.force(v, NO_POS)
                    .map_err(|e| String::from_utf8_lossy(&vm.errors[e as usize].msg).into_owned())?;
                let cur = val(v);
                assert_eq!(cur.tag(), Tag::Attrs, "expected attrs at '{comp}'");
                let sym = vm.symbols.create(comp.as_bytes());
                v = attrs_get(&cur, sym)
                    .ok_or_else(|| format!("attribute '{comp}' missing"))?
                    .val;
            }
            vm.force(v, NO_POS)
                .map_err(|e| String::from_utf8_lossy(&vm.errors[e as usize].msg).into_owned())?;
            let fv = val(v);
            assert_eq!(fv.tag(), Tag::String);
            Ok(String::from_utf8_lossy(str_bytes(&fv)).into_owned())
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn composite_locked_multi_input() {
    // Locate the committed fixture and copy it outside any git repo.
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("jinx-flake")
        .join("tests")
        .join("fixtures")
        .join("composite");
    assert!(fixture.join("flake.lock").exists(), "fixture missing: {fixture:?}");

    let tmp = std::env::temp_dir().join(format!("jinx-composite-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy_dir(&fixture, &tmp);

    let flakeref = format!("path:{}", tmp.display());
    let got = eval_flake_attr(flakeref, "combined".to_string());
    let _ = std::fs::remove_dir_all(&tmp);

    // Matches the oracle: `nix eval path:<copy>#combined` => "parent+child-42".
    assert_eq!(got.unwrap(), "parent+child-42");
}
