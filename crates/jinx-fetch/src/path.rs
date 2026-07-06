//! The `path` input scheme.
//!
//! Port of `src/libfetchers/path.cc`. A `path` input copies a filesystem tree
//! into the store verbatim (no filtering — `defaultPathFilter` accepts
//! everything), content-addressed by the NAR hash under the name `"source"`.

use std::path::PathBuf;

use jinx_store::hash::{Hash, HashAlgorithm, HashFormat};
use jinx_store::nar;
use jinx_store::store_path::{FileIngestionMethod, FixedOutputInfo, StoreDir};

use crate::attrs::{maybe_get_str, Attr, Attrs};
use crate::fetchers::{Input, InputScheme};
use crate::url::ParsedUrl;
use crate::{FetchError, FetchedTree, StoreWriter};

/// The `path` scheme. Port of `PathInputScheme`.
pub struct PathInputScheme;

/// Attribute names permitted on a `path` input (port of `allowedAttrs`).
const ALLOWED: &[&str] = &["path", "rev", "revCount", "lastModified", "narHash"];

impl PathInputScheme {
    fn attrs_from_url(url: &ParsedUrl) -> Result<Attrs, FetchError> {
        if url.authority.as_deref().is_some_and(|a| !a.is_empty()) {
            return Err(FetchError("path URL must not have an authority".into()));
        }
        let mut attrs = Attrs::new();
        attrs.insert("type".into(), Attr::Str("path".into()));
        attrs.insert("path".into(), Attr::Str(url.path.clone()));
        for (name, value) in &url.query {
            match name.as_str() {
                "rev" | "narHash" => {
                    attrs.insert(name.clone(), Attr::Str(value.clone()));
                }
                "revCount" | "lastModified" => {
                    let n: u64 = value
                        .parse()
                        .map_err(|_| FetchError(format!("path URL '{name}' is not a number")))?;
                    attrs.insert(name.clone(), Attr::Int(n));
                }
                other => {
                    return Err(FetchError(format!("unsupported path URL parameter '{other}'")))
                }
            }
        }
        Ok(attrs)
    }
}

impl InputScheme for PathInputScheme {
    fn scheme_name(&self) -> &'static str {
        "path"
    }

    fn allowed_attrs(&self) -> &'static [&'static str] {
        ALLOWED
    }

    fn input_from_url(
        &self,
        raw: &str,
        parsed: Option<&ParsedUrl>,
    ) -> Option<Result<Input, FetchError>> {
        match parsed {
            Some(url) if url.scheme == "path" => {
                Some(Self::attrs_from_url(url).and_then(Input::from_attrs))
            }
            // A URL with any other scheme is not ours.
            Some(_) => None,
            // No scheme: treat as a plain filesystem path (absolute or
            // relative). This is what the flake layer does for bare paths.
            None => {
                if raw.is_empty() {
                    return None;
                }
                let mut attrs = Attrs::new();
                attrs.insert("type".into(), Attr::Str("path".into()));
                attrs.insert("path".into(), Attr::Str(raw.to_string()));
                Some(Input::from_attrs(attrs))
            }
        }
    }

    fn input_from_attrs(&self, attrs: Attrs) -> Result<Input, FetchError> {
        // `path` is required.
        crate::attrs::get_str(&attrs, "path")?;
        Ok(Input::with_scheme(attrs, "path"))
    }

    fn to_url(&self, input: &Input) -> Result<String, FetchError> {
        let path = crate::attrs::get_str(input.to_attrs(), "path")?;
        let mut query: Vec<(String, String)> = input
            .to_attrs()
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "path" | "type" | "__final"))
            .map(|(k, v)| (k.clone(), v.to_query_value()))
            .collect();
        query.sort();
        let url = ParsedUrl {
            scheme: "path".into(),
            authority: None,
            path: path.to_string(),
            query,
        };
        Ok(url.to_string())
    }

    fn is_locked(&self, input: &Input) -> bool {
        // Port of `PathInputScheme::isLocked`: locked iff a narHash is present.
        maybe_get_str(input.to_attrs(), "narHash").is_some()
    }

    fn fetch(
        &self,
        input: &Input,
        store: &StoreDir,
        writer: Option<&dyn StoreWriter>,
    ) -> Result<FetchedTree, FetchError> {
        let path = crate::attrs::get_str(input.to_attrs(), "path")?;
        let real_path = PathBuf::from(path);
        if !real_path.is_absolute() {
            return Err(FetchError(format!(
                "path input '{path}' is not an absolute path"
            )));
        }

        // Hash the NAR serialization of the tree (no filtering).
        let (nar_hash, _nar_size) = nar::hash_path(&real_path, HashAlgorithm::Sha256)
            .map_err(|e| FetchError(format!("hashing '{path}': {e}")))?;

        let name = input.get_name().to_string();

        // Content-addressed store path (NAR / recursive, sha256).
        let computed = store
            .make_fixed_output_path(
                &name,
                &FixedOutputInfo {
                    method: FileIngestionMethod::NixArchive,
                    hash: nar_hash,
                    references: Default::default(),
                },
            )
            .map_err(|e| FetchError(format!("{e}")))?;

        // Optionally actually write to the store via the hook.
        let store_path = match writer {
            Some(w) => Some(w.add_to_store_nar(&name, &real_path)?),
            None => Some(computed),
        };

        // Assemble the info attrs, echoing the user's pins plus the NAR hash.
        let mut info: Attrs = input.to_attrs().clone();
        info.insert(
            "narHash".into(),
            Attr::Str(nar_hash_sri(&nar_hash)),
        );

        Ok(FetchedTree { store_path, real_path, info })
    }
}

fn nar_hash_sri(h: &Hash) -> String {
    h.to_string(HashFormat::Sri, true)
}
