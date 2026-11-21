//! Classify each input byte once. A row reuses the comma and quote masks from
//! the blocks in which its terminating LF was found.
use crate::parse::{fields, fields_from_masks};

#[derive(Clone, Copy, Default)]
struct Marks {
    commas: u64,
    quotes: u64,
    lines: u64,
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn classify(block: &[u8; 64]) -> Marks {
    use std::arch::aarch64::*;
    // SAFETY: the array contains all 64 bytes and NEON is part of arm64.
    unsafe {
        let v = vld4q_u8(block.as_ptr());
        let mask = |c| {
            let c = vdupq_n_u8(c);
            // Each lane covers four adjacent input bytes. Replicate the four
            // matches into both nibbles, then narrow adjacent lanes to a byte.
            let a = vandq_u8(vceqq_u8(v.0, c), vdupq_n_u8(0x11));
            let b = vandq_u8(vceqq_u8(v.1, c), vdupq_n_u8(0x22));
            let c2 = vandq_u8(vceqq_u8(v.2, c), vdupq_n_u8(0x44));
            let d = vandq_u8(vceqq_u8(v.3, c), vdupq_n_u8(0x88));
            let both = vorrq_u8(vorrq_u8(a, b), vorrq_u8(c2, d));
            vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(
                both,
            ))))
        };
        Marks {
            commas: mask(b','),
            quotes: mask(b'"'),
            lines: mask(b'\n'),
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
fn classify(block: &[u8; 64]) -> Marks {
    use std::arch::x86_64::*;
    // SAFETY: AVX2 is enabled at compile time. Both unaligned loads are in bounds.
    unsafe {
        let lo = _mm256_loadu_si256(block.as_ptr().cast());
        let hi = _mm256_loadu_si256(block.as_ptr().add(32).cast());
        let mask = |c| {
            let c = _mm256_set1_epi8(c);
            let low = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, c)) as u32;
            let high = _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, c)) as u32;
            u64::from(low) | (u64::from(high) << 32)
        };
        Marks {
            commas: mask(b',' as i8),
            quotes: mask(b'"' as i8),
            lines: mask(b'\n' as i8),
        }
    }
}

#[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
#[inline]
fn classify(block: &[u8; 64]) -> Marks {
    use std::arch::x86_64::*;
    let mut marks = Marks::default();
    for i in 0..4 {
        // SAFETY: SSE2 is baseline on x86-64. The last load ends at byte 64.
        unsafe {
            let bytes = _mm_loadu_si128(block.as_ptr().add(i * 16).cast());
            let mask =
                |c| (_mm_movemask_epi8(_mm_cmpeq_epi8(bytes, _mm_set1_epi8(c))) as u64) << (16 * i);
            marks.commas |= mask(b',' as i8);
            marks.quotes |= mask(b'"' as i8);
            marks.lines |= mask(b'\n' as i8);
        }
    }
    marks
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn classify(block: &[u8; 64]) -> Marks {
    let mut m = Marks::default();
    for (i, &byte) in block.iter().enumerate() {
        m.commas |= u64::from(byte == b',') << i;
        m.quotes |= u64::from(byte == b'"') << i;
        m.lines |= u64::from(byte == b'\n') << i;
    }
    m
}

#[inline]
fn aligned(first: u64, previous: u64, current: u64, base: usize, start: usize) -> u128 {
    if start >= base {
        return u128::from(current >> (start - base));
    }
    let prefix = base - start;
    if prefix <= 64 {
        return (u128::from(current) << prefix) | u128::from(previous >> (64 - prefix));
    }
    let older = prefix - 64;
    u128::from(current).checked_shl(prefix as u32).unwrap_or(0)
        | (u128::from(previous) << older)
        | u128::from(first >> (64 - older))
}

pub fn records<'a, const N: usize>(data: &'a [u8], mut emit: impl FnMut([&'a [u8]; N])) {
    let mut start = 0;
    let mut base = 0;
    let (mut first, mut previous) = (Marks::default(), Marks::default());
    let mut block = |bytes: &[u8; 64]| {
        let current = classify(bytes);
        let mut endings = current.lines;
        while endings != 0 {
            let end = base + endings.trailing_zeros() as usize;
            endings &= endings - 1;
            let line = &data[start..end];
            let parsed = match line.len() {
                0 => None,
                len @ 1..=128 => {
                    let limit = u128::MAX >> (128 - len);
                    let commas =
                        aligned(first.commas, previous.commas, current.commas, base, start) & limit;
                    let quotes =
                        aligned(first.quotes, previous.quotes, current.quotes, base, start) & limit;
                    fields_from_masks(line, commas, quotes)
                }
                _ => fields(line),
            };
            if let Some(row) = parsed {
                emit(row);
            }
            start = end + 1;
        }
        first = previous;
        previous = current;
        base += 64;
    };
    let mut chunks = data.chunks_exact(64);
    for chunk in &mut chunks {
        block(chunk.try_into().unwrap());
    }
    let rest = chunks.remainder();
    let mut tail = [0; 64];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = b'\n';
    block(&tail);
}
