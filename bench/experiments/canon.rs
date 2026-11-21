//! Lab tool, not part of the pipeline: rewrites every events file of a
//! generated dataset so that all rows have one format, keeping every value.
//!
//!   canon IN_EVENTS_DIR OUT_EVENTS_DIR MODE
//!
//! MODE q: quotes around fields removed. MODE t: `ts` rewritten to the form
//! 2026-09-16T18:24:20Z where that loses nothing (13-digit milliseconds ending
//! in 000, and the forms with a +HH:MM / -HH:MM offset). MODE a: both.
//! Rows it does not understand are copied unchanged and counted.

use std::fs;
use std::io::Write;

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn iso(seconds: i64) -> String {
    let days = seconds.div_euclid(86400);
    let rest = seconds.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rest / 3600, rest / 60 % 60, rest % 60)
}

fn number(text: &[u8]) -> Option<i64> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(text.iter().fold(0i64, |sum, &digit| sum * 10 + i64::from(digit - b'0')))
}

/// The `ts` text in the Z form, when that is the same instant exactly.
fn canonical_ts(text: &[u8]) -> Option<String> {
    if text.len() == 13 {
        let millis = number(text)?;
        return (millis % 1000 == 0).then(|| iso(millis / 1000));
    }
    // 2026-09-11T09:11:15+09:00
    if text.len() == 25 && (text[19] == b'+' || text[19] == b'-') && text[22] == b':' && text[10] == b'T' {
        let (y, m, d) = (number(&text[0..4])?, number(&text[5..7])?, number(&text[8..10])?);
        let (h, mi, s) = (number(&text[11..13])?, number(&text[14..16])?, number(&text[17..19])?);
        let offset = (number(&text[20..22])? * 60 + number(&text[23..25])?) * 60;
        let local = days_from_civil(y, m, d) * 86400 + h * 3600 + mi * 60 + s;
        return Some(iso(if text[19] == b'+' { local - offset } else { local + offset }));
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (input, output, mode) = (&args[1], &args[2], args[3].as_str());
    let (unquote, retime) = (mode == "q" || mode == "a", mode == "t" || mode == "a");
    fs::create_dir_all(output).unwrap();
    let (mut rows, mut odd, mut quoted_rows, mut retimed, mut kept_ts) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut names: Vec<_> = fs::read_dir(input).unwrap().map(|entry| entry.unwrap().path()).collect();
    names.sort();
    for path in names {
        let bytes = fs::read(&path).unwrap();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut first = true;
        for line in bytes.split_inclusive(|&byte| byte == b'\n') {
            let (body, end) = match line.last() {
                Some(b'\n') => (&line[..line.len() - 1], &line[line.len() - 1..]),
                _ => (line, &line[line.len()..]),
            };
            if first {
                first = false;
                out.extend_from_slice(line);
                continue;
            }
            rows += 1;
            let fields: Vec<&[u8]> = body.split(|&byte| byte == b',').collect();
            if fields.len() != 7 {
                odd += 1;
                out.extend_from_slice(line);
                continue;
            }
            let mut any_quote = false;
            for (at, field) in fields.iter().enumerate() {
                if at > 0 {
                    out.push(b',');
                }
                let is_quoted = field.len() >= 2 && field[0] == b'"' && field[field.len() - 1] == b'"';
                any_quote |= is_quoted;
                let inner = if is_quoted { &field[1..field.len() - 1] } else { field };
                let keep_quotes = is_quoted && !unquote;
                if keep_quotes {
                    out.push(b'"');
                }
                if at == 2 && retime {
                    match canonical_ts(inner) {
                        Some(text) => {
                            retimed += 1;
                            out.extend_from_slice(text.as_bytes());
                        }
                        None => {
                            kept_ts += 1;
                            out.extend_from_slice(inner);
                        }
                    }
                } else {
                    out.extend_from_slice(inner);
                }
                if keep_quotes {
                    out.push(b'"');
                }
            }
            quoted_rows += u64::from(any_quote);
            out.extend_from_slice(end);
        }
        let mut file = fs::File::create(format!("{}/{}", output, path.file_name().unwrap().to_string_lossy())).unwrap();
        file.write_all(&out).unwrap();
    }
    println!(
        "canon mode {mode}: {rows} rows, {quoted_rows} with a quoted field, {retimed} ts rewritten, {kept_ts} ts kept, {odd} copied unchanged"
    );
}
