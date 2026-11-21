mod aggregate;
mod catalog;
mod input;
mod keys;
mod output;
mod parse;
mod pipeline;

use anyhow::{Context, Result, ensure};
use input::Input;
use keys::Keys;
use rayon::prelude::*;
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
    let mut timer = Timer(Instant::now());
    let keys = Keys::default();
    let catalog = catalog::Catalog::load(data)?;
    let people = catalog::people(data, &keys)?;
    timer.report("dimensions and links");

    let input = Input::open(&data.join("events"))?;
    let bins = input
        .bytes()
        .div_ceil(4 << 20)
        .clamp(1, 256)
        .next_power_of_two();
    let candidates = input::partition(&input, bins, |line| {
        pipeline::candidate(line, &keys, &catalog, bins)
    });
    drop(input);
    timer.report("parse events");
    let events = pipeline::deduplicate(candidates, &people, bins);
    timer.report("deduplicate and partition people");

    let input = Input::open(&data.join("clicks"))?;
    let clicks = input::partition(&input, bins, |line| {
        pipeline::click(line, &keys, &catalog, &people, bins)
    });
    drop(input);
    drop(people);
    timer.report("parse clicks");
    let results: Vec<_> = events
        .into_par_iter()
        .zip(clicks)
        .map(|(events, clicks)| aggregate::summarize(events, clicks, &keys, &catalog))
        .collect();
    timer.report("sort, sessionize and attribute");
    let counts = output::write(output, results, &keys, &catalog)?;
    timer.report("write parquet");
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
