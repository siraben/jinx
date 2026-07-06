//! Flake references.
//!
//! Port of the parse/render subset of `src/libflake/flakeref.cc` (plus the
//! attr shapes of the `indirect`, `path`, `github`/`gitlab`/`sourcehut`, `git`,
//! and `tarball`/`file` input schemes from `src/libfetchers`).
//!
//! A [`FlakeRef`] is an input attribute set plus a `subdir` (the `dir` attr /
//! `?dir=` query). We model the attrs directly (rather than through a
//! `jinx_fetch::Input`, whose registry only knows the `path` scheme) so all
//! flake schemes round-trip losslessly.

use std::sync::OnceLock;

use jinx_fetch::attrs::{maybe_get_str, Attr, Attrs};
use regex::Regex;

/// Error parsing a flake reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlakeRefError(pub String);

impl std::fmt::Display for FlakeRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FlakeRefError {}

fn err<T>(msg: impl Into<String>) -> Result<T, FlakeRefError> {
    Err(FlakeRefError(msg.into()))
}

// --- shared regexes (url-parts.hh / flakeref.hh) ---------------------------

fn flake_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9_-]*$").unwrap())
}

fn rev_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9a-fA-F]{40}$").unwrap())
}

fn ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9@][a-zA-Z0-9_.\-@+/]*$").unwrap())
}

fn is_rev(s: &str) -> bool {
    rev_re().is_match(s)
}

/// A parsed flake reference: input attributes plus a subdirectory.
///
/// Port of `FlakeRef` (`{ fetchers::Input input; std::string subdir; }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlakeRef {
    /// The input attributes (including `type`, excluding `dir`).
    pub attrs: Attrs,
    /// The subdirectory within the fetched tree (the `dir` attr).
    pub subdir: String,
}

impl FlakeRef {
    fn from_type_attrs(attrs: Attrs) -> FlakeRef {
        FlakeRef { attrs, subdir: String::new() }
    }

    /// The `type` attribute (e.g. `"github"`, `"path"`, `"indirect"`).
    pub fn ty(&self) -> Option<&str> {
        maybe_get_str(&self.attrs, "type")
    }

    /// Port of `FlakeRef::fromAttrs`: split `dir` off into `subdir`.
    pub fn from_attrs(mut attrs: Attrs) -> FlakeRef {
        let subdir = match attrs.remove("dir") {
            Some(Attr::Str(s)) => s,
            _ => String::new(),
        };
        FlakeRef { attrs, subdir }
    }

    /// Port of `FlakeRef::toAttrs`: fold `subdir` back into `dir`.
    pub fn to_attrs(&self) -> Attrs {
        let mut attrs = self.attrs.clone();
        if !self.subdir.is_empty() {
            attrs.insert("dir".into(), Attr::Str(self.subdir.clone()));
        }
        attrs
    }

    /// Port of `parseFlakeRef`. Tries indirect, then URL, then path forms.
    pub fn parse(s: &str) -> Result<FlakeRef, FlakeRefError> {
        // Reject a fragment (handled by parseFlakeRefWithFragment in C++).
        let s = s.split('#').next().unwrap_or(s);
        if let Some(r) = parse_indirect(s) {
            return Ok(r);
        }
        if s.contains("://") || has_url_scheme(s) {
            return parse_url_flake_ref(s);
        }
        parse_path_flake_ref(s)
    }

    /// Port of `FlakeRef::to_string` / `toURLString`.
    pub fn to_url(&self) -> Result<String, FlakeRefError> {
        let ty = self.ty().ok_or_else(|| FlakeRefError("flakeref has no type".into()))?;
        let mut url = match ty {
            "indirect" => render_indirect(&self.attrs)?,
            "path" => render_path(&self.attrs)?,
            "github" | "gitlab" | "sourcehut" => render_git_archive(ty, &self.attrs)?,
            "git" | "tarball" | "file" => render_url_attr(ty, &self.attrs)?,
            other => return err(format!("cannot render flakeref of type '{other}'")),
        };
        if !self.subdir.is_empty() {
            let sep = if url.contains('?') { '&' } else { '?' };
            url.push(sep);
            url.push_str("dir=");
            url.push_str(&self.subdir);
        }
        Ok(url)
    }
}

fn has_url_scheme(s: &str) -> bool {
    // scheme like "path:", "github:", "git+https:" ...
    match s.split_once(':') {
        Some((scheme, _)) => {
            !scheme.is_empty()
                && scheme.as_bytes()[0].is_ascii_alphabetic()
                && scheme
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-'))
        }
        None => false,
    }
}

// --- indirect --------------------------------------------------------------

/// Port of `parseFlakeIdRef`: `[flake:]id[/ref-or-rev[/rev]]`.
fn parse_indirect(s: &str) -> Option<FlakeRef> {
    let body = s.strip_prefix("flake:").unwrap_or(s);
    // Reject anything that is clearly a URL or path.
    if body.contains("://") || body.starts_with('.') || body.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = body.split('/').collect();
    if parts.is_empty() || !flake_id_re().is_match(parts[0]) {
        return None;
    }
    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::Str("indirect".into()));
    attrs.insert("id".into(), Attr::Str(parts[0].into()));
    match parts.len() {
        1 => {}
        2 => {
            if is_rev(parts[1]) {
                attrs.insert("rev".into(), Attr::Str(parts[1].into()));
            } else if ref_re().is_match(parts[1]) {
                attrs.insert("ref".into(), Attr::Str(parts[1].into()));
            } else {
                return None;
            }
        }
        3 => {
            if !ref_re().is_match(parts[1]) || !is_rev(parts[2]) {
                return None;
            }
            attrs.insert("ref".into(), Attr::Str(parts[1].into()));
            attrs.insert("rev".into(), Attr::Str(parts[2].into()));
        }
        _ => return None,
    }
    Some(FlakeRef::from_type_attrs(attrs))
}

fn render_indirect(attrs: &Attrs) -> Result<String, FlakeRefError> {
    let id = maybe_get_str(attrs, "id").ok_or_else(|| FlakeRefError("indirect ref has no id".into()))?;
    let mut s = format!("flake:{id}");
    if let Some(r) = maybe_get_str(attrs, "ref") {
        s.push('/');
        s.push_str(r);
    }
    if let Some(rev) = maybe_get_str(attrs, "rev") {
        s.push('/');
        s.push_str(rev);
    }
    Ok(s)
}

// --- path ------------------------------------------------------------------

/// Port of `parsePathFlakeRefWithFragment` (the non-git-boundary case) and the
/// `path` scheme's `inputFromURL`.
fn parse_path_flake_ref(s: &str) -> Result<FlakeRef, FlakeRefError> {
    // `path:...` URI or a plain path.
    let (path_and_query, is_uri) = match s.strip_prefix("path:") {
        Some(rest) => (rest, true),
        None => (s, false),
    };
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    if !is_uri && !(path.starts_with('/') || path.starts_with('.')) {
        return err(format!("'{s}' is not a valid flake reference"));
    }

    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::Str("path".into()));
    attrs.insert("path".into(), Attr::Str(path.to_string()));
    let mut subdir = String::new();
    if let Some(q) = query {
        for (k, v) in parse_query(q) {
            match k.as_str() {
                "dir" => subdir = v,
                "rev" | "narHash" => {
                    attrs.insert(k, Attr::Str(v));
                }
                "revCount" | "lastModified" => {
                    let n: u64 = v
                        .parse()
                        .map_err(|_| FlakeRefError(format!("path param '{k}' is not a number")))?;
                    attrs.insert(k, Attr::Int(n));
                }
                other => return err(format!("unsupported path parameter '{other}'")),
            }
        }
    }
    Ok(FlakeRef { attrs, subdir })
}

fn render_path(attrs: &Attrs) -> Result<String, FlakeRefError> {
    let path = maybe_get_str(attrs, "path").ok_or_else(|| FlakeRefError("path ref has no path".into()))?;
    let mut query: Vec<(String, String)> = attrs
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "path" | "type"))
        .map(|(k, v)| (k.clone(), v.to_query_value()))
        .collect();
    query.sort();
    let mut s = format!("path:{path}");
    render_query(&mut s, &query);
    Ok(s)
}

// --- github / gitlab / sourcehut ------------------------------------------

/// Port of `GitArchiveInputScheme::inputFromURL`.
fn parse_git_archive(ty: &str, rest: &str) -> Result<FlakeRef, FlakeRefError> {
    let (path_part, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let segs: Vec<&str> = path_part.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return err(format!("'{ty}' flake reference is missing owner/repo"));
    }
    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::Str(ty.into()));
    attrs.insert("owner".into(), Attr::Str(segs[0].into()));
    attrs.insert("repo".into(), Attr::Str(segs[1].into()));
    if segs.len() >= 3 {
        let refrev = segs[2..].join("/");
        if segs.len() == 3 && is_rev(segs[2]) {
            attrs.insert("rev".into(), Attr::Str(refrev));
        } else {
            attrs.insert("ref".into(), Attr::Str(refrev));
        }
    }
    let mut subdir = String::new();
    if let Some(q) = query {
        for (k, v) in parse_query(q) {
            match k.as_str() {
                "dir" => subdir = v,
                "rev" | "ref" | "host" | "narHash" => {
                    attrs.insert(k, Attr::Str(v));
                }
                other => return err(format!("unknown '{ty}' parameter '{other}'")),
            }
        }
    }
    Ok(FlakeRef { attrs, subdir })
}

fn render_git_archive(ty: &str, attrs: &Attrs) -> Result<String, FlakeRefError> {
    let owner = maybe_get_str(attrs, "owner").ok_or_else(|| FlakeRefError("missing owner".into()))?;
    let repo = maybe_get_str(attrs, "repo").ok_or_else(|| FlakeRefError("missing repo".into()))?;
    let mut s = format!("{ty}:{owner}/{repo}");
    if let Some(rev) = maybe_get_str(attrs, "rev") {
        s.push('/');
        s.push_str(rev);
    } else if let Some(r) = maybe_get_str(attrs, "ref") {
        s.push('/');
        s.push_str(r);
    }
    let mut query: Vec<(String, String)> = attrs
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "host" | "narHash"))
        .map(|(k, v)| (k.clone(), v.to_query_value()))
        .collect();
    query.sort();
    render_query(&mut s, &query);
    Ok(s)
}

// --- url-carrying schemes: git+*, tarball, file, http(s) -------------------

/// Port of the `git`/`tarball`/`file`/`curl` `inputFromURL`: keep the full
/// transport URL in the `url` attr plus recognized query params.
fn parse_url_flake_ref(s: &str) -> Result<FlakeRef, FlakeRefError> {
    let scheme = s.split_once(':').map(|(a, _)| a).unwrap_or("");
    if scheme == "path" {
        return parse_path_flake_ref(s);
    }
    if scheme == "github" || scheme == "gitlab" || scheme == "sourcehut" {
        let rest = &s[scheme.len() + 1..];
        return parse_git_archive(scheme, rest);
    }

    // Split off the query so we can lift out `dir`, `ref`, `rev`, `narHash`.
    let (base, query) = match s.split_once('?') {
        Some((b, q)) => (b.to_string(), Some(q)),
        None => (s.to_string(), None),
    };

    let (ty, transport_removed) = if let Some(app_rest) = scheme.strip_prefix("git+") {
        ("git", Some(app_rest.to_string()))
    } else if scheme == "git" {
        ("git", None)
    } else if scheme == "http" || scheme == "https" || scheme == "file" || scheme == "tarball" {
        // Heuristic split (as Nix's registries do): tarball extensions -> tarball.
        let is_tarball = base.ends_with(".tar")
            || base.ends_with(".tar.gz")
            || base.ends_with(".tgz")
            || base.ends_with(".tar.xz")
            || base.ends_with(".tar.bz2")
            || base.ends_with(".zip");
        if scheme == "tarball" || is_tarball {
            ("tarball", None)
        } else {
            ("file", None)
        }
    } else {
        return err(format!("unsupported flake reference scheme '{scheme}'"));
    };

    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::Str(ty.into()));

    // Reconstruct the url attr. For git+<transport>, the url is the transport
    // URL without the "git+" prefix; other leftover query params stay on it.
    let mut leftover_query: Vec<(String, String)> = Vec::new();
    let mut subdir = String::new();
    if let Some(q) = query {
        for (k, v) in parse_query(q) {
            match k.as_str() {
                "dir" => subdir = v,
                "ref" | "rev" | "narHash" => {
                    attrs.insert(k, Attr::Str(v));
                }
                _ => leftover_query.push((k, v)),
            }
        }
    }

    let mut url = match &transport_removed {
        Some(app_rest) => {
            // e.g. "git+https://host/repo" -> url attr "https://host/repo"?
            // Nix keeps the "git+" form on the url attr; mirror that.
            let _ = app_rest;
            base.clone()
        }
        None => base.clone(),
    };
    if !leftover_query.is_empty() {
        leftover_query.sort();
        render_query(&mut url, &leftover_query);
    }
    attrs.insert("url".into(), Attr::Str(url));

    Ok(FlakeRef { attrs, subdir })
}

fn render_url_attr(_ty: &str, attrs: &Attrs) -> Result<String, FlakeRefError> {
    let url = maybe_get_str(attrs, "url").ok_or_else(|| FlakeRefError("missing url".into()))?;
    let mut s = url.to_string();
    let mut query: Vec<(String, String)> = attrs
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "ref" | "rev" | "narHash"))
        .map(|(k, v)| (k.clone(), v.to_query_value()))
        .collect();
    query.sort();
    render_query(&mut s, &query);
    Ok(s)
}

// --- query helpers ---------------------------------------------------------

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.to_string(), String::new()),
        })
        .collect()
}

fn render_query(s: &mut String, query: &[(String, String)]) {
    for (i, (k, v)) in query.iter().enumerate() {
        s.push(if i == 0 { '?' } else { '&' });
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
}
