//! Minimal URL parsing for fetcher inputs.
//!
//! This is intentionally small — just enough to parse the `path:` scheme and
//! plain filesystem paths that [`crate::path::PathInputScheme`] accepts. The
//! full flake-reference parser lives in `jinx-flake`.

/// A parsed URL: `scheme:[//authority]path[?query]`.
///
/// Loosely models the fields of C++ `ParsedURL` that the fetchers need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    pub scheme: String,
    pub authority: Option<String>,
    pub path: String,
    /// Query parameters in order (unescaped).
    pub query: Vec<(String, String)>,
}

impl ParsedUrl {
    /// Parse a URL of the form `scheme:...`. Returns `None` if there is no
    /// scheme (i.e. no `:` before any `/`), in which case the caller should
    /// treat the string as a plain path.
    pub fn parse(s: &str) -> Option<ParsedUrl> {
        // A scheme is `[a-zA-Z][a-zA-Z0-9+.-]*` followed by ':'.
        let colon = s.find(':')?;
        let scheme = &s[..colon];
        if scheme.is_empty() || !scheme.as_bytes()[0].is_ascii_alphabetic() {
            return None;
        }
        if !scheme
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-'))
        {
            return None;
        }
        let mut rest = &s[colon + 1..];

        let mut authority = None;
        if let Some(after) = rest.strip_prefix("//") {
            let end = after.find(['/', '?']).unwrap_or(after.len());
            authority = Some(after[..end].to_string());
            rest = &after[end..];
        }

        let (path_part, query_part) = match rest.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (rest, None),
        };

        let query = query_part
            .map(|q| {
                q.split('&')
                    .filter(|s| !s.is_empty())
                    .map(|kv| match kv.split_once('=') {
                        Some((k, v)) => (percent_decode(k), percent_decode(v)),
                        None => (percent_decode(kv), String::new()),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Some(ParsedUrl {
            scheme: scheme.to_string(),
            authority,
            path: percent_decode(path_part),
            query,
        })
    }

    /// Render back to a `scheme:path?query` string.
    pub fn to_string(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.scheme);
        out.push(':');
        if let Some(a) = &self.authority {
            out.push_str("//");
            out.push_str(a);
        }
        out.push_str(&self.path);
        if !self.query.is_empty() {
            out.push('?');
            for (i, (k, v)) in self.query.iter().enumerate() {
                if i > 0 {
                    out.push('&');
                }
                out.push_str(k);
                out.push('=');
                out.push_str(v);
            }
        }
        out
    }
}

/// Decode `%XX` escapes and `+`-as-space is NOT applied (query is raw here,
/// matching Nix, which uses `%` escaping only).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
