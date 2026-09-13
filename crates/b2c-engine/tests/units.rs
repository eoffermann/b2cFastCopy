//! Unit coverage for the small pure helpers.

#![cfg(windows)]

use b2c_engine::device::round_up;
use b2c_engine::fmt::{bytes, parse_size, rate};
use b2c_engine::scanner::glob_match;
use b2c_engine::stats::Ewma;

#[test]
fn round_up_lands_on_alignment() {
    assert_eq!(round_up(0, 4096), 0);
    assert_eq!(round_up(1, 4096), 4096);
    assert_eq!(round_up(4096, 4096), 4096);
    assert_eq!(round_up(4097, 4096), 8192);
    // H: uses 64 KiB clusters, so this is the case a global 4 KiB constant
    // would silently get wrong.
    assert_eq!(round_up(65537, 65536), 131072);
    assert_eq!(
        round_up(12345, 0),
        12345,
        "zero alignment must not divide by zero"
    );
}

#[test]
fn glob_matches_expected_shapes() {
    assert!(glob_match("*.tmp", "scratch.tmp"));
    assert!(
        glob_match("*.TMP", "scratch.tmp"),
        "matching is case-insensitive"
    );
    assert!(!glob_match("*.tmp", "scratch.tmpx"));
    assert!(glob_match("*", "anything"));
    assert!(glob_match("a?c", "abc"));
    assert!(!glob_match("a?c", "ac"));
    assert!(glob_match("*cache*", "my-cache-dir"));
    assert!(!glob_match("cache", "my-cache-dir"));
}

#[test]
fn parses_size_suffixes() {
    assert_eq!(parse_size("1024").unwrap(), 1024);
    assert_eq!(parse_size("4K").unwrap(), 4096);
    assert_eq!(parse_size("8M").unwrap(), 8 << 20);
    assert_eq!(parse_size("16G").unwrap(), 16 << 30);
    assert_eq!(
        parse_size("1.5G").unwrap(),
        (1.5 * (1u64 << 30) as f64) as u64
    );
    assert!(parse_size("").is_err());
    assert!(parse_size("banana").is_err());
}

#[test]
fn formats_sizes_and_rates() {
    assert_eq!(bytes(512), "512 B");
    assert_eq!(bytes(1024), "1.00 KiB");
    assert_eq!(bytes(1536), "1.50 KiB");
    assert!(rate(0.0).starts_with('0'));
    assert_eq!(rate(1_500_000_000.0), "1.50 GB/s");
}

#[test]
fn ewma_tracks_a_step_change_at_its_own_pace() {
    // The cliff detector depends on a short window reacting while a long one
    // still remembers the previous rate. If both converge together, a cache
    // cliff becomes invisible.
    let mut short = Ewma::new(2.0);
    let mut long = Ewma::new(30.0);

    for _ in 0..20 {
        short.update(1000.0, 0.1);
        long.update(1000.0, 0.1);
    }
    assert!((short.get() - 1000.0).abs() < 1.0);

    // Rate collapses to a quarter, as an exhausted SLC cache would do.
    for _ in 0..20 {
        short.update(250.0, 0.1);
        long.update(250.0, 0.1);
    }

    assert!(
        short.get() < long.get() * 0.9,
        "short window ({}) should fall well below long ({})",
        short.get(),
        long.get()
    );
}
