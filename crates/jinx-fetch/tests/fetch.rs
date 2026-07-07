//! Fetch + store-path computation tests.
//!
//! The golden store paths were produced by real Nix (Determinate Nix 3.17.3,
//! `nix (Nix) 2.33.3`) over the exact tree recreated below:
//!
//! ```text
//! <root>/
//!   hello.txt        ("hello jinx\n")
//!   sub/nested.txt   ("nested\n")
//! ```
//!
//! Commands used to generate the goldens (both are NAR / recursive sha256
//! content-addressed adds):
//!
//! ```console
//! $ nix-store --add gtree
//! /nix/store/bg5rj219dqv4fijvhv410ybf59y1d8q6-gtree
//! $ nix store add --name source src_tree
//! /nix/store/ap6rq6cf3wh8pw8d7fhm1h03bcdvv8p0-source
//! ```

use std::fs;
use std::path::PathBuf;

use jinx_fetch::attrs::{Attr, Attrs};
use jinx_fetch::fetchers::Input;
use jinx_store::hash::HashAlgorithm;
use jinx_store::store_path::{FileIngestionMethod, FixedOutputInfo, StoreDir};

/// Golden for the tree named `gtree` (from `nix-store --add` of the fixture).
const GOLDEN_GTREE: &str = "/nix/store/2bz72j1dz95x9zrqydjq7bhpsfjdrysd-gtree";
/// Golden for the same content named `source` (what a path input yields).
const GOLDEN_SOURCE: &str = "/nix/store/rp2z2iw4jib5ak5ax6zxgzbsb02mjwx8-source";

fn make_tree(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jinx-fetch-test-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("hello.txt"), "hello jinx\n").unwrap();
    fs::write(dir.join("sub/nested.txt"), "nested\n").unwrap();
    dir
}

#[test]
fn make_fixed_output_path_matches_nix() {
    // Directly cross-check the CA machinery against `nix-store --add`.
    let root = make_tree("ca");
    let store = StoreDir::default();
    let (nar_hash, _) = jinx_store::nar::hash_path(&root, HashAlgorithm::Sha256).unwrap();
    let path = store
        .make_fixed_output_path(
            "gtree",
            &FixedOutputInfo {
                method: FileIngestionMethod::NixArchive,
                hash: nar_hash,
                references: Default::default(),
            },
        )
        .unwrap();
    assert_eq!(store.print_store_path(&path), GOLDEN_GTREE);
}

#[test]
fn path_input_fetch_computes_store_path() {
    // A `path` input names the tree "source"; fetch() (no StoreWriter) must
    // compute exactly the store path Nix would produce.
    let root = make_tree("fetch");
    let store = StoreDir::default();

    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::Str("path".into()));
    attrs.insert(
        "path".into(),
        Attr::Str(root.to_string_lossy().into_owned()),
    );
    let input = Input::from_attrs(attrs).unwrap();
    assert_eq!(input.get_name(), "source");

    let fetched = input.fetch(&store, None).unwrap();
    let sp = fetched.store_path.expect("store path computed");
    assert_eq!(store.print_store_path(&sp), GOLDEN_SOURCE);

    // The info carries an SRI narHash and the input is now considered locked.
    assert!(fetched.info.contains_key("narHash"));
    let locked = Input::from_attrs(fetched.info.clone()).unwrap();
    assert!(locked.is_locked());

    // computeStorePath from the locked narHash agrees with the fetched path.
    let via_hash = locked.compute_store_path(&store).unwrap();
    assert_eq!(via_hash, sp);
}

#[test]
fn path_url_and_attrs_roundtrip() {
    let input =
        Input::from_url("path:/home/user/proj?lastModified=123&rev=abc").unwrap();
    assert_eq!(input.get_type(), Some("path"));
    assert_eq!(input.get_last_modified(), Some(123));
    let url = input.to_url().unwrap();
    // Round-trip back to an equal input.
    let reparsed = Input::from_url(&url).unwrap();
    assert_eq!(reparsed, input);
}

#[test]
fn path_scheme_rejects_unknown_param() {
    assert!(Input::from_url("path:/x?bogus=1").is_err());
}

#[test]
fn plain_path_is_a_path_input() {
    let input = Input::from_url("/absolute/dir").unwrap();
    assert_eq!(input.get_type(), Some("path"));
    assert_eq!(
        jinx_fetch::attrs::maybe_get_str(input.to_attrs(), "path"),
        Some("/absolute/dir")
    );
}
