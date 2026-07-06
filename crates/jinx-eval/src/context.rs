//! String-context elements and their wire encoding, plus experimental-feature
//! tracking. Port of `src/libexpr/value/context.{hh,cc}`.
//!
//! A string value carries a set of context-element *ids*; each id indexes the
//! VM's `ctx_elems` table, whose entries are the C++ wire encodings:
//!   - `Opaque(path)`      -> `<basename>`
//!   - `DrvDeep(drvPath)`  -> `=<basename>`
//!   - `Built{drv,output}` -> `!<output>!<basename>`
//! where `<basename>` is the store-path base name (hash-name, *no* store dir).

/// A decoded context element. Paths here are store-path base names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextElem {
    /// A plain store-path dependency.
    Opaque { path: Vec<u8> },
    /// A deep derivation dependency (all outputs + closure).
    DrvDeep { drv_path: Vec<u8> },
    /// A single output of a derivation.
    Built { drv_path: Vec<u8>, output: Vec<u8> },
}

impl ContextElem {
    /// Parse a wire encoding. Mirrors `NixStringContextElem::parse`. We do not
    /// support nested (dynamic-derivation) `Built` drvPaths; a second `!` in a
    /// `Built` element is left as part of the (opaque) drv path.
    pub fn parse(s: &[u8]) -> ContextElem {
        match s.first() {
            None => ContextElem::Opaque { path: Vec::new() },
            Some(b'!') => {
                let rest = &s[1..];
                match rest.iter().position(|&c| c == b'!') {
                    Some(i) => ContextElem::Built {
                        output: rest[..i].to_vec(),
                        drv_path: rest[i + 1..].to_vec(),
                    },
                    None => ContextElem::Opaque { path: s.to_vec() },
                }
            }
            Some(b'=') => ContextElem::DrvDeep {
                drv_path: s[1..].to_vec(),
            },
            _ => ContextElem::Opaque { path: s.to_vec() },
        }
    }

    /// Wire encoding. Mirrors `NixStringContextElem::to_string`.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ContextElem::Opaque { path } => path.clone(),
            ContextElem::DrvDeep { drv_path } => {
                let mut v = Vec::with_capacity(drv_path.len() + 1);
                v.push(b'=');
                v.extend_from_slice(drv_path);
                v
            }
            ContextElem::Built { drv_path, output } => {
                let mut v = Vec::with_capacity(output.len() + drv_path.len() + 2);
                v.push(b'!');
                v.extend_from_slice(output);
                v.push(b'!');
                v.extend_from_slice(drv_path);
                v
            }
        }
    }
}

/// Enabled experimental features. C++ `ExperimentalFeatureSettings`.
#[derive(Debug, Clone, Default)]
pub struct ExperimentalFeatures {
    pub flakes: bool,
    pub ca_derivations: bool,
    pub impure_derivations: bool,
    pub dynamic_derivations: bool,
    pub git_hashing: bool,
    pub blake3_hashes: bool,
    pub parse_toml_timestamps: bool,
}

impl ExperimentalFeatures {
    /// Enable a feature by its canonical name; unknown names are ignored.
    pub fn enable(&mut self, name: &str) {
        match name {
            "flakes" => self.flakes = true,
            "ca-derivations" => self.ca_derivations = true,
            "impure-derivations" => self.impure_derivations = true,
            "dynamic-derivations" => self.dynamic_derivations = true,
            "git-hashing" => self.git_hashing = true,
            "blake3-hashes" => self.blake3_hashes = true,
            "parse-toml-timestamps" => self.parse_toml_timestamps = true,
            _ => {}
        }
    }
}
