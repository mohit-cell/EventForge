use crate::catalog::{Catalog, People};
use crate::input::{empty_buckets, transpose};
use crate::keys::{Key, Keys};
use crate::parse::{DAY, WEEK, fields, price, timestamp, utc};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::collections::hash_map::Entry;

#[derive(Clone, Copy, Default)]
pub struct Event {
    pub person: Key,
    pub time: i64,
    pub amount: u32,
    pub category: u32,
}

#[derive(Clone, Copy)]
pub struct Candidate {
    ingest: i64,
    event: Event,
}

pub struct Click {
    pub person: Key,
    pub time: i64,
    pub id: Key,
    pub campaign: u32,
}

pub fn candidate(
    line: &[u8],
    keys: &Keys,
    catalog: &Catalog,
    bins: usize,
) -> Option<(usize, (Key, Candidate))> {
    let [id, user, ts, ingest, kind, product, cents] = fields(line)?;
    if id.is_empty() {
        return None;
    }
    let id = keys.get(id);
    let ingest = utc(ingest)?;
    let validate = || {
        if user.is_empty() || product.is_empty() {
            return None;
        }
        let time = timestamp(ts)?;
        if time < catalog.start || time >= catalog.end || ingest - time > DAY {
            return None;
        }
        let (amount, category) = match kind {
            b"view" | b"cart" => (0, 0),
            b"purchase" => (
                price(cents)?,
                *catalog.products.get(product).unwrap_or(&catalog.unknown),
            ),
            _ => return None,
        };
        Some(Event {
            person: keys.get(user),
            time,
            amount,
            category,
        })
    };
    // An invalid copy stays in contention. Its ingest can suppress a later valid copy.
    let event = validate().unwrap_or_default();
    Some((id.bucket(bins), (id, Candidate { ingest, event })))
}

pub fn deduplicate(
    shards: Vec<Vec<Vec<(Key, Candidate)>>>,
    people: &People,
    bins: usize,
) -> Vec<Vec<Vec<Event>>> {
    let workers = shards
        .into_par_iter()
        .map(|parts| {
            let capacity = parts.iter().map(Vec::len).sum();
            let mut earliest = FxHashMap::with_capacity_and_hasher(capacity, Default::default());
            for (id, row) in parts.into_iter().flatten() {
                match earliest.entry(id) {
                    Entry::Vacant(slot) => {
                        slot.insert(row);
                    }
                    Entry::Occupied(mut slot) if row.ingest < slot.get().ingest => {
                        slot.insert(row);
                    }
                    _ => {}
                }
            }
            let mut events = empty_buckets(bins);
            for row in earliest.into_values() {
                let mut event = row.event;
                if event.person != Key::EMPTY {
                    event.person = *people.get(&event.person).unwrap_or(&event.person);
                    events[event.person.bucket(bins)].push(event);
                }
            }
            events
        })
        .collect();
    transpose(workers, bins)
}

pub fn click(
    line: &[u8],
    keys: &Keys,
    catalog: &Catalog,
    people: &People,
    bins: usize,
) -> Option<(usize, Click)> {
    let [id, user, campaign, ts] = fields(line)?;
    if id.is_empty() || user.is_empty() {
        return None;
    }
    let campaign = *catalog.campaign_ids.get(campaign)?;
    let time = timestamp(ts)?;
    if time < catalog.start - WEEK || time >= catalog.end {
        return None;
    }
    let user = keys.get(user);
    let person = *people.get(&user).unwrap_or(&user);
    Some((
        person.bucket(bins),
        Click {
            person,
            time,
            id: keys.get(id),
            campaign,
        },
    ))
}
