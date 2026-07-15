use std::path::{Path, PathBuf};

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

#[test]
fn equivalent_locked_inputs_share_one_evaluation_and_follows_still_resolves() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("jinx-flake")
        .join("tests")
        .join("fixtures")
        .join("equivalent-inputs");
    let tmp = std::env::temp_dir().join(format!(
        "jinx-equivalent-inputs-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    copy_dir(&fixture, &tmp);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_jinx"))
        .args([
            "eval",
            "--extra-experimental-features",
            "nix-command flakes",
            "--raw",
            &format!("path:{}#combined", tmp.display()),
        ])
        .env("NIX_REMOTE", "dummy://")
        .env("NIX_STORE_DIR", "/nix/store")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout, b"xxx");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("trace: equivalent-input-evaluated").count(),
        1,
        "{stderr}"
    );
}
