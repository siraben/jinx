//! Golden tests for nix32 and hash formats.
//!
//! Golden values generated with Nix 2.33.3 (`nix hash convert`,
//! `nix-hash`), embedded as constants; the tests do not invoke nix.

use jinx_store::hash::{
    base16_decode, base16_encode, base64_decode, base64_encode, compress_hash, hash_string, Hash,
    HashAlgorithm, HashFormat,
};
use jinx_store::nix32;

/// (algo, input, base16, nix32, base64)
const GOLDENS: &[(HashAlgorithm, &str, &str, &str, &str)] = &[
    (
        HashAlgorithm::Md5,
        "",
        "d41d8cd98f00b204e9800998ecf8427e",
        "3y8bwfr609h3lh9ch0izcqq7fl",
        "1B2M2Y8AsgTpgAmY7PhCfg==",
    ),
    (
        HashAlgorithm::Md5,
        "abc",
        "900150983cd24fb0d6963f7d28e17f72",
        "3jgzhjhz9zjvbb0kyj7jc500ch",
        "kAFQmDzST7DWlj99KOF/cg==",
    ),
    (
        HashAlgorithm::Md5,
        "hello world",
        "5eb63bbbe01eeed093cb22bb8f5acdc3",
        "63rmd8zfr2rf9x1vhyw2xkpdjy",
        "XrY7u+Ae7tCTyyK7j1rNww==",
    ),
    (
        HashAlgorithm::Sha1,
        "",
        "da39a3ee5e6b4b0d3255bfef95601890afd80709",
        "143xibwh31h9bvxzalr0sjvbbvpa6ffs",
        "2jmj7l5rSw0yVb/vlWAYkK/YBwk=",
    ),
    (
        HashAlgorithm::Sha1,
        "abc",
        "a9993e364706816aba3e25717850c26c9cd0d89d",
        "kpcd173cq987hw957sx6m0868wv3x6d9",
        "qZk+NkcGgWq6PiVxeFDCbJzQ2J0=",
    ),
    (
        HashAlgorithm::Sha1,
        "hello world",
        "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
        "xm3fh7p9kj5l0pz9vcav9ksgr4snrbia",
        "Kq5sNclPz7QV2+lfQIuc6R7oRu0=",
    ),
    (
        HashAlgorithm::Sha256,
        "",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73",
        "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
    ),
    (
        HashAlgorithm::Sha256,
        "abc",
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s",
        "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=",
    ),
    (
        HashAlgorithm::Sha256,
        "hello world",
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        "1sfdxziarxw8j3p80lvswgpq9i7smdyxmmsj5sjhhgjdjfwjfkdr",
        "uU0nuZNNPgilLlLX2n2r+sSE7+N6U4DukIj3rOLvzek=",
    ),
    (
        HashAlgorithm::Sha512,
        "",
        "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
        "0zdl9zrg8r3i9c1g90lgg9ip5ijzv3yhz91i0zzn3r8ap9ws784gkp9dk9j3aglhgf1amqb0pj21mh7h1nxcl18akqvvf7ggqsy30yg",
        "z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg==",
    ),
    (
        HashAlgorithm::Sha512,
        "abc",
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        "2gs8k559z4rlahfx0y688s49m2vvszylcikrfinm30ly9rak69236nkam5ydvly1ai7xac99vxfc4ii84hawjbk876blyk1jfhkbbyx",
        "3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw==",
    ),
    (
        HashAlgorithm::Sha512,
        "hello world",
        "309ecc489c12d6eb4cc40f50c902f2b4d0ed77ee511a7c7a9bcd3ca86d4cd86f989dd35bc5ff499670da34255b45b0cfd830e81f605dcf7dc5542e93ae9cd76f",
        "1pxg75fjcp59ibxrxfn07z863cczc25bcjk9nkhjr4zziavsffrhvyq9inshg6dkdx7q6jixrvyvl5ly81cjl0gqi6fpmhjki4cr7ih",
        "MJ7MSJwS1utMxA9QyQLytNDtd+5RGnx6m808qG1M2G+YndNbxf9JlnDaNCVbRbDP2DDoH2Bdz33FVC6TrpzXbw==",
    ),
];

#[test]
fn golden_hash_string_and_formats() {
    for (algo, input, hex, n32, b64) in GOLDENS {
        let h = hash_string(*algo, input.as_bytes());
        assert_eq!(h.to_string(HashFormat::Base16, false), *hex, "{algo:?} {input:?}");
        assert_eq!(h.to_string(HashFormat::Nix32, false), *n32);
        assert_eq!(h.to_string(HashFormat::Base64, false), *b64);
        assert_eq!(
            h.to_string(HashFormat::Sri, false),
            format!("{}-{b64}", algo.name())
        );
        assert_eq!(
            h.to_string(HashFormat::Base16, true),
            format!("{}:{hex}", algo.name())
        );
    }
}

#[test]
fn golden_parse_all_formats() {
    for (algo, _input, hex, n32, b64) in GOLDENS {
        let expect = Hash::parse_non_sri_unprefixed(hex, *algo).unwrap();
        // unprefixed, inferred format by length
        assert_eq!(Hash::parse_any(n32, Some(*algo)).unwrap(), expect);
        assert_eq!(Hash::parse_any(b64, Some(*algo)).unwrap(), expect);
        // prefixed
        assert_eq!(
            Hash::parse_any_prefixed(&format!("{}:{n32}", algo.name())).unwrap(),
            expect
        );
        assert_eq!(
            Hash::parse_any(&format!("{}:{hex}", algo.name()), None).unwrap(),
            expect
        );
        // SRI
        let sri = format!("{}-{b64}", algo.name());
        assert_eq!(Hash::parse_sri(&sri).unwrap(), expect);
        assert_eq!(Hash::parse_any(&sri, Some(*algo)).unwrap(), expect);
        // roundtrip through nix32 decode
        assert_eq!(
            nix32::decode(n32.as_bytes()).unwrap(),
            expect.as_bytes().to_vec()
        );
    }
}

#[test]
fn nix32_basics() {
    assert_eq!(nix32::encode(b""), "");
    assert_eq!(nix32::decode(b"").unwrap(), Vec::<u8>::new());
    // roundtrip all byte values
    let all: Vec<u8> = (0u8..=255).collect();
    assert_eq!(nix32::decode(nix32::encode(&all).as_bytes()).unwrap(), all);
    // invalid characters (e, o, u, t and non-alphabet)
    for c in ["e", "o", "u", "t", "E", "A", " ", "-"] {
        assert!(nix32::decode(c.as_bytes()).is_err(), "{c:?} should be invalid");
    }
    assert_eq!(nix32::encoded_length(20), 32);
    assert_eq!(nix32::encoded_length(32), 52);
}

#[test]
fn hash_parse_errors() {
    // no type
    assert!(Hash::parse_any_prefixed("0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73").is_err());
    assert!(Hash::parse_any("abc", None).is_err());
    // wrong length
    assert!(Hash::parse_any("sha256:abcd", None).is_err());
    // type mismatch between prefix and context
    assert!(Hash::parse_any(
        "sha256:0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73",
        Some(HashAlgorithm::Sha1)
    )
    .is_err());
    // unknown algo
    assert!(Hash::parse_any_prefixed("sha42:abcd").is_err());
    assert!(HashAlgorithm::parse("sha42").is_err());
    // bad nix32 char in a 52-char (nix32-length) sha256 string
    assert!(Hash::parse_any(
        "sha256:0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9ce3",
        None
    )
    .is_err());
    // SRI requires '-'
    assert!(Hash::parse_sri("sha256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=").is_err());
}

#[test]
fn base16_base64_roundtrip() {
    let all: Vec<u8> = (0u8..=255).collect();
    assert_eq!(base16_decode(&base16_encode(&all)).unwrap(), all);
    assert_eq!(base64_decode(&base64_encode(&all)).unwrap(), all);
    assert_eq!(base16_decode("DEADbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    assert!(base16_decode("0g").is_err());
    assert!(base64_decode("a!").is_err());
    // base64 decode skips newlines and stops at '='
    assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
}

#[test]
fn compress_hash_matches_store_path_math() {
    // compressHash is exercised end-to-end by the store-path goldens; here
    // check basic XOR-folding properties.
    let h = hash_string(HashAlgorithm::Sha256, b"");
    let c = compress_hash(&h, 20);
    assert_eq!(c.as_bytes().len(), 20);
    let mut expect = [0u8; 20];
    for (i, b) in h.as_bytes().iter().enumerate() {
        expect[i % 20] ^= b;
    }
    assert_eq!(c.as_bytes(), expect);
    assert_eq!(c.to_nix32().len(), 32);
}
