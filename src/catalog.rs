use crate::input::{Input, Reader, dimension};
use crate::keys::{Key, Keys};
use crate::parse::DateCache;
use anyhow::{Context, Result, ensure};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use std::collections::hash_map::Entry;
use std::path::Path;

#[derive(Deserialize)]
struct Manifest {
    window_start: String,
    window_end: String,
}

/// Dense indices cover IDs with a shared prefix and canonical decimal suffix.
/// Sparse keys retain their exact spelling, including leading zeroes.
pub struct Lookup {
    prefix: Vec<u8>,
    dense: Vec<u32>,
    sparse: FxHashMap<Vec<u8>, u32>,
}

impl Lookup {
    pub fn new<'a>(mut rows: impl ExactSizeIterator<Item = (&'a str, u32)>) -> Self {
        let limit = rows.len().saturating_mul(2).min(1 << 24);
        let first = rows.next();
        let name = first.map_or("", |(s, _)| s).as_bytes();
        let split = name
            .iter()
            .rposition(|c| !c.is_ascii_digit())
            .map_or(0, |i| i + 1);
        let mut lookup = Self {
            prefix: name[..split].to_vec(),
            dense: Vec::new(),
            sparse: FxHashMap::default(),
        };
        for (name, value) in first.into_iter().chain(rows) {
            if let Some(i) = lookup.index(name.as_bytes()).filter(|&i| i < limit) {
                if i >= lookup.dense.len() {
                    lookup.dense.resize(i + 1, u32::MAX);
                }
                lookup.dense[i] = value;
            } else {
                lookup.sparse.insert(name.as_bytes().to_vec(), value);
            }
        }
        lookup
    }

    #[inline]
    fn index(&self, name: &[u8]) -> Option<usize> {
        let digits = name.strip_prefix(self.prefix.as_slice())?;
        if digits.is_empty() || digits.len() > 8 || (digits[0] == b'0' && digits.len() != 1) {
            return None;
        }
        let mut value = 0;
        for &c in digits {
            let digit = c.wrapping_sub(b'0');
            if digit >= 10 {
                return None;
            }
            value = value * 10 + usize::from(digit);
        }
        Some(value)
    }

    #[inline]
    pub fn get(&self, name: &[u8]) -> Option<u32> {
        if let Some(&value) = self.index(name).and_then(|i| self.dense.get(i)) {
            return (value != u32::MAX).then_some(value);
        }
        self.sparse.get(name).copied()
    }
}

pub struct Catalog {
    pub start: i64,
    pub end: i64,
    pub products: Lookup,
    pub categories: Vec<String>,
    pub unknown: u32,
    pub campaign_ids: Lookup,
    pub campaigns: Vec<(String, String)>,
}

impl Catalog {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("manifest.json");
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )?;
        let mut dates = DateCache::default();
        let start = dates
            .utc(manifest.window_start.as_bytes())
            .context("invalid window_start")?;
        let end = dates
            .utc(manifest.window_end.as_bytes())
            .context("invalid window_end")?;
        ensure!(start < end, "empty or reversed manifest window");
        let read = |name| {
            let path = root.join(name);
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
        };
        let bytes = read("products.csv")?;
        let rows = dimension(&bytes).context("reading products.csv")?;
        let mut names: FxHashSet<_> = rows.iter().map(|&(_, c)| c).collect();
        names.insert("unknown");
        let mut categories: Vec<String> = names.into_iter().map(str::to_owned).collect();
        categories.sort_unstable();
        let ids: FxHashMap<&str, u32> = categories
            .iter()
            .enumerate()
            .map(|(i, c)| (c.as_str(), i as u32))
            .collect();
        let unknown = ids["unknown"];
        let products = Lookup::new(rows.into_iter().map(|(p, c)| (p, ids[c])));

        let mut campaigns = vec![("organic".to_owned(), "organic".to_owned())];
        let bytes = read("campaigns.csv")?;
        campaigns.extend(
            dimension(&bytes)
                .context("reading campaigns.csv")?
                .into_iter()
                .map(|(name, channel)| (name.to_owned(), channel.to_owned())),
        );
        let campaign_ids = Lookup::new(
            campaigns
                .iter()
                .enumerate()
                .skip(1)
                .map(|(i, (name, _))| (name.as_str(), i as u32)),
        );
        Ok(Self {
            start,
            end,
            products,
            categories,
            unknown,
            campaign_ids,
            campaigns,
        })
    }
}

struct Components {
    indices: FxHashMap<Key, usize>,
    parent: Vec<usize>,
    size: Vec<usize>,
    smallest: Vec<Key>,
}

impl Components {
    fn insert(&mut self, key: Key) -> usize {
        match self.indices.entry(key) {
            Entry::Occupied(slot) => *slot.get(),
            Entry::Vacant(slot) => {
                let i = self.parent.len();
                slot.insert(i);
                self.parent.push(i);
                self.size.push(1);
                self.smallest.push(key);
                i
            }
        }
    }

    fn root(&mut self, mut i: usize) -> usize {
        while i != self.parent[i] {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    fn join(&mut self, a: Key, b: Key, keys: &Keys) {
        let ai = self.insert(a);
        let bi = self.insert(b);
        let (mut a, mut b) = (self.root(ai), self.root(bi));
        if a == b {
            return;
        }
        if self.size[a] < self.size[b] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[b] = a;
        self.size[a] += self.size[b];
        if keys.compare(self.smallest[b], self.smallest[a]).is_lt() {
            self.smallest[a] = self.smallest[b];
        }
    }
}

pub type People = FxHashMap<Key, Key>;

/// Frequent aliases can be resolved without revisiting the shared graph table.
pub struct Resolver<'a> {
    aliases: &'a People,
    slots: Vec<(Key, Key)>,
}

impl<'a> Resolver<'a> {
    pub fn new(aliases: &'a People) -> Self {
        Self {
            aliases,
            slots: vec![(Key::EMPTY, Key::EMPTY); 1024],
        }
    }

    #[inline]
    pub fn get(&mut self, id: Key) -> Key {
        let index = id.bucket(self.slots.len());
        let slot = &mut self.slots[index];
        if slot.0 != id {
            *slot = (id, self.aliases.get(&id).copied().unwrap_or(id));
        }
        slot.1
    }
}

pub fn people(data: &Path, keys: &Keys) -> Result<People> {
    let input = Input::open(&data.join("links"))?;
    // Typical links have two short IDs. Reserving up front avoids repeatedly
    // moving the alias map as the connected components are built.
    let estimated_aliases = (input.bytes() / 16).min(1 << 22);
    let mut sets = Components {
        indices: FxHashMap::with_capacity_and_hasher(estimated_aliases, Default::default()),
        parent: Vec::with_capacity(estimated_aliases),
        size: Vec::with_capacity(estimated_aliases),
        smallest: Vec::with_capacity(estimated_aliases),
    };
    let mut reader = Reader::default();
    for chunk in input.chunks() {
        crate::csv::records(reader.body(chunk)?, |[a, b]| {
            if !a.is_empty() && !b.is_empty() {
                sets.join(keys.get(a), keys.get(b), keys);
            }
        });
    }
    let mut people = People::with_capacity_and_hasher(
        sets.parent
            .iter()
            .enumerate()
            .filter(|&(i, &p)| i != p)
            .count(),
        Default::default(),
    );
    for (name, index) in std::mem::take(&mut sets.indices) {
        let root = sets.root(index);
        let canonical = sets.smallest[root];
        if name != canonical {
            people.insert(name, canonical);
        }
    }
    Ok(people)
}
