//! The fetcher cache.
//!
//! Port of `src/libfetchers/cache.{cc,hh}`. A single sqlite table maps
//! `(domain, canonical-JSON-key)` to `(canonical-JSON-value, timestamp)`.
//!
//! Unlike C++ Nix — which uses `~/.cache/nix/fetcher-cache-v4.sqlite` — we use a
//! **private** database at `~/.cache/jinx/fetcher-cache.sqlite` so jinx never
//! shares or corrupts Nix's cache. The table schema and canonicalization are
//! otherwise identical.

use std::path::PathBuf;

use rusqlite::Connection;

use crate::attrs::{attrs_to_json_string, json_to_attrs, Attrs};

/// Default TTL for cache entries, in seconds. Port of the `tarball-ttl`
/// default (`60 * 60`).
pub const DEFAULT_TTL_SECONDS: u64 = 60 * 60;

/// A cache lookup domain (C++ `Cache::Domain`).
pub type Domain = String;

/// A cache key: a domain plus the attributes identifying the entry.
///
/// Port of `Cache::Key` (`std::pair<Domain, Attrs>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub domain: Domain,
    pub attrs: Attrs,
}

impl Key {
    pub fn new(domain: impl Into<Domain>, attrs: Attrs) -> Self {
        Key { domain: domain.into(), attrs }
    }
}

/// A cache lookup result: the stored value plus whether it is past its TTL.
///
/// Port of `Cache::Result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheResult {
    pub expired: bool,
    pub value: Attrs,
}

/// The persistent fetcher cache backed by sqlite.
pub struct Cache {
    conn: Connection,
    ttl_seconds: u64,
}

/// Port of `getCacheDir`, but for jinx: `$JINX_CACHE_HOME`, else
/// `$XDG_CACHE_HOME/jinx`, else `$HOME/.cache/jinx`.
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JINX_CACHE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("jinx");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache").join("jinx")
}

impl Cache {
    /// Open (creating if needed) the default cache database.
    pub fn open_default() -> rusqlite::Result<Self> {
        let dir = cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        Self::open(dir.join("fetcher-cache.sqlite"))
    }

    /// Open a cache at a specific path (used by tests).
    pub fn open(path: impl AsRef<std::path::Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory cache (used by tests).
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "create table if not exists Cache (
                 domain    text not null,
                 key       text not null,
                 value     text not null,
                 timestamp integer not null,
                 primary key (domain, key)
             );",
        )?;
        Ok(Cache { conn, ttl_seconds: DEFAULT_TTL_SECONDS })
    }

    /// Override the TTL (default [`DEFAULT_TTL_SECONDS`]).
    pub fn set_ttl_seconds(&mut self, ttl: u64) {
        self.ttl_seconds = ttl;
    }

    /// Port of `Cache::upsert`.
    pub fn upsert(&self, key: &Key, value: &Attrs) -> rusqlite::Result<()> {
        self.conn.execute(
            "insert or replace into Cache(domain, key, value, timestamp) values (?, ?, ?, ?)",
            rusqlite::params![
                key.domain,
                attrs_to_json_string(&key.attrs),
                attrs_to_json_string(value),
                now() as i64,
            ],
        )?;
        Ok(())
    }

    /// Port of `Cache::lookupExpired`: returns the value with an expiry flag.
    pub fn lookup_expired(&self, key: &Key) -> rusqlite::Result<Option<CacheResult>> {
        let key_json = attrs_to_json_string(&key.attrs);
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "select value, timestamp from Cache where domain = ? and key = ?",
                rusqlite::params![key.domain, key_json],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((value_json, timestamp)) = row else {
            return Ok(None);
        };
        let value: serde_json::Value = match serde_json::from_str(&value_json) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let attrs = match json_to_attrs(&value) {
            Ok(a) => a,
            Err(_) => return Ok(None),
        };
        // Port: expired iff ttl == 0 or timestamp + ttl < now.
        let expired =
            self.ttl_seconds == 0 || (timestamp as u64).saturating_add(self.ttl_seconds) < now();
        Ok(Some(CacheResult { expired, value: attrs }))
    }

    /// Port of `Cache::lookup`: value ignoring expiry.
    pub fn lookup(&self, key: &Key) -> rusqlite::Result<Option<Attrs>> {
        Ok(self.lookup_expired(key)?.map(|r| r.value))
    }

    /// Port of `Cache::lookupWithTTL`: value only if not expired.
    pub fn lookup_with_ttl(&self, key: &Key) -> rusqlite::Result<Option<Attrs>> {
        Ok(self.lookup_expired(key)?.and_then(|r| (!r.expired).then_some(r.value)))
    }
}

/// Current Unix time in seconds.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
