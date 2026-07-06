//! Input schemes, the [`Input`] type, and the scheme registry.
//!
//! Port of the scheme-dispatch parts of `src/libfetchers/fetchers.{cc,hh}`.

use jinx_store::store_path::StorePath;

use crate::attrs::{get_str, maybe_get_int, maybe_get_str, AttrError, Attrs};
use crate::url::ParsedUrl;
use crate::{FetchError, FetchedTree, StoreWriter};

/// A fetcher input: a set of attributes plus the scheme that understands them.
///
/// Port of `fetchers::Input`. The scheme is looked up by the `type` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub attrs: Attrs,
    /// `None` for a "raw"/unsupported input (no matching scheme).
    scheme_name: Option<&'static str>,
}

impl Input {
    /// Port of `Input::fromURL`: try every registered scheme in turn.
    pub fn from_url(url: &str) -> Result<Input, FetchError> {
        // Plain paths (no scheme, or a bare absolute/relative path) are handled
        // by the path scheme's plain-path recognizer.
        let parsed = ParsedUrl::parse(url);
        for scheme in registry() {
            if let Some(res) = scheme.input_from_url(url, parsed.as_ref()) {
                return res;
            }
        }
        Err(FetchError(format!("input '{url}' is unsupported")))
    }

    /// Port of `Input::fromAttrs`: dispatch on the `type` attribute and
    /// validate the attribute names against the scheme's whitelist.
    pub fn from_attrs(attrs: Attrs) -> Result<Input, FetchError> {
        let ty = get_str(&attrs, "type")
            .map_err(|_| FetchError("attribute 'type' is missing in input".into()))?
            .to_string();
        for scheme in registry() {
            if scheme.scheme_name() == ty {
                // Validate attr names.
                for name in attrs.keys() {
                    if name == "type" || name == "__final" {
                        continue;
                    }
                    if !scheme.allowed_attrs().contains(&name.as_str()) {
                        return Err(FetchError(format!(
                            "input attribute '{name}' not supported by scheme '{ty}'"
                        )));
                    }
                }
                return scheme.input_from_attrs(attrs);
            }
        }
        // Raw input: keep attrs, no scheme.
        Ok(Input { attrs, scheme_name: None })
    }

    /// Construct an input with a known scheme (used by scheme implementations).
    pub fn with_scheme(attrs: Attrs, scheme_name: &'static str) -> Input {
        Input { attrs, scheme_name: Some(scheme_name) }
    }

    /// Port of `Input::toAttrs`.
    pub fn to_attrs(&self) -> &Attrs {
        &self.attrs
    }

    /// Port of `Input::getType`.
    pub fn get_type(&self) -> Option<&str> {
        maybe_get_str(&self.attrs, "type")
    }

    /// Port of `Input::getName`: the `name` attr or `"source"`.
    pub fn get_name(&self) -> &str {
        maybe_get_str(&self.attrs, "name").unwrap_or("source")
    }

    /// Port of `Input::getRef`.
    pub fn get_ref(&self) -> Option<&str> {
        maybe_get_str(&self.attrs, "ref")
    }

    /// Port of `Input::getRevCount`.
    pub fn get_rev_count(&self) -> Option<u64> {
        maybe_get_int(&self.attrs, "revCount")
    }

    /// Port of `Input::getLastModified`.
    pub fn get_last_modified(&self) -> Option<u64> {
        maybe_get_int(&self.attrs, "lastModified")
    }

    /// The scheme name, or `None` for a raw input.
    pub fn scheme_name(&self) -> Option<&'static str> {
        self.scheme_name
    }

    /// Port of `Input::toURL` → string form.
    pub fn to_url(&self) -> Result<String, FetchError> {
        let name = self
            .scheme_name
            .ok_or_else(|| FetchError("don't know how to convert input to a URL".into()))?;
        for scheme in registry() {
            if scheme.scheme_name() == name {
                return scheme.to_url(self);
            }
        }
        Err(FetchError("scheme not registered".into()))
    }

    /// Port of `Input::isDirect`.
    pub fn is_direct(&self) -> bool {
        match self.scheme_name.and_then(find_scheme) {
            Some(s) => s.is_direct(self),
            None => true,
        }
    }

    /// Port of `Input::isLocked`.
    pub fn is_locked(&self) -> bool {
        match self.scheme_name.and_then(find_scheme) {
            Some(s) => s.is_locked(self),
            None => false,
        }
    }

    /// Port of `Input::getFingerprint`.
    pub fn get_fingerprint(&self) -> Option<String> {
        self.scheme_name.and_then(find_scheme).and_then(|s| s.get_fingerprint(self))
    }

    /// Port of `Input::computeStorePath` — requires a `narHash` attribute.
    /// Computed via NAR-hash content addressing, no daemon/store needed.
    pub fn compute_store_path(
        &self,
        store: &jinx_store::store_path::StoreDir,
    ) -> Result<StorePath, FetchError> {
        let nar_hash = maybe_get_str(&self.attrs, "narHash")
            .ok_or_else(|| FetchError("cannot compute store path: no narHash".into()))?;
        let hash = jinx_store::hash::Hash::parse_sri(nar_hash)
            .map_err(|e| FetchError(format!("bad narHash: {e}")))?;
        let info = jinx_store::store_path::FixedOutputInfo {
            method: jinx_store::store_path::FileIngestionMethod::NixArchive,
            hash,
            references: Default::default(),
        };
        store
            .make_fixed_output_path(self.get_name(), &info)
            .map_err(|e| FetchError(format!("{e}")))
    }

    /// Port of `Input::fetchToStore` / `getAccessor`: materialize (or, without
    /// a [`StoreWriter`], merely compute) the fetched tree.
    pub fn fetch(
        &self,
        store: &jinx_store::store_path::StoreDir,
        writer: Option<&dyn StoreWriter>,
    ) -> Result<FetchedTree, FetchError> {
        let name = self
            .scheme_name
            .ok_or_else(|| FetchError("cannot fetch a raw input".into()))?;
        let scheme = find_scheme(name).ok_or_else(|| FetchError("scheme not registered".into()))?;
        scheme.fetch(self, store, writer)
    }
}

/// The abstract fetcher scheme interface.
///
/// Port of `fetchers::InputScheme`. We keep only the methods jinx needs for the
/// path scheme and lockfile groundwork; the fetch is expressed directly (as
/// `fetch`) rather than through C++'s `getAccessor`/`SourceAccessor`.
pub trait InputScheme: Send + Sync {
    /// Port of `schemeName`: the value of the `type` attribute.
    fn scheme_name(&self) -> &'static str;

    /// Port of `allowedAttrs`: the permitted attribute names (excluding
    /// `type`).
    fn allowed_attrs(&self) -> &'static [&'static str];

    /// Port of `inputFromURL`. `parsed` is the pre-parsed URL if it had a
    /// scheme. Returns `None` if this scheme does not recognize the URL.
    fn input_from_url(
        &self,
        raw: &str,
        parsed: Option<&ParsedUrl>,
    ) -> Option<Result<Input, FetchError>>;

    /// Port of `inputFromAttrs`.
    fn input_from_attrs(&self, attrs: Attrs) -> Result<Input, FetchError>;

    /// Port of `toURL` → string form.
    fn to_url(&self, input: &Input) -> Result<String, FetchError>;

    /// Materialize or compute the tree. Port of `getAccessor` + `fetchToStore`.
    fn fetch(
        &self,
        input: &Input,
        store: &jinx_store::store_path::StoreDir,
        writer: Option<&dyn StoreWriter>,
    ) -> Result<FetchedTree, FetchError>;

    /// Port of `isDirect` (default `true`).
    fn is_direct(&self, _input: &Input) -> bool {
        true
    }

    /// Port of `isLocked` (default `false`).
    fn is_locked(&self, _input: &Input) -> bool {
        false
    }

    /// Port of `getFingerprint` (default `None`).
    fn get_fingerprint(&self, _input: &Input) -> Option<String> {
        None
    }
}

/// Convert an [`AttrError`] into a [`FetchError`].
impl From<AttrError> for FetchError {
    fn from(e: AttrError) -> Self {
        FetchError(e.0)
    }
}

// ---------------------------------------------------------------------------
// Scheme registry (port of `registerInputScheme` / `inputSchemes`)
// ---------------------------------------------------------------------------

fn registry() -> &'static [Box<dyn InputScheme>] {
    use std::sync::OnceLock;
    static REG: OnceLock<Vec<Box<dyn InputScheme>>> = OnceLock::new();
    REG.get_or_init(|| vec![Box::new(crate::path::PathInputScheme)])
}

fn find_scheme(name: &str) -> Option<&'static dyn InputScheme> {
    registry().iter().find(|s| s.scheme_name() == name).map(|b| b.as_ref())
}
