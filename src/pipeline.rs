use crate::catalog::{Catalog, People};
use crate::input::{Shards, empty_buckets, transpose};
use crate::keys::{Key, Keys};
use crate::parse::{DAY, WEEK, fields, price, timestamp, utc};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, Default)]
pub struct Event<P> {
    pub person: Key,
    payload: P,
}

pub trait EventPayload: Copy + Default + Send {
    /// Time is the event's millisecond offset from the manifest start.
    fn new(time: i64, amount: u32, category: u32) -> Self;
    fn time(self) -> i64;
    fn purchase(self) -> (u32, u32);
}

impl<P: EventPayload> Event<P> {
    pub fn time(self) -> i64 {
        self.payload.time()
    }

    pub fn purchase(self) -> (u32, u32) {
        self.payload.purchase()
    }
}

#[derive(Clone, Copy, Default)]
pub struct WideEvent {
    time: i64,
    amount: u32,
    category: u32,
}

impl EventPayload for WideEvent {
    fn new(time: i64, amount: u32, category: u32) -> Self {
        Self {
            time,
            amount,
            category,
        }
    }

    fn time(self) -> i64 {
        self.time
    }

    fn purchase(self) -> (u32, u32) {
        (self.amount, self.category)
    }
}

#[derive(Clone, Copy, Default)]
pub struct CompactEvent(u64);

impl CompactEvent {
    // The purchase cap of 10,000,000 fits in 24 bits.
    const AMOUNT_BITS: u32 = 24;
    const CATEGORY_BITS: u32 = 9;
    const TIME_SHIFT: u32 = Self::AMOUNT_BITS + Self::CATEGORY_BITS;

    pub fn supports(catalog: &Catalog) -> bool {
        catalog.end - catalog.start <= (1 << (64 - Self::TIME_SHIFT))
            && catalog.categories.len() <= (1 << Self::CATEGORY_BITS)
    }
}

impl EventPayload for CompactEvent {
    fn new(time: i64, amount: u32, category: u32) -> Self {
        Self(
            (time as u64) << Self::TIME_SHIFT
                | u64::from(category) << Self::AMOUNT_BITS
                | u64::from(amount),
        )
    }

    fn time(self) -> i64 {
        (self.0 >> Self::TIME_SHIFT) as i64
    }

    fn purchase(self) -> (u32, u32) {
        let amount = self.0 as u32 & ((1 << Self::AMOUNT_BITS) - 1);
        let category = (self.0 >> Self::AMOUNT_BITS) as u32 & ((1 << Self::CATEGORY_BITS) - 1);
        (amount, category)
    }
}

#[derive(Clone, Copy)]
pub struct Candidate<P> {
    ingest: i64,
    event: Event<P>,
}

pub struct Click {
    pub person: Key,
    pub time: i64,
    pub id: Key,
    pub campaign: u32,
}

pub fn candidate<P: EventPayload>(
    line: &[u8],
    keys: &Keys,
    catalog: &Catalog,
    bins: usize,
) -> Option<(usize, (Key, Candidate<P>))> {
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
            payload: P::new(time - catalog.start, amount, category),
        })
    };
    // An invalid copy stays in contention. Its ingest can suppress a later valid copy.
    let event = validate().unwrap_or_default();
    Some((id.bucket(bins), (id, Candidate { ingest, event })))
}

pub fn deduplicate<P: EventPayload>(
    shards: Shards<(Key, Candidate<P>)>,
    people: &People,
    bins: usize,
) -> Shards<Event<P>> {
    let mut groups = empty_buckets(rayon::current_num_threads().min(shards.len()));
    let workers = groups.len();
    for (index, parts) in shards.into_iter().enumerate() {
        groups[index % workers].push(parts);
    }
    let workers = groups
        .into_par_iter()
        .map(|group| {
            let mut events = empty_buckets(bins);
            let mut route = |mut event: Event<P>| {
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
                let mut earliest = FxHashMap::<Key, Candidate<P>>::default();
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
