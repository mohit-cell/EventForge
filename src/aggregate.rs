use crate::catalog::Catalog;
use crate::keys::{Key, Keys};
use crate::parse::{DAY, GAP, WEEK};
use crate::pipeline::{Click, Event, EventPayload, PersonClick};
use anyhow::Result;
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
    who: Key,
    events: &[P],
    clicks: &[PersonClick],
    origin: i64,
    spend: &mut Spend,
    active: &mut Active,
    out: &mut Summary,
) {
    let new_day = |day| UserDay {
        person: who,
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

#[derive(Default)]
struct Group {
    person: Key,
    events: usize,
    clicks: usize,
    event_cursor: usize,
    click_cursor: usize,
}

pub type PersonShard<P> = (Vec<Vec<Event<P>>>, Vec<Vec<Click>>);

/// Count and scatter into contiguous person ranges. The second pass uses
/// recorded group indices, so each source row needs just one dictionary probe.
fn summarize_shard<P: EventPayload>(
    (events, clicks): PersonShard<P>,
    keys: &Keys,
    catalog: &Catalog,
    spend: &mut Spend,
    active: &mut Active,
    summary: &mut Summary,
) {
    let n: usize = events.iter().map(Vec::len).sum();
    if n == 0 {
        return;
    }
    let mut map = FxHashMap::with_capacity_and_hasher(n / 32 + 1, Default::default());
    let mut groups: Vec<Group> = Vec::with_capacity(n / 32 + 1);
    let mut indices = Vec::with_capacity(n);
    for event in events.iter().flatten() {
        let i = *map.entry(event.person).or_insert_with(|| {
            let i = groups.len();
            groups.push(Group {
                person: event.person,
                ..Default::default()
            });
            i
        });
        groups[i].events += 1;
        indices.push(i);
    }
    let mut click_indices = Vec::with_capacity(clicks.iter().map(Vec::len).sum());
    for click in clicks.iter().flatten() {
        let i = map.get(&click.person).copied().unwrap_or(usize::MAX);
        if i != usize::MAX {
            groups[i].clicks += 1;
        }
        click_indices.push(i);
    }
    let (mut ne, mut nc) = (0, 0);
    for group in &mut groups {
        group.event_cursor = ne;
        group.click_cursor = nc;
        ne += group.events;
        nc += group.clicks;
    }
    let mut times = vec![P::default(); ne];
    for (event, i) in events.into_iter().flatten().zip(indices) {
        let cursor = &mut groups[i].event_cursor;
        times[*cursor] = event.payload;
        *cursor += 1;
    }
    let mut visits = vec![PersonClick::default(); nc];
    for (click, i) in clicks.into_iter().flatten().zip(click_indices) {
        if i != usize::MAX {
            let cursor = &mut groups[i].click_cursor;
            visits[*cursor] = click.data;
            *cursor += 1;
        }
    }
    groups.sort_unstable_by(|a, b| keys.compare(a.person, b.person));
    let mut scratch = Vec::new();
    for group in groups {
        let events = &mut times[group.event_cursor - group.events..group.event_cursor];
        let clicks = &mut visits[group.click_cursor - group.clicks..group.click_cursor];
        P::sort(events, &mut scratch);
        clicks.sort_unstable_by(|a, b| a.time.cmp(&b.time).then_with(|| keys.compare(b.id, a.id)));
        // Identical click copies are adjacent. Compact the range in place.
        let mut distinct = 0;
        for i in 0..clicks.len() {
            if distinct == 0 || clicks[distinct - 1].id != clicks[i].id {
                clicks[distinct] = clicks[i];
                distinct += 1;
            }
        }
        person(
            group.person,
            events,
            &clicks[..distinct],
            catalog.start,
            spend,
            active,
            summary,
        );
    }
}

pub fn summarize<P: EventPayload>(
    shards: Vec<PersonShard<P>>,
    keys: &Keys,
    catalog: &Catalog,
    mut flush: impl FnMut(&mut Vec<UserDay>) -> Result<()>,
) -> Result<Summary> {
    let mut out = Summary {
        users: Vec::new(),
        campaigns: Campaigns::new(catalog),
    };
    let mut spend = Spend {
        cents: vec![0; catalog.categories.len()],
        used: Vec::new(),
    };
    let mut active = Active::new(catalog.campaigns.len());
    for shard in shards {
        summarize_shard(shard, keys, catalog, &mut spend, &mut active, &mut out);
        flush(&mut out.users)?;
    }
    Ok(out)
}
