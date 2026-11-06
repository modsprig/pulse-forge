//! Timing bench for inline-range resolution at real-binary scale
//! (`#[ignore]`d; run with `--ignored --nocapture`). A dev-profile system
//! library carries tens of thousands of DWARF inline ranges; the resolver
//! previously scanned all of them per unique IP. Numbers are printed for
//! the ledger and never asserted against (roadmap §16).
//!
//! The synthetic module is injected straight into the symbol cache (the
//! cache-hit path skips file IO), so the bench measures the lookup
//! structure, not the filesystem.

// The parsers live in Linux-gated modules; the bench vanishes elsewhere.
#![cfg(target_os = "linux")]
#![allow(clippy::arithmetic_side_effects)]

use std::collections::HashMap;

use pf_linux::dwarf::InlineRange;
use pf_linux::symbols::{MapsEntry, ModuleSymbols, resolve_ip_chains};

const CLUSTERS: usize = 2_000; // functions
const NESTING: usize = 10; // nested inline levels per function
const RANGES: usize = CLUSTERS * NESTING; // 20k ranges
const IPS: usize = 5_000;

fn synthetic_module() -> (Vec<MapsEntry>, ModuleSymbols) {
    let mut inline = Vec::with_capacity(RANGES);
    for c in 0..CLUSTERS {
        for k in 0..NESTING {
            // Nested ranges: level k covers [c*1024 + k*50, c*1024+1024-k*50).
            let low = (c * 1024 + k * 50) as u64;
            let high = (c * 1024 + 1024 - k * 50) as u64;
            inline.push(InlineRange {
                low,
                high,
                chain: (0..=k).map(|d| format!("inline_{c}_{d}")).collect(),
            });
        }
    }
    let maps = vec![MapsEntry {
        start: 0x400000,
        end: 0x400000 + (CLUSTERS as u64) * 1024,
        pathname: "/synthetic/libbig.so".into(),
    }];
    let syms = ModuleSymbols {
        funcs: Vec::new(),
        inline_index: pf_linux::symbols::InlineIndex::of(&inline),
        inline,
        lines: None,
        symbols_file: Some("/synthetic/libbig.so".into()),
    };
    (maps, syms)
}

#[test]
#[ignore = "timing bench: run with `cargo test -p pf-linux --test inline_bench -- --ignored --nocapture`"]
fn resolve_throughput_many_inline_ranges() {
    let (maps, syms) = synthetic_module();
    let mut symcache = HashMap::new();
    symcache.insert("/synthetic/libbig.so".to_string(), syms);

    // Unique IPs spread across the clusters (persist dedups to uniques —
    // this is the shape the resolver actually faces).
    let ips: Vec<u64> = (0..IPS)
        .map(|i| 0x400000u64 + ((i as u64 * 7919) % (CLUSTERS as u64 * 1024 - 64)))
        .collect();

    let mut medians = Vec::new();
    let mut resolved = 0usize;
    for _ in 0..5 {
        let t0 = std::time::Instant::now();
        let out = resolve_ip_chains(&ips, &maps, &mut symcache);
        medians.push(t0.elapsed().as_secs_f64() * 1000.0);
        resolved = out.iter().flatten().count();
    }
    medians.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    eprintln!(
        "inline_resolve: {RANGES} ranges, {IPS} ips, resolved={resolved}, median {:.2} ms",
        medians[2]
    );
    assert_eq!(resolved, IPS, "every IP resolves to its inline chain");
}
