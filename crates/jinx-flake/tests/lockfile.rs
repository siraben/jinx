//! Lock-file parsing / round-trip tests.
//!
//! `fixtures/nix-flake.lock` is a copy of the real `flake.lock` from the C++
//! Nix checkout (version 7). `fixtures/v7-relative.lock` is a hand-made v7 lock
//! with a relative `path:` input carrying a `parent` field and a `follows`
//! input.

use jinx_flake::lockfile::{Edge, LockFile};

const NIX_LOCK: &str = include_str!("fixtures/nix-flake.lock");
const V7_RELATIVE: &str = include_str!("fixtures/v7-relative.lock");

#[test]
fn parse_real_nix_lock() {
    let lf = LockFile::parse(NIX_LOCK).unwrap();
    assert_eq!(lf.version, 7);
    assert_eq!(lf.root, "root");
    // The root node has inputs but no lock info.
    assert!(lf.root_node().locked.is_none());
    assert!(!lf.root_node().inputs.is_empty());
    // flake-compat is marked flake=false.
    let fc = lf.node("flake-compat").expect("flake-compat node");
    assert!(!fc.locked.as_ref().unwrap().is_flake);
}

#[test]
fn nix_lock_roundtrips() {
    let lf = LockFile::parse(NIX_LOCK).unwrap();
    let original: serde_json::Value = serde_json::from_str(NIX_LOCK).unwrap();
    assert_eq!(lf.to_json(), original, "lock file did not round-trip");
}

#[test]
fn v7_relative_parent_and_follows() {
    let lf = LockFile::parse(V7_RELATIVE).unwrap();
    assert_eq!(lf.version, 7);

    let sub = lf.node("sub").expect("sub node");
    let locked = sub.locked.as_ref().unwrap();
    // Relative path input carries a parent attr path.
    assert_eq!(locked.parent.as_deref(), Some(&["sub".to_string()][..]));
    assert_eq!(locked.locked.ty(), Some("path"));

    // sub's nixpkgs input `follows` the root's nixpkgs.
    match sub.inputs.get("nixpkgs").unwrap() {
        Edge::Follows(path) => assert_eq!(path, &["nixpkgs".to_string()]),
        other => panic!("expected follows, got {other:?}"),
    }

    // Resolving the follows from sub yields the root's nixpkgs node.
    let resolved = lf.get_input_by_path("sub", &["nixpkgs".to_string()]).unwrap();
    assert_eq!(resolved, "nixpkgs");
    // Round-trips.
    let original: serde_json::Value = serde_json::from_str(V7_RELATIVE).unwrap();
    assert_eq!(lf.to_json(), original);
}

#[test]
fn rejects_bad_version() {
    let bad = r#"{"version": 4, "root": "root", "nodes": {"root": {}}}"#;
    assert!(LockFile::parse(bad).is_err());
}
