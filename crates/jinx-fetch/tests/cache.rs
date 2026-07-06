//! Fetcher cache round-trip tests.

use jinx_fetch::attrs::{Attr, Attrs};
use jinx_fetch::cache::{Cache, Key};

fn attrs(pairs: &[(&str, &str)]) -> Attrs {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), Attr::Str(v.to_string())))
        .collect()
}

#[test]
fn upsert_and_lookup() {
    let cache = Cache::open_in_memory().unwrap();
    let key = Key::new(
        "sourcePathToHash",
        attrs(&[("fingerprint", "path:sha256-abc"), ("method", "nar"), ("path", "/")]),
    );
    assert_eq!(cache.lookup(&key).unwrap(), None);

    let value = attrs(&[("hash", "sha256-deadbeef")]);
    cache.upsert(&key, &value).unwrap();

    assert_eq!(cache.lookup(&key).unwrap().as_ref(), Some(&value));
    // Fresh entry is not expired under the default TTL.
    assert_eq!(cache.lookup_with_ttl(&key).unwrap().as_ref(), Some(&value));
}

#[test]
fn zero_ttl_is_always_expired() {
    let mut cache = Cache::open_in_memory().unwrap();
    cache.set_ttl_seconds(0);
    let key = Key::new("d", attrs(&[("k", "v")]));
    cache.upsert(&key, &attrs(&[("hash", "x")])).unwrap();
    // lookup() ignores expiry, lookup_with_ttl() respects it.
    assert!(cache.lookup(&key).unwrap().is_some());
    assert!(cache.lookup_with_ttl(&key).unwrap().is_none());
    assert!(cache.lookup_expired(&key).unwrap().unwrap().expired);
}

#[test]
fn key_is_canonical_sorted_json() {
    // Two logically-equal keys with different insertion order hit the same row.
    let cache = Cache::open_in_memory().unwrap();
    let mut a = Attrs::new();
    a.insert("b".into(), Attr::Int(2));
    a.insert("a".into(), Attr::Str("x".into()));
    let mut b = Attrs::new();
    b.insert("a".into(), Attr::Str("x".into()));
    b.insert("b".into(), Attr::Int(2));

    cache.upsert(&Key::new("dom", a), &attrs(&[("hash", "h")])).unwrap();
    assert!(cache.lookup(&Key::new("dom", b)).unwrap().is_some());
}
