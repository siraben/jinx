//! jinx-store: store-layer primitives for the jinx Nix evaluator.
//!
//! Faithful ports of the corresponding C++ Nix (2.33-dev) algorithms:
//!
//! - [`nix32`]: Nix's base-32 codec (`base-nix-32.cc`)
//! - [`hash`]: hash types, textual formats, `hashString`, `compressHash`
//!   (`hash.cc`)
//! - [`store_path`]: [`store_path::StorePath`], store dir handling, the
//!   `makeStorePath` family, content addresses (`path.cc`,
//!   `store-dir-config.cc`, `content-address.cc`)
//! - [`derivation`]: derivation type, ATerm parse/unparse,
//!   `hashDerivationModulo` (`derivations.cc`)
//! - [`nar`]: NAR serializer and hasher (`archive.cc`)
//! - [`wire`]: daemon protocol framing primitives (`serialise.cc`)
//! - [`daemon`]: the worker-protocol client (`worker-protocol.cc`,
//!   `remote-store.cc`, `uds-remote-store.cc`)

pub mod daemon;
pub mod derivation;
pub mod escape_scan;
pub mod hash;
pub mod nar;
pub mod nar_stats;
pub mod nix32;
pub mod store_path;
pub mod wire;

pub use derivation::{
    hash_derivation_modulo, Derivation, DerivationOutput, DerivedPathMap, DerivedPathMapNode,
    DrvHashModulo, DrvHashes,
};
pub use hash::{compress_hash, hash_string, Hash, HashAlgorithm, HashFormat};
pub use store_path::{
    ContentAddress, ContentAddressMethod, FileIngestionMethod, StoreDir, StorePath, StorePathSet,
};
