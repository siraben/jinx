//! SIMD-accelerated escape-byte scanning for string printers.
//!
//! Both the derivation ATerm printer (`derivation::print_string`) and the JSON
//! string printer (`jinx-eval`'s `json_escape`) share the same shape: walk a
//! byte string, bulk-copy clean spans, and emit an escape sequence at each byte
//! that needs escaping. The hot part is *finding* the next byte-needing-escape;
//! the copies are handled by `Vec::extend_from_slice`.
//!
//! This module provides two classifiers that return the offset of the next
//! byte-needing-escape (or the slice length if there is none), using a 16-byte
//! NEON path on `aarch64`, a 16-byte SSE2 path on `x86_64`, and a scalar
//! fallback everywhere else. SSE2 is baseline on `x86_64` and NEON is baseline
//! on `aarch64`, so plain `#[cfg(target_arch)]` on stable `core::arch`
//! intrinsics is sufficient — no runtime feature detection.
//!
//! The escape sets differ between the two call sites, so there are two
//! instantiations:
//!   * derivation ATerm: `"` `\` `\n` `\r` `\t` = `{0x22, 0x5C, 0x0A, 0x0D, 0x09}`
//!   * JSON: any control char `< 0x20`, plus `"` (`0x22`) and `\` (`0x5C`)
//!
//! Both are exercised against a scalar reference in the unit tests below; the
//! public output of the printers must be byte-for-byte identical to the old
//! scalar scan.

/// Scalar predicate: does this byte need escaping in a derivation ATerm string?
#[inline(always)]
pub fn is_drv_escape(c: u8) -> bool {
    matches!(c, b'"' | b'\\' | b'\n' | b'\r' | b'\t')
}

/// Scalar predicate: does this byte need escaping in a JSON string?
#[inline(always)]
pub fn is_json_escape(c: u8) -> bool {
    c < 0x20 || c == b'"' || c == b'\\'
}

// ---------------------------------------------------------------------------
// aarch64 / NEON
// ---------------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
mod arch {
    use core::arch::aarch64::*;

    /// Reduce a NEON byte comparison mask (each lane 0x00 or 0xFF) to the index
    /// of the first set lane, or 16 if none are set.
    ///
    /// NEON has no `movemask`; the standard reduction narrows each 16-bit lane
    /// pair down to 4 bits via `vshrn_n_u16`, producing a 64-bit value with 4
    /// bits per input byte. The first set byte is `trailing_zeros() / 4`.
    ///
    /// SAFETY: `cmp` is a valid vector; the intrinsics used are baseline NEON,
    /// guaranteed present on aarch64.
    #[inline(always)]
    unsafe fn first_set(cmp: uint8x16_t) -> usize {
        let paired = vreinterpretq_u16_u8(cmp);
        let narrowed = vshrn_n_u16(paired, 4); // uint8x8_t, 4 bits per source byte
        let mask = vget_lane_u64(vreinterpret_u64_u8(narrowed), 0);
        if mask == 0 {
            16
        } else {
            (mask.trailing_zeros() >> 2) as usize
        }
    }

    /// Classify 16 bytes at `p` for the derivation escape set, returning the
    /// offset (0..=16) of the first byte needing escape.
    ///
    /// SAFETY: caller guarantees 16 readable bytes at `p`. NEON is baseline on
    /// aarch64, so the intrinsics are always available.
    #[inline(always)]
    pub unsafe fn drv_block(p: *const u8) -> usize {
        let v = vld1q_u8(p);
        let mut cmp = vceqq_u8(v, vdupq_n_u8(b'"'));
        cmp = vorrq_u8(cmp, vceqq_u8(v, vdupq_n_u8(b'\\')));
        cmp = vorrq_u8(cmp, vceqq_u8(v, vdupq_n_u8(b'\n')));
        cmp = vorrq_u8(cmp, vceqq_u8(v, vdupq_n_u8(b'\r')));
        cmp = vorrq_u8(cmp, vceqq_u8(v, vdupq_n_u8(b'\t')));
        first_set(cmp)
    }

    /// Classify 16 bytes at `p` for the JSON escape set, returning the offset
    /// (0..=16) of the first byte needing escape.
    ///
    /// SAFETY: caller guarantees 16 readable bytes at `p`. NEON is baseline on
    /// aarch64, so the intrinsics are always available.
    #[inline(always)]
    pub unsafe fn json_block(p: *const u8) -> usize {
        let v = vld1q_u8(p);
        // c < 0x20  <=>  c <= 0x1F  (unsigned compare, direct in NEON).
        let mut cmp = vcleq_u8(v, vdupq_n_u8(0x1F));
        cmp = vorrq_u8(cmp, vceqq_u8(v, vdupq_n_u8(b'"')));
        cmp = vorrq_u8(cmp, vceqq_u8(v, vdupq_n_u8(b'\\')));
        first_set(cmp)
    }
}

// ---------------------------------------------------------------------------
// x86_64 / SSE2
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
mod arch {
    use core::arch::x86_64::*;

    /// Classify 16 bytes at `p` for the derivation escape set, returning the
    /// offset (0..=16) of the first byte needing escape.
    ///
    /// SAFETY: caller guarantees 16 readable bytes at `p`. SSE2 is baseline on
    /// x86_64, so the intrinsics are always available.
    #[inline(always)]
    pub unsafe fn drv_block(p: *const u8) -> usize {
        let v = _mm_loadu_si128(p as *const __m128i);
        let mut cmp = _mm_cmpeq_epi8(v, _mm_set1_epi8(b'"' as i8));
        cmp = _mm_or_si128(cmp, _mm_cmpeq_epi8(v, _mm_set1_epi8(b'\\' as i8)));
        cmp = _mm_or_si128(cmp, _mm_cmpeq_epi8(v, _mm_set1_epi8(b'\n' as i8)));
        cmp = _mm_or_si128(cmp, _mm_cmpeq_epi8(v, _mm_set1_epi8(b'\r' as i8)));
        cmp = _mm_or_si128(cmp, _mm_cmpeq_epi8(v, _mm_set1_epi8(b'\t' as i8)));
        let m = (_mm_movemask_epi8(cmp) as u32) & 0xFFFF;
        if m == 0 {
            16
        } else {
            m.trailing_zeros() as usize
        }
    }

    /// Classify 16 bytes at `p` for the JSON escape set, returning the offset
    /// (0..=16) of the first byte needing escape.
    ///
    /// SAFETY: caller guarantees 16 readable bytes at `p`. SSE2 is baseline on
    /// x86_64, so the intrinsics are always available.
    #[inline(always)]
    pub unsafe fn json_block(p: *const u8) -> usize {
        let v = _mm_loadu_si128(p as *const __m128i);
        // c < 0x20  <=>  c <= 0x1F. SSE2 lacks unsigned byte compare, so use
        // min_epu8: min(c, 0x1F) == c  iff  c <= 0x1F.
        let lo = _mm_set1_epi8(0x1F);
        let mut cmp = _mm_cmpeq_epi8(_mm_min_epu8(v, lo), v);
        cmp = _mm_or_si128(cmp, _mm_cmpeq_epi8(v, _mm_set1_epi8(b'"' as i8)));
        cmp = _mm_or_si128(cmp, _mm_cmpeq_epi8(v, _mm_set1_epi8(b'\\' as i8)));
        let m = (_mm_movemask_epi8(cmp) as u32) & 0xFFFF;
        if m == 0 {
            16
        } else {
            m.trailing_zeros() as usize
        }
    }
}

// ---------------------------------------------------------------------------
// Shared chunked drivers
// ---------------------------------------------------------------------------

/// Return the offset of the next byte needing a derivation ATerm escape at or
/// after the start of `s`, or `s.len()` if there is none.
#[inline]
pub fn next_drv_escape(s: &[u8]) -> usize {
    let mut i = 0;
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        while i + 16 <= s.len() {
            // SAFETY: `i + 16 <= s.len()`, so 16 bytes are readable at this
            // offset; the SIMD path is baseline for the target arch (cfg).
            let off = unsafe { arch::drv_block(s.as_ptr().add(i)) };
            if off < 16 {
                return i + off;
            }
            i += 16;
        }
    }
    while i < s.len() {
        if is_drv_escape(s[i]) {
            return i;
        }
        i += 1;
    }
    s.len()
}

/// Return the offset of the next byte needing a JSON escape at or after the
/// start of `s`, or `s.len()` if there is none.
#[inline]
pub fn next_json_escape(s: &[u8]) -> usize {
    let mut i = 0;
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        while i + 16 <= s.len() {
            // SAFETY: `i + 16 <= s.len()`, so 16 bytes are readable at this
            // offset; the SIMD path is baseline for the target arch (cfg).
            let off = unsafe { arch::json_block(s.as_ptr().add(i)) };
            if off < 16 {
                return i + off;
            }
            i += 16;
        }
    }
    while i < s.len() {
        if is_json_escape(s[i]) {
            return i;
        }
        i += 1;
    }
    s.len()
}

// ---------------------------------------------------------------------------
// Tests: SIMD scan == scalar reference, byte-for-byte.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure scalar reference: index of the first byte for which `pred` holds,
    /// or `s.len()`. Independent of the (possibly SIMD) driver under test.
    fn scalar_ref(s: &[u8], pred: impl Fn(u8) -> bool) -> usize {
        s.iter().position(|&c| pred(c)).unwrap_or(s.len())
    }

    /// Tiny deterministic PRNG (xorshift64*), so the randomized test needs no
    /// external crate and is reproducible.
    struct Rng(u64);
    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn byte(&mut self) -> u8 {
            (self.next_u64() >> 33) as u8
        }
    }

    /// Bytes that appear in the escape sets, plus a few boundary bytes, so
    /// random inputs are dense in escape-relevant values.
    const INTERESTING: &[u8] = &[
        b'"', b'\\', b'\n', b'\r', b'\t', 0x00, 0x01, 0x08, 0x0b, 0x0c, 0x1f, 0x20, 0x21, 0x5b,
        0x5d, 0x7f, 0x80, 0xff, b'a', b'/',
    ];

    fn check(s: &[u8]) {
        assert_eq!(
            next_drv_escape(s),
            scalar_ref(s, is_drv_escape),
            "drv mismatch for {s:?}"
        );
        assert_eq!(
            next_json_escape(s),
            scalar_ref(s, is_json_escape),
            "json mismatch for {s:?}"
        );
        // Also verify scanning from every possible restart offset (mirrors how
        // the printers re-scan after each escape), catching alignment bugs.
        for start in 0..=s.len() {
            let sub = &s[start..];
            assert_eq!(next_drv_escape(sub), scalar_ref(sub, is_drv_escape));
            assert_eq!(next_json_escape(sub), scalar_ref(sub, is_json_escape));
        }
    }

    #[test]
    fn empty_and_short() {
        check(b"");
        for &b in INTERESTING {
            check(&[b]);
            check(&[b, b'a']);
            check(&[b'a', b]);
        }
    }

    #[test]
    fn boundaries_15_16_17() {
        // Escape byte placed at every position within lengths spanning the
        // 16-byte SIMD chunk boundary.
        for len in [1usize, 14, 15, 16, 17, 18, 31, 32, 33, 47, 48, 49] {
            for pos in 0..len {
                for &esc in &[b'"', b'\\', b'\n', b'\r', b'\t', 0x00u8, 0x1f] {
                    let mut buf = vec![b'x'; len];
                    buf[pos] = esc;
                    check(&buf);
                }
            }
        }
    }

    #[test]
    fn all_escape_inputs() {
        for len in [1usize, 15, 16, 17, 33] {
            check(&vec![b'"'; len]);
            check(&vec![b'\n'; len]);
            check(&vec![0x00u8; len]);
            check(&vec![0x1fu8; len]);
        }
    }

    #[test]
    fn no_escape_inputs() {
        for len in [15usize, 16, 17, 32, 64, 100] {
            check(&vec![b'a'; len]);
            check(&vec![0x20u8; len]); // space: not escaped by either set
            check(&vec![0xffu8; len]); // high byte: not escaped by either set
        }
    }

    #[test]
    fn randomized_equivalence() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for _ in 0..20_000 {
            let len = (rng.next_u64() % 80) as usize;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                // Bias heavily toward interesting bytes.
                let b = if rng.next_u64() & 1 == 0 {
                    INTERESTING[(rng.next_u64() as usize) % INTERESTING.len()]
                } else {
                    rng.byte()
                };
                buf.push(b);
            }
            check(&buf);
        }
    }
}
