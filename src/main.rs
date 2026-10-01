use bindiff::{compare_files, hash_file, Mode, Options, Range};
use clap::{Parser, ValueEnum};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

#[derive(Clone, Copy, ValueEnum)]
enum IoMode {
    Mmap,
    Read,
}

#[derive(Parser)]
#[command(version, about = "Compare two large binary files and list the differing ranges")]
struct Cli {
    a: PathBuf,
    b: PathBuf,
    /// How to read the files
    #[arg(long, value_enum, default_value = "read")]
    io: IoMode,
    /// Chunk size per worker task, in MiB
    #[arg(long, default_value_t = 8)]
    chunk_mib: usize,
    /// Worker threads (default: number of CPUs)
    #[arg(short = 'j', long, default_value_t = 0)]
    threads: usize,
    /// Join differences that are this many bytes apart or fewer into one range
    #[arg(long, default_value_t = 0)]
    merge_gap: u64,
    /// List at most this many ranges
    #[arg(long, default_value_t = 20)]
    max_ranges: usize,
    /// Show this many bytes from each file at the start of every listed range (0 to hide)
    #[arg(long, default_value_t = 8)]
    context: usize,
    /// Print nothing and stop at the first difference; the exit code says the result
    #[arg(short, long)]
    quiet: bool,
    /// Compare BLAKE3 digests only: no ranges, but fastest for a yes or no answer
    #[arg(long, conflicts_with = "quiet")]
    hash: bool,
    /// Print timing and throughput
    #[arg(long)]
    stats: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("bindiff: {e}");
            ExitCode::from(2)
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn peek(path: &Path, r: Range, n: usize) -> std::io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(r.offset))?;
    let mut buf = vec![0u8; n.min(r.len as usize)];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.2} {}", U[i]) }
}

fn run(cli: Cli) -> Result<bool, String> {
    let ctx = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let started = Instant::now();

    if cli.hash {
        let ha = hash_file(&cli.a).map_err(|e| ctx(&cli.a, e))?;
        let hb = hash_file(&cli.b).map_err(|e| ctx(&cli.b, e))?;
        println!("{ha}  {}\n{hb}  {}", cli.a.display(), cli.b.display());
        println!("{}", if ha == hb { "identical" } else { "different" });
        return Ok(ha == hb);
    }

    let opts = Options {
        mode: match cli.io {
            IoMode::Mmap => Mode::Mmap,
            IoMode::Read => Mode::Read,
        },
        chunk_size: cli.chunk_mib.max(1) << 20,
        threads: cli.threads,
        merge_gap: cli.merge_gap,
        stop_at_first: cli.quiet,
    };
    let rep = compare_files(&cli.a, &cli.b, &opts).map_err(|e| format!("{}: {e}", cli.a.display()))?;
    if cli.quiet {
        return Ok(rep.identical());
    }
    let elapsed = started.elapsed();

    println!("{}  {}", human(rep.len_a), cli.a.display());
    println!("{}  {}", human(rep.len_b), cli.b.display());
    if rep.identical() {
        println!("identical");
    } else {
        if let Some(t) = rep.tail() {
            let longer = if rep.len_a > rep.len_b { &cli.a } else { &cli.b };
            println!("sizes differ: {} has {} extra bytes from offset {:#x}", longer.display(), t.len, t.offset);
        }
        let pct = if rep.common() > 0 { rep.differing_bytes as f64 * 100.0 / rep.common() as f64 } else { 0.0 };
        let share = if pct > 0.0 && pct < 0.0001 { "<0.0001%".to_string() } else { format!("{pct:.4}%") };
        println!(
            "{} differing bytes in {} ranges within the first {} ({share})",
            rep.differing_bytes,
            rep.ranges.len(),
            human(rep.common())
        );
        for r in rep.ranges.iter().take(cli.max_ranges) {
            print!("  {:#012x}  len {}", r.offset, r.len);
            if cli.context > 0 {
                let (x, y) = (
                    peek(&cli.a, *r, cli.context).map_err(|e| ctx(&cli.a, e))?,
                    peek(&cli.b, *r, cli.context).map_err(|e| ctx(&cli.b, e))?,
                );
                print!("   a: {}   b: {}", hex(&x), hex(&y));
            }
            println!();
        }
        if rep.ranges.len() > cli.max_ranges {
            println!("  ... {} more ranges", rep.ranges.len() - cli.max_ranges);
        }
    }
    if cli.stats {
        let bytes = rep.common() * 2;
        let secs = elapsed.as_secs_f64().max(1e-9);
        eprintln!("{:.3} s, {:.2} GB/s over both files", secs, bytes as f64 / secs / 1e9);
    }
    Ok(rep.identical())
}
