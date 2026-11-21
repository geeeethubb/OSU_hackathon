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
    // The eight base-37 digits, most significant first. Every divisor is a
    // constant, so no division instruction is involved.
    let (high, low) = (code / 1_874_161, code % 1_874_161);
    let digits = [
        high / 50_653,
        (high / 1_369) % 37,
        (high / 37) % 37,
        high % 37,
        low / 50_653,
        (low / 1_369) % 37,
        (low / 37) % 37,
        low % 37,
    ];
    let mut value = 0u64;
    let mut count = 0;
    for digit in digits {
        // Digit 0 is "no character": the text ends there.
        if digit == 0 {
            break;
        }
        value = value * 36 + (digit - 1);
        count += 1;
    }
    // Not canonical: no characters, or a leading `0` followed by more.
    if count == 0 || (digits[0] == 1 && count > 1) {
        return None;
    }
    (value <= MASK).then_some(value)
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

    /// The user number (`index * 512 + alias`) behind a user's base-37 code,
    /// if the ID is a generated one.
    #[inline]
    pub fn user_number_of_code(&self, code: u64) -> Option<u64> {
        if code >= super::codes::EXOTIC_BASE {
            return None;
        }
        self.user_number(value_of_code37(code)?)
    }

    /// The user index of a user's base-37 code, if the ID is a generated one.
    #[inline]
    pub fn user_index_of_code(&self, code: u64) -> Option<u64> {
        self.user_number_of_code(code).map(|number| number >> 9)
    }

    /// The base-37 code of the ID with this user number.
    #[inline]
    pub fn code_of_user_number(&self, number: u64) -> u64 {
        code37_of_value(scramble(number, self.user_key))
    }
}

/// The first `number` in `0..bound`, counting up in steps of one with
/// `number * multiplier` as the anchor's pre-image, whose key makes every
/// sample fit. `anchor` and `samples` are `pre` values; `fits` judges an
/// unscrambled sample. Stops at the first key that fits: with a dozen
/// samples a wrong key fits with a probability far below 2^-100, and a wrong
/// key could only cost time, since every later use of an index is checked.
fn first_key(anchor: u64, samples: &[u64], bound: u64, multiplier: u64, fits: impl Fn(u64) -> bool) -> Option<u64> {
    let unscrambled = |sample: u64, key: u64| (sample ^ key).wrapping_mul(INVERSE_A) & MASK;
    // The sample tried first: almost every wrong key fails on it.
    let probe = *samples.iter().find(|&&sample| sample != anchor)?;
    let mut product = 0u64;
    for _ in 0..bound {
        let key = anchor ^ product;
        if fits(unscrambled(probe, key)) && samples.iter().all(|&sample| fits(unscrambled(sample, key))) {
            return Some(key);
        }
        product = product.wrapping_add(multiplier) & MASK;
    }
    None
}

/// Looks for the keys in the first lines of an events file. `event_bytes` is
/// the total size of the event files (it bounds the number of events).
///
/// The search is cheap because of how the numbers are laid out. Most rows
/// carry a user's first ID, whose number `index * 512` ends in nine zero
/// bits; the scramble's first step keeps those zeros, so the low nine bits
/// of such a row's pre-image are the user key's, and the row seen most often
/// gives them away. The rest of the user key is found by counting the
/// anchor's user index up from zero. Event indexes are handed out user by
/// user, so the sample with the smallest user index has a small event index:
/// counting up from zero reaches it quickly.
pub fn recover(first_lines: &[u8], event_bytes: u64) -> Option<Keys> {
    // (event pre-image, user pre-image) of every row whose IDs are canonical.
    let mut samples: Vec<(u64, u64)> = Vec::new();
    // Skip the header; the last line of the excerpt may be cut short, and a
    // cut ID could still look canonical: leave it out.
    let mut lines = first_lines.split(|&byte| byte == b'\n');
    lines.next()?;
    let mut lines = lines.peekable();
    while let Some(line) = lines.next() {
        if lines.peek().is_none() {
            break;
        }
        let Some([event_id, user_id, ..]) = split_fields::<7>(line) else { continue };
        if event_id.first() != Some(&b'e') || user_id.first() != Some(&b'u') {
            continue;
        }
        if let (Some(event), Some(user)) = (base36_value(&event_id[1..]), base36_value(&user_id[1..])) {
            samples.push((pre(event), pre(user)));
        }
    }
    if samples.len() < 16 {
        return None;
    }
    // No well-formed event row is shorter than about 50 bytes, and the
    // generator makes one user per 30 events (at least 20 users).
    let index_bound = event_bytes / 48 + 1024;
    let user_bound = index_bound / 8 + 64;

    // The user key's low nine bits: the most frequent low bits of a pre-image.
    let mut counts = [0u32; 512];
    for &(_, user) in &samples {
        counts[(user & 511) as usize] += 1;
    }
    let low = (0..512u64).max_by_key(|&bits| counts[bits as usize])?;
    if (counts[low as usize] as usize) * 4 < samples.len() {
        return None;
    }
    let users: Vec<u64> = samples.iter().map(|&(_, user)| user).collect();
    let anchor = *users.iter().find(|&&user| user & 511 == low)?;
    let user_fits = |number: u64| (number >> 9) < user_bound;
    let user_key = first_key(anchor, &users, user_bound, 512u64.wrapping_mul(MULTIPLIER_A) & MASK, user_fits)?;

    // The event of the smallest user index is the anchor of the event key.
    let user_index = |user: u64| ((user ^ user_key).wrapping_mul(INVERSE_A) & MASK) >> 9;
    let &(anchor, _) = samples.iter().min_by_key(|&&(_, user)| user_index(user))?;
    let events: Vec<u64> = samples.iter().map(|&(event, _)| event).collect();
    let event_key = first_key(anchor, &events, index_bound, MULTIPLIER_A & MASK, |index| index < index_bound)?;

    // Index mode needs more than the keys: event indexes must be handed out
    // user by user, so along the index order the user index never decreases.
    // The engine checks that on every surviving event later and starts over
    // if it fails; looking at the samples first means data that is laid out
    // differently never gets that far. A few samples may break the order
    // without harm (a late copy of an event can name another user and then
    // loses the dedup), data without the layout breaks it at every other
    // step: one in fifty is the line between the two.
    let mut layout: Vec<(u64, u64)> = samples
        .iter()
        .map(|&(event, user)| ((event ^ event_key).wrapping_mul(INVERSE_A) & MASK, user_index(user)))
        .collect();
    layout.sort_unstable();
    let backward = layout.windows(2).filter(|pair| pair[1].1 < pair[0].1).count();
    if backward * 50 > layout.len() {
        return None;
    }
    Some(Keys { event_key, user_key, index_bound, user_bound })
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
            let user_index = row * 131 % 1_500;
            let index = user_index * 30 + row % 7;
            let user = user_index * 512 + if row % 3 == 0 { row % 5 } else { 0 };
            text.extend_from_slice(b"e");
            text.extend_from_slice(&base36(scramble(index, event_key)));
            text.extend_from_slice(b",u");
            text.extend_from_slice(&base36(scramble(user, user_key)));
            text.extend_from_slice(b",1,2026-10-03T23:58:10Z,view,p1,\n");
        }
        let keys = recover(&text, 4_000_000).unwrap();
        assert_eq!(keys.event_key, event_key);
        assert_eq!(keys.user_key, user_key);
        // The same IDs with event indexes that do not follow the users: no index mode.
        let mut shuffled = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents
".to_vec();
        for row in 0..40u64 {
            let user_index = row * 131 % 1_500;
            let index = row * 7919 % 50_000;
            shuffled.extend_from_slice(b"e");
            shuffled.extend_from_slice(&base36(scramble(index, event_key)));
            shuffled.extend_from_slice(b",u");
            shuffled.extend_from_slice(&base36(scramble(user_index * 512, user_key)));
            shuffled.extend_from_slice(b",1,2026-10-03T23:58:10Z,view,p1,
");
        }
        assert!(recover(&shuffled, 4_000_000).is_none());
        // IDs that do not come from one scramble have no key.
        let mut noise = b"h\n".to_vec();
        for row in 0..40u64 {
            noise.extend_from_slice(format!("e{row}x{},u{}q,1,2,view,p1,\n", row * 3, row * 5).as_bytes());
        }
        assert!(recover(&noise, 4_000_000).is_none());
    }
}
