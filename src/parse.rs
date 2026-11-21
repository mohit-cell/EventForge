use memchr::memchr2;

pub const DAY: i64 = 86_400_000;
pub const WEEK: i64 = 7 * DAY;
pub const GAP: i64 = 1_800_000;

/// The dialect has no embedded delimiters, escaped quotes, or multiline values.
#[inline]
pub fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut result = [&[][..]; N];
    let mut tail = line;
    for (index, field) in result.iter_mut().enumerate() {
        let quoted = tail.first() == Some(&b'"');
        let content = if quoted { &tail[1..] } else { tail };
        let end = memchr2(b',', b'"', content).unwrap_or(content.len());
        let (value, suffix) = content.split_at(end);
        let suffix = if quoted {
            suffix.strip_prefix(b"\"")?
        } else {
            suffix
        };
        if index + 1 == N {
            if !suffix.is_empty() {
                return None;
            }
        } else {
            tail = suffix.strip_prefix(b",")?;
        }
        *field = value;
    }
    Some(result)
}

#[inline]
fn two(bytes: &[u8], at: usize) -> Option<i64> {
    let a = bytes[at].wrapping_sub(b'0');
    let b = bytes[at + 1].wrapping_sub(b'0');
    (a < 10 && b < 10).then_some(i64::from(a) * 10 + i64::from(b))
}

fn decimal(bytes: &[u8], max: i64) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0i64;
    for &byte in bytes {
        let digit = byte.wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(digit))?;
        if value > max {
            return None;
        }
    }
    Some(value)
}

pub fn price(bytes: &[u8]) -> Option<u32> {
    decimal(bytes, 10_000_000)
        .filter(|&v| v > 0)
        .map(|v| v as u32)
}

/// Strict calendar validation, then Gregorian 400-year eras relative to 1970-01-01.
fn civil_ms(s: &[u8]) -> Option<i64> {
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let year = two(s, 0)? * 100 + two(s, 2)?;
    let month = two(s, 5)?;
    let day = two(s, 8)?;
    let hour = two(s, 11)?;
    let minute = two(s, 14)?;
    let second = two(s, 17)?;
    let limit = match month {
        2 => 28 + i64::from(year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)),
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if day == 0 || day > limit || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let adjusted = year - i64::from(month <= 2);
    let era = adjusted.div_euclid(400);
    let y = adjusted.rem_euclid(400);
    let march_month = if month <= 2 { month + 9 } else { month - 3 };
    let day_of_year = (153 * march_month + 2) / 5 + day - 1;
    let days = era * 146_097 + y * 365 + y / 4 - y / 100 + day_of_year - 719_468;
    Some(days * DAY + ((hour * 60 + minute) * 60 + second) * 1000)
}

pub fn utc(s: &[u8]) -> Option<i64> {
    if s.len() != 20 || s[19] != b'Z' {
        return None;
    }
    civil_ms(s)
}

#[inline]
pub fn timestamp(s: &[u8]) -> Option<i64> {
    if s.get(4) == Some(&b'-') {
        if s.len() == 20 {
            return utc(s);
        }
        if s.len() != 25 || s[22] != b':' {
            return None;
        }
        let sign = match s[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let hour = two(s, 20)?;
        let minute = two(s, 23)?;
        if hour > 23 || minute > 59 {
            return None;
        }
        return Some(civil_ms(s)? - sign * (hour * 60 + minute) * 60_000);
    }
    decimal(s, i64::MAX)
}
