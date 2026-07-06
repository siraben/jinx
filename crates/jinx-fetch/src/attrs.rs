//! Fetcher input attributes.
//!
//! Port of `src/libfetchers/attrs.{cc,hh}`. An [`Attrs`] is a sorted map from
//! attribute name to a scalar value ([`Attr`]) that is either a string, an
//! unsigned integer, or a boolean. The sorted-key invariant (a `std::map` in
//! C++) is what makes the JSON canonicalization used for cache keys
//! deterministic; we mirror it with a [`BTreeMap`].

use std::collections::BTreeMap;

/// A single fetcher attribute value.
///
/// Port of `Attr` (minus the lazy variant, which we treat as already forced).
/// JSON numbers map to [`Attr::Int`], strings to [`Attr::Str`], booleans to
/// [`Attr::Bool`]; arrays/objects/null are unsupported.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attr {
    Str(String),
    Int(u64),
    Bool(bool),
}

impl Attr {
    /// Port of the string case of `attrsToQuery`: ints render as decimal,
    /// bools as `"1"`/`"0"`, strings as themselves.
    pub fn to_query_value(&self) -> String {
        match self {
            Attr::Str(s) => s.clone(),
            Attr::Int(n) => n.to_string(),
            Attr::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        }
    }
}

/// A sorted set of fetcher attributes (C++ `Attrs`, a `std::map`).
pub type Attrs = BTreeMap<String, Attr>;

/// Port of `maybeGetStrAttr`.
pub fn maybe_get_str<'a>(attrs: &'a Attrs, name: &str) -> Option<&'a str> {
    match attrs.get(name) {
        Some(Attr::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// Port of `getStrAttr`: like [`maybe_get_str`] but errors when missing.
pub fn get_str<'a>(attrs: &'a Attrs, name: &str) -> Result<&'a str, AttrError> {
    match attrs.get(name) {
        Some(Attr::Str(s)) => Ok(s.as_str()),
        Some(_) => Err(AttrError(format!("input attribute '{name}' is not a string"))),
        None => Err(AttrError(format!("input attribute '{name}' is missing"))),
    }
}

/// Port of `maybeGetIntAttr`.
pub fn maybe_get_int(attrs: &Attrs, name: &str) -> Option<u64> {
    match attrs.get(name) {
        Some(Attr::Int(n)) => Some(*n),
        _ => None,
    }
}

/// Port of `maybeGetBoolAttr`.
pub fn maybe_get_bool(attrs: &Attrs, name: &str) -> Option<bool> {
    match attrs.get(name) {
        Some(Attr::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// Error raised while reading/validating attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrError(pub String);

impl std::fmt::Display for AttrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AttrError {}

/// Port of `attrsToJSON`: serialize to a canonical, sorted-key JSON value
/// (integers as numbers, bools as booleans, strings as strings).
pub fn attrs_to_json(attrs: &Attrs) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in attrs {
        let jv = match v {
            Attr::Str(s) => serde_json::Value::String(s.clone()),
            Attr::Int(n) => serde_json::Value::Number((*n).into()),
            Attr::Bool(b) => serde_json::Value::Bool(*b),
        };
        map.insert(k.clone(), jv);
    }
    serde_json::Value::Object(map)
}

/// Canonical JSON serialization used as the cache key. Because [`Attrs`] is a
/// [`BTreeMap`] and `serde_json`'s object preserves our sorted insertion, the
/// output is key-sorted with no whitespace, matching nlohmann's `dump()`.
pub fn attrs_to_json_string(attrs: &Attrs) -> String {
    serde_json::to_string(&attrs_to_json(attrs)).expect("attrs serialize")
}

/// Port of `jsonToAttrs`: numbers → [`Attr::Int`], strings → [`Attr::Str`],
/// bools → [`Attr::Bool`]; anything else is rejected.
pub fn json_to_attrs(value: &serde_json::Value) -> Result<Attrs, AttrError> {
    let obj = value
        .as_object()
        .ok_or_else(|| AttrError("expected a JSON object for attrs".into()))?;
    let mut attrs = Attrs::new();
    for (k, v) in obj {
        let attr = match v {
            serde_json::Value::String(s) => Attr::Str(s.clone()),
            serde_json::Value::Bool(b) => Attr::Bool(*b),
            serde_json::Value::Number(n) => {
                let u = n
                    .as_u64()
                    .ok_or_else(|| AttrError("unsupported non-integer attribute".into()))?;
                Attr::Int(u)
            }
            _ => return Err(AttrError("unsupported input attribute type in lock file".into())),
        };
        attrs.insert(k.clone(), attr);
    }
    Ok(attrs)
}

/// Port of `attrsToQuery`: build URL query params (unescaped) from attrs.
pub fn attrs_to_query(attrs: &Attrs) -> Vec<(String, String)> {
    attrs
        .iter()
        .map(|(k, v)| (k.clone(), v.to_query_value()))
        .collect()
}
