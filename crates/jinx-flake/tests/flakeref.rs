//! Flake-reference parse/render round-trip tests.

use jinx_fetch::attrs::maybe_get_str;
use jinx_flake::flakeref::FlakeRef;

#[test]
fn indirect_bare() {
    let r = FlakeRef::parse("nixpkgs").unwrap();
    assert_eq!(r.ty(), Some("indirect"));
    assert_eq!(maybe_get_str(&r.attrs, "id"), Some("nixpkgs"));
    assert_eq!(r.to_url().unwrap(), "flake:nixpkgs");
}

#[test]
fn indirect_with_ref_and_rev() {
    let r = FlakeRef::parse("nixpkgs/nixos-24.05").unwrap();
    assert_eq!(maybe_get_str(&r.attrs, "ref"), Some("nixos-24.05"));
    assert!(maybe_get_str(&r.attrs, "rev").is_none());

    let rev = "abcdef0123456789abcdef0123456789abcdef01";
    let r2 = FlakeRef::parse(&format!("nixpkgs/{rev}")).unwrap();
    assert_eq!(maybe_get_str(&r2.attrs, "rev"), Some(rev));
    assert!(maybe_get_str(&r2.attrs, "ref").is_none());

    let r3 = FlakeRef::parse(&format!("nixpkgs/release-23.11/{rev}")).unwrap();
    assert_eq!(maybe_get_str(&r3.attrs, "ref"), Some("release-23.11"));
    assert_eq!(maybe_get_str(&r3.attrs, "rev"), Some(rev));
}

#[test]
fn github_owner_repo() {
    let r = FlakeRef::parse("github:NixOS/nixpkgs").unwrap();
    assert_eq!(r.ty(), Some("github"));
    assert_eq!(maybe_get_str(&r.attrs, "owner"), Some("NixOS"));
    assert_eq!(maybe_get_str(&r.attrs, "repo"), Some("nixpkgs"));
    assert_eq!(r.to_url().unwrap(), "github:NixOS/nixpkgs");
}

#[test]
fn github_with_ref_rev_host_dir() {
    let r = FlakeRef::parse("github:NixOS/nixpkgs/nixos-24.05?host=example.com&dir=sub").unwrap();
    assert_eq!(maybe_get_str(&r.attrs, "ref"), Some("nixos-24.05"));
    assert_eq!(maybe_get_str(&r.attrs, "host"), Some("example.com"));
    assert_eq!(r.subdir, "sub");
    // Round-trip through attrs.
    let attrs = r.to_attrs();
    let r2 = FlakeRef::from_attrs(attrs);
    assert_eq!(r2, r);

    let rev = "abcdef0123456789abcdef0123456789abcdef01";
    let r3 = FlakeRef::parse(&format!("github:NixOS/nixpkgs/{rev}")).unwrap();
    assert_eq!(maybe_get_str(&r3.attrs, "rev"), Some(rev));
}

#[test]
fn path_uri_and_plain() {
    let r = FlakeRef::parse("path:/home/u/proj?dir=sub").unwrap();
    assert_eq!(r.ty(), Some("path"));
    assert_eq!(maybe_get_str(&r.attrs, "path"), Some("/home/u/proj"));
    assert_eq!(r.subdir, "sub");

    let r2 = FlakeRef::parse("/absolute/flake").unwrap();
    assert_eq!(r2.ty(), Some("path"));
    assert_eq!(maybe_get_str(&r2.attrs, "path"), Some("/absolute/flake"));

    let r3 = FlakeRef::parse("./relative/dir").unwrap();
    assert_eq!(maybe_get_str(&r3.attrs, "path"), Some("./relative/dir"));
}

#[test]
fn git_and_tarball_urls() {
    let r = FlakeRef::parse("git+https://example.com/repo.git?ref=main").unwrap();
    assert_eq!(r.ty(), Some("git"));
    assert_eq!(maybe_get_str(&r.attrs, "url"), Some("git+https://example.com/repo.git"));
    assert_eq!(maybe_get_str(&r.attrs, "ref"), Some("main"));

    let t = FlakeRef::parse("https://example.com/src.tar.gz").unwrap();
    assert_eq!(t.ty(), Some("tarball"));
    assert_eq!(maybe_get_str(&t.attrs, "url"), Some("https://example.com/src.tar.gz"));

    let f = FlakeRef::parse("https://example.com/plain").unwrap();
    assert_eq!(f.ty(), Some("file"));
}

#[test]
fn attrs_roundtrip_all_types() {
    for s in [
        "nixpkgs",
        "github:NixOS/nixpkgs",
        "path:/x/y?dir=z",
        "git+https://h/r.git?ref=main",
    ] {
        let r = FlakeRef::parse(s).unwrap();
        let r2 = FlakeRef::from_attrs(r.to_attrs());
        assert_eq!(r, r2, "roundtrip failed for {s}");
    }
}
