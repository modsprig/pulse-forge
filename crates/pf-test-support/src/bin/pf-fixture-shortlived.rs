// Deterministic workload arithmetic: fixed iteration counts, wrapping checksums.
#![allow(clippy::arithmetic_side_effects)]
//! Short-lived fixture (PF-002): exits after ~400 ms — for target-exit
//! recording tests. `--ms` default 400.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match pf_test_support::parse_ms_arg(&args, 400) {
        Ok(ms) => {
            let checksum = pf_test_support::spin_ms(ms);
            println!("shortlived {ms}ms checksum={checksum}");
        }
        Err(e) => {
            eprintln!("pf-fixture-shortlived: {e}");
            std::process::exit(2);
        }
    }
}
