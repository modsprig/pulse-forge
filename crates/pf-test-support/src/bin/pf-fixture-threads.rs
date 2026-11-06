// Deterministic workload arithmetic: fixed iteration counts, wrapping checksums.
#![allow(clippy::arithmetic_side_effects)]
//! Two-thread fixture (PF-002): main + one worker spinning for `--ms`
//! (default 2000). Thread count observable via toolhelp.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match pf_test_support::parse_ms_arg(&args, 2_000) {
        Ok(ms) => {
            let worker = std::thread::spawn(move || pf_test_support::spin_ms(ms));
            let main_sum = pf_test_support::spin_ms(ms);
            let worker_sum = worker.join().expect("worker");
            println!("threads main={main_sum} worker={worker_sum}");
        }
        Err(e) => {
            eprintln!("pf-fixture-threads: {e}");
            std::process::exit(2);
        }
    }
}
