use anyhow::{Context, Result, ensure};
use memchr::memchr;
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Input {
    maps: Vec<Mmap>,
}

impl Input {
    pub fn open(dir: &Path) -> Result<Self> {
        let mut paths = Vec::<PathBuf>::new();
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "csv") && path.is_file() {
                paths.push(path);
            }
        }
        paths.sort();
        let mut maps = Vec::with_capacity(paths.len());
        for path in paths {
            let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
            if file.metadata()?.len() > 0 {
                // The input files are mounted read-only for the duration of the run.
                maps.push(
                    unsafe { Mmap::map(&file) }
                        .with_context(|| format!("mapping {}", path.display()))?,
                );
            }
        }
        Ok(Self { maps })
    }

    pub fn bytes(&self) -> usize {
        self.maps.iter().map(|m| m.len()).sum()
    }

    pub fn chunks(&self) -> Vec<Chunk<'_>> {
        const CHUNK: usize = 1 << 20;
        let mut chunks = Vec::new();
        for map in &self.maps {
            for start in (0..map.len()).step_by(CHUNK) {
                chunks.push(Chunk {
                    file: map,
                    start,
                    end: (start + CHUNK).min(map.len()),
                });
            }
        }
        chunks
    }
}

pub struct Chunk<'a> {
    file: &'a [u8],
    start: usize,
    end: usize,
}

impl<'a> Chunk<'a> {
    /// Own every row whose first byte is in this interval. A worker skips a
    /// partial first row and finishes its last row, even across several chunks.
    /// Compute boundaries on the worker so cold page faults are parallel too.
    pub fn body(&self) -> &'a [u8] {
        let mut start = self.start;
        if start == 0 || self.file[start - 1] != b'\n' {
            let Some(lf) = memchr(b'\n', &self.file[start..self.end]) else {
                return &[];
            };
            start += lf + 1;
        }
        if start == self.end {
            return &[];
        }
        let end = if self.end == self.file.len() || self.file[self.end - 1] == b'\n' {
            self.end
        } else {
            memchr(b'\n', &self.file[self.end..]).map_or(self.file.len(), |n| self.end + n + 1)
        };
        &self.file[start..end]
    }
}

pub fn after_header(data: &[u8]) -> &[u8] {
    memchr(b'\n', data).map_or(&[], |end| &data[end + 1..])
}

pub fn lines(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        if data.is_empty() {
            return None;
        }
        if let Some(at) = memchr(b'\n', data) {
            let (line, rest) = data.split_at(at);
            data = &rest[1..];
            Some(line)
        } else {
            Some(std::mem::take(&mut data))
        }
    })
}

pub fn empty_buckets<T>(n: usize) -> Vec<Vec<T>> {
    (0..n).map(|_| Vec::new()).collect()
}

/// Move the worker-local buffers into shard order without copying their rows.
pub fn transpose<T>(workers: Vec<Vec<Vec<T>>>, bins: usize) -> Vec<Vec<Vec<T>>> {
    let mut result = empty_buckets(bins);
    for worker in workers {
        for (part, rows) in result.iter_mut().zip(worker) {
            if !rows.is_empty() {
                part.push(rows);
            }
        }
    }
    result
}

pub fn partition<T: Send>(
    input: &Input,
    bins: usize,
    parse: impl Fn(&[u8]) -> Option<(usize, T)> + Sync,
) -> Vec<Vec<Vec<T>>> {
    let chunks = input.chunks();
    let next = AtomicUsize::new(0);
    let workers = rayon::current_num_threads().min(chunks.len());
    let buffers = (0..workers)
        .into_par_iter()
        .map(|_| {
            let mut parts = empty_buckets(bins);
            while let Some(chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                for line in lines(chunk.body()) {
                    if let Some((bin, row)) = parse(line) {
                        parts[bin].push(row);
                    }
                }
            }
            parts
        })
        .collect();
    transpose(buffers, bins)
}

pub fn dimension(path: &Path) -> Result<Vec<(String, String)>> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut rows = Vec::new();
    for line in lines(after_header(&bytes)) {
        let [key, value] = crate::parse::fields(line).context("malformed lookup row")?;
        ensure!(
            !key.is_empty() && !value.is_empty(),
            "empty lookup in {}",
            path.display()
        );
        rows.push((
            std::str::from_utf8(key)?.to_owned(),
            std::str::from_utf8(value)?.to_owned(),
        ));
    }
    Ok(rows)
}
