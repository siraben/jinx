//! jinx-fetch: fetcher inputs, the path scheme, and the fetcher cache.
//!
//! Groundwork port of parts of C++ Nix's `src/libfetchers`:
//!
//! - [`attrs`]: the [`attrs::Attr`] / [`attrs::Attrs`] scalar attribute map and
//!   its canonical JSON form (`attrs.cc`).
//! - [`url`]: a minimal URL parser for the path scheme (`url.cc` subset).
//! - [`fetchers`]: the [`fetchers::InputScheme`] trait, [`fetchers::Input`], and
//!   the scheme registry (`fetchers.cc`).
//! - [`path`]: the [`path::PathInputScheme`] (`path.cc`).
//! - [`cache`]: the sqlite fetcher [`cache::Cache`] (`cache.cc`), using a
//!   private jinx database (never Nix's).
//!
//! The fetch pipeline computes a store path via NAR-hash content addressing
//! (using `jinx-store`) **without writing to any store**, mirroring a read-only
//! store. A [`StoreWriter`] hook lets the daemon client be plugged in later to
//! perform real store additions.

use std::path::{Path, PathBuf};

use jinx_store::store_path::StorePath;

pub mod attrs;
pub mod cache;
pub mod fetchers;
pub mod path;
pub mod url;

pub use attrs::{Attr, Attrs};
pub use cache::{Cache, CacheResult, Key};
pub use fetchers::{Input, InputScheme};

/// An error from the fetch layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchError(pub String);

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FetchError {}

/// The result of fetching an [`Input`].
///
/// Mirrors the `(StorePath, Input)` pair C++ `fetchToStore`/`getAccessor`
/// returns, but keeps the on-disk `real_path` and lets `store_path` be `None`
/// when no store is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedTree {
    /// The content-addressed store path, computed even without a store.
    pub store_path: Option<StorePath>,
    /// The real filesystem path the tree was fetched from.
    pub real_path: PathBuf,
    /// Locked info attributes (e.g. `narHash`, `lastModified`).
    pub info: Attrs,
}

/// A hook for actually adding a tree to a store.
///
/// Implemented later by the daemon client (`jinx_store::daemon`) so that
/// [`fetchers::Input::fetch`] can perform real store additions. When absent,
/// the fetch only *computes* the store path.
pub trait StoreWriter {
    /// Add the NAR serialization of `path` to the store under `name`
    /// (recursive/NAR content addressing, sha256) and return its store path.
    fn add_to_store_nar(&self, name: &str, path: &Path) -> Result<StorePath, FetchError>;
}
