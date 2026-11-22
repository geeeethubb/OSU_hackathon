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

use super::codes::{hex_value32, id_code, set_long_value, LONG_EVENT};
use super::arena::PartWriter;
use super::keys::{pre, unscramble, MASK as NUMBER_MASK};
use super::parse::{
    emit_code_click, emit_indexed, emit_long_value, emit_number_click, emit_raw, ClickRecord, EventRow, ParseOut,
};
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
            lean: LeanTables::new(base_day, ingest_adjust, window_start, text),
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
    // Rows of up to 127 bytes: a row with an ID of 33 characters (one in
    // eight distinct IDs of current generated data) is about 100 to 125.
    let block3 = _mm256_loadu_si256(p.add(96) as *const __m256i);
    let newline_low = mask(block0, newline) | (mask(block1, newline) << 32);
    let newline_high = mask(block2, newline) | (mask(block3, newline) << 32);
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
    let comma_high = (mask(block2, comma) | (mask(block3, comma) << 32)) & below_high;
    let quote_low = mask(block0, quote) | (mask(block1, quote) << 32);
    let quote_high = (mask(block2, quote) | (mask(block3, quote) << 32)) & below_high;
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

    // The six commas, in order, from both words (exactly six, checked above).
    let all = (u128::from(comma_high) << 64) | u128::from(comma_low);
    let c0 = all.trailing_zeros() as usize;
    let all = all & (all - 1);
    let c1 = all.trailing_zeros() as usize;
    let all = all & (all - 1);
    let c2 = all.trailing_zeros() as usize;
    let all = all & (all - 1);
    let c3 = all.trailing_zeros() as usize;
    let all = all & (all - 1);
    let c4 = all.trailing_zeros() as usize;
    let all = all & (all - 1);
    let c5 = all.trailing_zeros() as usize;
    let _ = commas_low;
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
/// The lean kernel as the compiler makes it, without the one in assembly.
pub const KERNEL_LEAN_PLAIN: u32 = 2;

/// The event kernel of this run: the lean one (in index mode with the
/// assembly in front of it) unless `SS_KERNEL=plain` asks for the lean
/// kernel without its assembly, or `SS_KERNEL=general` for `event_row_fast`
/// alone (both for A/B runs).
pub fn event_kernel() -> u32 {
    static KERNEL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *KERNEL.get_or_init(|| match std::env::var("SS_KERNEL").as_deref() {
        Ok("general") => KERNEL_GENERAL,
        Ok("plain") => KERNEL_LEAN_PLAIN,
        _ => KERNEL_LEAN,
    })
}

/// What the kernel in assembly does itself with the rows that are not a
/// plain `view` or `cart` (`SS_RARE`, for A/B runs on one binary): bit 0,
/// it finishes purchases. `SS_RARE=0`: nothing, every purchase goes back to
/// its caller as it did before; `SS_RARE=a`: purchases; unset: everything.
/// Bit 1: a row the assembly declines goes straight to the general kernel
/// (`event_row_rare`) instead of through the compiled lean kernel first.
pub fn rare_rows() -> u64 {
    static RARE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *RARE.get_or_init(|| match std::env::var("SS_RARE").as_deref() {
        Ok("0") => 0,
        Ok("a") => 1,
        _ => u64::MAX,
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
    // From here on: what only the kernel in assembly reads.
    /// The two bytes from the `Z` of ingest_ts on: `Z,`, or `Z"` in a quoted field.
    ingest_close: [u16; 2],
    /// `view` and `cart` with their commas and quotes: what to keep of the
    /// eight bytes from the comma after ingest_ts, and the four texts.
    type_masks: [u64; 4],
    type_texts: [u64; 4],
    /// The first eight bytes of `,purchase,` and of `,"purchase",`, then
    /// what to keep of the next four bytes and what that must be (`e,` and
    /// `se",`).
    purchase: [u64; 2],
    purchase_rest_mask: [u32; 2],
    purchase_rest: [u32; 2],
    /// A purchase's price and its product's number, as two texts of digits
    /// in the sixteen bytes that end with them: all ones in the last `n`
    /// bytes (by `n`), `0` bytes, nines, and the weights that make two
    /// numbers of eight digits each of sixteen digit pairs.
    keep_last: [[u8; 16]; 17],
    digit_zero: [u8; 32],
    nines: [u8; 32],
    hundreds: [i16; 16],
    ten_thousands: [i16; 16],
    /// Added to a thousand times the ingest seconds, less ts since the
    /// window's start: how late the row is, plus 2^62 so that an unsigned
    /// compare does; and the first such number that is too late.
    late_bias: i64,
    late_limit: u64,
    /// Ten times the first byte of a pair plus the second.
    tens: [u8; 32],
    /// The characters of an ID, classed by their two halves. By a byte's
    /// high four bits: the one kind of character it may then be (1 a digit,
    /// 2 a letter up to `o`, 4 a letter from `p`), and what to add to it for
    /// its base-36 value. By its low four bits: the kinds it may be.
    low_nibbles: [u8; 16],
    id_kind_high: [u8; 16],
    id_kind_low: [u8; 16],
    id_value: [u8; 16],
    zeros: [u8; 16],
    /// Base 36, most significant character first: pairs, fours, eights.
    id36_pairs: [u8; 16],
    id36_fours: [u32; 4],
    id36_eights: [u64; 2],
    /// By the lengths of both IDs (event: 7 or 8 characters, then user the
    /// same, as one index): for each ID the prefix's part of the number its
    /// last eight bytes spell, then the smallest number a canonical text of
    /// that length stands for and how many more there are, both with the
    /// top bit flipped for a signed compare.
    id_ranges: [[u64; 8]; 4],
    /// The `+HH:MM` that ends an F2 timestamp, as `TimeForm` has it for the
    /// other bytes.
    offset_pattern: [u8; 16],
    offset_limit: [u8; 16],
    offset_shuffle: [u8; 16],
    offset_ceilings: [i16; 8],
    offset_weights: [i16; 8],
    /// By the length of ts plus 64 (a length is -64 to 64 before it is
    /// known to be one): where its form's constants are in these tables.
    form_of_length: [u16; 129],
    forms: TimeForms,
}

/// A shuffle index that gives zero, a limit every byte is within, a ceiling
/// no number is over.
const SKIP: u8 = 0x80;
const FREE: u8 = 255;
const NONE: i16 = i16::MAX;

// F1, bytes 4..20: `-MM-DDTHH:MM:SSZ`. The numbers come out as hour,
// minute, second, nothing, month, day, nothing, nothing; the first weights
// make minutes of the day, the second, `month * 32 + day` and nothing of
// them, the second weights the second of the day, the same `month * 32 +
// day`, and nothing twice.
const PLAIN_PATTERN: [u8; 16] = *b"-00-00T00:00:00Z";
const PLAIN_LIMIT: [u8; 16] = [0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0];
const PLAIN_SHUFFLE: [u8; 16] = [7, 8, 10, 11, 13, 14, SKIP, SKIP, 1, 2, 4, 5, SKIP, SKIP, SKIP, SKIP];
const PLAIN_CEILINGS: [i16; 8] = [23, 59, 59, NONE, 12, 31, NONE, NONE];
const PLAIN_FIRST: [i16; 8] = [60, 1, 1, 0, 32, 1, 0, 0];
const PLAIN_SECOND: [i16; 8] = [60, 1, 1, 0, 0, 0, 0, 0];
// Thirteen digits, in the sixteen bytes that end with them: the first digit
// alone, nothing, then six pairs; four numbers of up to four digits; the
// first five digits as one number, nothing where F1 has its day, nothing,
// and the last eight digits as another number.
const DIGITS_PATTERN: [u8; 16] = [0, 0, 0, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30];
const DIGITS_LIMIT: [u8; 16] = [FREE, FREE, FREE, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9];
const DIGITS_SHUFFLE: [u8; 16] = [SKIP, 3, SKIP, SKIP, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
const DIGITS_FIRST: [i16; 8] = [1, 0, 100, 1, 100, 1, 100, 1];
const DIGITS_SECOND: [i16; 8] = [10_000, 1, 0, 0, 0, 0, 10_000, 1];

/// The constants of one form of ts, for the kernel in assembly: what the
/// sixteen bytes it takes of a timestamp are XORed with, the largest value
/// each may then have, how they pair up into numbers, the numbers' ceilings,
/// and the weights that add the numbers up in two steps. The low half of
/// each is for ts in this form, the high half for ingest_ts, which is always
/// F1. Then, for ts alone, what makes milliseconds since the window's start
/// of the sums, which are four numbers `low`, `day`, nothing and `high`
/// (F1: the second of the day, `month * 32 + day`, and no `high`; 13
/// digits: the first five digits, no `day`, the last eight digits), with
/// the form's day table (the real one, or a single entry of zeros):
/// `(table[day].seconds + low) * scale + high - start`,
/// and which bits of that day's year text ts must start with.
#[repr(C, align(64))]
struct TimeForm {
    pattern: [u8; 32],
    limit: [u8; 32],
    shuffle: [u8; 32],
    ceilings: [i16; 16],
    first: [i16; 16],
    second: [i16; 16],
    year_mask: u64,
    scale: i64,
    start: i64,
}

impl TimeForm {
    #[allow(clippy::too_many_arguments)]
    fn new(
        pattern: [u8; 16],
        limit: [u8; 16],
        shuffle: [u8; 16],
        ceilings: [i16; 8],
        first: [i16; 8],
        second: [i16; 8],
        of_day: bool,
        scale: i64,
        start: i64,
    ) -> Self {
        let bytes = |low: [u8; 16], high: [u8; 16]| {
            let mut both = [0u8; 32];
            both[..16].copy_from_slice(&low);
            both[16..].copy_from_slice(&high);
            both
        };
        let words = |low: [i16; 8], high: [i16; 8]| {
            let mut both = [0i16; 16];
            both[..8].copy_from_slice(&low);
            both[8..].copy_from_slice(&high);
            both
        };
        Self {
            pattern: bytes(pattern, PLAIN_PATTERN),
            limit: bytes(limit, PLAIN_LIMIT),
            shuffle: bytes(shuffle, PLAIN_SHUFFLE),
            ceilings: words(ceilings, PLAIN_CEILINGS),
            first: words(first, PLAIN_FIRST),
            second: words(second, PLAIN_SECOND),
            year_mask: if of_day { u64::from(u32::MAX) } else { 0 },
            scale,
            start,
        }
    }
}

/// The forms of ts and the day table, at fixed distances from one another:
/// the assembly reaches a form's day table from the form's own constants,
/// and that is the real table for F1 and one entry of zeros for 13 digits.
#[repr(C, align(64))]
struct TimeForms {
    digits: TimeForm,
    plain: TimeForm,
    /// Every other length: nothing passes.
    bad: TimeForm,
    /// F2: as F1 up to the seconds, any byte after them.
    offset: TimeForm,
    zero_day: [u32; 2],
    gap: [u8; 248],
    /// By `month * 32 + day`: the year text of the table day with that month
    /// and day, and the compact ingest seconds of its midnight (a signed
    /// number). Where there is no such day: a year no text has in a row the
    /// kernel would store as valid, and seconds past every range.
    day: [[u32; 2]; 512],
}

/// From a form's constants to its day table.
const FORM_DAY: usize = std::mem::offset_of!(TimeForms, day) - std::mem::offset_of!(TimeForms, plain);
const _: () = {
    assert!(std::mem::size_of::<TimeForm>() == 256);
    assert!(std::mem::offset_of!(TimeForms, zero_day) - std::mem::offset_of!(TimeForms, digits) == FORM_DAY);
    // The form no length should have looks its day up inside the table.
    assert!(std::mem::offset_of!(TimeForms, bad) + FORM_DAY >= std::mem::offset_of!(TimeForms, day));
    assert!(std::mem::offset_of!(TimeForms, bad) + FORM_DAY + 8 <= std::mem::size_of::<TimeForms>());
    assert!(std::mem::size_of::<LeanTables>() < 1 << 16);
};

impl LeanTables {
    fn new(base_day: i64, ingest_adjust: i64, window_start: i64, text: impl Fn(i64) -> u32) -> Self {
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
        let base_ms = (base_day * 86_400 - ingest_adjust) * 1_000;
        let mut tens = [1u8; 32];
        for pair in 0..16 {
            tens[2 * pair] = 10;
        }
        let flip = 1u64 << 63;
        let mut id_ranges = [[0u64; 8]; 4];
        for (lengths, entry) in id_ranges.iter_mut().enumerate() {
            for (id, prefix) in [14u64, 30].into_iter().enumerate() {
                let eight = (lengths >> id) & 1 == 1;
                entry[id] = if eight { 0 } else { prefix * POWER36_7 };
                entry[2 + id] = CANONICAL36[usize::from(eight)][0] ^ flip;
                entry[4 + id] = CANONICAL36[usize::from(eight)][1] ^ flip;
            }
        }
        let mut offset_pattern = [0u8; 16];
        offset_pattern[..6].copy_from_slice(b"+00:00");
        let mut offset_limit = [FREE; 16];
        offset_limit[1..6].copy_from_slice(&[9, 9, 0, 9, 9]);
        let mut offset_shuffle = [SKIP; 16];
        offset_shuffle[..4].copy_from_slice(&[1, 2, 4, 5]);
        let forms = std::mem::offset_of!(LeanTables, forms);
        let mut form_of_length = [(forms + std::mem::offset_of!(TimeForms, bad)) as u16; 129];
        form_of_length[64 + 13] = (forms + std::mem::offset_of!(TimeForms, digits)) as u16;
        form_of_length[64 + 20] = (forms + std::mem::offset_of!(TimeForms, plain)) as u16;
        let mut offset_tail = PLAIN_LIMIT;
        offset_tail[15] = FREE;
        Self {
            newline: [b'\n'; 32],
            comma: [b','; 32],
            quote: [b'"'; 32],
            base_ms,
            type_step,
            ingest_end: [u16::from_le_bytes(*b"Z,"), u16::from_le_bytes(*b"\",")],
            ingest_close: [u16::from_le_bytes(*b"Z,"), u16::from_le_bytes(*b"Z\"")],
            type_masks: [TYPE_SHORT, TYPE_SHORT, u64::MAX, u64::MAX],
            type_texts: [TYPE_VIEW, TYPE_CART, TYPE_VIEW_QUOTED, TYPE_CART_QUOTED],
            purchase: [TYPE_PURCHASE, TYPE_PURCHASE_QUOTED],
            purchase_rest_mask: [0xFFFF, 0xFFFF_FFFF],
            purchase_rest: [0x2C65, 0x2C22_6573],
            keep_last: std::array::from_fn(|kept| std::array::from_fn(|at| if at >= 16 - kept { 0xFF } else { 0 })),
            digit_zero: [b'0'; 32],
            nines: [9; 32],
            hundreds: std::array::from_fn(|at| if at % 2 == 0 { 100 } else { 1 }),
            ten_thousands: std::array::from_fn(|at| if at % 2 == 0 { 10_000 } else { 1 }),
            late_bias: base_ms - window_start + (1 << 62),
            late_limit: LATE_MS as u64 + (1 << 62) + 1,
            tens,
            low_nibbles: [15; 16],
            // `0`..`9` are 0x30..0x39, `a`..`o` 0x61..0x6F, `p`..`z` 0x70..0x7A.
            id_kind_high: [0, 0, 0, 1, 0, 0, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0],
            id_kind_low: [1 | 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 2 | 4, 2, 2, 2, 2, 2],
            id_value: [0, 0, 0, 0u8.wrapping_sub(b'0'), 0, 0, 10u8.wrapping_sub(b'a'), 10u8.wrapping_sub(b'a'), 0, 0, 0, 0, 0, 0, 0, 0],
            zeros: [0; 16],
            id36_pairs: [36, 1, 36, 1, 36, 1, 36, 1, 36, 1, 36, 1, 36, 1, 36, 1],
            id36_fours: [0x0001_0510; 4],
            id36_eights: [1_679_616; 2],
            id_ranges,
            offset_pattern,
            offset_limit,
            offset_shuffle,
            offset_ceilings: [23, 59, NONE, NONE, NONE, NONE, NONE, NONE],
            offset_weights: [3_600, 60, 0, 0, 0, 0, 0, 0],
            form_of_length,
            forms: TimeForms {
                digits: TimeForm::new(
                    DIGITS_PATTERN,
                    DIGITS_LIMIT,
                    DIGITS_SHUFFLE,
                    [NONE; 8],
                    DIGITS_FIRST,
                    DIGITS_SECOND,
                    false,
                    100_000_000,
                    window_start,
                ),
                plain: TimeForm::new(
                    PLAIN_PATTERN,
                    PLAIN_LIMIT,
                    PLAIN_SHUFFLE,
                    PLAIN_CEILINGS,
                    PLAIN_FIRST,
                    PLAIN_SECOND,
                    true,
                    1_000,
                    window_start - base_ms,
                ),
                // Every number is over a ceiling of -1.
                bad: TimeForm::new([0; 16], [0; 16], [SKIP; 16], [-1; 8], [0; 8], [0; 8], false, 0, 0),
                offset: TimeForm::new(
                    PLAIN_PATTERN,
                    offset_tail,
                    PLAIN_SHUFFLE,
                    PLAIN_CEILINGS,
                    PLAIN_FIRST,
                    PLAIN_SECOND,
                    true,
                    1_000,
                    window_start - base_ms,
                ),
                zero_day: [0; 2],
                gap: [0; 248],
                day,
            },
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
/// Index mode reads the eight bytes that end an ID as a base-36 number. For
/// an ID of 7 characters the first of them is the prefix: its part of the
/// number, by prefix and by whether the ID has 8 characters.
static LEAD36: [[u64; 2]; 2] = [[14 * POWER36_7, 0], [30 * POWER36_7, 0]];
/// The numbers of canonical texts (no leading zero, below 2^40) of 7 and of
/// 8 characters: the smallest, and how many more there are.
static CANONICAL36: [[u64; 2]; 2] = [[POWER36_6, POWER36_7 - 1 - POWER36_6], [POWER36_7, NUMBER_MASK - POWER36_7]];

/// The lean kernel on the row at `p`: `(length, guess, status)`. The status
/// is `ROW_DONE` (stored), `ROW_EMIT` (in `row`), `ROW_DROP` (a diagnostic
/// mode: parsed, not stored) or `ROW_DECLINE` (nothing was done and the
/// lengths mean nothing). `length` is the row's length; `guess` is 64 plus
/// where bytes 64..96 have their first newline, known long before `length`
/// is and nearly always equal to it.
#[inline(always)]
unsafe fn event_lean<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    lean: *const LeanTables,
    window_length: u64,
    p: *const u8,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> (usize, usize, u32) {
    const DECLINE: (usize, usize, u32) = (0, 0, ROW_DECLINE);
    let comma = _mm256_load_si256(std::ptr::addr_of!((*lean).comma) as *const __m256i);
    let quote = _mm256_load_si256(std::ptr::addr_of!((*lean).quote) as *const __m256i);
    let newline = _mm256_load_si256(std::ptr::addr_of!((*lean).newline) as *const __m256i);
    let block0 = _mm256_loadu_si256(p as *const __m256i);
    let block1 = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let comma_low = mask(block0, comma) | (mask(block1, comma) << 32);
    let quote_low = mask(block0, quote) | (mask(block1, quote) << 32);
    // 32 when there is no newline there, which is right for a row of 96 bytes.
    let guess = 64 + (mask(_mm256_loadu_si256(p.add(64) as *const __m256i), newline) as u32).trailing_zeros() as usize;
    if MODE == 2 {
        row.event ^= comma_low ^ quote_low;
        return (guess, guess, ROW_DROP);
    }

    // Which of the first four fields open with a quote (bit `i`: field `i`),
    // and which of the first three have one right before their comma. Fields
    // 0 to 2 are quoted at both ends or at neither.
    let opened = _pext_u64(quote_low, (comma_low << 1) | 1);
    let before = _pext_u64(quote_low << 1, comma_low);
    if (opened ^ before) & 7 != 0 {
        return DECLINE;
    }
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
    if (event_extra | user_extra) > 1 {
        return DECLINE;
    }
    if *p.add(q0) != b'e' {
        return DECLINE;
    }
    if *user_at != b'u' {
        return DECLINE;
    }
    let (event_code, user_code) = if INDEXED {
        // Index mode: both IDs must be generated ones. The eight bytes that
        // end an ID are its 8 characters, or its prefix and 7 characters
        // (the prefix is a letter, so all eight are characters either way).
        // Read as one base-36 number, less the prefix's part, they give the
        // ID's number; the text is canonical (no leading zero, below 2^40)
        // exactly when the number is in the range of its length.
        let keys = ctx.keys.as_ref().unwrap_unchecked();
        let (event_number, user_number, known) =
            two_numbers36(read_u64(p.add(c0 - q0).sub(8)), read_u64(p.add(c1 - q1).sub(8)));
        if known != 0xFFFF {
            return DECLINE;
        }
        let event_number = event_number.wrapping_sub(LEAD36[0][event_extra]);
        let user_number = user_number.wrapping_sub(LEAD36[1][user_extra]);
        if event_number.wrapping_sub(CANONICAL36[event_extra][0]) > CANONICAL36[event_extra][1]
            || user_number.wrapping_sub(CANONICAL36[user_extra][0]) > CANONICAL36[user_extra][1]
        {
            return DECLINE;
        }
        let index = unscramble(event_number, keys.event_key);
        let number = unscramble(user_number, keys.user_key);
        if index >= keys.index_bound || (number >> 9) >= keys.user_bound {
            return DECLINE;
        }
        (index, number)
    } else {
        // The eight bytes after the prefix are the characters, or seven of
        // them and the quote or comma that ends the field, which counts as
        // no character.
        let (event_code, user_code, known) = two_codes(read_u64(p.add(q0 + 1)), read_u64(user_at.add(1)));
        if known != (0x7F7F | ((event_extra as u32) << 7) | ((user_extra as u32) << 15)) {
            return DECLINE;
        }
        (event_code, user_code)
    };

    // Both timestamps. ingest_ts must be F1 on a table day, ended by `Z,` or
    // by `",` when it opened with a quote.
    let ts_at = p.add(c1 + q2 + 1);
    let ts_length = c2.wrapping_sub(c1 + 2 * q2 + 1) as u32;
    let ingest_at = p.add(c2 + q3 + 1);
    let type_at = ingest_at.add(20 + q3);
    let (ts_packed, ingest_packed, good) = iso_tails(ts_at, ingest_at);
    let days = std::ptr::addr_of!((*lean).forms.day) as *const [u32; 2];
    let ingest_day = *days.add((ingest_packed & 511) as usize);
    if good < 0xFFFF_0000 {
        return DECLINE;
    }
    if ingest_day[0] != (ingest_at as *const u32).read_unaligned() {
        return DECLINE;
    }
    if (type_at.sub(1) as *const u16).read_unaligned() != *(*lean).ingest_end.get_unchecked(q3) {
        return DECLINE;
    }
    let ingest_seconds = i64::from(ingest_day[1] as i32) + (ingest_packed >> 32) as i64;
    if ingest_seconds as u64 >= 1 << 21 {
        return DECLINE;
    }
    let base_ms = (*lean).base_ms;
    let ingest_ms = base_ms + ingest_seconds * 1_000;

    // ts: which of the three forms a row has is close to random, so the
    // 13-digit and the F1 reading are both made and one is picked without a
    // branch; the one in ten that is F2 takes a branch.
    let ts_day = *days.add((ts_packed & 511) as usize);
    let ts_year = (ts_at as *const u32).read_unaligned();
    let iso_ms = base_ms + (i64::from(ts_day[1] as i32) + (ts_packed >> 32) as i64) * 1_000;
    let ts_ms = if ts_length == 25 {
        // F2: the same 19 bytes, then the offset.
        let offset = iso_offset(ts_at.add(19));
        if good & 0x7FFF != 0x7FFF || ts_day[0] != ts_year || offset == NOT_FAST {
            return DECLINE;
        }
        iso_ms.wrapping_sub(offset.wrapping_mul(1_000))
    } else {
        let (digits, digit_mask) = digits13(ts_at);
        // Zero when the field is 13 digits; zero when it is F1 on a table day.
        let not_digits = (digit_mask ^ 0xFFFF) | (ts_length ^ 13);
        let not_plain = ((good & 0xFFFF) ^ 0xFFFF) | (ts_length ^ 20) | (ts_day[0] ^ ts_year);
        if u64::from(not_digits) * u64::from(not_plain) != 0 {
            return DECLINE;
        }
        select_unpredictable(not_digits == 0, digits, iso_ms)
    };
    let ts = ts_ms.wrapping_sub(ctx.window_start) as u64;
    let valid = (ts < window_length) & (ingest_ms.wrapping_sub(ts_ms) <= LATE_MS);

    // The event type with its commas and quotes, as one of four texts; a
    // purchase is the rare other case.
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
    let mut step = usize::from(*(*lean).type_step.get_unchecked(hits));
    let is_purchase = step == 0;
    if is_purchase {
        // `,purchase,` or `,"purchase",`: eight bytes from the same lanes, then the rest.
        let heads = _mm256_setr_epi64x(TYPE_PURCHASE as i64, TYPE_PURCHASE_QUOTED as i64, 0, 0);
        let found = _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(lanes, heads))) & 3;
        let plain = (found == 1) & ((type_at.add(8) as *const u16).read_unaligned() == 0x2C65);
        let quoted = (found == 2) & ((type_at.add(8) as *const u32).read_unaligned() == 0x2C22_6573);
        if !(plain | quoted) {
            return DECLINE;
        }
        step = 9 + 2 * usize::from(quoted);
    }

    // The 32 bytes from the comma after the type: the product, one more
    // comma, the price and the newline, which bit `reach` stands for. Every
    // byte before this comma was matched above and none is a newline, so
    // this newline ends the row.
    let region_at = type_at.add(step);
    let region = _mm256_loadu_si256(region_at as *const __m256i);
    let newlines = mask(region, newline) as u32;
    if newlines == 0 {
        return DECLINE;
    }
    let reach = newlines.trailing_zeros();
    let region_commas = _bzhi_u32(mask(region, comma) as u32, reach);
    let region_quotes = _bzhi_u32(mask(region, quote) as u32, reach);
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
    let length = region_at.offset_from(p) as usize + reach as usize;

    let mut valid = valid;
    let mut purchase = None;
    if is_purchase {
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
        return (length, guess, event_lean_finish::<MODE, INDEXED>(out, row, event_code, user_code, ts, valid, ingest_seconds, purchase));
    }
    (length, guess, event_lean_finish::<MODE, INDEXED>(out, row, event_code, user_code, ts, valid, ingest_seconds, None))
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

/// The lean kernel for the rows `event_lean` declines because an ID is long:
/// the event ID or the user ID (or both) is one byte and 32 lowercase
/// hexadecimal digits, as one in eight distinct IDs of current generated
/// data is. `event_lean` only looks at a row's first 64 bytes, where such a
/// row's first three commas need not be. This one reads the row the same way
/// with masks of its first 128 bytes and the exact place of all six commas;
/// any other row it declines, for the general kernel.
///
/// The shape it takes: event_id `e` and 7 or 8 of `[0-9a-z]`, or any byte and
/// 32 of `[0-9a-f]`; user_id the same with `u`; ts 13 digits, or F1 or F2 on a
/// day of the date table; ingest_ts F1 on a day of the date table; event_type
/// `view`, `cart` or `purchase`; product_id not empty; any field quoted or
/// not; the newline within 128 bytes. (Index mode never calls it.)
///
/// # Safety
/// `ROW_PAD` bytes after `p` are readable, and 3 before it.
#[inline(never)]
pub unsafe fn event_lean_long<const MODE: u32>(
    ctx: &Ctx,
    p: *const u8,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> (usize, u32) {
    const DECLINE: (usize, u32) = (0, ROW_DECLINE);
    let (lean, window_length) = lean_setup(ctx);
    let comma = _mm256_load_si256(std::ptr::addr_of!((*lean).comma) as *const __m256i);
    let quote = _mm256_load_si256(std::ptr::addr_of!((*lean).quote) as *const __m256i);
    let newline = _mm256_load_si256(std::ptr::addr_of!((*lean).newline) as *const __m256i);
    let blocks = [
        _mm256_loadu_si256(p as *const __m256i),
        _mm256_loadu_si256(p.add(32) as *const __m256i),
        _mm256_loadu_si256(p.add(64) as *const __m256i),
        _mm256_loadu_si256(p.add(96) as *const __m256i),
    ];
    let wide = |byte: __m256i| -> u128 {
        let low = mask(blocks[0], byte) | (mask(blocks[1], byte) << 32);
        let high = mask(blocks[2], byte) | (mask(blocks[3], byte) << 32);
        (u128::from(high) << 64) | u128::from(low)
    };
    let newlines = wide(newline);
    if newlines == 0 {
        return DECLINE;
    }
    let length = newlines.trailing_zeros() as usize;
    let below = (1u128 << length) - 1;
    let commas = wide(comma) & below;
    let quotes = wide(quote) & below;
    if commas.count_ones() != 6 {
        return DECLINE;
    }
    // The quote rule of `event_row_fast`, in two words: a quote only right
    // after a separator or right before one, never both, and a field quoted
    // at both ends or at neither. `opened` bit `i`: field `i` is quoted.
    let separators = commas | (1u128 << length);
    let starts = (separators << 1) | 1;
    let ends = separators >> 1;
    let pext128 = |bits: u128, select: u128| -> u64 {
        let (low, high) = (bits as u64, (bits >> 64) as u64);
        let (select_low, select_high) = (select as u64, (select >> 64) as u64);
        _pext_u64(low, select_low) | (_pext_u64(high, select_high) << select_low.count_ones())
    };
    let opened = pext128(quotes, starts);
    if ((quotes & !(starts | ends)) | (quotes & starts & ends)) != 0 || opened != pext128(quotes, ends) {
        return DECLINE;
    }
    let c0 = commas.trailing_zeros() as usize;
    let rest = commas & (commas - 1);
    let c1 = rest.trailing_zeros() as usize;
    let rest = rest & (rest - 1);
    let c2 = rest.trailing_zeros() as usize;
    let rest = rest & (rest - 1);
    let c3 = rest.trailing_zeros() as usize;
    let rest = rest & (rest - 1);
    let c4 = rest.trailing_zeros() as usize;
    let rest = rest & (rest - 1);
    let c5 = rest.trailing_zeros() as usize;
    let quoted = |index: u32| ((opened >> index) & 1) as usize;
    // Field 4's quotes are part of the type's text, matched below.
    let (q0, q1, q2, q3, q5, q6) = (quoted(0), quoted(1), quoted(2), quoted(3), quoted(5), quoted(6));

    // Both IDs: short (the prefix and 7 or 8 characters, coded as
    // `event_lean` codes them) or long (one byte and 32 hexadecimal digits).
    let event_at = p.add(q0);
    let event_length = c0 - 2 * q0;
    let user_at = p.add(c0 + 1 + q1);
    let user_length = c1 - c0 - 1 - 2 * q1;
    let (event_short, user_short, known) = two_codes(read_u64(event_at.add(1)), read_u64(user_at.add(1)));
    let event_value = if event_length == 33 {
        match hex_value32(event_at.add(1)) {
            Some(value) => Some(value),
            None => return DECLINE,
        }
    } else {
        let extra = event_length.wrapping_sub(8);
        if extra > 1 || *event_at != b'e' || known & 0xFF != 0x7F | ((extra as u32) << 7) {
            return DECLINE;
        }
        None
    };
    let user_value = if user_length == 33 {
        match hex_value32(user_at.add(1)) {
            Some(value) => {
                // Its slot in the user table loads while the rest is read.
                ctx.user_ids.prefetch_hex(*user_at, value);
                Some(value)
            }
            None => return DECLINE,
        }
    } else {
        let extra = user_length.wrapping_sub(8);
        if extra > 1 || *user_at != b'u' || (known >> 8) & 0xFF != 0x7F | ((extra as u32) << 7) {
            return DECLINE;
        }
        None
    };

    // Both timestamps, as `event_lean` reads them. ingest_ts is 20 bytes.
    let ts_at = p.add(c1 + 1 + q2);
    let ts_length = (c2 - c1 - 1 - 2 * q2) as u32;
    let ingest_at = p.add(c2 + 1 + q3);
    if c3 - c2 - 1 - 2 * q3 != 20 {
        return DECLINE;
    }
    let (ts_packed, ingest_packed, good) = iso_tails(ts_at, ingest_at);
    let days = std::ptr::addr_of!((*lean).forms.day) as *const [u32; 2];
    let ingest_day = *days.add((ingest_packed & 511) as usize);
    if good < 0xFFFF_0000 || ingest_day[0] != (ingest_at as *const u32).read_unaligned() {
        return DECLINE;
    }
    let ingest_seconds = i64::from(ingest_day[1] as i32) + (ingest_packed >> 32) as i64;
    if ingest_seconds as u64 >= 1 << 21 {
        return DECLINE;
    }
    let base_ms = (*lean).base_ms;
    let ingest_ms = base_ms + ingest_seconds * 1_000;
    let ts_day = *days.add((ts_packed & 511) as usize);
    let ts_year = (ts_at as *const u32).read_unaligned();
    let iso_ms = base_ms + (i64::from(ts_day[1] as i32) + (ts_packed >> 32) as i64) * 1_000;
    let ts_ms = if ts_length == 25 {
        let offset = iso_offset(ts_at.add(19));
        if good & 0x7FFF != 0x7FFF || ts_day[0] != ts_year || offset == NOT_FAST {
            return DECLINE;
        }
        iso_ms.wrapping_sub(offset.wrapping_mul(1_000))
    } else {
        let (digits, digit_mask) = digits13(ts_at);
        let not_digits = (digit_mask ^ 0xFFFF) | (ts_length ^ 13);
        let not_plain = ((good & 0xFFFF) ^ 0xFFFF) | (ts_length ^ 20) | (ts_day[0] ^ ts_year);
        if u64::from(not_digits) * u64::from(not_plain) != 0 {
            return DECLINE;
        }
        select_unpredictable(not_digits == 0, digits, iso_ms)
    };
    let ts = ts_ms.wrapping_sub(ctx.window_start) as u64;
    let mut valid = (ts < window_length) & (ingest_ms.wrapping_sub(ts_ms) <= LATE_MS);

    // The event type with its commas and quotes, as `event_lean` matches it,
    // and its closing comma must be the fifth.
    let type_at = p.add(c3);
    let lanes = _mm256_set1_epi64x(read_u64(type_at) as i64);
    let masks = _mm256_setr_epi64x(TYPE_SHORT as i64, TYPE_SHORT as i64, -1, -1);
    let texts =
        _mm256_setr_epi64x(TYPE_VIEW as i64, TYPE_CART as i64, TYPE_VIEW_QUOTED as i64, TYPE_CART_QUOTED as i64);
    let hits = _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(_mm256_and_si256(lanes, masks), texts)))
        as usize;
    let mut step = usize::from(*(*lean).type_step.get_unchecked(hits));
    let is_purchase = step == 0;
    if is_purchase {
        let heads = _mm256_setr_epi64x(TYPE_PURCHASE as i64, TYPE_PURCHASE_QUOTED as i64, 0, 0);
        let found = _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(lanes, heads))) & 3;
        let plain = (found == 1) & ((type_at.add(8) as *const u16).read_unaligned() == 0x2C65);
        let quoted = (found == 2) & ((type_at.add(8) as *const u32).read_unaligned() == 0x2C22_6573);
        if !(plain | quoted) {
            return DECLINE;
        }
        step = 9 + 2 * usize::from(quoted);
    }
    let product_length = c5 - c4 - 1 - 2 * q5;
    if c3 + step != c4 || product_length == 0 {
        return DECLINE;
    }

    let mut purchase = None;
    if is_purchase && valid {
        let price = std::slice::from_raw_parts(p.add(c5 + 1 + q6), length - c5 - 1 - 2 * q6);
        match parse_price(price) {
            Some(price) => purchase = Some((price, product_category(ctx, p.add(c4 + 1 + q5), product_length))),
            None => valid = false,
        }
    }
    // An invalid row keeps neither its time nor its user, as in
    // `event_lean_finish`; only a valid one needs its user's code.
    let user_code = match (valid, user_value) {
        (false, _) => 0,
        (true, None) => user_short,
        (true, Some(value)) => ctx.user_ids.hex_code(*user_at, value),
    };
    let ts = ts & u64::from(valid).wrapping_neg();
    let Some(value) = event_value else {
        return (length, event_lean_finish::<MODE, false>(out, row, event_short, user_code, ts, valid, ingest_seconds, purchase));
    };
    if MODE == MODE_STORE {
        emit_long_value(out, *event_at, value, ts, u64::from(valid), user_code, ingest_seconds as u64, purchase);
        return (length, ROW_DONE);
    }
    if MODE == MODE_ROW {
        // As the general parser leaves it: the code, and the value aside.
        set_long_value(*event_at, value);
        *row = EventRow {
            event: LONG_EVENT,
            user: user_code,
            user_is_number: false,
            ts: ts as u32,
            ingest: ingest_seconds as u32,
            valid,
            purchase,
        };
        return (length, ROW_EMIT);
    }
    row.event ^= (value as u64) ^ user_code ^ ts ^ (ingest_seconds as u64);
    (length, ROW_DROP)
}

// ---------------------------------------------------------------------------
// The lean kernel in assembly, for index mode: `event_lean` again, as one
// piece of code in which every value stays in its register from where it is
// made to where it is used. What the compiler makes of `event_lean` is about
// a third longer: it moves values between registers and the stack, turns
// each check into a flag and tests the flags later, and reads ts twice (as
// F1 and as 13 digits). Here:
//   - a check is a compare and a branch that is never taken;
//   - ts goes once through the same vector steps as ingest_ts, with the
//     constants of its form (found by its length) as their operands, and the
//     two sums come out as milliseconds by that form's scale and day table;
//   - an F2 timestamp (one row in ten) and a purchase (3 in 100) leave the
//     straight line for a piece of their own.
// It takes exactly the rows `event_lean` takes that end where the early look
// for the newline said, and reads the same from them; the tests hold the
// two against each other on every line they have.
//
// Registers: rdi the row, rsi the tables, rdx the frame; rax and r15 are
// scratch throughout. The vector constants every row uses are in ymm5 to
// ymm15 from before the first row on (`lean_asm!` loads them): comma, quote
// and newline bytes, the tens, the low half-byte mask, the three base-36
// weights, and the three tables that class an ID's characters. After `lean_head`: r9 and r10 where the texts of the
// IDs start, rcx and r14 where they end, r12 where ts starts and r8 where
// it ends (all as offsets), r13 ingest_ts and r11 the comma after it (as
// addresses). After `lean_ids`: r9 the event index and r10 the user number,
// each in the top 40 bits. After the timestamps: rax ts in milliseconds
// since the window's start, rcx the compact ingest seconds, r12 all ones
// for a valid row. Labels: 2 declines, 3 is a purchase, 4 an F2 timestamp,
// 5 where it comes back, 8 the end.
// ---------------------------------------------------------------------------

/// Masks, the early look for the newline, where the first four fields are.
/// `$next` puts where the next row then starts (`rdi + rax + 65`) in its place.
macro_rules! lean_head {
    ($next:literal) => {
        concat!(
            // Masks of the commas (r8) and quotes (r9) of bytes 0..64.
            "vmovdqu ymm0, ymmword ptr [rdi]\n",
            "vmovdqu ymm1, ymmword ptr [rdi + 32]\n",
            "vpcmpeqb ymm2, ymm0, ymm5\n",
            "vpcmpeqb ymm3, ymm1, ymm5\n",
            "vpmovmskb eax, ymm2\n",
            "vpmovmskb r8d, ymm3\n",
            "shl r8, 32\n",
            "or r8, rax\n",
            "vpcmpeqb ymm2, ymm0, ymm6\n",
            "vpcmpeqb ymm3, ymm1, ymm6\n",
            "vpmovmskb eax, ymm2\n",
            "vpmovmskb r9d, ymm3\n",
            "shl r9, 32\n",
            "or r9, rax\n",
            // Where the next row starts if this one's newline is the first
            // of bytes 64..96.
            "vpcmpeqb ymm2, ymm7, ymmword ptr [rdi + 64]\n",
            "vpmovmskb eax, ymm2\n",
            "tzcnt eax, eax\n",
            $next,
            // Quotes at the starts (r10) of the first four fields and before
            // the first three commas: both or neither. r11 is q3.
            "lea r10, [r8 + r8 + 1]\n",
            "pext r11, r9, r10\n",
            "lea rax, [r9 + r9]\n",
            "pext rax, rax, r8\n",
            "xor rax, r11\n",
            "test al, 7\n",
            "jnz 2f\n",
            "shr r11d, 3\n",
            "and r11d, 1\n",
            // Text starts (rcx) and text ends (r8) as bits.
            "mov rcx, r10\n",
            "and rcx, r9\n",
            "add rcx, r10\n",
            "rorx r10, r8, 1\n",
            "and r10, r9\n",
            "sub r8, r10\n",
            // s0 r9, s1 r10, s2 r12, s3 r13, e0 rcx, e1 r14, e2 r8.
            "tzcnt r9, rcx\n",
            "blsr rcx, rcx\n",
            "tzcnt r10, rcx\n",
            "blsr rcx, rcx\n",
            "tzcnt r12, rcx\n",
            "blsr rcx, rcx\n",
            "tzcnt r13, rcx\n",
            "tzcnt rcx, r8\n",
            "blsr r8, r8\n",
            "tzcnt r14, r8\n",
            "blsr r8, r8\n",
            "tzcnt r8, r8\n",
            // ingest_ts (r13): its `Z` (which the timestamps' step checks
            // again) and then the comma, or the quote that closes it (the
            // comma after that is the event type's to check). r11 becomes
            // that comma.
            "add r13, rdi\n",
            "movzx eax, word ptr [r13 + 19]\n",
            "cmp ax, word ptr [rsi + r11*2 + {ingest_close}]\n",
            "jne 2f\n",
            "lea r11, [r13 + r11 + 20]\n",
        )
    };
}

/// Both IDs as generated ones: the event's index and the user's number.
macro_rules! lean_ids {
    () => {
        concat!(
            // The eight bytes that end each ID.
            "vmovq xmm0, qword ptr [rdi + rcx - 8]\n",
            "vpinsrq xmm0, xmm0, qword ptr [rdi + r14 - 8], 1\n",
            // Each text is its prefix and 7 or 8 characters.
            "sub rcx, r9\n",
            "sub r14, r10\n",
            "lea rax, [rcx - 8]\n",
            "lea r15, [r14 - 8]\n",
            "or rax, r15\n",
            "cmp rax, 1\n",
            "ja 2f\n",
            "cmp byte ptr [rdi + r9], 101\n",
            "jne 2f\n",
            "cmp byte ptr [rdi + r10], 117\n",
            "jne 2f\n",
            // Base-36 values of the sixteen bytes; all must be characters
            // (of 7 characters, the first byte is the prefix, a letter). By
            // a byte's high half (xmm1): the kind it may be (table in xmm13)
            // and what makes its value of it (xmm15); by its low half, which
            // a byte over 127 has none of: the kinds it may be (xmm14).
            // xmm3 collects what is wrong with the IDs (all ones in a byte
            // that is no character); the timestamps' step tests it.
            "vpsrlw xmm1, xmm0, 4\n",
            "vpand xmm1, xmm1, xmm9\n",
            "vpshufb xmm2, xmm13, xmm1\n",
            "vpshufb xmm3, xmm14, xmm0\n",
            "vpand xmm3, xmm3, xmm2\n",
            "vpcmpeqb xmm3, xmm3, xmmword ptr [rsi + {zeros}]\n",
            "vpshufb xmm1, xmm15, xmm1\n",
            "vpaddb xmm1, xmm1, xmm0\n",
            // The two numbers the bytes spell.
            "vpmaddubsw xmm1, xmm1, xmm10\n",
            "vpmaddwd xmm1, xmm1, xmm11\n",
            "vpmuludq xmm2, xmm1, xmm12\n",
            "vpsrlq xmm1, xmm1, 32\n",
            "vpaddq xmm1, xmm1, xmm2\n",
            // By both lengths: less the prefix's part, and in the range of
            // the canonical texts of that length.
            "lea eax, [rcx + r14*2 - 24]\n",
            "shl eax, 6\n",
            "vpsubq xmm1, xmm1, xmmword ptr [rsi + rax + {id_ranges}]\n",
            "vpsubq xmm2, xmm1, xmmword ptr [rsi + rax + {id_ranges} + 16]\n",
            "vpcmpgtq xmm2, xmm2, xmmword ptr [rsi + rax + {id_ranges} + 32]\n",
            "vpor xmm3, xmm3, xmm2\n",
            "vmovq r9, xmm1\n",
            "vpextrq r10, xmm1, 1\n",
            // `unscramble` with the number in the top 40 bits, where the
            // multiplications drop what overflows by themselves: times the
            // first inverse, the two shifts (the rotation brings in zeros,
            // the low 24 bits being zero), the key, then down and times the
            // second inverse; and below its bound.
            "imul r9, qword ptr [rdx + {inverse_b}]\n",
            "rorx rax, r9, 17\n",
            "xor r9, rax\n",
            "shr rax, 17\n",
            "xor r9, rax\n",
            "xor r9, qword ptr [rdx + {event_key}]\n",
            "shr r9, 24\n",
            "imul r9, qword ptr [rdx + {inverse_a}]\n",
            "cmp r9, qword ptr [rdx + {index_limit}]\n",
            "jae 2f\n",
            "imul r10, qword ptr [rdx + {inverse_b}]\n",
            "rorx rax, r10, 17\n",
            "xor r10, rax\n",
            "shr rax, 17\n",
            "xor r10, rax\n",
            "xor r10, qword ptr [rdx + {user_key}]\n",
            "shr r10, 24\n",
            "imul r10, qword ptr [rdx + {inverse_a}]\n",
            "cmp r10, qword ptr [rdx + {user_limit}]\n",
            "jae 2f\n",
        )
    };
}

/// Both timestamps through the vector units, with the constants at r14: the
/// sums of ts in xmm0, the first of them in rax too, those of ingest_ts in
/// xmm4. Declines the row if a byte is over its limit, a number over its
/// ceiling, or the IDs left something in ymm3.
macro_rules! lean_times {
    () => {
        concat!(
            "vpxor ymm0, ymm0, ymmword ptr [r14 + {f_pattern}]\n",
            "vpsubusb ymm1, ymm0, ymmword ptr [r14 + {f_limit}]\n",
            "vpshufb ymm0, ymm0, ymmword ptr [r14 + {f_shuffle}]\n",
            "vpmaddubsw ymm0, ymm0, ymm8\n",
            "vpcmpgtw ymm2, ymm0, ymmword ptr [r14 + {f_ceilings}]\n",
            "vpor ymm1, ymm1, ymm2\n",
            "vpor ymm1, ymm1, ymm3\n",
            "vptest ymm1, ymm1\n",
            "jnz 2f\n",
            "vpmaddwd ymm0, ymm0, ymmword ptr [r14 + {f_first}]\n",
            "vpackusdw ymm0, ymm0, ymm0\n",
            "vpmaddwd ymm0, ymm0, ymmword ptr [r14 + {f_second}]\n",
            "vextracti128 xmm4, ymm0, 1\n",
            "vmovd eax, xmm0\n",
        )
    };
}

/// ts in F1 or as 13 digits, by its form's constants.
macro_rules! lean_ts {
    () => {
        concat!(
            // The length of ts picks the form; F2 goes its own way.
            "mov rax, r8\n",
            "sub rax, r12\n",
            "cmp eax, 25\n",
            "je 4f\n",
            "movzx r14d, word ptr [rsi + rax*2 + {form_mid}]\n",
            "add r14, rsi\n",
            // The sixteen bytes that end ts, and bytes 4..20 of ingest_ts.
            "vmovdqu xmm0, xmmword ptr [rdi + r8 - 16]\n",
            "vinserti128 ymm0, ymm0, xmmword ptr [r13 + 4], 1\n",
            lean_times!(),
            // The sums of ts (rax low, r15 day, r8 high) as milliseconds
            // since the window's start, and the year text of its day.
            "vpextrd r15d, xmm0, 1\n",
            "vpextrd r8d, xmm0, 3\n",
            "mov r12d, dword ptr [rdi + r12]\n",
            "xor r12d, dword ptr [r14 + r15*8 + {f_day}]\n",
            "and r12d, dword ptr [r14 + {f_year_mask}]\n",
            "jnz 2f\n",
            "movsxd r15, dword ptr [r14 + r15*8 + {f_day} + 4]\n",
            "add rax, r15\n",
            "imul rax, qword ptr [r14 + {f_scale}]\n",
            "add rax, r8\n",
            "sub rax, qword ptr [r14 + {f_start}]\n",
            "5:\n",
        )
    };
}

/// ingest_ts on a table day and in range (rcx), and whether the row is valid (r12).
macro_rules! lean_ingest {
    () => {
        concat!(
            "vmovd ecx, xmm4\n",
            "vpextrd r8d, xmm4, 1\n",
            "mov r12d, dword ptr [r13]\n",
            "cmp r12d, dword ptr [rsi + r8*8 + {day}]\n",
            "jne 2f\n",
            "movsxd r8, dword ptr [rsi + r8*8 + {day} + 4]\n",
            "add rcx, r8\n",
            "cmp rcx, 0x1FFFFF\n",
            "ja 2f\n",
            // In the window (r12) and not late (r8).
            "imul r8, rcx, 1000\n",
            "add r8, qword ptr [rsi + {late_bias}]\n",
            "sub r8, rax\n",
            "cmp rax, qword ptr [rdx + {window_length}]\n",
            "sbb r12, r12\n",
            "cmp r8, qword ptr [rsi + {late_limit}]\n",
            "sbb r8, r8\n",
            "and r12, r8\n",
        )
    };
}

/// `view` or `cart` with its commas and quotes: r11 becomes the comma after it.
macro_rules! lean_type {
    () => {
        concat!(
            "vpbroadcastq ymm0, qword ptr [r11]\n",
            "vpand ymm0, ymm0, ymmword ptr [rsi + {type_masks}]\n",
            "vpcmpeqq ymm0, ymm0, ymmword ptr [rsi + {type_texts}]\n",
            "vmovmskpd r8d, ymm0\n",
            "movzx r8d, byte ptr [rsi + r8 + {type_step}]\n",
            "test r8d, r8d\n",
            "jz 3f\n",
            "add r11, r8\n",
        )
    };
}

/// The 32 bytes from the comma after the type (r11): the product, one comma,
/// the price and the newline, which must be where the next row was taken to
/// start. Leaves the flags of the last check: zero for a row that is
/// regular, and then r15 is zero too. `$next` compares r15 with where the
/// next row was taken to start. The other three arguments are what a
/// purchase stores on the way: after the newline is found (r8d), after the
/// comma is (r13d, as a bit), after the fields' opening quotes are (r8d).
macro_rules! lean_tail {
    ($next:literal, $reach:literal, $comma:literal, $opened:literal) => {
        concat!(
            "vmovdqu ymm0, ymmword ptr [r11]\n",
            "vpcmpeqb ymm1, ymm0, ymm7\n",
            "vpmovmskb r8d, ymm1\n",
            "vpcmpeqb ymm1, ymm0, ymm5\n",
            "vpmovmskb r13d, ymm1\n",
            "vpcmpeqb ymm1, ymm0, ymm6\n",
            "vpmovmskb r14d, ymm1\n",
            "tzcnt r8d, r8d\n",
            "jc 2f\n",
            $reach,
            "lea r15, [r11 + r8 + 1]\n",
            $next,
            "jne 2f\n",
            // Without the type's comma (bit 0): exactly one comma.
            "bzhi r13d, r13d, r8d\n",
            "blsr r13d, r13d\n",
            "jz 2f\n",
            "blsr r15d, r13d\n",
            "jnz 2f\n",
            $comma,
            // The separators (r13d: that comma, the newline), where the two
            // fields start (r15d) and finish (r13d). A quote is at a start or
            // at a finish, never at both, and a field has both or neither.
            "bzhi r14d, r14d, r8d\n",
            "bts r13d, r8d\n",
            "lea r15d, [r13 + r13 + 2]\n",
            "shr r13d, 1\n",
            "pext r8d, r14d, r15d\n",
            $opened,
            "xor r15d, r13d\n",
            "andn r15d, r15d, r14d\n",
            "jnz 2f\n",
            "pext r14d, r14d, r13d\n",
            "cmp r8d, r14d\n",
            "jne 2f\n",
            // An empty product (its field finishes right where its text
            // starts: at 0, or at 2 after a quote) is the general kernel's.
            "and r8d, 1\n",
            "lea r8d, [r8 + r8*2 + 1]\n",
            "test r13d, r8d\n",
        )
    };
}

/// ts in F2: F1 up to the seconds through the same steps, then the offset.
macro_rules! lean_offset {
    () => {
        concat!(
            "4:\n",
            "lea r14, [rsi + {offset_form}]\n",
            "vmovdqu xmm0, xmmword ptr [rdi + r12 + 4]\n",
            "vinserti128 ymm0, ymm0, xmmword ptr [r13 + 4], 1\n",
            lean_times!(),
            // Local seconds on a table day (rax).
            "vpextrd r8d, xmm0, 1\n",
            "mov r15d, dword ptr [rdi + r12]\n",
            "cmp r15d, dword ptr [rsi + r8*8 + {day}]\n",
            "jne 2f\n",
            "movsxd r15, dword ptr [rsi + r8*8 + {day} + 4]\n",
            "add rax, r15\n",
            // `+HH:MM` or `-HH:MM`: hours to 23, minutes to 59, as seconds (r8).
            "vmovq xmm0, qword ptr [rdi + r12 + 19]\n",
            "vpxor xmm0, xmm0, xmmword ptr [rsi + {offset_pattern}]\n",
            "vpminub xmm1, xmm0, xmmword ptr [rsi + {offset_limit}]\n",
            "vpcmpeqb xmm1, xmm1, xmm0\n",
            "vpshufb xmm0, xmm0, xmmword ptr [rsi + {offset_shuffle}]\n",
            "vpmaddubsw xmm0, xmm0, xmm8\n",
            "vpcmpgtw xmm2, xmm0, xmmword ptr [rsi + {offset_ceilings}]\n",
            "vpandn xmm1, xmm2, xmm1\n",
            "vpmovmskb r8d, xmm1\n",
            "cmp r8d, 0xFFFF\n",
            "jne 2f\n",
            "vpmaddwd xmm0, xmm0, xmmword ptr [rsi + {offset_weights}]\n",
            "vmovd r8d, xmm0\n",
            // The sign: -1 for `+` (the offset comes off), 1 for `-`.
            "movzx r15d, byte ptr [rdi + r12 + 19]\n",
            "sub r15, 44\n",
            "lea r14, [r15 + 1]\n",
            "test r14, -3\n",
            "jnz 2f\n",
            "imul r8, r15\n",
            "add rax, r8\n",
            "imul rax, rax, 1000\n",
            "sub rax, qword ptr [rsi + {plain_start}]\n",
            "jmp 5b\n",
        )
    };
}

/// A purchase: `,purchase,` or `,"purchase",`, then the same 32 bytes, and
/// what `event_lean` reads of them for a valid row: the price (digits only,
/// at most eight of them after leading zeros, 1 to 10,000,000) and the
/// category of the product. The category is taken from the table by
/// product number for a product written `p` and 1 to 7 digits with no
/// leading zero; any other product, a number past the table, and a price
/// text of more than 16 bytes are left to the caller (25), with what it
/// needs to read product and price in the frame. A row that is not valid,
/// or whose price is no price (26: then it is not valid either), goes on as
/// the invalid row it is (24), like a `view`.
///
/// `$next` is the tail's; `$caller` is where 25 goes on to after setting
/// r15 to 2; `$bought` ends a valid purchase: r13 holds its price (low
/// half) and category (high half), and rax, rcx, r9, r10 and r12 are as
/// for a `view`.
macro_rules! lean_purchase {
    ($next:literal, $caller:literal, $bought:ident) => {
        concat!(
            "3:\n",
            // Which of the two texts (r13: 1 for the one in quotes), then all of it.
            "mov r8, qword ptr [r11]\n",
            "xor r13d, r13d\n",
            "cmp r8, qword ptr [rsi + {purchase} + 8]\n",
            "sete r13b\n",
            "cmp r8, qword ptr [rsi + r13*8 + {purchase}]\n",
            "jne 2f\n",
            "mov r8d, dword ptr [r11 + 8]\n",
            "and r8d, dword ptr [rsi + r13*4 + {purchase_rest_mask}]\n",
            "cmp r8d, dword ptr [rsi + r13*4 + {purchase_rest}]\n",
            "jne 2f\n",
            "lea r11, [r11 + r13*2 + 9]\n",
            "mov qword ptr [rdx + {region}], r11\n",
            lean_tail!(
                $next,
                "mov dword ptr [rdx + {reach}], r8d\n",
                "mov dword ptr [rdx + {last_comma}], r13d\n",
                "mov dword ptr [rdx + {opened}], r8d\n"
            ),
            "jnz 2f\n",
            // (Unless this run leaves every purchase to the caller.)
            "test byte ptr [rdx + {rare}], 1\n",
            "jz 25f\n",
            "test r12, r12\n",
            "jz 24b\n",
            // r8 the newline, r13 the comma between product and price (both
            // from r11 on), r14 whether the product opens with a quote, r15
            // whether the price does.
            "mov r8d, dword ptr [rdx + {reach}]\n",
            "mov r13d, dword ptr [rdx + {last_comma}]\n",
            "mov r14d, dword ptr [rdx + {opened}]\n",
            "tzcnt r13d, r13d\n",
            "mov r15d, r14d\n",
            "shr r15d, 1\n",
            "and r14d, 1\n",
            // The price: where it ends (r8), its length (r15); the sixteen
            // bytes that end with it, and which of them are its own.
            "add r8, r11\n",
            "sub r8, r15\n",
            "lea r15, [r13 + r15 + 1]\n",
            "add r15, r11\n",
            "neg r15\n",
            "add r15, r8\n",
            "cmp r15, 16\n",
            "ja 25f\n",
            "vmovdqu xmm0, xmmword ptr [r8 - 16]\n",
            "shl r15, 4\n",
            "vmovdqu xmm1, xmmword ptr [rsi + r15 + {keep_last}]\n",
            // The product: `p`, then a digit that is not `0`; where it ends
            // (r8); the same sixteen bytes for it; 1 to 7 digits (r13 is
            // their number plus two).
            "cmp byte ptr [r11 + r14 + 1], 112\n",
            "jne 25f\n",
            "cmp byte ptr [r11 + r14 + 2], 48\n",
            "je 25f\n",
            "lea r8, [r11 + r13]\n",
            "sub r8, r14\n",
            "vinserti128 ymm0, ymm0, xmmword ptr [r8 - 16], 1\n",
            "sub r13, r14\n",
            "sub r13, r14\n",
            "lea r8, [r13 - 3]\n",
            "cmp r8, 6\n",
            "ja 25f\n",
            "shl r13, 4\n",
            "vinserti128 ymm1, ymm1, xmmword ptr [rsi + r13 + {keep_last} - 32], 1\n",
            // Digit values of both texts, nothing for the bytes before
            // them. A product with anything but digits is the caller's; a
            // price with anything but digits is no price.
            "vpxor ymm0, ymm0, ymmword ptr [rsi + {digit_zero}]\n",
            "vpand ymm0, ymm0, ymm1\n",
            "vpsubusb ymm1, ymm0, ymmword ptr [rsi + {nines}]\n",
            "vextracti128 xmm2, ymm1, 1\n",
            "vptest xmm2, xmm2\n",
            "jnz 25f\n",
            "vptest xmm1, xmm1\n",
            "jnz 26f\n",
            // Each text as its first eight digits and its last eight.
            "vpmaddubsw ymm0, ymm0, ymm8\n",
            "vpmaddwd ymm0, ymm0, ymmword ptr [rsi + {hundreds}]\n",
            "vpackusdw ymm0, ymm0, ymm0\n",
            "vpmaddwd ymm0, ymm0, ymmword ptr [rsi + {ten_thousands}]\n",
            "vmovq r8, xmm0\n",
            "vextracti128 xmm0, ymm0, 1\n",
            "vmovq r13, xmm0\n",
            // The price (r8): nothing in the first eight digits, 1 to 10,000,000.
            "rorx r8, r8, 32\n",
            "lea r14, [r8 - 1]\n",
            "cmp r14, 9999999\n",
            "ja 26f\n",
            // The product's number, inside the table; its category above the price.
            "shr r13, 32\n",
            "cmp r13, qword ptr [rdx + {category_count}]\n",
            "jae 25f\n",
            "mov r14, qword ptr [rdx + {categories}]\n",
            "mov r13d, dword ptr [r14 + r13*4]\n",
            "shl r13, 32\n",
            "or r13, r8\n",
            $bought!(),
            "26:\n",
            "xor r12d, r12d\n",
            "jmp 24b\n",
            "25:\n",
            "mov r15d, 2\n",
            $caller,
        )
    };
}

/// A valid purchase, for the caller of one row: price and category into the frame.
macro_rules! lean_bought_row {
    () => {
        concat!("mov qword ptr [rdx + {bought}], r13\n", "mov r15d, 4\n", "jmp 8f\n",)
    };
}

/// A valid purchase into the list of its index partition: the three words
/// `emit_indexed` stores. A full block is the caller's (who reads price and
/// category again).
macro_rules! lean_bought_store {
    () => {
        concat!(
            "pext r8, r9, qword ptr [rdx + {part_mask}]\n",
            "shl r8, 5\n",
            "add r8, qword ptr [rdx + {writers}]\n",
            "mov r11, qword ptr [r8]\n",
            "lea r14, [r11 + 24]\n",
            "cmp r14, qword ptr [r8 + 8]\n",
            "ja 25f\n",
            "mov qword ptr [r8], r14\n",
            "shl r9, 24\n",
            "and r9, qword ptr [rdx + {low_mask}]\n",
            "lea rax, [rax + rax + 1]\n",
            "bts rax, 32\n",
            "or rax, r9\n",
            "mov qword ptr [r11], rax\n",
            "shr r10, 2\n",
            "lea r10, [r10 + rcx*2 + 1]\n",
            "mov qword ptr [r11 + 8], r10\n",
            "mov qword ptr [r11 + 16], r13\n",
            "jmp 27b\n",
        )
    };
}

/// A valid purchase in the timing variant that stores nothing.
macro_rules! lean_bought_dropped {
    () => {
        "jmp 27b\n"
    };
}

/// The whole row. r15 at the end: 0 for a row that is read and is no valid
/// purchase, 4 for a valid purchase (price and category in the frame), 2
/// for a purchase that is read up to its product and price, 1 for a row the
/// kernel declines.
macro_rules! lean_row {
    () => {
        concat!(
            lean_head!("lea rax, [rdi + rax + 65]\nmov qword ptr [rdx + {next}], rax\n"),
            lean_ids!(),
            lean_ts!(),
            lean_ingest!(),
            lean_type!(),
            lean_tail!("cmp r15, qword ptr [rdx + {next}]\n", "", "", ""),
            "jz 8f\n",
            "jmp 2f\n",
            "24:\n",
            "xor r15d, r15d\n",
            "jmp 8f\n",
            lean_offset!(),
            lean_purchase!("cmp r15, qword ptr [rdx + {next}]\n", "jmp 8f\n", lean_bought_row),
            "2:\n",
            "mov r15d, 1\n",
            "8:\n",
        )
    };
}

/// A `view` or `cart` into the list of its index partition: the two words
/// `emit_indexed` stores. From the index in the top 40 bits: the partition
/// (the bits from the partition shift up), its writer (the cursor, then the
/// end of its block), and the index within the partition at bit 48. A full
/// block is the caller's to replace (22).
macro_rules! lean_store {
    () => {
        concat!(
            "pext r8, r9, qword ptr [rdx + {part_mask}]\n",
            "shl r8, 5\n",
            "add r8, qword ptr [rdx + {writers}]\n",
            "mov r11, qword ptr [r8]\n",
            "lea r13, [r11 + 16]\n",
            "cmp r13, qword ptr [r8 + 8]\n",
            "ja 22f\n",
            "mov qword ptr [r8], r13\n",
            "shl r9, 24\n",
            "and r9, qword ptr [rdx + {low_mask}]\n",
            // ts, valid and user-is-number: all of them, or nothing.
            "lea rax, [rax + rax + 1]\n",
            "bts rax, 32\n",
            "and rax, r12\n",
            "or rax, r9\n",
            "mov qword ptr [r11], rax\n",
            // The user (kept by a valid row only) at bit 22, the ingest seconds at bit 1.
            "and r10, r12\n",
            "shr r10, 2\n",
            "lea r10, [r10 + rcx*2]\n",
            "mov qword ptr [r11 + 8], r10\n",
        )
    };
}

/// No store: the timing variant that only reads.
macro_rules! lean_no_store {
    () => {
        ""
    };
}

/// Row after row from rdi until the frame's `last`, each `view` and `cart`
/// stored (by the piece `$store`). r15 at the end: 0 when the next row (rdi) is at or past `last`;
/// 1 for a row (rdi) the kernel declines; 2 for a purchase and 3 for a row
/// whose list needs a new block: what was read of it is in the frame, and
/// the caller stores it and comes back for the row at `next`. Where the
/// next row starts is in rbx while a row is read; rbx is the compiler's, so
/// it is saved in the frame and put back at the end.
macro_rules! lean_run {
    ($store:ident, $bought:ident) => {
        concat!(
            "mov qword ptr [rdx + {saved}], rbx\n",
            "9:\n",
            lean_head!("lea rbx, [rdi + rax + 65]\n"),
            lean_ids!(),
            lean_ts!(),
            lean_ingest!(),
            lean_type!(),
            lean_tail!("cmp r15, rbx\n", "", "", ""),
            "jnz 2f\n",
            "24:\n",
            $store!(),
            "27:\n",
            "mov rdi, rbx\n",
            "cmp rdi, qword ptr [rdx + {last}]\n",
            "jb 9b\n",
            "xor r15d, r15d\n",
            "jmp 8f\n",
            lean_offset!(),
            lean_purchase!("cmp r15, rbx\n", "jmp 23f\n", $bought),
            "22:\n",
            "mov r15d, 3\n",
            "23:\n",
            "mov qword ptr [rdx + {next}], rbx\n",
            "shr r9, 24\n",
            "shr r10, 24\n",
            "mov qword ptr [rdx + {read}], rax\n",
            "mov qword ptr [rdx + {read} + 8], rcx\n",
            "mov qword ptr [rdx + {read} + 16], r9\n",
            "mov qword ptr [rdx + {read} + 24], r10\n",
            "mov qword ptr [rdx + {read} + 32], r12\n",
            "jmp 8f\n",
            "2:\n",
            "mov r15d, 1\n",
            "8:\n",
            "mov rbx, qword ptr [rdx + {saved}]\n",
            // The compiler does not know the vector registers were used here.
            "vzeroupper\n",
        )
    };
}

/// One `asm!` of the kernel: its pieces (after the vector constants every
/// row uses, which stay in registers), its operands, and the places in the
/// tables and in the frame that every use of it names.
macro_rules! lean_asm {
    ([$($template:expr,)*] $($operands:tt)*) => {
        std::arch::asm!(
            "vmovdqu ymm5, ymmword ptr [rsi + {comma}]",
            "vmovdqu ymm6, ymmword ptr [rsi + {quote}]",
            "vmovdqu ymm7, ymmword ptr [rsi + {newline}]",
            "vmovdqu ymm8, ymmword ptr [rsi + {tens}]",
            "vmovdqu xmm9, xmmword ptr [rsi + {low_nibbles}]",
            "vmovdqu xmm10, xmmword ptr [rsi + {id36_pairs}]",
            "vmovdqu xmm11, xmmword ptr [rsi + {id36_fours}]",
            "vmovdqu xmm12, xmmword ptr [rsi + {id36_eights}]",
            "vmovdqu xmm13, xmmword ptr [rsi + {id_kind_high}]",
            "vmovdqu xmm14, xmmword ptr [rsi + {id_kind_low}]",
            "vmovdqu xmm15, xmmword ptr [rsi + {id_value}]",
            $($template,)*
            $($operands)*
            out("ymm5") _, out("ymm6") _, out("ymm7") _, out("ymm8") _, out("ymm9") _, out("ymm10") _,
            out("ymm11") _, out("ymm12") _, out("ymm13") _, out("ymm14") _, out("ymm15") _,
            comma = const std::mem::offset_of!(LeanTables, comma),
            quote = const std::mem::offset_of!(LeanTables, quote),
            newline = const std::mem::offset_of!(LeanTables, newline),
            ingest_close = const std::mem::offset_of!(LeanTables, ingest_close),
            type_masks = const std::mem::offset_of!(LeanTables, type_masks),
            type_texts = const std::mem::offset_of!(LeanTables, type_texts),
            type_step = const std::mem::offset_of!(LeanTables, type_step),
            purchase = const std::mem::offset_of!(LeanTables, purchase),
            purchase_rest_mask = const std::mem::offset_of!(LeanTables, purchase_rest_mask),
            purchase_rest = const std::mem::offset_of!(LeanTables, purchase_rest),
            keep_last = const std::mem::offset_of!(LeanTables, keep_last),
            digit_zero = const std::mem::offset_of!(LeanTables, digit_zero),
            nines = const std::mem::offset_of!(LeanTables, nines),
            hundreds = const std::mem::offset_of!(LeanTables, hundreds),
            ten_thousands = const std::mem::offset_of!(LeanTables, ten_thousands),
            categories = const std::mem::offset_of!(LeanFrame, categories),
            rare = const std::mem::offset_of!(LeanFrame, rare),
            category_count = const std::mem::offset_of!(LeanFrame, category_count),
            late_bias = const std::mem::offset_of!(LeanTables, late_bias),
            late_limit = const std::mem::offset_of!(LeanTables, late_limit),
            tens = const std::mem::offset_of!(LeanTables, tens),
            low_nibbles = const std::mem::offset_of!(LeanTables, low_nibbles),
            id_kind_high = const std::mem::offset_of!(LeanTables, id_kind_high),
            id_kind_low = const std::mem::offset_of!(LeanTables, id_kind_low),
            id_value = const std::mem::offset_of!(LeanTables, id_value),
            zeros = const std::mem::offset_of!(LeanTables, zeros),
            id36_pairs = const std::mem::offset_of!(LeanTables, id36_pairs),
            id36_fours = const std::mem::offset_of!(LeanTables, id36_fours),
            id36_eights = const std::mem::offset_of!(LeanTables, id36_eights),
            id_ranges = const std::mem::offset_of!(LeanTables, id_ranges),
            offset_pattern = const std::mem::offset_of!(LeanTables, offset_pattern),
            offset_limit = const std::mem::offset_of!(LeanTables, offset_limit),
            offset_shuffle = const std::mem::offset_of!(LeanTables, offset_shuffle),
            offset_ceilings = const std::mem::offset_of!(LeanTables, offset_ceilings),
            offset_weights = const std::mem::offset_of!(LeanTables, offset_weights),
            form_mid = const std::mem::offset_of!(LeanTables, form_of_length) + 128,
            offset_form = const std::mem::offset_of!(LeanTables, forms) + std::mem::offset_of!(TimeForms, offset),
            plain_start = const std::mem::offset_of!(LeanTables, forms)
                + std::mem::offset_of!(TimeForms, plain)
                + std::mem::offset_of!(TimeForm, start),
            day = const std::mem::offset_of!(LeanTables, forms) + std::mem::offset_of!(TimeForms, day),
            f_pattern = const std::mem::offset_of!(TimeForm, pattern),
            f_limit = const std::mem::offset_of!(TimeForm, limit),
            f_shuffle = const std::mem::offset_of!(TimeForm, shuffle),
            f_ceilings = const std::mem::offset_of!(TimeForm, ceilings),
            f_first = const std::mem::offset_of!(TimeForm, first),
            f_second = const std::mem::offset_of!(TimeForm, second),
            f_year_mask = const std::mem::offset_of!(TimeForm, year_mask),
            f_scale = const std::mem::offset_of!(TimeForm, scale),
            f_start = const std::mem::offset_of!(TimeForm, start),
            f_day = const FORM_DAY,
            next = const std::mem::offset_of!(LeanFrame, next),
            window_length = const std::mem::offset_of!(LeanFrame, window_length),
            inverse_b = const std::mem::offset_of!(LeanFrame, inverse_b),
            inverse_a = const std::mem::offset_of!(LeanFrame, inverse_a),
            event_key = const std::mem::offset_of!(LeanFrame, event_key),
            user_key = const std::mem::offset_of!(LeanFrame, user_key),
            index_limit = const std::mem::offset_of!(LeanFrame, index_limit),
            user_limit = const std::mem::offset_of!(LeanFrame, user_limit),
            region = const std::mem::offset_of!(LeanFrame, region),
            reach = const std::mem::offset_of!(LeanFrame, reach),
            last_comma = const std::mem::offset_of!(LeanFrame, last_comma),
            opened = const std::mem::offset_of!(LeanFrame, opened),
            options(nostack),
        )
    };
}

/// What the kernel in assembly needs besides the tables, and where it leaves
/// what does not come back in registers.
#[repr(C)]
pub struct LeanFrame {
    /// Where the next row starts if this one ends where the early look for
    /// its newline said. The kernel takes no row that ends elsewhere.
    pub next: *const u8,
    window_length: u64,
    // `unscramble` with numbers in the top 40 bits: its two inverses, both
    // keys and both bounds, moved up by 24 bits.
    inverse_b: u64,
    inverse_a: u64,
    event_key: u64,
    user_key: u64,
    index_limit: u64,
    user_limit: u64,
    // A purchase: the comma after its type, and from there the newline, the
    // comma between product and price (as a bit) and which of the two open
    // with a quote.
    region: *const u8,
    reach: u32,
    last_comma: u32,
    opened: u32,
    // For the loop in assembly: where it stops, the writers of the index
    // partitions, which bits of an index (in the top 40 bits) are its
    // partition, and which bits of the index at bit 48 a record keeps.
    last: *const u8,
    writers: *mut PartWriter,
    part_mask: u64,
    low_mask: u64,
    /// What the loop read of the row it hands back: ts, ingest seconds,
    /// index, user number and the validity mask.
    handed: [u64; 5],
    /// The caller's rbx while the loop runs.
    saved: u64,
    // Purchases: the categories by product number, and what a valid
    // purchase read for one row cost and is (price low, category high).
    categories: *const u32,
    category_count: u64,
    bought: u64,
    /// `rare_rows()`.
    rare: u64,
}

/// Whether a `PartWriter` is what the loop in assembly takes it for: four
/// words, the cursor first and the end of the block second, a record of two
/// words going where the cursor is as long as the end is not passed. Asked
/// of `reserve` itself, once: with room for exactly one record it must hand
/// out the cursor and move it on, and nothing else (with the end anywhere
/// else there would be no room, and a new block).
fn writer_as_assumed() -> bool {
    static ASSUMED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ASSUMED.get_or_init(|| {
        if std::mem::size_of::<PartWriter>() != 32 {
            return false;
        }
        let mut block = [0u64; 8];
        let base = block.as_mut_ptr();
        let mut arena = super::arena::Arena::new();
        // SAFETY: four pointers into `block`, as a writer has them; whatever
        // their order, `reserve` and `grow` only write `block[1..3]`.
        let assumed = unsafe {
            let words: [*mut u64; 4] = [base, base.add(2), base.add(1), base.add(1)];
            let mut writer: PartWriter = std::mem::transmute(words);
            let at = writer.reserve(&mut arena, super::EVENT_BLOCK, 2);
            let after: [*mut u64; 4] = std::mem::transmute(writer);
            at == base && after == [base.add(2), base.add(2), base.add(1), base.add(1)]
        };
        if std::env::var_os("SS_TIMING").is_some() {
            eprintln!("SS_TIMING note=event_store_in_assembly={assumed}");
        }
        assumed
    })
}

impl LeanFrame {
    /// The frame of a chunk in index mode (`ctx.keys` is set).
    pub fn new(ctx: &Ctx) -> Self {
        let keys = ctx.keys.as_ref().expect("the kernel in assembly is for index mode");
        // `unscramble` is `(pre(s) ^ key) * A' & MASK` and `pre(s)` is
        // `f ^ f >> 17 ^ f >> 34` with `f = s * B' & MASK`: so A' is
        // `unscramble(0, 1)`, and B' is `pre(1)` with its shifts undone.
        let inverse_a = unscramble(0, 1);
        let folded = pre(1);
        let inverse_b = folded ^ (folded >> 17);
        // A bound past every 40-bit number: nothing is at or above it.
        let limit = |bound: u64| if bound > NUMBER_MASK { u64::MAX } else { bound << 24 };
        Self {
            next: std::ptr::null(),
            window_length: (ctx.window_end - ctx.window_start) as u64,
            inverse_b: inverse_b << 24,
            inverse_a: inverse_a << 24,
            event_key: (keys.event_key & NUMBER_MASK) << 24,
            user_key: (keys.user_key & NUMBER_MASK) << 24,
            index_limit: limit(keys.index_bound),
            // A user number is its index times 512 plus an alias.
            user_limit: limit(keys.user_bound.saturating_mul(512)),
            region: std::ptr::null(),
            reach: 0,
            last_comma: 0,
            opened: 0,
            last: std::ptr::null(),
            writers: std::ptr::null_mut(),
            part_mask: u64::MAX << (24 + keys.index_shift),
            low_mask: ((1u64 << keys.index_shift) - 1) << 48,
            handed: [0; 5],
            saved: 0,
            categories: ctx.category_by_product_number.as_ptr(),
            category_count: ctx.category_by_product_number.len() as u64,
            bought: 0,
            rare: rare_rows(),
        }
    }

    /// Whether `event_lean_run` can store for this run: the writers are as
    /// its store takes them, and an index within a partition fits the 16
    /// bits a record has for it.
    pub fn stores(ctx: &Ctx) -> bool {
        ctx.keys.as_ref().is_some_and(|keys| keys.index_shift <= 16) && writer_as_assumed()
    }

    /// Timing only: every row into the first writer `event_lean_run` is
    /// given, whatever its index.
    pub fn one_list(&mut self) {
        self.part_mask = 0;
    }

    /// What `event_lean_run` read of the row it handed back with `status`.
    pub fn read(&self, status: u64) -> LeanRow {
        let [ts, ingest_seconds, index, user_number, keep] = self.handed;
        LeanRow { status, ts, ingest_seconds, index, user_number, keep }
    }
}

/// What the kernel in assembly read of a row.
#[derive(Clone, Copy)]
pub struct LeanRow {
    /// 0: a row that is read and is no valid purchase. 4: a valid purchase
    /// (`event_lean_bought` ends it). 2: a purchase, read up to its product
    /// and price (`event_lean_purchase` reads those). 1: declined, and the
    /// rest means nothing.
    pub status: u64,
    /// Milliseconds since the window's start, whatever the row's validity.
    pub ts: u64,
    pub ingest_seconds: u64,
    pub index: u64,
    pub user_number: u64,
    /// All ones for a valid row (ts in the window, the row not late), else zero.
    pub keep: u64,
}

/// The lean kernel in assembly on the row at `p`, in index mode.
///
/// # Safety
/// `p` must have `ROW_PAD` readable bytes after it and 16 before it; `lean`
/// comes from `lean_setup` and `frame` from `LeanFrame::new` with the same
/// `ctx`.
#[inline(always)]
pub unsafe fn event_lean_asm(lean: *const LeanTables, p: *const u8, frame: *mut LeanFrame) -> LeanRow {
    let (status, ts, ingest_seconds, index, user_number, keep): (u64, u64, u64, u64, u64, u64);
    lean_asm!(
        [
            lean_row!(),
            // The numbers come back at the bottom of their words.
            "shr r9, 24",
            "shr r10, 24",
        ]
        in("rdi") p,
        in("rsi") lean,
        in("rdx") frame,
        lateout("r15") status,
        lateout("rax") ts,
        lateout("rcx") ingest_seconds,
        lateout("r9") index,
        lateout("r10") user_number,
        lateout("r12") keep,
        out("r8") _, out("r11") _, out("r13") _, out("r14") _,
        out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _, out("ymm4") _,
        bought = const std::mem::offset_of!(LeanFrame, bought),
    );
    LeanRow { status, ts, ingest_seconds, index, user_number, keep }
}

/// The lean kernel in assembly on the rows from `p` on, storing every
/// `view` and `cart` it reads in `out`, until a row starts at or after
/// `last` or one needs the caller. Returns a status and a row:
///   0: nothing more to do; the next row is the one returned (at or after `last`).
///   1: the kernel declines the row returned.
///   2, 3: the row returned was read but not stored: a purchase (2,
///      `event_lean_purchase` ends it) or a row whose list needs a new
///      block (3, `event_lean_common` stores it). `frame.read(status)` is
///      what was read, and the next row is at `frame.next`.
/// Without `STORE` (timing only) nothing is stored and 3 does not happen.
///
/// # Safety
/// `LeanFrame::stores(ctx)` holds for the `ctx` of `lean` and `frame`; `p`
/// is before `last`, and every row start before `last` has `ROW_PAD`
/// readable bytes after it and 16 before it. `writers` are the writers of
/// that `ctx`'s index partitions (`ParseOut::indexed`, taken anew after
/// every other use of the `ParseOut`), or, after `LeanFrame::one_list`, one
/// writer's worth of words: a cursor and the end of its block.
#[inline(always)]
pub unsafe fn event_lean_run<const STORE: bool>(
    lean: *const LeanTables,
    p: *const u8,
    last: *const u8,
    frame: *mut LeanFrame,
    writers: *mut PartWriter,
) -> (u64, *const u8) {
    (*frame).last = last;
    (*frame).writers = writers;
    let status: u64;
    let at: *const u8;
    if STORE {
        lean_asm!(
            [lean_run!(lean_store, lean_bought_store),]
            inout("rdi") p => at,
            in("rsi") lean,
            in("rdx") frame,
            lateout("r15") status,
            out("rax") _, out("rcx") _, out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            out("r12") _, out("r13") _, out("r14") _,
            out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _, out("ymm4") _,
            last = const std::mem::offset_of!(LeanFrame, last),
            writers = const std::mem::offset_of!(LeanFrame, writers),
            part_mask = const std::mem::offset_of!(LeanFrame, part_mask),
            low_mask = const std::mem::offset_of!(LeanFrame, low_mask),
            read = const std::mem::offset_of!(LeanFrame, handed),
            saved = const std::mem::offset_of!(LeanFrame, saved),
        );
    } else {
        lean_asm!(
            [lean_run!(lean_no_store, lean_bought_dropped),]
            inout("rdi") p => at,
            in("rsi") lean,
            in("rdx") frame,
            lateout("r15") status,
            out("rax") _, out("rcx") _, out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            out("r12") _, out("r13") _, out("r14") _,
            out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _, out("ymm4") _,
            last = const std::mem::offset_of!(LeanFrame, last),
            read = const std::mem::offset_of!(LeanFrame, handed),
            saved = const std::mem::offset_of!(LeanFrame, saved),
        );
    }
    (status, at)
}

/// The end of a purchase `event_lean_asm` read (status 2): its price and its
/// product's category, as `event_lean` reads them, and then the row is
/// stored or handed back like any other.
///
/// # Safety
/// `frame` is the one `event_lean_asm` just filled for this row.
#[inline(always)]
pub unsafe fn event_lean_purchase<const MODE: u32>(
    ctx: &Ctx,
    frame: &LeanFrame,
    read: LeanRow,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> u32 {
    let mut valid = read.keep != 0;
    let mut purchase = None;
    if valid {
        let region_at = frame.region;
        let reach = frame.reach as usize;
        let r5 = frame.last_comma.trailing_zeros() as usize;
        let q5 = (frame.opened & 1) as usize;
        let q6 = ((frame.opened >> 1) & 1) as usize;
        let price = std::slice::from_raw_parts(region_at.add(r5 + 1 + q6), reach - r5 - 1 - 2 * q6);
        match parse_price(price) {
            Some(price) => purchase = Some((price, product_category(ctx, region_at.add(1 + q5), r5 - 1 - 2 * q5))),
            None => valid = false,
        }
    }
    event_lean_finish::<MODE, true>(
        out,
        row,
        read.index,
        read.user_number,
        read.ts,
        valid,
        read.ingest_seconds as i64,
        purchase,
    )
}

/// The end of a valid purchase `event_lean_asm` read whole (status 4): its
/// price and category are in the frame.
///
/// # Safety
/// `frame` is the one `event_lean_asm` just filled for this row.
#[inline(always)]
pub unsafe fn event_lean_bought<const MODE: u32>(
    frame: &LeanFrame,
    read: LeanRow,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> u32 {
    let purchase = Some((frame.bought as u32, (frame.bought >> 32) as u32));
    event_lean_finish::<MODE, true>(
        out,
        row,
        read.index,
        read.user_number,
        read.ts,
        true,
        read.ingest_seconds as i64,
        purchase,
    )
}

/// The end of a row `event_lean_asm` read that is no valid purchase (status 0).
///
/// # Safety
/// As `event_lean_finish`.
#[inline(always)]
pub unsafe fn event_lean_common<const MODE: u32>(read: LeanRow, out: &mut ParseOut, row: &mut EventRow) -> u32 {
    if MODE == MODE_STORE {
        // What `event_lean_finish` stores, with the row's validity as the
        // mask it comes as: an invalid row keeps neither its time nor its
        // user, and a valid one's user is its number.
        let valid = read.keep & 1;
        emit_indexed(
            out,
            read.index,
            read.ts & read.keep,
            valid,
            read.user_number & read.keep,
            valid,
            read.ingest_seconds,
            None,
        );
        return ROW_DONE;
    }
    event_lean_finish::<MODE, true>(
        out,
        row,
        read.index,
        read.user_number,
        read.ts,
        read.keep != 0,
        read.ingest_seconds as i64,
        None,
    )
}

/// The tables of the lean kernel and the window's length, for `event_lean`.
#[inline(always)]
pub fn lean_setup(ctx: &Ctx) -> (*const LeanTables, u64) {
    (&ctx.ts_tables.lean, (ctx.window_end - ctx.window_start) as u64)
}

/// `event_lean` for the fast loop: see there.
///
/// # Safety
/// `p` must have `ROW_PAD` readable bytes after it and one before it;
/// `lean` and `window_length` come from `lean_setup`.
#[inline(always)]
pub unsafe fn event_row_lean_step<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    lean: *const LeanTables,
    window_length: u64,
    p: *const u8,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> (usize, usize, u32) {
    event_lean::<MODE, INDEXED>(ctx, lean, window_length, p, out, row)
}

#[cfg(test)]
thread_local! {
    /// Rows the lean event kernel declined on this thread.
    static DECLINED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Purchases the kernel in assembly finished by itself on this thread,
    /// and purchases it left to its caller.
    static BOUGHT: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
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
    let (length, _, status) = event_lean::<MODE, INDEXED>(ctx, lean, window_length, p, out, row);
    if status == ROW_DECLINE {
        #[cfg(test)]
        DECLINED.with(|count| count.set(count.get() + 1));
        if !INDEXED {
            let (length, status) = event_lean_long::<MODE>(ctx, p, out, row);
            if status != ROW_DECLINE {
                return (length, status);
            }
        }
        return event_row_fast::<MODE, INDEXED>(ctx, p, out, row);
    }
    (length, status)
}

/// As `event_row_lean` in index mode, with the kernel in assembly in front
/// of that: the same answers again. `frame` is the chunk's.
///
/// # Safety
/// As `event_row_lean`, and 16 bytes before `p` must be readable too.
#[inline(always)]
pub unsafe fn event_row_lean_asm<const MODE: u32>(
    ctx: &Ctx,
    frame: &mut LeanFrame,
    p: *const u8,
    out: &mut ParseOut,
    row: &mut EventRow,
) -> (usize, u32) {
    let (lean, _window_length) = lean_setup(ctx);
    let read = event_lean_asm(lean, p, frame);
    #[cfg(test)]
    if MODE == MODE_ROW {
        // The assembly must take exactly the rows `event_lean` takes that
        // end where the early look said, and read the same from them.
        let mut twin = EventRow::default();
        let (length, guess, status) = event_lean::<MODE, true>(ctx, lean, _window_length, p, out, &mut twin);
        let taken = status == ROW_EMIT && length == guess;
        assert_eq!(read.status != 1, taken, "the assembly and the lean kernel take different rows");
        if taken {
            assert_eq!(frame.next, p.add(length + 1), "the assembly ends the row elsewhere");
            // A purchase comes back as one the assembly finished (4), one
            // for the caller (2), or as the invalid row it is (0).
            assert!(read.status == 0 || purchase_text(p, length), "a purchase or not");
            assert!(read.status != 0 || twin.purchase.is_none(), "a purchase or not");
            let mut mine = EventRow::default();
            let finished = match read.status {
                2 => event_lean_purchase::<MODE>(ctx, frame, read, out, &mut mine),
                4 => event_lean_bought::<MODE>(frame, read, out, &mut mine),
                _ => event_lean_common::<MODE>(read, out, &mut mine),
            };
            assert!(finished == ROW_EMIT && mine == twin, "the assembly reads {mine:?}, the lean kernel {twin:?}");
        }
    }
    if read.status == 0 {
        let length = frame.next.offset_from(p) as usize - 1;
        return (length, event_lean_common::<MODE>(read, out, row));
    }
    #[cfg(test)]
    BOUGHT.with(|count| {
        let (whole, left) = count.get();
        count.set((whole + usize::from(read.status == 4), left + usize::from(read.status == 2)));
    });
    if read.status == 2 {
        let length = frame.next.offset_from(p) as usize - 1;
        return (length, event_lean_purchase::<MODE>(ctx, frame, read, out, row));
    }
    if read.status == 4 {
        let length = frame.next.offset_from(p) as usize - 1;
        return (length, event_lean_bought::<MODE>(frame, read, out, row));
    }
    event_row_lean::<MODE, true>(ctx, p, out, row)
}

/// Whether the row at `p` has `purchase` as its fifth field (in a test: the
/// row is one the lean kernel took).
#[cfg(test)]
unsafe fn purchase_text(p: *const u8, length: usize) -> bool {
    let line = std::slice::from_raw_parts(p, length);
    let field = line.split(|&byte| byte == b',').nth(4).unwrap_or(b"");
    field == b"purchase" || field == b"\"purchase\""
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
/// it). Rows of up to 127 bytes take the fast path: a click row with an ID of
/// 33 characters (current generated data has them) is about 70 to 100.
///
/// With `INDEXED` (`ctx.keys` must be set), a generated user ID comes back
/// as its user number (`user_is_number`), so that storing the click does not
/// have to turn a code back into a number; any other user ID as its code.
#[inline(always)]
pub unsafe fn click_row_fast<const INDEXED: bool>(
    ctx: &Ctx,
    p: *const u8,
    click: &mut ClickRecord,
) -> (usize, u32) {
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let comma = _mm256_set1_epi8(b',' as i8);
    let quote = _mm256_set1_epi8(b'"' as i8);
    let blocks = [
        _mm256_loadu_si256(p as *const __m256i),
        _mm256_loadu_si256(p.add(32) as *const __m256i),
        _mm256_loadu_si256(p.add(64) as *const __m256i),
        _mm256_loadu_si256(p.add(96) as *const __m256i),
    ];
    let wide = |byte: __m256i| -> u128 {
        let low = mask(blocks[0], byte) | (mask(blocks[1], byte) << 32);
        let high = mask(blocks[2], byte) | (mask(blocks[3], byte) << 32);
        (u128::from(high) << 64) | u128::from(low)
    };
    let newlines = wide(newline);
    if newlines == 0 {
        return (0, ROW_LONG);
    }
    let length = newlines.trailing_zeros() as usize;
    let below = (1u128 << length) - 1;
    let commas = wide(comma) & below;
    let quotes = wide(quote) & below;
    // Four fields and a first field that is not empty (an empty click_id
    // drops the line just as a wrong field count does).
    if commas.count_ones() != 3 || commas & 1 != 0 {
        return (length, ROW_DROP);
    }
    // Same quote rule as for events, in two words.
    let separators = commas | (1u128 << length);
    let starts = (separators << 1) | 1;
    let ends = separators >> 1;
    let pext128 = |bits: u128, select: u128| -> u64 {
        let (low, high) = (bits as u64, (bits >> 64) as u64);
        let (select_low, select_high) = (select as u64, (select >> 64) as u64);
        _pext_u64(low, select_low) | (_pext_u64(high, select_high) << select_low.count_ones())
    };
    let opened = pext128(quotes, starts);
    if ((quotes & !(starts | ends)) | (quotes & starts & ends)) != 0 || opened != pext128(quotes, ends) {
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
    if INDEXED {
        // A generated user ID is handed back as its user number, read from
        // the characters right-aligned: the same checks as in `event_row_fast`.
        let keys = ctx.keys.as_ref().unwrap_unchecked();
        let (value, _, known) = two_numbers36(user_text << ((8usize.wrapping_sub(user_chars) & 7) * 8), 0);
        let number = unscramble(value, keys.user_key);
        let generated = (*p.add(s1) == b'u')
            & canonical36(user_chars, user_text as u8, known, value)
            & ((number >> 9) < keys.user_bound);
        if generated {
            *click = ClickRecord {
                user: number,
                user_is_number: true,
                code_and_campaign: click_code | (u64::from(campaign) << 42),
                ts: (ts_ms - click_window_start) as u32,
            };
            return (length, ROW_EMIT);
        }
    }
    let user = if user_fast { user_fast_code } else { id_code(field(s1, e1), b'u', &ctx.user_ids) };
    *click = ClickRecord {
        user,
        user_is_number: false,
        code_and_campaign: click_code | (u64::from(campaign) << 42),
        ts: (ts_ms - click_window_start) as u32,
    };
    (length, ROW_EMIT)
}

// ---------------------------------------------------------------------------
// The lean click kernels.
//
// Like the lean event kernel, they take only rows of the generator's regular
// shape and decline every other row, which then goes through
// `click_row_fast` above (and from there, if need be, through the general
// parser). A row they take has been checked byte for byte: every byte of the
// row is a comma, a quote or the newline at a position the row's shape
// fixes, or part of a field whose whole text was compared or classified. So
// a row a lean kernel answers is well formed, its end is where the kernel
// says, and nothing about it is left to chance.
//
// The regular shape:
//   click_id    `k` and 7 or 8 characters of [0-9a-z]
//   user_id     `u` and 7 or 8 characters of [0-9a-z]
//               (index mode: a generated ID, see `Keys`)
//   campaign_id `c_` and digits, the first not `0`
//   ts          13 digits, or F1 or F2 on a day of the frame's day table
//   any field in quotes or not, the first three commas within bytes 4..36
//   of the row and its newline within bytes 32..64.
// What varies from row to row at random (the quotes, the ID lengths, the
// three timestamp forms) is never decided by a branch: the branches left are
// the checks that almost never fail.
//
// There are two of them. `click_row_lean` works the row's layout out with
// arithmetic on the quotes it finds; `click_row_shapes`, the one that runs,
// looks the layout up in tables and so needs about a third fewer
// instructions. The first stays for A/B runs: `SS_CLICK_KERNEL=lean`.
// ---------------------------------------------------------------------------

/// The third click kernel a fast loop can run, after `KERNEL_GENERAL`
/// (`click_row_fast` alone) and `KERNEL_LEAN` (`click_row_lean`):
/// `click_row_shapes`.
pub const KERNEL_SHAPES: u32 = 2;

/// The click kernel of this run: the shape-table one unless
/// `SS_CLICK_KERNEL` asks for `click_row_lean` (`lean`) or for
/// `click_row_fast` alone (`general`), for A/B runs.
pub fn click_kernel() -> u32 {
    static KERNEL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *KERNEL.get_or_init(|| match std::env::var("SS_CLICK_KERNEL").as_deref() {
        Ok("general") => KERNEL_GENERAL,
        Ok("lean") => KERNEL_LEAN,
        _ => KERNEL_SHAPES,
    })
}

const fn bytes32(low: [u8; 16], high: [u8; 16]) -> [u8; 32] {
    let mut joined = [0u8; 32];
    let mut at = 0;
    while at < 16 {
        joined[at] = low[at];
        joined[16 + at] = high[at];
        at += 1;
    }
    joined
}

const fn words16(low: [i16; 8], high: [i16; 8]) -> [i16; 16] {
    let mut joined = [0i16; 16];
    let mut at = 0;
    while at < 8 {
        joined[at] = low[at];
        joined[8 + at] = high[at];
        at += 1;
    }
    joined
}

/// A shuffle index that gives a zero byte.
const NO: u8 = 0x80;
/// A limit that every byte meets.
const ANY: u8 = 255;
/// A ceiling that no number of two digits exceeds.
const TOP: i16 = i16::MAX;

// The low half of the ISO forms, `-MM-DDTHH:MM:SS` and the byte after the
// seconds. After the shuffle the lanes are month, day, -, -, hour, minute,
// second, -. Round one: `month * 32 + day`, -, `hour * 60 + minute`, second.
// Round two: `month * 32 + day` and the second of the day.
const ISO_PATTERN: [u8; 16] = *b"-00-00T00:00:00Z";
const ISO_LIMIT: [u8; 16] = [0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0];
const ISO_SHUFFLE: [u8; 16] = [1, 2, 4, 5, NO, NO, NO, NO, 7, 8, 10, 11, 13, 14, NO, NO];
// Month and day must stay below 13 and 32: `month * 32 + day` names the day,
// and a larger day would name another one (day 33 of September, October 1).
const ISO_CEILINGS: [i16; 8] = [12, 31, TOP, TOP, 23, 59, 59, TOP];
const ISO_FIRST: [i16; 8] = [32, 1, 0, 0, 60, 1, 1, 0];
const ISO_SECOND: [i16; 8] = [1, 0, 60, 1, 0, 0, 0, 0];

/// 62 days in a row touch at most four months, which differ in `month & 3`.
const CLICK_DAY_SLOTS: usize = 128;

/// What the lean click kernels need of the run's window, made once per
/// chunk of rows. It lives in the frame of the loop that runs the kernel,
/// so the compiler knows that nothing the loop stores can change it, and it
/// keeps the kernels independent of the event kernels' tables.
pub struct ClickFrame {
    /// The start of the click window: epoch ms of `window_start - 7 days`.
    since: i64,
    /// The click window's length in ms.
    span: u64,
    /// The midnight of day 0 of the day table: in ms from `since`, and in
    /// seconds from the epoch.
    base: i64,
    base_seconds: i64,
    /// The days an ISO timestamp near the window can name: the 62 from 9
    /// days before the window's first day (the click window and a day of
    /// UTC offset on either side of any window the engine takes). Slot
    /// `(month & 3) << 5 | day` has the one table day that lands there, as
    /// `year text | (month * 32 + day) << 32`, so a slot can only ever
    /// answer for the day stored in it; `u64::MAX` (which no row can
    /// produce) where there is none.
    day_key: [u64; CLICK_DAY_SLOTS],
    /// Seconds from day 0 to the start of the slot's day.
    day_seconds: [u32; CLICK_DAY_SLOTS],
}

impl ClickFrame {
    pub fn new(ctx: &Ctx) -> Self {
        let since = ctx.window_start - WEEK_MS;
        let base_day = ctx.window_start.div_euclid(DAY_MS) - 9;
        let mut day_key = [u64::MAX; CLICK_DAY_SLOTS];
        let mut day_seconds = [0u32; CLICK_DAY_SLOTS];
        for offset in 0..62 {
            let (year, month, day) = civil_from_days(base_day + offset);
            // Only a year of four digits has a text a timestamp can hold.
            if !(0..=9999).contains(&year) {
                continue;
            }
            let digit = |power: i64| b'0' + (year / power % 10) as u8;
            let text = u32::from_le_bytes([digit(1000), digit(100), digit(10), digit(1)]);
            let slot = (((month & 3) << 5) | day) as usize;
            day_key[slot] = u64::from(text) | (((month * 32 + day) as u64) << 32);
            day_seconds[slot] = (offset * 86_400) as u32;
        }
        Self {
            since,
            span: (ctx.window_end - since) as u64,
            base: base_day * DAY_MS - since,
            base_seconds: base_day * 86_400,
            day_key,
            day_seconds,
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Rows a lean click kernel declined on this thread.
    static CLICK_DECLINED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `click_row_fast`, out of line: the lean kernels' way out.
#[inline(never)]
unsafe fn click_row_declined<const INDEXED: bool>(ctx: &Ctx, p: *const u8, click: &mut ClickRecord) -> (usize, u32) {
    #[cfg(test)]
    CLICK_DECLINED.with(|count| count.set(count.get() + 1));
    click_row_fast::<INDEXED>(ctx, p, click)
}

// The first lean kernel: `click_row_lean`. It takes campaigns of 1 to 4
// digits. The commas and quotes of bytes 4..36 give it the quotes around
// each field, and with them where each field's text is.

/// The constants of `click_time`: its low 16 bytes work on the bytes 4..20
/// of an ISO timestamp, `-MM-DDTHH:MM:SSZ`; its high 16 bytes on one of two
/// other pieces of the field (see `CLICK_TIME`).
#[repr(C, align(32))]
struct TimeConstants {
    /// XOR with it leaves digit values where digits belong and zeros where
    /// the expected separators are.
    pattern: [u8; 32],
    /// Largest allowed value after the XOR; 255 for a byte that may be anything.
    limit: [u8; 32],
    /// Brings the digit pairs to the 16-bit lanes their numbers are wanted in.
    shuffle: [u8; 32],
    /// Largest allowed number per 16-bit lane.
    ceilings: [i16; 16],
    /// The weights of the two rounds that add the numbers up.
    first: [i16; 16],
    second: [i16; 16],
}

/// `[0]`: the high half reads 13 digits, given with the 3 bytes before
/// them: the digits in fours, then the first 5 and the last 8.
/// `[1]`: the high half reads the bytes 16..32 of an ISO timestamp, `:SS`
/// (which the low half checks) and then, in F2, `+HH:MM`: the offset in
/// minutes, then in seconds.
static CLICK_TIME: [TimeConstants; 2] = [
    TimeConstants {
        pattern: bytes32(ISO_PATTERN, [0, 0, 0, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48]),
        limit: bytes32(ISO_LIMIT, [ANY, ANY, ANY, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9]),
        shuffle: bytes32(ISO_SHUFFLE, [NO, NO, NO, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
        ceilings: words16(ISO_CEILINGS, [TOP; 8]),
        first: words16(ISO_FIRST, [100, 1, 100, 1, 100, 1, 100, 1]),
        second: words16(ISO_SECOND, [10_000, 1, 10_000, 1, 0, 0, 0, 0]),
    },
    TimeConstants {
        pattern: bytes32(ISO_PATTERN, [0, 0, 0, 0, b'0', b'0', b':', b'0', b'0', 0, 0, 0, 0, 0, 0, 0]),
        limit: bytes32(ISO_LIMIT, [ANY, ANY, ANY, ANY, 9, 9, 0, 9, 9, ANY, ANY, ANY, ANY, ANY, ANY, ANY]),
        shuffle: bytes32(ISO_SHUFFLE, [4, 5, 7, 8, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO]),
        ceilings: words16(ISO_CEILINGS, [23, 59, TOP, TOP, TOP, TOP, TOP, TOP]),
        first: words16(ISO_FIRST, [60, 1, 0, 0, 0, 0, 0, 0]),
        second: words16(ISO_SECOND, [60, 0, 0, 0, 0, 0, 0, 0]),
    },
];

// What `click_time` knows about a field, as bits: 0..16 and 16..32 are the
// two halves' bytes (in shape, and of no number over its ceiling), then
// whether the date is a day of the table and whether byte 19 is a sign.
const TIME_DAY: u64 = 1 << 32;
const TIME_SIGN: u64 = 1 << 33;

/// The bits a `ts` field of each length needs; a bit that is never set for
/// the lengths no regular timestamp has.
static TIME_NEED: [u64; 64] = {
    let mut need = [1u64 << 63; 64];
    // F3: thirteen digits.
    need[13] = 0xFFFF_0000;
    // F1: the 16 bytes from byte 4, with the `Z`, on a table day.
    need[20] = 0xFFFF | TIME_DAY;
    // F2: the same up to the seconds, a sign, `HH:MM` within bounds.
    need[25] = 0xFFFF_7FFF | TIME_DAY | TIME_SIGN;
    need
};

/// A click's `ts` field for the lean kernel: `length` bytes (fewer than
/// `TIME_NEED` has entries) at `start`, with 3 readable bytes before them
/// and 32 from `start` on. Returns the milliseconds from the start of the
/// click window to it, and whether the field is 13 digits or an F1 or F2
/// timestamp on a day of the frame's day table; if it is not, the
/// milliseconds mean nothing.
#[inline(always)]
unsafe fn click_time(frame: &ClickFrame, start: *const u8, length: usize) -> (i64, bool) {
    // Which form a row has is close to random: the form picks the constants
    // and the second 16 bytes, and nothing here branches on it.
    let is_digits = length == 13;
    let constants = &CLICK_TIME[usize::from(!is_digits)];
    let other = select_unpredictable(is_digits, start.wrapping_sub(3), start.wrapping_add(16));
    let text = _mm256_inserti128_si256::<1>(
        _mm256_castsi128_si256(_mm_loadu_si128(start.add(4) as *const __m128i)),
        _mm_loadu_si128(other as *const __m128i),
    );
    let values = _mm256_xor_si256(text, _mm256_load_si256(constants.pattern.as_ptr() as *const __m256i));
    let limit = _mm256_load_si256(constants.limit.as_ptr() as *const __m256i);
    let shaped = _mm256_cmpeq_epi8(_mm256_min_epu8(values, limit), values);
    let pairs = _mm256_shuffle_epi8(values, _mm256_load_si256(constants.shuffle.as_ptr() as *const __m256i));
    let numbers = _mm256_maddubs_epi16(pairs, _mm256_set1_epi16(0x010A));
    let over = _mm256_cmpgt_epi16(numbers, _mm256_load_si256(constants.ceilings.as_ptr() as *const __m256i));
    // A number over its ceiling takes two bits out, and every lane with a
    // ceiling lies under bits that the forms using it need.
    let good = _mm256_movemask_epi8(_mm256_andnot_si256(over, shaped)) as u32;
    let sums = _mm256_madd_epi16(numbers, _mm256_load_si256(constants.first.as_ptr() as *const __m256i));
    let folded = _mm256_madd_epi16(
        _mm256_packus_epi32(sums, sums),
        _mm256_load_si256(constants.second.as_ptr() as *const __m256i),
    );
    // `month * 32 + day` below the second of the day. Month and day are
    // within their bounds if the bits say so, and then the slot's key holds
    // for one day only.
    let iso = _mm256_extract_epi64::<0>(folded) as u64;
    let year = u64::from((start as *const u32).read_unaligned());
    let slot = (iso & (CLICK_DAY_SLOTS as u64 - 1)) as usize;
    let day_known = *frame.day_key.get_unchecked(slot) == (year | (iso << 32));
    let seconds = i64::from(*frame.day_seconds.get_unchecked(slot)) + (iso >> 32) as i64;
    // The other half: the first 5 digits below the last 8, or the offset in seconds.
    let extra = _mm256_extract_epi64::<2>(folded) as u64;
    let low = (extra & 0xFFFF_FFFF) as i64;
    let digits = low * 100_000_000 + (extra >> 32) as i64;
    // `+` is 43 and `-` is 45: an offset counts once, forwards or backwards.
    let sign = *start.add(19);
    let sign_known = sign.wrapping_sub(b'+') & !2 == 0;
    let offset = low * select_unpredictable(length == 25, 44 - i64::from(sign), 0);
    let iso_ms = frame.base + (seconds - offset) * 1_000;
    let known = u64::from(good) | (u64::from(day_known) << 32) | (u64::from(sign_known) << 33);
    let need = *TIME_NEED.get_unchecked(length);
    (select_unpredictable(is_digits, digits - frame.since, iso_ms), known & need == need)
}

/// The "is a known character" bits the lean click kernel expects of its
/// three fields, by the number of the campaign's digits (1 to 4): seven
/// characters of each ID (the eighth depends on the ID's length), and the
/// campaign's digits at the end of its 4 bytes.
static CLICK_KNOWN: [u32; 8] = {
    let mut known = [0u32; 8];
    let mut digits = 1;
    while digits <= 4 {
        known[digits] = 0x7F7F | (((0xF0 >> digits) & 0xF) << 16);
        digits += 1;
    }
    known
};

/// What the `u` in front of a 7-character user ID adds to its number when
/// the 8 bytes that end the ID are read as one: 30 (the digit `u`) * 36^7.
const PREFIX_AS_DIGIT: u64 = 30 * 78_364_164_096;

/// The lean kernel's work on the row starting at `p`: `ROW_DONE` (stored),
/// `ROW_EMIT` (in `click`) or `ROW_DROP` with the row's length, or
/// `ROW_DECLINE` for a row that is not of the regular shape.
#[inline(always)]
unsafe fn click_decode_lean<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    frame: &ClickFrame,
    p: *const u8,
    out: &mut ParseOut,
    click: &mut ClickRecord,
) -> (usize, u32) {
    const DECLINED: (usize, u32) = (0, ROW_DECLINE);
    // The row's structure: the commas and quotes of bytes 4..36, the newline
    // of bytes 32..64. No regular row has one of its first three commas or
    // its newline anywhere else, and the checks below leave no byte of the
    // row unexplained, so nothing is missed by not looking further.
    let head = p.add(4);
    let block = _mm256_loadu_si256(head as *const __m256i);
    let commas = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, _mm256_set1_epi8(b',' as i8))) as u32;
    let quotes = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, _mm256_set1_epi8(b'"' as i8))) as u32;
    let newlines = _mm256_movemask_epi8(_mm256_cmpeq_epi8(
        _mm256_loadu_si256(p.add(32) as *const __m256i),
        _mm256_set1_epi8(b'\n' as i8),
    )) as u32;
    let second = commas & commas.wrapping_sub(1);
    let third = second & second.wrapping_sub(1);
    if third == 0 || newlines == 0 {
        return DECLINED;
    }
    // The commas are at `head + t0`, `head + t1` and `head + t2`, the
    // newline at `p + 32 + end`.
    let t0 = commas.trailing_zeros() as usize;
    let t1 = second.trailing_zeros() as usize;
    let t2 = third.trailing_zeros() as usize;
    let end = newlines.trailing_zeros() as usize;

    // Bit `i` of `after` is a quote right after comma `i`, of `before` right
    // before it. A field opens with a quote exactly when it closes with one.
    let after = _pext_u32(quotes >> 1, commas);
    let before = _pext_u32(quotes << 1, commas);
    let q0 = usize::from(*p == b'"');
    let q1 = (after & 1) as usize;
    let q2 = ((after >> 1) & 1) as usize;
    let q3 = ((after >> 2) & 1) as usize;
    let opened = ((after << 1) | q0 as u32) & 15;
    let closed = (before & 7) | (u32::from(*p.add(31 + end) == b'"') << 3);

    // Where the fields are. The IDs have 7 or 8 characters (`*_extra` more
    // than 7), the campaign 1 to 4 digits; the lengths wrap for any other row.
    let click_extra = t0.wrapping_sub(2 * q0 + 4);
    let user = head.add(t0 + 1 + q1);
    let user_extra = t1.wrapping_sub(t0 + 2 * q1 + 9);
    let campaign = head.add(t1 + 1 + q2);
    let campaign_digits = t2.wrapping_sub(t1 + 2 * q2 + 3);
    let ts = head.add(t2 + 1 + q3);
    let ts_length = (end + 27).wrapping_sub(t2 + 2 * q3);
    if opened != closed
        || (click_extra | user_extra) > 1
        || campaign_digits.wrapping_sub(1) > 3
        || ts_length >= TIME_NEED.len()
        || *p.add(q0) != b'k'
        || *user != b'u'
        || (campaign as *const u16).read_unaligned() != u16::from_le_bytes(*b"c_")
        || *campaign.add(2) == b'0'
    {
        return DECLINED;
    }

    // Both IDs and the campaign's digits in one pass. An ID is the 8 bytes
    // after its prefix: its characters, or 7 of them and the quote or comma
    // that ends the field, which counts as no character. In index mode the
    // user ID is the 8 bytes that end it instead: its characters, or 7 of
    // them behind the prefix. The campaign is the 4 bytes that end it (they
    // start `t2 - q2` bytes into the row): its digits behind what is no
    // digit (the `_`, the `c`, a quote or a comma).
    let user_text = if INDEXED { user.add(user_extra) } else { user.add(1) };
    let ids = _mm_insert_epi64::<1>(_mm_loadl_epi64(p.add(q0 + 1) as *const __m128i), read_u64(user_text) as i64);
    let campaign_text = (p.add(t2 - q2) as *const u32).read_unaligned();
    let text = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(ids), _mm_cvtsi32_si128(campaign_text as i32));
    let ones = |byte: u8| (u64::from(byte) * 0x0101_0101_0101_0101) as i64;
    let per16 = |word: u16| (u64::from(word) * 0x0001_0001_0001_0001) as i64;
    let per32 = |dword: u32| (u64::from(dword) * 0x0000_0001_0000_0001) as i64;
    // Signed compares of bytes moved to the bottom of the range: a digit is
    // one of the 10 values from `0`, a letter one of the 26 from `a`. The
    // campaign has no letters.
    let is_digit = _mm256_cmpgt_epi8(_mm256_set1_epi8(-128 + 10), _mm256_add_epi8(text, _mm256_set1_epi8(0x50)));
    let is_letter = _mm256_cmpgt_epi8(
        _mm256_setr_epi64x(ones(0x80 + 26), ones(0x80 + 26), ones(0x80), ones(0x80)),
        _mm256_add_epi8(text, _mm256_set1_epi8(0x1F)),
    );
    let known = _mm256_or_si256(is_digit, is_letter);
    // A character of a code counts from 1 (`0` is 1, `a` is 11, as in
    // `two_codes`); a digit of a number (a user number in index mode, the
    // campaign's) is itself, and `a` is 10.
    let (user_zero, pair, four, eight) = if INDEXED {
        (b'0', 0x0124, 0x0001_0510, 1_679_616)
    } else {
        (b'0' - 1, 0x0125, 0x0001_0559, 1_874_161)
    };
    let from_zero = _mm256_sub_epi8(text, _mm256_setr_epi64x(ones(b'0' - 1), ones(user_zero), ones(b'0'), ones(b'0')));
    let from_letter = _mm256_and_si256(is_letter, _mm256_set1_epi8(b'a' as i8 - b'0' as i8 - 10));
    let values = _mm256_and_si256(_mm256_sub_epi8(from_zero, from_letter), known);
    // Most significant character first: pairs, fours, then all eight. The
    // fours are below 2^21, so the signed multiply is the plain one.
    let pairs =
        _mm256_maddubs_epi16(values, _mm256_setr_epi64x(per16(0x0125), per16(pair), per16(0x010A), per16(0x010A)));
    let fours = _mm256_madd_epi16(
        pairs,
        _mm256_setr_epi64x(per32(0x0001_0559), per32(four), per32(0x0001_0064), per32(0x0001_0064)),
    );
    let wide = _mm256_add_epi64(
        _mm256_mul_epi32(fours, _mm256_setr_epi64x(1_874_161, eight, 1, 1)),
        _mm256_srli_epi64::<32>(fours),
    );
    // Every character of the IDs known, and the campaign's digits exactly
    // where its length says.
    let user_known = if INDEXED { 0x8000 } else { (user_extra as u32) << 15 };
    let expected = *CLICK_KNOWN.get_unchecked(campaign_digits) | ((click_extra as u32) << 7) | user_known;
    if _mm256_movemask_epi8(known) as u32 != expected {
        return DECLINED;
    }
    let click_code = _mm256_extract_epi64::<0>(wide) as u64;
    let user_wide = _mm256_extract_epi64::<1>(wide) as u64;
    // The table holds every campaign written this way, so a number that is
    // not in it, or lies beyond it, is no campaign: `u32::MAX`.
    let campaign_number = _mm256_extract_epi64::<2>(wide) as usize;
    let campaigns = &ctx.campaign_by_number;
    let campaign =
        if campaign_number < campaigns.len() { *campaigns.get_unchecked(campaign_number) } else { u32::MAX };

    let (since, time_known) = click_time(frame, ts, ts_length);
    if !time_known {
        return DECLINED;
    }

    let user_value = if INDEXED {
        // The user must be a generated ID: no leading zero, a number of 40
        // bits, a user index within the keys' bound. Any other ID is the
        // general kernel's.
        let keys = ctx.keys.as_ref().unwrap_unchecked();
        let value = user_wide.wrapping_sub((user_extra as u64).wrapping_sub(1) & PREFIX_AS_DIGIT);
        let number = unscramble(value, keys.user_key);
        if *user.add(1) == b'0' || value > NUMBER_MASK || (number >> 9) >= keys.user_bound {
            return DECLINED;
        }
        number
    } else {
        user_wide
    };

    // The row is well formed and ends at the newline found: what is left is
    // the click's own validity.
    let length = 32 + end;
    if since as u64 >= frame.span || campaign == u32::MAX {
        return (length, ROW_DROP);
    }
    let code_and_campaign = click_code | (u64::from(campaign) << 42);
    if MODE == MODE_STORE {
        if INDEXED {
            // SAFETY (of the call): the user index was checked against the keys' bound.
            emit_number_click(out, user_value, code_and_campaign, since as u64);
        } else {
            emit_code_click(out, user_value, code_and_campaign, since as u64);
        }
        return (length, ROW_DONE);
    }
    *click = ClickRecord { user: user_value, user_is_number: INDEXED, code_and_campaign, ts: since as u32 };
    (length, ROW_EMIT)
}

/// Parses the click row starting at `p` (with `ROW_PAD` readable bytes after
/// it): the lean kernel, and `click_row_fast` for the rows it declines. In
/// `MODE_STORE` a row the lean kernel takes is stored in `out` (`ROW_DONE`);
/// in `MODE_ROW` it comes back in `click` (`ROW_EMIT`), in index mode with
/// the user as a user number. Everything else is `click_row_fast`'s answer.
#[inline(always)]
pub unsafe fn click_row_lean<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    frame: &ClickFrame,
    p: *const u8,
    out: &mut ParseOut,
    click: &mut ClickRecord,
) -> (usize, u32) {
    let (length, status) = click_decode_lean::<MODE, INDEXED>(ctx, frame, p, out, click);
    if status == ROW_DECLINE {
        return click_row_declined::<INDEXED>(ctx, p, click);
    }
    (length, status)
}

// The shape-table kernel: `click_row_shapes`. It takes campaigns of 1 to 6
// digits. The distances between the row's first three commas and its
// newline say what each field must look like (how many characters, quoted
// or not, which timestamp form), and a table row per field holds what is
// needed to check exactly that and to read the field: a shuffle that brings
// the field's characters and the bytes that frame them to fixed places, and
// what those bytes must be. A distance that belongs to no regular field has
// a table row that nothing passes, so there is no branch for a row with too
// few commas or no newline either.

/// One of the first three fields of a click row, as the lean kernel reads
/// it out of the 16 bytes that start at the field (the click ID) or at the
/// comma before it (the user ID, the campaign).
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct FieldShape {
    /// Brings the field's characters to bytes 0..8 (zero where it has none)
    /// and the bytes that frame them (prefix, quotes) to bytes 8..16.
    shuffle: [u8; 16],
    /// What bytes 8..16 must be after the shuffle; zeros in bytes 0..8.
    expected: [u8; 16],
    /// Which of the kernel's 24 character bytes (8 per field, the click
    /// ID's lowest) must be characters the field is made of.
    known: u32,
    /// The least number the characters may stand for: one without a
    /// leading zero. Zero where the characters are read as a code.
    least: u64,
}

/// The shape of no field: its framing bytes come out as zeros and are
/// expected to be 0xFF, so no row passes it.
const NO_FIELD: FieldShape = FieldShape {
    shuffle: [NO; 16],
    expected: [0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0, 0, 0, 0, 0, 0, 0],
    known: 0,
    least: 0,
};

/// An ID: `prefix` and `chars` characters (7 or 8), in quotes or not, that
/// starts at byte `start` of its 16 bytes. The characters go to bytes 0..8
/// from the left (to be read as a code: a missing eighth character counts
/// as none) or to the right (to be read as a number); `field` (0, 1 or 2)
/// says which 8 of the kernel's character bytes they become.
const fn id_shape(start: usize, prefix: u8, chars: usize, quoted: bool, right: bool, field: u32) -> FieldShape {
    let mut shape = NO_FIELD;
    let prefix_at = start + quoted as usize;
    shape.shuffle[8] = prefix_at as u8;
    shape.expected[8] = prefix;
    if quoted {
        shape.shuffle[9] = start as u8;
        shape.expected[9] = b'"';
        shape.shuffle[10] = (prefix_at + 1 + chars) as u8;
        shape.expected[10] = b'"';
    }
    let first = if right { 8 - chars } else { 0 };
    let mut at = 0;
    while at < chars {
        shape.shuffle[first + at] = (prefix_at + 1 + at) as u8;
        at += 1;
    }
    shape.known = (((1u32 << chars) - 1) << first) << (8 * field);
    shape.least = if right { 36u64.pow(chars as u32 - 1) } else { 0 };
    shape
}

/// A campaign: `c_` and `digits` digits (1 to 6), in quotes or not, after
/// the comma at byte 0 of its 16 bytes. The digits go to the right of bytes
/// 0..8, to be read as a number.
const fn campaign_shape(digits: usize, quoted: bool) -> FieldShape {
    let mut shape = NO_FIELD;
    let name_at = 1 + quoted as usize;
    shape.shuffle[8] = name_at as u8;
    shape.expected[8] = b'c';
    shape.shuffle[9] = (name_at + 1) as u8;
    shape.expected[9] = b'_';
    if quoted {
        shape.shuffle[10] = 1;
        shape.expected[10] = b'"';
        shape.shuffle[11] = (name_at + 2 + digits) as u8;
        shape.expected[11] = b'"';
    }
    let mut at = 0;
    while at < digits {
        shape.shuffle[8 - digits + at] = (name_at + 2 + at) as u8;
        at += 1;
    }
    shape.known = (((1u32 << digits) - 1) << (8 - digits)) << 16;
    shape.least = 10u64.pow(digits as u32 - 1);
    shape
}

const fn click_shapes() -> [FieldShape; 33] {
    let mut shapes = [NO_FIELD; 33];
    shapes[4] = id_shape(0, b'k', 7, false, false, 0);
    shapes[5] = id_shape(0, b'k', 8, false, false, 0);
    shapes[6] = id_shape(0, b'k', 7, true, false, 0);
    shapes[7] = id_shape(0, b'k', 8, true, false, 0);
    shapes
}

const fn user_shapes(right: bool) -> [FieldShape; 33] {
    let mut shapes = [NO_FIELD; 33];
    shapes[9] = id_shape(1, b'u', 7, false, right, 1);
    shapes[10] = id_shape(1, b'u', 8, false, right, 1);
    shapes[11] = id_shape(1, b'u', 7, true, right, 1);
    shapes[12] = id_shape(1, b'u', 8, true, right, 1);
    shapes
}

const fn campaign_shapes() -> [FieldShape; 65] {
    let mut shapes = [NO_FIELD; 65];
    let mut digits = 1;
    while digits <= 6 {
        shapes[digits + 5] = campaign_shape(digits, true);
        shapes[32 + digits + 3] = campaign_shape(digits, false);
        digits += 1;
    }
    shapes
}

/// The `ts` field of a click row, by the distance from the third comma to
/// the newline: one of the three forms, in quotes or not. The work is done
/// on 32 bytes: 16 from byte 4 of the field's text (`-MM-DDTHH:MM:SS` and
/// one more byte, of the ISO forms), and 16 more that the form picks.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct TimeShape {
    /// XOR with it leaves digit values where digits belong and zeros where
    /// the expected separators and quotes are.
    pattern: [u8; 32],
    /// Largest allowed value after the XOR; 255 for a byte that may be anything.
    limit: [u8; 32],
    /// Brings the digit pairs to the 16-bit lanes their numbers are wanted in.
    shuffle: [u8; 32],
    /// Largest allowed number per 16-bit lane.
    ceilings: [i16; 16],
    /// The weights of the two rounds that add the numbers up.
    first: [i16; 16],
    second: [i16; 16],
    /// Where the two halves begin, counted from the comma before the field.
    low_at: isize,
    high_at: isize,
    /// All ones if the date must be a day of the table (the ISO forms).
    date_mask: u64,
    /// What a second of the date and time of day counts in ms: 1000 for the
    /// ISO forms, nothing for digits.
    per_second: i64,
    /// What the low number of the second half counts: `(sign - 44) * by_sign
    /// + plain`, the sign being byte 19 of the field's text.
    by_sign: i64,
    plain: i64,
    /// The byte before the field's text: the comma, or the opening quote.
    before: u8,
}

/// The shape of no timestamp: a ceiling below every number, so no row
/// passes it. Its halves are where a row has bytes to read whatever it is.
const NO_TIME: TimeShape = TimeShape {
    pattern: [0; 32],
    limit: [ANY; 32],
    shuffle: [NO; 32],
    ceilings: [-1; 16],
    first: [0; 16],
    second: [0; 16],
    low_at: 5,
    high_at: 17,
    date_mask: 0,
    per_second: 0,
    by_sign: 0,
    plain: 0,
    before: 0,
};

/// `form` 1, 2 or 3 (F1, F2, F3), in quotes or not.
const fn time_shape(form: u32, quoted: bool) -> TimeShape {
    let extra = quoted as isize;
    let mut low_pattern = [0u8; 16];
    let mut low_limit = [ANY; 16];
    let mut low_shuffle = [NO; 16];
    let mut low_ceilings = [TOP; 8];
    let mut low_first = [0i16; 8];
    let mut low_second = [0i16; 8];
    let mut high_pattern = [0u8; 16];
    let mut high_limit = [ANY; 16];
    let mut high_shuffle = [NO; 16];
    let mut high_ceilings = [TOP; 8];
    let mut high_first = [0i16; 8];
    let mut high_second = [0i16; 8];
    let mut shape = NO_TIME;
    // The text starts one byte after the comma, or two in quotes.
    shape.low_at = 5 + extra;
    shape.before = if quoted { b'"' } else { b',' };
    if form == 3 {
        // The second half: the 2 bytes before the 13 digits, the digits,
        // the byte after them. In quotes the digits have a quote on either
        // side; without, the comma and the newline, which are where they
        // are by the row's commas and newline. The digits are read in
        // fours, then as the first 5 and the last 8.
        shape.high_at = extra - 1;
        let mut at = 2;
        while at <= 14 {
            high_pattern[at] = b'0';
            high_limit[at] = 9;
            high_shuffle[at + 1] = at as u8;
            at += 1;
        }
        if quoted {
            high_pattern[1] = b'"';
            high_limit[1] = 0;
            high_pattern[15] = b'"';
            high_limit[15] = 0;
        }
        high_first = [100, 1, 100, 1, 100, 1, 100, 1];
        high_second = [10_000, 1, 10_000, 1, 0, 0, 0, 0];
        shape.plain = 100_000_000;
    } else {
        low_pattern = ISO_PATTERN;
        low_limit = ISO_LIMIT;
        low_shuffle = ISO_SHUFFLE;
        low_ceilings = ISO_CEILINGS;
        low_first = ISO_FIRST;
        low_second = ISO_SECOND;
        // The second half: bytes 16..32 of the text, `:SS` (which the low
        // half checks) and what follows the seconds.
        shape.high_at = 17 + extra;
        shape.date_mask = u64::MAX;
        shape.per_second = 1_000;
        // Where the text ends, in the second half.
        let mut end = 4;
        if form == 2 {
            // The sign is in both halves. `/.-,+` are within 4 of `/` under
            // XOR and `)(+*-` within 4 of `)`: only `+` and `-` are both.
            low_pattern[15] = b'/';
            low_limit[15] = 4;
            high_pattern[3] = b')';
            high_limit[3] = 4;
            // `HH:MM`, in minutes and then in seconds.
            high_pattern[4] = b'0';
            high_limit[4] = 9;
            high_pattern[5] = b'0';
            high_limit[5] = 9;
            high_pattern[6] = b':';
            high_limit[6] = 0;
            high_pattern[7] = b'0';
            high_limit[7] = 9;
            high_pattern[8] = b'0';
            high_limit[8] = 9;
            high_shuffle[0] = 4;
            high_shuffle[1] = 5;
            high_shuffle[2] = 7;
            high_shuffle[3] = 8;
            high_ceilings[0] = 23;
            high_ceilings[1] = 59;
            high_first[0] = 60;
            high_first[1] = 1;
            high_second[0] = 60;
            shape.by_sign = 1_000;
            end = 9;
        }
        if quoted {
            high_pattern[end] = b'"';
            high_limit[end] = 0;
        }
    }
    shape.pattern = bytes32(low_pattern, high_pattern);
    shape.limit = bytes32(low_limit, high_limit);
    shape.shuffle = bytes32(low_shuffle, high_shuffle);
    shape.ceilings = words16(low_ceilings, high_ceilings);
    shape.first = words16(low_first, high_first);
    shape.second = words16(low_second, high_second);
    shape
}

const fn time_shapes() -> [TimeShape; 64] {
    let mut shapes = [NO_TIME; 64];
    shapes[14] = time_shape(3, false);
    shapes[16] = time_shape(3, true);
    shapes[21] = time_shape(1, false);
    shapes[23] = time_shape(1, true);
    shapes[26] = time_shape(2, false);
    shapes[28] = time_shape(2, true);
    shapes
}

/// Every shape of a click row's four fields, in one place, so that one
/// address serves all the kernel's lookups.
#[repr(C, align(64))]
struct ClickShapes {
    /// The click ID by the place of the row's first comma, counted from
    /// byte 4: the ID fills the row up to that comma.
    click: [FieldShape; 33],
    /// The user ID by the distance from the first comma to the second: read
    /// as a code (`[0]`), or in index mode as a number (`[1]`).
    user: [[FieldShape; 33]; 2],
    /// The campaign by the distance from the second comma to the third,
    /// plus 32 if bit 6 of the byte after the second comma is set: it is in
    /// `c`, where a campaign without quotes begins, and not in a quote.
    campaign: [FieldShape; 65],
    /// The timestamp by the distance from the third comma to the newline,
    /// modulo 64: the text's length, one for the comma and two more in quotes.
    time: [TimeShape; 64],
}

static CLICK_SHAPES: ClickShapes = ClickShapes {
    click: click_shapes(),
    user: [user_shapes(false), user_shapes(true)],
    campaign: campaign_shapes(),
    time: time_shapes(),
};

/// The shape-table kernel's work on the row starting at `p`: `ROW_DONE`
/// (stored), `ROW_EMIT` (in `click`) or `ROW_DROP` with the row's length, or
/// `ROW_DECLINE` for a row that is not of the regular shape.
#[inline(always)]
unsafe fn click_decode_shapes<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    frame: &ClickFrame,
    p: *const u8,
    out: &mut ParseOut,
    click: &mut ClickRecord,
) -> (usize, u32) {
    const DECLINED: (usize, u32) = (0, ROW_DECLINE);
    // The row's structure: the commas of bytes 4..36, the newline of bytes
    // 32..64. No regular row has one of its first three commas or its
    // newline anywhere else, and the checks below leave no byte of the row
    // unexplained, so nothing is missed by not looking further. A comma or
    // newline that is not there counts as one at position 32, and the
    // distances that come of it belong to no shape.
    let head = p.add(4);
    let commas = _mm256_movemask_epi8(_mm256_cmpeq_epi8(
        _mm256_loadu_si256(head as *const __m256i),
        _mm256_set1_epi8(b',' as i8),
    )) as u32;
    let newlines = _mm256_movemask_epi8(_mm256_cmpeq_epi8(
        _mm256_loadu_si256(p.add(32) as *const __m256i),
        _mm256_set1_epi8(b'\n' as i8),
    )) as u32;
    let second = commas & commas.wrapping_sub(1);
    let third = second & second.wrapping_sub(1);
    // The commas are at `head + t0`, `head + t1` and `head + t2`, the
    // newline at `p + 32 + end`.
    let t0 = commas.trailing_zeros() as usize;
    let t1 = second.trailing_zeros() as usize;
    let t2 = third.trailing_zeros() as usize;
    let end = newlines.trailing_zeros() as usize;

    // The first three fields: their characters to fixed places, and the
    // bytes that frame them (prefix, quotes) against what the shape expects.
    let shapes = &CLICK_SHAPES;
    let click_shape = shapes.click.get_unchecked(t0);
    let user_shape = shapes.user.get_unchecked(usize::from(INDEXED)).get_unchecked(t1 - t0);
    let campaign_at = head.add(t1);
    let campaign_shape =
        shapes.campaign.get_unchecked(t2 - t1 + 32 * usize::from((*campaign_at.add(1) >> 6) & 1));
    let field = |at: *const u8, shape: &FieldShape| {
        _mm_shuffle_epi8(
            _mm_loadu_si128(at as *const __m128i),
            _mm_load_si128(shape.shuffle.as_ptr() as *const __m128i),
        )
    };
    let expected = |shape: &FieldShape| _mm_load_si128(shape.expected.as_ptr() as *const __m128i);
    let click_field = field(p, click_shape);
    let user_field = field(head.add(t0), user_shape);
    let campaign_field = field(campaign_at, campaign_shape);
    let unframed = _mm_or_si128(
        _mm_or_si128(
            _mm_xor_si128(click_field, expected(click_shape)),
            _mm_xor_si128(user_field, expected(user_shape)),
        ),
        _mm_xor_si128(campaign_field, expected(campaign_shape)),
    );

    // The characters of all three in one pass: 8 of the click ID, 8 of the
    // user ID, 8 of the campaign (its digits at the end). A missing
    // character is a zero byte, which is no character.
    let text = _mm256_inserti128_si256::<1>(
        _mm256_castsi128_si256(_mm_unpacklo_epi64(click_field, user_field)),
        campaign_field,
    );
    let ones = |byte: u8| (u64::from(byte) * 0x0101_0101_0101_0101) as i64;
    let per16 = |word: u16| (u64::from(word) * 0x0001_0001_0001_0001) as i64;
    let per32 = |dword: u32| (u64::from(dword) * 0x0000_0001_0000_0001) as i64;
    // Signed compares of bytes moved to the bottom of the range: a digit is
    // one of the 10 values from `0`, a letter one of the 26 from `a`. The
    // campaign has no letters.
    let is_digit = _mm256_cmpgt_epi8(_mm256_set1_epi8(-128 + 10), _mm256_add_epi8(text, _mm256_set1_epi8(0x50)));
    let is_letter = _mm256_cmpgt_epi8(
        _mm256_setr_epi64x(ones(0x80 + 26), ones(0x80 + 26), ones(0x80), ones(0x80)),
        _mm256_add_epi8(text, _mm256_set1_epi8(0x1F)),
    );
    let known = _mm256_or_si256(is_digit, is_letter);
    // A character of a code counts from 1 (`0` is 1, `a` is 11, as in
    // `two_codes`); a digit of a number (a user number in index mode, the
    // campaign's) is itself, and `a` is 10.
    let (user_zero, pair, four, eight) = if INDEXED {
        (b'0', 0x0124, 0x0001_0510, 1_679_616)
    } else {
        (b'0' - 1, 0x0125, 0x0001_0559, 1_874_161)
    };
    let from_zero = _mm256_sub_epi8(text, _mm256_setr_epi64x(ones(b'0' - 1), ones(user_zero), ones(b'0'), ones(b'0')));
    let from_letter = _mm256_and_si256(is_letter, _mm256_set1_epi8(b'a' as i8 - b'0' as i8 - 10));
    let values = _mm256_and_si256(_mm256_sub_epi8(from_zero, from_letter), known);
    // Most significant character first: pairs, fours, then all eight.
    let pairs =
        _mm256_maddubs_epi16(values, _mm256_setr_epi64x(per16(0x0125), per16(pair), per16(0x010A), per16(0x010A)));
    let fours = _mm256_madd_epi16(
        pairs,
        _mm256_setr_epi64x(per32(0x0001_0559), per32(four), per32(0x0001_0064), per32(0x0001_0064)),
    );
    let wide = _mm256_add_epi64(
        _mm256_mul_epu32(fours, _mm256_setr_epi64x(1_874_161, eight, 10_000, 10_000)),
        _mm256_srli_epi64::<32>(fours),
    );
    // Framed as expected (bytes 8..16 of the three fields), and characters
    // exactly where the shapes have them.
    if _mm_testz_si128(unframed, _mm_set_epi64x(-1, 0)) == 0
        || _mm256_movemask_epi8(known) as u32 & 0x00FF_FFFF
            != click_shape.known | user_shape.known | campaign_shape.known
    {
        return DECLINED;
    }
    let click_code = _mm256_extract_epi64::<0>(wide) as u64;
    let user_wide = _mm256_extract_epi64::<1>(wide) as u64;
    // A campaign number with a leading zero is another campaign's text: the
    // exact map's. The table holds every campaign written `c_<number>`
    // below 2^20, so a number that is not in it, or lies beyond it, is no
    // campaign: `u32::MAX`.
    let campaign_number = _mm256_extract_epi64::<2>(wide) as u64;
    if campaign_number < campaign_shape.least {
        return DECLINED;
    }
    let campaigns = &ctx.campaign_by_number;
    let campaign = if (campaign_number as usize) < campaigns.len() {
        *campaigns.get_unchecked(campaign_number as usize)
    } else {
        u32::MAX
    };

    // The timestamp: the two halves its shape names, checked byte for byte
    // against the shape and added up by it.
    let shape = shapes.time.get_unchecked((end + 28).wrapping_sub(t2) & 63);
    let low_at = head.add(t2).offset(shape.low_at);
    let halves = _mm256_inserti128_si256::<1>(
        _mm256_castsi128_si256(_mm_loadu_si128(low_at as *const __m128i)),
        _mm_loadu_si128(head.add(t2).offset(shape.high_at) as *const __m128i),
    );
    let digits = _mm256_xor_si256(halves, _mm256_load_si256(shape.pattern.as_ptr() as *const __m256i));
    let limit = _mm256_load_si256(shape.limit.as_ptr() as *const __m256i);
    let shaped = _mm256_cmpeq_epi8(_mm256_min_epu8(digits, limit), digits);
    let numbers = _mm256_maddubs_epi16(
        _mm256_shuffle_epi8(digits, _mm256_load_si256(shape.shuffle.as_ptr() as *const __m256i)),
        _mm256_set1_epi16(0x010A),
    );
    // A number over its ceiling takes bits out as a byte out of shape does.
    let over = _mm256_cmpgt_epi16(numbers, _mm256_load_si256(shape.ceilings.as_ptr() as *const __m256i));
    let good = _mm256_movemask_epi8(_mm256_andnot_si256(over, shaped)) as u32;
    let sums = _mm256_madd_epi16(numbers, _mm256_load_si256(shape.first.as_ptr() as *const __m256i));
    let folded = _mm256_madd_epi16(
        _mm256_packus_epi32(sums, sums),
        _mm256_load_si256(shape.second.as_ptr() as *const __m256i),
    );
    // `month * 32 + day` below the second of the day (both zero for
    // digits). Month and day are within their bounds if `good` says so, and
    // then the slot's key holds for one day only: the year's text and the
    // day must be the table's.
    let iso = _mm256_extract_epi64::<0>(folded) as u64;
    let year = u64::from((low_at.sub(4) as *const u32).read_unaligned());
    let slot = (iso & (CLICK_DAY_SLOTS as u64 - 1)) as usize;
    let off_table = ((year | (iso << 32)) ^ *frame.day_key.get_unchecked(slot)) & shape.date_mask;
    if good != u32::MAX || off_table != 0 || *low_at.sub(5) != shape.before {
        return DECLINED;
    }
    // The second half's numbers: the first 5 digits below the last 8, or the
    // offset in seconds. `+` is 43 and `-` is 45, so an offset counts
    // `(sign - 44) * 1000` ms a second: against the clock's direction.
    let other = _mm256_extract_epi64::<2>(folded) as u64;
    let seconds = i64::from(*frame.day_seconds.get_unchecked(slot)) + (iso >> 32) as i64 + frame.base_seconds;
    let by = (i64::from(*low_at.add(15)) - 44) * shape.by_sign + shape.plain;
    let since = seconds * shape.per_second + (other & 0xFFFF_FFFF) as i64 * by + (other >> 32) as i64 - frame.since;

    let user_value = if INDEXED {
        // The user must be a generated ID: no leading zero, a number of 40
        // bits, a user index within the keys' bound. Any other ID is the
        // general kernel's.
        let keys = ctx.keys.as_ref().unwrap_unchecked();
        let number = unscramble(user_wide, keys.user_key);
        if user_wide < user_shape.least || user_wide > NUMBER_MASK || (number >> 9) >= keys.user_bound {
            return DECLINED;
        }
        number
    } else {
        user_wide
    };

    // The row is well formed and ends at the newline found: what is left is
    // the click's own validity.
    let length = 32 + end;
    if since as u64 >= frame.span || campaign == u32::MAX {
        return (length, ROW_DROP);
    }
    let code_and_campaign = click_code | (u64::from(campaign) << 42);
    if MODE == MODE_STORE {
        if INDEXED {
            // SAFETY (of the call): the user index was checked against the keys' bound.
            emit_number_click(out, user_value, code_and_campaign, since as u64);
        } else {
            emit_code_click(out, user_value, code_and_campaign, since as u64);
        }
        return (length, ROW_DONE);
    }
    *click = ClickRecord { user: user_value, user_is_number: INDEXED, code_and_campaign, ts: since as u32 };
    (length, ROW_EMIT)
}

/// Parses the click row starting at `p` (with `ROW_PAD` readable bytes after
/// it, and one before it): the shape-table kernel, and `click_row_fast` for
/// the rows it declines. The same answers as `click_row_lean`.
#[inline(always)]
pub unsafe fn click_row_shapes<const MODE: u32, const INDEXED: bool>(
    ctx: &Ctx,
    frame: &ClickFrame,
    p: *const u8,
    out: &mut ParseOut,
    click: &mut ClickRecord,
) -> (usize, u32) {
    let (length, status) = click_decode_shapes::<MODE, INDEXED>(ctx, frame, p, out, click);
    if status == ROW_DECLINE {
        return click_row_declined::<INDEXED>(ctx, p, click);
    }
    (length, status)
}

// Every fast kernel against the general parsers, line by line: named probes
// for each range and shape check a kernel makes, then random damage.
#[cfg(test)]
mod tests {
    use super::super::keys::{scramble, Keys};
    use super::super::parse::{
        click_row_slow, click_row_slow_indexed, event_row_slow, event_row_slow_indexed, SlowIndexed,
    };
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
        /// indexed lean; then the click kernels: general, lean, shape-table,
        /// each without and with index mode.
        answered: [usize; 10],
        /// Click lines the lean kernels took themselves: lean, shape-table,
        /// each without and with index mode.
        lean_clicks: [usize; 4],
        /// The lean click kernels' view of the window.
        frame: ClickFrame,
        /// The frame of the event kernel in assembly.
        lean_frame: LeanFrame,
        /// Clicks whose user the indexed form turns into a number.
        numbered: usize,
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
            let frame = ClickFrame::new(&ctx);
            let lean_frame = LeanFrame::new(&ctx);
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
                answered: [0; 10],
                lean_clicks: [0; 4],
                frame,
                lean_frame,
                numbered: 0,
                event_id: format!("e{}", event_id.unwrap()),
                user_id: format!("u{}", user_id.unwrap()),
                ts,
                ingest,
                digits: format!("{ts_ms:013}"),
            }
        }

        /// `line`, then a newline, then enough of a tail, with the end of
        /// a row in front (the event kernel in assembly reads 16 bytes back).
        fn load(&mut self, line: &[u8], tail: &[u8]) -> *const u8 {
            const BEFORE: &[u8; 16] = b"e,u,1,2,view,p,\n";
            self.buffer.clear();
            self.buffer.extend_from_slice(BEFORE);
            self.buffer.extend_from_slice(line);
            self.buffer.push(b'\n');
            while self.buffer.len() < line.len() + 2 * ROW_PAD {
                self.buffer.extend_from_slice(tail);
            }
            // SAFETY: the buffer is longer than that.
            unsafe { self.buffer.as_ptr().add(BEFORE.len()) }
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
                        // With the assembly in front, which checks itself
                        // against the lean kernel on every row.
                        _ => event_row_lean_asm::<MODE_ROW>(&self.ctx, &mut self.lean_frame, p, &mut self.out, &mut row),
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
            // Index mode: the same click, a generated user as its number.
            let slow_indexed = click_row_slow_indexed(&self.ctx, &self.keys, line);
            if let (Some(plain), Some(indexed)) = (&slow, &slow_indexed) {
                // The indexed form says what `Keys::user_number_of_code` says.
                match self.keys.user_number_of_code(plain.user) {
                    Some(number) => {
                        assert!(indexed.user_is_number && indexed.user == number, "click {shown:?}");
                        self.numbered += 1;
                    }
                    None => assert_eq!(indexed, plain, "click {shown:?}"),
                }
            }
            // Kernels 0 and 1: `click_row_fast`; 2 and 3: the lean kernel in
            // front of it; 4 and 5: the shape-table kernel in front of it.
            // Odd ones in index mode.
            for kernel in 0..6 {
                let indexed = kernel % 2 == 1;
                let mut click = ClickRecord::default();
                let declined = CLICK_DECLINED.with(|count| count.get());
                // SAFETY: `load` left ROW_PAD readable bytes after `p`.
                let (length, status) = unsafe {
                    match kernel {
                        0 => click_row_fast::<false>(&self.ctx, p, &mut click),
                        1 => click_row_fast::<true>(&self.ctx, p, &mut click),
                        2 => click_row_lean::<MODE_ROW, false>(&self.ctx, &self.frame, p, &mut self.out, &mut click),
                        3 => click_row_lean::<MODE_ROW, true>(&self.ctx, &self.frame, p, &mut self.out, &mut click),
                        4 => {
                            click_row_shapes::<MODE_ROW, false>(&self.ctx, &self.frame, p, &mut self.out, &mut click)
                        }
                        _ => click_row_shapes::<MODE_ROW, true>(&self.ctx, &self.frame, p, &mut self.out, &mut click),
                    }
                };
                if status != ROW_LONG {
                    assert_eq!(length, line.len(), "click kernel {kernel}: length of {shown:?}");
                }
                let expected = if indexed { &slow_indexed } else { &slow };
                match status {
                    ROW_EMIT => assert_eq!(&Some(click), expected, "click kernel {kernel} on {shown:?}"),
                    ROW_DROP => assert_eq!(&None, expected, "click kernel {kernel} on {shown:?}"),
                    ROW_SLOW | ROW_LONG => continue,
                    _ => panic!("click kernel {kernel}: status {status} on {shown:?}"),
                }
                self.answered[4 + kernel] += 1;
                if kernel >= 2 && CLICK_DECLINED.with(|count| count.get()) == declined {
                    self.lean_clicks[kernel - 2] += 1;
                }
            }
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

    /// The lean click kernel has no branch for a row with fewer than three
    /// commas in its bytes 4..36 or no newline in its bytes 32..64: what is
    /// missing counts as found at position 32, and this is why that is
    /// enough. A row passes only with a shape for each field, and with
    /// those the third comma is at most at position 30; from there no
    /// distance to a position 32 belongs to a shape.
    #[test]
    fn click_shapes_leave_no_room() {
        let field = |shape: &FieldShape| shape.shuffle[8] != NO;
        let time = |shape: &TimeShape| shape.ceilings[0] != -1;
        assert!(!field(&NO_FIELD) && !time(&NO_TIME));
        let last = |shapes: &[FieldShape]| (0..shapes.len()).rev().find(|&at| field(&shapes[at])).unwrap();
        // The comma of every shape of a field: the last place it can be at.
        let shapes = &CLICK_SHAPES;
        let first_comma = last(&shapes.click);
        let second_comma = first_comma + shapes.user.iter().map(|user| last(user)).max().unwrap();
        let third_comma = second_comma + last(&shapes.campaign[..32]).max(last(&shapes.campaign[32..64]));
        assert_eq!((first_comma, second_comma, third_comma), (7, 19, 30));
        // No first comma: no click shape.
        assert!(!field(&shapes.click[32]));
        // No second comma: no user shape, wherever the first one is.
        for t0 in 0..=first_comma {
            assert!(shapes.user.iter().all(|user| !field(&user[32 - t0])));
        }
        // No third comma: no campaign shape, wherever the second one is.
        for t1 in 0..=second_comma {
            assert!(!field(&shapes.campaign[32 - t1]) && !field(&shapes.campaign[32 - t1 + 32]));
        }
        // No newline: no timestamp shape, wherever the third comma is; nor
        // for a newline before the third comma (the distance wraps).
        for t2 in 0..=third_comma {
            assert!(!time(&shapes.time[(32 + 28 - t2) & 63]));
        }
        for end in 0..32usize {
            for t2 in end + 29..=32 {
                assert!(!time(&shapes.time[(end + 28).wrapping_sub(t2) & 63]));
            }
        }
        // Every byte the kernel reads through a shape is within the row's
        // reach: the fields within 16 bytes of where they begin, the halves
        // of a timestamp from one byte before its comma on.
        for shape in shapes.click.iter().chain(shapes.user.iter().flatten()).chain(&shapes.campaign) {
            assert!(shape.shuffle.iter().all(|&from| from == NO || from < 16));
            assert!(shape.expected[..8].iter().all(|&byte| byte == 0));
            assert!(shape.known >> 24 == 0);
        }
        for shape in &shapes.time {
            assert!((5..=6).contains(&shape.low_at) && (-1..=18).contains(&shape.high_at));
        }
    }

    fn for_each_window(mut probe: impl FnMut(&mut Checker)) {
        for window in 0..WINDOWS.len() {
            let mut check = Checker::new(window);
            probe(&mut check);
            // The probes must reach the kernels, not only the general parser.
            assert!(check.lines > 0);
            // ... and index mode must have turned users into numbers.
            assert!(check.numbered > 0, "no click with a user number");
            // ... and the lean click kernels must have taken rows themselves.
            assert!(check.lean_clicks.iter().all(|&count| count > 0), "{:?}", check.lean_clicks);
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
                let taken = check.lean_clicks;
                check.click_all(line);
                // Rows the lean click kernels are made for: each must take
                // them itself, in both modes and with every tail.
                assert_eq!(
                    check.lean_clicks,
                    taken.map(|count| count + TAILS.len()),
                    "a lean click kernel declined {:?}",
                    String::from_utf8_lossy(line)
                );
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

    /// Valid purchases of the usual kind (a product written `p` and a
    /// number that is in the table, a price of digits) are finished by the
    /// assembly; the unusual ones are left to its caller. Either way the row
    /// is what the general parser makes of it (`Checker::event` sees to that).
    #[test]
    fn the_assembly_finishes_purchases() {
        for_each_window(|check| {
            let bought = || BOUGHT.with(|count| count.get());
            let ts = check.ts.clone();
            // The products of the table (see PRODUCTS), every way of quoting.
            for (product, price) in [("p1", "1"), ("p12345", "3216"), ("p2", "0250"), ("p10", "10000000"), ("p50000", "00000042")] {
                for quotes in 0..4 {
                    let product = if quotes & 1 == 1 { format!("\"{product}\"") } else { product.to_string() };
                    let price = if quotes & 2 == 2 { format!("\"{price}\"") } else { price.to_string() };
                    let before = bought();
                    let line = check.purchase(&ts, &product, &price);
                    assert!(line.len() >= 64 && line.len() <= 95);
                    check.event_with(&line, TAILS[0]);
                    assert_eq!(bought(), (before.0 + 1, before.1), "not finished in assembly: {product} {price}");
                }
            }
            // Left to the caller: a product that is not `p` and a number, a
            // leading zero, a number past the table, a price too long for
            // the assembly's sixteen bytes.
            for (product, price) in [("px", "5"), ("p007", "5"), ("P9", "5"), ("p9999999", "5"), ("p1", "00000000000000042")] {
                let before = bought();
                let line = check.purchase(&ts, product, price);
                check.event_with(&line, TAILS[0]);
                assert_eq!(bought(), (before.0, before.1 + 1), "finished in assembly: {product} {price}");
            }
            // No price: the row is not valid, and nobody finishes anything.
            for price in ["", "0", "000", "12x", "+5", "10000001", "123456789", " 5", "5 "] {
                let before = bought();
                let line = check.purchase(&ts, "p12345", price);
                check.event_with(&line, TAILS[0]);
                assert_eq!(bought(), before, "a price after all: {price:?}");
            }
            // The clicks and counts `for_each_window` asks for.
            check.times(&ts);
        });
    }

    /// The loop in assembly stores only where the writers are laid out as it
    /// takes them. This build must be one of those: `whole_files_in_chunks`
    /// is what covers that store.
    #[test]
    fn the_writers_are_as_the_assembly_takes_them() {
        assert!(writer_as_assumed());
        for window in 0..WINDOWS.len() {
            assert!(LeanFrame::stores(&Checker::new(window).ctx));
        }
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
    /// Not a test but a measurement: the click kernels over the click files
    /// of the dataset `SS_BENCH_DATA` names, pass after pass in turn in one
    /// process, in general mode and in index mode. Timed in turn, they see
    /// the same machine, whatever else runs on it; the fastest pass of each
    /// is what the kernel costs. Run it with
    /// `SS_BENCH_DATA=<dir> cargo test --release click_kernels_in_turn -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn click_kernels_in_turn() {
        use super::super::parse::parse_clicks_chunk_with;
        let Ok(directory) = std::env::var("SS_BENCH_DATA") else {
            eprintln!("SS_BENCH_DATA is not set: nothing to measure");
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        let passes: usize = std::env::var("SS_BENCH_PASSES").ok().and_then(|value| value.parse().ok()).unwrap_or(9);
        let mut ctx = Ctx::load(&directory).unwrap().expect("a window the engine takes");
        let paths = |name: &str| -> Vec<std::path::PathBuf> {
            let mut paths: Vec<_> =
                std::fs::read_dir(directory.join(name)).unwrap().map(|entry| entry.unwrap().path()).collect();
            paths.sort();
            paths
        };
        let clicks: Vec<Vec<u8>> = paths("clicks").iter().map(|path| std::fs::read(path).unwrap()).collect();
        let rows: usize = clicks
            .iter()
            .map(|file| file.iter().filter(|&&byte| byte == b'\n').count().saturating_sub(1))
            .sum();
        // The keys of index mode, found as the engine finds them.
        let events = paths("events");
        let sizes: Vec<u64> = events.iter().map(|path| std::fs::metadata(path).unwrap().len()).collect();
        let largest = (0..events.len()).max_by_key(|&at| sizes[at]).unwrap();
        let mut excerpt = std::fs::read(&events[largest]).unwrap();
        excerpt.truncate(1 << 16);
        let keys = super::super::keys::recover(&excerpt, sizes.iter().sum());
        for indexed in [false, true] {
            ctx.keys = if indexed { keys } else { None };
            if indexed && keys.is_none() {
                eprintln!("index mode: no keys in this dataset");
                continue;
            }
            let mut times = [Vec::new(), Vec::new(), Vec::new()];
            let mut stored = [0usize; 3];
            for _ in 0..passes {
                for kernel in [KERNEL_GENERAL, KERNEL_LEAN, KERNEL_SHAPES] {
                    let mut out = ParseOut::new(&ctx);
                    let started = std::time::Instant::now();
                    for file in &clicks {
                        parse_clicks_chunk_with(&ctx, file, 0, file.len(), &mut out, kernel);
                    }
                    times[kernel as usize].push(started.elapsed().as_secs_f64() * 1e3);
                    stored[kernel as usize] = out.clicks.finish().words()
                        + out.user_clicks.iter_mut().map(|writer| writer.finish().words()).sum::<usize>();
                }
            }
            // Every kernel must have stored the same number of words.
            assert!(stored[0] == stored[1] && stored[0] == stored[2], "{stored:?}");
            for (name, times) in ["click_row_fast  ", "click_row_lean  ", "click_row_shapes"].iter().zip(&mut times) {
                times.sort_by(f64::total_cmp);
                let (fastest, median) = (times[0], times[times.len() / 2]);
                eprintln!(
                    "{} {name}: fastest {fastest:.1} ms ({:.2} ns/row), median {median:.1} ms, {rows} rows, {} words stored",
                    if indexed { "index mode  " } else { "general mode" },
                    fastest * 1e6 / rows as f64,
                    stored[0],
                );
            }
        }
    }

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
