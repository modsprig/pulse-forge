// Deterministic workload arithmetic: fixed iteration counts, wrapping checksums.
#![allow(clippy::arithmetic_side_effects)]
//! CPU-bound fixture: spins for `--ms` (default 2000). Deterministic shape:
//! one thread, one hot loop (PF-002).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match pf_test_support::parse_ms_arg(&args, 2_000) {
        Ok(ms) => {
            let checksum = pf_test_support::spin_ms(ms);
            println!("cpu-spin {ms}ms checksum={checksum}");
        }
        Err(e) => {
            eprintln!("pf-fixture-cpu: {e}");
            std::process::exit(2);
        }
    }
}
