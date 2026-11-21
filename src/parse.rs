use memchr::memchr2;

pub const DAY: i64 = 86_400_000;
pub const WEEK: i64 = 7 * DAY;
pub const GAP: i64 = 1_800_000;
const ASCII_ZEROES: u64 = u64::from_le_bytes([b'0'; 8]);

#[inline]
pub(crate) fn fields_from_masks<const N: usize>(
    line: &[u8],
    mut commas: u128,
    quotes: u128,
) -> Option<[&[u8]; N]> {
    if commas.count_ones() as usize != N - 1 {
        return None;
    }
    // Each opening quote is at the beginning of a field. Adding those bits
    // to the non-comma bits carries to the next delimiter (or out of the
    // word for the final field). That predicts every required closing quote.
    let openings = quotes & ((commas << 1) | 1);
    let (ends, final_quoted) = (!commas).overflowing_add(openings);
    let closings = ((ends & commas) >> 1) | (u128::from(final_quoted) << (line.len() - 1));
    if quotes != openings | closings || openings & closings != 0 {
        return None;
    }

    let mut fields = [&[][..]; N];
    let mut start = 0;
    for (index, field) in fields.iter_mut().enumerate() {
        let end = if index + 1 == N {
            line.len()
        } else {
            let position = commas.trailing_zeros() as usize;
            commas &= commas - 1;
            position
        };
        let trim = usize::from(line.get(start) == Some(&b'"'));
        *field = &line[start + trim..end - trim];
        start = end + 1;
    }
    Some(fields)
}

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

#[inline]
fn digits(word: u64) -> Option<u64> {
    // An ASCII byte outside '0'..='9' either borrows on subtraction or sets
    // its high bit on addition. The original high bits reject non-ASCII.
    if (word | word.wrapping_sub(ASCII_ZEROES) | word.wrapping_add(0x4646_4646_4646_4646))
        & 0x8080_8080_8080_8080
        != 0
    {
        return None;
    }
    Some(word ^ ASCII_ZEROES)
}

#[inline]
fn decimal_eight(ascii: u64) -> Option<i64> {
    let bytes = digits(ascii)?;
    let pairs = (bytes * 10 + (bytes >> 8)) & 0x00ff_00ff_00ff_00ff;
    let groups = (pairs * 100 + (pairs >> 16)) & 0x0000_ffff_0000_ffff;
    Some(((groups & 0xffff) * 10_000 + (groups >> 32)) as i64)
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

/// A validated date is reusable only when all ten input bytes match.
pub struct DateCache {
    slots: [([u8; 10], i64); 32],
}

impl Default for DateCache {
    fn default() -> Self {
        Self {
            slots: [(*b"1970-01-01", 0); 32],
        }
    }
}

fn midnight(s: &[u8; 10]) -> Option<i64> {
    const SEPARATORS: u64 = u64::from_le_bytes([0, 0, 0, 0, 255, 0, 0, 255]);
    const MARKS: u64 = u64::from_le_bytes(*b"\0\0\0\0-\0\0-");
    let date = u64::from_le_bytes(s[..8].try_into().unwrap());
    if date & SEPARATORS != MARKS {
        return None;
    }
    let date = digits(date ^ ((ASCII_ZEROES ^ MARKS) & SEPARATORS))?;
    let pairs = date * 10 + (date >> 8);
    let year = (pairs & 255) * 100 + ((pairs >> 16) & 255);
    let month = ((pairs >> 40) & 255) as usize;
    let day = two(s, 8)?;
    let (year_start, leap) = YEARS[year as usize];
    let &(month_start, length) = MONTHS.get(month.wrapping_sub(1))?;
    if day == 0 || day > length + i64::from(leap && month == 2) {
        return None;
    }
    let days = i64::from(year_start + month_start) + day - 1 + i64::from(leap && month > 2);
    Some(days * DAY)
}

impl DateCache {
    #[inline]
    fn civil_ms(&mut self, s: &[u8]) -> Option<i64> {
        const SEPARATORS: u64 = u64::from_le_bytes([0, 0, 255, 0, 0, 255, 0, 0]);
        const MARKS: u64 = u64::from_le_bytes(*b"\0\0:\0\0:\0\0");
        let clock = u64::from_le_bytes(s[11..19].try_into().unwrap());
        if s[10] != b'T' || clock & SEPARATORS != MARKS {
            return None;
        }
        let clock = digits(clock ^ ((ASCII_ZEROES ^ MARKS) & SEPARATORS))?;
        let pairs = clock * 10 + (clock >> 8);
        let hour = (pairs & 255) as i64;
        let minute = ((pairs >> 24) & 255) as i64;
        let second = ((pairs >> 48) & 255) as i64;
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        let date: &[u8; 10] = s[..10].try_into().unwrap();
        let slot = &mut self.slots[(usize::from(s[8]) * 10 + usize::from(s[9])) & 31];
        if slot.0 != *date {
            *slot = (*date, midnight(date)?);
        }
        Some(slot.1 + ((hour * 60 + minute) * 60 + second) * 1000)
    }

    #[inline]
    pub fn utc(&mut self, s: &[u8]) -> Option<i64> {
        if s.len() != 20 || s[19] != b'Z' {
            return None;
        }
        self.civil_ms(s)
    }

    #[inline]
    pub fn timestamp(&mut self, s: &[u8]) -> Option<i64> {
        if let Ok(bytes) = <&[u8; 13]>::try_from(s) {
            let prefix = decimal_eight(u64::from_le_bytes(bytes[..8].try_into().unwrap()))?;
            let suffix = decimal_eight(u64::from_le_bytes([
                b'0', b'0', b'0', bytes[8], bytes[9], bytes[10], bytes[11], bytes[12],
            ]))?;
            return Some(prefix * 100_000 + suffix);
        }
        if s.get(4) == Some(&b'-') {
            if s.len() == 20 {
                return self.utc(s);
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
            return Some(self.civil_ms(s)? - sign * (hour * 60 + minute) * 60_000);
        }
        decimal(s, i64::MAX)
    }
}

#[cfg(test)]
pub fn timestamp(s: &[u8]) -> Option<i64> {
    DateCache::default().timestamp(s)
}
