use bindiff::{compare_files, hash_file, Mode, Options, Range};
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// Deterministic pseudo-random bytes (xorshift), so a one-byte change is never a coincidence.
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn write(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.path().join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn opts(mode: Mode, chunk: usize, threads: usize) -> Options {
    Options { mode, chunk_size: chunk, threads, ..Options::default() }
}

fn r(offset: u64, len: u64) -> Range {
    Range { offset, len }
}

const MODES: [Mode; 2] = [Mode::Mmap, Mode::Read];

#[test]
fn identical_files_in_both_modes() {
    let d = tempfile::tempdir().unwrap();
    let a = data(100_000, 1);
    let (pa, pb) = (write(&d, "a", &a), write(&d, "b", &a));
    for m in MODES {
        let rep = compare_files(&pa, &pb, &opts(m, 4096, 4)).unwrap();
        assert!(rep.identical() && rep.complete && rep.differing_bytes == 0);
    }
}

#[test]
fn empty_files() {
    let d = tempfile::tempdir().unwrap();
    let (pa, pb) = (write(&d, "a", b""), write(&d, "b", b""));
    for m in MODES {
        assert!(compare_files(&pa, &pb, &opts(m, 16, 2)).unwrap().identical());
    }
    let pc = write(&d, "c", b"xyz");
    let rep = compare_files(&pa, &pc, &opts(Mode::Mmap, 16, 2)).unwrap();
    assert_eq!(rep.tail(), Some(r(0, 3)));
    assert!(rep.ranges.is_empty() && !rep.identical());
}

#[test]
fn differences_across_chunk_borders_are_merged_and_exact() {
    let d = tempfile::tempdir().unwrap();
    let a = data(10_000, 2);
    let mut b = a.clone();
    // Chunk size 1000: this run straddles the border at 3000.
    for x in &mut b[2995..3005] {
        *x ^= 0xff;
    }
    b[0] ^= 1;
    b[9_999] ^= 1;
    let (pa, pb) = (write(&d, "a", &a), write(&d, "b", &b));
    for m in MODES {
        for threads in [1, 3, 8] {
            let rep = compare_files(&pa, &pb, &opts(m, 1000, threads)).unwrap();
            assert_eq!(rep.ranges, [r(0, 1), r(2995, 10), r(9_999, 1)], "{m:?} x{threads}");
            assert_eq!(rep.differing_bytes, 12);
        }
    }
}

#[test]
fn merge_gap_joins_nearby_differences() {
    let d = tempfile::tempdir().unwrap();
    let a = data(5_000, 3);
    let mut b = a.clone();
    b[100] ^= 1;
    b[110] ^= 1;
    let (pa, pb) = (write(&d, "a", &a), write(&d, "b", &b));
    let mut o = opts(Mode::Mmap, 256, 2);
    assert_eq!(compare_files(&pa, &pb, &o).unwrap().ranges, [r(100, 1), r(110, 1)]);
    o.merge_gap = 9;
    assert_eq!(compare_files(&pa, &pb, &o).unwrap().ranges, [r(100, 11)]);
}

#[test]
fn different_lengths_compare_the_common_prefix_and_report_the_tail() {
    let d = tempfile::tempdir().unwrap();
    let a = data(3_000, 4);
    let mut b = a.clone();
    b.extend_from_slice(&[1, 2, 3, 4, 5]);
    b[1500] ^= 0x10;
    let (pa, pb) = (write(&d, "a", &a), write(&d, "b", &b));
    for m in MODES {
        let rep = compare_files(&pa, &pb, &opts(m, 512, 4)).unwrap();
        assert_eq!(rep.ranges, [r(1500, 1)]);
        assert_eq!(rep.tail(), Some(r(3000, 5)));
        assert_eq!((rep.len_a, rep.len_b), (3000, 3005));
    }
}

#[test]
fn stop_at_first_marks_the_report_incomplete() {
    let d = tempfile::tempdir().unwrap();
    let a = data(200_000, 5);
    let mut b = a.clone();
    b[10] ^= 1;
    let (pa, pb) = (write(&d, "a", &a), write(&d, "b", &b));
    let mut o = opts(Mode::Read, 1024, 1);
    o.stop_at_first = true;
    let rep = compare_files(&pa, &pb, &o).unwrap();
    assert!(!rep.complete && !rep.identical());
    assert_eq!(rep.ranges.first(), Some(&r(10, 1)));
}

#[test]
fn missing_file_is_an_error() {
    let d = tempfile::tempdir().unwrap();
    let pa = write(&d, "a", b"x");
    assert!(compare_files(&pa, &d.path().join("nope"), &Options::default()).is_err());
}

#[test]
fn hash_agrees_with_content() {
    let d = tempfile::tempdir().unwrap();
    let a = data(50_000, 6);
    let mut b = a.clone();
    let (pa, pb, pc) = (write(&d, "a", &a), write(&d, "b", &a), {
        b[49_999] ^= 1;
        write(&d, "c", &b)
    });
    assert_eq!(hash_file(&pa).unwrap(), hash_file(&pb).unwrap());
    assert_ne!(hash_file(&pa).unwrap(), hash_file(&pc).unwrap());
}

#[test]
fn cli_exit_codes_and_output() {
    let d = tempfile::tempdir().unwrap();
    let a = data(20_000, 7);
    let mut b = a.clone();
    b[12_345] ^= 0xff;
    let (pa, pb, pc) = (write(&d, "a", &a), write(&d, "b", &b), write(&d, "c", &a));
    let bin = env!("CARGO_BIN_EXE_bindiff");

    let same = Command::new(bin).arg(&pa).arg(&pc).output().unwrap();
    assert_eq!(same.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&same.stdout).contains("identical"));

    let diff = Command::new(bin).arg(&pa).arg(&pb).output().unwrap();
    assert_eq!(diff.status.code(), Some(1));
    let text = String::from_utf8_lossy(&diff.stdout);
    assert!(text.contains("1 differing bytes in 1 ranges"), "{text}");
    assert!(text.contains("0x0000003039"), "{text}"); // 12345

    let quiet = Command::new(bin).arg("-q").arg(&pa).arg(&pb).output().unwrap();
    assert_eq!(quiet.status.code(), Some(1));
    assert!(quiet.stdout.is_empty());

    let hashed = Command::new(bin).arg("--hash").arg(&pa).arg(&pb).output().unwrap();
    assert_eq!(hashed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&hashed.stdout).contains("different"));

    let missing = Command::new(bin).arg(&pa).arg(d.path().join("none")).output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
}
