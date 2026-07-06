//! Flake lock files.
//!
//! Port of `src/libflake/lockfile.cc`. Parses `flake.lock` JSON versions 5–7
//! into a node graph and serializes it back. The structure is shaped to serve
//! `call-flake.nix` (which reads `locked`, `inputs`, `flake`, `parent`
//! directly) while retaining `original` for lock maintenance.
//!
//! Unlike C++ Nix — which regenerates node keys on write — we preserve the
//! keys from the parsed file, giving faithful round-trips. We also do *not*
//! inject the `__final` marker into locked attrs (the on-disk file never
//! carries it), so re-serialization matches the input byte-for-byte in shape.

use std::collections::BTreeMap;

use jinx_fetch::attrs::{attrs_to_json, json_to_attrs};

use crate::flakeref::FlakeRef;

/// A flake input identifier (`FlakeId`).
pub type FlakeId = String;

/// A `follows` path from the root node (`InputAttrPath`).
pub type InputAttrPath = Vec<FlakeId>;

/// Error parsing a lock file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockError(pub String);

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LockError {}

fn err<T>(msg: impl Into<String>) -> Result<T, LockError> {
    Err(LockError(msg.into()))
}

/// An edge from a node to one of its inputs.
///
/// Port of `Node::Edge` (`variant<ref<LockedNode>, InputAttrPath>`): either a
/// direct reference to another node (by key) or a `follows` path from root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edge {
    /// A direct input: the key of another node.
    Direct(String),
    /// A `follows` input: a path of input names from the root node.
    Follows(InputAttrPath),
}

/// The locked/original refs of a non-root node.
///
/// Port of `LockedNode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedNode {
    /// The pinned reference (`locked`).
    pub locked: FlakeRef,
    /// The user-written reference (`original`).
    pub original: FlakeRef,
    /// Whether this input is itself a flake (`flake`, default `true`).
    pub is_flake: bool,
    /// For relative `path:` inputs (v7): the node relative to which the path is
    /// interpreted (`parent`).
    pub parent: Option<InputAttrPath>,
}

/// A node in the lock graph.
///
/// The root node has `locked == None`; every other node carries a
/// [`LockedNode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// This node's inputs (`inputs`).
    pub inputs: BTreeMap<FlakeId, Edge>,
    /// The lock info, or `None` for the root node.
    pub locked: Option<LockedNode>,
}

/// A parsed flake lock file.
///
/// Port of `LockFile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockFile {
    /// The lock format version (5, 6, or 7).
    pub version: u64,
    /// The key of the root node (usually `"root"`).
    pub root: String,
    /// All nodes, keyed by their lock-file key.
    pub nodes: BTreeMap<String, Node>,
}

impl LockFile {
    /// An empty lock file (a lone root node with no inputs), as produced for a
    /// flake with no inputs.
    pub fn empty() -> LockFile {
        let mut nodes = BTreeMap::new();
        nodes.insert("root".to_string(), Node { inputs: BTreeMap::new(), locked: None });
        LockFile { version: 7, root: "root".into(), nodes }
    }

    /// Port of the `LockFile` JSON constructor. Accepts versions 5–7.
    pub fn parse(contents: &str) -> Result<LockFile, LockError> {
        let json: serde_json::Value =
            serde_json::from_str(contents).map_err(|e| LockError(format!("invalid JSON: {e}")))?;

        let version = json.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
        if !(5..=7).contains(&version) {
            return err(format!("lock file has unsupported version {version}"));
        }

        let root = json
            .get("root")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LockError("lock file missing 'root'".into()))?
            .to_string();

        let json_nodes = json
            .get("nodes")
            .and_then(|v| v.as_object())
            .ok_or_else(|| LockError("lock file missing 'nodes'".into()))?;

        let mut nodes = BTreeMap::new();
        for (key, jnode) in json_nodes {
            let is_root = *key == root;
            nodes.insert(key.clone(), parse_node(jnode, is_root)?);
        }

        if !nodes.contains_key(&root) {
            return err(format!("lock file references missing root node '{root}'"));
        }

        Ok(LockFile { version, root, nodes })
    }

    /// Port of `LockFile::toJSON` (always emitting version 7), preserving the
    /// parsed node keys.
    pub fn to_json(&self) -> serde_json::Value {
        let mut nodes = serde_json::Map::new();
        for (key, node) in &self.nodes {
            nodes.insert(key.clone(), node_to_json(node));
        }
        let mut obj = serde_json::Map::new();
        obj.insert("version".into(), serde_json::json!(7));
        obj.insert("root".into(), serde_json::Value::String(self.root.clone()));
        obj.insert("nodes".into(), serde_json::Value::Object(nodes));
        serde_json::Value::Object(obj)
    }

    /// Serialize to a pretty JSON string (2-space indent, matching Nix).
    pub fn to_string_pretty(&self) -> String {
        serde_json::to_string_pretty(&self.to_json()).unwrap()
    }

    /// The root node.
    pub fn root_node(&self) -> &Node {
        &self.nodes[&self.root]
    }

    /// Look up a node by key.
    pub fn node(&self, key: &str) -> Option<&Node> {
        self.nodes.get(key)
    }

    /// Port of `resolveInput`: a direct edge yields its node key; a `follows`
    /// edge is resolved from the root.
    pub fn resolve_input(&self, edge: &Edge) -> Option<String> {
        match edge {
            Edge::Direct(k) => Some(k.clone()),
            Edge::Follows(path) => self.get_input_by_path(&self.root, path),
        }
    }

    /// Port of `getInputByPath`: follow `path` (a list of input names) from
    /// `node_key`, resolving `follows` edges along the way.
    pub fn get_input_by_path(&self, node_key: &str, path: &[FlakeId]) -> Option<String> {
        if path.is_empty() {
            return Some(node_key.to_string());
        }
        let node = self.nodes.get(node_key)?;
        let edge = node.inputs.get(&path[0])?;
        let next = self.resolve_input(edge)?;
        self.get_input_by_path(&next, &path[1..])
    }
}

fn parse_node(jnode: &serde_json::Value, is_root: bool) -> Result<Node, LockError> {
    let mut inputs = BTreeMap::new();
    if let Some(jinputs) = jnode.get("inputs").and_then(|v| v.as_object()) {
        for (name, val) in jinputs {
            let edge = match val {
                serde_json::Value::String(s) => Edge::Direct(s.clone()),
                serde_json::Value::Array(arr) => {
                    let mut path = Vec::new();
                    for e in arr {
                        let s = e
                            .as_str()
                            .ok_or_else(|| LockError("follows path element is not a string".into()))?;
                        path.push(s.to_string());
                    }
                    Edge::Follows(path)
                }
                _ => return err(format!("invalid input '{name}': not a string or array")),
            };
            inputs.insert(name.clone(), edge);
        }
    }

    let locked = if is_root && jnode.get("locked").is_none() {
        None
    } else if jnode.get("locked").is_some() {
        let locked_attrs = json_to_attrs(
            jnode.get("locked").unwrap(),
        )
        .map_err(|e| LockError(format!("bad 'locked' attrs: {e}")))?;
        let original_attrs = json_to_attrs(
            jnode
                .get("original")
                .ok_or_else(|| LockError("locked node missing 'original'".into()))?,
        )
        .map_err(|e| LockError(format!("bad 'original' attrs: {e}")))?;
        let is_flake = jnode.get("flake").and_then(|v| v.as_bool()).unwrap_or(true);
        let parent = match jnode.get("parent") {
            Some(serde_json::Value::Array(arr)) => {
                let mut p = Vec::new();
                for e in arr {
                    p.push(
                        e.as_str()
                            .ok_or_else(|| LockError("'parent' element is not a string".into()))?
                            .to_string(),
                    );
                }
                Some(p)
            }
            _ => None,
        };
        Some(LockedNode {
            locked: FlakeRef::from_attrs(locked_attrs),
            original: FlakeRef::from_attrs(original_attrs),
            is_flake,
            parent,
        })
    } else {
        None
    };

    Ok(Node { inputs, locked })
}

fn node_to_json(node: &Node) -> serde_json::Value {
    let mut obj = serde_json::Map::new();

    if !node.inputs.is_empty() {
        let mut inputs = serde_json::Map::new();
        for (name, edge) in &node.inputs {
            let v = match edge {
                Edge::Direct(k) => serde_json::Value::String(k.clone()),
                Edge::Follows(path) => {
                    serde_json::Value::Array(path.iter().cloned().map(serde_json::Value::String).collect())
                }
            };
            inputs.insert(name.clone(), v);
        }
        obj.insert("inputs".into(), serde_json::Value::Object(inputs));
    }

    if let Some(locked) = &node.locked {
        let mut orig = attrs_to_json(&locked.original.to_attrs());
        strip_final(&mut orig);
        let mut lck = attrs_to_json(&locked.locked.to_attrs());
        strip_final(&mut lck);
        obj.insert("original".into(), orig);
        obj.insert("locked".into(), lck);
        if !locked.is_flake {
            obj.insert("flake".into(), serde_json::Value::Bool(false));
        }
        if let Some(parent) = &locked.parent {
            obj.insert(
                "parent".into(),
                serde_json::Value::Array(
                    parent.iter().cloned().map(serde_json::Value::String).collect(),
                ),
            );
        }
    }

    serde_json::Value::Object(obj)
}

fn strip_final(v: &mut serde_json::Value) {
    if let Some(o) = v.as_object_mut() {
        o.remove("__final");
    }
}
