//! A `nix search`-style evaluation cache, stored in **C++ Nix's exact SQLite
//! schema** (`libexpr/eval-cache.cc`): a flat `Attributes(parent, name, type,
//! value, context)` table forming an attribute tree, with the same `AttrType`
//! integer encoding. This makes the on-disk format compatible with Nix's
//! eval-cache (Nix's `AttrCursor` can read the rows), and gives jinx the same
//! hot/cold behaviour: the first search evaluates the package set and populates
//! the cache; later searches read it back and skip evaluation entirely.
//!
//! NOTE ON COMPAT: the schema, `AttrType` values, and value serialization match
//! Nix byte-for-byte, so the table is interoperable. Sharing Nix's *exact* DB
//! file additionally requires reproducing `LockedFlake::getFingerprint` (the
//! `<hash>.sqlite` filename) and Nix's precise `AttrCursor` tree shape; jinx
//! uses its own fingerprint + a per-package tree for now (see README/`search`).

use rusqlite::{params, Connection};
use std::path::Path;

// C++ `nix::eval_cache::AttrType` (eval-cache.hh) — do not renumber.
const T_FULL_ATTRS: i64 = 1;
const T_STRING: i64 = 2;
const T_MISSING: i64 = 3;
const T_FAILED: i64 = 5;

const SCHEMA: &str = "create table if not exists Attributes (\n    \
    parent      integer not null,\n    \
    name        text,\n    \
    type        integer not null,\n    \
    value       text,\n    \
    context     text,\n    \
    primary key (parent, name)\n);";

/// One package's cached metadata (what a search matches against).
pub struct Pkg {
    pub path: String,
    pub name: String,
    /// `None` = the package has no `meta.description` (stored as `Missing`).
    pub desc: Option<String>,
    /// `true` = the attribute threw during evaluation (stored as `Failed`).
    pub failed: bool,
}

pub struct Cache {
    conn: Connection,
}

impl Cache {
    /// Open (creating if needed) a cache DB in Nix's schema.
    pub fn open(path: &Path) -> rusqlite::Result<Cache> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        // Match Nix's pragmas (WAL + relaxed sync — it's a rebuildable cache).
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Cache { conn })
    }

    /// The root `FullAttrs` node (parent 0) — present iff the cache is populated.
    pub fn root(&self) -> rusqlite::Result<Option<i64>> {
        self.conn
            .query_row(
                "select rowid from Attributes where parent = 0 and type = ?1 limit 1",
                params![T_FULL_ATTRS],
                |r| r.get::<_, i64>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
    }

    /// Populate the cache from a full package walk (the cold path). Writes a
    /// root `FullAttrs` node, then per package a `FullAttrs` node with `name`
    /// and `description` (`String`/`Missing`) children, or a `Failed` node.
    pub fn write_all(&mut self, pkgs: &[Pkg]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "insert or replace into Attributes(parent, name, type) values (0, '', ?1)",
            params![T_FULL_ATTRS],
        )?;
        let root = tx.last_insert_rowid();
        {
            let mut ins = tx.prepare(
                "insert or replace into Attributes(parent, name, type, value) values (?1, ?2, ?3, ?4)",
            )?;
            for p in pkgs {
                if p.failed {
                    ins.execute(params![root, p.path, T_FAILED, Option::<String>::None])?;
                    continue;
                }
                ins.execute(params![root, p.path, T_FULL_ATTRS, Option::<String>::None])?;
                let node = tx.last_insert_rowid();
                ins.execute(params![node, "name", T_STRING, p.name])?;
                match &p.desc {
                    Some(d) => ins.execute(params![node, "description", T_STRING, d])?,
                    None => ins.execute(params![node, "description", T_MISSING, Option::<String>::None])?,
                };
            }
        }
        tx.commit()
    }

    /// Read every cached package back (the hot path — no evaluation).
    pub fn read_all(&self, root: i64) -> rusqlite::Result<Vec<Pkg>> {
        // path -> (rowid, type) for every child of the root.
        let mut stmt = self
            .conn
            .prepare("select rowid, name, type from Attributes where parent = ?1")?;
        let rows: Vec<(i64, String, i64)> = stmt
            .query_map(params![root], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut child = self
            .conn
            .prepare("select type, value from Attributes where parent = ?1 and name = ?2")?;
        let mut out = Vec::with_capacity(rows.len());
        for (rowid, path, ty) in rows {
            if ty == T_FAILED {
                out.push(Pkg { path, name: String::new(), desc: None, failed: true });
                continue;
            }
            if ty != T_FULL_ATTRS {
                continue;
            }
            let name: String = child
                .query_row(params![rowid, "name"], |r| r.get::<_, Option<String>>(1))
                .ok()
                .flatten()
                .unwrap_or_default();
            let (dty, dval): (i64, Option<String>) = child
                .query_row(params![rowid, "description"], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap_or((T_MISSING, None));
            out.push(Pkg {
                path,
                name,
                desc: if dty == T_STRING { dval } else { None },
                failed: false,
            });
        }
        Ok(out)
    }
}
