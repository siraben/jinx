//! Nix's base-32 encoding ("nix32").
//!
//! Port of `src/libutil/base-nix-32.{cc,hh}`. The alphabet omits
//! `E O U T` and the bits are consumed in *reverse* order compared to
//! conventional base-32.

/// The nix32 alphabet: `0123456789abcdfghijklmnpqrsvwxyz` (omits e, o, u, t).
pub const CHARACTERS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

const INVALID: u8 = 0xff;

const REVERSE_MAP: [u8; 256] = {
    let mut map = [INVALID; 256];
    let mut i = 0;
    while i < 32 {
        map[CHARACTERS[i] as usize] = i as u8;
        i += 1;
    }
    map
};

/// Look up the value of a nix32 digit, if valid.
#[inline]
pub fn lookup_reverse(c: u8) -> Option<u8> {
    let digit = REVERSE_MAP[c as usize];
    if digit == INVALID {
        None
    } else {
        Some(digit)
    }
}

/// Length of the nix32 encoding of `original_length` bytes.
///
/// Port of `BaseNix32::encodedLength`. Note: `original_length` must be > 0.
#[inline]
pub const fn encoded_length(original_length: usize) -> usize {
    (original_length * 8 - 1) / 5 + 1
}

/// Encode bytes in nix32 (reversed-bits base-32).
///
/// Port of `BaseNix32::encode`.
pub fn encode(bs: &[u8]) -> String {
    if bs.is_empty() {
        return String::new();
    }

    let len = encoded_length(bs.len());
    let mut s = Vec::with_capacity(len);

    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        // Use u32 arithmetic: in C++ `byte << 8` truncates to 0 via integral
        // promotion, which u8 shifts in Rust would not allow.
        let next = if i >= bs.len() - 1 {
            0u32
        } else {
            (bs[i + 1] as u32) << (8 - j)
        };
        let c = (bs[i] as u32 >> j) | next;
        s.push(CHARACTERS[(c & 0x1f) as usize]);
    }

    // The alphabet is ASCII, so this is always valid UTF-8.
    unsafe { String::from_utf8_unchecked(s) }
}

/// Error type for nix32 decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadNix32Char(pub char);

impl std::fmt::Display for BadNix32Char {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid character in Nix32 (Nix's Base32 variation) string: '{}'",
            self.0
        )
    }
}

impl std::error::Error for BadNix32Char {}

/// Decode a nix32 string to bytes.
///
/// Port of `BaseNix32::decode`. Like the C++ version, this does not
/// validate that the string has a canonical length; the caller checks the
/// resulting byte length where it matters (e.g. hash parsing).
pub fn decode(s: &[u8]) -> Result<Vec<u8>, BadNix32Char> {
    let mut res: Vec<u8> = Vec::with_capacity((s.len() * 5 + 7) / 8);

    for n in 0..s.len() {
        let c = s[s.len() - n - 1];
        let digit = lookup_reverse(c).ok_or(BadNix32Char(c as char))?;

        let b = n * 5;
        let i = b / 8;
        let j = b % 8;

        if res.len() < i + 1 {
            res.resize(i + 1, 0);
        }
        res[i] |= digit << j;

        // Note: `digit >> (8 - j)` in C++ promotes to int; for j == 0 the
        // shift amount is 8 which yields 0 for a 5-bit digit.
        let carry = if j == 0 { 0 } else { digit >> (8 - j) };
        if carry != 0 {
            if res.len() < i + 2 {
                res.resize(i + 2, 0);
            }
            res[i + 1] |= carry;
        }
    }

    Ok(res)
}
