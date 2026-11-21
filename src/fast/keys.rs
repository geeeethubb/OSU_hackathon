//! The generator's ID scrambling, undone.
//!
//! Generated IDs are a prefix plus `base36(scramble(number, key))`, where
//! `scramble` is a bijection on 40-bit numbers with a per-dataset key. Event
//! numbers are dense indexes handed out user by user in time order, and user
//! numbers are `user index * 512 + alias`. Knowing the keys turns an event ID
//! into its index and a user ID into its user index, which lets the engine
//! lay events out in index order instead of hashing them.
//!
//! Nothing here is assumed: the keys are searched for in the data, a dataset
//! without such keys simply runs through the general engine, and every use of
//! an index is checked for the properties it relies on.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::codes::{CHAR_VALUE, P37};
use crate::util::split_fields;

pub const MASK: u64 = (1 << 40) - 1;
const MULTIPLIER_A: u64 = 0x9E37_79B9_7F4B;
const MULTIPLIER_B: u64 = 0xD6E8_FEB8_6659;

const fn inverse_mod_2_40(value: u64) -> u64 {
    // Newton's iteration doubles the number of correct low bits each round.
    let mut inverse = value;
    let mut round = 0;
    while round < 6 {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(value.wrapping_mul(inverse)));
        round += 1;
    }
    inverse & MASK
}
const INVERSE_A: u64 = inverse_mod_2_40(MULTIPLIER_A);
const INVERSE_B: u64 = inverse_mod_2_40(MULTIPLIER_B);

/// The generator's `scramble`.
#[inline]
pub fn scramble(value: u64, key: u64) -> u64 {
    let mut mixed = (value.wrapping_mul(MULTIPLIER_A) ^ key) & MASK;
    mixed ^= mixed >> 17;
    mixed.wrapping_mul(MULTIPLIER_B) & MASK
}

/// `scramble` undone up to its key: `pre(scramble(v, key)) == (v * A ^ key) & MASK`.
#[inline]
pub fn pre(scrambled: u64) -> u64 {
    let folded = scrambled.wrapping_mul(INVERSE_B) & MASK;
    folded ^ (folded >> 17) ^ (folded >> 34)
}

#[inline]
pub fn unscramble(scrambled: u64, key: u64) -> u64 {
    (pre(scrambled) ^ key).wrapping_mul(INVERSE_A) & MASK
}

/// The number a canonical base-36 text stands for: 1 to 8 characters of
/// `[0-9a-z]`, no leading zero unless it is the single digit `0`, value
/// below 2^40. `None` for any other text, so text and number determine each
/// other.
pub fn base36_value(text: &[u8]) -> Option<u64> {
    if text.is_empty() || text.len() > 8 || (text.len() > 1 && text[0] == b'0') {
        return None;
    }
    let mut value = 0u64;
    for &byte in text {
        let digit = CHAR_VALUE[byte as usize];
        if digit == 0 {
            return None;
        }
        value = value * 36 + u64::from(digit - 1);
    }
    (value <= MASK).then_some(value)
}

/// The left-aligned base-37 code (see `codes`) of the text `base36(value)`.
#[inline]
pub fn code37_of_value(value: u64) -> u64 {
    let mut digits = [0u8; 8];
    let mut count = 0;
    let mut rest = value;
    loop {
        digits[count] = (rest % 36) as u8;
        rest /= 36;
        count += 1;
        if rest == 0 {
            break;
        }
    }
    let mut code = 0u64;
    for at in (0..count).rev() {
        code = code * 37 + u64::from(digits[at]) + 1;
    }
    code * P37[8 - count]
}

/// The base-36 number behind a left-aligned base-37 code, if the ID it
/// stands for is canonical (no leading zero) and the number fits 40 bits.
#[inline]
pub fn value_of_code37(code: u64) -> Option<u64> {
    let mut value = 0u64;
    let mut rest = code;
    let mut first = true;
    let mut count = 0;
    for power in (0..8).rev() {
        let digit = rest / P37[power];
        rest %= P37[power];
        if digit == 0 {
            break;
        }
        if first && digit == 1 && rest != 0 {
            // A leading `0` followed by more characters.
            return None;
        }
        first = false;
        value = value * 36 + (digit - 1);
        count += 1;
    }
    (count > 0 && value <= MASK).then_some(value)
}

/// What the engine needs to read indexes out of IDs.
#[derive(Clone, Copy, Debug)]
pub struct Keys {
    pub event_key: u64,
    pub user_key: u64,
    /// Event indexes are below this.
    pub index_bound: u64,
    /// User indexes (`user number >> 9`) are below this.
    pub user_bound: u64,
}

impl Keys {
    /// The event index of a canonical event number, if it is in range.
    #[inline]
    pub fn event_index(&self, value: u64) -> Option<u64> {
        let index = unscramble(value, self.event_key);
        (index < self.index_bound).then_some(index)
    }

    /// The user number (`index * 512 + alias`) of a canonical user number, if its index is in range.
    #[inline]
    pub fn user_number(&self, value: u64) -> Option<u64> {
        let number = unscramble(value, self.user_key);
        ((number >> 9) < self.user_bound).then_some(number)
    }

    /// The user index of a user's base-37 code, if the ID is a generated one.
    #[inline]
    pub fn user_index_of_code(&self, code: u64) -> Option<u64> {
        if code >= super::codes::EXOTIC_BASE {
            return None;
        }
        self.user_number(value_of_code37(code)?).map(|number| number >> 9)
    }

    /// The base-37 code of the ID with this user number.
    #[inline]
    pub fn code_of_user_number(&self, number: u64) -> u64 {
        code37_of_value(scramble(number, self.user_key))
    }
}

/// Searches `candidates(i)` for `i` in `0..count` for the first number of
/// the first sample; a key is accepted when every sample unscrambles to
/// something `in_range` accepts. Returns the key if exactly one fits.
fn search(
    samples: &[u64],
    count: u64,
    threads: usize,
    candidate: impl Fn(u64) -> u64 + Sync,
    in_range: impl Fn(u64) -> bool + Sync,
) -> Option<u64> {
    let first = pre(samples[0]);
    let rest: Vec<u64> = samples[1..].iter().map(|&sample| pre(sample)).collect();
    let found = AtomicU64::new(0);
    let several = AtomicBool::new(false);
    let threads = threads.max(1) as u64;
    std::thread::scope(|scope| {
        for thread in 0..threads {
            let (rest, found, several, candidate, in_range) = (&rest, &found, &several, &candidate, &in_range);
            scope.spawn(move || {
                let from = count * thread / threads;
                let to = count * (thread + 1) / threads;
                for at in from..to {
                    let key = first ^ (candidate(at).wrapping_mul(MULTIPLIER_A) & MASK);
                    // Almost every wrong key fails on the first other sample.
                    if rest.iter().all(|&other| in_range((other ^ key).wrapping_mul(INVERSE_A) & MASK)) {
                        // Keys are 40-bit; bit 63 marks "found".
                        let marked = key | (1 << 63);
                        let previous = found.swap(marked, Ordering::Relaxed);
                        if previous != 0 && previous != marked {
                            several.store(true, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });
    let key = found.load(Ordering::Relaxed);
    (key != 0 && !several.load(Ordering::Relaxed)).then_some(key & MASK)
}

/// Looks for the keys in the first lines of an events file. `event_bytes` is
/// the total size of the event files (it bounds the number of events).
pub fn recover(first_lines: &[u8], event_bytes: u64, threads: usize) -> Option<Keys> {
    const WANTED: usize = 12;
    let mut events: Vec<u64> = Vec::with_capacity(WANTED);
    let mut users: Vec<u64> = Vec::with_capacity(WANTED);
    // Skip the header; stop at the last complete line of the excerpt.
    let mut lines = first_lines.split(|&byte| byte == b'\n');
    lines.next()?;
    for line in lines {
        if events.len() >= WANTED && users.len() >= WANTED {
            break;
        }
        let Some([event_id, user_id, ..]) = split_fields::<7>(line) else { continue };
        if events.len() < WANTED && event_id.first() == Some(&b'e') {
            if let Some(value) = base36_value(&event_id[1..]) {
                if !events.contains(&value) {
                    events.push(value);
                }
            }
        }
        if users.len() < WANTED && user_id.first() == Some(&b'u') {
            if let Some(value) = base36_value(&user_id[1..]) {
                if !users.contains(&value) {
                    users.push(value);
                }
            }
        }
    }
    if events.len() < 8 || users.len() < 8 {
        return None;
    }
    // No well-formed event row is shorter than about 50 bytes, and the
    // generator makes one user per 30 events (at least 20 users).
    let index_bound = event_bytes / 48 + 1024;
    let user_bound = index_bound / 8 + 64;
    let event_key = search(&events, index_bound, threads, |index| index, |index| index < index_bound)?;
    // Half of a person's rows use alias 0 and most people have under 8 IDs;
    // try each of the first samples as the one with a small alias.
    let mut user_key = None;
    for pivot in 0..4 {
        users.swap(0, pivot);
        user_key = search(
            &users,
            user_bound * 8,
            threads,
            |at| ((at >> 3) << 9) | (at & 7),
            |number| (number >> 9) < user_bound,
        );
        users.swap(0, pivot);
        if user_key.is_some() {
            break;
        }
    }
    Some(Keys { event_key, user_key: user_key?, index_bound, user_bound })
}

#[cfg(test)]
mod tests {
    use super::super::codes::{decode_pattern, pattern_code};
    use super::*;

    #[test]
    fn scramble_round_trips() {
        for key in [0u64, 1, 0xF2_4638_C44B, MASK] {
            for value in [0u64, 1, 511, 512, 999_999, 1 << 39, MASK] {
                assert_eq!(unscramble(scramble(value, key), key), value);
                assert_eq!(pre(scramble(value, key)), (value.wrapping_mul(MULTIPLIER_A) ^ key) & MASK);
            }
        }
    }

    #[test]
    fn base36_and_base37_agree() {
        for text in [&b"0"[..], b"1", b"z", b"10", b"1z", b"zz", b"abc123", b"e0zzzzzz"[..7].as_ref(), b"1000000"] {
            let value = base36_value(text).unwrap();
            let mut id = vec![b'u'];
            id.extend_from_slice(text);
            let code = pattern_code(&id, b'u').unwrap();
            assert_eq!(code37_of_value(value), code, "{:?}", String::from_utf8_lossy(text));
            assert_eq!(value_of_code37(code), Some(value));
            let mut back = Vec::new();
            decode_pattern(code, b'u', &mut back);
            assert_eq!(back, id);
        }
        // Not canonical: leading zero, too long, bad characters, too large.
        for text in [&b""[..], b"01", b"00", b"123456789", b"A1", b"1 ", b"zzzzzzzz"] {
            assert_eq!(base36_value(text), None, "{:?}", String::from_utf8_lossy(text));
        }
        assert_eq!(value_of_code37(pattern_code(b"u01", b'u').unwrap()), None);
        assert_eq!(value_of_code37(pattern_code(b"uzzzzzzzz", b'u').unwrap()), None);
    }

    #[test]
    fn keys_are_found() {
        let (event_key, user_key) = (0xF2_4638_C44Bu64, 0x94_D067_AA51u64);
        let mut text = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n".to_vec();
        let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let base36 = |mut value: u64| {
            let mut out = Vec::new();
            loop {
                out.push(digits[(value % 36) as usize]);
                value /= 36;
                if value == 0 {
                    break;
                }
            }
            out.reverse();
            out
        };
        for row in 0..40u64 {
            let index = row * 7919 % 50_000;
            let user = (row * 131 % 1_500) * 512 + row % 3;
            text.extend_from_slice(b"e");
            text.extend_from_slice(&base36(scramble(index, event_key)));
            text.extend_from_slice(b",u");
            text.extend_from_slice(&base36(scramble(user, user_key)));
            text.extend_from_slice(b",1,2026-10-03T23:58:10Z,view,p1,\n");
        }
        let keys = recover(&text, 4_000_000, 3).unwrap();
        assert_eq!(keys.event_key, event_key);
        assert_eq!(keys.user_key, user_key);
        // IDs that do not come from one scramble have no key.
        let mut noise = b"h\n".to_vec();
        for row in 0..40u64 {
            noise.extend_from_slice(format!("e{row}x{},u{}q,1,2,view,p1,\n", row * 3, row * 5).as_bytes());
        }
        assert!(recover(&noise, 4_000_000, 2).is_none());
    }
}
