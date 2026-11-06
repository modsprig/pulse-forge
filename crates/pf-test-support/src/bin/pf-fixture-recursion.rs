// Deterministic workload arithmetic: fixed iteration counts, wrapping checksums.
#![allow(clippy::arithmetic_side_effects)]
//! Recursive-stack fixture (PF-002): repeated deep recursion so a future
//! stack sampler can observe real frames. `--ms` default 2000.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match pf_test_support::parse_ms_arg(&args, 2_000) {
        Ok(ms) => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            let mut acc = 0u64;
            let mut depth = 200u32;
            while std::time::Instant::now() < deadline {
                acc = acc.wrapping_add(pf_test_support::recurse(depth));
                depth = (depth % 4_000) + 220;
            }
            println!("recursion acc={acc}");
        }
        Err(e) => {
            eprintln!("pf-fixture-recursion: {e}");
            std::process::exit(2);
        }
    }
}
