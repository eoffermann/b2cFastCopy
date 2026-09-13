//! End-to-end copy correctness.
//!
//! The sizes here are chosen to land on and around sector and block boundaries,
//! because the unbuffered write path rounds the final chunk *up* to a whole
//! sector and relies on `set_len` to trim it back. If that trim regresses, the
//! copy still "succeeds" and the corruption is invisible until something reads
//! the file — so these assertions check exact length and exact bytes, not just
//! that the call returned Ok.

#![cfg(windows)]

use b2c_engine::{Copier, Options};
use std::path::{Path, PathBuf};

const BLOCK: usize = 64 * 1024;

/// Sizes straddling every boundary that matters: empty, sub-sector, exact
/// sector, sector+1, exact block, block+1, and a large awkward remainder.
const SIZES: &[u64] = &[
    0, 1, 511, 512, 4095, 4096, 4097, 65535, 65536, 65537, 1_000_003, 12_345_678,
];

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let mut p = std::env::temp_dir();
        let unique = format!(
            "b2fc-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(unique);
        std::fs::create_dir_all(&p).expect("create temp dir");
        TempDir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Deterministic, non-repeating content so a misplaced or duplicated chunk
/// cannot pass by coincidence.
fn pattern(size: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(size as usize);
    let mut state = 0x243F_6A88_85A3_08D3u64 ^ size;
    for _ in 0..size {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.push((state >> 33) as u8);
    }
    v
}

fn options() -> Options {
    Options {
        // Force everything through the bulk pipeline rather than the
        // small-file path, which is what we are actually testing.
        small_threshold: 0,
        block_size: Some(BLOCK),
        arena_bytes: Some(8 << 20),
        workers: Some(4),
        ..Default::default()
    }
}

#[test]
fn copies_boundary_sizes_byte_exact() {
    let src = TempDir::new("src");
    let dst = TempDir::new("dst");

    let mut expected = Vec::new();
    for &size in SIZES {
        let name = format!("f{size}.bin");
        let data = pattern(size);
        std::fs::write(src.path().join(&name), &data).expect("write source");
        expected.push((name, data));
    }

    let copier = Copier::new(src.path(), dst.path(), options()).expect("build copier");
    let outcome = copier.run().expect("run copy");
    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);

    for (name, data) in expected {
        let got = std::fs::read(dst.path().join(&name)).expect("read destination");
        assert_eq!(
            got.len(),
            data.len(),
            "{name}: length differs — the unbuffered tail was not trimmed back"
        );
        assert_eq!(got, data, "{name}: contents differ");
    }
}

#[test]
fn copies_nested_directory_tree() {
    let src = TempDir::new("tree-src");
    let dst = TempDir::new("tree-dst");

    let deep = src.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&deep).expect("create nested dirs");
    std::fs::write(deep.join("deep.bin"), pattern(70_000)).expect("write nested file");
    std::fs::write(src.path().join("top.bin"), pattern(1234)).expect("write top file");
    std::fs::create_dir_all(src.path().join("empty")).expect("create empty dir");

    let copier = Copier::new(src.path(), dst.path(), options()).expect("build copier");
    let outcome = copier.run().expect("run copy");
    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);

    assert_eq!(
        std::fs::read(dst.path().join("a").join("b").join("c").join("deep.bin")).unwrap(),
        pattern(70_000)
    );
    assert_eq!(
        std::fs::read(dst.path().join("top.bin")).unwrap(),
        pattern(1234)
    );
    assert!(
        dst.path().join("empty").is_dir(),
        "empty directories should be recreated"
    );
}

#[test]
fn honours_exclude_globs() {
    let src = TempDir::new("excl-src");
    let dst = TempDir::new("excl-dst");

    std::fs::write(src.path().join("keep.bin"), pattern(5000)).unwrap();
    std::fs::write(src.path().join("drop.tmp"), pattern(5000)).unwrap();

    let opts = Options {
        excludes: vec!["*.tmp".into()],
        ..options()
    };
    let copier = Copier::new(src.path(), dst.path(), opts).expect("build copier");
    copier.run().expect("run copy");

    assert!(dst.path().join("keep.bin").exists());
    assert!(
        !dst.path().join("drop.tmp").exists(),
        "excluded file was copied"
    );
}

#[test]
fn dry_run_moves_nothing() {
    let src = TempDir::new("dry-src");
    let dst = TempDir::new("dry-dst");
    std::fs::write(src.path().join("a.bin"), pattern(9999)).unwrap();

    let opts = Options {
        dry_run: true,
        ..options()
    };
    let copier = Copier::new(src.path(), dst.path(), opts).expect("build copier");
    let outcome = copier.run().expect("run dry copy");

    assert_eq!(outcome.bytes, 9999);
    assert!(!dst.path().join("a.bin").exists(), "dry run wrote a file");
}

#[test]
fn verify_pass_accepts_a_good_copy() {
    let src = TempDir::new("ver-src");
    let dst = TempDir::new("ver-dst");
    for &size in &[0u64, 4096, 100_001] {
        std::fs::write(src.path().join(format!("v{size}.bin")), pattern(size)).unwrap();
    }

    let opts = Options {
        verify: true,
        ..options()
    };
    let copier = Copier::new(src.path(), dst.path(), opts).expect("build copier");
    let outcome = copier.run().expect("run copy");

    assert!(
        outcome.errors.is_empty(),
        "verify reported: {:?}",
        outcome.errors
    );
    assert_eq!(outcome.verified, 3);
}

#[test]
fn copies_to_a_destination_containing_a_dot_component() {
    // `b2fc src G:\.` is an ordinary thing to type. It used to copy only the
    // small files: they go through the platform copy, which normalises the
    // path, while every bulk destination open got the \\?\ prefix applied to a
    // literal "." component and failed with ERROR_PATH_NOT_FOUND.
    let src = TempDir::new("dot-src");
    let dst = TempDir::new("dot-dst");
    std::fs::write(src.path().join("big.bin"), pattern(200_000)).unwrap();
    std::fs::write(src.path().join("small.bin"), pattern(100)).unwrap();

    let dotted = dst.path().join(".");
    let copier = Copier::new(src.path(), &dotted, options()).expect("build copier");
    let outcome = copier.run().expect("run copy");

    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
    assert_eq!(
        outcome.files, outcome.files_expected,
        "every planned file must copy"
    );
    assert_eq!(
        std::fs::read(dst.path().join("big.bin")).unwrap(),
        pattern(200_000)
    );
    assert_eq!(
        std::fs::read(dst.path().join("small.bin")).unwrap(),
        pattern(100)
    );
}

#[test]
fn copies_with_forward_slash_separators() {
    // Same failure mode, different trigger: the prefix also disables the
    // forward-slash translation that the rest of Win32 performs.
    let src = TempDir::new("slash-src");
    let dst = TempDir::new("slash-dst");
    std::fs::write(src.path().join("big.bin"), pattern(200_000)).unwrap();

    let slashed = PathBuf::from(src.path().to_string_lossy().replace('\\', "/"));
    let copier = Copier::new(&slashed, dst.path(), options()).expect("build copier");
    let outcome = copier.run().expect("run copy");

    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);
    assert_eq!(outcome.files, outcome.files_expected);
    assert_eq!(
        std::fs::read(dst.path().join("big.bin")).unwrap(),
        pattern(200_000)
    );
}

#[test]
fn buffered_fallback_produces_identical_output() {
    // --no-unbuffered exists for filesystems that refuse FILE_FLAG_NO_BUFFERING.
    // It takes a different code path with alignment padding switched off, so it
    // needs its own proof that awkward sizes still come out byte-exact.
    const CASES: &[u64] = &[0, 1, 4095, 65537, 1_000_003];

    let src = TempDir::new("buf-src");
    let dst = TempDir::new("buf-dst");
    for &size in CASES {
        std::fs::write(src.path().join(format!("b{size}.bin")), pattern(size)).unwrap();
    }

    let opts = Options {
        unbuffered: false,
        ..options()
    };
    let copier = Copier::new(src.path(), dst.path(), opts).expect("build copier");
    let outcome = copier.run().expect("run copy");
    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);

    for &size in CASES {
        let got = std::fs::read(dst.path().join(format!("b{size}.bin"))).unwrap();
        assert_eq!(got.len() as u64, size, "b{size}.bin length differs");
        assert_eq!(got, pattern(size), "b{size}.bin contents differ");
    }
}
