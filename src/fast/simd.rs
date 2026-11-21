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
    /// The lean event kernel's tables.
    lean: LeanTables,
}

impl TsTables {
    pub fn new(window_start: i64, ingest_base_seconds: i64) -> Self {
        // From 9 days before the window (click window plus a day of UTC
        // offset) for 62 days, which covers every ingest time the compact
        // records can hold.
        let base_day = window_start.div_euclid(DAY_MS) - 9;
        let (year0, _, _) = civil_from_days(base_day);
        let text = |year: i64| {
            let digits = [
                b'0' + (year / 1000 % 10) as u8,
                b'0' + (year / 100 % 10) as u8,
                b'0' + (year / 10 % 10) as u8,
                b'0' + (year % 10) as u8,
            ];
            u32::from_le_bytes(digits)
        };
        let mut date = [0xFFu8; 2 << 12];
        for offset in 0..62 {
            let (year, month, day) = civil_from_days(base_day + offset);
            let year_index = (year - year0) as usize;
            // A year past 9999 has no four-digit text, so its days stay out.
            if year_index <= 1 && year <= 9999 {
                date[(year_index << 12) | ((month as usize) << 5) | day as usize] = offset as u8;
            }
        }
        let ingest_adjust = base_day * 86_400 - ingest_base_seconds;
        Self {
            year_text: [text(year0), text(year0 + 1)],
            base_ms: base_day * DAY_MS,
            ingest_adjust,
            date,
            lean: LeanTables::new(base_day, ingest_adjust, text),
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

// ---------------------------------------------------------------------------
// The lean event kernel.
//
// It takes only rows of the generator's regular shape and declines every
// other row, which then goes through `event_row_fast` above (and from there,
// if need be, through the general parser). A row it takes has been checked
// byte for byte: every byte of the row is either a comma, a quote or the
// newline at a position the row's shape fixes, or part of a field whose
// whole text was compared or classified. So a row the lean kernel stores is
// well formed, and nothing about it is left to chance.
//
// The regular shape:
//   event_id   `e` and 7 or 8 characters of [0-9a-z]
//   user_id    `u` and 7 or 8 characters of [0-9a-z]
//   ts         13 digits, or F1 or F2 on a day of the date table
//   ingest_ts  F1 on a day of the date table
//   event_type `view`, `cart` or `purchase`
//   product_id not empty
//   any field in quotes or not; product, price and their separators within
//   32 bytes. In index mode both IDs must be generated ones.
// What varies from row to row at random (the quotes, the three timestamp
// forms, the ID lengths) is handled without branches; the branches left are
// the checks that almost never fail, the 10% of F2 timestamps and the 3% of
// purchases.
//
// How a row is read:
//   1. Comma and quote masks of the first 64 bytes give the first three
//      commas and, with two PEXT, which of the first four fields open with a
//      quote and which of the first three close with one.
//   2. That fixes where both IDs, both timestamps and the event type are:
//      ingest_ts is 20 bytes, so its closing comma needs no search. Each is
//      matched in full, quotes and commas included.
//   3. The 32 bytes from the comma after the type hold the product, one
//      comma, the price and the newline; their masks give the rest.
// The newline itself is looked for in bytes 64..96 before any of this, only
// to know early where the next row starts: steps 1 to 3 find the row's true
// end, and a row whose end is elsewhere is still read correctly, just later.
// ---------------------------------------------------------------------------

/// The lean kernel declines the row.
pub const ROW_DECLINE: u32 = 6;

/// The event kernels a fast loop can run.
pub const KERNEL_GENERAL: u32 = 0;
pub const KERNEL_LEAN: u32 = 1;

/// The event kernel of this run: the lean one unless `SS_KERNEL=general`
/// asks for `event_row_fast` alone (for A/B runs).
pub fn event_kernel() -> u32 {
    static KERNEL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *KERNEL.get_or_init(|| match std::env::var("SS_KERNEL").as_deref() {
        Ok("general") => KERNEL_GENERAL,
        _ => KERNEL_LEAN,
    })
}

/// What the lean kernel reads from memory at every use. The three byte
/// vectors are here and not constants in the code so that each compare takes
/// its operand straight from memory, on every target, with a call in the loop
/// or without (a constant in a register does not survive a call on Linux,
/// and is then loaded again for every row).
#[repr(C, align(64))]
pub struct LeanTables {
    newline: [u8; 32],
    comma: [u8; 32],
    quote: [u8; 32],
    /// Added to a thousand times a day entry's seconds to get epoch ms.
    base_ms: i64,
    /// Bytes from the comma before `view`/`cart` to the comma after it, by
    /// the four-way match of the type (0: neither).
    type_step: [u8; 16],
    /// The two bytes that end ingest_ts: `Z,` or `",`.
    ingest_end: [u16; 2],
    /// Index mode reads the eight bytes that end an ID as a base-36 number.
    /// For an ID of 7 characters the first of them is the prefix: its part
    /// of that number, for `e` (7 and 8 characters) and for `u`.
    lead36: [u64; 4],
    /// The numbers of canonical texts (no leading zero, below 2^40) of 7 and
    /// of 8 characters: the smallest, and how many more there are.
    canonical36: [[u64; 2]; 2],
    /// By `month * 32 + day`: the year text of the table day with that month
    /// and day, and the compact ingest seconds of its midnight (a signed
    /// number). Where there is no such day: a year no text has in a row the
    /// kernel would store as valid, and seconds past every range.
    day: [[u32; 2]; 512],
}

impl LeanTables {
    fn new(base_day: i64, ingest_adjust: i64, text: impl Fn(i64) -> u32) -> Self {
        // A missing day's year is four 0xFF bytes: a timestamp with them is
        // bad whatever the rest is, and the seconds below put it far outside
        // every window, so the row comes out invalid, as it must.
        let mut day = [[u32::MAX, i32::MAX as u32]; 512];
        for offset in 0..62 {
            let (year, month, date) = civil_from_days(base_day + offset);
            if year <= 9999 {
                day[(month * 32 + date) as usize] = [text(year), (offset * 86_400 + ingest_adjust) as i32 as u32];
            }
        }
        let mut type_step = [0u8; 16];
        type_step[1] = 5;
        type_step[2] = 5;
        type_step[4] = 7;
        type_step[8] = 7;
        Self {
            newline: [b'\n'; 32],
            comma: [b','; 32],
            quote: [b'"'; 32],
            base_ms: (base_day * 86_400 - ingest_adjust) * 1_000,
            type_step,
            ingest_end: [u16::from_le_bytes(*b"Z,"), u16::from_le_bytes(*b"\",")],
            lead36: [14 * POWER36_7, 0, 30 * POWER36_7, 0],
            canonical36: [[POWER36_6, POWER36_7 - 1 - POWER36_6], [POWER36_7, NUMBER_MASK - POWER36_7]],
            day,
        }
    }
}

/// Bytes 4..20 of an F1 timestamp, `-MM-DDTHH:MM:SSZ`, with `0` where a
/// digit belongs, so XOR leaves digit values and zeros.
#[inline(always)]
unsafe fn tail_pattern() -> __m128i {
    _mm_setr_epi8(
        b'-' as i8, b'0' as i8, b'0' as i8, b'-' as i8, b'0' as i8, b'0' as i8, b'T' as i8, b'0' as i8, b'0' as i8,
        b':' as i8, b'0' as i8, b'0' as i8, b':' as i8, b'0' as i8, b'0' as i8, b'Z' as i8,
    )
}

/// Largest allowed value after the XOR: 9 for a digit, 0 for the rest.
#[inline(always)]
unsafe fn tail_limit() -> __m128i {
    _mm_setr_epi8(0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0)
}

/// Month, day, hour, minute, second digit pairs.
#[inline(always)]
unsafe fn tail_shuffle() -> __m128i {
    _mm_setr_epi8(1, 2, 4, 5, 7, 8, 10, 11, 13, 14, Z, Z, Z, Z, Z, Z)
}

/// Bytes 4..20 of two timestamps at once. For each: `month * 32 + day` in
/// the low half and the second of the day in the high half of a word, and 16
/// bits of a mask (the first timestamp's in the low half): bit `i` is set
/// when byte `4 + i` is what F1 has there (a digit, or that very separator,
/// or the `Z`) and belongs to no number over its bound (month 12, day 31,
/// hour 23, minute 59, second 59). All 16 set: the 16 bytes are the end of
/// an F1 timestamp, up to whether that day exists. Bits 0..15 set: the same
/// up to the byte after the seconds.
#[inline(always)]
unsafe fn iso_tails(a: *const u8, b: *const u8) -> (u64, u64, u32) {
    let text = _mm256_inserti128_si256(
        _mm256_castsi128_si256(_mm_loadu_si128(a.add(4) as *const __m128i)),
        _mm_loadu_si128(b.add(4) as *const __m128i),
        1,
    );
    let twice = |lane: __m128i| _mm256_broadcastsi128_si256(lane);
    let values = _mm256_xor_si256(text, twice(tail_pattern()));
    let shaped = _mm256_cmpeq_epi8(_mm256_min_epu8(values, twice(tail_limit())), values);
    let numbers = _mm256_maddubs_epi16(_mm256_shuffle_epi8(values, twice(tail_shuffle())), _mm256_set1_epi16(0x010A));
    let over = _mm256_cmpgt_epi16(numbers, twice(iso_ceilings()));
    let good = _mm256_movemask_epi8(_mm256_andnot_si256(over, shaped)) as u32;
    let sums = _mm256_madd_epi16(numbers, twice(iso_weights()));
    // Dword 1 gains the second (dword 2); dword 0 gains dword 3, which is zero.
    let folded = _mm256_add_epi32(sums, _mm256_shuffle_epi32(sums, 0b11_11_10_11));
    let low = _mm_cvtsi128_si64(_mm256_castsi256_si128(folded)) as u64;
    let high = _mm_cvtsi128_si64(_mm256_extracti128_si256(folded, 1)) as u64;
    (low, high, good)
}

/// The number written as the 13 bytes at `p` (3 bytes before them must be
/// readable), and a mask that is 0xFFFF when all 13 are ASCII digits.
#[inline(always)]
unsafe fn digits13(p: *const u8) -> (i64, u32) {
    let text = _mm_loadu_si128(p.sub(3) as *const __m128i);
    let keep = _mm_setr_epi8(0, 0, 0, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1);
    let values = _mm_and_si128(_mm_xor_si128(text, _mm_set1_epi8(0x30)), keep);
    let digits = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(values, _mm_set1_epi8(9)), values)) as u32;
    let twos = _mm_maddubs_epi16(values, _mm_set1_epi16(0x010A));
    let fours = _mm_madd_epi16(twos, _mm_set1_epi32(0x0001_0064));
    let packed = _mm_packus_epi32(fours, fours);
    let eights = _mm_madd_epi16(packed, _mm_set1_epi32(0x0001_2710));
    let both = _mm_cvtsi128_si64(eights) as u64;
    (((both & 0xFFFF_FFFF) * 100_000_000 + (both >> 32)) as i64, digits)
}

// What follows the ingest_ts of a regular row, from its comma on.
const TYPE_VIEW: u64 = u64::from_le_bytes(*b",view,\0\0");
const TYPE_CART: u64 = u64::from_le_bytes(*b",cart,\0\0");
const TYPE_VIEW_QUOTED: u64 = u64::from_le_bytes(*b",\"view\",");
const TYPE_CART_QUOTED: u64 = u64::from_le_bytes(*b",\"cart\",");
/// The six bytes of the unquoted forms.
const TYPE_SHORT: u64 = 0x0000_FFFF_FFFF_FFFF;
const TYPE_PURCHASE: u64 = u64::from_le_bytes(*b",purchas");
const TYPE_PURCHASE_QUOTED: u64 = u64::from_le_bytes(*b",\"purcha");

/// 36^6 and 36^7: the smallest numbers written with 7 and with 8 characters.
const POWER36_6: u64 = 2_176_782_336;
const POWER36_7: u64 = 78_364_164_096;
/// `value`, unknown to the optimizer from here on. No instruction: the value
/// just has to sit in a register. Used where a test must stay one test.
#[inline(always)]
fn opaque(mut value: u64) -> u64 {
    // SAFETY: an empty block that claims only to read and write one register.
    unsafe {
        std::arch::asm!("/* {0} */", inout(reg) value, options(pure, nomem, nostack, preserves_flags));
    }
    value
}

/// What the first half of the lean kernel leaves for the second half.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct LeanFront {
    /// Nonzero: the row is not regular (found by the first half's own checks).
    bad: u64,
    /// 64 plus where bytes 64..96 have their first newline: the row's
    /// length, nearly always, and known long before the true length is.
    pub guess: usize,
    /// The event's code; in index mode its index.
    event: u64,
    /// The user's code; in index mode the user number.
    user: u64,
    /// Both timestamps as `iso_tails` returns them.
    ts_packed: u64,
    ingest_packed: u64,
    good: u32,
    /// ts as 13 digits, and whether it is that (`digits13`).
    digit_mask: u32,
    digits: i64,
    ts_length: u32,
    /// Masks of the 32 bytes from the comma after `view` or `cart`.
    region_newlines: u32,
    region_commas: u32,
    region_quotes: u32,
    /// From the comma before the type to the one after it; 0 for no `view` or `cart`.
    step: usize,
    ts_at: *const u8,
    ingest_at: *const u8,
    /// The comma after ingest_ts.
    type_at: *const u8,
}

impl LeanFront {
    /// Diagnostic: everything the first half computed, folded into a word.
    #[inline(always)]
    pub fn fold(&self) -> u64 {
        self.bad
            ^ self.event
            ^ self.user
            ^ self.ts_packed
            ^ self.ingest_packed
            ^ u64::from(self.good)
            ^ u64::from(self.digit_mask)
            ^ self.digits as u64
            ^ u64::from(self.ts_length)
            ^ u64::from(self.region_newlines)
            ^ u64::from(self.region_commas)
            ^ u64::from(self.region_quotes)
            ^ self.step as u64
    }
}

/// The first half of the lean kernel on the row at `p`: everything that
/// takes long to arrive (the row's masks, the bytes of its fields through
/// the vector units), with no branch and no lookup that depends on the
/// window. It reads up to 132 bytes from `p` whatever is there, so it may run
/// on a place that turns out not to be a row's start; its result is then
/// simply not used.
#[inline(always)]
pub unsafe fn event_lean_front<const INDEXED: bool>(ctx: &Ctx, lean: *const LeanTables, p: *const u8, front: *mut LeanFront) {
    let comma = _mm256_load_si256(std::ptr::addr_of!((*lean).comma) as *const __m256i);
    let quote = _mm256_load_si256(std::ptr::addr_of!((*lean).quote) as *const __m256i);
    let newline = _mm256_load_si256(std::ptr::addr_of!((*lean).newline) as *const __m256i);
    let block0 = _mm256_loadu_si256(p as *const __m256i);
    let block1 = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let comma_low = mask(block0, comma) | (mask(block1, comma) << 32);
    let quote_low = mask(block0, quote) | (mask(block1, quote) << 32);
    // 32 when there is no newline there, which is right for a row of 96 bytes.
    let guess = 64 + (mask(_mm256_loadu_si256(p.add(64) as *const __m256i), newline) as u32).trailing_zeros() as usize;

    // Which of the first four fields open with a quote (bit `i`: field `i`),
    // and which of the first three have one right before their comma. Fields
    // 0 to 2 are quoted at both ends or at neither.
    let opened = _pext_u64(quote_low, (comma_low << 1) | 1);
    let before = _pext_u64(quote_low << 1, comma_low);
    let mut bad = (opened ^ before) & 7;
    // The first three commas. If the first 64 bytes have fewer, a position
    // is 64, and the lengths below are not those of a regular row.
    let c0 = comma_low.trailing_zeros() as usize;
    let rest = _blsr_u64(comma_low);
    let c1 = rest.trailing_zeros() as usize;
    let c2 = _blsr_u64(rest).trailing_zeros() as usize;
    let q0 = (opened & 1) as usize;
    let q1 = ((opened >> 1) & 1) as usize;
    let q2 = ((opened >> 2) & 1) as usize;
    let q3 = ((opened >> 3) & 1) as usize;

    // Both IDs: the prefix and 7 or 8 characters (`*_extra` is 0 or 1).
    let event_extra = c0.wrapping_sub(2 * q0 + 8);
    let user_extra = c1.wrapping_sub(c0 + 2 * q1 + 9);
    let user_at = p.add(c0 + q1 + 1);
    bad |= ((event_extra | user_extra) >> 1) as u64;
    bad |= u64::from(*p.add(q0) ^ b'e') | u64::from(*user_at ^ b'u');
    let (event, user) = if INDEXED {
        // Index mode: both IDs must be generated ones. The eight bytes that
        // end an ID are its 8 characters, or its prefix and 7 characters
        // (the prefix is a letter, so all eight are characters either way).
        // Read as one base-36 number, less the prefix's part, they give the
        // ID's number; the text is canonical (no leading zero, below 2^40)
        // exactly when the number is in the range of its length. (Where the
        // lengths are not regular, `bad` is set already; the bytes read are
        // then kept inside the row's first 64.)
        let keys = ctx.keys.as_ref().unwrap_unchecked();
        let event_end = c0.wrapping_sub(q0 + 8);
        let user_end = c1.wrapping_sub(q1 + 8);
        let (event_number, user_number, known) = two_numbers36(
            read_u64(p.add(select_unpredictable(event_end > 56, 0, event_end))),
            read_u64(p.add(select_unpredictable(user_end > 56, 0, user_end))),
        );
        bad |= u64::from(known ^ 0xFFFF);
        let (event_extra, user_extra) = (event_extra & 1, user_extra & 1);
        let event_number = event_number.wrapping_sub(*(*lean).lead36.get_unchecked(event_extra));
        let user_number = user_number.wrapping_sub(*(*lean).lead36.get_unchecked(2 + user_extra));
        let canonical = std::ptr::addr_of!((*lean).canonical36) as *const [u64; 2];
        let event_range = *canonical.add(event_extra);
        let user_range = *canonical.add(user_extra);
        bad |= u64::from(event_number.wrapping_sub(event_range[0]) > event_range[1]);
        bad |= u64::from(user_number.wrapping_sub(user_range[0]) > user_range[1]);
        let index = unscramble(event_number, keys.event_key);
        let number = unscramble(user_number, keys.user_key);
        bad |= u64::from(index >= keys.index_bound) | u64::from((number >> 9) >= keys.user_bound);
        (index, number)
    } else {
        // The eight bytes after the prefix are the characters, or seven of
        // them and the quote or comma that ends the field, which counts as
        // no character.
        let (event_code, user_code, known) = two_codes(read_u64(p.add(q0 + 1)), read_u64(user_at.add(1)));
        let need = 0x7F7F | ((event_extra as u32 & 1) << 7) | ((user_extra as u32 & 1) << 15);
        bad |= u64::from(known ^ need);
        (event_code, user_code)
    };

    // Both timestamps; ingest_ts must end with `Z,`, or with `",` when it
    // opened with a quote.
    let ts_at = p.add(c1 + q2 + 1);
    let ts_length = c2.wrapping_sub(c1 + 2 * q2 + 1) as u32;
    let ingest_at = p.add(c2 + q3 + 1);
    let type_at = ingest_at.add(20 + q3);
    let (ts_packed, ingest_packed, good) = iso_tails(ts_at, ingest_at);
    let (digits, digit_mask) = digits13(ts_at);
    bad |= u64::from((type_at.sub(1) as *const u16).read_unaligned() ^ *(*lean).ingest_end.get_unchecked(q3));

    // The event type with its commas and quotes, as one of four texts, and
    // the masks of the 32 bytes from its last comma on.
    let lanes = _mm256_set1_epi64x(read_u64(type_at) as i64);
    let masks = _mm256_setr_epi64x(TYPE_SHORT as i64, TYPE_SHORT as i64, -1, -1);
    let texts = _mm256_setr_epi64x(
        TYPE_VIEW as i64,
        TYPE_CART as i64,
        TYPE_VIEW_QUOTED as i64,
        TYPE_CART_QUOTED as i64,
    );
    let hits = _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(_mm256_and_si256(lanes, masks), texts)))
        as usize;
    let step = usize::from(*(*lean).type_step.get_unchecked(hits));
    let region = _mm256_loadu_si256(type_at.add(step) as *const __m256i);
    front.write(LeanFront {
        bad,
        guess,
        event,
        user,
        ts_packed,
        ingest_packed,
        good,
        digit_mask,
        digits,
        ts_length,
        region_newlines: mask(region, newline) as u32,
        region_commas: mask(region, comma) as u32,
        region_quotes: mask(region, quote) as u32,
        step,
        ts_at,
        ingest_at,
        type_at,
    });
}

/// The second half of the lean kernel: the row at `p`, whose first half is
/// `front`. Returns the row's length and `ROW_DONE` (stored), `ROW_EMIT` (in
/// `row`), `ROW_DROP` (a diagnostic mode: parsed, not stored) or
/// `ROW_DECLINE` (nothing was done; the length means nothing).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub unsafe fn event_lean_back<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    lean: *const LeanTables,
    window_length: u64,
    p: *const u8,
    front: *const LeanFront,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> (usize, u32) {
    const DECLINE: (usize, u32) = (0, ROW_DECLINE);
    let front = &*front;
    if front.bad != 0 {
        return DECLINE;
    }
    // ingest_ts must be F1 on a table day.
    let days = std::ptr::addr_of!((*lean).day) as *const [u32; 2];
    let ingest_day = *days.add((front.ingest_packed & 511) as usize);
    if front.good < 0xFFFF_0000 {
        return DECLINE;
    }
    if ingest_day[0] != (front.ingest_at as *const u32).read_unaligned() {
        return DECLINE;
    }
    let ingest_seconds = i64::from(ingest_day[1] as i32) + (front.ingest_packed >> 32) as i64;
    if ingest_seconds as u64 >= 1 << 21 {
        return DECLINE;
    }
    let base_ms = (*lean).base_ms;
    let ingest_ms = base_ms + ingest_seconds * 1_000;

    // ts: which of the three forms a row has is close to random, so the
    // 13-digit and the F1 reading are both there and one is picked without a
    // branch; the one in ten that is F2 takes a branch.
    let ts_at = front.ts_at;
    let ts_length = front.ts_length;
    let good = front.good;
    let ts_day = *days.add((front.ts_packed & 511) as usize);
    let ts_year = (ts_at as *const u32).read_unaligned();
    let iso_ms = base_ms + (i64::from(ts_day[1] as i32) + (front.ts_packed >> 32) as i64) * 1_000;
    let ts_ms = if ts_length == 25 {
        // F2: the same 19 bytes, then the offset.
        let offset = iso_offset(ts_at.add(19));
        if good & 0x7FFF != 0x7FFF || ts_day[0] != ts_year || offset == NOT_FAST {
            return DECLINE;
        }
        iso_ms.wrapping_sub(offset.wrapping_mul(1_000))
    } else {
        // Zero when the field is 13 digits; zero when it is F1 on a table day.
        let not_digits = (front.digit_mask ^ 0xFFFF) | (ts_length ^ 13);
        let not_plain = ((good & 0xFFFF) ^ 0xFFFF) | (ts_length ^ 20) | (ts_day[0] ^ ts_year);
        // One test for "neither". The value goes through `opaque` so that
        // the compiler cannot turn it into two branches, the first of which
        // would be on which of the two forms the row has.
        if opaque(u64::from(select_unpredictable(not_digits == 0, 0, not_plain))) != 0 {
            return DECLINE;
        }
        select_unpredictable(not_digits == 0, front.digits, iso_ms)
    };
    let ts = ts_ms.wrapping_sub(ctx.window_start) as u64;
    let mut valid = (ts < window_length) & (ingest_ms.wrapping_sub(ts_ms) <= LATE_MS);

    // After `view` or `cart` the first half has the masks of the rest of the
    // row; a purchase is the rare other case and gets them here.
    let type_at = front.type_at;
    let mut step = front.step;
    let mut newlines = front.region_newlines;
    let mut commas = front.region_commas;
    let mut quotes = front.region_quotes;
    let is_purchase = step == 0;
    if is_purchase {
        // `,purchase,` or `,"purchase",`.
        let head = read_u64(type_at);
        let plain = (head == TYPE_PURCHASE) & ((type_at.add(8) as *const u16).read_unaligned() == 0x2C65);
        let quoted =
            (head == TYPE_PURCHASE_QUOTED) & ((type_at.add(8) as *const u32).read_unaligned() == 0x2C22_6573);
        if !(plain | quoted) {
            return DECLINE;
        }
        step = 9 + 2 * usize::from(quoted);
        let region = _mm256_loadu_si256(type_at.add(step) as *const __m256i);
        newlines = mask(region, _mm256_set1_epi8(b'\n' as i8)) as u32;
        commas = mask(region, _mm256_set1_epi8(b',' as i8)) as u32;
        quotes = mask(region, _mm256_set1_epi8(b'"' as i8)) as u32;
    }

    // The 32 bytes from the comma after the type: the product, one more
    // comma, the price and the newline, which bit `reach` stands for. Every
    // byte before this comma was matched and none is a newline, so this
    // newline ends the row.
    if newlines == 0 {
        return DECLINE;
    }
    let reach = newlines.trailing_zeros();
    let region_commas = _bzhi_u32(commas, reach);
    let region_quotes = _bzhi_u32(quotes, reach);
    // Without the type's comma (bit 0): exactly one comma.
    let last = _blsr_u32(region_commas);
    if last == 0 || _blsr_u32(last) != 0 {
        return DECLINE;
    }
    let r5 = last.trailing_zeros() as usize;
    let separators = last | (1 << reach);
    let starts = (separators << 1) | 2;
    let finishes = separators >> 1;
    // A quote must be at the start or the end of a field, never at both (a
    // field that is one quote), and a field has both or neither.
    let opened_late = _pext_u32(region_quotes, starts);
    let closed_late = _pext_u32(region_quotes, finishes);
    let q5 = (opened_late & 1) as usize;
    if region_quotes & !(starts ^ finishes) != 0 {
        return DECLINE;
    }
    if opened_late != closed_late {
        return DECLINE;
    }
    // An empty product makes the row invalid: left to the general kernel.
    if r5 == 1 + 2 * q5 {
        return DECLINE;
    }
    let region_at = type_at.add(step);
    let length = region_at.offset_from(p) as usize + reach as usize;

    if is_purchase {
        let mut purchase = None;
        if valid {
            let q6 = (opened_late >> 1) as usize;
            let price = std::slice::from_raw_parts(region_at.add(r5 + 1 + q6), reach as usize - r5 - 1 - 2 * q6);
            match parse_price(price) {
                Some(price) => {
                    purchase = Some((price, product_category(ctx, region_at.add(1 + q5), r5 - 1 - 2 * q5)))
                }
                None => valid = false,
            }
        }
        let status =
            event_lean_finish::<MODE, INDEXED>(out, row, front.event, front.user, ts, valid, ingest_seconds, purchase);
        return (length, status);
    }
    (length, event_lean_finish::<MODE, INDEXED>(out, row, front.event, front.user, ts, valid, ingest_seconds, None))
}

/// Stores (or hands back) what the lean kernel read.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn event_lean_finish<const MODE: u32, const INDEXED: bool>(
    out: &mut ParseOut,
    row: &mut EventRow,
    event_code: u64,
    user_code: u64,
    ts: u64,
    valid: bool,
    ingest_seconds: i64,
    purchase: Option<(u32, u32)>,
) -> u32 {
    // An invalid row keeps neither its time nor its user.
    let keep = u64::from(valid).wrapping_neg();
    let ts = ts & keep;
    let user_code = user_code & keep;
    // Index mode: the user of a valid row is its user number.
    let user_is_number = INDEXED & valid;
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
        return ROW_DONE;
    }
    if MODE == MODE_ROW {
        *row = EventRow {
            event: event_code,
            user: user_code,
            user_is_number,
            ts: ts as u32,
            ingest: ingest_seconds as u32,
            valid,
            purchase,
        };
        return ROW_EMIT;
    }
    // Diagnostic: parsed, not stored.
    row.event ^= event_code ^ user_code ^ ts ^ (ingest_seconds as u64);
    ROW_DROP
}

/// Bytes past a row's start that must be readable for the fast loop to run
/// the lean kernel there: the first half is also run where the next row is
/// expected to start (at most 97 bytes on) and reads 132 bytes from there.
pub const LEAN_PAD: usize = 320;

/// The tables of the lean kernel and the window's length, for its halves.
#[inline(always)]
pub fn lean_setup(ctx: &Ctx) -> (*const LeanTables, u64) {
    (&ctx.ts_tables.lean, (ctx.window_end - ctx.window_start) as u64)
}

/// Diagnostic: only the masks of the row at `p`, folded, and the guess of its length.
#[inline(always)]
pub unsafe fn event_lean_masks(lean: *const LeanTables, p: *const u8) -> (u64, usize) {
    let comma = _mm256_load_si256(std::ptr::addr_of!((*lean).comma) as *const __m256i);
    let quote = _mm256_load_si256(std::ptr::addr_of!((*lean).quote) as *const __m256i);
    let newline = _mm256_load_si256(std::ptr::addr_of!((*lean).newline) as *const __m256i);
    let block0 = _mm256_loadu_si256(p as *const __m256i);
    let block1 = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let comma_low = mask(block0, comma) | (mask(block1, comma) << 32);
    let quote_low = mask(block0, quote) | (mask(block1, quote) << 32);
    let guess = 64 + (mask(_mm256_loadu_si256(p.add(64) as *const __m256i), newline) as u32).trailing_zeros() as usize;
    (comma_low ^ quote_low, guess)
}

#[cfg(test)]
thread_local! {
    /// Rows the lean event kernel declined on this thread.
    static DECLINED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// As `event_row_fast`, with the lean kernel in front of it: the same
/// arguments, the same answers.
#[inline(always)]
pub unsafe fn event_row_lean<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    p: *const u8,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> (usize, u32) {
    let (lean, window_length) = lean_setup(ctx);
    let mut front = std::mem::MaybeUninit::<LeanFront>::uninit();
    event_lean_front::<INDEXED>(ctx, lean, p, front.as_mut_ptr());
    let (length, status) =
        event_lean_back::<MODE, INDEXED>(ctx, lean, window_length, p, front.as_ptr(), out, row);
    if status == ROW_DECLINE {
        #[cfg(test)]
        DECLINED.with(|count| count.set(count.get() + 1));
        return event_row_fast::<MODE, INDEXED>(ctx, p, out, row);
    }
    (length, status)
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

// Every fast kernel against the general parsers, line by line: named probes
// for each range and shape check a kernel makes, then random damage.
#[cfg(test)]
mod tests {
    use super::super::keys::{scramble, Keys};
    use super::super::parse::{click_row_slow, event_row_slow, event_row_slow_indexed, SlowIndexed};
    use super::*;

    const PRODUCTS: &str = "product_id,category\np1,shoes\np2,books\np10,toys\np12345,games\np50000,games\n\
        p007,odd\nP9,upper\npx,letters\np4194303,edge\np4194304,past\np9999999,seven\np10000000,eight\n\"p77\",quoted\n";
    const CAMPAIGNS: &str = "campaign_id,channel\nc_1,search\nc_2,social\nc_10,email\nc_2000,display\nc_007,odd\n\
        C_9,upper\ncamp,letters\nc_1048575,edge\nc_1048576,past\nc_9999999,seven\nc_10000000,eight\n\"c_77\",quoted\n";

    /// What may follow a row's newline: the kernels read past it.
    const TAILS: [&[u8]; 5] = [
        b"e9zzzzzzz,u9zzzzzzz,2026-10-03T23:58:10Z,2026-10-03T23:59:10Z,view,p1,\n",
        b"\"\"\"\"\"\"\"\"",
        b",,,,,,,,",
        b"\n\n\n\n",
        b"0123456789",
    ];

    const WINDOWS: [(&str, &str, &str); 5] = [
        ("october", "2026-10-01T00:00:00Z", "2026-10-15T00:00:00Z"),
        ("new-year", "2027-12-25T00:00:00Z", "2028-01-08T00:00:00Z"),
        ("leap-day", "2028-02-20T00:00:00Z", "2028-03-05T00:00:00Z"),
        ("epoch", "1970-01-10T00:00:00Z", "1970-01-20T00:00:00Z"),
        ("odd-start", "1791071890123", "1791671890123"),
    ];

    fn f1(milliseconds: i64) -> String {
        let (year, month, day) = civil_from_days(milliseconds.div_euclid(DAY_MS));
        let second = milliseconds.rem_euclid(DAY_MS) / 1_000;
        format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", second / 3_600, second / 60 % 60, second % 60)
    }

    fn base36(mut value: u64) -> String {
        let mut digits = Vec::new();
        loop {
            digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(value % 36) as usize]);
            value /= 36;
            if value == 0 {
                break;
            }
        }
        digits.reverse();
        String::from_utf8(digits).unwrap()
    }

    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    /// Runs lines through every kernel and compares with the general parser.
    struct Checker {
        ctx: Ctx,
        keys: Keys,
        out: ParseOut,
        buffer: Vec<u8>,
        lines: usize,
        /// Lines each kernel answered itself: general, lean, indexed general,
        /// indexed lean, click.
        answered: [usize; 5],
        /// Generated IDs and good times, for the probes to build rows from.
        event_id: String,
        user_id: String,
        ts: String,
        ingest: String,
        /// The same time as `ts`, as 13 digits.
        digits: String,
    }

    impl Checker {
        fn new(window: usize) -> Self {
            let (tag, window_start, window_end) = WINDOWS[window];
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!("ss-simd-{}-{tag}-{number}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let manifest = format!("{{\"window_start\": \"{window_start}\", \"window_end\": \"{window_end}\"}}");
            std::fs::write(directory.join("manifest.json"), manifest).unwrap();
            std::fs::write(directory.join("products.csv"), PRODUCTS).unwrap();
            std::fs::write(directory.join("campaigns.csv"), CAMPAIGNS).unwrap();
            let mut ctx = Ctx::load(&directory).unwrap().expect("a window the engine takes");
            let _ = std::fs::remove_dir_all(&directory);
            let keys =
                Keys { event_key: 0xD1_40AB_A0D1, user_key: 0x0E_8948_5787, index_bound: 3 << 20, user_bound: 1 << 17, index_shift: 14 };
            ctx.keys = Some(keys);
            let out = ParseOut::new(&ctx);
            // Generated IDs of 8 characters.
            let event_id = (1_000..).map(|index| base36(scramble(index, keys.event_key))).find(|id| id.len() == 8);
            let user_id = (700..).map(|user| base36(scramble(user << 9 | 3, keys.user_key))).find(|id| id.len() == 8);
            let ts_ms = (ctx.window_start + DAY_MS + 3_723_000).div_euclid(1_000) * 1_000;
            let ts = f1(ts_ms);
            let ingest = f1((ctx.window_start + DAY_MS + 3_900_000).div_euclid(1_000) * 1_000);
            Self {
                ctx,
                keys,
                out,
                buffer: Vec::new(),
                lines: 0,
                answered: [0; 5],
                event_id: format!("e{}", event_id.unwrap()),
                user_id: format!("u{}", user_id.unwrap()),
                ts,
                ingest,
                digits: format!("{ts_ms:013}"),
            }
        }

        /// `line`, then a newline, then enough of a tail, with a newline in front.
        fn load(&mut self, line: &[u8], tail: &[u8]) -> *const u8 {
            self.buffer.clear();
            self.buffer.push(b'\n');
            self.buffer.extend_from_slice(line);
            self.buffer.push(b'\n');
            while self.buffer.len() < line.len() + 2 * ROW_PAD {
                self.buffer.extend_from_slice(tail);
            }
            // SAFETY: the buffer is longer than one byte.
            unsafe { self.buffer.as_ptr().add(1) }
        }

        fn event_with(&mut self, line: &[u8], tail: &[u8]) {
            let p = self.load(line, tail);
            let shown = String::from_utf8_lossy(line).into_owned();
            let slow = event_row_slow(&self.ctx, line);
            let slow_indexed = event_row_slow_indexed(&self.ctx, &self.keys, line);
            for kernel in 0..4 {
                let mut row = EventRow::default();
                // SAFETY: `load` left ROW_PAD readable bytes after `p`.
                let (length, status) = unsafe {
                    match kernel {
                        0 => event_row_fast::<MODE_ROW, false>(&self.ctx, p, &mut self.out, &mut row),
                        1 => event_row_lean::<MODE_ROW, false>(&self.ctx, p, &mut self.out, &mut row),
                        2 => event_row_fast::<MODE_ROW, true>(&self.ctx, p, &mut self.out, &mut row),
                        _ => event_row_lean::<MODE_ROW, true>(&self.ctx, p, &mut self.out, &mut row),
                    }
                };
                if status != ROW_LONG {
                    assert_eq!(length, line.len(), "kernel {kernel}: length of {shown:?}");
                }
                match (status, kernel < 2) {
                    (ROW_EMIT, true) => assert_eq!(Some(row), slow, "kernel {kernel} on {shown:?}"),
                    (ROW_DROP, true) => assert_eq!(None, slow, "kernel {kernel} on {shown:?}"),
                    (ROW_EMIT, false) => {
                        assert_eq!(SlowIndexed::Indexed(row), slow_indexed, "kernel {kernel} on {shown:?}")
                    }
                    (ROW_DROP, false) => assert_eq!(SlowIndexed::Drop, slow_indexed, "kernel {kernel} on {shown:?}"),
                    (ROW_SLOW | ROW_LONG, _) | (ROW_GENERIC, false) => continue,
                    _ => panic!("kernel {kernel}: status {status} on {shown:?}"),
                }
                self.answered[kernel] += 1;
            }
        }

        fn click_with(&mut self, line: &[u8], tail: &[u8]) {
            let p = self.load(line, tail);
            let shown = String::from_utf8_lossy(line).into_owned();
            let slow = click_row_slow(&self.ctx, line);
            let mut click = ClickRecord { user: 0, code_and_campaign: 0, ts: 0 };
            // SAFETY: `load` left ROW_PAD readable bytes after `p`.
            let (length, status) = unsafe { click_row_fast(&self.ctx, p, &mut click) };
            if status != ROW_LONG {
                assert_eq!(length, line.len(), "click: length of {shown:?}");
            }
            match status {
                ROW_EMIT => assert_eq!(Some(click), slow, "click {shown:?}"),
                ROW_DROP => assert_eq!(None, slow, "click {shown:?}"),
                ROW_SLOW | ROW_LONG => return,
                _ => panic!("click: status {status} on {shown:?}"),
            }
            self.answered[4] += 1;
        }

        /// One tail per line, in turn.
        fn event(&mut self, line: &[u8]) {
            if !line.contains(&b'\n') {
                self.lines += 1;
                self.event_with(line, TAILS[self.lines % TAILS.len()]);
            }
        }

        fn click(&mut self, line: &[u8]) {
            if !line.contains(&b'\n') {
                self.lines += 1;
                self.click_with(line, TAILS[self.lines % TAILS.len()]);
            }
        }

        /// Every tail.
        fn event_all(&mut self, line: &[u8]) {
            if !line.contains(&b'\n') {
                for tail in TAILS {
                    self.event_with(line, tail);
                }
            }
        }

        fn click_all(&mut self, line: &[u8]) {
            if !line.contains(&b'\n') {
                for tail in TAILS {
                    self.click_with(line, tail);
                }
            }
        }

        /// A view with these times, long enough for the fast kernels (the
        /// price of a view is ignored, so it pads the row).
        fn view(&self, ts: &str, ingest: &str) -> Vec<u8> {
            let mut line = format!("{},{},{ts},{ingest},view,p12345,", self.event_id, self.user_id).into_bytes();
            while line.len() < 66 {
                line.push(b'7');
            }
            line
        }

        fn purchase(&self, ts: &str, product: &str, price: &str) -> Vec<u8> {
            format!("{},{},{ts},{},purchase,{product},{price}", self.event_id, self.user_id, self.ingest).into_bytes()
        }

        fn click_line(&self, campaign: &str, ts: &str) -> Vec<u8> {
            format!("k{},{},{campaign},{ts}", &self.event_id[1..], self.user_id).into_bytes()
        }

        /// `ts` as an event's and a click's timestamp, and as an ingest time.
        fn times(&mut self, ts: &str) {
            let ingest = self.ingest.clone();
            let good = self.ts.clone();
            self.event(&self.view(ts, &ingest));
            self.event(&self.view(&good, ts));
            self.click(&self.click_line("c_1", ts));
        }
    }

    /// The years around the window, and some that are no years.
    fn years(check: &Checker) -> Vec<String> {
        let (year, _, _) = civil_from_days(check.ctx.window_start.div_euclid(DAY_MS));
        let mut years: Vec<String> = (year - 1..=year + 1).map(|year| format!("{year:04}")).collect();
        years.extend(["0000", "9999", "2O26", "20 6", "-026", "+026"].map(String::from));
        years
    }

    fn for_each_window(mut probe: impl FnMut(&mut Checker)) {
        for window in 0..WINDOWS.len() {
            let mut check = Checker::new(window);
            probe(&mut check);
            // The probes must reach the kernels, not only the general parser.
            assert!(check.lines > 0);
        }
    }

    #[test]
    fn every_month_and_day() {
        for_each_window(|check| {
            for year in years(check) {
                for month in 0..100 {
                    for day in 0..100 {
                        check.times(&format!("{year}-{month:02}-{day:02}T12:34:56Z"));
                        if month <= 13 && (day <= 1 || day >= 27) {
                            check.times(&format!("{year}-{month:02}-{day:02}T00:00:00+00:00"));
                            check.times(&format!("{year}-{month:02}-{day:02}T23:59:59-10:00"));
                            check.times(&format!("{year}-{month:02}-{day:02}T00:00:00+12:00"));
                        }
                    }
                }
            }
            assert!(check.answered.iter().all(|&count| count > 1_000), "{:?}", check.answered);
        });
    }

    #[test]
    fn every_hour_minute_second_and_offset() {
        for_each_window(|check| {
            let day = check.ts[..10].to_string();
            for value in 0..100 {
                for zone in ["Z", "+00:00", "-04:00", "+05:30"] {
                    check.times(&format!("{day}T{value:02}:34:56{zone}"));
                    check.times(&format!("{day}T12:{value:02}:56{zone}"));
                    check.times(&format!("{day}T12:34:{value:02}{zone}"));
                }
            }
            // Every offset, with every sign that is one and some that are not.
            for sign in ["+", "-", " ", "Z", "*", ".", ":", "0", "T", "\"", "\u{2212}"] {
                for hours in 0..100 {
                    for minutes in 0..100 {
                        check.times(&format!("{day}T12:34:56{sign}{hours:02}:{minutes:02}"));
                    }
                }
            }
            for text in ["+0200", "+02-00", "+02:0", "+2:00", "+02:000", "+02:00Z", "Z+02:00", "+02.00", "+ 2:00", "+02: 0"] {
                check.times(&format!("{day}T12:34:56{text}"));
            }
            assert!(check.answered.iter().all(|&count| count > 1_000), "{:?}", check.answered);
        });
    }

    #[test]
    fn every_byte_of_a_timestamp() {
        for_each_window(|check| {
            let start = check.ctx.window_start + DAY_MS;
            let f1_text = check.ts.clone();
            let f2_text = format!("{}-04:00", &f1_text[..19]);
            let f3_text = format!("{:013}", start + 3_723_456);
            for text in [f1_text, f2_text, f3_text] {
                for at in 0..text.len() {
                    for byte in 0..=255u8 {
                        let mut bytes = text.clone().into_bytes();
                        bytes[at] = byte;
                        // A line is bytes, not text: build it as such.
                        let ingest = check.ingest.clone();
                        let mut line = format!("{},{},", check.event_id, check.user_id).into_bytes();
                        line.extend_from_slice(&bytes);
                        line.extend_from_slice(format!(",{ingest},view,p12345,7777777777777").as_bytes());
                        check.event(&line);
                        let mut line = format!("{},{},{},", check.event_id, check.user_id, check.ts).into_bytes();
                        line.extend_from_slice(&bytes);
                        line.extend_from_slice(b",view,p12345,7777777777777");
                        check.event(&line);
                        let mut line = format!("k{},{},c_1,", &check.event_id[1..], check.user_id).into_bytes();
                        line.extend_from_slice(&bytes);
                        check.click(&line);
                    }
                }
            }
            assert!(check.answered.iter().all(|&count| count > 1_000), "{:?}", check.answered);
        });
    }

    #[test]
    fn digit_timestamps() {
        for_each_window(|check| {
            let (start, end) = (check.ctx.window_start, check.ctx.window_end);
            let mut values: Vec<String> = Vec::new();
            for value in [
                0,
                1,
                start - WEEK_MS - 1,
                start - WEEK_MS,
                start - 1,
                start,
                start + 1,
                start + DAY_MS,
                end - 1,
                end,
                end + 1,
                i64::MAX,
            ] {
                values.push(value.to_string());
            }
            values.extend(
                ["9223372036854775808", "18446744073709551615", "18446744073709551616", "99999999999999999999999999"]
                    .map(String::from),
            );
            for length in 1..=30 {
                values.push("9".repeat(length));
                values.push("0".repeat(length));
                values.push(format!("1{}", "0".repeat(length - 1)));
            }
            for value in values.clone() {
                // Leading zeros up to every length, and damage of every kind.
                for width in value.len()..=value.len().max(26) {
                    check.times(&format!("{value:0>width$}"));
                }
                for extra in ["+", "-", " ", ".0", "e3", "\r", "x", "Z"] {
                    check.times(&format!("{extra}{value}"));
                    check.times(&format!("{value}{extra}"));
                }
            }
            assert!(check.answered.iter().all(|&count| count > 100), "{:?}", check.answered);
        });
    }

    #[test]
    fn prices_products_and_campaigns() {
        for_each_window(|check| {
            let mut numbers: Vec<String> = Vec::new();
            for value in [
                0u64, 1, 2, 7, 9, 10, 12, 77, 99, 100, 250, 999, 1_000, 2_000, 2_001, 12_345, 50_000, 50_001, 99_999,
                100_000, 999_999, 1_000_000, 1_048_575, 1_048_576, 4_194_303, 4_194_304, 9_999_999, 10_000_000,
                10_000_001, 12_345_678, 99_999_999, 100_000_000, 999_999_999, 4_294_967_295, 4_294_967_296,
                4_294_967_297, 10_000_000_000, 18_446_744_073_709_551_615,
            ] {
                for width in 0..=20 {
                    numbers.push(format!("{value:0>width$}"));
                }
            }
            numbers.extend(
                ["", "-5", "+500", "19.99", "1e3", "1 ", " 1", "1\r", "0x10", "١٢٣", "12a", "a12", "1,2", "\u{ff11}"]
                    .map(String::from),
            );
            let ts = check.digits.clone();
            for number in &numbers {
                for (open, close) in [("", ""), ("\"", "\""), ("\"", ""), ("", "\"")] {
                    // As a price, as a product number and as a campaign number.
                    check.event(&check.purchase(&ts, "p12345", &format!("{open}{number}{close}")));
                    check.event(&check.purchase(&ts, &format!("{open}p{number}{close}"), "1234"));
                    check.event(&check.purchase(&ts, &format!("{open}{number}{close}"), "1234"));
                    check.event(&check.purchase(&ts, &format!("{open}P{number}{close}"), "1234"));
                    check.click(&check.click_line(&format!("{open}c_{number}{close}"), &ts));
                    check.click(&check.click_line(&format!("{open}C_{number}{close}"), &ts));
                    check.click(&check.click_line(&format!("{open}c{number}{close}"), &ts));
                    check.click(&check.click_line(&format!("{open}{number}{close}"), &ts));
                }
            }
            for text in ["px", "camp", "p", "c_", "organic", "unknown", "p 1", "c_ 1", "p1 ", "c_1 ", " p1", " c_1"] {
                check.event(&check.purchase(&ts, text, "1234"));
                check.click(&check.click_line(text, &ts));
            }
            assert!(check.answered.iter().all(|&count| count > 100), "{:?}", check.answered);
        });
    }

    #[test]
    fn ids_and_types() {
        for_each_window(|check| {
            let (ts, ingest) = (check.ts.clone(), check.ingest.clone());
            let keys = check.keys;
            let mut ids: Vec<String> = Vec::new();
            // Generated numbers of every length, in and out of range.
            for index in [0, 1, 35, 36, 1_295, 1_296, 46_655, 3 << 20, (3 << 20) - 1, (1 << 40) - 1] {
                ids.push(base36(scramble(index, keys.event_key)));
                ids.push(base36(scramble(index, keys.user_key)));
                ids.push(base36(index));
            }
            for index in 0..600u64 {
                ids.push(base36(scramble(index * 7_919, keys.event_key)));
                ids.push(base36(scramble((index * 131) << 9 | (index & 7), keys.user_key)));
            }
            let plain = ids.clone();
            for id in plain.iter().take(30) {
                ids.push(format!("0{id}"));
                ids.push(format!("{id}0"));
                ids.push(id.to_uppercase());
                ids.push(format!("{id} "));
                ids.push(format!(" {id}"));
                for at in 0..id.len() {
                    for byte in ["A", "Z", "_", "-", "{", "`", "/", ":", "@", "[", "\u{e9}", "\r", " "] {
                        ids.push(format!("{}{byte}{}", &id[..at], &id[at + 1..]));
                    }
                }
            }
            ids.extend(["", "zzzzzzzz", "zzzzzzzzz", "1000000000", "00000000", "0"].map(String::from));
            for id in &ids {
                for prefix in ["e", "u", "k", "E", "U", "", "x", "ee"] {
                    for (open, close) in [("", ""), ("\"", "\"")] {
                        let id = format!("{open}{prefix}{id}{close}");
                        let mut line = format!("{id},{},{ts},{ingest},view,p12345,", check.user_id).into_bytes();
                        line.resize(line.len().max(70), b'7');
                        check.event(&line);
                        let mut line = format!("{},{id},{ts},{ingest},view,p12345,", check.event_id).into_bytes();
                        line.resize(line.len().max(70), b'7');
                        check.event(&line);
                        check.click(&format!("{id},{},c_1,{ts}", check.user_id).into_bytes());
                        check.click(&format!("k{},{id},c_1,{ts}", &check.event_id[1..]).into_bytes());
                    }
                }
            }
            for kind in [
                "view", "cart", "purchase", "View", "CART", "Purchase", "view ", " cart", "purchases", "viewx", "car",
                "", "vieW", "purchas", "purchase\r", "refund", "v", "viewcart", "cartview", "purchaseX",
            ] {
                for (open, close) in [("", ""), ("\"", "\""), ("\"", ""), ("", "\"")] {
                    for price in ["", "1234", "\"1234\"", "0", "x", "77777777777777777"] {
                        let line = format!(
                            "{},{},{ts},{ingest},{open}{kind}{close},p12345,{price}",
                            check.event_id, check.user_id
                        );
                        check.event(line.as_bytes());
                    }
                }
            }
            assert!(check.answered.iter().all(|&count| count > 100), "{:?}", check.answered);
        });
    }

    /// Rows of every regular kind, for the damage tests.
    fn regular_rows(check: &Checker) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let start = check.ctx.window_start + DAY_MS;
        let ts_forms = [
            check.ts.clone(),
            format!("{}-04:00", &check.ts[..19]),
            format!("{}+05:30", &check.ts[..19]),
            format!("{:013}", start + 3_723_456),
        ];
        let short_event = (2_000..).map(|index| base36(scramble(index, check.keys.event_key))).find(|id| id.len() == 7);
        let short_user =
            (900..).map(|user| base36(scramble(user << 9, check.keys.user_key))).find(|id| id.len() == 7).unwrap();
        let short_event = short_event.unwrap();
        let mut events = Vec::new();
        let mut clicks = Vec::new();
        for ts in &ts_forms {
            for (event, user) in [
                (check.event_id.clone(), check.user_id.clone()),
                (format!("e{short_event}"), format!("u{short_user}")),
            ] {
                for (kind, product, price) in [
                    ("view", "p12345", ""),
                    ("cart", "p2", "77777"),
                    ("view", "p50000", "+500"),
                    ("purchase", "p12345", "3216"),
                    ("purchase", "p1", "0250"),
                    ("purchase", "p777", "10000000"),
                ] {
                    let fields = [&event[..], &user[..], &ts[..], &check.ingest[..], kind, product, price];
                    // Every choice of quoted fields.
                    for quoted in 0..128 {
                        let mut line = Vec::new();
                        for (at, field) in fields.iter().enumerate() {
                            if at > 0 {
                                line.push(b',');
                            }
                            if quoted >> at & 1 == 1 {
                                line.push(b'"');
                                line.extend_from_slice(field.as_bytes());
                                line.push(b'"');
                            } else {
                                line.extend_from_slice(field.as_bytes());
                            }
                        }
                        events.push(line);
                    }
                }
                for campaign in ["c_1", "c_2000", "c_10"] {
                    let fields = [&format!("k{}", &event[1..])[..], &user[..], campaign, &ts[..]];
                    for quoted in 0..16 {
                        let mut line = Vec::new();
                        for (at, field) in fields.iter().enumerate() {
                            if at > 0 {
                                line.push(b',');
                            }
                            if quoted >> at & 1 == 1 {
                                line.push(b'"');
                                line.extend_from_slice(field.as_bytes());
                                line.push(b'"');
                            } else {
                                line.extend_from_slice(field.as_bytes());
                            }
                        }
                        clicks.push(line);
                    }
                }
            }
        }
        (events, clicks)
    }

    #[test]
    fn regular_rows_with_one_change() {
        for_each_window(|check| {
            let (events, clicks) = regular_rows(check);
            let changes: [&[u8]; 12] = [b"\"", b",", b"\r", b" ", b"x", b"0", b"Z", b"\"\"", b",,", b"\",", b",\"", b"9"];
            for (number, line) in events.iter().enumerate() {
                let declined = DECLINED.with(|count| count.get());
                check.event_all(line);
                // Rows the lean kernel is made for: it must take them itself.
                if line.len() >= 64 && line.len() <= 95 {
                    assert_eq!(
                        DECLINED.with(|count| count.get()),
                        declined,
                        "the lean kernel declined {:?}",
                        String::from_utf8_lossy(line)
                    );
                }
                // One in 13 rows gets every change at every position.
                if number % 13 == 0 {
                    for at in 0..=line.len() {
                        for change in changes {
                            let mut inserted = line[..at].to_vec();
                            inserted.extend_from_slice(change);
                            inserted.extend_from_slice(&line[at..]);
                            check.event(&inserted);
                            if at < line.len() {
                                let mut replaced = line[..at].to_vec();
                                replaced.extend_from_slice(change);
                                replaced.extend_from_slice(&line[at + 1..]);
                                check.event(&replaced);
                            }
                        }
                        if at < line.len() {
                            let mut removed = line[..at].to_vec();
                            removed.extend_from_slice(&line[at + 1..]);
                            check.event(&removed);
                        }
                    }
                }
            }
            for line in &clicks {
                check.click_all(line);
                for at in 0..=line.len() {
                    for change in changes {
                        let mut inserted = line[..at].to_vec();
                        inserted.extend_from_slice(change);
                        inserted.extend_from_slice(&line[at..]);
                        check.click(&inserted);
                        if at < line.len() {
                            let mut replaced = line[..at].to_vec();
                            replaced.extend_from_slice(change);
                            replaced.extend_from_slice(&line[at + 1..]);
                            check.click(&replaced);
                        }
                    }
                    if at < line.len() {
                        let mut removed = line[..at].to_vec();
                        removed.extend_from_slice(&line[at + 1..]);
                        check.click(&removed);
                    }
                }
            }
            assert!(check.answered.iter().all(|&count| count > 1_000), "{:?}", check.answered);
        });
    }

    #[test]
    fn random_damage() {
        let rounds = if cfg!(debug_assertions) { 20_000 } else { 400_000 };
        for_each_window(|check| {
            let (events, clicks) = regular_rows(check);
            let mut random = Random(0x9E37_79B9_7F4A_7C15 ^ check.ctx.window_start as u64);
            const BYTES: &[u8] = b"\",,\"\r 0123456789azAZ:-+TZ._/e\x80\xff\x00pu";
            let damage = |random: &mut Random, line: &mut Vec<u8>, pool: &[Vec<u8>]| {
                for _ in 0..random.below(4) {
                    let at = random.below(line.len() + 1);
                    match random.below(7) {
                        0 if at < line.len() => line[at] = BYTES[random.below(BYTES.len())],
                        1 if at < line.len() => line[at] = random.next() as u8,
                        2 => line.insert(at, BYTES[random.below(BYTES.len())]),
                        3 if at < line.len() => {
                            line.remove(at);
                        }
                        4 => {
                            // A piece of another row in place of the rest.
                            let other = &pool[random.below(pool.len())];
                            line.truncate(at);
                            line.extend_from_slice(&other[random.below(other.len() + 1)..]);
                        }
                        5 if at < line.len() => {
                            // Two bytes swapped.
                            let to = random.below(line.len());
                            line.swap(at, to);
                        }
                        _ => {
                            let length = random.below(12);
                            let digit = b'0' + random.below(10) as u8;
                            line.splice(at..at, std::iter::repeat(digit).take(length));
                        }
                    }
                }
            };
            for _ in 0..rounds {
                let mut line = events[random.below(events.len())].clone();
                damage(&mut random, &mut line, &events);
                check.event(&line);
                let mut line = clicks[random.below(clicks.len())].clone();
                damage(&mut random, &mut line, &clicks);
                check.click(&line);
            }
            assert!(check.answered.iter().all(|&count| count > rounds / 20), "{:?}", check.answered);
        });
    }

    /// Everything a chunk parse stored, list by list.
    fn stored(out: &mut ParseOut) -> Vec<Vec<u64>> {
        let mut lists = Vec::new();
        for writer in out.events.iter_mut().chain(out.indexed.iter_mut()) {
            let mut words = Vec::new();
            writer.finish().copy_into(&mut words);
            lists.push(words);
        }
        lists.push(out.odd_event_users.clone());
        lists
    }

    /// A file of rows through `parse_events_chunk`, in chunks: the fast loops
    /// must store exactly what the general parser alone stores.
    #[test]
    fn whole_files_in_chunks() {
        use super::super::parse::parse_events_chunk;
        let files = if cfg!(debug_assertions) { 1 } else { 2 };
        for window in 0..WINDOWS.len() {
            let mut check = Checker::new(window);
            let (events, _) = regular_rows(&check);
            let mut random = Random(0x2545_F491_4F6C_DD1D ^ window as u64);
            for file in 0..files {
                // Regular rows, short and long ones, damaged ones, empty lines.
                let mut buffer = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n".to_vec();
                for _ in 0..12_000 {
                    let mut line = events[random.below(events.len())].clone();
                    match random.below(40) {
                        0 => line.clear(),
                        1 => line.truncate(random.below(line.len() + 1)),
                        2 => line.extend_from_slice(&events[random.below(events.len())]),
                        3 => line.extend(std::iter::repeat(b'7').take(random.below(40))),
                        4 => {
                            let at = random.below(line.len());
                            line[at] = b"\",x0 \r"[random.below(6)];
                        }
                        5 => {
                            // A valid row shorter than 64 bytes.
                            line = format!("{},{},{},{},view,p1,", check.event_id, check.user_id, check.digits, check.ingest)
                                .into_bytes();
                        }
                        _ => {}
                    }
                    buffer.extend_from_slice(&line);
                    buffer.push(b'\n');
                }
                if file % 3 == 0 {
                    // No newline at the end of the file.
                    buffer.pop();
                }
                for mode in 0..2 {
                    check.ctx.keys = if mode == 1 { Some(check.keys) } else { None };
                    let mut answers = Vec::new();
                    for fast in [false, true] {
                        check.ctx.fast_rows = fast;
                        let mut out = ParseOut::new(&check.ctx);
                        // Chunks of a random size, as the workers take them.
                        let chunk = if fast { 1 + random.below(60_000) } else { buffer.len() };
                        let mut start = 0;
                        while start < buffer.len() {
                            let end = (start + chunk).min(buffer.len());
                            parse_events_chunk(&check.ctx, &buffer, start, end, &mut out);
                            start = end;
                        }
                        answers.push(stored(&mut out));
                    }
                    assert!(answers[0] == answers[1], "window {window}, file {file}, mode {mode}");
                    assert!(answers[0].iter().map(Vec::len).sum::<usize>() > 18_000);
                }
                check.ctx.keys = Some(check.keys);
                check.ctx.fast_rows = true;
            }
        }
    }
}
