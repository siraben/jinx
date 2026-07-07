//! Derivations: the ATerm (`Derive(...)`) format, output path computation,
//! and `hashDerivationModulo`.
//!
//! Port of `src/libstore/derivations.cc` and
//! `src/libstore/derived-path-map.{cc,hh}`.

use std::collections::{BTreeMap, BTreeSet};

use bstr::{BString, ByteSlice};

use crate::hash::{hash_string, BadHash, Hash, HashAlgorithm, HashFormat};
use crate::store_path::{
    output_path_name, BadStorePath, ContentAddress, ContentAddressMethod,
    ContentAddressWithReferences, StoreDir, StorePath, StorePathSet, TextInfo, DRV_EXTENSION,
};

/// Error while parsing or processing a derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrvError(pub String);

impl std::fmt::Display for DrvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DrvError {}

impl From<BadHash> for DrvError {
    fn from(e: BadHash) -> Self {
        DrvError(e.0)
    }
}

impl From<BadStorePath> for DrvError {
    fn from(e: BadStorePath) -> Self {
        DrvError(e.0)
    }
}

macro_rules! drv_err {
    ($($arg:tt)*) => { DrvError(format!($($arg)*)) };
}

/// Output names, e.g. `out`, `dev`.
pub type OutputName = String;

/// A single output of a [`Derivation`].
///
/// Port of `DerivationOutput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivationOutput {
    /// The traditional non-fixed-output derivation type.
    InputAddressed { path: StorePath },
    /// Fixed-output derivation: output path is content-addressed by a hash
    /// known up front.
    CAFixed { ca: ContentAddress },
    /// Floating content-addressed output (experimental `ca-derivations`).
    CAFloating {
        method: ContentAddressMethod,
        hash_algo: HashAlgorithm,
    },
    /// Input-addressed output whose path isn't known yet because it depends
    /// on a CA derivation.
    Deferred,
    /// Impure output (experimental `impure-derivations`).
    Impure {
        method: ContentAddressMethod,
        hash_algo: HashAlgorithm,
    },
}

impl DerivationOutput {
    /// Port of `DerivationOutput::path`: the store path of this output, if
    /// statically known.
    pub fn path(
        &self,
        store: &StoreDir,
        drv_name: &str,
        output_name: &str,
    ) -> Result<Option<StorePath>, DrvError> {
        match self {
            DerivationOutput::InputAddressed { path } => Ok(Some(path.clone())),
            DerivationOutput::CAFixed { ca } => {
                Ok(Some(ca_fixed_path(store, ca, drv_name, output_name)?))
            }
            _ => Ok(None),
        }
    }
}

/// Port of `DerivationOutput::CAFixed::path`.
fn ca_fixed_path(
    store: &StoreDir,
    ca: &ContentAddress,
    drv_name: &str,
    output_name: &str,
) -> Result<StorePath, DrvError> {
    Ok(store.make_fixed_output_path_from_ca(
        &output_path_name(drv_name, output_name),
        &ContentAddressWithReferences::without_refs(ca),
    )?)
}

/// A node of [`DerivedPathMap`]: the set of outputs requested from the
/// parent key, plus (for dynamic derivations) requests on those outputs'
/// own outputs.
///
/// Port of `DerivedPathMap<StringSet>::ChildNode`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DerivedPathMapNode {
    /// Output names requested directly.
    pub value: BTreeSet<OutputName>,
    /// For dynamic derivations: `output name -> further requests` on the
    /// derivation that the parent's output *is*.
    pub child_map: BTreeMap<OutputName, DerivedPathMapNode>,
}

/// The input derivations of a [`Derivation`]: a trie keyed on the `.drv`
/// store path, then (for dynamic derivations) on output names.
///
/// Port of `DerivedPathMap<StringSet>` (the shape used for `inputDrvs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DerivedPathMap {
    pub map: BTreeMap<StorePath, DerivedPathMapNode>,
}

/// Port of `Derivation` (including the `BasicDerivation` fields).
///
/// `env` keys/values, `platform`, `builder` and `args` are arbitrary bytes.
/// Unlike C++, structured attrs (`__json`) are *not* extracted from `env`;
/// they stay in `env`, which unparses identically.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Derivation {
    /// Outputs, keyed on symbolic IDs.
    pub outputs: BTreeMap<OutputName, DerivationOutput>,
    /// Inputs that are sources.
    pub input_srcs: StorePathSet,
    /// Inputs that are sub-derivations.
    pub input_drvs: DerivedPathMap,
    pub platform: BString,
    pub builder: BString,
    pub args: Vec<BString>,
    pub env: BTreeMap<BString, BString>,
    /// The derivation name (store path name without the `.drv` suffix).
    pub name: String,
}

/// Port of `DerivationType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivationType {
    InputAddressed { deferred: bool },
    ContentAddressed { sandboxed: bool, fixed: bool },
    Impure,
}

impl DerivationType {
    /// Port of `DerivationType::isFixed`.
    pub fn is_fixed(&self) -> bool {
        matches!(self, DerivationType::ContentAddressed { fixed: true, .. })
    }

    /// Port of `DerivationType::isCA`.
    pub fn is_ca(&self) -> bool {
        matches!(
            self,
            DerivationType::ContentAddressed { .. } | DerivationType::Impure
        )
    }

    /// Port of `DerivationType::hasKnownOutputPaths`.
    pub fn has_known_output_paths(&self) -> bool {
        match self {
            DerivationType::InputAddressed { deferred } => !deferred,
            DerivationType::ContentAddressed { fixed, .. } => *fixed,
            DerivationType::Impure => false,
        }
    }
}

impl Derivation {
    /// Port of `BasicDerivation::type()`.
    pub fn get_type(&self) -> Result<DerivationType, DrvError> {
        let mut floating_hash_algo: Option<HashAlgorithm> = None;
        let mut ty: Option<DerivationType> = None;

        let decide = |ty: &mut Option<DerivationType>,
                          new_ty: DerivationType|
         -> Result<(), DrvError> {
            match ty {
                None => {
                    *ty = Some(new_ty);
                    Ok(())
                }
                Some(t) if *t != new_ty => Err(drv_err!("can't mix derivation output types")),
                Some(t)
                    if *t
                        == (DerivationType::ContentAddressed {
                            sandboxed: false,
                            fixed: true,
                        }) =>
                {
                    Err(drv_err!("only one fixed output is allowed for now"))
                }
                _ => Ok(()),
            }
        };

        for (output_name, output) in &self.outputs {
            match output {
                DerivationOutput::InputAddressed { .. } => {
                    decide(&mut ty, DerivationType::InputAddressed { deferred: false })?;
                }
                DerivationOutput::CAFixed { .. } => {
                    decide(
                        &mut ty,
                        DerivationType::ContentAddressed {
                            sandboxed: false,
                            fixed: true,
                        },
                    )?;
                    if output_name != "out" {
                        return Err(drv_err!("single fixed output must be named \"out\""));
                    }
                }
                DerivationOutput::CAFloating { hash_algo, .. } => {
                    decide(
                        &mut ty,
                        DerivationType::ContentAddressed {
                            sandboxed: true,
                            fixed: false,
                        },
                    )?;
                    match floating_hash_algo {
                        None => floating_hash_algo = Some(*hash_algo),
                        Some(a) if a != *hash_algo => {
                            return Err(drv_err!(
                                "all floating outputs must use the same hash algorithm"
                            ))
                        }
                        _ => {}
                    }
                }
                DerivationOutput::Deferred => {
                    decide(&mut ty, DerivationType::InputAddressed { deferred: true })?;
                }
                DerivationOutput::Impure { .. } => {
                    decide(&mut ty, DerivationType::Impure)?;
                }
            }
        }

        ty.ok_or_else(|| drv_err!("must have at least one output"))
    }

    /// Port of `BasicDerivation::isBuiltin`.
    pub fn is_builtin(&self) -> bool {
        self.builder.starts_with(b"builtin:")
    }

    /// The references of the `.drv` store object: `inputSrcs` plus the input
    /// derivation paths. (Part of `infoForDerivation`.)
    pub fn drv_references(&self) -> StorePathSet {
        let mut references = self.input_srcs.clone();
        for drv_path in self.input_drvs.map.keys() {
            references.insert(drv_path.clone());
        }
        references
    }

    /// The store path the `.drv` file would be written to (a text-hashed
    /// path with references).
    ///
    /// Port of `computeStorePath` / the pure part of `writeDerivation` in
    /// `derivations.cc` (`infoForDerivation`).
    pub fn compute_store_path(&self, store: &StoreDir) -> Result<StorePath, DrvError> {
        let suffix = format!("{}{}", self.name, DRV_EXTENSION);
        let hash = self.unparse_hash(store, false, None)?;
        Ok(store.make_fixed_output_path_from_ca(
            &suffix,
            &ContentAddressWithReferences::Text(TextInfo {
                hash,
                references: self.drv_references(),
            }),
        )?)
    }

    /// Port of `Derivation::unparse`: render to the ATerm format, byte-exact
    /// with C++ Nix.
    ///
    /// If `mask_outputs` is set, output paths (in the outputs list and in
    /// env vars named after outputs) are replaced by empty strings.
    /// `actual_inputs`, if given, replaces `inputDrvs` (used by
    /// `hashDerivationModulo`, where keys are hash hex strings instead of
    /// store paths).
    pub fn unparse(
        &self,
        store: &StoreDir,
        mask_outputs: bool,
        actual_inputs: Option<&BTreeMap<String, DerivedPathMapNode>>,
    ) -> Result<Vec<u8>, DrvError> {
        let mut s: Vec<u8> = Vec::with_capacity(65536);
        self.unparse_into(store, mask_outputs, actual_inputs, &mut s)?;
        Ok(s)
    }

    /// Like [`Derivation::unparse`], but renders into a caller-provided buffer
    /// (cleared first). Lets hot paths that only hash the result reuse one
    /// allocation across many derivations.
    pub fn unparse_into(
        &self,
        store: &StoreDir,
        mask_outputs: bool,
        actual_inputs: Option<&BTreeMap<String, DerivedPathMapNode>>,
        s: &mut Vec<u8>,
    ) -> Result<(), DrvError> {
        s.clear();

        /* Use older unversioned form if possible, for wider compat. Use
        newer form only if we need it, which we do for dynamic
        derivations. */
        if self.has_dynamic_drv_dep() {
            s.extend_from_slice(b"DrvWithVersion(");
            print_unquoted_string(s, b"xp-dyn-drv");
            s.push(b',');
        } else {
            s.extend_from_slice(b"Derive(");
        }

        let mut first = true;
        s.push(b'[');
        for (output_name, output) in &self.outputs {
            if first {
                first = false;
            } else {
                s.push(b',');
            }
            s.push(b'(');
            print_unquoted_string(s, output_name.as_bytes());
            match output {
                DerivationOutput::InputAddressed { path } => {
                    s.push(b',');
                    if mask_outputs {
                        print_unquoted_string(s, b"");
                    } else {
                        print_unquoted_string(s, store.print_store_path(path).as_bytes());
                    }
                    s.push(b',');
                    print_unquoted_string(s, b"");
                    s.push(b',');
                    print_unquoted_string(s, b"");
                }
                DerivationOutput::CAFixed { ca } => {
                    s.push(b',');
                    if mask_outputs {
                        print_unquoted_string(s, b"");
                    } else {
                        let path = ca_fixed_path(store, ca, &self.name, output_name)?;
                        print_unquoted_string(s, store.print_store_path(&path).as_bytes());
                    }
                    s.push(b',');
                    print_unquoted_string(s, ca.print_method_algo().as_bytes());
                    s.push(b',');
                    print_unquoted_string(
                        s,
                        ca.hash.to_string(HashFormat::Base16, false).as_bytes(),
                    );
                }
                DerivationOutput::CAFloating { method, hash_algo } => {
                    s.push(b',');
                    print_unquoted_string(s, b"");
                    s.push(b',');
                    print_unquoted_string(
                        s,
                        format!("{}{}", method.render_prefix(), hash_algo.name()).as_bytes(),
                    );
                    s.push(b',');
                    print_unquoted_string(s, b"");
                }
                DerivationOutput::Deferred => {
                    s.push(b',');
                    print_unquoted_string(s, b"");
                    s.push(b',');
                    print_unquoted_string(s, b"");
                    s.push(b',');
                    print_unquoted_string(s, b"");
                }
                DerivationOutput::Impure { method, hash_algo } => {
                    s.push(b',');
                    print_unquoted_string(s, b"");
                    s.push(b',');
                    print_unquoted_string(
                        s,
                        format!("{}{}", method.render_prefix(), hash_algo.name()).as_bytes(),
                    );
                    s.push(b',');
                    print_unquoted_string(s, b"impure");
                }
            }
            s.push(b')');
        }

        s.extend_from_slice(b"],[");
        first = true;
        if let Some(actual_inputs) = actual_inputs {
            for (drv_hash_modulo, child_node) in actual_inputs {
                if first {
                    first = false;
                } else {
                    s.push(b',');
                }
                s.push(b'(');
                print_unquoted_string(s, drv_hash_modulo.as_bytes());
                unparse_derived_path_map_node(s, child_node);
                s.push(b')');
            }
        } else {
            for (drv_path, child_node) in &self.input_drvs.map {
                if first {
                    first = false;
                } else {
                    s.push(b',');
                }
                s.push(b'(');
                print_unquoted_string(s, store.print_store_path(drv_path).as_bytes());
                unparse_derived_path_map_node(s, child_node);
                s.push(b')');
            }
        }

        s.extend_from_slice(b"],");
        // inputSrcs, sorted; full paths sort the same as base names since
        // they share the store-dir prefix.
        s.push(b'[');
        first = true;
        for path in &self.input_srcs {
            if first {
                first = false;
            } else {
                s.push(b',');
            }
            print_unquoted_string(s, store.print_store_path(path).as_bytes());
        }
        s.push(b']');

        s.push(b',');
        print_unquoted_string(s, &self.platform);
        s.push(b',');
        print_string(s, &self.builder);
        s.push(b',');
        s.push(b'[');
        first = true;
        for arg in &self.args {
            if first {
                first = false;
            } else {
                s.push(b',');
            }
            print_string(s, arg);
        }
        s.push(b']');

        s.extend_from_slice(b",[");
        first = true;
        for (k, v) in &self.env {
            if first {
                first = false;
            } else {
                s.push(b',');
            }
            s.push(b'(');
            print_string(s, k);
            s.push(b',');
            let mask = mask_outputs
                && std::str::from_utf8(k)
                    .map(|k| self.outputs.contains_key(k))
                    .unwrap_or(false);
            print_string(s, if mask { b"" } else { v });
            s.push(b')');
        }

        s.extend_from_slice(b"])");

        Ok(())
    }

    /// Unparse and SHA-256 the result, reusing a thread-local buffer across
    /// calls. Used by the hashing paths (`compute_store_path`,
    /// `hashDerivationModulo`) which discard the ATerm text after hashing, so
    /// the per-derivation 64 KiB allocation is amortized away.
    fn unparse_hash(
        &self,
        store: &StoreDir,
        mask_outputs: bool,
        actual_inputs: Option<&BTreeMap<String, DerivedPathMapNode>>,
    ) -> Result<Hash, DrvError> {
        thread_local! {
            static SCRATCH: std::cell::RefCell<Vec<u8>> =
                std::cell::RefCell::new(Vec::with_capacity(65536));
        }
        SCRATCH.with(|b| {
            // try_borrow_mut guards against any (unexpected) reentrancy by
            // falling back to a fresh buffer rather than panicking.
            if let Ok(mut s) = b.try_borrow_mut() {
                self.unparse_into(store, mask_outputs, actual_inputs, &mut s)?;
                Ok(hash_string(HashAlgorithm::Sha256, &s[..]))
            } else {
                let mut s = Vec::with_capacity(65536);
                self.unparse_into(store, mask_outputs, actual_inputs, &mut s)?;
                Ok(hash_string(HashAlgorithm::Sha256, &s[..]))
            }
        })
    }

    /// Port of `hasDynamicDrvDep`.
    fn has_dynamic_drv_dep(&self) -> bool {
        self.input_drvs
            .map
            .values()
            .any(|node| !node.child_map.is_empty())
    }

    /// Port of `parseDerivation`: parse the ATerm format.
    ///
    /// `name` is the store path name without the `.drv` extension.
    pub fn parse(store: &StoreDir, s: &[u8], name: &str) -> Result<Derivation, DrvError> {
        let mut drv = Derivation {
            name: name.to_owned(),
            ..Default::default()
        };

        let mut str = Parser { remaining: s };
        str.expect(b"D")?;
        let version = match str.peek() {
            Some(b'e') => {
                str.expect(b"erive(")?;
                AtermVersion::Traditional
            }
            Some(b'r') => {
                str.expect(b"rvWithVersion(")?;
                let version_s = str.parse_string()?;
                if version_s != b"xp-dyn-drv" {
                    return Err(drv_err!(
                        "Unknown derivation ATerm format version '{}'",
                        version_s.as_bstr()
                    ));
                }
                str.expect(b",")?;
                AtermVersion::DynamicDerivations
            }
            _ => {
                return Err(drv_err!(
                    "derivation does not start with 'Derive' or 'DrvWithVersion'"
                ))
            }
        };

        /* Parse the list of outputs. */
        str.expect(b"[")?;
        while !str.end_of_list() {
            str.expect(b"(")?;
            let id = utf8(str.parse_string()?)?;
            let output = parse_derivation_output(store, &mut str)?;
            // C++ uses `emplace`, which keeps the first entry on duplicates.
            drv.outputs.entry(id).or_insert(output);
        }

        /* Parse the list of input derivations. */
        str.expect(b",[")?;
        while !str.end_of_list() {
            str.expect(b"(")?;
            let drv_path = utf8(str.parse_path()?)?;
            str.expect(b",")?;
            let node = parse_derived_path_map_node(&mut str, version)?;
            drv.input_drvs
                .map
                .insert(store.parse_store_path(&drv_path)?, node);
            str.expect(b")")?;
        }

        str.expect(b",")?;
        str.expect(b"[")?;
        while !str.end_of_list() {
            let p = utf8(str.parse_path()?)?;
            drv.input_srcs.insert(store.parse_store_path(&p)?);
        }
        str.expect(b",")?;
        drv.platform = str.parse_string()?.into();
        str.expect(b",")?;
        drv.builder = str.parse_string()?.into();

        /* Parse the builder arguments. */
        str.expect(b",[")?;
        while !str.end_of_list() {
            drv.args.push(str.parse_string()?.into());
        }

        /* Parse the environment variables. */
        str.expect(b",[")?;
        while !str.end_of_list() {
            str.expect(b"(")?;
            let name = str.parse_string()?;
            str.expect(b",")?;
            let value = str.parse_string()?;
            drv.env.insert(name.into(), value.into());
            str.expect(b")")?;
        }

        str.expect(b")")?;
        Ok(drv)
    }
}

fn utf8(v: Vec<u8>) -> Result<String, DrvError> {
    String::from_utf8(v).map_err(|e| drv_err!("invalid UTF-8 in derivation: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtermVersion {
    Traditional,
    DynamicDerivations,
}

/// Port of `unparseDerivedPathMapNode`.
fn unparse_derived_path_map_node(s: &mut Vec<u8>, node: &DerivedPathMapNode) {
    s.push(b',');
    if node.child_map.is_empty() {
        print_unquoted_strings(s, node.value.iter());
    } else {
        s.push(b'(');
        print_unquoted_strings(s, node.value.iter());
        s.extend_from_slice(b",[");
        let mut first = true;
        for (output_name, child_node) in &node.child_map {
            if first {
                first = false;
            } else {
                s.push(b',');
            }
            s.push(b'(');
            print_unquoted_string(s, output_name.as_bytes());
            unparse_derived_path_map_node(s, child_node);
            s.push(b')');
        }
        s.extend_from_slice(b"])");
    }
}

/// Port of `parseDerivedPathMapNode`.
fn parse_derived_path_map_node(
    str: &mut Parser<'_>,
    version: AtermVersion,
) -> Result<DerivedPathMapNode, DrvError> {
    let mut node = DerivedPathMapNode::default();

    let parse_strings_into =
        |str: &mut Parser<'_>, set: &mut BTreeSet<String>| -> Result<(), DrvError> {
            str.expect(b"[")?;
            while !str.end_of_list() {
                set.insert(utf8(str.parse_string()?)?);
            }
            Ok(())
        };

    match version {
        AtermVersion::Traditional => parse_strings_into(str, &mut node.value)?,
        AtermVersion::DynamicDerivations => match str.peek() {
            Some(b'[') => parse_strings_into(str, &mut node.value)?,
            Some(b'(') => {
                str.expect(b"(")?;
                parse_strings_into(str, &mut node.value)?;
                str.expect(b",[")?;
                while !str.end_of_list() {
                    str.expect(b"(")?;
                    let output_name = utf8(str.parse_string()?)?;
                    str.expect(b",")?;
                    let child = parse_derived_path_map_node(str, version)?;
                    node.child_map.insert(output_name, child);
                    str.expect(b")")?;
                }
                str.expect(b")")?;
            }
            _ => return Err(drv_err!("invalid inputDrvs entry in derivation")),
        },
    }
    Ok(node)
}

/// Port of the streaming `parseDerivationOutput` (reads `,path,algo,hash)`).
fn parse_derivation_output(
    store: &StoreDir,
    str: &mut Parser<'_>,
) -> Result<DerivationOutput, DrvError> {
    str.expect(b",")?;
    let path_s = utf8(str.parse_string()?)?;
    str.expect(b",")?;
    let hash_algo_s = utf8(str.parse_string()?)?;
    str.expect(b",")?;
    let hash_s = utf8(str.parse_string()?)?;
    str.expect(b")")?;

    if !hash_algo_s.is_empty() {
        let mut rest: &str = &hash_algo_s;
        let method = ContentAddressMethod::parse_prefix(&mut rest);
        let hash_algo = HashAlgorithm::parse(rest)?;
        if hash_s == "impure" {
            if !path_s.is_empty() {
                return Err(drv_err!(
                    "impure derivation output should not specify output path"
                ));
            }
            Ok(DerivationOutput::Impure { method, hash_algo })
        } else if !hash_s.is_empty() {
            validate_path(&path_s)?;
            let hash = Hash::parse_non_sri_unprefixed(&hash_s, hash_algo)?;
            Ok(DerivationOutput::CAFixed {
                ca: ContentAddress { method, hash },
            })
        } else {
            if !path_s.is_empty() {
                return Err(drv_err!(
                    "content-addressing derivation output should not specify output path"
                ));
            }
            Ok(DerivationOutput::CAFloating { method, hash_algo })
        }
    } else if path_s.is_empty() {
        Ok(DerivationOutput::Deferred)
    } else {
        validate_path(&path_s)?;
        Ok(DerivationOutput::InputAddressed {
            path: store.parse_store_path(&path_s)?,
        })
    }
}

/// Port of `validatePath`.
fn validate_path(s: &str) -> Result<(), DrvError> {
    if s.is_empty() || !s.starts_with('/') {
        return Err(drv_err!("bad path '{s}' in derivation"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ATerm string printing (port of printString / printUnquotedString)
// ---------------------------------------------------------------------------

/// Port of `printString`: quote and escape `"`, `\`, `\n`, `\r`, `\t`.
fn print_string(res: &mut Vec<u8>, s: &[u8]) {
    res.push(b'"');
    // Scan for the next byte that needs escaping and bulk-copy the clean span
    // before it; the common (no-escape) case becomes a single extend.
    let mut start = 0;
    for (i, &c) in s.iter().enumerate() {
        let esc: &[u8] = match c {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            _ => continue,
        };
        res.extend_from_slice(&s[start..i]);
        res.extend_from_slice(esc);
        start = i + 1;
    }
    res.extend_from_slice(&s[start..]);
    res.push(b'"');
}

/// Port of `printUnquotedString`: quote without escaping.
fn print_unquoted_string(res: &mut Vec<u8>, s: &[u8]) {
    res.push(b'"');
    res.extend_from_slice(s);
    res.push(b'"');
}

fn print_unquoted_strings<'a>(res: &mut Vec<u8>, iter: impl Iterator<Item = &'a String>) {
    res.push(b'[');
    let mut first = true;
    for s in iter {
        if first {
            first = false;
        } else {
            res.push(b',');
        }
        print_unquoted_string(res, s.as_bytes());
    }
    res.push(b']');
}

// ---------------------------------------------------------------------------
// ATerm parser (port of StringViewStream & helpers)
// ---------------------------------------------------------------------------

struct Parser<'a> {
    remaining: &'a [u8],
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.remaining.first().copied()
    }

    fn get(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.remaining = &self.remaining[1..];
        Some(c)
    }

    /// Port of `expect`.
    fn expect(&mut self, s: &[u8]) -> Result<(), DrvError> {
        if !self.remaining.starts_with(s) {
            return Err(drv_err!("expected string '{}'", s.as_bstr()));
        }
        self.remaining = &self.remaining[s.len()..];
        Ok(())
    }

    /// Port of `endOfList`.
    fn end_of_list(&mut self) -> bool {
        match self.peek() {
            Some(b',') => {
                self.get();
                false
            }
            Some(b']') => {
                self.get();
                true
            }
            _ => false,
        }
    }

    /// Port of `parseString`: a quoted string with `\X` escapes, where
    /// `\n`, `\r`, `\t` map to control characters and any other escaped
    /// character maps to itself.
    fn parse_string(&mut self) -> Result<Vec<u8>, DrvError> {
        self.expect(b"\"")?;
        let data = self.remaining;

        // Find the closing quote: a '"' preceded by an even number of
        // backslashes.
        let mut start = 0usize;
        let mut end = None;
        while start < data.len() {
            let idx = match data[start..].iter().position(|&c| c == b'"') {
                Some(i) => start + i,
                None => break,
            };
            let mut pos = idx;
            while pos > 0 && data[pos - 1] == b'\\' {
                pos -= 1;
            }
            if (idx - pos) % 2 == 0 {
                end = Some(idx);
                break;
            }
            start = idx + 1;
        }
        let end = end.ok_or_else(|| drv_err!("unterminated string in derivation"))?;

        let content = &data[..end];
        self.remaining = &data[end + 1..];

        // Fast path: no escapes.
        if !content.contains(&b'\\') {
            return Ok(content.to_vec());
        }

        let mut res = Vec::with_capacity(end);
        let mut i = 0;
        while i < content.len() {
            match content[i] {
                b'\\' => {
                    // A trailing backslash cannot occur: the closing-quote
                    // scan guarantees an even number of backslashes before
                    // `end`.
                    let c = content[i + 1];
                    res.push(match c {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        other => other,
                    });
                    i += 2;
                }
                c => {
                    res.push(c);
                    i += 1;
                }
            }
        }
        Ok(res)
    }

    /// Port of `parsePath`.
    fn parse_path(&mut self) -> Result<Vec<u8>, DrvError> {
        let s = self.parse_string()?;
        if s.is_empty() || s[0] != b'/' {
            return Err(drv_err!("bad path '{}' in derivation", s.as_bstr()));
        }
        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// hashDerivationModulo (port of derivations.cc:900)
// ---------------------------------------------------------------------------

/// Result of [`hash_derivation_modulo`].
///
/// Port of `DrvHashModulo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrvHashModulo {
    /// Single hash of the derivation (input-addressed derivations without
    /// floating-CA dependencies).
    DrvHash(Hash),
    /// Fixed-output derivations: per-output hashes, unique up to the
    /// output's contents.
    CaOutputHashes(BTreeMap<OutputName, Hash>),
    /// Output hashes not yet known (floating CA / impure / dynamic deps).
    DeferredDrv,
}

/// Memoization cache for [`hash_derivation_modulo`], keyed on `.drv` store
/// path. (Analogue of the global `drvHashes` in C++.)
pub type DrvHashes = rustc_hash::FxHashMap<StorePath, DrvHashModulo>;

/// Abstraction over "read derivation by store path", needed to recurse into
/// input derivations. (Stands in for `Store::readInvalidDerivation`.)
pub trait DrvResolver {
    fn read_derivation(&mut self, drv_path: &StorePath) -> Result<Derivation, DrvError>;
}

impl<F> DrvResolver for F
where
    F: FnMut(&StorePath) -> Result<Derivation, DrvError>,
{
    fn read_derivation(&mut self, drv_path: &StorePath) -> Result<Derivation, DrvError> {
        self(drv_path)
    }
}

/// Port of `pathDerivationModulo`: memoized `hash_derivation_modulo` of the
/// derivation at `drv_path`.
pub fn path_derivation_modulo(
    store: &StoreDir,
    memo: &mut DrvHashes,
    resolver: &mut dyn DrvResolver,
    drv_path: &StorePath,
) -> Result<DrvHashModulo, DrvError> {
    if let Some(h) = memo.get(drv_path) {
        return Ok(h.clone());
    }
    let drv = resolver.read_derivation(drv_path)?;
    let h = hash_derivation_modulo(store, memo, resolver, &drv, false)?;
    memo.insert(drv_path.clone(), h.clone());
    Ok(h)
}

/// Port of `hashDerivationModulo`.
///
/// Returns hashes with the details of fixed-output subderivations expunged:
///
/// - For fixed-output derivations: a map from output name to
///   `sha256("fixed:out:<methodPrefix><algo>:<hash-base16>:<store-path>")`.
/// - For floating-CA / impure derivations (or ones with dynamic
///   derivation deps): [`DrvHashModulo::DeferredDrv`].
/// - Otherwise: the sha256 of the ATerm after substituting each input drv
///   path with its own `hashDerivationModulo` (fixed-output inputs become
///   single-`out` pseudo-derivations, to avoid leaking provenance).
pub fn hash_derivation_modulo(
    store: &StoreDir,
    memo: &mut DrvHashes,
    resolver: &mut dyn DrvResolver,
    drv: &Derivation,
    mask_outputs: bool,
) -> Result<DrvHashModulo, DrvError> {
    let ty = drv.get_type()?;

    /* Return a fixed hash for fixed-output derivations. */
    if ty.is_fixed() {
        let mut output_hashes = BTreeMap::new();
        for (output_name, output) in &drv.outputs {
            let ca = match output {
                DerivationOutput::CAFixed { ca } => ca,
                // `is_fixed` guarantees all outputs are CAFixed.
                _ => unreachable!(),
            };
            let path = ca_fixed_path(store, ca, &drv.name, output_name)?;
            let hash = hash_string(
                HashAlgorithm::Sha256,
                format!(
                    "fixed:out:{}:{}:{}",
                    ca.print_method_algo(),
                    ca.hash.to_string(HashFormat::Base16, false),
                    store.print_store_path(&path)
                ),
            );
            output_hashes.insert(output_name.clone(), hash);
        }
        return Ok(DrvHashModulo::CaOutputHashes(output_hashes));
    }

    // Floating-CA or impure: deferred.
    match ty {
        DerivationType::InputAddressed { .. } => {
            /* This might be a "pessimistically" deferred output, so we don't
            "taint" the kind yet. */
        }
        DerivationType::ContentAddressed { .. } | DerivationType::Impure => {
            return Ok(DrvHashModulo::DeferredDrv);
        }
    }

    /* For other derivations, replace the inputs paths with recursive calls
    to this function. */
    let mut inputs2: BTreeMap<String, DerivedPathMapNode> = BTreeMap::new();
    for (drv_path, node) in &drv.input_drvs.map {
        /* Need to build and resolve dynamic derivations first */
        if !node.child_map.is_empty() {
            return Ok(DrvHashModulo::DeferredDrv);
        }

        let res = path_derivation_modulo(store, memo, resolver, drv_path)?;
        match res {
            DrvHashModulo::DeferredDrv => return Ok(DrvHashModulo::DeferredDrv),
            // Regular non-CA derivation, replace derivation
            DrvHashModulo::DrvHash(drv_hash) => {
                inputs2.insert(
                    drv_hash.to_string(HashFormat::Base16, false),
                    node.clone(),
                );
            }
            // CA derivation's output hashes
            DrvHashModulo::CaOutputHashes(output_hashes) => {
                for output_name in &node.value {
                    /* Put each one in with a single "out" output. */
                    let h = output_hashes.get(output_name).ok_or_else(|| {
                        drv_err!(
                            "no hash for output '{output_name}' of derivation '{}'",
                            drv.name
                        )
                    })?;
                    inputs2.insert(
                        h.to_string(HashFormat::Base16, false),
                        DerivedPathMapNode {
                            value: BTreeSet::from(["out".to_owned()]),
                            child_map: BTreeMap::new(),
                        },
                    );
                }
            }
        }
    }

    Ok(DrvHashModulo::DrvHash(
        drv.unparse_hash(store, mask_outputs, Some(&inputs2))?,
    ))
}
