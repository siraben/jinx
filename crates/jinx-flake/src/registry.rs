//! Flake registries.
//!
//! Port of `src/libfetchers/registry.cc`. A registry maps input references
//! (`from`) to concrete ones (`to`), used to resolve `indirect` refs like
//! `nixpkgs`. We model `from`/`to` as raw fetcher [`Attrs`] and implement the
//! exact/prefix matching and override logic of `lookupInRegistries`.

use std::path::{Path, PathBuf};

use jinx_fetch::attrs::{json_to_attrs, maybe_get_str, Attr, Attrs};

/// Error reading or resolving a registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError(pub String);

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RegistryError {}

/// Where a registry came from (`RegistryType`), determining precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryType {
    Flag,
    User,
    System,
    Global,
    Custom,
}

/// A single registry mapping (`Registry::Entry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The reference to match (`from`).
    pub from: Attrs,
    /// The reference to substitute (`to`).
    pub to: Attrs,
    /// Extra attributes (notably `dir`) applied on match (`extraAttrs`).
    pub extra_attrs: Attrs,
    /// Whether `from` must match `input` exactly (`exact`).
    pub exact: bool,
}

/// A flake registry (`Registry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry {
    pub type_: RegistryType,
    pub entries: Vec<Entry>,
}

impl Registry {
    /// An empty registry of the given type.
    pub fn empty(type_: RegistryType) -> Registry {
        Registry { type_, entries: Vec::new() }
    }

    /// Port of `Registry::read`: parse version-2 registry JSON.
    pub fn parse(contents: &str, type_: RegistryType) -> Result<Registry, RegistryError> {
        let json: serde_json::Value = serde_json::from_str(contents)
            .map_err(|e| RegistryError(format!("invalid registry JSON: {e}")))?;
        let version = json.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
        if version != 2 {
            return Err(RegistryError(format!(
                "flake registry has unsupported version {version}"
            )));
        }
        let flakes = json
            .get("flakes")
            .and_then(|v| v.as_array())
            .ok_or_else(|| RegistryError("registry missing 'flakes' array".into()))?;
        let mut entries = Vec::new();
        for item in flakes {
            let from = json_to_attrs(
                item.get("from").ok_or_else(|| RegistryError("entry missing 'from'".into()))?,
            )
            .map_err(|e| RegistryError(format!("bad 'from': {e}")))?;
            let mut to = json_to_attrs(
                item.get("to").ok_or_else(|| RegistryError("entry missing 'to'".into()))?,
            )
            .map_err(|e| RegistryError(format!("bad 'to': {e}")))?;
            // Peel `dir` off `to` into extraAttrs.
            let mut extra_attrs = Attrs::new();
            if let Some(dir) = to.remove("dir") {
                extra_attrs.insert("dir".into(), dir);
            }
            let exact = item.get("exact").and_then(|v| v.as_bool()).unwrap_or(false);
            entries.push(Entry { from, to, extra_attrs, exact });
        }
        Ok(Registry { type_, entries })
    }

    /// Read a registry from a file, or return an empty one if absent.
    pub fn from_file(path: impl AsRef<Path>, type_: RegistryType) -> Result<Registry, RegistryError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Registry::parse(&s, type_),
            Err(_) => Ok(Registry::empty(type_)),
        }
    }

    /// Port of `Registry::add`.
    pub fn add(&mut self, from: Attrs, to: Attrs, extra_attrs: Attrs) {
        self.entries.push(Entry { from, to, extra_attrs, exact: false });
    }

    /// Port of `Registry::remove`: delete all entries whose `from` equals
    /// `input`.
    pub fn remove(&mut self, input: &Attrs) {
        self.entries.retain(|e| &e.from != input);
    }
}

/// Port of `getUserRegistryPath`: `~/.config/nix/registry.json`.
pub fn user_registry_path() -> PathBuf {
    config_dir().join("registry.json")
}

/// Port of `getSystemRegistryPath`: `/etc/nix/registry.json`.
pub fn system_registry_path() -> PathBuf {
    let dir = std::env::var("NIX_CONF_DIR").unwrap_or_else(|_| "/etc/nix".into());
    PathBuf::from(dir).join("registry.json")
}

fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("NIX_CONFIG_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(d) = std::env::var("XDG_CONFIG_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join("nix");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("nix")
}

/// Load the standard registries in precedence order (User, System). The Flag
/// and Global registries are supplied by the caller when available.
pub fn default_registries() -> Vec<Registry> {
    vec![
        Registry::from_file(user_registry_path(), RegistryType::User)
            .unwrap_or_else(|_| Registry::empty(RegistryType::User)),
        Registry::from_file(system_registry_path(), RegistryType::System)
            .unwrap_or_else(|_| Registry::empty(RegistryType::System)),
    ]
}

// ---------------------------------------------------------------------------
// Resolution (port of lookupInRegistries)
// ---------------------------------------------------------------------------

/// Whether `input`'s type is `indirect` (i.e. still needs resolving).
fn is_indirect(attrs: &Attrs) -> bool {
    maybe_get_str(attrs, "type") == Some("indirect")
}

/// Port of `Input::contains`: equal, or equal after erasing `ref`/`rev` from
/// `input`.
fn contains(from: &Attrs, input: &Attrs) -> bool {
    if from == input {
        return true;
    }
    let mut trimmed = input.clone();
    trimmed.remove("ref");
    trimmed.remove("rev");
    from == &trimmed
}

/// Port of `applyOverrides`: carry `ref`/`rev` from the query onto `to`.
fn apply_overrides(mut to: Attrs, ref_: Option<&str>, rev: Option<&str>) -> Attrs {
    if let Some(r) = ref_ {
        to.insert("ref".into(), Attr::Str(r.to_string()));
    }
    if let Some(r) = rev {
        to.insert("rev".into(), Attr::Str(r.to_string()));
    }
    to
}

/// Port of `lookupInRegistries`: resolve `input` through `registries` in order,
/// restarting after each substitution (bounded to 100 iterations). Returns the
/// resolved attrs plus any extra attrs (e.g. `dir`).
///
/// Errors if the input remains `indirect` after exhausting the registries.
pub fn lookup_in_registries(
    input: &Attrs,
    registries: &[Registry],
) -> Result<(Attrs, Attrs), RegistryError> {
    let mut input = input.clone();
    let mut extra_attrs = Attrs::new();
    let mut n = 0;

    'restart: loop {
        n += 1;
        if n > 100 {
            return Err(RegistryError("cycle detected in flake registry".into()));
        }
        for registry in registries {
            for entry in &registry.entries {
                if entry.exact {
                    if entry.from == input {
                        input = entry.to.clone();
                        extra_attrs = entry.extra_attrs.clone();
                        continue 'restart;
                    }
                } else if contains(&entry.from, &input) {
                    let ref_ = if maybe_get_str(&entry.from, "ref").is_none() {
                        maybe_get_str(&input, "ref").map(|s| s.to_string())
                    } else {
                        None
                    };
                    let rev = if maybe_get_str(&entry.from, "rev").is_none() {
                        maybe_get_str(&input, "rev").map(|s| s.to_string())
                    } else {
                        None
                    };
                    input = apply_overrides(entry.to.clone(), ref_.as_deref(), rev.as_deref());
                    extra_attrs = entry.extra_attrs.clone();
                    continue 'restart;
                }
            }
        }
        break;
    }

    if is_indirect(&input) {
        return Err(RegistryError(format!(
            "cannot find flake '{}' in the flake registries",
            maybe_get_str(&input, "id").unwrap_or("?")
        )));
    }

    Ok((input, extra_attrs))
}
