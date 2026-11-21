use crate::catalog::{Catalog, People};
use crate::input::{empty_buckets, transpose};
use crate::keys::{Key, Keys};
use crate::parse::{DAY, WEEK, fields, price, timestamp, utc};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

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
    let mut groups = empty_buckets(rayon::current_num_threads().min(shards.len()));
    let workers = groups.len();
    for (index, parts) in shards.into_iter().enumerate() {
        groups[index % workers].push(parts);
    }
    let workers = groups
        .into_par_iter()
        .map(|group| {
            let mut events = empty_buckets(bins);
            let mut route = |mut event: Event| {
                if event.person != Key::EMPTY {
                    event.person = *people.get(&event.person).unwrap_or(&event.person);
                    events[event.person.bucket(bins)].push(event);
                }
            };
            for parts in group {
                let count: usize = parts.iter().map(Vec::len).sum();
                // Use at least eight hash slots per candidate. A singleton slot
                // proves its ID is unique; repeated slots need full Key equality.
                let words = count.div_ceil(8).next_power_of_two();
                let mut hits = vec![[0u64; 2]; words];
                let slot = |id: Key| {
                    let hash = id.mixed() >> bins.trailing_zeros();
                    ((hash >> 6) as usize & (words - 1), 1u64 << (hash & 63))
                };
                for &(id, _) in parts.iter().flatten() {
                    let (word, bit) = slot(id);
                    let [seen, repeated] = &mut hits[word];
                    *repeated |= *seen & bit;
                    *seen |= bit;
                }
                let mut earliest = FxHashMap::<Key, Candidate>::default();
                for (id, row) in parts.into_iter().flatten() {
                    let (word, bit) = slot(id);
                    if hits[word][1] & bit == 0 {
                        route(row.event);
                    } else {
                        let first = earliest.entry(id).or_insert(row);
                        if row.ingest < first.ingest {
                            *first = row;
                        }
                    }
                }
                for row in earliest.into_values() {
                    route(row.event);
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
