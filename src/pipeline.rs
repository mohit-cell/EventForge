use crate::catalog::{Catalog, People, Resolver};
use crate::input::{Bucket, Shards, empty_buckets, transpose};
use crate::keys::{Key, Keys};
use crate::parse::{DAY, DateCache, WEEK, price};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, Default)]
pub struct Event<P> {
    pub person: Key,
    pub payload: P,
}

pub trait EventPayload: Copy + Default + Send {
    /// Time is the event's millisecond offset from the manifest start.
    fn new(time: i64, amount: u32, category: u32) -> Self;
    fn time(self) -> i64;
    fn purchase(self) -> (u32, u32);

    fn sort(events: &mut [Self], _scratch: &mut Vec<Self>) {
        events.sort_unstable_by_key(|e| e.time());
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

    fn sort(events: &mut [Self], scratch: &mut Vec<Self>) {
        if events.len() < 4096 {
            events.sort_unstable_by_key(|e| e.time());
            return;
        }
        // Three stable counting passes cover the full 31-bit time offset.
        const BITS: usize = 11;
        const WIDTH: usize = 1 << BITS;
        let digit =
            |event: Self, pass: usize| (event.time() as usize >> (pass * BITS)) & (WIDTH - 1);
        let mut positions = [[0usize; WIDTH]; 3];
        for &event in events.iter() {
            for (pass, hist) in positions.iter_mut().enumerate() {
                hist[digit(event, pass)] += 1;
            }
        }
        for hist in &mut positions {
            let mut sum = 0;
            for count in hist {
                let next = sum + *count;
                *count = sum;
                sum = next;
            }
        }
        scratch.resize(events.len(), Self::default());
        for (pass, hist) in positions.iter_mut().enumerate() {
            let (from, to): (&[Self], &mut [Self]) = if pass % 2 == 0 {
                (events, scratch)
            } else {
                (scratch, events)
            };
            for &event in from {
                let pos = &mut hist[digit(event, pass)];
                to[*pos] = event;
                *pos += 1;
            }
        }
        events.copy_from_slice(scratch);
    }
}

#[derive(Clone, Copy)]
pub struct Candidate<P> {
    ingest: i64,
    event: Event<P>,
}

#[derive(Clone, Copy)]
struct Staged {
    id: Key,
    person: Key,
    data: u64,
}

/// Most candidates need only the event offset and ingest seconds. Store a
/// full payload separately for purchases and times outside the compact range.
#[derive(Default)]
pub struct Candidates<P> {
    full: Vec<Vec<Staged>>,
    rows: Vec<Staged>,
    extra: Vec<Candidate<P>>,
}

const STAGED_PER_PAGE: usize = 4096;

impl<P> Candidates<P> {
    fn len(&self) -> usize {
        self.full.len() * STAGED_PER_PAGE + self.rows.len()
    }
}

impl<P: EventPayload> Bucket<(Key, Candidate<P>)> for Candidates<P> {
    fn is_empty(&self) -> bool {
        self.full.is_empty() && self.rows.is_empty()
    }

    fn push(&mut self, (id, row): (Key, Candidate<P>)) {
        let mut person = row.event.person;
        let payload = row.event.payload;
        let data = if person == Key::EMPTY {
            // Ingest is a difference of four-digit civil dates in whole seconds.
            (row.ingest as u64) << 1 | 1
        } else if payload.purchase().0 == 0
            && payload.time() <= i64::from(u32::MAX)
            && i32::try_from(row.ingest).is_ok()
        {
            ((row.ingest as u64) << 32) | payload.time() as u64
        } else {
            person = Key::EMPTY;
            let index = self.extra.len();
            self.extra.push(row);
            (index as u64) << 1
        };
        if self.rows.len() == STAGED_PER_PAGE {
            self.full.push(std::mem::replace(
                &mut self.rows,
                Vec::with_capacity(STAGED_PER_PAGE),
            ));
        }
        self.rows.push(Staged { id, person, data });
    }
}

impl Staged {
    #[inline]
    fn decode<P: EventPayload>(self, extra: &[Candidate<P>]) -> Candidate<P> {
        if self.person != Key::EMPTY {
            Candidate {
                ingest: (self.data as i64) >> 32,
                event: Event {
                    person: self.person,
                    payload: P::new(i64::from(self.data as u32), 0, 0),
                },
            }
        } else if self.data & 1 != 0 {
            Candidate {
                ingest: (self.data as i64) >> 1,
                event: Event::default(),
            }
        } else {
            extra[(self.data >> 1) as usize]
        }
    }
}

pub struct Click {
    pub person: Key,
    pub data: PersonClick,
}

#[derive(Clone, Copy, Default)]
pub struct PersonClick {
    pub time: i64,
    pub id: Key,
    pub campaign: u32,
}

pub fn candidate<P: EventPayload>(
    [id, user, ts, ingest, kind, product, cents]: [&[u8]; 7],
    dates: &mut DateCache,
    keys: &Keys,
    catalog: &Catalog,
    bins: usize,
) -> Option<(usize, (Key, Candidate<P>))> {
    if id.is_empty() {
        return None;
    }
    let id = keys.get(id);
    let ingest = dates.utc(ingest)?;
    let mut validate = || {
        if user.is_empty() || product.is_empty() {
            return None;
        }
        let time = dates.timestamp(ts)?;
        if time < catalog.start || time >= catalog.end || ingest - time > DAY {
            return None;
        }
        let (amount, category) = match kind {
            b"view" | b"cart" => (0, 0),
            b"purchase" => (
                price(cents)?,
                catalog.products.get(product).unwrap_or(catalog.unknown),
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
    let ingest = (ingest - catalog.start) / 1000;
    Some((id.bucket(bins), (id, Candidate { ingest, event })))
}

pub fn deduplicate<P: EventPayload>(
    shards: Vec<Vec<Candidates<P>>>,
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
            let mut resolver = Resolver::new(people);
            let mut route = |mut event: Event<P>| {
                if event.person != Key::EMPTY {
                    event.person = resolver.get(event.person);
                    events[event.person.bucket(bins)].push(event);
                }
            };
            for parts in group {
                let count: usize = parts.iter().map(Candidates::len).sum();
                // Use at least eight hash slots per candidate. A singleton slot
                // proves its ID is unique; repeated slots need full Key equality.
                let words = count.div_ceil(8).next_power_of_two();
                let mut hits = vec![[0u64; 2]; words];
                let slot = |id: Key| {
                    let hash = id.mixed() >> bins.trailing_zeros();
                    ((hash >> 6) as usize & (words - 1), 1u64 << (hash & 63))
                };
                for row in parts
                    .iter()
                    .flat_map(|p| p.full.iter().flatten().chain(&p.rows))
                {
                    let (word, bit) = slot(row.id);
                    let [seen, repeated] = &mut hits[word];
                    *repeated |= *seen & bit;
                    *seen |= bit;
                }
                let mut earliest = FxHashMap::<Key, Candidate<P>>::default();
                for part in parts {
                    for staged in part.full.into_iter().chain([part.rows]).flatten() {
                        let id = staged.id;
                        let row = staged.decode(&part.extra);
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
                }
                for row in earliest.into_values() {
                    route(row.event);
                }
            }
            events
        })
        .collect();
    transpose(workers, bins, Vec::is_empty)
}

pub fn click(
    [id, user, campaign, ts]: [&[u8]; 4],
    dates: &mut DateCache,
    keys: &Keys,
    catalog: &Catalog,
    resolver: &mut Resolver<'_>,
    bins: usize,
) -> Option<(usize, Click)> {
    if id.is_empty() || user.is_empty() {
        return None;
    }
    let campaign = catalog.campaign_ids.get(campaign)?;
    let time = dates.timestamp(ts)?;
    if time < catalog.start - WEEK || time >= catalog.end {
        return None;
    }
    let user = keys.get(user);
    let person = resolver.get(user);
    Some((
        person.bucket(bins),
        Click {
            person,
            data: PersonClick {
                time,
                id: keys.get(id),
                campaign,
            },
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_candidates_round_trip_extreme_ingests_and_wide_payloads() {
        let keys = Keys::default();
        let who = keys.get(b"person");
        let id = keys.get(b"event");
        let mut part = Candidates::default();
        for (person, time, ingest, amount, category) in [
            (who, 0, i64::from(i32::MIN), 0, 0),
            (who, i64::from(u32::MAX), i64::from(i32::MAX), 0, 0),
            (who, i64::from(u32::MAX) + 1, -1, 0, 0),
            (who, 0, i64::from(i32::MIN) - 1, 0, 0),
            (who, 0, i64::from(i32::MAX) + 1, 10_000_000, 513),
            (Key::EMPTY, 0, -315_537_811_200, 0, 0),
            (Key::EMPTY, 0, 315_537_811_200, 0, 0),
        ] {
            part.push((
                id,
                Candidate {
                    ingest,
                    event: Event {
                        person,
                        payload: WideEvent::new(time, amount, category),
                    },
                },
            ));
            let staged = *part.rows.last().unwrap();
            let decoded = staged.decode(&part.extra);
            assert_eq!(staged.id, id);
            assert_eq!(decoded.ingest, ingest);
            assert_eq!(decoded.event.person, person);
            assert_eq!(decoded.event.payload.time(), time);
            assert_eq!(decoded.event.payload.purchase(), (amount, category));
        }
        assert_eq!(part.extra.len(), 3);
        assert_eq!(std::mem::size_of::<Staged>(), 24);
    }
}
