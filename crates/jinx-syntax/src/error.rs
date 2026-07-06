//! Parse errors and their rendering, byte-compatible with Nix's
//! `showErrorInfo` (libutil/error.cc) plus the logger's
//! `filterANSIEscapes` pass (libutil/terminal.cc) for the non-terminal
//! case.

use crate::pos::{split_lines, Origin, PosIdx, PosTable};

#[derive(Debug)]
pub struct ParseError {
    /// Raw message bytes (Nix messages can embed arbitrary source bytes).
    pub msg: Vec<u8>,
    pub pos: PosIdx,
}

impl ParseError {
    pub fn new(msg: impl Into<Vec<u8>>, pos: PosIdx) -> Self {
        ParseError {
            msg: msg.into(),
            pos,
        }
    }

    /// Render as `error: ...` with position and source excerpt, exactly as
    /// Nix prints it to a non-terminal stderr (no trailing newline; the
    /// caller appends one, like Nix's logger). This includes the logger's
    /// `filterANSIEscapes` pass (tab expansion etc.).
    pub fn render(&self, positions: &PosTable) -> Vec<u8> {
        // Build the equivalent of the `oss` stream in showErrorInfo.
        let mut oss: Vec<u8> = Vec::new();
        oss.extend_from_slice(&self.msg);
        oss.push(b'\n');
        if let (Some(pos), Some(origin)) =
            (positions.lookup(self.pos), positions.origin_of(self.pos))
        {
            // printPosMaybe: "at <pos>:" then code lines
            oss.extend_from_slice(format!("at {pos}:").as_bytes());
            oss.extend_from_slice(&code_lines(origin, pos.line, pos.column));
            oss.push(b'\n');
        }
        // out << indent("error: ", "       ", chomp(oss));
        // the logger then applies filterANSIEscapes to the whole message.
        filter_ansi_escapes(&indent(b"error: ", b"       ", chomp(&oss)))
    }
}

/// A single trace frame for [`render_error`], in C++ print order
/// (outermost first). `text` is the raw hint bytes; `pos` may be [`NO_POS`].
pub struct RenderFrame {
    pub pos: PosIdx,
    pub text: Vec<u8>,
    /// `TracePrint::Always` — printed even when the trace is truncated.
    pub always: bool,
}

/// Port of `printPosMaybe`: append `<indent>at <pos>:` plus the source
/// excerpt (when available) to `out`. Returns whether a position was shown.
fn print_pos_maybe(out: &mut Vec<u8>, indent: &[u8], pos: PosIdx, positions: &PosTable) -> bool {
    if let (Some(p), Some(origin)) = (positions.lookup(pos), positions.origin_of(pos)) {
        out.extend_from_slice(indent);
        out.extend_from_slice(format!("at {p}:").as_bytes());
        out.extend_from_slice(&code_lines(origin, p.line, p.column));
        out.push(b'\n');
        true
    } else {
        false
    }
}

/// Full port of `showErrorInfo` for the `error:` level with `--show-trace`
/// always on: renders trace frames (with C++ dedup / "duplicate frames
/// omitted" semantics) followed by the final error message and position.
///
/// `frames` are in print order (outermost first). `msg` is the base error
/// message; `suffix` is appended verbatim after the position block (used for
/// "Did you mean …?").
pub fn render_error(
    msg: &[u8],
    pos: PosIdx,
    frames: &[RenderFrame],
    suffix: &[u8],
    show_trace: bool,
    positions: &PosTable,
) -> Vec<u8> {
    let mut oss: Vec<u8> = Vec::new();

    // Filter out empty-hint frames up front (C++ `continue`s on them).
    let frames: Vec<&RenderFrame> = frames.iter().filter(|f| !f.text.is_empty()).collect();

    if !frames.is_empty() {
        let mut seen: std::collections::HashSet<(u32, Vec<u8>)> = std::collections::HashSet::new();
        let mut skipped: Vec<&RenderFrame> = Vec::new();
        let mut count: usize = 0;
        let mut truncate = false;

        // Appends a frame; returns 1 if it carried a position (which counts
        // as an extra trace toward the truncation limit), else 0.
        let print_trace = |out: &mut Vec<u8>, f: &RenderFrame| -> usize {
            out.push(b'\n');
            out.extend_from_slice("… ".as_bytes());
            out.extend_from_slice(&f.text);
            out.push(b'\n');
            usize::from(print_pos_maybe(out, b"  ", f.pos, positions))
        };

        let flush_skipped =
            |out: &mut Vec<u8>, skipped: &mut Vec<&RenderFrame>, count: &mut usize| {
                if !skipped.is_empty() {
                    if skipped.len() <= 5 {
                        for f in skipped.iter() {
                            *count += 1;
                            *count += print_trace(out, f);
                        }
                    } else {
                        out.push(b'\n');
                        out.extend_from_slice(
                            format!("({} duplicate frames omitted)", skipped.len()).as_bytes(),
                        );
                        out.push(b'\n');
                    }
                    skipped.clear();
                }
            };

        for f in &frames {
            if !show_trace && count > 3 {
                truncate = true;
            }
            if !truncate || f.always {
                let key = (f.pos.0, f.text.clone());
                if seen.contains(&key) {
                    skipped.push(f);
                    continue;
                }
                seen.insert(key);
                flush_skipped(&mut oss, &mut skipped, &mut count);
                count += 1;
                count += print_trace(&mut oss, f);
            }
        }
        flush_skipped(&mut oss, &mut skipped, &mut count);

        if truncate {
            oss.push(b'\n');
            oss.extend_from_slice(
                b"(stack trace truncated; use '--show-trace' to show the full, detailed trace)",
            );
            oss.push(b'\n');
        }

        oss.extend_from_slice(b"\nerror: ");
    }

    oss.extend_from_slice(msg);
    oss.push(b'\n');
    print_pos_maybe(&mut oss, b"", pos, positions);
    oss.extend_from_slice(suffix);

    filter_ansi_escapes(&indent(b"error: ", b"       ", chomp(&oss)))
}

fn code_lines(origin: &Origin, line: u32, column: u32) -> Vec<u8> {
    let lines = split_lines(origin.source());
    let line = line as usize;
    let mut out: Vec<u8> = Vec::new();
    let get = |n: usize| -> Option<&[u8]> {
        if n >= 1 && n <= lines.len() {
            Some(lines[n - 1])
        } else {
            None
        }
    };
    fn push_line(out: &mut Vec<u8>, n: usize, text: &[u8]) {
        out.extend_from_slice(format!("\n {n:>5}| ").as_bytes());
        out.extend_from_slice(text);
    }
    if line > 1 {
        if let Some(prev) = get(line - 1) {
            push_line(&mut out, line - 1, prev);
        }
    }
    if let Some(err) = get(line) {
        push_line(&mut out, line, err);
        if column > 0 {
            out.extend_from_slice(b"\n      |");
            out.extend_from_slice(&vec![b' '; column as usize]);
            out.push(b'^');
        }
    }
    if let Some(next) = get(line + 1) {
        push_line(&mut out, line + 1, next);
    }
    out
}

/// Nix's `chomp`: strip trailing whitespace.
fn chomp(s: &[u8]) -> &[u8] {
    let mut end = s.len();
    while end > 0 && matches!(s[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
        end -= 1;
    }
    &s[..end]
}

/// Nix's `indent(indentFirst, indentRest, s)`: prefix each line, chomping
/// each resulting line.
fn indent(first: &[u8], rest: &[u8], s: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for (i, line) in s.split(|&c| c == b'\n').enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        let mut prefixed: Vec<u8> = Vec::with_capacity(line.len() + 8);
        prefixed.extend_from_slice(if i == 0 { first } else { rest });
        prefixed.extend_from_slice(line);
        out.extend_from_slice(chomp(&prefixed));
    }
    out
}

/// Port of `charWidthUTF8Helper` (terminal.cc): (display width, bytes).
fn char_width_utf8(s: &[u8]) -> (usize, usize) {
    let c = s[0];
    let (mut ch, bytes, max): (u32, usize, u32) = if c & 0x80 == 0 {
        (c as u32, 1, 1 << 7)
    } else if c & 0xe0 == 0xc0 {
        ((c & 0x1f) as u32, 2, 1 << 11)
    } else if c & 0xf0 == 0xe0 {
        ((c & 0x0f) as u32, 3, 1 << 16)
    } else if c & 0xf8 == 0xf0 {
        ((c & 0x07) as u32, 4, 0x110000)
    } else {
        return (1, 1); // invalid UTF-8 start byte
    };
    for i in 1..bytes {
        if i < s.len() && s[i] & 0xc0 == 0x80 {
            ch = (ch << 6) | (s[i] & 0x3f) as u32;
        } else {
            return (i, i); // invalid encoding: one column per byte
        }
    }
    let mut width = bytes; // in case of overlong encoding
    if ch < max {
        width = char::from_u32(ch).map(char_display_width).unwrap_or(0);
    }
    (width, bytes)
}

/// Approximation of widechar_wcwidth with Nix's adjustments
/// (ambiguous -> 1, widened-in-9 -> 2, negative -> 0).
fn char_display_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x20 || (0x7f..0xa0).contains(&cp) {
        return 0; // control characters
    }
    // Wide ranges (East Asian Wide / Fullwidth), close enough to
    // widechar_width for the characters that can plausibly appear here.
    let wide = matches!(cp,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF |
        0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF |
        0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 |
        0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x2FFFD | 0x30000..=0x3FFFD);
    if wide {
        2
    } else {
        1
    }
}

/// Port of `filterANSIEscapes(s, filterAll = true)` (terminal.cc): strip
/// escape sequences, expand tabs (against a width counter that is *not*
/// reset at newlines), drop `\r` and `\a`.
pub fn filter_ansi_escapes(s: &[u8]) -> Vec<u8> {
    let mut t: Vec<u8> = Vec::with_capacity(s.len());
    let mut w: usize = 0;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == 0x1b {
            // ESC sequence: skip (filterAll drops even SGR)
            i += 1;
            if i < s.len() && s[i] == b'[' {
                i += 1;
                while i < s.len() && (0x30..=0x3f).contains(&s[i]) {
                    i += 1;
                }
                while i < s.len() && (0x20..=0x2f).contains(&s[i]) {
                    i += 1;
                }
                if i < s.len() && (0x40..=0x7e).contains(&s[i]) {
                    i += 1;
                }
            } else if i < s.len() && s[i] == b']' {
                i += 1;
                while i < s.len() && s[i] != 0x1b && s[i] != 0x07 {
                    i += 1;
                }
                if i < s.len() {
                    let v = s[i];
                    i += 1;
                    if i < s.len() && v == 0x1b && s[i] == b'\\' {
                        i += 1;
                    }
                }
            } else if i < s.len() && (0x40..=0x5f).contains(&s[i]) {
                i += 1;
            }
        } else if c == b'\t' {
            loop {
                w += 1;
                t.push(b' ');
                if w.is_multiple_of(8) {
                    break;
                }
            }
            i += 1;
        } else if c == b'\r' || c == 0x07 {
            i += 1;
        } else {
            let (cw, bytes) = char_width_utf8(&s[i..]);
            w += cw;
            t.extend_from_slice(&s[i..i + bytes]);
            i += bytes;
        }
    }
    t
}
