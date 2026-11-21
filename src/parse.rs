use memchr::memchr2;

pub const DAY: i64 = 86_400_000;
pub const WEEK: i64 = 7 * DAY;
pub const GAP: i64 = 1_800_000;

/// The dialect has no embedded delimiters, escaped quotes, or multiline values.
#[inline]
pub fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    if N >= 4 && (16..=128).contains(&line.len()) {
        return short_fields(line);
    }
    scalar_fields(line)
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline]
fn short_fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut commas = 0u128;
    let mut quotes = 0u128;
    let last = line.len() - 16;
    // Overlap the last block if necessary so every vector load stays in the line.
    for offset in (0..last).step_by(16).chain(std::iter::once(last)) {
        // SAFETY: offsets are at most line.len() - 16. SSE2 and NEON are
        // baseline features of the respective 64-bit targets.
        let (c, q) = unsafe { punctuation(line.as_ptr().add(offset)) };
        commas |= u128::from(c) << offset;
        quotes |= u128::from(q) << offset;
    }
    if commas.count_ones() as usize != N - 1 {
        return None;
    }

    let mut result = [&[][..]; N];
    let mut start = 0;
    let mut paired_quotes = 0;
    for (index, field) in result.iter_mut().enumerate() {
        let end = if index + 1 == N {
            line.len()
        } else {
            let end = commas.trailing_zeros() as usize;
            commas &= commas - 1;
            end
        };
        let raw = &line[start..end];
        *field = if let [b'"', value @ .., b'"'] = raw {
            paired_quotes += 2;
            value
        } else {
            raw
        };
        start = end + 1;
    }
    // Any quote that was not consumed as a whole-field pair is malformed.
    (quotes.count_ones() == paired_quotes).then_some(result)
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn punctuation(block: *const u8) -> (u16, u16) {
    use std::arch::x86_64::*;
    unsafe {
        let bytes = _mm_loadu_si128(block.cast());
        let commas = _mm_cmpeq_epi8(bytes, _mm_set1_epi8(b',' as i8));
        let quotes = _mm_cmpeq_epi8(bytes, _mm_set1_epi8(b'"' as i8));
        (
            _mm_movemask_epi8(commas) as u16,
            _mm_movemask_epi8(quotes) as u16,
        )
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn punctuation(block: *const u8) -> (u16, u16) {
    use std::arch::aarch64::*;
    unsafe {
        let bytes = vld1q_u8(block);
        let commas = vceqq_u8(bytes, vdupq_n_u8(b','));
        let quotes = vceqq_u8(bytes, vdupq_n_u8(b'"'));
        let weights = vld1q_u8([1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
        let mask = |matches| {
            let bits = vandq_u8(matches, weights);
            u16::from(vaddv_u8(vget_low_u8(bits))) | (u16::from(vaddv_u8(vget_high_u8(bits))) << 8)
        };
        (mask(commas), mask(quotes))
    }
}

#[inline]
fn scalar_fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
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
    if bytes.len() <= 18 {
        // Up to 18 decimal digits always fit in i64.
        let mut value = 0;
        for &byte in bytes {
            let digit = byte.wrapping_sub(b'0');
            if digit > 9 {
                return None;
            }
            value = value * 10 + i64::from(digit);
        }
        return (value <= max).then_some(value);
    }
    decimal_long(bytes, max)
}

#[cold]
fn decimal_long(bytes: &[u8], max: i64) -> Option<i64> {
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

const fn leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

const fn days_before_year(year: i32) -> i32 {
    365 * year + (year + 3) / 4 - (year + 99) / 100 + (year + 399) / 400
}

// The ISO forms have exactly four year digits. Keep the Gregorian conversion
// out of the row loop, including the leap-day adjustment.
static YEARS: [(i32, bool); 10_000] = {
    let mut years = [(0, false); 10_000];
    let mut year = 0;
    while year < years.len() {
        let y = year as i32;
        years[year] = (days_before_year(y) - days_before_year(1970), leap_year(y));
        year += 1;
    }
    years
};
const MONTHS: [(i32, i64); 12] = [
    (0, 31),
    (31, 28),
    (59, 31),
    (90, 30),
    (120, 31),
    (151, 30),
    (181, 31),
    (212, 31),
    (243, 30),
    (273, 31),
    (304, 30),
    (334, 31),
];

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
    let (year_start, leap) = YEARS[year as usize];
    let &(month_start, length) = MONTHS.get((month - 1) as usize)?;
    if day == 0
        || day > length + i64::from(leap && month == 2)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = i64::from(year_start + month_start) + day - 1 + i64::from(leap && month > 2);
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
