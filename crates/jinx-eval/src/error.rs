//! Evaluation errors: kinds (mirroring the C++ eval-error.hh hierarchy),
//! trace frames, and rendering in Nix's error-block format.

use jinx_syntax::pos::{PosIdx, PosTable};

/// Index into the VM's error table.
pub type ErrId = u32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrKind {
    /// Plain `EvalError`.
    Eval,
    Type,
    /// `assert` failure (catchable by tryEval).
    Assertion,
    /// `throw` (catchable by tryEval; subclass of AssertionError in C++).
    Thrown,
    /// `abort` (NOT catchable).
    Abort,
    UndefinedVar,
    InfiniteRecursion,
    MissingArgument,
    /// max-call-depth exceeded (EvalBaseError; not cacheable in C++, but we
    /// treat it like other errors for M2).
    StackOverflow,
}

impl ErrKind {
    /// What `builtins.tryEval` catches: `AssertionError` and its subclass
    /// `ThrownError` (see prim_tryEval in primops.cc).
    pub fn catchable(self) -> bool {
        matches!(self, ErrKind::Assertion | ErrKind::Thrown)
    }
}

#[derive(Clone, Debug)]
pub struct Trace {
    pub pos: PosIdx,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct EvalError {
    pub kind: ErrKind,
    /// Message bytes (may embed arbitrary source bytes).
    pub msg: Vec<u8>,
    pub pos: PosIdx,
    /// Trace frames, innermost first (added while unwinding).
    pub traces: Vec<Trace>,
    /// "Did you mean ...?" suggestions.
    pub suggestions: Vec<String>,
}

impl EvalError {
    pub fn new(kind: ErrKind, msg: impl Into<Vec<u8>>, pos: PosIdx) -> Self {
        EvalError {
            kind,
            msg: msg.into(),
            pos,
            traces: Vec::new(),
            suggestions: Vec::new(),
        }
    }

    /// Render like C++ `showErrorInfo` (without --show-trace frames for
    /// now; trace fidelity is M3). Reuses jinx-syntax's renderer.
    pub fn render(&self, positions: &PosTable) -> Vec<u8> {
        let mut msg = self.msg.clone();
        if !self.suggestions.is_empty() {
            msg.extend_from_slice(b"\nDid you mean ");
            if self.suggestions.len() == 1 {
                msg.extend_from_slice(format!("{}?", self.suggestions[0]).as_bytes());
            } else {
                msg.extend_from_slice(b"one of ");
                for (i, s) in self.suggestions.iter().enumerate() {
                    if i > 0 {
                        msg.extend_from_slice(if i + 1 == self.suggestions.len() {
                            b" or ".as_slice()
                        } else {
                            b", ".as_slice()
                        });
                    }
                    msg.extend_from_slice(s.as_bytes());
                }
                msg.push(b'?');
            }
        }
        jinx_syntax::error::ParseError::new(msg, self.pos).render(positions)
    }
}

/// Port of `Suggestions::bestMatches` (libutil/suggestions.cc): the up-to-2
/// entries with the smallest Levenshtein distance, provided it is at most
/// `max(query.len(), match.len()) / 3`.
pub fn best_matches(candidates: impl Iterator<Item = String>, query: &str) -> Vec<String> {
    let mut scored: Vec<(usize, String)> = candidates
        .map(|c| (levenshtein(query, &c), c))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .filter(|(d, c)| *d <= std::cmp::max(query.len(), c.len()) / 3)
        .take(2)
        .map(|(_, c)| format!("'{c}'"))
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = std::cmp::min(std::cmp::min(cur[j - 1] + 1, prev[j] + 1), prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}
