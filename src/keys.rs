use rustc_hash::{FxHashMap, FxHasher};
use std::cmp::Ordering;
use std::hash::Hasher;
use std::sync::{Arc, Mutex};

const INDIRECT: u64 = 1 << 63;
const STRIPES: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u64);

impl Key {
    pub const EMPTY: Self = Self(0);

    pub fn mixed(self) -> u64 {
        let mixed = self.0.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        mixed ^ (mixed >> 32)
    }

    pub fn bucket(self, count: usize) -> usize {
        self.mixed() as usize & (count - 1)
    }

    fn inline_bytes(self) -> impl Iterator<Item = u8> {
        (0..9)
            .rev()
            .map(move |i| ((self.0 >> (i * 7)) & 127) as u8)
            .take_while(|&c| c != 0)
    }
}

#[derive(Default)]
struct Strings {
    indices: FxHashMap<Arc<[u8]>, usize>,
    values: Vec<Arc<[u8]>>,
}

pub struct Keys {
    strings: [Mutex<Strings>; STRIPES],
}

impl Default for Keys {
    fn default() -> Self {
        Self {
            strings: std::array::from_fn(|_| Mutex::default()),
        }
    }
}

impl Keys {
    /// Nine nonzero ASCII bytes fit exactly in 63 bits. All other values are interned
    /// by full byte equality. Hash collisions never identify distinct IDs.
    #[inline]
    pub fn get(&self, bytes: &[u8]) -> Key {
        if matches!(bytes.len(), 8 | 9) {
            let word = u64::from_be_bytes(bytes[..8].try_into().unwrap());
            let last = if bytes.len() == 9 { bytes[8] } else { 0 };
            // Check for a high bit or a zero byte before removing the eight padding bits.
            if (word | word.wrapping_sub(0x0101_0101_0101_0101)) & 0x8080_8080_8080_8080 == 0
                && (bytes.len() == 8 || last.wrapping_sub(1) < 127)
            {
                let pairs = (word & 0x007f_007f_007f_007f) | ((word >> 1) & 0x3f80_3f80_3f80_3f80);
                let quads =
                    (pairs & 0x0000_3fff_0000_3fff) | ((pairs >> 2) & 0x0fff_c000_0fff_c000);
                let packed = (quads & 0x0fff_ffff) | ((quads >> 4) & 0x00ff_ffff_f000_0000);
                return Key((packed << 7) | u64::from(last));
            }
            return self.intern(bytes);
        }
        if bytes.len() < 8 {
            let mut packed = 0u64;
            let mut valid = true;
            for &byte in bytes {
                packed = (packed << 7) | u64::from(byte);
                valid &= byte != 0 && byte < 128;
            }
            if valid {
                return Key(packed << ((9 - bytes.len()) * 7));
            }
        }
        self.intern(bytes)
    }

    #[cold]
    fn intern(&self, bytes: &[u8]) -> Key {
        let mut hash = FxHasher::default();
        hash.write(bytes);
        let stripe = hash.finish() as usize % STRIPES;
        let mut strings = self.strings[stripe].lock().unwrap();
        let index = if let Some(&n) = strings.indices.get(bytes) {
            n
        } else {
            let n = strings.values.len();
            let owned: Arc<[u8]> = bytes.into();
            strings.indices.insert(owned.clone(), n);
            strings.values.push(owned);
            n
        };
        Key(INDIRECT | (index as u64 * STRIPES as u64) | stripe as u64)
    }

    pub fn bytes(&self, key: Key) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.append_bytes(key, &mut bytes);
        bytes
    }

    pub fn append_bytes(&self, key: Key, out: &mut Vec<u8>) {
        if key.0 & INDIRECT == 0 {
            out.extend(key.inline_bytes());
        } else {
            let stripe = key.0 as usize % STRIPES;
            let index = (key.0 & !INDIRECT) as usize / STRIPES;
            out.extend_from_slice(&self.strings[stripe].lock().unwrap().values[index]);
        }
    }

    pub fn compare(&self, a: Key, b: Key) -> Ordering {
        if a == b || (a.0 | b.0) & INDIRECT == 0 {
            a.cmp(&b)
        } else {
            self.bytes(a).cmp(&self.bytes(b))
        }
    }
}
