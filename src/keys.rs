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

    pub fn bucket(self, count: usize) -> usize {
        let mixed = self.0.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (mixed ^ (mixed >> 32)) as usize & (count - 1)
    }

    fn inline_bytes(self) -> Vec<u8> {
        (0..9)
            .rev()
            .map(|i| ((self.0 >> (i * 7)) & 127) as u8)
            .take_while(|&c| c != 0)
            .collect()
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
        if bytes.len() <= 9 {
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
        if key.0 & INDIRECT == 0 {
            key.inline_bytes()
        } else {
            let stripe = key.0 as usize % STRIPES;
            let index = (key.0 & !INDIRECT) as usize / STRIPES;
            self.strings[stripe].lock().unwrap().values[index].to_vec()
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
