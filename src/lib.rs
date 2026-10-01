//! Parallel comparison of two large files.
//!
//! The common length is cut into chunks. Worker threads take chunks from an atomic counter,
//! compare them 4 KiB at a time with `memcmp` (slice equality), and only scan byte by byte inside
//! blocks that differ. Per-chunk range lists are stitched back together in file order and ranges
//! that touch, or sit within `merge_gap` bytes of each other, are merged.
//!
//! Two I/O modes: positioned reads into per-thread buffers (the default, faster in the benchmark), or memory-mapped. A length mismatch is
//! reported as a tail; only the common prefix is compared byte by byte.

use memmap2::Mmap;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

const BLOCK: usize = 4096;

/// A run of differing bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub offset: u64,
    pub len: u64,
}

impl Range {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Mmap,
    Read,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub mode: Mode,
    pub chunk_size: usize,
    /// 0 means one per CPU.
    pub threads: usize,
    /// Differences closer together than this many bytes are reported as one range.
    pub merge_gap: u64,
    /// Stop as soon as any difference is found. The report then lists only what was seen.
    pub stop_at_first: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            mode: Mode::Read,
            chunk_size: 8 << 20,
            threads: 0,
            merge_gap: 0,
            stop_at_first: false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub len_a: u64,
    pub len_b: u64,
    /// Differing ranges inside the common prefix, in file order.
    pub ranges: Vec<Range>,
    /// Bytes that differ inside the common prefix (counted before range merging).
    pub differing_bytes: u64,
    /// False when `stop_at_first` ended the scan early.
    pub complete: bool,
}

impl Report {
    pub fn common(&self) -> u64 {
        self.len_a.min(self.len_b)
    }

    pub fn identical(&self) -> bool {
        self.len_a == self.len_b && self.ranges.is_empty()
    }

    /// The bytes past the end of the shorter file, if the lengths differ.
    pub fn tail(&self) -> Option<Range> {
        (self.len_a != self.len_b).then(|| Range {
            offset: self.common(),
            len: self.len_a.abs_diff(self.len_b),
        })
    }
}

/// Appends the ranges where `a` and `b` differ. Both slices must have the same length.
pub fn diff_slices(a: &[u8], b: &[u8], base: u64, out: &mut Vec<Range>) -> u64 {
    assert_eq!(a.len(), b.len());
    let mut differing = 0u64;
    let mut open: Option<u64> = None;
    let mut pos = 0usize;
    while pos < a.len() {
        let end = (pos + BLOCK).min(a.len());
        if a[pos..end] == b[pos..end] {
            if let Some(start) = open.take() {
                out.push(Range { offset: start, len: base + pos as u64 - start });
            }
        } else {
            for i in pos..end {
                if a[i] != b[i] {
                    differing += 1;
                    open.get_or_insert(base + i as u64);
                } else if let Some(start) = open.take() {
                    out.push(Range { offset: start, len: base + i as u64 - start });
                }
            }
        }
        pos = end;
    }
    if let Some(start) = open {
        out.push(Range { offset: start, len: base + a.len() as u64 - start });
    }
    differing
}

/// Merges sorted ranges whose gap is at most `gap` bytes.
pub fn merge_ranges(ranges: Vec<Range>, gap: u64) -> Vec<Range> {
    let mut out: Vec<Range> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.offset <= last.end().saturating_add(gap) => {
                last.len = r.end().max(last.end()) - last.offset;
            }
            _ => out.push(r),
        }
    }
    out
}

enum Source {
    Mapped(Mmap),
    File(File),
    Empty,
}

impl Source {
    fn open(path: &Path, mode: Mode, len: u64) -> io::Result<Source> {
        let file = File::open(path)?;
        if len == 0 {
            return Ok(Source::Empty);
        }
        match mode {
            // Safety: the map is read-only and the tool does not write. A file truncated by
            // another process during the run can fault; this is the documented mmap hazard.
            Mode::Mmap => Ok(Source::Mapped(unsafe { Mmap::map(&file)? })),
            Mode::Read => Ok(Source::File(file)),
        }
    }

    /// Fills `buf` from `offset`, borrowing straight from the map when possible.
    fn read<'a>(&'a self, offset: u64, len: usize, buf: &'a mut Vec<u8>) -> io::Result<&'a [u8]> {
        match self {
            Source::Empty => Ok(&[]),
            Source::Mapped(m) => Ok(&m[offset as usize..offset as usize + len]),
            Source::File(f) => {
                buf.resize(len, 0);
                read_exact_at(f, buf, offset)?;
                Ok(&buf[..])
            }
        }
    }
}

#[cfg(windows)]
fn read_exact_at(f: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_read(buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, offset)
}

pub fn compare_files(a: &Path, b: &Path, opts: &Options) -> io::Result<Report> {
    let len_a = std::fs::metadata(a)?.len();
    let len_b = std::fs::metadata(b)?.len();
    let common = len_a.min(len_b);
    let src_a = Source::open(a, opts.mode, len_a)?;
    let src_b = Source::open(b, opts.mode, len_b)?;
    let chunk = opts.chunk_size.max(1) as u64;
    let nchunks = common.div_ceil(chunk) as usize;
    let threads = if opts.threads == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        opts.threads
    }
    .min(nchunks.max(1));

    let next = AtomicUsize::new(0);
    let found = AtomicBool::new(false);
    let failure: Mutex<Option<io::Error>> = Mutex::new(None);
    let parts: Mutex<Vec<(usize, Vec<Range>, u64)>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let (mut buf_a, mut buf_b) = (Vec::new(), Vec::new());
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= nchunks || (opts.stop_at_first && found.load(Ordering::Relaxed)) {
                        return;
                    }
                    let offset = idx as u64 * chunk;
                    let len = chunk.min(common - offset) as usize;
                    let res = src_a.read(offset, len, &mut buf_a).and_then(|x| {
                        let y = src_b.read(offset, len, &mut buf_b)?;
                        let mut ranges = Vec::new();
                        let n = diff_slices(x, y, offset, &mut ranges);
                        Ok((ranges, n))
                    });
                    match res {
                        Ok((ranges, n)) => {
                            if !ranges.is_empty() {
                                found.store(true, Ordering::Relaxed);
                                parts.lock().unwrap().push((idx, ranges, n));
                            }
                        }
                        Err(e) => {
                            *failure.lock().unwrap() = Some(e);
                            found.store(true, Ordering::Relaxed);
                            next.store(usize::MAX / 2, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            });
        }
    });

    if let Some(e) = failure.into_inner().unwrap() {
        return Err(e);
    }
    let mut parts = parts.into_inner().unwrap();
    parts.sort_by_key(|p| p.0);
    let differing_bytes = parts.iter().map(|p| p.2).sum();
    let all: Vec<Range> = parts.into_iter().flat_map(|p| p.1).collect();
    let complete = !(opts.stop_at_first && found.load(Ordering::Relaxed));
    Ok(Report {
        len_a,
        len_b,
        // Adjacent ranges across chunk borders must always join, so gap 0 is applied at minimum.
        ranges: merge_ranges(all, opts.merge_gap),
        differing_bytes,
        complete,
    })
}

/// BLAKE3 of a whole file, memory-mapped and hashed on all cores.
pub fn hash_file(path: &Path) -> io::Result<blake3::Hash> {
    let mut h = blake3::Hasher::new();
    h.update_mmap_rayon(path)?;
    Ok(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(offset: u64, len: u64) -> Range {
        Range { offset, len }
    }

    #[test]
    fn identical_slices_have_no_ranges() {
        let a = vec![7u8; 10_000];
        let mut out = Vec::new();
        assert_eq!(diff_slices(&a, &a.clone(), 100, &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn finds_single_bytes_runs_and_block_borders() {
        let a = vec![0u8; 3 * BLOCK];
        let mut b = a.clone();
        b[0] = 1; // first byte
        b[10..14].fill(1); // a run
        b[BLOCK - 1] = 1; // last byte of block 0 ...
        b[BLOCK] = 1; // ... and first of block 1: one run across the border
        b[3 * BLOCK - 1] = 1; // very last byte
        let mut out = Vec::new();
        let n = diff_slices(&a, &b, 1000, &mut out);
        assert_eq!(n, 1 + 4 + 2 + 1);
        assert_eq!(
            out,
            [r(1000, 1), r(1010, 4), r(1000 + BLOCK as u64 - 1, 2), r(1000 + 3 * BLOCK as u64 - 1, 1)]
        );
    }

    #[test]
    fn merging_respects_the_gap() {
        let v = vec![r(0, 2), r(2, 3), r(10, 1), r(14, 1)];
        assert_eq!(merge_ranges(v.clone(), 0), [r(0, 5), r(10, 1), r(14, 1)]);
        assert_eq!(merge_ranges(v.clone(), 2), [r(0, 5), r(10, 1), r(14, 1)]);
        // Bytes 11..14 sit between the last two ranges: a gap of exactly 3 joins them.
        assert_eq!(merge_ranges(v.clone(), 3), [r(0, 5), r(10, 5)]);
        // Five bytes (5..10) sit between the first merged range and the next.
        assert_eq!(merge_ranges(v, 5), [r(0, 15)]);
        assert!(merge_ranges(Vec::new(), 5).is_empty());
    }

    #[test]
    fn tail_reporting() {
        let rep = Report { len_a: 10, len_b: 25, ranges: vec![], differing_bytes: 0, complete: true };
        assert_eq!(rep.tail(), Some(r(10, 15)));
        assert!(!rep.identical());
        let same = Report { len_a: 5, len_b: 5, ranges: vec![], differing_bytes: 0, complete: true };
        assert!(same.identical() && same.tail().is_none());
    }
}
