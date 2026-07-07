//! Hand-written lexer, a faithful port of nix's `lexer.l` (flex), including
//! its start-condition stack, longest-match semantics, string/indented-string
//! rules, `${` interpolation nesting, paths (incl. `~/` and `<spath>`), URIs
//! and comments.

use crate::error::ParseError;
use crate::pos::{OriginId, PosIdx, PosTable};

#[derive(Clone, Debug, PartialEq)]
pub enum TokKind {
    Id,
    Str { has_indent: bool },
    IndStr { has_indent: bool },
    Int(i64),
    Float(f64),
    Path,
    HPath,
    SPath,
    PathEnd,
    Uri,
    If,
    Then,
    Else,
    Assert,
    With,
    Let,
    In,
    Rec,
    Inherit,
    OrKw,
    Eq,
    NEq,
    Leq,
    Geq,
    Update,
    Concat,
    And,
    Or,
    Impl,
    Ellipsis,
    DollarCurly,
    IndStringOpen,
    IndStringClose,
    /// Single-character token; may or may not be a token the grammar knows.
    Char(u8),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub kind: TokKind,
    /// Payload for Id/Str/IndStr/Path/HPath/SPath/Uri.
    pub text: Vec<u8>,
    pub begin: u32,
    pub end: u32,
}

/// Display name of a token for bison-style error messages, matching the
/// `%token` aliases in parser.y.
pub fn token_name(t: &TokKind) -> String {
    match t {
        TokKind::Id => "identifier".into(),
        TokKind::Str { .. } => "string".into(),
        TokKind::IndStr { .. } => "indented string".into(),
        TokKind::Int(_) => "integer".into(),
        TokKind::Float(_) => "floating-point literal".into(),
        TokKind::Path => "path".into(),
        TokKind::HPath => "'~/…' path".into(),
        TokKind::SPath => "'<…>' path".into(),
        TokKind::PathEnd => "end of path".into(),
        TokKind::Uri => "URI".into(),
        TokKind::If => "'if'".into(),
        TokKind::Then => "'then'".into(),
        TokKind::Else => "'else'".into(),
        TokKind::Assert => "'assert'".into(),
        TokKind::With => "'with'".into(),
        TokKind::Let => "'let'".into(),
        TokKind::In => "'in'".into(),
        TokKind::Rec => "'rec'".into(),
        TokKind::Inherit => "'inherit'".into(),
        TokKind::OrKw => "'or'".into(),
        TokKind::Eq => "'=='".into(),
        TokKind::NEq => "'!='".into(),
        TokKind::Leq => "'<='".into(),
        TokKind::Geq => "'>='".into(),
        TokKind::Update => "'//'".into(),
        TokKind::Concat => "'++'".into(),
        TokKind::And => "'&&'".into(),
        TokKind::Or => "'||'".into(),
        TokKind::Impl => "'->'".into(),
        TokKind::Ellipsis => "'...'".into(),
        TokKind::DollarCurly => "'${'".into(),
        TokKind::IndStringOpen => "start of an indented string".into(),
        TokKind::IndStringClose => "end of an indented string".into(),
        TokKind::Char(c) => {
            // Characters that appear as literal tokens in the grammar get
            // quoted names; anything else is bison's "invalid token".
            if b":;@!<>?+*/-.\"(){}[]=,".contains(c) {
                format!("'{}'", *c as char)
            } else {
                "invalid token".into()
            }
        }
        TokKind::Eof => "end of file".into(),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    /// flex INITIAL: bottom-of-stack marker, lexes like Default.
    Initial,
    Default,
    String,
    IndString,
    InPath,
    InPathSlash,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    stack: Vec<State>,
    last_begin: u32,
    last_end: u32,
    /// Location before the last matched rule (ParserLocation::stash), used
    /// by rules that rewind with yyless(0) + unstash.
    stash_begin: u32,
    stash_end: u32,
    origin: OriginId,
}

fn is_path_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'+')
}

fn is_id_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_id_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'\'' | b'-')
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a [u8], origin: OriginId) -> Self {
        Lexer {
            src,
            pos: 0,
            stack: vec![State::Initial],
            last_begin: 0,
            last_end: 0,
            stash_begin: 0,
            stash_end: 0,
            origin,
        }
    }

    fn state(&self) -> State {
        *self.stack.last().unwrap()
    }

    fn push(&mut self, s: State) {
        self.stack.push(s);
    }

    fn pop(&mut self) {
        self.stack.pop();
    }

    /// YY_USER_ACTION: advance the location over `len` consumed bytes.
    fn adjust_loc(&mut self, len: usize) {
        self.stash_begin = self.last_begin;
        self.stash_end = self.last_end;
        self.last_begin = self.last_end;
        self.last_end += len as u32;
    }

    fn tok(&mut self, kind: TokKind, len: usize) -> Token {
        let text = self.src[self.pos..self.pos + len].to_vec();
        self.adjust_loc(len);
        let t = Token {
            kind,
            text,
            begin: self.last_begin,
            end: self.last_end,
        };
        self.pos += len;
        t
    }

    fn tok_with_text(&mut self, kind: TokKind, len: usize, text: Vec<u8>) -> Token {
        self.adjust_loc(len);
        let t = Token {
            kind,
            text,
            begin: self.last_begin,
            end: self.last_end,
        };
        self.pos += len;
        t
    }

    fn eof(&self) -> Token {
        Token {
            kind: TokKind::Eof,
            text: vec![],
            begin: self.last_end,
            end: self.last_end,
        }
    }

    fn pos_at(&self, positions: &PosTable, offset: u32) -> PosIdx {
        positions.add(self.origin, offset)
    }

    /// Port of C++ `forceNoNullByte` (lexer.l): a string token whose (for
    /// regular strings: unescaped; for indented strings: raw) content contains
    /// a NUL byte cannot be represented as a Nix string. The NUL is rendered as
    /// U+2400 (␀) in the message, and the error is reported at the token's
    /// start position. Checked at lex time, matching Nix.
    fn no_null_byte(&self, positions: &PosTable, tok: &Token) -> Result<(), ParseError> {
        if !tok.text.contains(&0u8) {
            return Ok(());
        }
        let mut shown = Vec::with_capacity(tok.text.len());
        for &b in &tok.text {
            if b == 0 {
                shown.extend_from_slice("␀".as_bytes());
            } else {
                shown.push(b);
            }
        }
        let mut msg =
            b"input string '".to_vec();
        msg.extend_from_slice(&shown);
        msg.extend_from_slice(
            b"' cannot be represented as Nix string because it contains null bytes",
        );
        Err(ParseError::new(msg, self.pos_at(positions, tok.begin)))
    }

    pub fn next_token(&mut self, positions: &PosTable) -> Result<Token, ParseError> {
        match self.state() {
            State::String => self.lex_string(positions),
            State::IndString => self.lex_ind_string(positions),
            State::InPath | State::InPathSlash => self.lex_in_path(positions),
            State::Initial | State::Default => self.lex_default(positions),
        }
    }

    // ---------- DEFAULT / INITIAL ----------

    fn lex_default(&mut self, positions: &PosTable) -> Result<Token, ParseError> {
        loop {
            if self.pos >= self.src.len() {
                return Ok(self.eof());
            }
            let rest = &self.src[self.pos..];

            // Determine the longest match among all rules; ties go to the
            // rule listed earlier in lexer.l. We enumerate candidates as
            // (length, rule-id) and emulate that ordering.
            #[derive(Clone, Copy, PartialEq)]
            enum Rule {
                Kw(&'static str, u8), // keyword / fixed operator, id encodes which
                Id,
                Int,
                Float,
                DollarCurly,
                CloseBrace,
                OpenBrace,
                Quote,
                IndOpen,
                PathSegDollar,
                HPathStartDollar,
                Path,
                HPath,
                SPath,
                Uri,
                Ws,
                LineComment,
                BlockComment,
                Any,
            }

            let mut best: Option<(usize, usize, Rule)> = None; // (len, order, rule)
            let consider = |len: usize, order: usize, rule: Rule, best: &mut Option<(usize, usize, Rule)>| {
                if len == 0 {
                    return;
                }
                match best {
                    Some((blen, border, _)) if *blen > len || (*blen == len && *border <= order) => {}
                    _ => *best = Some((len, order, rule)),
                }
            };

            // Fixed strings, in lexer.l order.
            const FIXED: &[(&str, u8)] = &[
                ("if", 0),
                ("then", 1),
                ("else", 2),
                ("assert", 3),
                ("with", 4),
                ("let", 5),
                ("in", 6),
                ("rec", 7),
                ("inherit", 8),
                ("or", 9),
                ("...", 10),
                ("==", 11),
                ("!=", 12),
                ("<=", 13),
                (">=", 14),
                ("&&", 15),
                ("||", 16),
                ("->", 17),
                ("//", 18),
                ("++", 19),
                ("<|", 20),
                ("|>", 21),
            ];
            // First bytes of all FIXED entries.
            const KW_FIRST: [bool; 256] = {
                let mut t = [false; 256];
                let firsts = b"itewalro.=!<>&|-/+";
                let mut i = 0;
                while i < firsts.len() {
                    t[firsts[i] as usize] = true;
                    i += 1;
                }
                t
            };
            // First-byte gating: every candidate rule's language constrains its
            // first byte, so only enumerate rules whose first-byte class
            // matches `c`. The `consider` calls (and their order arguments)
            // are byte-for-byte the same as the ungated enumeration, so the
            // longest-match + rule-order tie-break semantics are unchanged.
            let c = rest[0];
            if KW_FIRST[c as usize] {
                for (order, (s, id)) in FIXED.iter().enumerate() {
                    let sb = s.as_bytes();
                    if sb[0] == c && rest.starts_with(sb) {
                        consider(s.len(), order, Rule::Kw(s, *id), &mut best);
                    }
                }
            }
            let base = FIXED.len();
            if is_id_start(c) {
                consider(match_id(rest), base, Rule::Id, &mut best);
            }
            if c.is_ascii_digit() {
                consider(match_int(rest), base + 1, Rule::Int, &mut best);
            }
            // FLOAT starts [0-9] (alt1 [1-9]..., alt2 0?\.) or '.' (alt2).
            if c.is_ascii_digit() || c == b'.' {
                consider(match_float(rest), base + 2, Rule::Float, &mut best);
            }
            if c == b'$' && rest.starts_with(b"${") {
                consider(2, base + 3, Rule::DollarCurly, &mut best);
            }
            if c == b'}' {
                consider(1, base + 4, Rule::CloseBrace, &mut best);
            }
            if c == b'{' {
                consider(1, base + 5, Rule::OpenBrace, &mut best);
            }
            if c == b'"' {
                consider(1, base + 6, Rule::Quote, &mut best);
            }
            if c == b'\'' {
                consider(match_ind_open(rest), base + 7, Rule::IndOpen, &mut best);
            }
            // PATH / PATH_SEG${: {PATH_CHAR}* may be empty, so they can start
            // with a path char or directly with '/'.
            if is_path_char(c) || c == b'/' {
                consider(
                    match_path_seg_dollar(rest),
                    base + 8,
                    Rule::PathSegDollar,
                    &mut best,
                );
            }
            if c == b'~' {
                consider(
                    match_hpath_start_dollar(rest),
                    base + 9,
                    Rule::HPathStartDollar,
                    &mut best,
                );
            }
            if is_path_char(c) || c == b'/' {
                consider(match_path(rest), base + 10, Rule::Path, &mut best);
            }
            if c == b'~' {
                consider(match_hpath(rest), base + 11, Rule::HPath, &mut best);
            }
            if c == b'<' {
                consider(match_spath(rest), base + 12, Rule::SPath, &mut best);
            }
            if c.is_ascii_alphabetic() {
                consider(match_uri(rest), base + 13, Rule::Uri, &mut best);
            }
            if matches!(c, b' ' | b'\t' | b'\r' | b'\n') {
                consider(match_ws(rest), base + 14, Rule::Ws, &mut best);
            }
            if c == b'#' {
                consider(match_line_comment(rest), base + 15, Rule::LineComment, &mut best);
            }
            if c == b'/' {
                consider(
                    match_block_comment(rest),
                    base + 16,
                    Rule::BlockComment,
                    &mut best,
                );
            }
            consider(1, base + 17, Rule::Any, &mut best);

            let (len, _, rule) = best.unwrap();
            match rule {
                Rule::Kw(_, id) => {
                    let kind = match id {
                        0 => TokKind::If,
                        1 => TokKind::Then,
                        2 => TokKind::Else,
                        3 => TokKind::Assert,
                        4 => TokKind::With,
                        5 => TokKind::Let,
                        6 => TokKind::In,
                        7 => TokKind::Rec,
                        8 => TokKind::Inherit,
                        9 => TokKind::OrKw,
                        10 => TokKind::Ellipsis,
                        11 => TokKind::Eq,
                        12 => TokKind::NEq,
                        13 => TokKind::Leq,
                        14 => TokKind::Geq,
                        15 => TokKind::And,
                        16 => TokKind::Or,
                        17 => TokKind::Impl,
                        18 => TokKind::Update,
                        19 => TokKind::Concat,
                        20 | 21 => {
                            self.adjust_loc(len);
                            self.pos += len;
                            return Err(ParseError::new(
                                "experimental Nix feature 'pipe-operators' is disabled; \
                                 add '--extra-experimental-features pipe-operators' to enable it",
                                self.pos_at(positions, self.last_begin),
                            ));
                        }
                        _ => unreachable!(),
                    };
                    return Ok(self.tok(kind, len));
                }
                Rule::Id => return Ok(self.tok(TokKind::Id, len)),
                Rule::Int => {
                    let text = &rest[..len];
                    let s = std::str::from_utf8(text).unwrap();
                    match s.parse::<i64>() {
                        Ok(n) => return Ok(self.tok(TokKind::Int(n), len)),
                        Err(_) => {
                            self.adjust_loc(len);
                            self.pos += len;
                            return Err(ParseError::new(
                                format!("invalid integer '{s}'"),
                                self.pos_at(positions, self.last_begin),
                            ));
                        }
                    }
                }
                Rule::Float => {
                    let s = std::str::from_utf8(&rest[..len]).unwrap().to_string();
                    let v: f64 = s.parse().unwrap_or(f64::INFINITY);
                    if v.is_infinite() {
                        self.adjust_loc(len);
                        self.pos += len;
                        return Err(ParseError::new(
                            format!("invalid float '{s}'"),
                            self.pos_at(positions, self.last_begin),
                        ));
                    }
                    return Ok(self.tok(TokKind::Float(v), len));
                }
                Rule::DollarCurly => {
                    self.push(State::Default);
                    return Ok(self.tok(TokKind::DollarCurly, len));
                }
                Rule::CloseBrace => {
                    if self.state() != State::Initial {
                        self.pop();
                    }
                    return Ok(self.tok(TokKind::Char(b'}'), len));
                }
                Rule::OpenBrace => {
                    self.push(State::Default);
                    return Ok(self.tok(TokKind::Char(b'{'), len));
                }
                Rule::Quote => {
                    self.push(State::String);
                    return Ok(self.tok(TokKind::Char(b'"'), len));
                }
                Rule::IndOpen => {
                    self.push(State::IndString);
                    return Ok(self.tok(TokKind::IndStringOpen, len));
                }
                Rule::PathSegDollar => {
                    // Emit PATH for the segment only; "${" is re-scanned in
                    // INPATH_SLASH (like PATH_START in flex).
                    let seg_len = len - 2;
                    self.push(State::InPathSlash);
                    return Ok(self.tok(TokKind::Path, seg_len));
                }
                Rule::HPathStartDollar => {
                    let seg_len = len - 2; // "~/"
                    self.push(State::InPathSlash);
                    return Ok(self.tok(TokKind::HPath, seg_len));
                }
                Rule::Path => {
                    let ends_slash = rest[len - 1] == b'/';
                    self.push(if ends_slash {
                        State::InPathSlash
                    } else {
                        State::InPath
                    });
                    return Ok(self.tok(TokKind::Path, len));
                }
                Rule::HPath => {
                    let ends_slash = rest[len - 1] == b'/';
                    self.push(if ends_slash {
                        State::InPathSlash
                    } else {
                        State::InPath
                    });
                    return Ok(self.tok(TokKind::HPath, len));
                }
                Rule::SPath => return Ok(self.tok(TokKind::SPath, len)),
                Rule::Uri => return Ok(self.tok(TokKind::Uri, len)),
                Rule::Ws | Rule::LineComment | Rule::BlockComment => {
                    self.adjust_loc(len);
                    self.pos += len;
                    continue;
                }
                Rule::Any => {
                    let c = rest[0];
                    return Ok(self.tok(TokKind::Char(c), 1));
                }
            }
        }
    }

    // ---------- STRING ----------

    fn lex_string(&mut self, positions: &PosTable) -> Result<Token, ParseError> {
        let src = self.src;
        let len = src.len();
        let p = self.pos;
        let mut q = p;
        // ([^\$\"\\]|\$[^\{\"\\]|\\{ANY}|\$\\{ANY})+ with the additional
        // (...)*\$/\" variant: a '$' directly before the closing quote is
        // included in the string.
        while q < len {
            match src[q] {
                b'\\' => {
                    if q + 1 < len {
                        q += 2;
                    } else {
                        break;
                    }
                }
                b'$' => {
                    if q + 1 >= len {
                        break;
                    }
                    match src[q + 1] {
                        b'{' => break,
                        b'"' => {
                            q += 1; // trailing-$ variant
                            break;
                        }
                        b'\\' => {
                            if q + 2 < len {
                                q += 3;
                            } else {
                                break;
                            }
                        }
                        _ => q += 2,
                    }
                }
                b'"' => break,
                _ => q += 1,
            }
        }
        if q > p {
            let text = unescape_str(&src[p..q]);
            let tok = self.tok_with_text(TokKind::Str { has_indent: false }, q - p, text);
            self.no_null_byte(positions, &tok)?;
            return Ok(tok);
        }
        if p >= len {
            return Ok(self.eof());
        }
        match src[p] {
            b'"' => {
                self.pop();
                Ok(self.tok(TokKind::Char(b'"'), 1))
            }
            b'$' if p + 1 < len && src[p + 1] == b'{' => {
                self.push(State::Default);
                Ok(self.tok(TokKind::DollarCurly, 2))
            }
            b'$' | b'\\' => {
                // \$|\\|\$\\ at EOF: consume and report EOF (the parser
                // fails with the exact location).
                let n = if src[p] == b'$' && p + 1 < len && src[p + 1] == b'\\' {
                    2
                } else {
                    1
                };
                self.adjust_loc(n);
                self.pos += n;
                Ok(self.eof())
            }
            _ => unreachable!("string lexer stuck"),
        }
    }

    // ---------- IND_STRING ----------

    fn lex_ind_string(&mut self, positions: &PosTable) -> Result<Token, ParseError> {
        let src = self.src;
        let len = src.len();
        let p = self.pos;
        if p >= len {
            return Ok(self.eof());
        }
        // Main content rule: ([^\$\']|\$[^\{\']|\'[^\'\$])+
        let mut q = p;
        while q < len {
            match src[q] {
                b'$' => {
                    if q + 1 < len && src[q + 1] != b'{' && src[q + 1] != b'\'' {
                        q += 2;
                    } else {
                        break;
                    }
                }
                b'\'' => {
                    if q + 1 < len && src[q + 1] != b'\'' && src[q + 1] != b'$' {
                        q += 2;
                    } else {
                        break;
                    }
                }
                _ => q += 1,
            }
        }
        let main_len = q - p;
        let rest = &src[p..];
        // Competing fixed rules (longest wins; main rule first on ties).
        let mut best_len = main_len;
        let mut best_rule = if main_len > 0 { 0 } else { usize::MAX }; // 0 = main
        let consider = |len: usize, rule: usize, best_len: &mut usize, best_rule: &mut usize| {
            if len > *best_len {
                *best_len = len;
                *best_rule = rule;
            }
        };
        if rest.starts_with(b"''$") {
            consider(3, 1, &mut best_len, &mut best_rule);
        }
        if rest.starts_with(b"'''") {
            consider(3, 2, &mut best_len, &mut best_rule);
        }
        if rest.starts_with(b"''\\") && rest.len() >= 4 {
            consider(4, 3, &mut best_len, &mut best_rule);
        }
        if rest.starts_with(b"${") {
            consider(2, 4, &mut best_len, &mut best_rule);
        }
        if rest.starts_with(b"''") {
            consider(2, 5, &mut best_len, &mut best_rule);
        }
        if rest.starts_with(b"'") {
            consider(1, 6, &mut best_len, &mut best_rule);
        }
        if rest.starts_with(b"$") && best_rule == usize::MAX {
            // <IND_STRING>\$ (lone $ before ' or at position where main
            // rule failed): IND_STR "$"
            consider(1, 7, &mut best_len, &mut best_rule);
        }
        match best_rule {
            0 => {
                let text = src[p..p + best_len].to_vec();
                let tok = self.tok_with_text(TokKind::IndStr { has_indent: true }, best_len, text);
                self.no_null_byte(positions, &tok)?;
                Ok(tok)
            }
            1 => Ok(self.tok_with_text(TokKind::IndStr { has_indent: false }, 3, b"$".to_vec())),
            2 => Ok(self.tok_with_text(TokKind::IndStr { has_indent: false }, 3, b"''".to_vec())),
            3 => {
                let text = unescape_str(&src[p + 2..p + 4]);
                let tok = self.tok_with_text(TokKind::IndStr { has_indent: false }, 4, text);
                self.no_null_byte(positions, &tok)?;
                Ok(tok)
            }
            4 => {
                self.push(State::Default);
                Ok(self.tok(TokKind::DollarCurly, 2))
            }
            5 => {
                self.pop();
                Ok(self.tok(TokKind::IndStringClose, 2))
            }
            6 => Ok(self.tok_with_text(TokKind::IndStr { has_indent: false }, 1, b"'".to_vec())),
            7 => Ok(self.tok_with_text(TokKind::IndStr { has_indent: false }, 1, b"$".to_vec())),
            _ => Ok(self.eof()),
        }
    }

    // ---------- INPATH / INPATH_SLASH ----------

    fn lex_in_path(&mut self, positions: &PosTable) -> Result<Token, ParseError> {
        let src = self.src;
        let len = src.len();
        let p = self.pos;
        let in_slash = self.state() == State::InPathSlash;
        if p < len {
            let rest = &src[p..];
            if rest.starts_with(b"${") {
                self.pop();
                self.push(State::InPath);
                self.push(State::Default);
                return Ok(self.tok(TokKind::DollarCurly, 2));
            }
            // {PATH}|{PATH_SEG}|{PATH_CHAR}+ (longest match)
            let l = match_path(rest)
                .max(match_path_seg(rest))
                .max(match_path_chars(rest));
            if l > 0 {
                let ends_slash = rest[l - 1] == b'/';
                self.pop();
                self.push(if ends_slash {
                    State::InPathSlash
                } else {
                    State::InPath
                });
                return Ok(self.tok(TokKind::Str { has_indent: false }, l));
            }
        }
        if in_slash {
            // <INPATH_SLASH>{ANY} / <<EOF>>: "path has a trailing slash"
            if p < len {
                self.adjust_loc(1);
                self.pos += 1;
            }
            return Err(ParseError::new(
                "path has a trailing slash",
                self.pos_at(positions, self.last_begin),
            ));
        }
        // <INPATH>{ANY} / <<EOF>>: end of path; re-scan the char in the
        // enclosing context (yyless(0) + loc unstash). At EOF no user
        // action ran, so the unstash reverts the location to the one
        // *before* the last matched rule (affecting a later EOF report).
        self.pop();
        if p >= len {
            self.last_begin = self.stash_begin;
            self.last_end = self.stash_end;
        }
        Ok(Token {
            kind: TokKind::PathEnd,
            text: vec![],
            begin: self.last_begin,
            end: self.last_end,
        })
    }
}

/// unescapeStr from lexer.l: process escapes and normalise CR / CRLF to LF.
fn unescape_str(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c == b'\\' {
            let c2 = s[i];
            i += 1;
            out.push(match c2 {
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                other => other,
            });
        } else if c == b'\r' {
            out.push(b'\n');
            if i < s.len() && s[i] == b'\n' {
                i += 1;
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------- pattern matchers (return match length, 0 = no match) ----------

fn match_id(s: &[u8]) -> usize {
    if s.is_empty() || !is_id_start(s[0]) {
        return 0;
    }
    let mut i = 1;
    while i < s.len() && is_id_char(s[i]) {
        i += 1;
    }
    i
}

fn match_int(s: &[u8]) -> usize {
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    i
}

/// FLOAT: (([1-9][0-9]*\.[0-9]*)|(0?\.[0-9]+))([Ee][+-]?[0-9]+)?
fn match_float(s: &[u8]) -> usize {
    let mantissa = |s: &[u8]| -> usize {
        // alt1: [1-9][0-9]*\.[0-9]*
        let alt1 = if !s.is_empty() && (b'1'..=b'9').contains(&s[0]) {
            let mut i = 1;
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
            if i < s.len() && s[i] == b'.' {
                i += 1;
                while i < s.len() && s[i].is_ascii_digit() {
                    i += 1;
                }
                i
            } else {
                0
            }
        } else {
            0
        };
        // alt2: 0?\.[0-9]+
        let alt2 = {
            let mut i = 0;
            if i < s.len() && s[i] == b'0' {
                i += 1;
            }
            if i < s.len() && s[i] == b'.' {
                i += 1;
                let start = i;
                while i < s.len() && s[i].is_ascii_digit() {
                    i += 1;
                }
                if i > start {
                    i
                } else {
                    0
                }
            } else {
                0
            }
        };
        alt1.max(alt2)
    };
    let m = mantissa(s);
    if m == 0 {
        return 0;
    }
    // optional exponent
    let rest = &s[m..];
    if !rest.is_empty() && (rest[0] == b'e' || rest[0] == b'E') {
        let mut i = 1;
        if i < rest.len() && (rest[i] == b'+' || rest[i] == b'-') {
            i += 1;
        }
        let start = i;
        while i < rest.len() && rest[i].is_ascii_digit() {
            i += 1;
        }
        if i > start {
            return m + i;
        }
    }
    m
}

/// IND_STRING_OPEN: \'\'(\ *\n)?
fn match_ind_open(s: &[u8]) -> usize {
    if !s.starts_with(b"''") {
        return 0;
    }
    let mut i = 2;
    while i < s.len() && s[i] == b' ' {
        i += 1;
    }
    if i < s.len() && s[i] == b'\n' {
        i + 1
    } else {
        2
    }
}

/// PATH: {PATH_CHAR}*(\/{PATH_CHAR}+)+\/?
fn match_path(s: &[u8]) -> usize {
    let mut i = 0;
    while i < s.len() && is_path_char(s[i]) {
        i += 1;
    }
    let mut groups = 0;
    let mut end = 0;
    loop {
        if i < s.len() && s[i] == b'/' {
            let mut j = i + 1;
            while j < s.len() && is_path_char(s[j]) {
                j += 1;
            }
            if j > i + 1 {
                groups += 1;
                i = j;
                end = i;
                continue;
            } else if groups > 0 {
                // optional trailing slash
                end = i + 1;
                break;
            }
        }
        break;
    }
    if groups > 0 {
        end
    } else {
        0
    }
}

/// PATH_SEG: {PATH_CHAR}*\/
fn match_path_seg(s: &[u8]) -> usize {
    let mut i = 0;
    while i < s.len() && is_path_char(s[i]) {
        i += 1;
    }
    if i < s.len() && s[i] == b'/' {
        i + 1
    } else {
        0
    }
}

fn match_path_chars(s: &[u8]) -> usize {
    let mut i = 0;
    while i < s.len() && is_path_char(s[i]) {
        i += 1;
    }
    i
}

/// {PATH_SEG}\$\{
fn match_path_seg_dollar(s: &[u8]) -> usize {
    let l = match_path_seg(s);
    if l > 0 && s[l..].starts_with(b"${") {
        l + 2
    } else {
        0
    }
}

/// {HPATH_START}\$\{ where HPATH_START = ~\/
fn match_hpath_start_dollar(s: &[u8]) -> usize {
    if s.starts_with(b"~/") && s[2..].starts_with(b"${") {
        4
    } else {
        0
    }
}

/// HPATH: \~(\/{PATH_CHAR}+)+\/?
fn match_hpath(s: &[u8]) -> usize {
    if s.is_empty() || s[0] != b'~' {
        return 0;
    }
    let mut i = 1;
    let mut groups = 0;
    let mut end = 0;
    loop {
        if i < s.len() && s[i] == b'/' {
            let mut j = i + 1;
            while j < s.len() && is_path_char(s[j]) {
                j += 1;
            }
            if j > i + 1 {
                groups += 1;
                i = j;
                end = i;
                continue;
            } else if groups > 0 {
                end = i + 1;
                break;
            }
        }
        break;
    }
    if groups > 0 {
        end
    } else {
        0
    }
}

/// SPATH: \<{PATH_CHAR}+(\/{PATH_CHAR}+)*\>
fn match_spath(s: &[u8]) -> usize {
    if s.is_empty() || s[0] != b'<' {
        return 0;
    }
    let mut i = 1;
    let start = i;
    while i < s.len() && is_path_char(s[i]) {
        i += 1;
    }
    if i == start {
        return 0;
    }
    loop {
        if i < s.len() && s[i] == b'/' {
            let mut j = i + 1;
            while j < s.len() && is_path_char(s[j]) {
                j += 1;
            }
            if j > i + 1 {
                i = j;
                continue;
            }
            return 0;
        }
        break;
    }
    if i < s.len() && s[i] == b'>' {
        i + 1
    } else {
        0
    }
}

/// URI: [a-zA-Z][a-zA-Z0-9\+\-\.]*\:[a-zA-Z0-9\%\/\?\:\@\&\=\+\$\,\-\_\.\!\~\*\']+
fn match_uri(s: &[u8]) -> usize {
    if s.is_empty() || !s[0].is_ascii_alphabetic() {
        return 0;
    }
    let mut i = 1;
    while i < s.len() && (s[i].is_ascii_alphanumeric() || matches!(s[i], b'+' | b'-' | b'.')) {
        i += 1;
    }
    if i >= s.len() || s[i] != b':' {
        return 0;
    }
    i += 1;
    let start = i;
    while i < s.len()
        && (s[i].is_ascii_alphanumeric()
            || matches!(
                s[i],
                b'%' | b'/'
                    | b'?'
                    | b':'
                    | b'@'
                    | b'&'
                    | b'='
                    | b'+'
                    | b'$'
                    | b','
                    | b'-'
                    | b'_'
                    | b'.'
                    | b'!'
                    | b'~'
                    | b'*'
                    | b'\''
            ))
    {
        i += 1;
    }
    if i > start {
        i
    } else {
        0
    }
}

fn match_ws(s: &[u8]) -> usize {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

fn match_line_comment(s: &[u8]) -> usize {
    if s.is_empty() || s[0] != b'#' {
        return 0;
    }
    let mut i = 1;
    while i < s.len() && s[i] != b'\r' && s[i] != b'\n' {
        i += 1;
    }
    i
}

/// Long comment (also covers doc comments): \/\*([^*]|\*+[^*/])*\*+\/
fn match_block_comment(s: &[u8]) -> usize {
    if !s.starts_with(b"/*") {
        return 0;
    }
    let mut i = 2;
    loop {
        if i >= s.len() {
            return 0; // unterminated: rule fails
        }
        if s[i] == b'*' {
            let mut j = i;
            while j < s.len() && s[j] == b'*' {
                j += 1;
            }
            if j < s.len() && s[j] == b'/' {
                return j + 1;
            }
            if j >= s.len() {
                return 0;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
}
