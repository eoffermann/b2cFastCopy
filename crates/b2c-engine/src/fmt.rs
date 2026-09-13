//! Human-readable sizes, rates and durations.

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

pub fn rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "0 MB/s".into();
    }
    let mbs = bytes_per_sec / 1_000_000.0;
    if mbs >= 1000.0 {
        format!("{:.2} GB/s", mbs / 1000.0)
    } else if mbs >= 100.0 {
        format!("{mbs:.0} MB/s")
    } else {
        format!("{mbs:.1} MB/s")
    }
}

pub fn duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--".into();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Parse sizes like `16G`, `512M`, `2048`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty size".into());
    }
    let (num, mult) = match t.chars().last().unwrap().to_ascii_uppercase() {
        'K' => (&t[..t.len() - 1], 1u64 << 10),
        'M' => (&t[..t.len() - 1], 1u64 << 20),
        'G' => (&t[..t.len() - 1], 1u64 << 30),
        'T' => (&t[..t.len() - 1], 1u64 << 40),
        _ => (t, 1),
    };
    let n: f64 = num.trim().parse().map_err(|_| format!("bad size: {s}"))?;
    if n < 0.0 {
        return Err(format!("negative size: {s}"));
    }
    Ok((n * mult as f64) as u64)
}
