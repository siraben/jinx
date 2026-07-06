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
    /// `UsageError` (libutil): rendered like a normal error, but the CLI
    /// appends a "Try '<program> --help' for more information." line.
    Usage,
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
    /// `TracePrint::Always` (produced by `builtins.addErrorContext`): shown
    /// even when the trace is truncated (without `--show-trace`).
    pub always: bool,
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

    /// Render like C++ `showErrorInfo` with `--show-trace` on.
    pub fn render(&self, positions: &PosTable) -> Vec<u8> {
        self.render_with(positions, true)
    }

    /// Render like C++ `showErrorInfo`; `show_trace` controls whether the full
    /// trace is shown or truncated (keeping only `TracePrint::Always` frames).
    pub fn render_with(&self, positions: &PosTable, show_trace: bool) -> Vec<u8> {
        // The "Did you mean …?" suffix, appended after the position block.
        let mut suffix: Vec<u8> = Vec::new();
        if !self.suggestions.is_empty() {
            suffix.extend_from_slice(b"Did you mean ");
            if self.suggestions.len() == 1 {
                suffix.extend_from_slice(format!("{}?", self.suggestions[0]).as_bytes());
            } else {
                suffix.extend_from_slice(b"one of ");
                for (i, s) in self.suggestions.iter().enumerate() {
                    if i > 0 {
                        suffix.extend_from_slice(if i + 1 == self.suggestions.len() {
                            b" or ".as_slice()
                        } else {
                            b", ".as_slice()
                        });
                    }
                    suffix.extend_from_slice(s.as_bytes());
                }
                suffix.push(b'?');
            }
            suffix.push(b'\n');
        }
        // `traces` is stored in C++ `addTrace` order (each frame appended as
        // the stack unwinds). C++ prints them front-to-back where front is the
        // *last* added (push_front); replicate by iterating in reverse.
        let frames: Vec<jinx_syntax::error::RenderFrame> = self
            .traces
            .iter()
            .rev()
            .map(|t| jinx_syntax::error::RenderFrame {
                pos: t.pos,
                text: t.text.clone().into_bytes(),
                always: t.always,
            })
            .collect();
        jinx_syntax::error::render_error(
            &self.msg, self.pos, &frames, &suffix, show_trace, positions,
        )
    }
}

/// Port of `Suggestions::bestMatches` (libutil/suggestions.cc): the up-to-2
/// entries with the smallest Levenshtein distance, provided it is at most
/// `max(query.len(), match.len()) / 3`.
pub fn best_matches(candidates: impl Iterator<Item = String>, query: &str) -> Vec<String> {
    // Port of `Suggestions::bestMatches` + `trim(limit=5, maxDistance=2)`:
    // sort by (distance, name), keep distance ≤ 2, at most 5. Names are shown
    // verbatim (no quoting) — the quotes come from the surrounding message.
    let mut scored: Vec<(usize, String)> = candidates
        .map(|c| (levenshtein(query, &c), c))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.dedup();
    scored
        .into_iter()
        .filter(|(d, _)| *d <= 2)
        .take(5)
        .map(|(_, c)| c)
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
