//! Small shared helpers: calendar maths, the exact timestamp parser from
//! rules.md, the CSV dialect, and stage timing.

use std::time::Instant;

pub const SECOND_MS: i64 = 1_000;
pub const DAY_MS: i64 = 86_400_000;
pub const WEEK_MS: i64 = 7 * DAY_MS;
/// A new session starts when the gap is strictly over this.
pub const SESSION_GAP_MS: i64 = 1_800_000;
/// An event is late when `ingest_ts - ts` is strictly over this.
pub const LATE_MS: i64 = DAY_MS;
pub const MAX_PRICE: u64 = 10_000_000;

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` for days since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

#[inline]
fn digit(byte: u8) -> Option<i64> {
    if byte.is_ascii_digit() {
        Some(i64::from(byte - b'0'))
    } else {
        None
    }
}

#[inline]
fn two_digits(text: &[u8], at: usize) -> Option<i64> {
    Some(digit(text[at])? * 10 + digit(text[at + 1])?)
}

/// The three timestamp forms of rules.md, as epoch milliseconds (UTC).
/// `None` is a bad timestamp. An F3 value too long for 64 bits is valid but
/// far outside every window, so it comes back as `i64::MAX`.
pub fn parse_ts(text: &[u8]) -> Option<i64> {
    let length = text.len();
    if length == 0 {
        return None;
    }
    if text.iter().all(u8::is_ascii_digit) {
        // F3: digits only, leading zeros allowed.
        let significant = match text.iter().position(|&byte| byte != b'0') {
            Some(first) => &text[first..],
            None => return Some(0),
        };
        if significant.len() > 18 {
            return Some(i64::MAX);
        }
        let mut value: i64 = 0;
        for &byte in significant {
            value = value * 10 + i64::from(byte - b'0');
        }
        return Some(value);
    }
    if length != 20 && length != 25 {
        return None;
    }
    // Shared prefix `YYYY-MM-DDTHH:MM:SS`.
    let year = digit(text[0])? * 1000 + digit(text[1])? * 100 + digit(text[2])? * 10 + digit(text[3])?;
    if text[4] != b'-' || text[7] != b'-' || text[10] != b'T' || text[13] != b':' || text[16] != b':' {
        return None;
    }
    let month = two_digits(text, 5)?;
    let day = two_digits(text, 8)?;
    let hour = two_digits(text, 11)?;
    let minute = two_digits(text, 14)?;
    let second = two_digits(text, 17)?;
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    let mut milliseconds = seconds * SECOND_MS;
    if length == 20 {
        // F1
        if text[19] != b'Z' {
            return None;
        }
    } else {
        // F2: `+HH:MM` or `-HH:MM`, subtracted to get UTC.
        let sign = match text[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if text[22] != b':' {
            return None;
        }
        let offset_hours = two_digits(text, 20)?;
        let offset_minutes = two_digits(text, 23)?;
        if offset_hours > 23 || offset_minutes > 59 {
            return None;
        }
        milliseconds -= sign * (offset_hours * 60 + offset_minutes) * 60_000;
    }
    Some(milliseconds)
}

/// A purchase price: ASCII digits only, value 1 to 10,000,000 (`0250` is 250).
pub fn parse_price(text: &[u8]) -> Option<u32> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let significant = match text.iter().position(|&byte| byte != b'0') {
        Some(first) => &text[first..],
        None => return None, // all zeros: value 0
    };
    if significant.len() > 8 {
        return None;
    }
    let mut value: u64 = 0;
    for &byte in significant {
        value = value * 10 + u64::from(byte - b'0');
    }
    if (1..=MAX_PRICE).contains(&value) {
        Some(value as u32)
    } else {
        None
    }
}

/// One field of the CSV dialect: quotes are syntax around a whole field and
/// are legal nowhere else. `None` means the line is malformed.
#[inline]
pub fn unquote(field: &[u8]) -> Option<&[u8]> {
    if !field.contains(&b'"') {
        return Some(field);
    }
    let length = field.len();
    if length >= 2 && field[0] == b'"' && field[length - 1] == b'"' && !field[1..length - 1].contains(&b'"') {
        Some(&field[1..length - 1])
    } else {
        None
    }
}

/// Splits a line into exactly `N` fields. Every comma separates fields.
/// `None` for a malformed line (wrong field count or a misplaced quote).
#[inline]
pub fn split_fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut fields: [&[u8]; N] = [&line[0..0]; N];
    let mut count = 0;
    let mut start = 0;
    for at in 0..=line.len() {
        if at == line.len() || line[at] == b',' {
            if count == N {
                return None;
            }
            fields[count] = unquote(&line[start..at])?;
            count += 1;
            start = at + 1;
        }
    }
    if count == N {
        Some(fields)
    } else {
        None
    }
}

/// Calls `visit` for every line after the header. Only `\n` ends a line; text
/// after the last `\n` is a line too when it is not empty.
#[inline]
pub fn for_each_data_line<'a>(buffer: &'a [u8], mut visit: impl FnMut(&'a [u8])) {
    let mut position = match buffer.iter().position(|&byte| byte == b'\n') {
        Some(end_of_header) => end_of_header + 1,
        None => return,
    };
    while position < buffer.len() {
        let end = buffer[position..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(buffer.len(), |offset| position + offset);
        visit(&buffer[position..end]);
        position = end + 1;
    }
}

/// Reads `window_start` and `window_end` from manifest.json as epoch ms.
pub fn parse_manifest(text: &[u8]) -> Option<(i64, i64)> {
    fn value_after(text: &[u8], key: &[u8]) -> Option<i64> {
        let at = text.windows(key.len()).position(|window| window == key)?;
        let rest = &text[at + key.len()..];
        let colon = rest.iter().position(|&byte| byte == b':')?;
        let rest = &rest[colon + 1..];
        let open = rest.iter().position(|&byte| byte == b'"')?;
        let rest = &rest[open + 1..];
        let close = rest.iter().position(|&byte| byte == b'"')?;
        parse_ts(&rest[..close])
    }
    Some((value_after(text, b"\"window_start\"")?, value_after(text, b"\"window_end\"")?))
}

/// Prints `SS_TIMING stage=<name> ms=<float>` lines to stderr when the
/// environment variable `SS_TIMING` is set to something other than `0`.
pub struct Timing {
    enabled: bool,
    start: Instant,
    last: Instant,
}

impl Timing {
    pub fn new() -> Self {
        let enabled = std::env::var_os("SS_TIMING").is_some_and(|value| value != "0");
        let now = Instant::now();
        Self { enabled, start: now, last: now }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Reports the time since the previous stage ended.
    pub fn stage(&mut self, name: &str) {
        let now = Instant::now();
        if self.enabled {
            eprintln!("SS_TIMING stage={} ms={:.3}", name, (now - self.last).as_secs_f64() * 1e3);
        }
        self.last = now;
    }

    pub fn total(&self) {
        if self.enabled {
            eprintln!("SS_TIMING stage=total ms={:.3}", self.start.elapsed().as_secs_f64() * 1e3);
        }
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(parse_ts(b"2026-10-03T23:58:10Z"), Some(1_791_071_890_000));
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(1_791_071_890_000));
        assert_eq!(parse_ts(b"1791071890000"), Some(1_791_071_890_000));
        assert_eq!(parse_ts(b"0001791071890000"), Some(1_791_071_890_000));
        assert_eq!(parse_ts(b"86400000"), Some(86_400_000));
        assert_eq!(parse_ts(b"999999999999999999999"), Some(i64::MAX));
        for bad in [
            &b""[..],
            b"2026-10-03T23:58:10.000Z",
            b"2026-10-03T23:58:10z",
            b"2026-10-03t23:58:10Z",
            b"2026-10-03 23:58:10Z",
            b"2026-10-03T23:58:10",
            b"1791071890000.0",
            b"1.79e12",
            b"-5",
            b"2026-13-03T23:58:10Z",
            b"2026-02-30T23:58:10Z",
            b"2027-02-29T23:58:10Z",
            b"2026-10-03T24:58:10Z",
            b"2026-10-03T23:58:60Z",
            b"2026-10-03T23:58:10+24:00",
            b"2026-10-03T23:58:10+05:60",
            b"2026-10-03T23:58:10+0200",
        ] {
            assert_eq!(parse_ts(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(parse_ts(b"2028-02-29T00:00:00Z").is_some());
        assert_eq!(parse_ts(b"2026-10-03T23:58:10-00:00"), parse_ts(b"2026-10-03T23:58:10Z"));
    }

    #[test]
    fn prices() {
        assert_eq!(parse_price(b"0250"), Some(250));
        assert_eq!(parse_price(b"10000000"), Some(10_000_000));
        assert_eq!(parse_price(b"1"), Some(1));
        for bad in [&b""[..], b"0", b"00", b"10000001", b"19.99", b"-5", b"+500", b"1e3", b"250000000"] {
            assert_eq!(parse_price(bad), None);
        }
    }

    #[test]
    fn csv_dialect() {
        assert_eq!(split_fields::<2>(b"\"u1\",u2"), Some([&b"u1"[..], &b"u2"[..]]));
        assert_eq!(split_fields::<2>(b"\"\",u2"), Some([&b""[..], &b"u2"[..]]));
        assert_eq!(split_fields::<2>(b"u1,u5\""), None);
        assert_eq!(split_fields::<2>(b"u1,u2,u3"), None);
        assert_eq!(split_fields::<2>(b"u1"), None);
        assert_eq!(split_fields::<2>(b""), None);
        assert_eq!(split_fields::<2>(b"\",u2"), None);
        assert_eq!(split_fields::<2>(b"\"a\"b\",u2"), None);
    }

    #[test]
    fn calendar_round_trip() {
        for days in [-1, 0, 1, 19_000, 20_000, 21_244, 21_245, 25_000] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}
