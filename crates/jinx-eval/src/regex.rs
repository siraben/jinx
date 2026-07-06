//! A self-contained POSIX ERE (Extended Regular Expression) engine that mirrors
//! C++ `std::regex` with the `std::regex::extended` flag, as used by Nix's
//! `builtins.match` and `builtins.split`.
//!
//! # Design
//!
//! The engine parses a pattern (given as bytes, since Nix strings are byte
//! strings) into an AST and matches with a recursive backtracking matcher using
//! continuation-passing style.
//!
//! The crux of POSIX semantics is *leftmost-longest*: the overall match must be
//! the leftmost, and among leftmost matches the longest. Empirically (validated
//! against the reference `nix-instantiate` oracle) the capture/submatch
//! behaviour of `std::regex::extended` is: **maximise the overall match length,
//! and among the paths achieving that maximum length take the one found first in
//! greedy depth-first order** (quantifiers prefer more repetitions first;
//! alternation prefers earlier branches first; `?` prefers "present" first).
//!
//! This is *not* a per-subexpression longest-match comparator. For example
//! `(a*)*` on `"aaa"` yields group 1 = `""` (the greedy outer star performs a
//! final empty iteration that overwrites the capture), and `(a|ab)(c|bcd)` on
//! `"abcd"` yields `["a", "bcd"]` because that is the path reaching the maximum
//! overall length. Both are reproduced by "max overall length, first greedy
//! path".
//!
//! `find_iter` reproduces `std::cregex_iterator`, including the zero-width-match
//! advancement rule.

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A compiled POSIX ERE.
pub struct Regex {
    root: Node,
    ngroups: usize,
}

/// Opaque compilation error. The caller produces the Nix-facing error string.
#[derive(Debug)]
pub struct RegexError;

/// A single non-overlapping match produced by [`Regex::find_iter`].
pub struct Match {
    pub start: usize,
    pub end: usize,
    pub groups: Vec<Option<(usize, usize)>>,
}

impl Regex {
    /// Compile a POSIX ERE. Bytes in, since Nix strings are byte strings.
    pub fn compile(pattern: &[u8]) -> Result<Regex, RegexError> {
        // The oracle rejects the empty pattern outright.
        if pattern.is_empty() {
            return Err(RegexError);
        }
        let mut p = Parser {
            pat: pattern,
            pos: 0,
            ngroups: 0,
        };
        let root = p.parse_alternation(0)?;
        if p.pos != pattern.len() {
            // Leftover input (e.g. an unmatched `)` handled as literal is fine,
            // but a genuine parse stop before EOF is an error). In practice the
            // only way to stop early is an unmatched `)` at depth 0, which is
            // parsed as a literal, so reaching here means a real error.
            return Err(RegexError);
        }
        Ok(Regex {
            root,
            ngroups: p.ngroups,
        })
    }

    /// Number of capture groups (excluding group 0).
    pub fn num_groups(&self) -> usize {
        self.ngroups
    }

    /// Full anchored match (equivalent to `std::regex_match`): the entire input
    /// must match. Returns the capture groups on success.
    pub fn match_full(&self, input: &[u8]) -> Option<Vec<Option<(usize, usize)>>> {
        run_search(input.len(), || self.match_full_inner(input))
    }

    fn match_full_inner(&self, input: &[u8]) -> Option<Vec<Option<(usize, usize)>>> {
        let mut s = Search {
            input,
            full: true,
            best_end: None,
            best_caps: vec![None; self.ngroups],
            caps: vec![None; self.ngroups + 1],
        };
        s.go(&self.root, 0, &Cont::Done);
        if s.best_end == Some(input.len()) {
            Some(s.best_caps)
        } else {
            None
        }
    }

    /// Longest match anchored at position `p` (not required to reach the end of
    /// input). Returns `(end, groups)`.
    fn longest_at(&self, input: &[u8], p: usize) -> Option<(usize, Vec<Option<(usize, usize)>>)> {
        let mut s = Search {
            input,
            full: false,
            best_end: None,
            best_caps: vec![None; self.ngroups],
            caps: vec![None; self.ngroups + 1],
        };
        s.go(&self.root, p, &Cont::Done);
        s.best_end.map(|e| (e, s.best_caps))
    }

    /// Leftmost match at or after position `from`.
    fn leftmost(
        &self,
        input: &[u8],
        from: usize,
    ) -> Option<(usize, usize, Vec<Option<(usize, usize)>>)> {
        let mut p = from;
        while p <= input.len() {
            if let Some((e, caps)) = self.longest_at(input, p) {
                return Some((p, e, caps));
            }
            p += 1;
        }
        None
    }

    /// Non-overlapping left-to-right search iteration (equivalent to
    /// `std::cregex_iterator`). Replicates the zero-width-match advancement rule.
    pub fn find_iter(&self, input: &[u8]) -> Vec<Match> {
        run_search(input.len(), || self.find_iter_inner(input))
    }

    fn find_iter_inner(&self, input: &[u8]) -> Vec<Match> {
        let n = input.len();
        let mut out = Vec::new();
        let mut have_prev = false;
        let mut prev_empty = false;
        let mut prev_end = 0usize;

        loop {
            let found = if !have_prev {
                self.leftmost(input, 0)
            } else if prev_empty {
                if prev_end == n {
                    None
                } else {
                    // Mirror libstdc++: after an empty match, first try a
                    // *non-empty* match anchored exactly at `prev_end`; if that
                    // fails, advance one byte and do a normal leftmost search.
                    match self.longest_at(input, prev_end) {
                        Some((e, caps)) if e > prev_end => Some((prev_end, e, caps)),
                        _ => self.leftmost(input, prev_end + 1),
                    }
                }
            } else {
                self.leftmost(input, prev_end)
            };

            match found {
                None => break,
                Some((s, e, groups)) => {
                    prev_empty = s == e;
                    prev_end = e;
                    have_prev = true;
                    out.push(Match {
                        start: s,
                        end: e,
                        groups,
                    });
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Stack management: deep backtracking recursion can exceed a small thread
// stack for long inputs. Run the search on a large stack when the input is big.
// ---------------------------------------------------------------------------

fn run_search<T: Send>(input_len: usize, f: impl FnOnce() -> T + Send) -> T {
    if input_len <= 256 {
        f()
    } else {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(256 * 1024 * 1024)
                .spawn_scoped(scope, f)
                .expect("spawn regex search thread")
                .join()
                .expect("regex search thread panicked")
        })
    }
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

enum Node {
    /// A single literal byte.
    Byte(u8),
    /// `.` matches any single byte (including newline).
    Any,
    /// A bracket expression, as a 256-entry membership table.
    Class(Box<[bool; 256]>),
    /// `^` (start of input).
    Start,
    /// `$` (end of input).
    End,
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    /// Greedy repetition with a minimum and optional maximum count.
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        /// Capture-group indices nested within `node`. At the start of each
        /// iteration these are reset to `None`, so a subgroup that did not
        /// participate in the *last* iteration ends up unset (matching the
        /// oracle, e.g. `((a)|(b))+` on "ab" gives group 2 = null).
        groups: Vec<usize>,
    },
    /// A capturing group with its 1-based index.
    Group { idx: usize, node: Box<Node> },
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Set of characters that may be backslash-escaped outside a bracket
/// expression. Determined empirically against the oracle: exactly the ERE
/// metacharacters (note `]` is *not* escapable). Escaping anything else is an
/// error; a trailing backslash is a literal backslash.
fn is_escapable(b: u8) -> bool {
    matches!(
        b,
        b'.' | b'[' | b'(' | b')' | b'{' | b'}' | b'|' | b'^' | b'$' | b'*' | b'+' | b'?' | b'\\'
    )
}

struct Parser<'a> {
    pat: &'a [u8],
    pos: usize,
    ngroups: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.pat.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.pat.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    /// `depth` is the current parenthesis nesting depth (so we know whether a
    /// `)` closes a group or is a literal).
    fn parse_alternation(&mut self, depth: usize) -> Result<Node, RegexError> {
        let mut branches = vec![self.parse_concat(depth)?];
        while self.peek() == Some(b'|') {
            self.bump();
            branches.push(self.parse_concat(depth)?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn parse_concat(&mut self, depth: usize) -> Result<Node, RegexError> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None => break,
                Some(b'|') => break,
                Some(b')') if depth > 0 => break,
                _ => items.push(self.parse_quantified(depth)?),
            }
        }
        // An empty branch (e.g. `a|`, `|a`, `()`, `(|a)`) is an error.
        if items.is_empty() {
            return Err(RegexError);
        }
        if items.len() == 1 {
            Ok(items.pop().unwrap())
        } else {
            Ok(Node::Concat(items))
        }
    }

    fn parse_quantified(&mut self, depth: usize) -> Result<Node, RegexError> {
        let atom = self.parse_atom(depth)?;
        let quant = match self.peek() {
            Some(b'*') => {
                self.bump();
                Some((0u32, None))
            }
            Some(b'+') => {
                self.bump();
                Some((1u32, None))
            }
            Some(b'?') => {
                self.bump();
                Some((0u32, Some(1u32)))
            }
            Some(b'{') => Some(self.parse_interval()?),
            _ => None,
        };
        match quant {
            None => Ok(atom),
            Some((min, max)) => {
                // A second, stacked quantifier is an error (`a**`, `a+?`, ...).
                match self.peek() {
                    Some(b'*') | Some(b'+') | Some(b'?') | Some(b'{') => return Err(RegexError),
                    _ => {}
                }
                let mut groups = Vec::new();
                collect_groups(&atom, &mut groups);
                Ok(Node::Repeat {
                    node: Box::new(atom),
                    min,
                    max,
                    groups,
                })
            }
        }
    }

    /// Parse a `{n}`, `{n,}` or `{n,m}` interval. The opening `{` is at the
    /// current position.
    fn parse_interval(&mut self) -> Result<(u32, Option<u32>), RegexError> {
        debug_assert_eq!(self.peek(), Some(b'{'));
        self.bump(); // consume '{'
        let min = self.parse_number().ok_or(RegexError)?;
        match self.peek() {
            Some(b'}') => {
                self.bump();
                Ok((min, Some(min)))
            }
            Some(b',') => {
                self.bump();
                if self.peek() == Some(b'}') {
                    self.bump();
                    Ok((min, None))
                } else {
                    let max = self.parse_number().ok_or(RegexError)?;
                    if self.peek() != Some(b'}') {
                        return Err(RegexError);
                    }
                    self.bump();
                    if max < min {
                        return Err(RegexError);
                    }
                    Ok((min, Some(max)))
                }
            }
            _ => Err(RegexError),
        }
    }

    fn parse_number(&mut self) -> Option<u32> {
        let start = self.pos;
        let mut val: u32 = 0;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                val = val.saturating_mul(10).saturating_add((b - b'0') as u32);
                self.bump();
            } else {
                break;
            }
        }
        if self.pos == start {
            None
        } else {
            Some(val)
        }
    }

    fn parse_atom(&mut self, depth: usize) -> Result<Node, RegexError> {
        match self.peek() {
            None => Err(RegexError),
            Some(b'(') => {
                self.bump();
                self.ngroups += 1;
                let idx = self.ngroups;
                let inner = self.parse_alternation(depth + 1)?;
                if self.peek() != Some(b')') {
                    return Err(RegexError);
                }
                self.bump();
                Ok(Node::Group {
                    idx,
                    node: Box::new(inner),
                })
            }
            Some(b'.') => {
                self.bump();
                Ok(Node::Any)
            }
            Some(b'^') => {
                self.bump();
                Ok(Node::Start)
            }
            Some(b'$') => {
                self.bump();
                Ok(Node::End)
            }
            Some(b'[') => self.parse_class(),
            // A quantifier with nothing to repeat, or a `{`/interval at atom
            // position, is an error.
            Some(b'*') | Some(b'+') | Some(b'?') | Some(b'{') => Err(RegexError),
            Some(b')') => {
                // Only reachable at depth 0: an unmatched `)` is a literal.
                self.bump();
                Ok(Node::Byte(b')'))
            }
            Some(b'\\') => {
                self.bump();
                match self.peek() {
                    None => Ok(Node::Byte(b'\\')), // trailing backslash: literal
                    Some(e) if is_escapable(e) => {
                        self.bump();
                        Ok(Node::Byte(e))
                    }
                    Some(_) => Err(RegexError),
                }
            }
            Some(c) => {
                self.bump();
                Ok(Node::Byte(c))
            }
        }
    }

    fn parse_class(&mut self) -> Result<Node, RegexError> {
        debug_assert_eq!(self.peek(), Some(b'['));
        self.bump(); // consume '['
        let mut set = Box::new([false; 256]);
        let negated = if self.peek() == Some(b'^') {
            self.bump();
            true
        } else {
            false
        };

        let mut first = true;
        loop {
            let c = self.peek().ok_or(RegexError)?; // unterminated bracket
            if c == b']' && !first {
                self.bump();
                break;
            }
            first = false;

            // POSIX class / collating symbol / equivalence class.
            if c == b'[' {
                match self.peek_at(1) {
                    Some(b':') => {
                        self.parse_posix_class(&mut set)?;
                        continue;
                    }
                    Some(b'.') | Some(b'=') => {
                        // Collating / equivalence: handle minimally as a single
                        // literal character (the fixtures do not use them).
                        let ch = self.parse_collating()?;
                        // Fall through to a possible range with this as start.
                        self.parse_range_tail(&mut set, ch)?;
                        continue;
                    }
                    _ => {}
                }
            }

            // A plain byte. Backslash is a *literal* backslash inside brackets.
            let start = c;
            self.bump();
            self.parse_range_tail(&mut set, start)?;
        }

        if negated {
            for b in set.iter_mut() {
                *b = !*b;
            }
        }
        Ok(Node::Class(set))
    }

    /// After reading a range start byte `start`, optionally consume `-end` to
    /// form a range; otherwise just add `start`.
    fn parse_range_tail(&mut self, set: &mut [bool; 256], start: u8) -> Result<(), RegexError> {
        // A range applies only if the next char is `-`, the char after it
        // exists and is not `]`, and it is not the start of a POSIX class /
        // collating element.
        if self.peek() == Some(b'-') {
            match self.peek_at(1) {
                Some(b']') | None => {
                    // `-` is a trailing literal; add start, leave `-` for the
                    // next iteration.
                    set[start as usize] = true;
                }
                Some(b'[')
                    if matches!(self.peek_at(2), Some(b':') | Some(b'.') | Some(b'=')) =>
                {
                    // `-` cannot form a range with a class endpoint: literal.
                    set[start as usize] = true;
                }
                Some(end) => {
                    self.bump(); // '-'
                    self.bump(); // end
                    if start <= end {
                        for b in start..=end {
                            set[b as usize] = true;
                        }
                    }
                    // Reversed ranges (start > end) contribute nothing, matching
                    // the oracle (no error, empty set).
                }
            }
        } else {
            set[start as usize] = true;
        }
        Ok(())
    }

    /// Parse `[.x.]` or `[=x=]`, returning the single collating character.
    fn parse_collating(&mut self) -> Result<u8, RegexError> {
        debug_assert_eq!(self.peek(), Some(b'['));
        self.bump();
        let kind = self.bump().ok_or(RegexError)?; // '.' or '='
        let ch = self.bump().ok_or(RegexError)?;
        // Expect closing `kind` then `]`.
        if self.bump() != Some(kind) || self.bump() != Some(b']') {
            return Err(RegexError);
        }
        Ok(ch)
    }

    fn parse_posix_class(&mut self, set: &mut [bool; 256]) -> Result<(), RegexError> {
        debug_assert_eq!(self.peek(), Some(b'['));
        self.bump(); // '['
        self.bump(); // ':'
        let name_start = self.pos;
        while let Some(b) = self.peek() {
            if b == b':' {
                break;
            }
            self.bump();
        }
        let name = &self.pat[name_start..self.pos];
        // Expect `:]`.
        if self.bump() != Some(b':') || self.bump() != Some(b']') {
            return Err(RegexError);
        }
        let pred: fn(u8) -> bool = match name {
            b"alpha" => is_alpha,
            b"digit" => is_digit,
            b"alnum" => is_alnum,
            b"space" => is_space,
            b"upper" => is_upper,
            b"lower" => is_lower,
            b"punct" => is_punct,
            b"blank" => is_blank,
            b"cntrl" => is_cntrl,
            b"graph" => is_graph,
            b"print" => is_print,
            b"xdigit" => is_xdigit,
            _ => return Err(RegexError),
        };
        for b in 0u16..256 {
            if pred(b as u8) {
                set[b as usize] = true;
            }
        }
        Ok(())
    }
}

/// Collect the capture-group indices appearing anywhere within `node`.
fn collect_groups(node: &Node, out: &mut Vec<usize>) {
    match node {
        Node::Byte(_) | Node::Any | Node::Class(_) | Node::Start | Node::End => {}
        Node::Concat(xs) | Node::Alt(xs) => {
            for x in xs {
                collect_groups(x, out);
            }
        }
        Node::Repeat { node, .. } => collect_groups(node, out),
        Node::Group { idx, node } => {
            out.push(*idx);
            collect_groups(node, out);
        }
    }
}

// ---------------------------------------------------------------------------
// POSIX character-class predicates (C/ASCII locale; bytes >= 128 are in no
// class).
// ---------------------------------------------------------------------------

fn is_alpha(b: u8) -> bool {
    b.is_ascii_alphabetic()
}
fn is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}
fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
fn is_upper(b: u8) -> bool {
    b.is_ascii_uppercase()
}
fn is_lower(b: u8) -> bool {
    b.is_ascii_lowercase()
}
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}
fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}
fn is_graph(b: u8) -> bool {
    (0x21..=0x7e).contains(&b)
}
fn is_print(b: u8) -> bool {
    (0x20..=0x7e).contains(&b)
}
fn is_cntrl(b: u8) -> bool {
    b < 0x20 || b == 0x7f
}
fn is_punct(b: u8) -> bool {
    is_graph(b) && !is_alnum(b)
}
fn is_xdigit(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

// ---------------------------------------------------------------------------
// Matcher (continuation-passing backtracking)
// ---------------------------------------------------------------------------

/// The continuation to run once a node has matched.
enum Cont<'c> {
    /// Done: offer the current position as a candidate match end.
    Done,
    /// Match the given sequence of nodes in order, then run the next cont.
    Seq(&'c [Node], &'c Cont<'c>),
    /// Close capture group `idx` (started at the second field), then continue.
    CloseGroup(usize, usize, &'c Cont<'c>),
    /// Continuation after one iteration of a repetition.
    RepeatIter {
        node: &'c Node,
        min: u32,
        max: Option<u32>,
        groups: &'c [usize],
        count: u32,
        iter_start: usize,
        k: &'c Cont<'c>,
    },
}

struct Search<'a> {
    input: &'a [u8],
    /// If true, only whole-input matches (`pos == len`) are accepted.
    full: bool,
    best_end: Option<usize>,
    best_caps: Vec<Option<(usize, usize)>>,
    caps: Vec<Option<(usize, usize)>>,
}

impl<'a> Search<'a> {
    fn len(&self) -> usize {
        self.input.len()
    }

    /// Offer `pos` as a candidate overall match end. Keep the maximum end, and
    /// for a given maximum keep the *first* candidate found in DFS order.
    fn offer(&mut self, pos: usize) {
        if self.full && pos != self.len() {
            return;
        }
        let better = match self.best_end {
            None => true,
            Some(e) => pos > e,
        };
        if better {
            self.best_end = Some(pos);
            self.best_caps.copy_from_slice(&self.caps[1..]);
        }
    }

    fn go<'c>(&mut self, node: &'c Node, pos: usize, k: &Cont<'c>) {
        match node {
            Node::Byte(b) => {
                if pos < self.len() && self.input[pos] == *b {
                    self.cont(k, pos + 1);
                }
            }
            Node::Any => {
                if pos < self.len() {
                    self.cont(k, pos + 1);
                }
            }
            Node::Class(set) => {
                if pos < self.len() && set[self.input[pos] as usize] {
                    self.cont(k, pos + 1);
                }
            }
            Node::Start => {
                if pos == 0 {
                    self.cont(k, pos);
                }
            }
            Node::End => {
                if pos == self.len() {
                    self.cont(k, pos);
                }
            }
            Node::Concat(xs) => {
                let k2 = Cont::Seq(xs, k);
                self.cont(&k2, pos);
            }
            Node::Alt(bs) => {
                for b in bs {
                    self.go(b, pos, k);
                }
            }
            Node::Group { idx, node: inner } => {
                let k2 = Cont::CloseGroup(*idx, pos, k);
                self.go(inner, pos, &k2);
            }
            Node::Repeat {
                node,
                min,
                max,
                groups,
            } => {
                self.repeat(node, *min, *max, groups, 0, pos, k);
            }
        }
    }

    fn cont<'c>(&mut self, k: &Cont<'c>, pos: usize) {
        match k {
            Cont::Done => self.offer(pos),
            Cont::Seq(xs, k2) => {
                if let Some((head, tail)) = xs.split_first() {
                    let k3 = Cont::Seq(tail, k2);
                    self.go(head, pos, &k3);
                } else {
                    self.cont(k2, pos);
                }
            }
            Cont::CloseGroup(idx, start, k2) => {
                let saved = self.caps[*idx];
                self.caps[*idx] = Some((*start, pos));
                self.cont(k2, pos);
                self.caps[*idx] = saved;
            }
            Cont::RepeatIter {
                node,
                min,
                max,
                groups,
                count,
                iter_start,
                k,
            } => {
                if pos == *iter_start {
                    // The iteration matched empty. Do not loop forever; only
                    // keep iterating (with no progress) to satisfy `min`.
                    if count + 1 < *min {
                        self.repeat(node, *min, *max, groups, count + 1, pos, k);
                    } else {
                        self.cont(k, pos);
                    }
                } else {
                    self.repeat(node, *min, *max, groups, count + 1, pos, k);
                }
            }
        }
    }

    fn repeat<'c>(
        &mut self,
        node: &'c Node,
        min: u32,
        max: Option<u32>,
        groups: &'c [usize],
        count: u32,
        pos: usize,
        k: &Cont<'c>,
    ) {
        let can_more = max.map_or(true, |m| count < m);
        // Greedy: try one more iteration before stopping, so the first path
        // found in DFS order is the one with the most repetitions.
        if can_more {
            // Reset capture slots nested in the repeated node at the start of
            // this iteration, so subgroups reflect only the last iteration.
            let saved: Vec<Option<(usize, usize)>> = groups.iter().map(|&g| self.caps[g]).collect();
            for &g in groups {
                self.caps[g] = None;
            }
            let k2 = Cont::RepeatIter {
                node,
                min,
                max,
                groups,
                count,
                iter_start: pos,
                k,
            };
            self.go(node, pos, &k2);
            for (i, &g) in groups.iter().enumerate() {
                self.caps[g] = saved[i];
            }
        }
        if count >= min {
            self.cont(k, pos);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Convenience: does the whole input match?
    fn matches(pat: &str, s: &str) -> bool {
        Regex::compile(pat.as_bytes())
            .unwrap()
            .match_full(s.as_bytes())
            .is_some()
    }

    /// Whole-match capture groups as `Option<String>`.
    fn match_groups(pat: &str, s: &str) -> Option<Vec<Option<String>>> {
        let re = Regex::compile(pat.as_bytes()).unwrap();
        re.match_full(s.as_bytes()).map(|caps| {
            caps.into_iter()
                .map(|g| g.map(|(a, b)| String::from_utf8_lossy(&s.as_bytes()[a..b]).into_owned()))
                .collect()
        })
    }

    /// Mirror `builtins.split`: interleave non-matching text and per-match group
    /// lists.
    #[derive(Debug, PartialEq, Eq)]
    enum Part {
        Str(String),
        Groups(Vec<Option<String>>),
    }

    fn split(pat: &str, s: &str) -> Vec<Part> {
        let bytes = s.as_bytes();
        let re = Regex::compile(pat.as_bytes()).unwrap();
        let ms = re.find_iter(bytes);
        let mut out = Vec::new();
        let mut last = 0;
        for m in ms {
            out.push(Part::Str(
                String::from_utf8_lossy(&bytes[last..m.start]).into_owned(),
            ));
            out.push(Part::Groups(
                m.groups
                    .iter()
                    .map(|g| {
                        g.map(|(a, b)| String::from_utf8_lossy(&bytes[a..b]).into_owned())
                    })
                    .collect(),
            ));
            last = m.end;
        }
        out.push(Part::Str(
            String::from_utf8_lossy(&bytes[last..]).into_owned(),
        ));
        out
    }

    // Builders for expected values.
    fn s(x: &str) -> Part {
        Part::Str(x.to_string())
    }
    fn g(xs: &[Option<&str>]) -> Part {
        Part::Groups(xs.iter().map(|o| o.map(|x| x.to_string())).collect())
    }
    fn some(x: &str) -> Option<String> {
        Some(x.to_string())
    }

    // --- eval-okay-regex-match.nix -----------------------------------------

    #[test]
    fn fixture_match() {
        assert!(matches("foobar", "foobar"));
        assert!(matches("fo*", "f"));
        assert!(!matches("fo+", "f"));
        assert!(matches("fo*", "fo"));
        assert!(matches("fo*", "foo"));
        assert!(matches("fo+", "foo"));
        assert!(matches("fo{1,2}", "foo"));
        assert!(!matches("fo{1,2}", "fooo"));
        assert!(!matches("fo*", "foobar"));
        assert!(matches("[[:space:]]+([^[:space:]]+)[[:space:]]+", "  foo   "));
        assert!(!matches("[[:space:]]+([[:upper:]]+)[[:space:]]+", "  foo   "));

        assert_eq!(match_groups("(.*)\\.nix", "foobar.nix"), Some(vec![some("foobar")]));
        assert_eq!(
            match_groups("[[:space:]]+([[:upper:]]+)[[:space:]]+", "  FOO   "),
            Some(vec![some("FOO")])
        );

        let fn_pat = "((.*)/)?([^/]*)\\.(nix|cc)";
        assert_eq!(
            match_groups(fn_pat, "/path/to/foobar.nix"),
            Some(vec![
                some("/path/to/"),
                some("/path/to"),
                some("foobar"),
                some("nix"),
            ])
        );
        assert_eq!(
            match_groups(fn_pat, "foobar.cc"),
            Some(vec![None, None, some("foobar"), some("cc")])
        );
    }

    // --- eval-okay-regex-match2.nix (representative cases) ------------------

    #[test]
    fn fixture_match2() {
        // null (no match)
        assert_eq!(match_groups("(.*)e?abi.*", "linux"), None);
        assert_eq!(match_groups(".*-none.*", "x86_64-unknown-linux-gnu"), None);
        // empty group list (no captures but matches)
        assert_eq!(
            match_groups("[[:alnum:]+_?=-][[:alnum:]+._?=-]*", "glibc-2.40-66"),
            Some(vec![])
        );
        assert_eq!(
            match_groups("mirror://([a-z]+)/(.*)", "mirror://gnu/m4/m4-1.4.19.tar.bz2"),
            Some(vec![some("gnu"), some("m4/m4-1.4.19.tar.bz2")])
        );
        assert_eq!(
            match_groups("^([0-9][0-9\\.]*)(.*)$", "10"),
            Some(vec![some("10"), some("")])
        );
        assert_eq!(
            match_groups(
                "^([[:digit:]]+)\\.([[:digit:]]+)\\.([[:digit:]]+)\\.([[:digit:]]+)$",
                "8.9.5.30"
            ),
            Some(vec![some("8"), some("9"), some("5"), some("30")])
        );
        // alternation, unmatched branches become null
        assert_eq!(
            match_groups("^(@([^/]+)/)?([^/]+)$", "draupnir"),
            Some(vec![None, None, some("draupnir")])
        );
        // greedy nested repetition capture (last iteration)
        assert_eq!(
            match_groups("(.+)+(.+)", "17.0.14+7"),
            Some(vec![some("17.0.14+"), some("7")])
        );
        assert_eq!(
            match_groups("(pypy|python)([[:digit:]]*)", "pypy310"),
            Some(vec![some("pypy"), some("310")])
        );
        assert_eq!(
            match_groups("<(.*)>", "<name>"),
            Some(vec![some("name")])
        );
        assert_eq!(
            match_groups("(.*)-([^-]*)-([^-]*)", "2.2.4-20231021.200112-6"),
            Some(vec![some("2.2.4"), some("20231021.200112"), some("6")])
        );
        assert_eq!(
            match_groups("([^/]*)/([^/]*)(/SNAPSHOT)?(/.*)?", "jna/5.6.0"),
            Some(vec![some("jna"), some("5.6.0"), None, None])
        );
        // `.` matches newline; anchored multiline capture
        assert_eq!(
            match_groups(
                "^.*CONFIG_BOARD_DIRECTORY=\"([a-zA-Z0-9_]+)\".*$",
                "a\nCONFIG_BOARD_DIRECTORY=\"simulator\"\nb"
            ),
            Some(vec![some("simulator")])
        );
        assert_eq!(match_groups("(^|.*/)\\.git", ".flake8"), None);
        assert_eq!(match_groups("0+", "0"), Some(vec![]));
    }

    // --- eval-okay-regex-split.nix -----------------------------------------

    #[test]
    fn fixture_split() {
        assert_eq!(split("foobar", "foobar"), vec![s(""), g(&[]), s("")]);
        assert_eq!(split("fo*", "f"), vec![s(""), g(&[]), s("")]);
        assert_eq!(split("fo+", "f"), vec![s("f")]);
        assert_eq!(split("fo*", "fo"), vec![s(""), g(&[]), s("")]);
        assert_eq!(split("fo*", "foo"), vec![s(""), g(&[]), s("")]);
        assert_eq!(split("fo+", "foo"), vec![s(""), g(&[]), s("")]);
        assert_eq!(split("fo{1,2}", "foo"), vec![s(""), g(&[]), s("")]);
        assert_eq!(split("fo{1,2}", "fooo"), vec![s(""), g(&[]), s("o")]);
        assert_eq!(split("fo*", "foobar"), vec![s(""), g(&[]), s("bar")]);

        assert_eq!(split("(fo*)", "f"), vec![s(""), g(&[some("f").as_deref()]), s("")]);
        assert_eq!(split("(fo+)", "f"), vec![s("f")]);
        assert_eq!(split("(fo*)", "fo"), vec![s(""), g(&[Some("fo")]), s("")]);
        assert_eq!(
            split("(f)(o*)", "f"),
            vec![s(""), g(&[Some("f"), Some("")]), s("")]
        );
        assert_eq!(
            split("(f)(o*)", "foo"),
            vec![s(""), g(&[Some("f"), Some("oo")]), s("")]
        );
        assert_eq!(split("(fo+)", "foo"), vec![s(""), g(&[Some("foo")]), s("")]);
        assert_eq!(split("(fo{1,2})", "foo"), vec![s(""), g(&[Some("foo")]), s("")]);
        assert_eq!(
            split("(fo{1,2})", "fooo"),
            vec![s(""), g(&[Some("foo")]), s("o")]
        );
        assert_eq!(split("(fo*)", "foobar"), vec![s(""), g(&[Some("foo")]), s("bar")]);

        // greedy
        assert_eq!(
            split("(o+)", "oooofoooo"),
            vec![s(""), g(&[Some("oooo")]), s("f"), g(&[Some("oooo")]), s("")]
        );
        // multiple matches
        assert_eq!(
            split("(b)", "foobarbaz"),
            vec![s("foo"), g(&[Some("b")]), s("ar"), g(&[Some("b")]), s("az")]
        );

        // alternation with class and group, spanning newlines
        let input = "Nix Rocks!\nThat's why I use it.\n";
        assert_eq!(
            split("[[:space:]]+|([',.!?])", input),
            vec![
                s("Nix"),
                g(&[None]),
                s("Rocks"),
                g(&[Some("!")]),
                s(""),
                g(&[None]),
                s("That"),
                g(&[Some("'")]),
                s("s"),
                g(&[None]),
                s("why"),
                g(&[None]),
                s("I"),
                g(&[None]),
                s("use"),
                g(&[None]),
                s("it"),
                g(&[Some(".")]),
                s(""),
                g(&[None]),
                s(""),
            ]
        );

        // documentation examples
        assert_eq!(split("(a)b", "abc"), vec![s(""), g(&[Some("a")]), s("c")]);
        assert_eq!(
            split("([ac])", "abc"),
            vec![s(""), g(&[Some("a")]), s("b"), g(&[Some("c")]), s("")]
        );
        assert_eq!(
            split("(a)|(c)", "abc"),
            vec![
                s(""),
                g(&[Some("a"), None]),
                s("b"),
                g(&[None, Some("c")]),
                s("")
            ]
        );
        assert_eq!(
            split("([[:upper:]]+)", "  FOO   "),
            vec![s("  "), g(&[Some("FOO")]), s("   ")]
        );
    }

    // --- targeted semantic cases -------------------------------------------

    #[test]
    fn posix_longest_and_captures() {
        // alternation is longest-overall, not first-match
        assert_eq!(match_groups("(a)|(ab)", "ab"), Some(vec![None, some("ab")]));
        assert_eq!(
            match_groups("(a|ab)(c|bcd)", "abcd"),
            Some(vec![some("a"), some("bcd")])
        );
        assert_eq!(
            match_groups("(a|ab)(c|bcd)(d*)", "abcd"),
            Some(vec![some("a"), some("bcd"), some("")])
        );
        // greedy captures
        assert_eq!(match_groups("(a*)(a*)", "aaa"), Some(vec![some("aaa"), some("")]));
        assert_eq!(match_groups("(a+)(a+)", "aaaa"), Some(vec![some("aaa"), some("a")]));
        assert_eq!(match_groups("(a?)(a?)a", "aa"), Some(vec![some("a"), some("")]));
        // trailing empty iteration overwrites the capture
        assert_eq!(match_groups("(a*)*", "aaa"), Some(vec![some("")]));
        // last iteration capture
        assert_eq!(match_groups("(a|b)*", "abab"), Some(vec![some("b")]));
        assert_eq!(match_groups("x*", ""), Some(vec![]));
        // subgroups nested in a repeated group reset each iteration
        assert_eq!(
            match_groups("((a)|(b))+", "ab"),
            Some(vec![some("b"), None, some("b")])
        );
        assert_eq!(match_groups("(a?)*", "aa"), Some(vec![some("")]));
    }

    #[test]
    fn escaping_and_anchors() {
        // Only ERE metacharacters may be escaped.
        assert!(matches("\\.", "."));
        assert!(!matches("\\.", "z"));
        assert!(matches("a\\{2\\}", "a{2}"));
        assert!(Regex::compile(b"\\1").is_err());
        assert!(Regex::compile(b"\\b").is_err());
        assert!(Regex::compile(b"\\d").is_err());
        // trailing backslash is a literal backslash
        assert!(matches("\\", "\\"));
        // anchors
        assert!(matches("^$", ""));
        assert!(matches("^a", "a"));
        assert_eq!(match_groups("(a$)", "a"), Some(vec![some("a")]));
        assert!(!matches("a$b", "a"));
        // `.` matches any byte including newline
        assert!(matches(".", "\n"));
    }

    #[test]
    fn bracket_expressions() {
        assert!(matches("[a-z]", "m"));
        assert!(matches("[]a]", "]"));
        assert!(matches("[^]a]", "b"));
        assert!(!matches("[^]a]", "]"));
        assert!(matches("[a-]", "-"));
        assert!(matches("[-a]", "-"));
        assert!(matches("[\\]", "\\")); // backslash literal in brackets
        assert!(!matches("[z-a]", "z")); // reversed range: empty
        assert!(matches("[[:alpha:]-z]", "-")); // `-` after class is literal
        assert!(matches("[[:xdigit:]]", "f"));
        assert!(!matches("[[:xdigit:]]", "g"));
        assert!(matches("[[:punct:]]", "!"));
        assert!(!matches("[[:punct:]]", "a"));
    }

    #[test]
    fn compile_errors() {
        for bad in [
            "", "*a", "a**", "(*)", "()", "a|", "|a", "(|a)", "+", "?", "a+*", "{2}", "a{2,1}",
            "a{", "a{2", "a{,2}", "a{b}", "{", "(a", "[[:foo:]]", "[abc",
        ] {
            assert!(Regex::compile(bad.as_bytes()).is_err(), "expected error for {bad:?}");
        }
        // `}` and `)` are literals; `a{2}` is a valid interval.
        assert!(matches("}", "}"));
        assert!(matches(")", ")"));
        assert!(matches("a{2}", "aa"));
    }

    #[test]
    fn find_iter_zero_width() {
        let re = Regex::compile(b"a*").unwrap();
        let ms = re.find_iter(b"baab");
        let spans: Vec<(usize, usize)> = ms.iter().map(|m| (m.start, m.end)).collect();
        assert_eq!(spans, vec![(0, 0), (1, 3), (3, 3), (4, 4)]);

        let re = Regex::compile(b"x*").unwrap();
        let ms = re.find_iter(b"abc");
        let spans: Vec<(usize, usize)> = ms.iter().map(|m| (m.start, m.end)).collect();
        assert_eq!(spans, vec![(0, 0), (1, 1), (2, 2), (3, 3)]);

        let re = Regex::compile(b"a").unwrap();
        let ms = re.find_iter(b"aaa");
        let spans: Vec<(usize, usize)> = ms.iter().map(|m| (m.start, m.end)).collect();
        assert_eq!(spans, vec![(0, 1), (1, 2), (2, 3)]);
    }

    // --- differential fuzz test against the oracle -------------------------
    //
    // Ignored by default; run manually with:
    //   cargo test -p jinx-eval regex::tests::fuzz_against_oracle -- --ignored --nocapture

    const ORACLE: &str = "$JINX_ROOT/.oracle/bin/nix-instantiate";

    fn oracle_eval(expr: &str) -> Result<String, String> {
        let out = Command::new(ORACLE)
            .args([
                "--eval",
                "--strict",
                "--extra-experimental-features",
                "nix-command",
                "-E",
                expr,
            ])
            .env("NIX_REMOTE", "dummy://")
            .env("NIX_STORE_DIR", "/nix/store")
            .output()
            .expect("run oracle");
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !stdout.is_empty() {
            Ok(stdout)
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    /// Escape a pattern for embedding in a Nix double-quoted string literal.
    fn nix_escape(pat: &[u8]) -> String {
        let mut out = String::new();
        for &b in pat {
            match b {
                b'\\' => out.push_str("\\\\"),
                b'"' => out.push_str("\\\""),
                b'$' => out.push_str("\\$"),
                _ => out.push(b as char),
            }
        }
        out
    }

    /// Serialize an engine match/split result to a Nix expression (inputs use
    /// only `a`,`b`,`c` so no string escaping of the values is needed).
    fn nix_str(bytes: &[u8]) -> String {
        format!("\"{}\"", String::from_utf8_lossy(bytes))
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 16
        }
        fn upto(&mut self, n: usize) -> usize {
            (self.next() as usize) % n
        }
    }

    fn random_pattern(rng: &mut Lcg) -> Vec<u8> {
        let toks: &[&[u8]] = &[
            b"a", b"b", b"c", b".", b"*", b"+", b"?", b"(", b")", b"|", b"[", b"]", b"^", b"$",
            b"[abc]", b"[^ab]", b"[a-c]", b"{1,2}", b"{2}", b"a*", b"b+",
            // composite tokens that exercise nested groups and repeated groups
            b"(a)", b"(a|b)", b"((a)|(b))", b"(a)(b)", b"(a|b)+", b"((a)|(b))*", b"(a?)",
        ];
        let len = 1 + rng.upto(6);
        let mut p = Vec::new();
        for _ in 0..len {
            p.extend_from_slice(toks[rng.upto(toks.len())]);
        }
        p
    }

    fn random_input(rng: &mut Lcg) -> Vec<u8> {
        let len = rng.upto(6);
        (0..len).map(|_| b"abc"[rng.upto(3)]).collect()
    }

    #[test]
    #[ignore]
    fn fuzz_against_oracle() {
        let mut rng = Lcg(0x1234_5678_9abc_def0);
        let mut checked = 0usize;
        let mut divergences = 0usize;
        for _ in 0..3000 {
            let pat = random_pattern(&mut rng);
            let input = random_input(&mut rng);
            let pat_nix = nix_escape(&pat);
            let input_nix = nix_escape(&input);

            let compiled = Regex::compile(&pat);

            // ---- match ----
            let expected = compiled
                .as_ref()
                .ok()
                .and_then(|re| re.match_full(&input))
                .map(|caps| {
                    let items: Vec<String> = caps
                        .iter()
                        .map(|g| match g {
                            None => "null".to_string(),
                            Some((a, b)) => nix_str(&input[*a..*b]),
                        })
                        .collect();
                    format!("[ {} ]", items.join(" "))
                });
            let expr = match &expected {
                Some(v) => format!(
                    "(builtins.match \"{pat_nix}\" \"{input_nix}\") == {v}"
                ),
                None => format!("(builtins.match \"{pat_nix}\" \"{input_nix}\") == null"),
            };
            match oracle_eval(&expr) {
                Ok(v) if v == "true" => checked += 1,
                Ok(v) => {
                    divergences += 1;
                    eprintln!(
                        "MATCH DIVERGENCE pat={:?} input={:?} engine={:?} oracle-eq={}",
                        String::from_utf8_lossy(&pat),
                        String::from_utf8_lossy(&input),
                        expected,
                        v
                    );
                }
                Err(_) => {
                    // Oracle rejects the pattern; our compile must also reject it.
                    if compiled.is_ok() {
                        divergences += 1;
                        eprintln!(
                            "COMPILE DIVERGENCE (oracle rejects, engine accepts) pat={:?}",
                            String::from_utf8_lossy(&pat)
                        );
                    }
                    continue;
                }
            }

            // ---- split ----
            if let Ok(re) = &compiled {
                let ms = re.find_iter(&input);
                let mut parts = Vec::new();
                let mut last = 0;
                for m in &ms {
                    parts.push(nix_str(&input[last..m.start]));
                    let gs: Vec<String> = m
                        .groups
                        .iter()
                        .map(|gr| match gr {
                            None => "null".to_string(),
                            Some((a, b)) => nix_str(&input[*a..*b]),
                        })
                        .collect();
                    parts.push(format!("[ {} ]", gs.join(" ")));
                    last = m.end;
                }
                parts.push(nix_str(&input[last..]));
                let expected_split = format!("[ {} ]", parts.join(" "));
                let expr = format!(
                    "(builtins.split \"{pat_nix}\" \"{input_nix}\") == {expected_split}"
                );
                match oracle_eval(&expr) {
                    Ok(v) if v == "true" => checked += 1,
                    Ok(v) => {
                        divergences += 1;
                        eprintln!(
                            "SPLIT DIVERGENCE pat={:?} input={:?} engine={} oracle-eq={}",
                            String::from_utf8_lossy(&pat),
                            String::from_utf8_lossy(&input),
                            expected_split,
                            v
                        );
                    }
                    Err(e) => {
                        divergences += 1;
                        eprintln!(
                            "SPLIT ERROR pat={:?} input={:?} err={}",
                            String::from_utf8_lossy(&pat),
                            String::from_utf8_lossy(&input),
                            e
                        );
                    }
                }
            }
        }
        eprintln!("fuzz: {checked} checks passed, {divergences} divergences");
        assert_eq!(divergences, 0, "found {divergences} divergences");
    }
}
