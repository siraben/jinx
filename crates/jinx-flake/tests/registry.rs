//! Registry parsing / resolution tests.

use jinx_fetch::attrs::{maybe_get_str, Attr, Attrs};
use jinx_flake::registry::{lookup_in_registries, Registry, RegistryType};

const REGISTRY: &str = include_str!("fixtures/registry.json");

fn indirect(id: &str) -> Attrs {
    let mut a = Attrs::new();
    a.insert("type".into(), Attr::Str("indirect".into()));
    a.insert("id".into(), Attr::Str(id.into()));
    a
}

#[test]
fn parse_registry() {
    let reg = Registry::parse(REGISTRY, RegistryType::User).unwrap();
    assert_eq!(reg.entries.len(), 3);
    // The `flake-utils` entry lifts `dir` into extraAttrs.
    let fu = &reg.entries[1];
    assert_eq!(maybe_get_str(&fu.extra_attrs, "dir"), Some("sub"));
    assert!(maybe_get_str(&fu.to, "dir").is_none());
    // The `pinned` entry is exact.
    assert!(reg.entries[2].exact);
}

#[test]
fn prefix_resolution() {
    let reg = Registry::parse(REGISTRY, RegistryType::User).unwrap();
    let (resolved, extra) = lookup_in_registries(&indirect("nixpkgs"), &[reg]).unwrap();
    assert_eq!(maybe_get_str(&resolved, "type"), Some("github"));
    assert_eq!(maybe_get_str(&resolved, "owner"), Some("NixOS"));
    assert_eq!(maybe_get_str(&resolved, "repo"), Some("nixpkgs"));
    assert!(extra.is_empty());
}

#[test]
fn prefix_resolution_carries_ref_override() {
    let reg = Registry::parse(REGISTRY, RegistryType::User).unwrap();
    // `nixpkgs` with a user-specified ref -> resolved github ref = the override.
    let mut q = indirect("nixpkgs");
    q.insert("ref".into(), Attr::Str("nixos-24.05".into()));
    let (resolved, _) = lookup_in_registries(&q, &[reg]).unwrap();
    assert_eq!(maybe_get_str(&resolved, "ref"), Some("nixos-24.05"));
    assert_eq!(maybe_get_str(&resolved, "owner"), Some("NixOS"));
}

#[test]
fn dir_extra_attrs_resolution() {
    let reg = Registry::parse(REGISTRY, RegistryType::User).unwrap();
    let (_, extra) = lookup_in_registries(&indirect("flake-utils"), &[reg]).unwrap();
    assert_eq!(maybe_get_str(&extra, "dir"), Some("sub"));
}

#[test]
fn exact_resolution_requires_full_equality() {
    let reg = Registry::parse(REGISTRY, RegistryType::User).unwrap();
    // Exact entry `pinned` matches only the exact input.
    let (resolved, _) = lookup_in_registries(&indirect("pinned"), &[reg.clone()]).unwrap();
    assert_eq!(maybe_get_str(&resolved, "rev"), Some("0000000000000000000000000000000000000000"));

    // With an extra attr it is NOT an exact match -> stays indirect -> error.
    let mut q = indirect("pinned");
    q.insert("ref".into(), Attr::Str("main".into()));
    assert!(lookup_in_registries(&q, &[reg]).is_err());
}

#[test]
fn unresolved_indirect_errors() {
    let reg = Registry::empty(RegistryType::User);
    assert!(lookup_in_registries(&indirect("unknown"), &[reg]).is_err());
}
