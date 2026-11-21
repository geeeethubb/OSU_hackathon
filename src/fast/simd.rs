//! AVX2 fast paths for the hot row parsers.
//!
//! Each fast path either produces exactly what the general parser in
//! `parse.rs` produces for the same line, or declines the line (returns a
//! "slow" status) so the general parser handles it. `SS_VERIFY=1` runs both
//! on every line and aborts on any difference.
//!
//! The crate is compiled for x86-64-v3 (AVX2, BMI1/2, LZCNT, POPCNT), so the
//! intrinsics here need no per-function feature attributes.

#![cfg(target_arch = "x86_64")]

use std::arch::x86_64::*;

use super::codes::id_code;
use super::parse::EventRow;
use super::Ctx;
use crate::util::{civil_from_days, parse_price, parse_ts, DAY_MS, LATE_MS};

/// Bytes a fast row parser may read past the start of its row.
pub const ROW_PAD: usize = 192;

pub const ROW_DROP: u32 = 0;
pub const ROW_EMIT: u32 = 1;
/// The fast path declines; the row's length is known.
pub const ROW_SLOW: u32 = 2;
/// No newline within 128 bytes of the row start.
pub const ROW_LONG: u32 = 3;

const NOT_FAST: i64 = i64::MIN;

/// Lookup tables for ISO timestamps near the window.
pub struct TsTables {
    /// Calendar year of day offset 0.
    year0: u32,
    /// Epoch milliseconds of day offset 0.
    base_ms: i64,
    /// `[(year - year0) << 12 | month << 5 | day]` -> day offset, or 0xFF for
    /// every date that is invalid or outside the table's range.
    date: Box<[u8]>,
}

impl TsTables {
    pub fn new(window_start: i64) -> Self {
        // From 9 days before the window (click window plus a day of UTC
        // offset) for 62 days, which covers every ingest time the compact
        // records can hold.
        let base_day = window_start.div_euclid(DAY_MS) - 9;
        let (year0, _, _) = civil_from_days(base_day);
        let mut date = vec![0xFFu8; 2 << 12].into_boxed_slice();
        for offset in 0..62 {
            let (year, month, day) = civil_from_days(base_day + offset);
            let year_index = (year - year0) as usize;
            if year_index <= 1 {
                date[(year_index << 12) | ((month as usize) << 5) | day as usize] = offset as u8;
            }
        }
        Self { year0: year0 as u32, base_ms: base_day * DAY_MS, date }
    }
}

#[repr(C, align(32))]
struct A32([u8; 32]);

const fn iso_pattern(form: usize) -> A32 {
    // F1 `YYYY-MM-DDTHH:MM:SSZ` or F2 `YYYY-MM-DDTHH:MM:SS+HH:MM`, with `0`
    // at digit positions, so XOR leaves digit values and zeros.
    let mut bytes = [0u8; 32];
    let text: &[u8; 25] = if form == 0 { b"0000-00-00T00:00:00Z\0\0\0\0\0" } else { b"0000-00-00T00:00:00\x0000:00" };
    let mut at = 0;
    while at < 25 {
        bytes[at] = text[at];
        at += 1;
    }
    A32(bytes)
}

const fn iso_keep(form: usize) -> A32 {
    let mut bytes = [0u8; 32];
    let mut at = 0;
    while at < 25 {
        let keep = if form == 0 { at < 20 } else { at != 19 };
        bytes[at] = if keep { 0xFF } else { 0 };
        at += 1;
    }
    A32(bytes)
}

const fn iso_limit(form: usize) -> A32 {
    // Largest allowed value after XOR: 9 where a digit belongs, 0 elsewhere.
    let mut bytes = [0u8; 32];
    let digits: [usize; 14] = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let mut at = 0;
    while at < digits.len() {
        bytes[digits[at]] = 9;
        at += 1;
    }
    if form == 1 {
        bytes[20] = 9;
        bytes[21] = 9;
        bytes[23] = 9;
        bytes[24] = 9;
    }
    A32(bytes)
}

static ISO_PATTERN: [A32; 2] = [iso_pattern(0), iso_pattern(1)];
static ISO_KEEP: [A32; 2] = [iso_keep(0), iso_keep(1)];
static ISO_LIMIT: [A32; 2] = [iso_limit(0), iso_limit(1)];

const Z: u8 = 0x80;
/// Gathers digit pairs: low lane year(4) month day hour minute; high lane
/// (bytes 16..) second, a zero pair, offset hour, offset minute.
static ISO_SHUFFLE: A32 = A32([
    0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, Z, Z, Z, Z, //
    1, 2, Z, Z, 4, 5, 7, 8, Z, Z, Z, Z, Z, Z, Z, Z,
]);

#[repr(C, align(16))]
struct A16([u8; 16]);

const fn digit_shuffles() -> [A16; 17] {
    // Entry `n` right-aligns the first `n` bytes and zero-fills the front.
    let mut table = [const { A16([Z; 16]) }; 17];
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

/// F1 (`form` 0) or F2 (`form` 1) at `p`, as epoch ms; `NOT_FAST` when the
/// text is not exactly that form with a date inside the table.
#[inline(always)]
unsafe fn ts_iso(tables: &TsTables, p: *const u8, form: usize) -> i64 {
    let text = _mm256_loadu_si256(p as *const __m256i);
    let pattern = _mm256_load_si256(ISO_PATTERN[form].0.as_ptr() as *const __m256i);
    let keep = _mm256_load_si256(ISO_KEEP[form].0.as_ptr() as *const __m256i);
    let limit = _mm256_load_si256(ISO_LIMIT[form].0.as_ptr() as *const __m256i);
    let values = _mm256_and_si256(_mm256_xor_si256(text, pattern), keep);
    let shape_ok = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_min_epu8(values, limit), values)) as u32;
    let pairs = _mm256_shuffle_epi8(values, _mm256_load_si256(ISO_SHUFFLE.0.as_ptr() as *const __m256i));
    // Two digits -> one number per 16-bit lane.
    let numbers = _mm256_maddubs_epi16(pairs, _mm256_set1_epi16(0x010A));
    let ceilings = _mm256_setr_epi16(99, 99, 99, 99, 23, 59, 0x7FFF, 0x7FFF, 59, 0x7FFF, 23, 59, 0x7FFF, 0x7FFF, 0x7FFF, 0x7FFF);
    let too_big = _mm256_movemask_epi8(_mm256_cmpgt_epi16(numbers, ceilings)) as u32;
    let weights = _mm256_setr_epi16(100, 1, 32, 1, 3600, 60, 0, 0, 1, 0, 3600, 60, 0, 0, 0, 0);
    let sums = _mm256_madd_epi16(numbers, weights);
    let low = _mm256_castsi256_si128(sums);
    let high = _mm256_extracti128_si256(sums, 1);
    let year = _mm_cvtsi128_si32(low) as u32;
    let month_day = _mm_extract_epi32(low, 1) as u32;
    let hour_minute = i64::from(_mm_extract_epi32(low, 2));
    let second = i64::from(_mm_cvtsi128_si32(high));
    let offset = i64::from(_mm_extract_epi32(high, 1));
    let year_index = year.wrapping_sub(tables.year0);
    let day = *tables.date.get_unchecked((((year_index & 1) << 12) | (month_day & 4095)) as usize);
    let sign = *p.add(19);
    let minus = sign == b'-';
    let sign_ok = (form == 0) | minus | (sign == b'+');
    let offset = if minus { -offset } else { offset };
    let ok = (shape_ok == u32::MAX) & (too_big == 0) & (year_index <= 1) & (day != 0xFF) & sign_ok;
    let value = tables.base_ms + (i64::from(day) * 86_400 + hour_minute + second - offset) * 1_000;
    if ok {
        value
    } else {
        NOT_FAST
    }
}

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
    if ok {
        value as i64
    } else {
        NOT_FAST
    }
}

/// Newline, comma and quote positions in the 128 bytes at `p`.
#[inline(always)]
unsafe fn masks128(p: *const u8) -> (u128, u128, u128) {
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let comma = _mm256_set1_epi8(b',' as i8);
    let quote = _mm256_set1_epi8(b'"' as i8);
    let mut newlines = 0u128;
    let mut commas = 0u128;
    let mut quotes = 0u128;
    for block in 0..4 {
        let bytes = _mm256_loadu_si256(p.add(block * 32) as *const __m256i);
        let shift = block * 32;
        newlines |= u128::from(_mm256_movemask_epi8(_mm256_cmpeq_epi8(bytes, newline)) as u32) << shift;
        commas |= u128::from(_mm256_movemask_epi8(_mm256_cmpeq_epi8(bytes, comma)) as u32) << shift;
        quotes |= u128::from(_mm256_movemask_epi8(_mm256_cmpeq_epi8(bytes, quote)) as u32) << shift;
    }
    (newlines, commas, quotes)
}

/// Diagnostic: the length of the row at `p` from the same masks the parser
/// uses, with the comma and quote masks consumed so they are not optimized out.
#[inline(always)]
pub unsafe fn row_length(p: *const u8) -> usize {
    let (newlines, commas, quotes) = masks128(p);
    std::hint::black_box(commas ^ quotes);
    if newlines == 0 {
        return 127;
    }
    newlines.trailing_zeros() as usize
}

#[inline(always)]
unsafe fn read_u64(p: *const u8) -> u64 {
    (p as *const u64).read_unaligned()
}

const VIEW: u64 = u32::from_le_bytes(*b"view") as u64;
const CART: u64 = u32::from_le_bytes(*b"cart") as u64;
const PURCHASE: u64 = u64::from_le_bytes(*b"purchase");

/// Parses the event row starting at `p` (which must have `ROW_PAD` readable
/// bytes after it). Returns the row's length and a `ROW_*` status; `row` is
/// filled for `ROW_EMIT`.
#[inline(always)]
pub unsafe fn event_row_fast(ctx: &Ctx, p: *const u8, row: &mut EventRow) -> (usize, u32) {
    let (newlines, commas, quotes) = masks128(p);
    if newlines == 0 {
        return (0, ROW_LONG);
    }
    let length = newlines.trailing_zeros() as usize;
    let below = (1u128 << length) - 1;
    let commas = commas & below;
    let quotes = quotes & below;
    if commas.count_ones() != 6 {
        return (length, ROW_DROP);
    }
    // Quotes are legal only as a pair around a whole field.
    let separators = commas | (1u128 << length);
    let opens = quotes & ((separators << 1) | 1);
    let closes = quotes & (separators >> 1);
    let inside = !separators & below;
    let open_ends = opens.wrapping_add(inside) & !inside;
    if ((quotes ^ (opens | closes)) | (opens & closes) | (open_ends ^ (closes << 1))) != 0 {
        return (length, ROW_DROP);
    }

    // The first three commas from the low word, the last three from the 64
    // bytes that end at the newline.
    let low = commas as u64;
    let tail = ((commas << (128 - length)) >> 64) as u64;
    if low.count_ones() < 3 || tail.count_ones() < 3 {
        return (length, ROW_SLOW);
    }
    let c0 = low.trailing_zeros() as usize;
    let rest = low & (low - 1);
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

    // A validated field is quoted exactly when its first byte is a quote.
    let quoted = |at: usize| usize::from(*p.add(at) == b'"');
    let q0 = quoted(0);
    let q1 = quoted(c0 + 1);
    let q2 = quoted(c1 + 1);
    let q3 = quoted(c2 + 1);
    let q4 = quoted(c3 + 1);
    let q5 = quoted(c4 + 1);
    let q6 = quoted(c5 + 1);
    let (s0, e0) = (q0, c0 - q0);
    let (s1, e1) = (c0 + 1 + q1, c1 - q1);
    let (s2, e2) = (c1 + 1 + q2, c2 - q2);
    let (s3, e3) = (c2 + 1 + q3, c3 - q3);
    let (s4, e4) = (c3 + 1 + q4, c4 - q4);
    let (s5, e5) = (c4 + 1 + q5, c5 - q5);
    let (s6, e6) = (c5 + 1 + q6, length - q6);
    if e0 == s0 {
        return (length, ROW_DROP);
    }
    let field = |start: usize, end: usize| std::slice::from_raw_parts(p.add(start), end - start);

    // Event and user IDs together: 8 characters each after the prefix.
    let event_chars = e0 - s0 - 1;
    let user_chars = (e1 - s1).wrapping_sub(1);
    let event_text = _bzhi_u64(read_u64(p.add(s0 + 1)), (event_chars as u32).wrapping_mul(8));
    let user_text = _bzhi_u64(read_u64(p.add(s1 + 1)), (user_chars as u32).wrapping_mul(8));
    let text = _mm_set_epi64x(user_text as i64, event_text as i64);
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
    let event_need = (1u32 << (event_chars & 15)) - 1;
    let event_fast = (event_chars.wrapping_sub(1) < 8) & (*p.add(s0) == b'e') & (known & event_need == event_need);
    let event_code = if event_fast {
        _mm_cvtsi128_si64(codes) as u64
    } else {
        id_code(field(s0, e0), b'e', &ctx.event_ids)
    };

    // ingest_ts: always valid F1 by the rules; anything else goes the slow way.
    let tables = &ctx.ts_tables;
    let mut ingest_ms = if e3 - s3 == 20 { ts_iso(tables, p.add(s3), 0) } else { NOT_FAST };
    if ingest_ms == NOT_FAST {
        match parse_ts(field(s3, e3)) {
            Some(value) => ingest_ms = value,
            None => {
                ctx.request_fallback();
                return (length, ROW_DROP);
            }
        }
    }
    let ingest_seconds = ingest_ms.div_euclid(1_000) - ctx.ingest_base_seconds;
    if ingest_ms.rem_euclid(1_000) != 0 || !(0..(1 << 21)).contains(&ingest_seconds) {
        ctx.request_fallback();
        return (length, ROW_DROP);
    }

    // ts: F3 when at most 16 digits, else F1 (20 bytes) or F2 (25 bytes).
    let ts_length = e2 - s2;
    let digits = ts_digits(p.add(s2), ts_length.min(16));
    let iso = ts_iso(tables, p.add(s2), usize::from(ts_length == 25));
    let mut ts_ms = if ts_length <= 16 { digits } else { iso };
    if ts_ms == NOT_FAST || (ts_length > 16 && ts_length != 20 && ts_length != 25) {
        // Rare: a bad timestamp, a date outside the tables, or a very long F3.
        ts_ms = if ts_length <= 16 { NOT_FAST } else { parse_ts(field(s2, e2)).unwrap_or(NOT_FAST) };
    }
    let in_window = (ts_ms >= ctx.window_start) & (ts_ms < ctx.window_end);

    let type_length = e4 - s4;
    let type_text = _bzhi_u64(read_u64(p.add(s4)), (type_length.min(8) as u32) * 8);
    let is_purchase = (type_length == 8) & (type_text == PURCHASE);
    let known_type = is_purchase | ((type_length == 4) & ((type_text == VIEW) | (type_text == CART)));

    let mut valid = (e1 > s1) & in_window & known_type & (e5 > s5);
    if valid && ingest_ms - ts_ms > LATE_MS {
        valid = false;
    }
    let mut purchase = None;
    if valid && is_purchase {
        match parse_price(field(s6, e6)) {
            Some(price) => {
                let category = ctx.product_category.get(field(s5, e5)).copied().unwrap_or(ctx.unknown_category);
                purchase = Some((price, category));
            }
            None => valid = false,
        }
    }
    if !valid {
        *row = EventRow { event: event_code, user: 0, ts: 0, ingest: ingest_seconds as u32, valid: false, purchase: None };
        return (length, ROW_EMIT);
    }
    let user_need = (1u32 << (user_chars & 15)) - 1;
    let user_fast =
        (user_chars.wrapping_sub(1) < 8) & (*p.add(s1) == b'u') & ((known >> 8) & user_need == user_need);
    let user_code = if user_fast {
        _mm_extract_epi64(codes, 1) as u64
    } else {
        id_code(field(s1, e1), b'u', &ctx.user_ids)
    };
    *row = EventRow {
        event: event_code,
        user: user_code,
        ts: (ts_ms - ctx.window_start) as u32,
        ingest: ingest_seconds as u32,
        valid: true,
        purchase,
    };
    (length, ROW_EMIT)
}
