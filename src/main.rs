mod aggregate;
mod catalog;
mod csv;
mod input;
mod keys;
mod output;
mod parse;
mod pipeline;

use anyhow::{Context, Result, ensure};
use catalog::{Catalog, Resolver};
use input::Input;
use keys::Keys;
use parse::DateCache;
use pipeline::{CompactEvent, EventPayload, WideEvent};
use std::env;
use std::path::{Path, PathBuf};
use std::time::Instant;

struct Timer(Instant);
impl Timer {
    fn report(&mut self, phase: &str) {
        eprintln!("{phase} {:.3}s", self.0.elapsed().as_secs_f64());
        self.0 = Instant::now();
    }
}

fn run(data: &Path, output: &Path) -> Result<()> {
    let timer = Timer(Instant::now());
    let keys = Keys::default();
    let catalog = Catalog::load(data)?;
    let input = Input::open(&data.join("events"))?;
    if CompactEvent::supports(&catalog) {
        process::<CompactEvent>(data, output, &keys, &catalog, input, timer)
    } else {
        process::<WideEvent>(data, output, &keys, &catalog, input, timer)
    }
}

fn process<P: EventPayload>(
    data: &Path,
    output: &Path,
    keys: &Keys,
    catalog: &Catalog,
    input: Input,
    mut timer: Timer,
) -> Result<()> {
    let bins = input
        .bytes()
        .div_ceil(4 << 20)
        .clamp(1, 256)
        .next_power_of_two();
    let (people, candidates) = rayon::join(
        || catalog::people(data, keys),
        || {
            input::partition(&input, bins, || {
                let mut dates = DateCache::default();
                move |fields: [&[u8]; 7]| {
                    pipeline::candidate::<P>(fields, &mut dates, keys, catalog, bins)
                }
            })
        },
    );
    let people = people?;
    drop(input);
    timer.report("dimensions, links and event parsing");
    let events = pipeline::deduplicate(candidates?, &people, bins);
    timer.report("deduplicate and partition people");

    let input = Input::open(&data.join("clicks"))?;
    let clicks = input::partition(&input, bins, || {
        let mut dates = DateCache::default();
        let mut resolver = Resolver::new(&people);
        move |fields: [&[u8]; 4]| {
            pipeline::click(fields, &mut dates, keys, catalog, &mut resolver, bins)
        }
    })?;
    drop(input);
    drop(people);
    timer.report("parse clicks");
    let counts = output::write(output, events, clicks, keys, catalog)?;
    timer.report("group, sessionize, attribute and write parquet");
    eprintln!("{} person-days, {} campaign-days", counts.0, counts.1);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<PathBuf> = env::args_os().skip(1).map(PathBuf::from).collect();
    ensure!(
        args.len() == 2 || args.is_empty(),
        "usage: eventforge [DATA_DIR OUTPUT_DIR]"
    );
    if let Ok(raw) = env::var("NCPU") {
        let n: usize = raw.parse().context("NCPU must be a positive integer")?;
        ensure!(n > 0, "NCPU must be positive");
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()?;
    }
    let dir = |i, name, fallback| {
        args.get(i)
            .cloned()
            .or_else(|| env::var_os(name).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(fallback))
    };
    run(
        &dir(0, "DATA_DIR", "/data"),
        &dir(1, "OUTPUT_DIR", "/output"),
    )
}

#[cfg(test)]
mod tests;
