//! Compact, exact integer codes for IDs.
//!
//! An ID shaped like the generator's (`prefix` + 1 to 8 characters of
//! `[0-9a-z]`) maps to a number below 37^8 by reading it as a left-aligned
//! base-37 string, so numeric order equals byte-wise order. Every other ID
//! (upper case, spaces, long, non-ASCII) is "exotic" and gets `37^8 + index`
//! from an interner. Both together fit 42 bits and are exact: equal codes
//! mean equal IDs.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

pub const P37: [u64; 9] =
    [1, 37, 1_369, 50_653, 1_874_161, 69_343_957, 2_565_726_409, 94_931_877_133, 3_512_479_453_921];
pub const EXOTIC_BASE: u64 = P37[8];
pub const CODE_BITS: u32 = 42;
pub const CODE_MASK: u64 = (1 << CODE_BITS) - 1;

const fn char_values() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut at = 0;
    while at < 10 {
        table[b'0' as usize + at] = 1 + at as u8;
        at += 1;
    }
    at = 0;
    while at < 26 {
        table[b'a' as usize + at] = 11 + at as u8;
        at += 1;
    }
    table
}
pub static CHAR_VALUE: [u8; 256] = char_values();

/// The code of a generator-style ID, or `None` if it is exotic.
#[inline]
pub fn pattern_code(id: &[u8], prefix: u8) -> Option<u64> {
    let length = id.len();
    if !(2..=9).contains(&length) || id[0] != prefix {
        return None;
    }
    let mut code = 0u64;
    for &byte in &id[1..] {
        let value = CHAR_VALUE[byte as usize];
        if value == 0 {
            return None;
        }
        code = code * 37 + u64::from(value);
    }
    Some(code * P37[9 - length])
}

/// Appends the ID a pattern code stands for.
pub fn decode_pattern(code: u64, prefix: u8, out: &mut Vec<u8>) {
    debug_assert!(code < EXOTIC_BASE);
    out.push(prefix);
    let mut rest = code;
    for power in (0..8).rev() {
        let digit = (rest / P37[power]) as u8;
        rest %= P37[power];
        if digit == 0 {
            break;
        }
        out.push(if digit <= 10 { b'0' + digit - 1 } else { b'a' + digit - 11 });
    }
}

#[derive(Default)]
struct InternerInner {
    index: HashMap<Box<[u8]>, u32>,
    list: Vec<Box<[u8]>>,
}

/// Exotic IDs of one kind (events, users or clicks). Rare in generated data.
#[derive(Default)]
pub struct Interner {
    inner: Mutex<InternerInner>,
}

impl Interner {
    #[cold]
    #[inline(never)]
    pub fn code(&self, id: &[u8]) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        if let Some(&at) = inner.index.get(id) {
            return EXOTIC_BASE + u64::from(at);
        }
        let at = inner.list.len() as u32;
        inner.index.insert(id.into(), at);
        inner.list.push(id.into());
        EXOTIC_BASE + u64::from(at)
    }

    /// The exotic IDs interned so far, by index.
    pub fn snapshot(&self) -> Vec<Box<[u8]>> {
        self.inner.lock().unwrap().list.clone()
    }
}

/// The code of a non-empty ID.
#[inline]
pub fn id_code(id: &[u8], prefix: u8, interner: &Interner) -> u64 {
    match pattern_code(id, prefix) {
        Some(code) => code,
        None => interner.code(id),
    }
}

/// Appends the bytes of the ID behind `code`.
pub fn id_bytes(code: u64, prefix: u8, exotic: &[Box<[u8]>], out: &mut Vec<u8>) {
    if code < EXOTIC_BASE {
        decode_pattern(code, prefix, out);
    } else {
        out.extend_from_slice(&exotic[(code - EXOTIC_BASE) as usize]);
    }
}

/// Byte-wise order of the IDs behind two codes of the same kind.
#[inline]
pub fn id_cmp(a: u64, b: u64, prefix: u8, exotic: &[Box<[u8]>]) -> Ordering {
    if a < EXOTIC_BASE && b < EXOTIC_BASE {
        return a.cmp(&b);
    }
    id_cmp_slow(a, b, prefix, exotic)
}

#[cold]
#[inline(never)]
fn id_cmp_slow(a: u64, b: u64, prefix: u8, exotic: &[Box<[u8]>]) -> Ordering {
    let mut left = Vec::with_capacity(16);
    let mut right = Vec::with_capacity(16);
    id_bytes(a, prefix, exotic, &mut left);
    id_bytes(b, prefix, exotic, &mut right);
    left.cmp(&right)
}

// A bijection on 42-bit values whose high bits depend on every input bit:
// multiply, fold the top half down, multiply. It cuts a code into a
// (partition, local key) pair that can be put back together exactly.
const MIX_A: u64 = (0x2D3_5A5D_9C4B & CODE_MASK) | 1;
const MIX_B: u64 = (0x1B7_3E2F_A6D5 & CODE_MASK) | 1;

const fn inverse_mod_2_42(value: u64) -> u64 {
    // Newton's iteration doubles the number of correct low bits each round.
    let mut inverse = value;
    let mut round = 0;
    while round < 6 {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(value.wrapping_mul(inverse)));
        round += 1;
    }
    inverse & CODE_MASK
}
const MIX_A_INVERSE: u64 = inverse_mod_2_42(MIX_A);
const MIX_B_INVERSE: u64 = inverse_mod_2_42(MIX_B);

#[inline]
pub fn mix(code: u64) -> u64 {
    let mut mixed = code.wrapping_mul(MIX_A) & CODE_MASK;
    mixed ^= mixed >> 21;
    mixed.wrapping_mul(MIX_B) & CODE_MASK
}

#[inline]
pub fn unmix(mixed: u64) -> u64 {
    let mut code = mixed.wrapping_mul(MIX_B_INVERSE) & CODE_MASK;
    code ^= code >> 21;
    code.wrapping_mul(MIX_A_INVERSE) & CODE_MASK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_codes_round_trip_and_order() {
        let ids: [&[u8]; 8] = [b"u0", b"u00", b"u1", b"u10", b"u1z", b"u2", b"uzzzzzzzz", b"ua"];
        let mut codes: Vec<(u64, &[u8])> = ids.iter().map(|id| (pattern_code(id, b'u').unwrap(), *id)).collect();
        for (code, id) in &codes {
            let mut out = Vec::new();
            decode_pattern(*code, b'u', &mut out);
            assert_eq!(&out, id);
            assert!(*code < EXOTIC_BASE);
        }
        codes.sort();
        let mut sorted_ids: Vec<&[u8]> = ids.to_vec();
        sorted_ids.sort();
        assert_eq!(codes.iter().map(|(_, id)| *id).collect::<Vec<_>>(), sorted_ids);
        for exotic in [&b"u"[..], b"U1", b"u1A", b" u1", b"u123456789", b"e1", b"u1 "] {
            assert_eq!(pattern_code(exotic, b'u'), None);
        }
    }

    #[test]
    fn mix_is_a_bijection() {
        for code in [0u64, 1, 2, 37, EXOTIC_BASE - 1, EXOTIC_BASE, EXOTIC_BASE + 12345, CODE_MASK] {
            assert_eq!(unmix(mix(code)), code);
            assert!(mix(code) <= CODE_MASK);
        }
    }
}
