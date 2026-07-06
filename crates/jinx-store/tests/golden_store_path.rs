//! Golden tests for store path computation.
//!
//! Golden paths generated with Nix 2.33.3 (`builtins.toFile`,
//! `nix-store --add`, `nix-store --add-fixed`), embedded as constants.

use jinx_store::hash::{hash_string, Hash, HashAlgorithm};
use jinx_store::store_path::{
    check_name, ContentAddress, ContentAddressMethod, ContentAddressWithReferences,
    FileIngestionMethod, FixedOutputInfo, StoreDir, StorePath, StorePathSet, StoreReferences,
    TextInfo,
};

fn store() -> StoreDir {
    StoreDir::default()
}

#[test]
fn golden_text_paths() {
    let store = store();
    // nix eval --impure --expr 'builtins.toFile "foo" "bar"'
    assert_eq!(
        store
            .make_text_path("foo", b"bar", &StorePathSet::new())
            .unwrap()
            .to_string(),
        "vxjiwkjkn7x4079qvh1jkl5pn05j2aw0-foo"
    );
    // builtins.toFile "inner" "x"
    let inner = store
        .make_text_path("inner", b"x", &StorePathSet::new())
        .unwrap();
    assert_eq!(
        inner.to_string(),
        "5vpafad049f76a8rjsma1c6d9058jkyn-inner"
    );
    // builtins.toFile "wrapper" "ref: ${inner}\n"  (a text path with a reference)
    let contents = format!("ref: {}\n", store.print_store_path(&inner));
    let refs: StorePathSet = [inner].into_iter().collect();
    assert_eq!(
        store
            .make_text_path("wrapper", contents.as_bytes(), &refs)
            .unwrap()
            .to_string(),
        "v67m29pyjqq6cnmhskaac3gp8792b7y2-wrapper"
    );
}

#[test]
fn golden_source_paths() {
    let store = store();
    // nix-store --add xfile   (xfile contains "x"; NAR sha256 below)
    let nar_hash = Hash::parse_non_sri_unprefixed(
        "2ca0b8ce996f865db37619bfe91023559305aad8158042fc6ddb0ef1d43c5b67",
        HashAlgorithm::Sha256,
    )
    .unwrap();
    let info = FixedOutputInfo {
        method: FileIngestionMethod::NixArchive,
        hash: nar_hash,
        references: StoreReferences::default(),
    };
    assert_eq!(
        store.make_fixed_output_path("xfile", &info).unwrap().to_string(),
        "j3zpjwg2p40axa8419k7aabyzngq9aki-xfile"
    );

    // nix-store --add tree    (tree/a = "hello\n"; NAR sha256 below)
    let nar_hash = Hash::parse_non_sri_unprefixed(
        "9589bea391d3a6d3ba88d186e8be3510c8461b28762fd82de4feecb491fc82a2",
        HashAlgorithm::Sha256,
    )
    .unwrap();
    let info = FixedOutputInfo {
        method: FileIngestionMethod::NixArchive,
        hash: nar_hash,
        references: StoreReferences::default(),
    };
    assert_eq!(
        store.make_fixed_output_path("tree", &info).unwrap().to_string(),
        "0jxpcd0q2vdpq68c7ak39qkm0r4dbjpc-tree"
    );
}

#[test]
fn golden_fixed_output_paths() {
    let store = store();
    let cases: &[(FileIngestionMethod, HashAlgorithm, &str, &str)] = &[
        // nix-store --add-fixed sha256 xfile
        (
            FileIngestionMethod::Flat,
            HashAlgorithm::Sha256,
            "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881",
            "ldf4gf4b7ivgcgickfqjvjr6mp2z4nq9-xfile",
        ),
        // nix-store --add-fixed sha1 xfile
        (
            FileIngestionMethod::Flat,
            HashAlgorithm::Sha1,
            "11f6ad8ec52a2984abaafd7c3b516503785c2072",
            "jcfm398rrc2fbgs4ph8yi6hbvgk8hprh-xfile",
        ),
        // nix-store --add-fixed --recursive sha1 xfile  (sha1 NAR hash of xfile)
        (
            FileIngestionMethod::NixArchive,
            HashAlgorithm::Sha1,
            "d6a2f655dd4f065a52d876d1c3ff61311302a5f6",
            "j85wxnizvm7r1yzliy388wwr575cymdd-xfile",
        ),
        // nix-store --add-fixed md5 xfile
        (
            FileIngestionMethod::Flat,
            HashAlgorithm::Md5,
            "9dd4e461268c8034f5c8564e155c67a6",
            "bi4zmfgjm77ifnm4gdyylkp0wvhwd21w-xfile",
        ),
    ];
    for (method, algo, hex, expect) in cases {
        let info = FixedOutputInfo {
            method: *method,
            hash: Hash::parse_non_sri_unprefixed(hex, *algo).unwrap(),
            references: StoreReferences::default(),
        };
        assert_eq!(
            store.make_fixed_output_path("xfile", &info).unwrap().to_string(),
            *expect,
            "{method:?} {algo:?}"
        );
    }
}

#[test]
fn make_store_path_raw_layout() {
    // makeStorePath("source", sha256(nar), name) is definitionally
    // sha256("source:sha256:<hex>:/nix/store:<name>") compressed to 20 bytes.
    let store = store();
    let h = hash_string(HashAlgorithm::Sha256, b"x");
    let via_api = store.make_store_path("source", &h, "name").unwrap();
    let manual = {
        let s = format!(
            "source:sha256:{}:/nix/store:name",
            h.to_string(jinx_store::HashFormat::Base16, false)
        );
        let d = jinx_store::compress_hash(&hash_string(HashAlgorithm::Sha256, &s), 20);
        format!("{}-name", d.to_nix32())
    };
    assert_eq!(via_api.to_string(), manual);
}

#[test]
fn fixed_output_with_refs_rejected() {
    let store = store();
    let h = hash_string(HashAlgorithm::Sha256, b"x");
    let other = StorePath::new("5vpafad049f76a8rjsma1c6d9058jkyn-inner").unwrap();
    let info = FixedOutputInfo {
        method: FileIngestionMethod::Flat,
        hash: h,
        references: StoreReferences {
            others: [other].into_iter().collect(),
            self_ref: false,
        },
    };
    assert!(store.make_fixed_output_path("bad", &info).is_err());

    // git ingestion requires sha1/sha256
    let info = FixedOutputInfo {
        method: FileIngestionMethod::Git,
        hash: hash_string(HashAlgorithm::Md5, b"x"),
        references: StoreReferences::default(),
    };
    assert!(store.make_fixed_output_path("bad", &info).is_err());
}

#[test]
fn store_path_validation() {
    // valid
    let p = StorePath::new("5vpafad049f76a8rjsma1c6d9058jkyn-inner").unwrap();
    assert_eq!(p.name(), "inner");
    assert_eq!(p.hash_part(), "5vpafad049f76a8rjsma1c6d9058jkyn");
    assert!(!p.is_derivation());
    assert!(StorePath::new("n9dxhq7k540banjvbg00wj91bgr54yjp-t.drv")
        .unwrap()
        .is_derivation());

    // too short
    assert!(StorePath::new("abc").is_err());
    // bad base-32 chars in hash part (e, o, u, t, uppercase)
    assert!(StorePath::new("evpafad049f76a8rjsma1c6d9058jkyn-x").is_err());
    assert!(StorePath::new("5vpafad049f76a8rjsma1c6d9058jkyt-x").is_err());
    assert!(StorePath::new("5VPAFAD049F76A8RJSMA1C6D9058JKYN-x").is_err());
    // empty name
    assert!(StorePath::new("5vpafad049f76a8rjsma1c6d9058jkyn-").is_err());
    // bad name chars
    assert!(StorePath::new("5vpafad049f76a8rjsma1c6d9058jkyn-a b").is_err());
    assert!(StorePath::new("5vpafad049f76a8rjsma1c6d9058jkyn-a~b").is_err());

    // name rules (checkName)
    assert!(check_name("foo-1.2.3_?=+").is_ok());
    assert!(check_name("").is_err());
    assert!(check_name(".").is_err());
    assert!(check_name("..").is_err());
    assert!(check_name(".-foo").is_err());
    assert!(check_name("..-foo").is_err());
    assert!(check_name(".foo").is_ok());
    assert!(check_name("..foo").is_ok());
    assert!(check_name(&"a".repeat(211)).is_ok());
    assert!(check_name(&"a".repeat(212)).is_err());
}

#[test]
fn parse_store_path_full() {
    let store = store();
    let p = store
        .parse_store_path("/nix/store/5vpafad049f76a8rjsma1c6d9058jkyn-inner")
        .unwrap();
    assert_eq!(p.name(), "inner");
    assert_eq!(
        store.print_store_path(&p),
        "/nix/store/5vpafad049f76a8rjsma1c6d9058jkyn-inner"
    );
    assert!(store.parse_store_path("").is_err());
    assert!(store.parse_store_path("/nix/store").is_err());
    assert!(store
        .parse_store_path("/other/store/5vpafad049f76a8rjsma1c6d9058jkyn-inner")
        .is_err());
    assert!(store
        .parse_store_path("/nix/store/5vpafad049f76a8rjsma1c6d9058jkyn-inner/sub")
        .is_err());
    assert!(store.is_store_path("/nix/store/5vpafad049f76a8rjsma1c6d9058jkyn-inner"));
    assert!(!store.is_store_path("/nix/store/xyz"));
}

#[test]
fn content_address_render_parse() {
    let h = hash_string(HashAlgorithm::Sha256, b"x");
    let n32 = h.to_string(jinx_store::HashFormat::Nix32, true);

    let ca = ContentAddress {
        method: ContentAddressMethod::NixArchive,
        hash: h,
    };
    assert_eq!(ca.render(), format!("fixed:r:{n32}"));
    assert_eq!(ca.print_method_algo(), "r:sha256");
    assert_eq!(ContentAddress::parse(&ca.render()).unwrap(), ca);

    let ca = ContentAddress {
        method: ContentAddressMethod::Flat,
        hash: h,
    };
    assert_eq!(ca.render(), format!("fixed:{n32}"));
    assert_eq!(ca.print_method_algo(), "sha256");
    assert_eq!(ContentAddress::parse(&ca.render()).unwrap(), ca);

    let ca = ContentAddress {
        method: ContentAddressMethod::Text,
        hash: h,
    };
    assert_eq!(ca.render(), format!("text:{n32}"));
    assert_eq!(ca.print_method_algo(), "text:sha256");
    assert_eq!(ContentAddress::parse(&ca.render()).unwrap(), ca);

    let ca = ContentAddress {
        method: ContentAddressMethod::Git,
        hash: hash_string(HashAlgorithm::Sha1, b"x"),
    };
    assert_eq!(ca.print_method_algo(), "git:sha1");
    assert_eq!(ContentAddress::parse(&ca.render()).unwrap(), ca);

    assert!(ContentAddress::parse("bogus:sha256:abc").is_err());
    assert!(ContentAddress::parse("no-colon").is_err());

    // withoutRefs / fromParts
    let ca = ContentAddress {
        method: ContentAddressMethod::Text,
        hash: h,
    };
    match ContentAddressWithReferences::without_refs(&ca) {
        ContentAddressWithReferences::Text(TextInfo { hash, references }) => {
            assert_eq!(hash, h);
            assert!(references.is_empty());
        }
        _ => panic!("expected text"),
    }
    assert!(ContentAddressWithReferences::from_parts(
        ContentAddressMethod::Text,
        h,
        StoreReferences {
            others: StorePathSet::new(),
            self_ref: true,
        },
    )
    .is_err());
}
