//! Parse errors and their rendering, byte-compatible with Nix's
//! `showErrorInfo` (libutil/error.cc) for the non-terminal (no ANSI) case.

use crate::pos::{split_lines, Origin, PosIdx, PosTable};

#[derive(Debug)]
pub struct ParseError {
    pub msg: String,
    pub pos: PosIdx,
}

impl ParseError {
    pub fn new(msg: impl Into<String>, pos: PosIdx) -> Self {
        ParseError {
            msg: msg.into(),
            pos,
        }
    }

    /// Render as `error: ...` with position and source excerpt, exactly as
    /// Nix prints it to a non-terminal stderr (no trailing newline; the
    /// caller appends one, like Nix's logger).
    pub fn render(&self, positions: &PosTable) -> String {
        // Build the equivalent of the `oss` stream in showErrorInfo.
        let mut oss = String::new();
        oss.push_str(&self.msg);
        oss.push('\n');
        if let (Some(pos), Some(origin)) = (positions.lookup(self.pos), positions.origin_of(self.pos))
        {
            // printPosMaybe: "at <pos>:" then code lines
            oss.push_str(&format!("at {pos}:"));
            oss.push_str(&code_lines(origin, pos.line, pos.column));
            oss.push('\n');
        }
        // out << indent("error: ", "       ", chomp(oss))
        indent("error: ", "       ", chomp(&oss))
    }
}

fn code_lines(origin: &Origin, line: u32, column: u32) -> String {
    let lines = split_lines(origin.source());
    let line = line as usize;
    let mut out = String::new();
    let get = |n: usize| -> Option<String> {
        if n >= 1 && n <= lines.len() {
            Some(String::from_utf8_lossy(lines[n - 1]).into_owned())
        } else {
            None
        }
    };
    if line > 1 {
        if let Some(prev) = get(line - 1) {
            out.push_str(&format!("\n {:>5}| {}", line - 1, prev));
        }
    }
    if let Some(err) = get(line) {
        out.push_str(&format!("\n {line:>5}| {err}"));
        if column > 0 {
            out.push_str(&format!(
                "\n      |{}^",
                " ".repeat(column as usize)
            ));
        }
    }
    if let Some(next) = get(line + 1) {
        out.push_str(&format!("\n {:>5}| {}", line + 1, next));
    }
    out
}

/// Nix's `chomp`: strip trailing whitespace.
fn chomp(s: &str) -> &str {
    s.trim_end_matches([' ', '\t', '\n', '\r'])
}

/// Nix's `indent(indentFirst, indentRest, s)`: prefix each line, chomping
/// each resulting line.
fn indent(first: &str, rest: &str, s: &str) -> String {
    let mut out = String::new();
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let prefixed = format!("{}{}", if i == 0 { first } else { rest }, line);
        out.push_str(chomp(&prefixed));
    }
    out
}
