//! Golden tests for derivation ATerm parse/unparse, `.drv` store paths,
//! output path computation and `hashDerivationModulo`.
//!
//! The golden `.drv` bytes below are the actual store files produced by
//! `nix-instantiate` (Nix 2.33.3) for small `derivation { ... }` calls;
//! embedded as constants, no nix needed at test time.

use std::collections::BTreeMap;

use jinx_store::derivation::{
    hash_derivation_modulo, Derivation, DerivationOutput, DerivationType, DrvError, DrvHashModulo,
    DrvHashes, DrvResolver,
};
use jinx_store::hash::{Hash, HashAlgorithm};
use jinx_store::store_path::{ContentAddress, ContentAddressMethod, StoreDir, StorePath};

// --- goldens ---------------------------------------------------------------

// derivation { name = "t"; system = "x86_64-linux"; builder = "/bin/sh"; args = ["-c" "x"]; }
const T_DRV_PATH: &str = "n9dxhq7k540banjvbg00wj91bgr54yjp-t.drv";
const T_OUT: &str = "/nix/store/klvw9h33ymg714aabq5hk4rqkcvag7sr-t";
const T_DRV: &str = r#"Derive([("out","/nix/store/klvw9h33ymg714aabq5hk4rqkcvag7sr-t","","")],[],[],"x86_64-linux","/bin/sh",["-c","x"],[("builder","/bin/sh"),("name","t"),("out","/nix/store/klvw9h33ymg714aabq5hk4rqkcvag7sr-t"),("system","x86_64-linux")])"#;

// ... same but outputs = ["out" "dev"], name = "m"
const M_DRV_PATH: &str = "daqycbag9gnwg2fy1l7rqancadlkpm17-m.drv";
const M_OUT: &str = "/nix/store/lm39fw1g69cxd76sjcvlz56hqfg2c515-m";
const M_DEV: &str = "/nix/store/ljrpsi2sscybqya7dng6sd1kifl0vin2-m-dev";
const M_DRV: &str = r#"Derive([("dev","/nix/store/ljrpsi2sscybqya7dng6sd1kifl0vin2-m-dev","",""),("out","/nix/store/lm39fw1g69cxd76sjcvlz56hqfg2c515-m","","")],[],[],"x86_64-linux","/bin/sh",["-c","x"],[("builder","/bin/sh"),("dev","/nix/store/ljrpsi2sscybqya7dng6sd1kifl0vin2-m-dev"),("name","m"),("out","/nix/store/lm39fw1g69cxd76sjcvlz56hqfg2c515-m"),("outputs","out dev"),("system","x86_64-linux")])"#;

// fixed-output, flat sha256 (outputHash of "hello world")
const F_DRV_PATH: &str = "834ldy1rqb5q7jbxiz7kxi0xv0qm4cp8-f.drv";
const F_OUT: &str = "/nix/store/xd852v0q3c9yh0qnp6ly3syqdn83zbyw-f";
const F_DRV: &str = r#"Derive([("out","/nix/store/xd852v0q3c9yh0qnp6ly3syqdn83zbyw-f","sha256","b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9")],[],[],"x86_64-linux","/bin/sh",[],[("builder","/bin/sh"),("name","f"),("out","/nix/store/xd852v0q3c9yh0qnp6ly3syqdn83zbyw-f"),("outputHash","b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"),("outputHashAlgo","sha256"),("outputHashMode","flat"),("system","x86_64-linux")])"#;

// fixed-output, recursive sha1 (SRI outputHash)
const FR_DRV_PATH: &str = "9cchaaxxi4baj62vznmw2x1yayay76nk-fr.drv";
const FR_OUT: &str = "/nix/store/g54ha1zcbvlfri6jmp4260nsblzl67xl-fr";
const FR_DRV: &str = r#"Derive([("out","/nix/store/g54ha1zcbvlfri6jmp4260nsblzl67xl-fr","r:sha1","2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")],[],[],"x86_64-linux","/bin/sh",[],[("builder","/bin/sh"),("name","fr"),("out","/nix/store/g54ha1zcbvlfri6jmp4260nsblzl67xl-fr"),("outputHash","sha1-Kq5sNclPz7QV2+lfQIuc6R7oRu0="),("outputHashMode","recursive"),("system","x86_64-linux")])"#;

// env / args with newlines, tabs, CRs, quotes, backslashes, dollars, unicode
const W_DRV_PATH: &str = "bsgq9dn9v8wwhb4c1rvj4s718pdkjnah-w.drv";
const W_OUT: &str = "/nix/store/pp6kb5brjlbs1ghkdk5ghlmx75xcic25-w";
const W_DRV: &str = r#"Derive([("out","/nix/store/pp6kb5brjlbs1ghkdk5ghlmx75xcic25-w","","")],[],[],"x86_64-linux","/bin/sh",["a\nb","c\td\"e\\f"],[("builder","/bin/sh"),("env1","line1\nline2\rcr\ttab"),("env2","quote\" backslash\\ dollar$ unicodeé中"),("name","w"),("out","/nix/store/pp6kb5brjlbs1ghkdk5ghlmx75xcic25-w"),("system","x86_64-linux")])"#;

// b depends on t's "out" and m's "dev"
const B_DRV_PATH: &str = "bslv9y7qjb1155bfnbr98ygq8pxsxvkg-b.drv";
const B_OUT: &str = "/nix/store/i3595mrndl1abnnq9vr3w72hi4cj59qs-b";
const B_DRV: &str = r#"Derive([("out","/nix/store/i3595mrndl1abnnq9vr3w72hi4cj59qs-b","","")],[("/nix/store/daqycbag9gnwg2fy1l7rqancadlkpm17-m.drv",["dev"]),("/nix/store/n9dxhq7k540banjvbg00wj91bgr54yjp-t.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[("a","/nix/store/klvw9h33ymg714aabq5hk4rqkcvag7sr-t"),("builder","/bin/sh"),("dev","/nix/store/ljrpsi2sscybqya7dng6sd1kifl0vin2-m-dev"),("name","b"),("out","/nix/store/i3595mrndl1abnnq9vr3w72hi4cj59qs-b"),("system","x86_64-linux")])"#;

// bf depends on the two fixed-output drvs (the hashDerivationModulo special case)
const BF_DRV_PATH: &str = "9gdi7ifvz8ghzyfghv4l6fdyqzxjllzn-bf.drv";
const BF_OUT: &str = "/nix/store/4kg5r0k46wly5ak49wh8hkq2gs7ifi0b-bf";
const BF_DRV: &str = r#"Derive([("out","/nix/store/4kg5r0k46wly5ak49wh8hkq2gs7ifi0b-bf","","")],[("/nix/store/834ldy1rqb5q7jbxiz7kxi0xv0qm4cp8-f.drv",["out"]),("/nix/store/9cchaaxxi4baj62vznmw2x1yayay76nk-fr.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[("builder","/bin/sh"),("f","/nix/store/xd852v0q3c9yh0qnp6ly3syqdn83zbyw-f"),("fr","/nix/store/g54ha1zcbvlfri6jmp4260nsblzl67xl-fr"),("name","bf"),("out","/nix/store/4kg5r0k46wly5ak49wh8hkq2gs7ifi0b-bf"),("system","x86_64-linux")])"#;

// s has an inputSrc (builtins.toFile "inner" "x")
const S_DRV_PATH: &str = "qpsv5x5yqx9107z5va978l5zjx5mayqv-s.drv";
const S_OUT: &str = "/nix/store/y9ckj321ms3qwsdm0wy5qdzgl6hccg20-s";
const S_DRV: &str = r#"Derive([("out","/nix/store/y9ckj321ms3qwsdm0wy5qdzgl6hccg20-s","","")],[],["/nix/store/5vpafad049f76a8rjsma1c6d9058jkyn-inner"],"x86_64-linux","/bin/sh",[],[("builder","/bin/sh"),("name","s"),("out","/nix/store/y9ckj321ms3qwsdm0wy5qdzgl6hccg20-s"),("src","/nix/store/5vpafad049f76a8rjsma1c6d9058jkyn-inner"),("system","x86_64-linux")])"#;

// env key sort order (uppercase / underscore / dash), empty env value,
// backslash-heavy value
const K_DRV_PATH: &str = "b8700zjw98ak04xjb61nnc6rnys81jnw-k.drv";
const K_OUT: &str = "/nix/store/i1jphc8lvrcwifpj8a8idr7kb1b00xsx-k";
const K_DRV: &str = r#"Derive([("out","/nix/store/i1jphc8lvrcwifpj8a8idr7kb1b00xsx-k","","")],[],[],"x86_64-linux","/bin/sh",[],[("AAA",""),("ZZZ_UPPER","v"),("_under","u"),("builder","/bin/sh"),("name","k"),("out","/nix/store/i1jphc8lvrcwifpj8a8idr7kb1b00xsx-k"),("system","x86_64-linux"),("with-dash","\\only\\backslashes\\")])"#;

const ALL: &[(&str, &str, &str)] = &[
    ("t", T_DRV_PATH, T_DRV),
    ("m", M_DRV_PATH, M_DRV),
    ("f", F_DRV_PATH, F_DRV),
    ("fr", FR_DRV_PATH, FR_DRV),
    ("w", W_DRV_PATH, W_DRV),
    ("b", B_DRV_PATH, B_DRV),
    ("bf", BF_DRV_PATH, BF_DRV),
    ("s", S_DRV_PATH, S_DRV),
    ("k", K_DRV_PATH, K_DRV),
];

fn store() -> StoreDir {
    StoreDir::default()
}

// --- parse / unparse / drv path -------------------------------------------

#[test]
fn golden_parse_unparse_roundtrip_and_drv_path() {
    let store = store();
    for (name, drv_path, aterm) in ALL {
        let drv = Derivation::parse(&store, aterm.as_bytes(), name).unwrap();
        // byte-exact unparse
        let unparsed = drv.unparse(&store, false, None).unwrap();
        assert_eq!(
            unparsed.as_slice(),
            aterm.as_bytes(),
            "unparse mismatch for {name}"
        );
        // .drv store path (text path with references)
        assert_eq!(
            drv.compute_store_path(&store).unwrap().to_string(),
            *drv_path,
            "drv path mismatch for {name}"
        );
    }
}

#[test]
fn golden_parsed_structure() {
    let store = store();
    let drv = Derivation::parse(&store, B_DRV.as_bytes(), "b").unwrap();
    assert_eq!(drv.name, "b");
    assert_eq!(drv.platform.as_slice(), b"x86_64-linux");
    assert_eq!(drv.builder.as_slice(), b"/bin/sh");
    assert!(drv.args.is_empty());
    assert_eq!(drv.outputs.len(), 1);
    match &drv.outputs["out"] {
        DerivationOutput::InputAddressed { path } => {
            assert_eq!(store.print_store_path(path), B_OUT)
        }
        other => panic!("unexpected output: {other:?}"),
    }
    assert_eq!(drv.input_drvs.map.len(), 2);
    let t = StorePath::new(T_DRV_PATH).unwrap();
    let node = &drv.input_drvs.map[&t];
    assert_eq!(
        node.value.iter().cloned().collect::<Vec<_>>(),
        vec!["out".to_owned()]
    );
    assert!(node.child_map.is_empty());
    assert_eq!(drv.env.len(), 6);
    assert_eq!(drv.get_type().unwrap(), DerivationType::InputAddressed { deferred: false });

    let fr = Derivation::parse(&store, FR_DRV.as_bytes(), "fr").unwrap();
    match &fr.outputs["out"] {
        DerivationOutput::CAFixed { ca } => {
            assert_eq!(ca.method, ContentAddressMethod::NixArchive);
            assert_eq!(ca.hash.algo, HashAlgorithm::Sha1);
        }
        other => panic!("unexpected output: {other:?}"),
    }
    assert!(fr.get_type().unwrap().is_fixed());

    let w = Derivation::parse(&store, W_DRV.as_bytes(), "w").unwrap();
    assert_eq!(w.args[0].as_slice(), b"a\nb");
    assert_eq!(w.args[1].as_slice(), b"c\td\"e\\f");
    assert_eq!(
        w.env[bstr::BStr::new("env1")].as_slice(),
        b"line1\nline2\rcr\ttab"
    );
    assert_eq!(
        w.env[bstr::BStr::new("env2")].as_slice(),
        "quote\" backslash\\ dollar$ unicodeé中".as_bytes()
    );

    let s = Derivation::parse(&store, S_DRV.as_bytes(), "s").unwrap();
    assert_eq!(s.input_srcs.len(), 1);
    assert_eq!(
        s.input_srcs.iter().next().unwrap().to_string(),
        "5vpafad049f76a8rjsma1c6d9058jkyn-inner"
    );
}

#[test]
fn parse_errors() {
    let store = store();
    assert!(Derivation::parse(&store, b"Nope(", "x").is_err());
    assert!(Derivation::parse(&store, b"Derive([", "x").is_err());
    assert!(Derivation::parse(&store, br#"Derive([("out","/nix/store/klvw9h33ymg714aabq5hk4rqkcvag7sr-t","",""#, "x").is_err());
    // unterminated string
    assert!(Derivation::parse(&store, br#"Derive([("out"#, "x").is_err());
    // bad path in outputs
    assert!(Derivation::parse(
        &store,
        br#"Derive([("out","relative/path","","")],[],[],"x","/b",[],[])"#,
        "x"
    )
    .is_err());
    // unknown ATerm version
    assert!(Derivation::parse(&store, br#"DrvWithVersion("xp-other",[],[],[],"x","/b",[],[])"#, "x").is_err());
}

// --- from-scratch construction (evaluator pipeline) ------------------------

/// Build a derivation the way `derivationStrict` does for input-addressed
/// derivations: outputs start as Deferred with empty env placeholders, the
/// masked hashDerivationModulo is computed, then output paths are filled in.
fn make_input_addressed(
    store: &StoreDir,
    name: &str,
    mut drv: Derivation,
    output_names: &[&str],
    memo: &mut DrvHashes,
    resolver: &mut dyn DrvResolver,
) -> Derivation {
    drv.name = name.to_owned();
    for &o in output_names {
        drv.outputs.insert(o.to_owned(), DerivationOutput::Deferred);
        drv.env.insert(o.into(), b"".to_vec().into());
    }
    let h = hash_derivation_modulo(store, memo, resolver, &drv, true).unwrap();
    let h = match h {
        DrvHashModulo::DrvHash(h) => h,
        other => panic!("expected DrvHash, got {other:?}"),
    };
    for &o in output_names {
        let out_path = store.make_output_path(o, &h, name).unwrap();
        drv.env
            .insert(o.into(), store.print_store_path(&out_path).into_bytes().into());
        drv.outputs
            .insert(o.to_owned(), DerivationOutput::InputAddressed { path: out_path });
    }
    drv
}

fn no_inputs(_: &StorePath) -> Result<Derivation, DrvError> {
    panic!("derivation has no inputs")
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<bstr::BString, bstr::BString> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

#[test]
fn golden_output_path_computation_basic() {
    let store = store();
    let mut memo = DrvHashes::default();

    let base = Derivation {
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        args: vec!["-c".into(), "x".into()],
        env: env(&[("builder", "/bin/sh"), ("name", "t"), ("system", "x86_64-linux")]),
        ..Default::default()
    };
    let t = make_input_addressed(&store, "t", base, &["out"], &mut memo, &mut no_inputs);
    assert_eq!(t.unparse(&store, false, None).unwrap(), T_DRV.as_bytes());
    assert_eq!(t.compute_store_path(&store).unwrap().to_string(), T_DRV_PATH);
    match &t.outputs["out"] {
        DerivationOutput::InputAddressed { path } => {
            assert_eq!(store.print_store_path(path), T_OUT)
        }
        _ => unreachable!(),
    }
}

#[test]
fn golden_output_path_computation_multi_output() {
    let store = store();
    let mut memo = DrvHashes::default();

    let base = Derivation {
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        args: vec!["-c".into(), "x".into()],
        env: env(&[
            ("builder", "/bin/sh"),
            ("name", "m"),
            ("outputs", "out dev"),
            ("system", "x86_64-linux"),
        ]),
        ..Default::default()
    };
    let m = make_input_addressed(&store, "m", base, &["out", "dev"], &mut memo, &mut no_inputs);
    assert_eq!(m.unparse(&store, false, None).unwrap(), M_DRV.as_bytes());
    assert_eq!(m.compute_store_path(&store).unwrap().to_string(), M_DRV_PATH);
    let paths: Vec<String> = ["out", "dev"]
        .iter()
        .map(|o| match &m.outputs[*o] {
            DerivationOutput::InputAddressed { path } => store.print_store_path(path),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(paths, vec![M_OUT.to_owned(), M_DEV.to_owned()]);
}

fn make_fixed(name: &str, ca: ContentAddress, extra_env: &[(&str, &str)]) -> Derivation {
    let store = store();
    let mut drv = Derivation {
        name: name.to_owned(),
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        env: env(extra_env),
        ..Default::default()
    };
    drv.outputs
        .insert("out".to_owned(), DerivationOutput::CAFixed { ca: ca.clone() });
    let out_path = drv.outputs["out"]
        .path(&store, name, "out")
        .unwrap()
        .unwrap();
    drv.env.insert(
        "out".into(),
        store.print_store_path(&out_path).into_bytes().into(),
    );
    drv
}

fn fixed_f() -> Derivation {
    make_fixed(
        "f",
        ContentAddress {
            method: ContentAddressMethod::Flat,
            hash: Hash::parse_non_sri_unprefixed(
                "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
                HashAlgorithm::Sha256,
            )
            .unwrap(),
        },
        &[
            ("builder", "/bin/sh"),
            ("name", "f"),
            (
                "outputHash",
                "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
            ),
            ("outputHashAlgo", "sha256"),
            ("outputHashMode", "flat"),
            ("system", "x86_64-linux"),
        ],
    )
}

fn fixed_fr() -> Derivation {
    make_fixed(
        "fr",
        ContentAddress {
            method: ContentAddressMethod::NixArchive,
            hash: Hash::parse_sri("sha1-Kq5sNclPz7QV2+lfQIuc6R7oRu0=").unwrap(),
        },
        &[
            ("builder", "/bin/sh"),
            ("name", "fr"),
            ("outputHash", "sha1-Kq5sNclPz7QV2+lfQIuc6R7oRu0="),
            ("outputHashMode", "recursive"),
            ("system", "x86_64-linux"),
        ],
    )
}

#[test]
fn golden_fixed_output_derivations() {
    let store = store();
    let f = fixed_f();
    assert_eq!(f.unparse(&store, false, None).unwrap(), F_DRV.as_bytes());
    assert_eq!(f.compute_store_path(&store).unwrap().to_string(), F_DRV_PATH);
    assert_eq!(
        store.print_store_path(&f.outputs["out"].path(&store, "f", "out").unwrap().unwrap()),
        F_OUT
    );

    let fr = fixed_fr();
    assert_eq!(fr.unparse(&store, false, None).unwrap(), FR_DRV.as_bytes());
    assert_eq!(fr.compute_store_path(&store).unwrap().to_string(), FR_DRV_PATH);
    assert_eq!(
        store.print_store_path(&fr.outputs["out"].path(&store, "fr", "out").unwrap().unwrap()),
        FR_OUT
    );

    // hashDerivationModulo of a fixed-output drv is its CaOutputHashes map.
    let mut memo = DrvHashes::default();
    let h = hash_derivation_modulo(&store, &mut memo, &mut no_inputs, &f, false).unwrap();
    match h {
        DrvHashModulo::CaOutputHashes(m) => {
            assert_eq!(m.len(), 1);
            assert!(m.contains_key("out"));
        }
        other => panic!("expected CaOutputHashes, got {other:?}"),
    }
}

#[test]
fn golden_escaping() {
    let store = store();
    let mut memo = DrvHashes::default();
    let base = Derivation {
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        args: vec!["a\nb".into(), "c\td\"e\\f".into()],
        env: env(&[
            ("builder", "/bin/sh"),
            ("env1", "line1\nline2\rcr\ttab"),
            ("env2", "quote\" backslash\\ dollar$ unicodeé中"),
            ("name", "w"),
            ("system", "x86_64-linux"),
        ]),
        ..Default::default()
    };
    let w = make_input_addressed(&store, "w", base, &["out"], &mut memo, &mut no_inputs);
    assert_eq!(
        String::from_utf8(w.unparse(&store, false, None).unwrap()).unwrap(),
        W_DRV
    );
    assert_eq!(w.compute_store_path(&store).unwrap().to_string(), W_DRV_PATH);
    match &w.outputs["out"] {
        DerivationOutput::InputAddressed { path } => {
            assert_eq!(store.print_store_path(path), W_OUT)
        }
        _ => unreachable!(),
    }
}

#[test]
fn golden_env_sort_order_and_empty_values() {
    let store = store();
    let mut memo = DrvHashes::default();
    let base = Derivation {
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        env: env(&[
            ("AAA", ""),
            ("ZZZ_UPPER", "v"),
            ("_under", "u"),
            ("builder", "/bin/sh"),
            ("name", "k"),
            ("system", "x86_64-linux"),
            ("with-dash", "\\only\\backslashes\\"),
        ]),
        ..Default::default()
    };
    let k = make_input_addressed(&store, "k", base, &["out"], &mut memo, &mut no_inputs);
    assert_eq!(k.unparse(&store, false, None).unwrap(), K_DRV.as_bytes());
    assert_eq!(k.compute_store_path(&store).unwrap().to_string(), K_DRV_PATH);
    match &k.outputs["out"] {
        DerivationOutput::InputAddressed { path } => {
            assert_eq!(store.print_store_path(path), K_OUT)
        }
        _ => unreachable!(),
    }
}

/// Resolver over the embedded golden derivations.
fn golden_resolver(store: &StoreDir) -> impl FnMut(&StorePath) -> Result<Derivation, DrvError> {
    let store = store.clone();
    move |path: &StorePath| {
        for (name, drv_path, aterm) in ALL {
            if path.to_string() == *drv_path {
                return Derivation::parse(&store, aterm.as_bytes(), name);
            }
        }
        Err(DrvError(format!("unknown drv {path}")))
    }
}

#[test]
fn golden_hash_derivation_modulo_with_input_drvs() {
    let store = store();
    let mut memo = DrvHashes::default();
    let mut resolver = golden_resolver(&store);

    // b depends on t.out and m.dev (regular input-addressed inputs)
    let mut base = Derivation {
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        env: env(&[
            ("a", "/nix/store/klvw9h33ymg714aabq5hk4rqkcvag7sr-t"),
            ("builder", "/bin/sh"),
            ("dev", "/nix/store/ljrpsi2sscybqya7dng6sd1kifl0vin2-m-dev"),
            ("name", "b"),
            ("system", "x86_64-linux"),
        ]),
        ..Default::default()
    };
    base.input_drvs.map.insert(
        StorePath::new(T_DRV_PATH).unwrap(),
        jinx_store::DerivedPathMapNode {
            value: ["out".to_owned()].into_iter().collect(),
            child_map: BTreeMap::new(),
        },
    );
    base.input_drvs.map.insert(
        StorePath::new(M_DRV_PATH).unwrap(),
        jinx_store::DerivedPathMapNode {
            value: ["dev".to_owned()].into_iter().collect(),
            child_map: BTreeMap::new(),
        },
    );
    let b = make_input_addressed(&store, "b", base, &["out"], &mut memo, &mut resolver);
    assert_eq!(b.unparse(&store, false, None).unwrap(), B_DRV.as_bytes());
    assert_eq!(b.compute_store_path(&store).unwrap().to_string(), B_DRV_PATH);
    match &b.outputs["out"] {
        DerivationOutput::InputAddressed { path } => {
            assert_eq!(store.print_store_path(path), B_OUT)
        }
        _ => unreachable!(),
    }
    // memoization kicked in
    assert!(memo.contains_key(&StorePath::new(T_DRV_PATH).unwrap()));
    assert!(memo.contains_key(&StorePath::new(M_DRV_PATH).unwrap()));
}

#[test]
fn golden_hash_derivation_modulo_fixed_output_inputs() {
    let store = store();
    let mut memo = DrvHashes::default();
    let mut resolver = golden_resolver(&store);

    // bf depends on two fixed-output drvs: the modulo special case where the
    // input drv is replaced by sha256("fixed:out:<method><algo>:<hash>:<path>")
    let mut base = Derivation {
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        env: env(&[
            ("builder", "/bin/sh"),
            ("f", "/nix/store/xd852v0q3c9yh0qnp6ly3syqdn83zbyw-f"),
            ("fr", "/nix/store/g54ha1zcbvlfri6jmp4260nsblzl67xl-fr"),
            ("name", "bf"),
            ("system", "x86_64-linux"),
        ]),
        ..Default::default()
    };
    for p in [F_DRV_PATH, FR_DRV_PATH] {
        base.input_drvs.map.insert(
            StorePath::new(p).unwrap(),
            jinx_store::DerivedPathMapNode {
                value: ["out".to_owned()].into_iter().collect(),
                child_map: BTreeMap::new(),
            },
        );
    }
    let bf = make_input_addressed(&store, "bf", base, &["out"], &mut memo, &mut resolver);
    assert_eq!(bf.unparse(&store, false, None).unwrap(), BF_DRV.as_bytes());
    assert_eq!(bf.compute_store_path(&store).unwrap().to_string(), BF_DRV_PATH);
    match &bf.outputs["out"] {
        DerivationOutput::InputAddressed { path } => {
            assert_eq!(store.print_store_path(path), BF_OUT)
        }
        _ => unreachable!(),
    }
}

#[test]
fn golden_input_srcs() {
    let store = store();
    let mut memo = DrvHashes::default();
    let mut base = Derivation {
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        env: env(&[
            ("builder", "/bin/sh"),
            ("name", "s"),
            ("src", "/nix/store/5vpafad049f76a8rjsma1c6d9058jkyn-inner"),
            ("system", "x86_64-linux"),
        ]),
        ..Default::default()
    };
    base.input_srcs
        .insert(StorePath::new("5vpafad049f76a8rjsma1c6d9058jkyn-inner").unwrap());
    let s = make_input_addressed(&store, "s", base, &["out"], &mut memo, &mut no_inputs);
    assert_eq!(s.unparse(&store, false, None).unwrap(), S_DRV.as_bytes());
    assert_eq!(s.compute_store_path(&store).unwrap().to_string(), S_DRV_PATH);
    match &s.outputs["out"] {
        DerivationOutput::InputAddressed { path } => {
            assert_eq!(store.print_store_path(path), S_OUT)
        }
        _ => unreachable!(),
    }
}

#[test]
fn dynamic_derivation_aterm_roundtrip() {
    // No golden from real nix (needs xp dynamic-derivations), but lock the
    // DrvWithVersion syntax to the C++ unparser's output shape.
    let store = store();
    let mut drv = Derivation {
        name: "dyn".to_owned(),
        platform: "x".into(),
        builder: "/b".into(),
        ..Default::default()
    };
    drv.outputs
        .insert("out".to_owned(), DerivationOutput::Deferred);
    drv.input_drvs.map.insert(
        StorePath::new(T_DRV_PATH).unwrap(),
        jinx_store::DerivedPathMapNode {
            value: ["out".to_owned()].into_iter().collect(),
            child_map: [(
                "out".to_owned(),
                jinx_store::DerivedPathMapNode {
                    value: ["sub".to_owned()].into_iter().collect(),
                    child_map: BTreeMap::new(),
                },
            )]
            .into_iter()
            .collect(),
        },
    );
    let s = drv.unparse(&store, false, None).unwrap();
    let expected = format!(
        r#"DrvWithVersion("xp-dyn-drv",[("out","","","")],[("/nix/store/{T_DRV_PATH}",(["out"],[("out",["sub"])]))],[],"x","/b",[],[])"#
    );
    assert_eq!(String::from_utf8(s.clone()).unwrap(), expected);
    let reparsed = Derivation::parse(&store, &s, "dyn").unwrap();
    assert_eq!(reparsed, drv);
    // dynamic deps force DeferredDrv in hashDerivationModulo
    let mut memo = DrvHashes::default();
    let h = hash_derivation_modulo(&store, &mut memo, &mut no_inputs, &drv, false).unwrap();
    assert_eq!(h, DrvHashModulo::DeferredDrv);
}

#[test]
fn type_checks() {
    let store = store();
    // mixing fixed and input-addressed outputs is rejected
    let mut drv = Derivation::parse(&store, F_DRV.as_bytes(), "f").unwrap();
    drv.outputs.insert(
        "other".to_owned(),
        DerivationOutput::InputAddressed {
            path: StorePath::new("5vpafad049f76a8rjsma1c6d9058jkyn-inner").unwrap(),
        },
    );
    assert!(drv.get_type().is_err());
    // fixed output must be named "out"
    let mut drv = Derivation::parse(&store, F_DRV.as_bytes(), "f").unwrap();
    let out = drv.outputs.remove("out").unwrap();
    drv.outputs.insert("dev".to_owned(), out);
    assert!(drv.get_type().is_err());
    // no outputs
    let drv = Derivation {
        name: "x".to_owned(),
        ..Default::default()
    };
    assert!(drv.get_type().is_err());
}
