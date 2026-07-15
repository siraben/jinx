//! Store paths, the store directory, and content addresses.
//!
//! Ports of `src/libstore/path.cc`, `src/libstore/store-dir-config.cc` and
//! `src/libstore/content-address.cc`.

use std::collections::BTreeSet;

use crate::hash::{compress_hash, hash_string, BadHash, Hash, HashAlgorithm, HashFormat};

/// Error for invalid store paths / store path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadStorePath(pub String);

impl std::fmt::Display for BadStorePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BadStorePath {}

macro_rules! bad_path {
    ($($arg:tt)*) => { BadStorePath(format!($($arg)*)) };
}

/// The file extension of derivations, `.drv`.
pub const DRV_EXTENSION: &str = ".drv";

/// Check whether a name is a valid store path name.
///
/// Port of `checkName` in `src/libstore/path.cc`.
pub fn check_name(name: &str) -> Result<(), BadStorePath> {
    if name.is_empty() {
        return Err(bad_path!("name must not be empty"));
    }
    if name.len() > StorePath::MAX_PATH_LEN {
        return Err(bad_path!(
            "name '{name}' must be no longer than {} characters",
            StorePath::MAX_PATH_LEN
        ));
    }
    let bytes = name.as_bytes();
    if bytes[0] == b'.' {
        // check against "." and "..", followed by end or dash
        if bytes.len() == 1 {
            return Err(bad_path!("name '{name}' is not valid"));
        }
        if bytes[1] == b'-' {
            return Err(bad_path!(
                "name '{name}' is not valid: first dash-separated component must not be '.'"
            ));
        }
        if bytes[1] == b'.' {
            if bytes.len() == 2 {
                return Err(bad_path!("name '{name}' is not valid"));
            }
            if bytes[2] == b'-' {
                return Err(bad_path!(
                    "name '{name}' is not valid: first dash-separated component must not be '..'"
                ));
            }
        }
    }
    for &c in bytes {
        if !(c.is_ascii_digit()
            || c.is_ascii_lowercase()
            || c.is_ascii_uppercase()
            || c == b'+'
            || c == b'-'
            || c == b'.'
            || c == b'_'
            || c == b'?'
            || c == b'=')
        {
            return Err(bad_path!(
                "name '{name}' contains illegal character '{}'",
                c as char
            ));
        }
    }
    Ok(())
}

/// A store path: `<nix32 of 20-byte digest>-<name>` (without the store dir).
///
/// Port of `StorePath` in `src/libstore/path.{cc,hh}`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StorePath {
    base_name: String,
}

impl StorePath {
    /// Size of the hash part of store paths, in nix32 characters (160 bits).
    pub const HASH_LEN: usize = 32;

    pub const MAX_PATH_LEN: usize = 211;

    /// Parse a base name like `q2...z1-name` (no store dir).
    pub fn new(base_name: &str) -> Result<Self, BadStorePath> {
        if base_name.len() < Self::HASH_LEN + 1 {
            return Err(bad_path!(
                "'{base_name}' is too short to be a valid store path"
            ));
        }
        for &c in base_name.as_bytes()[..Self::HASH_LEN].iter() {
            if c == b'e'
                || c == b'o'
                || c == b'u'
                || c == b't'
                || !(c.is_ascii_digit() || c.is_ascii_lowercase())
            {
                return Err(bad_path!(
                    "store path '{base_name}' contains illegal base-32 character '{}'",
                    c as char
                ));
            }
        }
        // Note: matching C++, the separator character at index HASH_LEN is
        // *not* validated; the name starts unconditionally at HASH_LEN + 1.
        // (If the string is exactly HASH_LEN + 1 long, the name is empty and
        // rejected by check_name.)
        let name = match base_name.get(Self::HASH_LEN + 1..) {
            Some(n) => n,
            // Slicing can only fail on a UTF-8 boundary issue at the
            // separator position; treat like an invalid name.
            None => {
                return Err(bad_path!(
                    "path '{base_name}' is not a valid store path: name is not valid"
                ))
            }
        };
        check_name(name)
            .map_err(|e| bad_path!("path '{base_name}' is not a valid store path: {e}"))?;
        Ok(StorePath {
            base_name: base_name.to_owned(),
        })
    }

    /// Construct from a 20-byte digest (nix32-rendered) and a name.
    pub fn from_hash_part(hash_part: &crate::hash::CompressedHash, name: &str) -> Result<Self, BadStorePath> {
        let mut base_name = hash_part.to_nix32();
        base_name.push('-');
        base_name.push_str(name);
        check_name(name)
            .map_err(|e| bad_path!("path '{base_name}' is not a valid store path: {e}"))?;
        Ok(StorePath { base_name })
    }

    /// Construct from a full [`Hash`] (e.g. sha1 for `StorePath::random`).
    pub fn from_hash(hash: &Hash, name: &str) -> Result<Self, BadStorePath> {
        let mut base_name = hash.to_string(HashFormat::Nix32, false);
        base_name.push('-');
        base_name.push_str(name);
        check_name(name)
            .map_err(|e| bad_path!("path '{base_name}' is not a valid store path: {e}"))?;
        Ok(StorePath { base_name })
    }

    /// The whole base name, `<hash>-<name>`.
    pub fn to_string(&self) -> &str {
        &self.base_name
    }

    /// The name part.
    pub fn name(&self) -> &str {
        &self.base_name[Self::HASH_LEN + 1..]
    }

    /// The nix32 hash part.
    pub fn hash_part(&self) -> &str {
        &self.base_name[..Self::HASH_LEN]
    }

    /// Whether the name ends in `.drv`.
    pub fn is_derivation(&self) -> bool {
        self.name().ends_with(DRV_EXTENSION)
    }
}

impl std::fmt::Debug for StorePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StorePath({})", self.base_name)
    }
}

impl std::fmt::Display for StorePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.base_name)
    }
}

/// A sorted set of store paths (C++ `StorePathSet`).
pub type StorePathSet = BTreeSet<StorePath>;

// ---------------------------------------------------------------------------
// Content addresses (port of content-address.{cc,hh})
// ---------------------------------------------------------------------------

/// An enumeration of the ways we can serialize file system objects.
///
/// Port of `FileIngestionMethod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileIngestionMethod {
    /// Flat-file hashing: the contents of a single file.
    Flat,
    /// Hash the NAR serialization ("recursive").
    NixArchive,
    /// Git object hashing (experimental in C++ Nix).
    Git,
}

impl FileIngestionMethod {
    /// Port of `makeFileIngestionPrefix`.
    pub fn prefix(self) -> &'static str {
        match self {
            FileIngestionMethod::Flat => "", // not prefixed, for back compat
            FileIngestionMethod::NixArchive => "r:",
            FileIngestionMethod::Git => "git:",
        }
    }
}

/// How a store object is content-addressed.
///
/// Port of `ContentAddressMethod` (the `Raw` enum, flattened).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ContentAddressMethod {
    /// Flat-file hash of the contents.
    Flat,
    /// Hash of the NAR serialization.
    NixArchive,
    /// Git object hashing.
    Git,
    /// Like `Flat`, but the store object may have references
    /// (`builtins.toFile` / `.drv` files).
    Text,
}

impl ContentAddressMethod {
    /// Port of `ContentAddressMethod::getFileIngestionMethod`.
    pub fn file_ingestion_method(self) -> FileIngestionMethod {
        match self {
            ContentAddressMethod::Flat => FileIngestionMethod::Flat,
            ContentAddressMethod::NixArchive => FileIngestionMethod::NixArchive,
            ContentAddressMethod::Git => FileIngestionMethod::Git,
            ContentAddressMethod::Text => FileIngestionMethod::Flat,
        }
    }

    /// Port of `ContentAddressMethod::renderPrefix`:
    /// `""` (flat), `"r:"`, `"git:"` or `"text:"`.
    pub fn render_prefix(self) -> &'static str {
        match self {
            ContentAddressMethod::Text => "text:",
            _ => self.file_ingestion_method().prefix(),
        }
    }

    /// Port of `ContentAddressMethod::parsePrefix`: strips a recognized
    /// prefix from `s` and returns the method (`Flat` if no prefix).
    pub fn parse_prefix(s: &mut &str) -> ContentAddressMethod {
        for (prefix, method) in [
            ("r:", ContentAddressMethod::NixArchive),
            ("git:", ContentAddressMethod::Git),
            ("text:", ContentAddressMethod::Text),
        ] {
            if let Some(rest) = s.strip_prefix(prefix) {
                *s = rest;
                return method;
            }
        }
        ContentAddressMethod::Flat
    }

    /// Port of `renderPrefixModern`: `"text:"` or `"fixed:"` (+ `r:`/`git:`).
    fn render_prefix_modern(self) -> String {
        match self {
            ContentAddressMethod::Text => "text:".into(),
            _ => format!("fixed:{}", self.file_ingestion_method().prefix()),
        }
    }
}

/// A content address: how to compute the store object's digest, plus the
/// digest itself.
///
/// Port of `ContentAddress`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentAddress {
    pub method: ContentAddressMethod,
    pub hash: Hash,
}

impl ContentAddress {
    /// Port of `ContentAddress::render`, e.g. `fixed:r:sha256:1abc...`
    /// (hash in nix32) or `text:sha256:...`.
    pub fn render(&self) -> String {
        format!(
            "{}{}",
            self.method.render_prefix_modern(),
            self.hash.to_string(HashFormat::Nix32, true)
        )
    }

    /// Port of `ContentAddress::parse`.
    pub fn parse(raw: &str) -> Result<Self, BadHash> {
        let (prefix, mut rest) = raw
            .split_once(':')
            .ok_or_else(|| BadHash(format!(
                "not a content address because it is not in the form '<prefix>:<rest>': {raw}"
            )))?;
        let method = match prefix {
            "text" => ContentAddressMethod::Text,
            "fixed" => {
                // C++ only recognizes "r:" and "git:" after "fixed:"
                // ("fixed:text:..." fails hash-algo parsing below).
                if let Some(r) = rest.strip_prefix("r:") {
                    rest = r;
                    ContentAddressMethod::NixArchive
                } else if let Some(r) = rest.strip_prefix("git:") {
                    rest = r;
                    ContentAddressMethod::Git
                } else {
                    ContentAddressMethod::Flat
                }
            }
            _ => {
                return Err(BadHash(format!(
                    "content address prefix '{prefix}' is unrecognized. Recogonized prefixes are 'text' or 'fixed'"
                )))
            }
        };
        let (algo_s, hash_s) = rest.split_once(':').ok_or_else(|| {
            BadHash(format!(
                "content address hash must be in form '<algo>:<hash>', but found: {raw}"
            ))
        })?;
        let algo = HashAlgorithm::parse(algo_s)?;
        Ok(ContentAddress {
            method,
            hash: Hash::parse_non_sri_unprefixed(hash_s, algo)?,
        })
    }

    /// Port of `ContentAddress::printMethodAlgo`, e.g. `r:sha256`,
    /// `sha256`, `text:sha256`. (Used in `.drv` ATerm output and in the
    /// fixed-output case of `hashDerivationModulo`.)
    pub fn print_method_algo(&self) -> String {
        format!("{}{}", self.method.render_prefix(), self.hash.algo.name())
    }
}

/// References to other store objects (plus optional self-reference).
///
/// Port of `StoreReferences`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreReferences {
    pub others: StorePathSet,
    pub self_ref: bool,
}

impl StoreReferences {
    pub fn is_empty(&self) -> bool {
        !self.self_ref && self.others.is_empty()
    }
}

/// Port of `TextInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInfo {
    pub hash: Hash,
    /// No self-references allowed.
    pub references: StorePathSet,
}

/// Port of `FixedOutputInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedOutputInfo {
    pub method: FileIngestionMethod,
    pub hash: Hash,
    pub references: StoreReferences,
}

/// Port of `ContentAddressWithReferences`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentAddressWithReferences {
    Text(TextInfo),
    Fixed(FixedOutputInfo),
}

impl ContentAddressWithReferences {
    /// Port of `ContentAddressWithReferences::withoutRefs`.
    pub fn without_refs(ca: &ContentAddress) -> Self {
        match ca.method {
            ContentAddressMethod::Text => ContentAddressWithReferences::Text(TextInfo {
                hash: ca.hash,
                references: StorePathSet::new(),
            }),
            _ => ContentAddressWithReferences::Fixed(FixedOutputInfo {
                method: ca.method.file_ingestion_method(),
                hash: ca.hash,
                references: StoreReferences::default(),
            }),
        }
    }

    /// Port of `ContentAddressWithReferences::fromParts`.
    pub fn from_parts(
        method: ContentAddressMethod,
        hash: Hash,
        refs: StoreReferences,
    ) -> Result<Self, BadStorePath> {
        match method {
            ContentAddressMethod::Text => {
                if refs.self_ref {
                    return Err(bad_path!("self-reference not allowed with text hashing"));
                }
                Ok(ContentAddressWithReferences::Text(TextInfo {
                    hash,
                    references: refs.others,
                }))
            }
            _ => Ok(ContentAddressWithReferences::Fixed(FixedOutputInfo {
                method: method.file_ingestion_method(),
                hash,
                references: refs,
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// Store dir handling (port of store-dir-config.cc)
// ---------------------------------------------------------------------------

/// A store directory (default `/nix/store`) and the path-making algorithms.
///
/// Port of `StoreDirConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreDir {
    store_dir: String,
}

impl Default for StoreDir {
    fn default() -> Self {
        StoreDir {
            store_dir: "/nix/store".to_owned(),
        }
    }
}

impl StoreDir {
    pub fn new(store_dir: impl Into<String>) -> Self {
        StoreDir {
            store_dir: store_dir.into(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.store_dir
    }

    /// Port of `printStorePath`: `<storeDir>/<baseName>`.
    pub fn print_store_path(&self, path: &StorePath) -> String {
        use std::fmt::Write;

        // Render directly into the result. `format!` plus `path.to_string()`
        // built an intermediate allocation for every store-path print, which
        // is particularly frequent while serializing derivations.
        let mut out =
            String::with_capacity(self.store_dir.len() + 1 + 32 + 1 + path.name().len());
        out.push_str(&self.store_dir);
        out.push('/');
        write!(&mut out, "{path}").expect("writing to a String cannot fail");
        out
    }

    /// Port of `parseStorePath`. Note: unlike C++, no general path
    /// canonicalization is performed; the path must be exactly
    /// `<storeDir>/<baseName>` (which is what the evaluator produces).
    pub fn parse_store_path(&self, path: &str) -> Result<StorePath, BadStorePath> {
        if path.is_empty() {
            return Err(bad_path!("empty path is not a valid store path"));
        }
        let rest = path
            .strip_prefix(self.store_dir.as_str())
            .and_then(|r| r.strip_prefix('/'))
            .ok_or_else(|| bad_path!("path '{path}' is not in the Nix store"))?;
        if rest.is_empty() || rest.contains('/') {
            return Err(bad_path!("path '{path}' is not in the Nix store"));
        }
        StorePath::new(rest)
    }

    /// Port of `maybeParseStorePath`.
    pub fn maybe_parse_store_path(&self, path: &str) -> Option<StorePath> {
        self.parse_store_path(path).ok()
    }

    /// Port of `isStorePath`.
    pub fn is_store_path(&self, path: &str) -> bool {
        self.maybe_parse_store_path(path).is_some()
    }

    /// Port of `makeStorePath` (string-hash overload).
    ///
    /// Computes `nix32(compress20(sha256("<type>:<hash>:<storeDir>:<name>")))-<name>`.
    pub fn make_store_path_raw(
        &self,
        type_: &str,
        hash: &str,
        name: &str,
    ) -> Result<StorePath, BadStorePath> {
        let s = format!("{type_}:{hash}:{}:{name}", self.store_dir);
        let h = compress_hash(&hash_string(HashAlgorithm::Sha256, &s), 20);
        StorePath::from_hash_part(&h, name)
    }

    /// Port of `makeStorePath` (Hash overload): renders the hash as
    /// `<algo>:<base16>`.
    pub fn make_store_path(
        &self,
        type_: &str,
        hash: &Hash,
        name: &str,
    ) -> Result<StorePath, BadStorePath> {
        self.make_store_path_raw(type_, &hash.to_string(HashFormat::Base16, true), name)
    }

    /// Port of `makeOutputPath`.
    pub fn make_output_path(
        &self,
        id: &str,
        hash: &Hash,
        name: &str,
    ) -> Result<StorePath, BadStorePath> {
        self.make_store_path(
            &format!("output:{id}"),
            hash,
            &output_path_name(name, id),
        )
    }

    /// Port of the static `makeType`: stuffs references into the type.
    fn make_type(&self, mut type_: String, references: &StoreReferences) -> String {
        for i in &references.others {
            type_.push(':');
            type_.push_str(&self.print_store_path(i));
        }
        if references.self_ref {
            type_.push_str(":self");
        }
        type_
    }

    /// Port of `makeFixedOutputPath`.
    pub fn make_fixed_output_path(
        &self,
        name: &str,
        info: &FixedOutputInfo,
    ) -> Result<StorePath, BadStorePath> {
        if info.method == FileIngestionMethod::Git
            && !(info.hash.algo == HashAlgorithm::Sha1 || info.hash.algo == HashAlgorithm::Sha256)
        {
            return Err(bad_path!(
                "Git file ingestion must use SHA-1 or SHA-256 hash, but instead using: {}",
                info.hash.algo.name()
            ));
        }

        if info.hash.algo == HashAlgorithm::Sha256
            && info.method == FileIngestionMethod::NixArchive
        {
            self.make_store_path(
                &self.make_type("source".to_owned(), &info.references),
                &info.hash,
                name,
            )
        } else {
            if !info.references.is_empty() {
                return Err(bad_path!(
                    "fixed output derivation '{name}' is not allowed to refer to other store paths."
                ));
            }
            // make a unique digest based on the parameters for creating this store object
            let payload = format!(
                "fixed:out:{}{}:",
                info.method.prefix(),
                info.hash.to_string(HashFormat::Base16, true)
            );
            let digest = hash_string(HashAlgorithm::Sha256, &payload);
            self.make_store_path("output:out", &digest, name)
        }
    }

    /// Port of `makeFixedOutputPathFromCA`.
    pub fn make_fixed_output_path_from_ca(
        &self,
        name: &str,
        ca: &ContentAddressWithReferences,
    ) -> Result<StorePath, BadStorePath> {
        match ca {
            ContentAddressWithReferences::Text(ti) => {
                assert_eq!(ti.hash.algo, HashAlgorithm::Sha256);
                self.make_store_path(
                    &self.make_type(
                        "text".to_owned(),
                        &StoreReferences {
                            others: ti.references.clone(),
                            self_ref: false,
                        },
                    ),
                    &ti.hash,
                    name,
                )
            }
            ContentAddressWithReferences::Fixed(foi) => self.make_fixed_output_path(name, foi),
        }
    }

    /// Convenience: the store path of a "text" object (e.g.
    /// `builtins.toFile`, `.drv` files): sha256 of the contents, with
    /// references.
    pub fn make_text_path(
        &self,
        name: &str,
        contents: &[u8],
        references: &StorePathSet,
    ) -> Result<StorePath, BadStorePath> {
        let hash = hash_string(HashAlgorithm::Sha256, contents);
        self.make_fixed_output_path_from_ca(
            name,
            &ContentAddressWithReferences::Text(TextInfo {
                hash,
                references: references.clone(),
            }),
        )
    }
}

/// Port of `outputPathName`: `<drvName>` for output `out`,
/// `<drvName>-<outputName>` otherwise.
pub fn output_path_name(drv_name: &str, output_name: &str) -> String {
    if output_name == "out" {
        drv_name.to_owned()
    } else {
        format!("{drv_name}-{output_name}")
    }
}
