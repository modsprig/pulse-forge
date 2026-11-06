// Deterministic workload arithmetic: fixed iteration counts, wrapping checksums.
#![allow(clippy::arithmetic_side_effects)]
//! CPU + memory + file I/O fixture (PF-002): interleaves spin, allocation
//! waves and file rounds for `--ms` (default 2000).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match pf_test_support::parse_ms_arg(&args, 2_000) {
        Ok(ms) => {
            let dir = std::env::temp_dir().join(format!("pf-fixture-mixed-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            let mut round = 0u64;
            while std::time::Instant::now() < deadline {
                let _ = pf_test_support::spin_ms(120);
                let _ = pf_test_support::allocation_wave(24, 64 * 1024);
                let read = pf_test_support::file_io_rounds(&dir, 4, 128 * 1024);
                round += 1;
                let _ = read;
            }
            let _ = std::fs::remove_dir_all(&dir);
            println!("mixed rounds={round}");
        }
        Err(e) => {
            eprintln!("pf-fixture-mixed: {e}");
            std::process::exit(2);
        }
    }
}
