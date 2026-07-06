//! Cryptographic hashes and their textual renderings.
//!
//! Port of `src/libutil/hash.{cc,hh}` and the base16/base64 codecs from
//! `src/libutil/base-n.cc`. BLAKE3 (experimental in C++ Nix) is not
//! supported.

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

/// Maximum size of a hash in bytes (sha512).
pub const MAX_HASH_SIZE: usize = 64;

/// Error while parsing a hash. Message fidelity with C++ `BadHash` is not
/// guaranteed, only the error *conditions*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadHash(pub String);

impl std::fmt::Display for BadHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BadHash {}

macro_rules! bad_hash {
    ($($arg:tt)*) => { BadHash(format!($($arg)*)) };
}

/// Hash algorithms supported by Nix (sans the experimental BLAKE3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HashAlgorithm {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl HashAlgorithm {
    /// Port of `regularHashSize`.
    pub const fn size(self) -> usize {
        match self {
            HashAlgorithm::Md5 => 16,
            HashAlgorithm::Sha1 => 20,
            HashAlgorithm::Sha256 => 32,
            HashAlgorithm::Sha512 => 64,
        }
    }

    /// Port of `printHashAlgo`.
    pub const fn name(self) -> &'static str {
        match self {
            HashAlgorithm::Md5 => "md5",
            HashAlgorithm::Sha1 => "sha1",
            HashAlgorithm::Sha256 => "sha256",
            HashAlgorithm::Sha512 => "sha512",
        }
    }

    /// Port of `parseHashAlgoOpt`.
    pub fn parse_opt(s: &str) -> Option<Self> {
        match s {
            "md5" => Some(HashAlgorithm::Md5),
            "sha1" => Some(HashAlgorithm::Sha1),
            "sha256" => Some(HashAlgorithm::Sha256),
            "sha512" => Some(HashAlgorithm::Sha512),
            _ => None,
        }
    }

    /// Port of `parseHashAlgo`.
    pub fn parse(s: &str) -> Result<Self, BadHash> {
        Self::parse_opt(s).ok_or_else(|| {
            bad_hash!("unknown hash algorithm '{s}', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'")
        })
    }
}

impl std::fmt::Display for HashAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Textual hash representations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashFormat {
    Base64,
    /// Nix-specific base-32 (see [`crate::nix32`]).
    Nix32,
    Base16,
    /// `<algo>-<base64>`, always includes the algorithm.
    Sri,
}

/// A hash value: an algorithm plus `algo.size()` bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash {
    pub algo: HashAlgorithm,
    bytes: [u8; MAX_HASH_SIZE],
}

impl PartialOrd for Hash {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hash {
    /// Port of `Hash::operator<=>`: compare size, then bytes, then algo.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.algo
            .size()
            .cmp(&other.algo.size())
            .then_with(|| self.as_bytes().cmp(other.as_bytes()))
            .then_with(|| self.algo.cmp(&other.algo))
    }
}

impl std::fmt::Debug for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Hash({})", self.to_string(HashFormat::Base16, true))
    }
}

impl Hash {
    /// An all-zero hash of the given algorithm.
    pub fn zero(algo: HashAlgorithm) -> Self {
        Hash {
            algo,
            bytes: [0; MAX_HASH_SIZE],
        }
    }

    /// Construct from raw digest bytes; `bytes.len()` must equal `algo.size()`.
    pub fn from_bytes(algo: HashAlgorithm, bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), algo.size());
        let mut h = Hash::zero(algo);
        h.bytes[..bytes.len()].copy_from_slice(bytes);
        h
    }

    /// The digest bytes (length `algo.size()`).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.algo.size()]
    }

    /// Port of `Hash::to_string`.
    pub fn to_string(&self, format: HashFormat, include_algo: bool) -> String {
        let mut s = String::new();
        if format == HashFormat::Sri || include_algo {
            s.push_str(self.algo.name());
            s.push(if format == HashFormat::Sri { '-' } else { ':' });
        }
        match format {
            HashFormat::Base16 => s.push_str(&base16_encode(self.as_bytes())),
            HashFormat::Nix32 => s.push_str(&crate::nix32::encode(self.as_bytes())),
            HashFormat::Base64 | HashFormat::Sri => s.push_str(&base64_encode(self.as_bytes())),
        }
        s
    }

    /// Port of `Hash::parseSRI`.
    pub fn parse_sri(original: &str) -> Result<Self, BadHash> {
        let (algo_s, rest) = original
            .split_once('-')
            .ok_or_else(|| bad_hash!("hash '{original}' is not SRI"))?;
        let algo = HashAlgorithm::parse(algo_s)?;
        parse_low_level(rest, algo, HashFormat::Sri)
    }

    /// Port of `Hash::parseAnyPrefixed`: requires an `algo:` or `algo-` prefix.
    pub fn parse_any_prefixed(original: &str) -> Result<Self, BadHash> {
        parse_any_helper(original, |parsed| {
            parsed.ok_or_else(|| bad_hash!("hash '{original}' does not include a type"))
        })
        .map(|(h, _)| h)
    }

    /// Port of `Hash::parseAny`: optional prefix, must agree with `opt_algo`.
    pub fn parse_any(original: &str, opt_algo: Option<HashAlgorithm>) -> Result<Self, BadHash> {
        Self::parse_any_returning_format(original, opt_algo).map(|(h, _)| h)
    }

    /// Port of `Hash::parseAnyReturningFormat`.
    pub fn parse_any_returning_format(
        original: &str,
        opt_algo: Option<HashAlgorithm>,
    ) -> Result<(Self, HashFormat), BadHash> {
        parse_any_helper(original, |parsed| match (parsed, opt_algo) {
            (None, None) => Err(bad_hash!(
                "hash '{original}' does not include a type, nor is the type otherwise known from context"
            )),
            (Some(p), Some(o)) if p != o => {
                Err(bad_hash!("hash '{original}' should have type '{o}'"))
            }
            (Some(p), _) => Ok(p),
            (None, Some(o)) => Ok(o),
        })
    }

    /// Port of `Hash::parseNonSRIUnprefixed`: format determined by length.
    pub fn parse_non_sri_unprefixed(s: &str, algo: HashAlgorithm) -> Result<Self, BadHash> {
        let format = base_from_size(s, algo)?;
        parse_low_level(s, algo, format)
    }

    /// Port of `Hash::parseExplicitFormatUnprefixed`.
    pub fn parse_explicit_format_unprefixed(
        s: &str,
        algo: HashAlgorithm,
        format: HashFormat,
    ) -> Result<Self, BadHash> {
        parse_low_level(s, algo, format)
    }
}

/// Port of `baseFromSize`: infer the format of an unprefixed hash string
/// from its length.
fn base_from_size(rest: &str, algo: HashAlgorithm) -> Result<HashFormat, BadHash> {
    let hash_size = algo.size();
    if rest.len() == hash_size * 2 {
        Ok(HashFormat::Base16)
    } else if rest.len() == crate::nix32::encoded_length(hash_size) {
        Ok(HashFormat::Nix32)
    } else if rest.len() == base64_encoded_length(hash_size) {
        Ok(HashFormat::Base64)
    } else {
        Err(bad_hash!(
            "hash '{rest}' has wrong length for hash algorithm '{}'",
            algo.name()
        ))
    }
}

/// Port of `parseLowLevel`.
fn parse_low_level(rest: &str, algo: HashAlgorithm, format: HashFormat) -> Result<Hash, BadHash> {
    let d = match format {
        HashFormat::Base16 => base16_decode(rest)?,
        HashFormat::Nix32 => {
            crate::nix32::decode(rest.as_bytes()).map_err(|e| bad_hash!("{e}"))?
        }
        HashFormat::Base64 | HashFormat::Sri => base64_decode(rest)?,
    };
    if d.len() != algo.size() {
        return Err(bad_hash!(
            "invalid hash '{rest}', length {} != expected length {}",
            d.len(),
            algo.size()
        ));
    }
    Ok(Hash::from_bytes(algo, &d))
}

/// Port of `parseAnyHelper`.
fn parse_any_helper<'a>(
    original: &'a str,
    resolve_algo: impl FnOnce(Option<HashAlgorithm>) -> Result<HashAlgorithm, BadHash>,
) -> Result<(Hash, HashFormat), BadHash> {
    let mut rest = original;
    let mut is_sri = false;

    let mut parsed_algo = None;
    if let Some((prefix, r)) = rest.split_once(':') {
        parsed_algo = Some(HashAlgorithm::parse(prefix)?);
        rest = r;
    } else if let Some((prefix, r)) = rest.split_once('-') {
        is_sri = true;
        parsed_algo = Some(HashAlgorithm::parse(prefix)?);
        rest = r;
    }

    let algo = resolve_algo(parsed_algo)?;

    let format = if is_sri {
        HashFormat::Sri
    } else {
        base_from_size(rest, algo)?
    };

    Ok((parse_low_level(rest, algo, format)?, format))
}

/// Port of `hashString`.
pub fn hash_string(algo: HashAlgorithm, s: impl AsRef<[u8]>) -> Hash {
    let mut sink = HashSink::new(algo);
    sink.update(s.as_ref());
    sink.finish().0
}

/// Port of `compressHash`: XOR-fold the hash down to `new_size` bytes.
///
/// The result keeps the original algorithm tag but has `new_size` bytes;
/// use [`CompressedHash::as_bytes`] to get at them.
pub fn compress_hash(hash: &Hash, new_size: usize) -> CompressedHash {
    let mut bytes = vec![0u8; new_size];
    for (i, b) in hash.as_bytes().iter().enumerate() {
        bytes[i % new_size] ^= b;
    }
    CompressedHash { bytes }
}

/// The result of [`compress_hash`]: an irregularly-sized digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedHash {
    bytes: Vec<u8>,
}

impl CompressedHash {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// nix32 rendering (as used for the hash part of store paths).
    pub fn to_nix32(&self) -> String {
        crate::nix32::encode(&self.bytes)
    }
}

/// Streaming hasher, analogous to C++ `HashSink`.
#[derive(Clone)]
pub enum HashSink {
    Md5(Md5, u64),
    Sha1(Sha1, u64),
    Sha256(Sha256, u64),
    Sha512(Sha512, u64),
}

impl HashSink {
    pub fn new(algo: HashAlgorithm) -> Self {
        match algo {
            HashAlgorithm::Md5 => HashSink::Md5(Md5::new(), 0),
            HashAlgorithm::Sha1 => HashSink::Sha1(Sha1::new(), 0),
            HashAlgorithm::Sha256 => HashSink::Sha256(Sha256::new(), 0),
            HashAlgorithm::Sha512 => HashSink::Sha512(Sha512::new(), 0),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            HashSink::Md5(h, n) => {
                h.update(data);
                *n += data.len() as u64;
            }
            HashSink::Sha1(h, n) => {
                h.update(data);
                *n += data.len() as u64;
            }
            HashSink::Sha256(h, n) => {
                h.update(data);
                *n += data.len() as u64;
            }
            HashSink::Sha512(h, n) => {
                h.update(data);
                *n += data.len() as u64;
            }
        }
    }

    /// Returns the hash and the number of bytes consumed.
    pub fn finish(self) -> (Hash, u64) {
        match self {
            HashSink::Md5(h, n) => (Hash::from_bytes(HashAlgorithm::Md5, &h.finalize()), n),
            HashSink::Sha1(h, n) => (Hash::from_bytes(HashAlgorithm::Sha1, &h.finalize()), n),
            HashSink::Sha256(h, n) => (Hash::from_bytes(HashAlgorithm::Sha256, &h.finalize()), n),
            HashSink::Sha512(h, n) => (Hash::from_bytes(HashAlgorithm::Sha512, &h.finalize()), n),
        }
    }
}

impl std::io::Write for HashSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// base16 / base64 codecs (port of src/libutil/base-n.cc)
// ---------------------------------------------------------------------------

const BASE16_CHARS: &[u8; 16] = b"0123456789abcdef";

/// Lower-case hex encoding.
pub fn base16_encode(b: &[u8]) -> String {
    let mut buf = String::with_capacity(b.len() * 2);
    for byte in b {
        buf.push(BASE16_CHARS[(byte >> 4) as usize] as char);
        buf.push(BASE16_CHARS[(byte & 0x0f) as usize] as char);
    }
    buf
}

/// Hex decoding (accepts upper and lower case). Errors on invalid
/// characters or odd length.
pub fn base16_decode(s: &str) -> Result<Vec<u8>, BadHash> {
    fn digit(c: u8) -> Result<u8, BadHash> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(bad_hash!(
                "invalid character in Base16 string: '{}'",
                c as char
            )),
        }
    }
    let s = s.as_bytes();
    if s.len() % 2 != 0 {
        return Err(bad_hash!("Base16 string has odd length"));
    }
    let mut res = Vec::with_capacity(s.len() / 2);
    for chunk in s.chunks_exact(2) {
        res.push(digit(chunk[0])? << 4 | digit(chunk[1])?);
    }
    Ok(res)
}

const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Port of `base64::encodedLength`.
pub const fn base64_encoded_length(orig_size: usize) -> usize {
    ((4 * orig_size / 3) + 3) & !3
}

/// Port of `base64::encode`.
pub fn base64_encode(s: &[u8]) -> String {
    let mut res = String::with_capacity((s.len() + 2) / 3 * 4);
    let mut data: u32 = 0;
    let mut nbits: u32 = 0;

    for &c in s {
        data = data << 8 | c as u32;
        nbits += 8;
        while nbits >= 6 {
            nbits -= 6;
            res.push(BASE64_CHARS[(data >> nbits & 0x3f) as usize] as char);
        }
    }

    if nbits > 0 {
        res.push(BASE64_CHARS[(data << (6 - nbits) & 0x3f) as usize] as char);
    }
    while res.len() % 4 != 0 {
        res.push('=');
    }

    res
}

/// Port of `base64::decode`: stops at the first `=`, skips newlines,
/// errors on other invalid characters.
pub fn base64_decode(s: &str) -> Result<Vec<u8>, BadHash> {
    const NPOS: u8 = 0xff;
    const DECODE: [u8; 256] = {
        let mut result = [NPOS; 256];
        let mut i = 0;
        while i < 64 {
            result[BASE64_CHARS[i] as usize] = i as u8;
            i += 1;
        }
        result
    };

    let mut res = Vec::with_capacity((s.len() + 2) / 4 * 3);
    let mut d: u32 = 0;
    let mut bits: u32 = 0;

    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        if c == b'\n' {
            continue;
        }
        let digit = DECODE[c as usize];
        if digit == NPOS {
            return Err(bad_hash!(
                "invalid character in Base64 string: '{}'",
                c as char
            ));
        }
        bits += 6;
        d = d << 6 | digit as u32;
        if bits >= 8 {
            res.push((d >> (bits - 8) & 0xff) as u8);
            bits -= 8;
        }
    }

    Ok(res)
}
