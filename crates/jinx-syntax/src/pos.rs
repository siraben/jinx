//! Position handling, mirroring C++ `PosIdx` / `PosTable` / `Pos`.
//!
//! A `PosIdx` is `1 + origin.offset + byte_offset` into a virtual
//! concatenation of all parsed sources; 0 is `noPos`.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct PosIdx(pub u32);

impl PosIdx {
    pub fn is_set(self) -> bool {
        self.0 != 0
    }
}

pub const NO_POS: PosIdx = PosIdx(0);

/// Where a source buffer came from; determines how positions print.
#[derive(Clone, Debug)]
pub enum Origin {
    Stdin { source: Vec<u8> },
    String { source: Vec<u8> },
    Path { path: String, source: Vec<u8> },
}

impl Origin {
    pub fn source(&self) -> &[u8] {
        match self {
            Origin::Stdin { source } | Origin::String { source } | Origin::Path { source, .. } => {
                source
            }
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Origin::Stdin { .. } => "«stdin»".into(),
            Origin::String { .. } => "«string»".into(),
            Origin::Path { path, .. } => path.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OriginId(pub usize);

struct OriginEntry {
    origin: Origin,
    offset: u32,
    size: u32,
    /// Memoized line-start table, computed on first position lookup into this
    /// origin. Recomputing it per lookup (a full source scan) was a visible
    /// eval-time cost for large source files.
    line_starts: std::cell::OnceCell<Vec<u32>>,
}

/// A resolved position (1-based line/column) with its origin name.
#[derive(Clone, Debug)]
pub struct Pos {
    pub origin_name: String,
    pub line: u32,
    pub column: u32,
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.origin_name, self.line)?;
        if self.column > 0 {
            write!(f, ":{}", self.column)?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct PosTable {
    origins: Vec<OriginEntry>,
}

impl PosTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_origin(&mut self, origin: Origin, size: usize) -> OriginId {
        let offset = self
            .origins
            .last()
            .map(|o| o.offset + o.size + 2)
            .unwrap_or(0);
        self.origins.push(OriginEntry {
            origin,
            offset,
            size: size as u32,
            line_starts: std::cell::OnceCell::new(),
        });
        OriginId(self.origins.len() - 1)
    }

    pub fn add(&self, origin: OriginId, offset: u32) -> PosIdx {
        let o = &self.origins[origin.0];
        if offset > o.size {
            return NO_POS;
        }
        PosIdx(1 + o.offset + offset)
    }

    fn resolve_origin(&self, p: PosIdx) -> Option<(&OriginEntry, u32)> {
        if !p.is_set() {
            return None;
        }
        let idx = p.0 - 1;
        let entry = self
            .origins
            .iter()
            .rev()
            .find(|o| o.offset <= idx)
            .expect("origin for PosIdx");
        Some((entry, idx - entry.offset))
    }

    pub fn origin_of(&self, p: PosIdx) -> Option<&Origin> {
        self.resolve_origin(p).map(|(e, _)| &e.origin)
    }

    /// Byte offset of `p` within its origin's source.
    pub fn offset_of(&self, p: PosIdx) -> Option<u32> {
        self.resolve_origin(p).map(|(_, off)| off)
    }

    /// Convert a `PosIdx` to line/column, like `PosTable::operator[]`.
    pub fn lookup(&self, p: PosIdx) -> Option<Pos> {
        let (entry, offset) = self.resolve_origin(p)?;
        let starts = entry
            .line_starts
            .get_or_init(|| line_starts(entry.origin.source()));
        // last start <= offset
        let idx = match starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        Some(Pos {
            origin_name: entry.origin.display_name(),
            line: (idx + 1) as u32,
            column: offset - starts[idx] + 1,
        })
    }
}

/// Byte offsets of the start of each line, treating `\n`, `\r` and `\r\n`
/// all as line terminators (like `Pos::LinesIterator`). The first line
/// starts at 0 and is always present.
pub fn line_starts(source: &[u8]) -> Vec<u32> {
    let mut starts = vec![0u32];
    let mut i = 0usize;
    while i < source.len() {
        match source[i] {
            b'\r' => {
                i += 1;
                if i < source.len() && source[i] == b'\n' {
                    i += 1;
                }
                starts.push(i as u32);
            }
            b'\n' => {
                i += 1;
                starts.push(i as u32);
            }
            _ => i += 1,
        }
    }
    starts
}

/// Split source into lines the same way as `Pos::LinesIterator` (used for
/// error excerpts). A trailing line terminator produces a final empty line.
pub fn split_lines(source: &[u8]) -> Vec<&[u8]> {
    let starts = line_starts(source);
    let mut lines = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let end = if i + 1 < starts.len() {
            // strip the terminator: next start minus terminator length
            let next = starts[i + 1] as usize;
            let mut e = next;
            if e > start as usize && source[e - 1] == b'\n' {
                e -= 1;
            }
            if e > start as usize && source[e - 1] == b'\r' {
                e -= 1;
            }
            e
        } else {
            source.len()
        };
        lines.push(&source[start as usize..end]);
    }
    lines
}
