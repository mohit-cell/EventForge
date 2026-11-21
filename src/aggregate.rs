use crate::catalog::Catalog;
use crate::keys::{Key, Keys};
use crate::parse::{DAY, GAP, WEEK};
use crate::pipeline::{Click, Event, EventPayload};
use rustc_hash::FxHashMap;

#[derive(Default)]
pub struct UserDay {
    pub person: Key,
    pub day: i32,
    pub sessions: i64,
    pub events: i64,
    pub revenue: i64,
    pub top: Option<u32>,
}

#[derive(Clone, Default)]
pub struct CampaignDay {
    pub sessions: i64,
    pub purchasing: i64,
    pub revenue: i64,
    pub assists: i64,
}

impl CampaignDay {
    fn credit(&mut self, revenue: i64) {
        self.sessions += 1;
        self.revenue += revenue;
        self.purchasing += i64::from(revenue > 0);
    }

    fn merge(&mut self, other: &Self) {
        self.sessions += other.sessions;
        self.purchasing += other.purchasing;
        self.revenue += other.revenue;
        self.assists += other.assists;
    }
}

pub enum Campaigns {
    Dense {
        first_day: i32,
        width: usize,
        rows: Vec<CampaignDay>,
    },
    Sparse(FxHashMap<(u32, i32), CampaignDay>),
}

impl Campaigns {
    fn new(catalog: &Catalog) -> Self {
        let first_day = catalog.start.div_euclid(DAY) as i32;
        let days = (catalog.end - 1).div_euclid(DAY) as i32 - first_day + 1;
        let width = catalog.campaigns.len();
        // Bound the per-partition allocation when the manifest spans many days
        // or there are too many campaigns for a small direct-indexed table.
        if let Some(size @ ..=32_768) = width.checked_mul(days as usize) {
            Self::Dense {
                first_day,
                width,
                rows: vec![CampaignDay::default(); size],
            }
        } else {
            Self::Sparse(FxHashMap::default())
        }
    }

    fn record(&mut self, day: i32, winner: u32, revenue: i64, assists: &[u32]) {
        match self {
            Self::Dense {
                first_day,
                width,
                rows,
            } => {
                let start = (day - *first_day) as usize * *width;
                let day = &mut rows[start..start + *width];
                for &campaign in assists {
                    day[campaign as usize].assists += 1;
                }
                day[winner as usize].credit(revenue);
            }
            Self::Sparse(rows) => {
                for &campaign in assists {
                    rows.entry((campaign, day)).or_default().assists += 1;
                }
                rows.entry((winner, day)).or_default().credit(revenue);
            }
        }
    }

    pub fn merge(mut self, other: Self) -> Self {
        match (&mut self, other) {
            (Self::Dense { rows, .. }, Self::Dense { rows: other, .. }) => {
                for (row, other) in rows.iter_mut().zip(&other) {
                    row.merge(other);
                }
            }
            (Self::Sparse(rows), Self::Sparse(other)) => {
                for (key, row) in other {
                    rows.entry(key).or_default().merge(&row);
                }
            }
            _ => unreachable!("all partitions use the same catalog"),
        }
        self
    }

    pub fn into_rows(self) -> Vec<((u32, i32), CampaignDay)> {
        match self {
            Self::Dense {
                first_day,
                width,
                rows,
            } => rows
                .into_iter()
                .enumerate()
                .filter(|(_, row)| row.sessions != 0 || row.assists != 0)
                .map(|(i, row)| (((i % width) as u32, first_day + (i / width) as i32), row))
                .collect(),
            Self::Sparse(rows) => rows.into_iter().collect(),
        }
    }
}

pub struct Summary {
    pub users: Vec<UserDay>,
    pub campaigns: Campaigns,
}

struct Spend {
    cents: Vec<i64>,
    used: Vec<u32>,
}

impl Spend {
    fn add(&mut self, category: u32, amount: u32) {
        let value = &mut self.cents[category as usize];
        if *value == 0 {
            self.used.push(category);
        }
        *value += i64::from(amount);
    }

    fn finish_day(&mut self) -> Option<u32> {
        // Category numbers are assigned in byte order, so the smaller number wins a tie.
        let top = self
            .used
            .iter()
            .copied()
            .min_by_key(|&c| (-self.cents[c as usize], c));
        for c in self.used.drain(..) {
            self.cents[c as usize] = 0;
        }
        top
    }
}

/// Counts support constant-time expiry. The dense active list enumerates each
/// distinct assisting campaign once, including campaigns with no credited session.
struct Active {
    counts: Vec<usize>,
    positions: Vec<usize>,
    campaigns: Vec<u32>,
}

impl Active {
    fn new(size: usize) -> Self {
        Self {
            counts: vec![0; size],
            positions: vec![0; size],
            campaigns: Vec::new(),
        }
    }

    fn add(&mut self, id: u32) {
        let i = id as usize;
        if self.counts[i] == 0 {
            self.positions[i] = self.campaigns.len();
            self.campaigns.push(id);
        }
        self.counts[i] += 1;
    }

    fn remove(&mut self, id: u32) {
        let i = id as usize;
        self.counts[i] -= 1;
        if self.counts[i] == 0 {
            let at = self.positions[i];
            self.campaigns.swap_remove(at);
            if let Some(&moved) = self.campaigns.get(at) {
                self.positions[moved as usize] = at;
            }
        }
    }

    fn reset(&mut self) {
        for id in self.campaigns.drain(..) {
            self.counts[id as usize] = 0;
        }
    }
}

fn person<P: EventPayload>(
    events: &[Event<P>],
    clicks: &[Click],
    origin: i64,
    spend: &mut Spend,
    active: &mut Active,
    out: &mut Summary,
) {
    let new_day = |day| UserDay {
        person: events[0].person,
        day,
        ..Default::default()
    };
    let mut daily = new_day((events[0].time() + origin).div_euclid(DAY) as i32);
    let (mut left, mut right, mut i) = (0, 0, 0);
    while i < events.len() {
        let start = events[i].time() + origin;
        let day = start.div_euclid(DAY) as i32;
        if daily.day != day {
            daily.top = spend.finish_day();
            out.users.push(daily);
            daily = new_day(day);
        }

        let first = i;
        let mut revenue = 0;
        loop {
            let event = events[i];
            let (amount, category) = event.purchase();
            if amount > 0 {
                spend.add(category, amount);
                revenue += i64::from(amount);
            }
            i += 1;
            if i == events.len() || events[i].time() - event.time() > GAP {
                break;
            }
        }
        daily.sessions += 1;
        daily.events += (i - first) as i64;
        daily.revenue += revenue;

        while right < clicks.len() && clicks[right].time <= start {
            active.add(clicks[right].campaign);
            right += 1;
        }
        while left < right && clicks[left].time < start - WEEK {
            active.remove(clicks[left].campaign);
            left += 1;
        }
        let winner = if left < right {
            clicks[right - 1].campaign
        } else {
            0
        };
        out.campaigns
            .record(day, winner, revenue, &active.campaigns);
    }
    daily.top = spend.finish_day();
    out.users.push(daily);
    active.reset();
}

fn gather<T>(parts: Vec<Vec<T>>) -> Vec<T> {
    let mut parts = parts.into_iter();
    let mut rows = parts.next().unwrap_or_default();
    rows.reserve_exact(parts.as_slice().iter().map(Vec::len).sum());
    for mut part in parts {
        rows.append(&mut part);
    }
    rows
}

pub fn summarize<P: EventPayload>(
    events: Vec<Vec<Event<P>>>,
    clicks: Vec<Vec<Click>>,
    keys: &Keys,
    catalog: &Catalog,
) -> Summary {
    let mut events = gather(events);
    let mut clicks = gather(clicks);
    events.sort_unstable_by_key(|e| e.person);
    // The last eligible click must have the smallest byte ID among ties.
    clicks.sort_unstable_by(|a, b| {
        a.person
            .cmp(&b.person)
            .then(a.time.cmp(&b.time))
            .then_with(|| keys.compare(b.id, a.id))
    });
    clicks.dedup_by(|a, b| a.id == b.id && a.person == b.person && a.time == b.time);

    let mut out = Summary {
        users: Vec::new(),
        campaigns: Campaigns::new(catalog),
    };
    let mut spend = Spend {
        cents: vec![0; catalog.categories.len()],
        used: Vec::new(),
    };
    let mut active = Active::new(catalog.campaigns.len());
    let mut remaining = clicks.as_slice();
    for rows in events.chunk_by_mut(|a, b| a.person == b.person) {
        rows.sort_unstable_by_key(|e| e.time());
        let who = rows[0].person;
        while remaining.first().is_some_and(|c| c.person < who) {
            remaining = &remaining[1..];
        }
        let count = remaining.iter().take_while(|c| c.person == who).count();
        let (eligible_person, rest) = remaining.split_at(count);
        person(
            rows,
            eligible_person,
            catalog.start,
            &mut spend,
            &mut active,
            &mut out,
        );
        remaining = rest;
    }
    out
}
