use anyhow::{Context, Result, ensure};
use memchr::memchr;
use rayon::prelude::*;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const BLOCK: usize = 1 << 20;
const LOOKAHEAD: usize = 256;

struct Source {
    path: PathBuf,
    len: usize,
}

pub struct Input {
    files: Vec<Source>,
}

impl Input {
    pub fn open(dir: &Path) -> Result<Self> {
        let mut files = Vec::new();
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "csv") {
                let metadata = fs::metadata(&path)?;
                if metadata.is_file() && metadata.len() != 0 {
                    files.push(Source {
                        path,
                        len: usize::try_from(metadata.len())?,
                    });
                }
            }
        }
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Ok(Self { files })
    }

    pub fn bytes(&self) -> usize {
        self.files.iter().map(|f| f.len).sum()
    }

    pub fn chunks(&self) -> Vec<Chunk<'_>> {
        let mut chunks = Vec::new();
        for (index, file) in self.files.iter().enumerate() {
            for start in (0..file.len).step_by(BLOCK) {
                chunks.push(Chunk {
                    file,
                    index,
                    start,
                    end: (start + BLOCK).min(file.len),
                });
            }
        }
        chunks
    }
}

#[derive(Clone, Copy)]
pub struct Chunk<'a> {
    file: &'a Source,
    index: usize,
    start: usize,
    end: usize,
}

#[derive(Default)]
pub struct Reader {
    open: Option<(usize, File)>,
    bytes: Vec<u8>,
}

impl Reader {
    /// A block owns rows starting inside it. Read one preceding byte and a
    /// small lookahead so most boundaries are resolved in a single pread.
    pub fn body(&mut self, chunk: Chunk<'_>) -> Result<&[u8]> {
        let source = chunk.file;
        if self.open.as_ref().map(|(i, _)| *i) != Some(chunk.index) {
            self.open = Some((
                chunk.index,
                File::open(&source.path)
                    .with_context(|| format!("opening {}", source.path.display()))?,
            ));
        }
        let file = &self.open.as_ref().unwrap().1;
        let base = chunk.start.saturating_sub(1);
        let boundary = chunk.end - base;
        let mut filled = (source.len - base).min(boundary + LOOKAHEAD);
        if self.bytes.len() < filled {
            self.bytes.resize(filled, 0);
        }
        file.read_exact_at(&mut self.bytes[..filled], base as u64)?;
        let mut start = chunk.start - base;
        if chunk.start == 0 || self.bytes[0] != b'\n' {
            let Some(lf) = memchr(b'\n', &self.bytes[start..boundary]) else {
                return Ok(&[]);
            };
            start += lf + 1;
        }
        if start == boundary || chunk.end == source.len || self.bytes[boundary - 1] == b'\n' {
            return Ok(&self.bytes[start..boundary]);
        }
        let mut search = boundary;
        loop {
            if let Some(lf) = memchr(b'\n', &self.bytes[search..filled]) {
                return Ok(&self.bytes[start..search + lf + 1]);
            }
            if base + filled == source.len {
                return Ok(&self.bytes[start..filled]);
            }
            search = filled;
            filled = (filled + BLOCK).min(source.len - base);
            if self.bytes.len() < filled {
                self.bytes.resize(filled, 0);
            }
            file.read_exact_at(&mut self.bytes[search..filled], (base + search) as u64)?;
        }
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

// Each shard contains one row buffer per contributing worker.
pub type Shards<T> = Vec<Vec<Vec<T>>>;

/// Move the worker-local buffers into shard order without copying their rows.
pub fn transpose<B>(
    workers: Vec<Vec<B>>,
    bins: usize,
    is_empty: impl Fn(&B) -> bool,
) -> Vec<Vec<B>> {
    let mut result = empty_buckets(bins);
    for worker in workers {
        for (part, rows) in result.iter_mut().zip(worker) {
            if !is_empty(&rows) {
                part.push(rows);
            }
        }
    }
    result
}

pub trait Bucket<T>: Default + Send {
    fn push(&mut self, row: T);
    fn is_empty(&self) -> bool;
}

impl<T: Send> Bucket<T> for Vec<T> {
    fn push(&mut self, row: T) {
        Vec::push(self, row);
    }
    fn is_empty(&self) -> bool {
        Vec::is_empty(self)
    }
}

pub fn partition<const N: usize, T, B: Bucket<T>, F>(
    input: &Input,
    bins: usize,
    make_parser: impl Fn() -> F + Sync,
) -> Result<Vec<Vec<B>>>
where
    F: FnMut([&[u8]; N]) -> Option<(usize, T)>,
{
    let chunks = input.chunks();
    let next = AtomicUsize::new(0);
    let workers = rayon::current_num_threads().min(chunks.len());
    let buffers = (0..workers)
        .into_par_iter()
        .map(|_| {
            let mut parts: Vec<B> = (0..bins).map(|_| B::default()).collect();
            let mut reader = Reader::default();
            let mut parse = make_parser();
            while let Some(chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                crate::csv::records(reader.body(*chunk)?, |fields| {
                    if let Some((bin, row)) = parse(fields) {
                        parts[bin].push(row);
                    }
                });
            }
            Ok(parts)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(transpose(buffers, bins, B::is_empty))
}

pub fn dimension(data: &[u8]) -> Result<Vec<(&str, &str)>> {
    let mut rows = Vec::new();
    for line in lines(after_header(data)) {
        let [key, value] = crate::parse::fields(line).context("malformed lookup row")?;
        ensure!(!key.is_empty() && !value.is_empty(), "empty lookup field");
        rows.push((std::str::from_utf8(key)?, std::str::from_utf8(value)?));
    }
    Ok(rows)
}
