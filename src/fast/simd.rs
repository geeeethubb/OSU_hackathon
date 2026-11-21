//! AVX2 fast paths for the hot row parsers.
//!
//! Each fast path either produces exactly what the general parser in
//! `parse.rs` produces for the same line, or declines the line (a "slow"
//! status) so the general parser handles it. `SS_VERIFY=1` runs both on
//! every line and aborts on any difference.
//!
//! The crate is compiled for x86-64-v3 (AVX2, BMI1/2, LZCNT, POPCNT), so the
//! intrinsics here need no per-function feature attributes.

#![cfg(target_arch = "x86_64")]

use std::arch::x86_64::*;
use std::hint::select_unpredictable;

use super::codes::id_code;
use super::keys::{unscramble, MASK as NUMBER_MASK};
use super::parse::{emit_indexed, emit_raw, ClickRecord, EventRow, ParseOut};
use super::Ctx;
use crate::util::{civil_from_days, parse_price, parse_ts, DAY_MS, LATE_MS, WEEK_MS};

/// Bytes a fast row parser may read past the start of its row.
pub const ROW_PAD: usize = 192;

pub const ROW_DROP: u32 = 0;
/// `row` holds the parsed row; the caller stores it.
pub const ROW_EMIT: u32 = 1;
/// The fast path declines; the row's length is known.
pub const ROW_SLOW: u32 = 2;
/// The fast path declines and did not find the end of the row.
pub const ROW_LONG: u32 = 3;
/// The row was parsed and already stored.
pub const ROW_DONE: u32 = 4;
/// Index mode only: the event ID is not one of the generator's (or its
/// index is out of range), so the row belongs to the general path.
pub const ROW_GENERIC: u32 = 5;

/// What `event_row_fast` does with a parsed row.
pub const MODE_STORE: u32 = 0;
/// Hand the row back (`ROW_EMIT`) so the caller can check it first.
pub const MODE_ROW: u32 = 1;
// Diagnostic modes 2 to 7 stop a row early and store nothing: after the
// masks (2), the comma positions (3), the field bounds (4), the ID codes
// (5), the timestamps (6), or the whole parse (7).

const NOT_FAST: i64 = i64::MIN;

/// Lookup tables for ISO timestamps near the window.
pub struct TsTables {
    /// The four ASCII digits of the first table year and of the next one.
    year_text: [u32; 2],
    /// Epoch milliseconds of day offset 0.
    base_ms: i64,
    /// Added to seconds-since-base to get the compact ingest time.
    ingest_adjust: i64,
    /// `[year index << 12 | month << 5 | day]` -> day offset, or 0xFF for
    /// every date that is invalid or outside the table's range.
    date: [u8; 2 << 12],
}

impl TsTables {
    pub fn new(window_start: i64, ingest_base_seconds: i64) -> Self {
        // From 9 days before the window (click window plus a day of UTC
        // offset) for 62 days, which covers every ingest time the compact
        // records can hold.
        let base_day = window_start.div_euclid(DAY_MS) - 9;
        let (year0, _, _) = civil_from_days(base_day);
        let mut date = [0xFFu8; 2 << 12];
        for offset in 0..62 {
            let (year, month, day) = civil_from_days(base_day + offset);
            let year_index = (year - year0) as usize;
            // A year past 9999 has no four-digit text, so its days stay out.
            if year_index <= 1 && year <= 9999 {
                date[(year_index << 12) | ((month as usize) << 5) | day as usize] = offset as u8;
            }
        }
        let text = |year: i64| {
            let digits = [
                b'0' + (year / 1000 % 10) as u8,
                b'0' + (year / 100 % 10) as u8,
                b'0' + (year / 10 % 10) as u8,
                b'0' + (year % 10) as u8,
            ];
            u32::from_le_bytes(digits)
        };
        Self {
            year_text: [text(year0), text(year0 + 1)],
            base_ms: base_day * DAY_MS,
            ingest_adjust: base_day * 86_400 - ingest_base_seconds,
            date,
        }
    }
}

const Z: i8 = -128;

/// Bytes 3..19 of `YYYY-MM-DDTHH:MM:SS`, with `0` where a digit belongs, so
/// XOR leaves digit values and zeros.
#[inline(always)]
unsafe fn iso_pattern() -> __m128i {
    _mm_setr_epi8(
        b'0' as i8, b'-' as i8, b'0' as i8, b'0' as i8, b'-' as i8, b'0' as i8, b'0' as i8, b'T' as i8, b'0' as i8,
        b'0' as i8, b':' as i8, b'0' as i8, b'0' as i8, b':' as i8, b'0' as i8, b'0' as i8,
    )
}

/// Largest allowed value after the XOR: 9 for a digit, 0 for a separator.
#[inline(always)]
unsafe fn iso_limit() -> __m128i {
    _mm_setr_epi8(9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9)
}

/// Month, day, hour, minute, second digit pairs.
#[inline(always)]
unsafe fn iso_shuffle() -> __m128i {
    _mm_setr_epi8(2, 3, 5, 6, 8, 9, 11, 12, 14, 15, Z, Z, Z, Z, Z, Z)
}

/// Largest month, day, hour, minute and second. Month and day must stay
/// below 13 and 32 here: `month * 32 + day` picks the date's table entry, and
/// a larger day would land on another date's entry (day 33 of September on
/// October 1).
#[inline(always)]
unsafe fn iso_ceilings() -> __m128i {
    _mm_setr_epi16(12, 31, 23, 59, 59, 0x7FFF, 0x7FFF, 0x7FFF)
}

/// `month * 32 + day`, `hour * 3600 + minute * 60`, `second`.
#[inline(always)]
unsafe fn iso_weights() -> __m128i {
    _mm_setr_epi16(32, 1, 3600, 60, 1, 0, 0, 0)
}

/// Turns one lane of the vector work into seconds since the tables' base.
/// `packed` is `month * 32 + day` in the low half and the second of the day
/// in the high; `bad` is nonzero if the shape or a range check failed.
#[inline(always)]
unsafe fn iso_finish(tables: &TsTables, start: *const u8, packed: u64, bad: u32) -> i64 {
    let year = (start as *const u32).read_unaligned();
    let next_year = year == tables.year_text[1];
    let year_ok = (year == tables.year_text[0]) | next_year;
    let month_day = (packed as u32) & 4095;
    let day = *tables.date.get_unchecked((usize::from(next_year) << 12) | month_day as usize);
    let seconds = i64::from(day) * 86_400 + (packed >> 32) as i64;
    if (bad == 0) & year_ok & (day != 0xFF) {
        seconds
    } else {
        NOT_FAST
    }
}

/// The date and time of day of two timestamps shaped `YYYY-MM-DDTHH:MM:SS`
/// (their first 19 bytes), as seconds since the tables' base; `NOT_FAST` for
/// one that is not that shape or whose date is outside the tables. What
/// follows byte 18 (`Z` or an offset) is the caller's to check.
#[inline(always)]
unsafe fn iso_pair(tables: &TsTables, a: *const u8, b: *const u8) -> (i64, i64) {
    let text = _mm256_inserti128_si256(
        _mm256_castsi128_si256(_mm_loadu_si128(a.add(3) as *const __m128i)),
        _mm_loadu_si128(b.add(3) as *const __m128i),
        1,
    );
    let twice = |lane: __m128i| _mm256_broadcastsi128_si256(lane);
    let values = _mm256_xor_si256(text, twice(iso_pattern()));
    let limit = twice(iso_limit());
    let shape = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_min_epu8(values, limit), values)) as u32;
    let pairs = _mm256_shuffle_epi8(values, twice(iso_shuffle()));
    let numbers = _mm256_maddubs_epi16(pairs, _mm256_set1_epi16(0x010A));
    let over = _mm256_movemask_epi8(_mm256_cmpgt_epi16(numbers, twice(iso_ceilings()))) as u32;
    let sums = _mm256_madd_epi16(numbers, twice(iso_weights()));
    // dword 1 becomes hour/minute seconds plus the second.
    let folded = _mm256_blend_epi32(sums, _mm256_add_epi32(sums, _mm256_srli_si256(sums, 4)), 0b0010_0010);
    let low = _mm_cvtsi128_si64(_mm256_castsi256_si128(folded)) as u64;
    let high = _mm_cvtsi128_si64(_mm256_extracti128_si256(folded, 1)) as u64;
    let bad = !shape | over;
    (iso_finish(tables, a, low, bad & 0xFFFF), iso_finish(tables, b, high, bad >> 16))
}

/// As `iso_pair`, for one timestamp.
#[inline(always)]
unsafe fn iso_one(tables: &TsTables, a: *const u8) -> i64 {
    let text = _mm_loadu_si128(a.add(3) as *const __m128i);
    let values = _mm_xor_si128(text, iso_pattern());
    let shape = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(values, iso_limit()), values)) as u32;
    let pairs = _mm_shuffle_epi8(values, iso_shuffle());
    let numbers = _mm_maddubs_epi16(pairs, _mm_set1_epi16(0x010A));
    let over = _mm_movemask_epi8(_mm_cmpgt_epi16(numbers, iso_ceilings())) as u32;
    let sums = _mm_madd_epi16(numbers, iso_weights());
    let folded = _mm_blend_epi32(sums, _mm_add_epi32(sums, _mm_srli_si128(sums, 4)), 0b0010);
    iso_finish(tables, a, _mm_cvtsi128_si64(folded) as u64, (!shape & 0xFFFF) | over)
}

/// The `+HH:MM` or `-HH:MM` at `p` as signed seconds; `NOT_FAST` if it is not
/// a legal offset (hours 00 to 23, minutes 00 to 59, with the colon).
#[inline(always)]
unsafe fn iso_offset(p: *const u8) -> i64 {
    let digit = |at: usize| i64::from((*p.add(at)).wrapping_sub(b'0'));
    let (h1, h0, m1, m0) = (digit(1), digit(2), digit(4), digit(5));
    let sign = *p;
    let hours = h1 * 10 + h0;
    let minutes = m1 * 10 + m0;
    let ok = (h1 <= 9)
        & (h0 <= 9)
        & (m1 <= 9)
        & (m0 <= 9)
        & (*p.add(3) == b':')
        & ((sign == b'+') | (sign == b'-'))
        & (hours <= 23)
        & (minutes <= 59);
    let seconds = (hours * 60 + minutes) * 60;
    if !ok {
        NOT_FAST
    } else if sign == b'-' {
        -seconds
    } else {
        seconds
    }
}

#[repr(C, align(16))]
struct A16([u8; 16]);

const fn digit_shuffles() -> [A16; 17] {
    // Entry `n` right-aligns the first `n` bytes and zero-fills the front.
    let mut table = [const { A16([0x80; 16]) }; 17];
    let mut length = 0;
    while length <= 16 {
        let mut at = 16 - length;
        while at < 16 {
            table[length].0[at] = (at - (16 - length)) as u8;
            at += 1;
        }
        length += 1;
    }
    table
}
static DIGIT_SHUFFLE: [A16; 17] = digit_shuffles();

/// F3: `length` (at most 16) ASCII digits at `p`; `NOT_FAST` if any byte is
/// not a digit.
#[inline(always)]
unsafe fn ts_digits(p: *const u8, length: usize) -> i64 {
    let text = _mm_loadu_si128(p as *const __m128i);
    let values = _mm_xor_si128(text, _mm_set1_epi8(0x30));
    let aligned = _mm_shuffle_epi8(values, _mm_load_si128(DIGIT_SHUFFLE[length].0.as_ptr() as *const __m128i));
    let ok = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(aligned, _mm_set1_epi8(9)), aligned)) == 0xFFFF;
    let twos = _mm_maddubs_epi16(aligned, _mm_set1_epi16(0x010A));
    let fours = _mm_madd_epi16(twos, _mm_set1_epi32(0x0001_0064));
    let packed = _mm_packus_epi32(fours, fours);
    let eights = _mm_madd_epi16(packed, _mm_set1_epi32(0x0001_2710));
    let both = _mm_cvtsi128_si64(eights) as u64;
    let value = (both & 0xFFFF_FFFF) * 100_000_000 + (both >> 32);
    select_unpredictable(ok, value as i64, NOT_FAST)
}

/// A `ts` field as epoch ms, exactly as `parse_ts` reads it; `NOT_FAST` for a
/// bad timestamp. `iso` is the `iso_pair`/`iso_one` result for its first 19 bytes.
#[inline(always)]
unsafe fn ts_value(tables: &TsTables, p: *const u8, length: usize, iso: i64) -> i64 {
    // Which of the three forms a row uses is close to random, so pick the
    // candidate without branching on it.
    let digits = ts_digits(p, length.min(16));
    let plain = (length == 20) & (*p.add(19) == b'Z') & (iso != NOT_FAST);
    let iso_ms = tables.base_ms.wrapping_add(iso.wrapping_mul(1_000));
    let fast = select_unpredictable(length <= 16, digits, select_unpredictable(plain, iso_ms, NOT_FAST));
    if (fast != NOT_FAST) | (length <= 16) {
        // Up to 16 bytes that are not all digits cannot be any valid form.
        return fast;
    }
    ts_rest(tables, p, length, iso)
}

/// The less common shapes: F2, a date outside the tables, very long F3, bad text.
#[inline(never)]
unsafe fn ts_rest(tables: &TsTables, p: *const u8, length: usize, iso: i64) -> i64 {
    if length == 25 && iso != NOT_FAST {
        let offset = iso_offset(p.add(19));
        if offset != NOT_FAST {
            return tables.base_ms + (iso - offset) * 1_000;
        }
    }
    parse_ts(std::slice::from_raw_parts(p, length)).unwrap_or(NOT_FAST)
}

/// An `ingest_ts` the fast path could not read: `(epoch ms, compact seconds)`
/// through the general parser, or `None` after asking for the reference
/// pipeline (the rules promise valid F1 here).
#[cold]
#[inline(never)]
fn ingest_rest(ctx: &Ctx, text: &[u8]) -> Option<(i64, i64)> {
    let Some(ingest_ms) = parse_ts(text) else {
        ctx.request_fallback();
        return None;
    };
    if ingest_ms.rem_euclid(1_000) != 0 {
        ctx.request_fallback();
        return None;
    }
    Some((ingest_ms, ingest_ms.div_euclid(1_000) - ctx.ingest_base_seconds))
}

#[inline(always)]
unsafe fn read_u64(p: *const u8) -> u64 {
    (p as *const u64).read_unaligned()
}

/// Codes of two IDs at once. Each argument is the up-to-8 characters after
/// the prefix, already cut to the ID's length (unused high bytes zero).
/// Returns the two base-37 codes and, per byte, whether it is in `[0-9a-z]`
/// (bits 0..8 for the first ID, 8..16 for the second).
#[inline(always)]
unsafe fn two_codes(first: u64, second: u64) -> (u64, u64, u32) {
    let text = _mm_set_epi64x(second as i64, first as i64);
    let as_digit = _mm_sub_epi8(text, _mm_set1_epi8(b'0' as i8));
    let is_digit = _mm_cmpeq_epi8(_mm_min_epu8(as_digit, _mm_set1_epi8(9)), as_digit);
    let as_letter = _mm_sub_epi8(text, _mm_set1_epi8(b'a' as i8));
    let is_letter = _mm_cmpeq_epi8(_mm_min_epu8(as_letter, _mm_set1_epi8(25)), as_letter);
    let values = _mm_or_si128(
        _mm_and_si128(is_digit, _mm_add_epi8(as_digit, _mm_set1_epi8(1))),
        _mm_and_si128(is_letter, _mm_add_epi8(as_letter, _mm_set1_epi8(11))),
    );
    let known = _mm_movemask_epi8(_mm_or_si128(is_digit, is_letter)) as u32;
    // Base 37, most significant character first: pairs, fours, then eight.
    let pairs = _mm_maddubs_epi16(values, _mm_set1_epi16(0x0125));
    let fours = _mm_madd_epi16(pairs, _mm_set1_epi32(0x0001_0559));
    let codes = _mm_add_epi64(_mm_mul_epu32(fours, _mm_set1_epi64x(1_874_161)), _mm_srli_epi64(fours, 32));
    (_mm_cvtsi128_si64(codes) as u64, _mm_extract_epi64(codes, 1) as u64, known)
}

/// The base-36 numbers of two texts at once. Each argument holds up to 8
/// characters, right-aligned (the last character in the top byte, unused low
/// bytes zero). Returns the two numbers and, per byte, whether it is in
/// `[0-9a-z]` (bits 0..8 for the first text, 8..16 for the second).
#[inline(always)]
unsafe fn two_numbers36(first: u64, second: u64) -> (u64, u64, u32) {
    let text = _mm_set_epi64x(second as i64, first as i64);
    let as_digit = _mm_sub_epi8(text, _mm_set1_epi8(b'0' as i8));
    let is_digit = _mm_cmpeq_epi8(_mm_min_epu8(as_digit, _mm_set1_epi8(9)), as_digit);
    let as_letter = _mm_sub_epi8(text, _mm_set1_epi8(b'a' as i8));
    let is_letter = _mm_cmpeq_epi8(_mm_min_epu8(as_letter, _mm_set1_epi8(25)), as_letter);
    let values = _mm_or_si128(
        _mm_and_si128(is_digit, as_digit),
        _mm_and_si128(is_letter, _mm_add_epi8(as_letter, _mm_set1_epi8(10))),
    );
    let known = _mm_movemask_epi8(_mm_or_si128(is_digit, is_letter)) as u32;
    // Base 36, most significant character first: pairs, fours, then eight.
    let pairs = _mm_maddubs_epi16(values, _mm_set1_epi16(0x0124));
    let fours = _mm_madd_epi16(pairs, _mm_set1_epi32(0x0001_0510));
    let numbers = _mm_add_epi64(_mm_mul_epu32(fours, _mm_set1_epi64x(1_679_616)), _mm_srli_epi64(fours, 32));
    (_mm_cvtsi128_si64(numbers) as u64, _mm_extract_epi64(numbers, 1) as u64, known)
}

/// Whether `chars` characters starting with the byte `first` and read as
/// `number` are a canonical base-36 text: 1 to 8 known characters, no leading
/// zero, below 2^40. `known` has one bit per byte of the right-aligned text.
#[inline(always)]
fn canonical36(chars: usize, first: u8, known: u32, number: u64) -> bool {
    let need = (0xFFu32 << (8usize.wrapping_sub(chars) & 7)) & 0xFF;
    (chars.wrapping_sub(1) < 8) & (known & need == need) & ((first != b'0') | (chars == 1)) & (number <= NUMBER_MASK)
}

const VIEW: u64 = u32::from_le_bytes(*b"view") as u64;
const CART: u64 = u32::from_le_bytes(*b"cart") as u64;
const PURCHASE: u64 = u64::from_le_bytes(*b"purchase");

#[inline(always)]
unsafe fn mask(bytes: __m256i, byte: __m256i) -> u64 {
    u64::from(_mm256_movemask_epi8(_mm256_cmpeq_epi8(bytes, byte)) as u32)
}

/// Parses the event row starting at `p` (which must have `ROW_PAD` readable
/// bytes after it) and, in `MODE_STORE`, stores it in `out`. Returns the
/// row's length and a `ROW_*` status. Only rows of 64 to 95 bytes take the
/// fast path (99.9% of generated rows); the rest are declined.
///
/// With `INDEXED`, IDs are read as the generator's scrambled numbers
/// (`ctx.keys` must be set): the event's dense index replaces its code and
/// picks the partition, and a generated user ID is stored as its user number.
#[inline(always)]
pub unsafe fn event_row_fast<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    p: *const u8,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> (usize, u32) {
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let comma = _mm256_set1_epi8(b',' as i8);
    let quote = _mm256_set1_epi8(b'"' as i8);
    let block0 = _mm256_loadu_si256(p as *const __m256i);
    let block1 = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let block2 = _mm256_loadu_si256(p.add(64) as *const __m256i);
    let newline_low = mask(block0, newline) | (mask(block1, newline) << 32);
    let newline_high = mask(block2, newline);
    if newline_low != 0 {
        return (newline_low.trailing_zeros() as usize, ROW_SLOW);
    }
    if newline_high == 0 {
        return (0, ROW_LONG);
    }
    // Bits of the high word are positions 64.. of the row.
    let high_length = newline_high.trailing_zeros();
    let length = 64 + high_length as usize;
    let below_high = (1u64 << high_length) - 1;
    let comma_low = mask(block0, comma) | (mask(block1, comma) << 32);
    let comma_high = mask(block2, comma) & below_high;
    let quote_low = mask(block0, quote) | (mask(block1, quote) << 32);
    let quote_high = mask(block2, quote) & below_high;
    if MODE == 2 {
        row.event ^= comma_low ^ comma_high ^ quote_low ^ quote_high;
        return (length, ROW_DROP);
    }
    // Seven fields, and a first field that is not empty (an empty event_id
    // drops the line just as a wrong field count does).
    let commas_low = comma_low.count_ones();
    if commas_low + comma_high.count_ones() != 6 || comma_low & 1 != 0 {
        return (length, ROW_DROP);
    }

    // Quotes are legal only as a pair around a whole field. Every field has
    // a start position (after a separator, or 0) and an end position (before
    // its separator); a quote must sit on one of them, a field must be
    // quoted at both or neither, and one quote cannot be both. The bits
    // PEXT pulls out at the starts are the per-field "is quoted" flags.
    let separator_high = comma_high | (1u64 << high_length);
    let starts_low = (comma_low << 1) | 1;
    let starts_high = (separator_high << 1) | (comma_low >> 63);
    let ends_low = (comma_low >> 1) | (separator_high << 63);
    let ends_high = separator_high >> 1;
    let stray = (quote_low & !(starts_low | ends_low)) | (quote_high & !(starts_high | ends_high));
    let lone = (quote_low & starts_low & ends_low) | (quote_high & starts_high & ends_high);
    let opened = _pext_u64(quote_low, starts_low) | (_pext_u64(quote_high, starts_high) << starts_low.count_ones());
    let closed = _pext_u64(quote_low, ends_low) | (_pext_u64(quote_high, ends_high) << ends_low.count_ones());
    if (stray | lone | (opened ^ closed)) != 0 {
        return (length, ROW_DROP);
    }

    // The first three commas from the low word, the last three from the 64
    // bytes that end at the newline.
    let tail = (comma_low >> high_length) | comma_high.wrapping_shl(64 - high_length);
    if commas_low < 3 || tail.count_ones() < 3 {
        return (length, ROW_SLOW);
    }
    let c0 = comma_low.trailing_zeros() as usize;
    let rest = comma_low & (comma_low - 1);
    let c1 = rest.trailing_zeros() as usize;
    let rest = rest & (rest - 1);
    let c2 = rest.trailing_zeros() as usize;
    let lead5 = tail.leading_zeros();
    let tail = _bzhi_u64(tail, 63 - lead5);
    let lead4 = tail.leading_zeros();
    let tail = _bzhi_u64(tail, 63 - lead4);
    let lead3 = tail.leading_zeros();
    let c5 = length - 1 - lead5 as usize;
    let c4 = length - 1 - lead4 as usize;
    let c3 = length - 1 - lead3 as usize;
    if MODE == 3 {
        row.event ^= (c0 ^ c1 ^ c2 ^ c3 ^ c4 ^ c5) as u64 ^ opened;
        return (length, ROW_DROP);
    }

    // Field starts and lengths with the quotes stripped.
    let quoted = |index: u32| ((opened >> index) & 1) as usize;
    let (q0, q1, q2, q3, q4, q5) = (quoted(0), quoted(1), quoted(2), quoted(3), quoted(4), quoted(5));
    let event_start = q0;
    let event_length = c0 - 2 * q0;
    let user_start = c0 + 1 + q1;
    let user_length = c1 - c0 - 1 - 2 * q1;
    let ts_start = c1 + 1 + q2;
    let ts_length = c2 - c1 - 1 - 2 * q2;
    let ingest_start = c2 + 1 + q3;
    let ingest_length = c3 - c2 - 1 - 2 * q3;
    let type_start = c3 + 1 + q4;
    let type_length = c4 - c3 - 1 - 2 * q4;
    let product_length = c5 - c4 - 1 - 2 * q5;
    if event_length == 0 {
        return (length, ROW_DROP);
    }
    if MODE == 4 {
        row.event ^= (event_start ^ event_length ^ user_start ^ user_length ^ ts_start ^ ts_length) as u64
            ^ (ingest_start ^ ingest_length ^ type_start ^ type_length ^ product_length) as u64;
        return (length, ROW_DROP);
    }
    let field = |start: usize, length: usize| std::slice::from_raw_parts(p.add(start), length);

    // Event and user IDs together: up to 8 characters each after the prefix.
    let event_chars = event_length - 1;
    let user_chars = user_length.wrapping_sub(1);
    let event_text = _bzhi_u64(read_u64(p.add(event_start + 1)), (event_chars as u32).wrapping_mul(8));
    let user_text = _bzhi_u64(read_u64(p.add(user_start + 1)), (user_chars as u32).wrapping_mul(8));
    let (event_fast_code, user_fast_code, known) = if INDEXED {
        two_numbers36(
            event_text << ((8usize.wrapping_sub(event_chars) & 7) * 8),
            user_text << ((8usize.wrapping_sub(user_chars) & 7) * 8),
        )
    } else {
        two_codes(event_text, user_text)
    };
    let event_code = if INDEXED {
        // The event's dense index, or the general path for any other ID.
        let keys = ctx.keys.as_ref().unwrap_unchecked();
        let index = unscramble(event_fast_code, keys.event_key);
        let generated = (*p.add(event_start) == b'e')
            & canonical36(event_chars, event_text as u8, known, event_fast_code)
            & (index < keys.index_bound);
        if !generated {
            return (length, ROW_GENERIC);
        }
        index
    } else {
        let event_need = (1u32 << (event_chars & 15)) - 1;
        let event_fast =
            (event_chars.wrapping_sub(1) < 8) & (*p.add(event_start) == b'e') & (known & event_need == event_need);
        if event_fast { event_fast_code } else { id_code(field(event_start, event_length), b'e', &ctx.event_ids) }
    };
    if MODE == 5 {
        row.event ^= event_code ^ user_fast_code;
        return (length, ROW_DROP);
    }

    // Both timestamps' date and time in one pass.
    let tables = &ctx.ts_tables;
    let (ts_iso, ingest_iso) = iso_pair(tables, p.add(ts_start), p.add(ingest_start));

    // ingest_ts: always valid F1 by the rules; anything else goes the slow way.
    let ingest_fast = (ingest_length == 20) & (*p.add(ingest_start + 19) == b'Z') & (ingest_iso != NOT_FAST);
    let (ingest_ms, ingest_seconds) = if ingest_fast {
        (tables.base_ms + ingest_iso * 1_000, ingest_iso + tables.ingest_adjust)
    } else {
        match ingest_rest(ctx, field(ingest_start, ingest_length)) {
            Some(pair) => pair,
            None => return (length, ROW_DROP),
        }
    };
    if !(0..(1 << 21)).contains(&ingest_seconds) {
        ctx.request_fallback();
        return (length, ROW_DROP);
    }

    let ts_ms = ts_value(tables, p.add(ts_start), ts_length, ts_iso);
    if MODE == 6 {
        row.event ^= event_code ^ user_fast_code ^ (ts_ms as u64) ^ (ingest_seconds as u64);
        return (length, ROW_DROP);
    }
    let in_window = (ts_ms >= ctx.window_start) & (ts_ms < ctx.window_end);
    let late = ingest_ms.wrapping_sub(ts_ms) > LATE_MS;

    let type_text = _bzhi_u64(read_u64(p.add(type_start)), (type_length.min(8) as u32) * 8);
    let is_purchase = (type_length == 8) & (type_text == PURCHASE);
    let known_type = is_purchase | ((type_length == 4) & ((type_text == VIEW) | (type_text == CART)));

    let mut valid = (user_length != 0) & in_window & !late & known_type & (product_length != 0);
    let mut purchase = None;
    if valid & is_purchase {
        // Only purchases look at the product's and the price's text.
        let q6 = quoted(6);
        let price = field(c5 + 1 + q6, length - c5 - 1 - 2 * q6);
        match parse_price(price) {
            Some(price) => {
                purchase = Some((price, product_category(ctx, p.add(c4 + 1 + q5), product_length)));
            }
            None => valid = false,
        }
    }
    let (user_code, user_is_number, ts) = if !valid {
        (0, false, 0)
    } else if INDEXED {
        // A generated user ID is stored as its user number; any other ID as its code.
        let keys = ctx.keys.as_ref().unwrap_unchecked();
        let number = unscramble(user_fast_code, keys.user_key);
        let generated = (*p.add(user_start) == b'u')
            & canonical36(user_chars, user_text as u8, known >> 8, user_fast_code)
            & ((number >> 9) < keys.user_bound);
        if generated {
            (number, true, (ts_ms - ctx.window_start) as u64)
        } else {
            let code = id_code(field(user_start, user_length), b'u', &ctx.user_ids);
            (code, false, (ts_ms - ctx.window_start) as u64)
        }
    } else {
        let user_need = (1u32 << (user_chars & 15)) - 1;
        let user_fast = (user_chars.wrapping_sub(1) < 8)
            & (*p.add(user_start) == b'u')
            & ((known >> 8) & user_need == user_need);
        let user_code =
            if user_fast { user_fast_code } else { id_code(field(user_start, user_length), b'u', &ctx.user_ids) };
        (user_code, false, (ts_ms - ctx.window_start) as u64)
    };
    if MODE == MODE_STORE {
        if INDEXED {
            emit_indexed(
                out,
                event_code,
                ts,
                u64::from(valid),
                user_code,
                u64::from(user_is_number),
                ingest_seconds as u64,
                purchase,
            );
        } else {
            emit_raw(out, event_code, ts, u64::from(valid), user_code, ingest_seconds as u64, purchase);
        }
        return (length, ROW_DONE);
    }
    if MODE == 7 {
        row.event ^= event_code ^ user_code ^ ts ^ (ingest_seconds as u64);
        return (length, ROW_DROP);
    }
    *row = EventRow {
        event: event_code,
        user: user_code,
        user_is_number,
        ts: ts as u32,
        ingest: ingest_seconds as u32,
        valid,
        purchase,
    };
    (length, ROW_EMIT)
}

/// The number written as 1 to 7 ASCII digits with no leading zero at `p`;
/// `None` for any other text of that length.
#[inline(always)]
unsafe fn small_number(p: *const u8, digits: usize) -> Option<usize> {
    if digits.wrapping_sub(1) >= 7 {
        return None;
    }
    // Digits at the top of the word, zero-filled below, then the usual
    // 8-digit multiply reduction.
    let values = (read_u64(p) ^ 0x3030_3030_3030_3030) << ((8 - digits) * 8);
    let not_digit = (values.wrapping_add(0x7676_7676_7676_7676) | values) & 0x8080_8080_8080_8080;
    if not_digit != 0 || *p == b'0' {
        return None;
    }
    let pairs = (values.wrapping_mul(10) + (values >> 8)) & 0x00FF_00FF_00FF_00FF;
    let fours = (pairs.wrapping_mul(100) + (pairs >> 16)) & 0x0000_FFFF_0000_FFFF;
    Some((fours.wrapping_mul(10_000) + (fours >> 32)) as u32 as usize)
}

/// Parses the link row starting at `p` (with `ROW_PAD` readable bytes after
/// it). Only rows shorter than 32 bytes take the fast path.
#[inline(always)]
pub unsafe fn link_row_fast(ctx: &Ctx, p: *const u8, pair: &mut (u64, u64)) -> (usize, u32) {
    let block = _mm256_loadu_si256(p as *const __m256i);
    let newlines = mask(block, _mm256_set1_epi8(b'\n' as i8));
    if newlines == 0 {
        return (0, ROW_LONG);
    }
    let length = newlines.trailing_zeros() as usize;
    let below = (1u64 << length) - 1;
    let commas = mask(block, _mm256_set1_epi8(b',' as i8)) & below;
    let quotes = mask(block, _mm256_set1_epi8(b'"' as i8)) & below;
    // Two fields and a first field that is not empty (an empty side drops
    // the line just as a wrong field count does).
    if commas.count_ones() != 1 || commas & 1 != 0 {
        return (length, ROW_DROP);
    }
    // Same quote rule as for events, in one word.
    let separators = commas | (1u64 << length);
    let starts = (separators << 1) | 1;
    let ends = separators >> 1;
    let opened = _pext_u64(quotes, starts);
    if ((quotes & !(starts | ends)) | (quotes & starts & ends) | (opened ^ _pext_u64(quotes, ends))) != 0 {
        return (length, ROW_DROP);
    }
    let c0 = commas.trailing_zeros() as usize;
    let q0 = (opened & 1) as usize;
    let q1 = ((opened >> 1) & 1) as usize;
    let (s0, e0) = (q0, c0 - q0);
    let (s1, e1) = (c0 + 1 + q1, length - q1);
    if e0 == s0 || e1 == s1 {
        return (length, ROW_DROP);
    }
    let field = |start: usize, end: usize| std::slice::from_raw_parts(p.add(start), end - start);
    let a_chars = e0 - s0 - 1;
    let b_chars = e1 - s1 - 1;
    let a_text = _bzhi_u64(read_u64(p.add(s0 + 1)), (a_chars as u32).wrapping_mul(8));
    let b_text = _bzhi_u64(read_u64(p.add(s1 + 1)), (b_chars as u32).wrapping_mul(8));
    let (a_fast_code, b_fast_code, known) = two_codes(a_text, b_text);
    let a_need = (1u32 << (a_chars & 15)) - 1;
    let a_fast = (a_chars.wrapping_sub(1) < 8) & (*p.add(s0) == b'u') & (known & a_need == a_need);
    let b_need = (1u32 << (b_chars & 15)) - 1;
    let b_fast = (b_chars.wrapping_sub(1) < 8) & (*p.add(s1) == b'u') & ((known >> 8) & b_need == b_need);
    let a = if a_fast { a_fast_code } else { id_code(field(s0, e0), b'u', &ctx.user_ids) };
    let b = if b_fast { b_fast_code } else { id_code(field(s1, e1), b'u', &ctx.user_ids) };
    *pair = (a, b);
    (length, ROW_EMIT)
}

/// `c_<number>` as its number; `None` for any other text.
#[inline(always)]
unsafe fn campaign_number(p: *const u8, length: usize) -> Option<usize> {
    if length < 3 || (p as *const u16).read_unaligned() != u16::from_le_bytes(*b"c_") {
        return None;
    }
    small_number(p.add(2), length - 2)
}

/// The category of a purchase's product: a direct table for IDs written
/// `p<number>`, the exact map for anything else.
#[inline(always)]
unsafe fn product_category(ctx: &Ctx, p: *const u8, length: usize) -> u32 {
    if length >= 2 && *p == b'p' {
        if let Some(number) = small_number(p.add(1), length - 1) {
            if number < ctx.category_by_product_number.len() {
                // The table holds every product written this way, so a miss is `unknown`.
                return *ctx.category_by_product_number.get_unchecked(number);
            }
        }
    }
    let product = std::slice::from_raw_parts(p, length);
    ctx.product_category.get(product).copied().unwrap_or(ctx.unknown_category)
}

/// Parses the click row starting at `p` (with `ROW_PAD` readable bytes after
/// it). Only rows shorter than 64 bytes take the fast path.
#[inline(always)]
pub unsafe fn click_row_fast(ctx: &Ctx, p: *const u8, click: &mut ClickRecord) -> (usize, u32) {
    let block0 = _mm256_loadu_si256(p as *const __m256i);
    let block1 = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let newlines = mask(block0, _mm256_set1_epi8(b'\n' as i8)) | (mask(block1, _mm256_set1_epi8(b'\n' as i8)) << 32);
    if newlines == 0 {
        return (0, ROW_LONG);
    }
    let length = newlines.trailing_zeros() as usize;
    let below = (1u64 << length) - 1;
    let commas = (mask(block0, _mm256_set1_epi8(b',' as i8)) | (mask(block1, _mm256_set1_epi8(b',' as i8)) << 32)) & below;
    let quotes = (mask(block0, _mm256_set1_epi8(b'"' as i8)) | (mask(block1, _mm256_set1_epi8(b'"' as i8)) << 32)) & below;
    // Four fields and a first field that is not empty (an empty click_id
    // drops the line just as a wrong field count does).
    if commas.count_ones() != 3 || commas & 1 != 0 {
        return (length, ROW_DROP);
    }
    // Same quote rule as for events, in one word.
    let separators = commas | (1u64 << length);
    let starts = (separators << 1) | 1;
    let ends = separators >> 1;
    let opened = _pext_u64(quotes, starts);
    if ((quotes & !(starts | ends)) | (quotes & starts & ends) | (opened ^ _pext_u64(quotes, ends))) != 0 {
        return (length, ROW_DROP);
    }
    let c0 = commas.trailing_zeros() as usize;
    let rest = commas & (commas - 1);
    let c1 = rest.trailing_zeros() as usize;
    let rest = rest & (rest - 1);
    let c2 = rest.trailing_zeros() as usize;
    let quoted = |index: u32| ((opened >> index) & 1) as usize;
    let (q0, q1, q2, q3) = (quoted(0), quoted(1), quoted(2), quoted(3));
    let (s0, e0) = (q0, c0 - q0);
    let (s1, e1) = (c0 + 1 + q1, c1 - q1);
    let (s2, e2) = (c1 + 1 + q2, c2 - q2);
    let (s3, e3) = (c2 + 1 + q3, length - q3);
    if e0 == s0 || e1 == s1 {
        return (length, ROW_DROP);
    }
    let field = |start: usize, end: usize| std::slice::from_raw_parts(p.add(start), end - start);

    let click_window_start = ctx.window_start - WEEK_MS;
    let tables = &ctx.ts_tables;
    let ts_ms = ts_value(tables, p.add(s3), e3 - s3, iso_one(tables, p.add(s3)));
    if ts_ms < click_window_start || ts_ms >= ctx.window_end {
        return (length, ROW_DROP);
    }
    let campaign = match campaign_number(p.add(s2), e2 - s2) {
        Some(number) if number < ctx.campaign_by_number.len() => ctx.campaign_by_number[number],
        // Not `c_<number>`, or a number beyond the table: the exact map decides.
        _ => ctx.campaign_index.get(field(s2, e2)).copied().unwrap_or(u32::MAX),
    };
    if campaign == u32::MAX {
        return (length, ROW_DROP);
    }

    let click_chars = e0 - s0 - 1;
    let user_chars = e1 - s1 - 1;
    let click_text = _bzhi_u64(read_u64(p.add(s0 + 1)), (click_chars as u32).wrapping_mul(8));
    let user_text = _bzhi_u64(read_u64(p.add(s1 + 1)), (user_chars as u32).wrapping_mul(8));
    let (click_fast_code, user_fast_code, known) = two_codes(click_text, user_text);
    let click_need = (1u32 << (click_chars & 15)) - 1;
    let click_fast = (click_chars.wrapping_sub(1) < 8) & (*p.add(s0) == b'k') & (known & click_need == click_need);
    let click_code = if click_fast { click_fast_code } else { id_code(field(s0, e0), b'k', &ctx.click_ids) };
    let user_need = (1u32 << (user_chars & 15)) - 1;
    let user_fast =
        (user_chars.wrapping_sub(1) < 8) & (*p.add(s1) == b'u') & ((known >> 8) & user_need == user_need);
    let user = if user_fast { user_fast_code } else { id_code(field(s1, e1), b'u', &ctx.user_ids) };
    *click = ClickRecord {
        user,
        code_and_campaign: click_code | (u64::from(campaign) << 42),
        ts: (ts_ms - click_window_start) as u32,
    };
    (length, ROW_EMIT)
}
