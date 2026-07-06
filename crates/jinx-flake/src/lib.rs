//! jinx-flake: flake references, lock files, and registries.
//!
//! Groundwork port (no evaluation) of C++ Nix's flake plumbing:
//!
//! - [`flakeref`]: [`flakeref::FlakeRef`] parsing/rendering for the `indirect`,
//!   `path`, `github`/`gitlab`/`sourcehut`, `git`, and `tarball`/`file` schemes
//!   (`libflake/flakeref.cc` + the `libfetchers` scheme attr shapes).
//! - [`lockfile`]: [`lockfile::LockFile`] parsing/serialization of `flake.lock`
//!   versions 5–7 (`libflake/lockfile.cc`), shaped to serve `call-flake.nix`.
//! - [`registry`]: [`registry::Registry`] parsing and `indirect`-ref resolution
//!   (`libfetchers/registry.cc`).
//!
//! The evaluation-side `callFlake` machinery is a later milestone; these types
//! are its inputs. The `path` scheme here mirrors (and is the canonical full
//! version of) any minimal flakeref parser in `jinx-eval`; when the flakes-eval
//! milestone lands, that duplication should be removed in favor of this crate.

pub mod flakeref;
pub mod lockfile;
pub mod registry;

pub use flakeref::{FlakeRef, FlakeRefError};
pub use lockfile::{Edge, LockFile, LockedNode, Node};
pub use registry::{Registry, RegistryType};
