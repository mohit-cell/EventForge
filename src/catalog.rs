use crate::input::{Input, dimension, lines};
use crate::keys::{Key, Keys};
use crate::parse::{fields, utc};
use anyhow::{Context, Result, ensure};
use rustc_hash::FxHashMap;
use serde::Deserialize;
use std::collections::hash_map::Entry;
use std::path::Path;

#[derive(Deserialize)]
struct Manifest {
    window_start: String,
    window_end: String,
}

pub struct Catalog {
    pub start: i64,
    pub end: i64,
    pub products: FxHashMap<Vec<u8>, u32>,
    pub categories: Vec<String>,
    pub unknown: u32,
    pub campaign_ids: FxHashMap<Vec<u8>, u32>,
    pub campaigns: Vec<(String, String)>,
}

impl Catalog {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("manifest.json");
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )?;
        let start = utc(manifest.window_start.as_bytes()).context("invalid window_start")?;
        let end = utc(manifest.window_end.as_bytes()).context("invalid window_end")?;
        ensure!(start < end, "empty or reversed manifest window");
        let rows = dimension(&root.join("products.csv"))?;
        let mut categories: Vec<_> = rows.iter().map(|(_, c)| c.clone()).collect();
        categories.push("unknown".to_owned());
        categories.sort_unstable();
        categories.dedup();
        let ids: FxHashMap<&str, u32> = categories
            .iter()
            .enumerate()
            .map(|(i, c)| (c.as_str(), i as u32))
            .collect();
        let unknown = ids["unknown"];
        let products = rows
            .into_iter()
            .map(|(p, c)| (p.into_bytes(), ids[c.as_str()]))
            .collect();

        let mut campaigns = vec![("organic".to_owned(), "organic".to_owned())];
        campaigns.extend(dimension(&root.join("campaigns.csv"))?);
        let campaign_ids = campaigns
            .iter()
            .enumerate()
            .skip(1)
            .map(|(id, (name, _))| (name.as_bytes().to_vec(), id as u32))
            .collect();
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

#[derive(Default)]
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

pub fn people(data: &Path, keys: &Keys) -> Result<People> {
    let input = Input::open(&data.join("links"))?;
    let mut sets = Components::default();
    for chunk in input.chunks() {
        for row in lines(chunk.body()) {
            if let Some([a, b]) = fields(row)
                && !a.is_empty()
                && !b.is_empty()
            {
                sets.join(keys.get(a), keys.get(b), keys);
            }
        }
    }
    let mut people = People::default();
    for (name, index) in std::mem::take(&mut sets.indices) {
        let root = sets.root(index);
        let canonical = sets.smallest[root];
        if name != canonical {
            people.insert(name, canonical);
        }
    }
    Ok(people)
}
