# Binary diff tool

`bindiff` compares two very large files in parallel and lists exactly which byte ranges differ, with the bytes from each file at the start of every range. For anyone verifying disk images, build outputs, backups or downloads.

**Status:** v0.1.0, working. Benchmarked on 4 GiB files against GNU `cmp`.

## Features

- Parallel comparison: the file is cut into chunks, worker threads compare them 4 KiB at a time with `memcmp` and only scan byte by byte inside blocks that differ.
- Reports differing ranges (offset, length, bytes from both files), the count of differing bytes, and a length mismatch as an extra tail.
- `--merge-gap N` joins differences that are close together into one range.
- Two I/O modes: positioned reads (default) or memory mapping.
- `--hash` compares BLAKE3 digests for a yes or no answer, and `--quiet` stops at the first difference.
- Exit status for scripts: 0 identical, 1 different, 2 error.

## How to install

Needs a Rust toolchain.

```sh
git clone https://github.com/r3clusionn/binary-diff-tool
cd binary-diff-tool
cargo install --path .
```

## How to use

```sh
bindiff a.img b.img
bindiff --merge-gap 64 --max-ranges 50 a.bin b.bin
bindiff -q a.bin b.bin && echo same
bindiff --hash a.bin b.bin
```

Example output, comparing two 1 MiB files that differ in four bytes:

```text
1.00 MiB  a.bin
1.00 MiB  b.bin
4 differing bytes in 3 ranges within the first 1.00 MiB (0.0004%)
  0x0000001000  len 2   a: a0 29   b: 5f 09
  0x00000aae60  len 1   a: 00   b: 01
  0x00000fffff  len 1   a: 4f   b: cf
```

| Option | What it does |
|---|---|
| `--io read\|mmap` | Positioned reads (default) or memory-mapped files. |
| `--chunk-mib N` | Chunk size per worker task (default 8). |
| `-j N` | Worker threads (default: number of CPUs). |
| `--merge-gap N` | Join differences at most N bytes apart. |
| `--max-ranges N`, `--context N` | How many ranges to list, and how many bytes to show for each. |
| `-q` | No output; stop at the first difference. |
| `--hash` | Compare BLAKE3 digests only. |
| `--stats` | Print time and throughput to stderr. |

## Benchmarks

Wall-clock time for the whole command, output sent to `NUL`. Windows 11, Intel Core i9-14900KF (24
threads), 32 GB RAM, two 4 GiB files on an NVMe SSD, both fully in the file cache (warm: a first
run reads them, the timed runs do not touch the disk). Median of 5 runs. GB/s counts the bytes of
both files. GNU `cmp` 3.12 (diffutils) is the one shipped with Git for Windows. Reproduce with
`scripts/bench.ps1`.

| Tool | identical files | one byte differs 1 MiB from the end |
|---|---|---|
| `bindiff` (read, default) | 0.71 s, 12.0 GB/s | 0.71 s, 12.1 GB/s |
| `bindiff --io mmap` | 0.98 s, 8.8 GB/s | 1.01 s, 8.6 GB/s |
| `bindiff -j 1` (read) | 1.21 s, 7.1 GB/s | 1.21 s, 7.1 GB/s |
| `bindiff --hash` (BLAKE3) | 1.00 s, 8.6 GB/s | 1.00 s, 8.6 GB/s |
| GNU `cmp` | 1.15 s, 7.5 GB/s | 1.15 s, 7.5 GB/s |

On this machine the default is about 1.6 times faster than `cmp` and 1.7 times faster than the same
code on one thread, which runs at roughly `cmp` speed. Both files were in RAM, so the comparison is
limited by memory bandwidth and more threads stop helping early; with files that must come from
disk the gap would shrink or vanish. Memory mapping was slower than positioned reads here, which
is why reads are the default. Linux and macOS were not measured.

## How it works

Chunks go to threads through an atomic counter. Each chunk produces a list of ranges; the lists are
sorted back into file order and merged, so a difference that straddles a chunk border is one
range. A length mismatch compares only the common prefix and reports the longer file's extra bytes.
`--quiet` sets a shared flag at the first difference and workers stop taking chunks.

## Tests

`cargo test` runs 13 tests, including random-looking data with one flipped byte, differences that
straddle chunk borders at 1, 3 and 8 threads in both I/O modes, length mismatches, empty files,
early exit, hash agreement, and the binary's exit codes.

## License

MIT (see `LICENSE`).
