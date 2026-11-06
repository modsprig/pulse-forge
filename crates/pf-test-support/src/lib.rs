//! # pf-test-support
//!
//! Deterministic fixture workloads (roadmap PF-002) shared by integration
//! tests, feasibility probes and benchmarks.
//!
//! Contracts:
//! - every fixture exits 0 on success, non-zero on bad usage;
//! - workloads are deterministic in *shape* (fixed iteration counts, fixed
//!   buffers) so assertions are about behavior classes (CPU burned, bytes
//!   written), never wall-clock exactness;
//! - fixtures never touch the network and write only under a temp dir or an
//!   explicitly provided `--dir`.

use std::time::{Duration, Instant};

/// Burn `ms` of wall time with CPU work. Returns a checksum so the loop
/// cannot be optimized away.
// Loop arithmetic wraps by design (checksum) and the deadline add is
// bounded by the argument range enforced by callers (<= 600 s).
#[allow(clippy::arithmetic_side_effects)]
pub fn spin_ms(ms: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    while Instant::now() < deadline {
        for _ in 0..10_000 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
        }
    }
    x
}

/// Allocate and touch a wave of memory: `chunks` × `chunk_bytes`, returning
/// a checksum of touched bytes.
pub fn allocation_wave(chunks: usize, chunk_bytes: usize) -> u64 {
    let mut checksum: u64 = 0;
    let mut keep: Vec<Vec<u8>> = Vec::with_capacity(chunks);
    for i in 0..chunks {
        let mut v = vec![0u8; chunk_bytes];
        // Touch every page so residency really grows.
        for b in v.iter_mut().step_by(4096) {
            *b = u8::try_from(i % 251).unwrap_or(0);
        }
        checksum = checksum.wrapping_add(u64::from(v[chunk_bytes / 2]));
        keep.push(v);
    }
    checksum
}

/// Write and read back `rounds` fixed buffers in `dir`. Returns bytes read.
pub fn file_io_rounds(dir: &std::path::Path, rounds: usize, bytes_per_round: usize) -> u64 {
    use std::io::{Read, Write};
    let path = dir.join("pf-fixture-io.bin");
    let payload: Vec<u8> = (0..bytes_per_round)
        .map(|i| u8::try_from(i % 256).unwrap_or(0))
        .collect();
    let mut total_read = 0u64;
    for _ in 0..rounds {
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = f.write_all(&payload);
            let _ = f.sync_all();
        }
        if let Ok(mut f) = std::fs::File::open(&path) {
            let mut sink = [0u8; 8192];
            loop {
                match f.read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total_read = total_read.saturating_add(n as u64),
                }
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    total_read
}

/// Recursive workload: recurse to `depth`, doing a small deterministic
/// amount of arithmetic per frame so real stack frames exist (for future
/// stack-sampling validation).
// `level` is 0-checked before decrementing; depth is caller-bounded.
#[allow(clippy::arithmetic_side_effects)]
pub fn recurse(depth: u32) -> u64 {
    fn inner(level: u32, acc: u64) -> u64 {
        let acc = acc.wrapping_add(u64::from(level) * 2_655_399);
        if level == 0 {
            acc
        } else {
            inner(level - 1, acc)
        }
    }
    inner(depth, 0)
}

/// Parse `--ms N` (default from `default_ms`); returns Err(message) on bad
/// usage without panicking.
// Index stepping is bounded by the loop condition.
#[allow(clippy::arithmetic_side_effects)]
pub fn parse_ms_arg(args: &[String], default_ms: u64) -> Result<u64, String> {
    let mut ms = default_ms;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--ms" {
            let Some(v) = args.get(i + 1) else {
                return Err("--ms requires a value".into());
            };
            ms = v
                .parse::<u64>()
                .map_err(|_| format!("invalid --ms value `{v}`"))?;
            i += 2;
        } else {
            return Err(format!("unknown argument `{}`", args[i]));
        }
    }
    if ms == 0 || ms > 600_000 {
        return Err("--ms must be within 1..=600000".into());
    }
    Ok(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spin_is_deterministic_shape_and_bounded() {
        let t0 = Instant::now();
        let x = spin_ms(50);
        assert_ne!(x, 0);
        let elapsed = t0.elapsed();
        // Shape bound: never returns early, never runs away by an order of
        // magnitude (CI containers can be slow; allow generous slack).
        assert!(elapsed >= Duration::from_millis(40), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    }

    #[test]
    fn allocation_wave_grows_residency_deterministically() {
        // Shape assertion: touching N chunks of 64 KiB touches every page.
        let c = allocation_wave(16, 64 * 1024);
        assert_ne!(c, 0);
    }

    #[test]
    fn file_io_rounds_transfers_expected_bytes() {
        let dir = std::env::temp_dir().join(format!("pf-fixture-io-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let read = file_io_rounds(&dir, 3, 64 * 1024);
        assert_eq!(read, 3 * 64 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recurse_produces_depth_dependent_result() {
        assert_ne!(recurse(64), recurse(128));
    }

    #[test]
    fn ms_arg_parsing_rejects_garbage() {
        let a = ["--ms".to_string()];
        assert!(parse_ms_arg(&a, 1000).is_err());
        let a = ["--ms".to_string(), "abc".to_string()];
        assert!(parse_ms_arg(&a, 1000).is_err());
        let a = ["--ms".to_string(), "0".to_string()];
        assert!(parse_ms_arg(&a, 1000).is_err());
        let a: [String; 0] = [];
        assert_eq!(parse_ms_arg(&a, 750).unwrap(), 750);
        let a = ["--ms".to_string(), "1234".to_string()];
        assert_eq!(parse_ms_arg(&a, 750).unwrap(), 1234);
        let a = ["wat".to_string()];
        assert!(parse_ms_arg(&a, 750).is_err());
    }
}
