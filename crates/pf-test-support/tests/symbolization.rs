//! P10 bounded slice: symbolization of sampled IPs against the fixture
//! binary's own ELF symbols. The hot loop runs in a known function; the
//! sampled leaf IPs must resolve to that function's symbol — the
//! known-hotspot verification for Linux.

#![cfg(target_os = "linux")]

use pf_linux::symbols::{MapsEntry, elf_functions, parse_maps, read_maps};

fn maps_of_self() -> Vec<MapsEntry> {
    parse_maps(
        &std::fs::read_to_string(format!("/proc/{}/maps", std::process::id()))
            .expect("read self maps"),
    )
}

#[test]
fn maps_parse_extracts_file_backed_ranges() {
    let maps = maps_of_self();
    assert!(!maps.is_empty());
    // The test binary itself must appear as a file-backed range.
    let self_exe = std::env::current_exe().expect("current_exe");
    let exe_name = self_exe.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        maps.iter().any(|m| m.pathname.ends_with(&exe_name)),
        "self binary {exe_name} not found in maps: {:?}",
        maps.iter().map(|m| m.pathname.clone()).collect::<Vec<_>>()
    );
    // Ranges are ordered and non-overlapping (as parsed).
    for w in maps.windows(2) {
        assert!(w[0].end <= w[1].start, "overlapping ranges");
    }
}

#[test]
fn elf_functions_extracts_named_functions() {
    let exe = std::env::current_exe().expect("current_exe");
    let funcs = elf_functions(&exe).expect("elf parse");
    assert!(!funcs.is_empty(), "test binary has symbols (debug build)");
    // Sorted by value.
    for w in funcs.windows(2) {
        assert!(w[0].value <= w[1].value);
    }
    // Named functions exist (not stripped).
    assert!(
        funcs
            .iter()
            .any(|f| f.name.contains("hot_outer") || f.name.contains("inner"))
    );
}

#[test]
fn sampled_hotspot_child_resolves_to_the_hot_function() {
    // The known-hotspot verification samples a CHILD PROCESS running
    // exactly one workload (the fixture's spin loop) — cargo runs tests
    // in parallel threads of one process, so a same-process hotspot would
    // be diluted by concurrently-running tests. The child's address space
    // contains exactly the fixture.
    let fixture = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pf-fixture-cpu"));
    let mut child = std::process::Command::new(&fixture)
        .args(["--ms", "3000"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn fixture");
    std::thread::sleep(std::time::Duration::from_millis(150));

    // 1. Sample the CHILD (same user; paranoid=2 permits same-user
    //    user-space measurement).
    let mut sampler = pf_linux::perf::PerfSampler::open_for_pid(
        i32::try_from(child.id()).expect("pid fits"),
        1_000_000,
    )
    .expect("open sampler for child");
    sampler.start();
    std::thread::sleep(std::time::Duration::from_millis(500));
    sampler.stop();
    let samples = sampler.collect();
    assert!(!samples.is_empty(), "hot 500ms must produce samples");

    // 2. SELF-time attribution: the LEAF IP (first user-space callchain
    //    entry) is where the CPU was at sample time. Root frames
    //    (__libc_start_main, lang_start) appear on every stack but execute
    //    nothing — counting all frames would let them dominate.
    let leaf_ips: Vec<u64> = samples
        .iter()
        .filter_map(|s| s.ips.first().copied())
        .filter(|ip| *ip < 0xffff_8000_0000_0000) // user-space only
        .collect();
    assert!(!leaf_ips.is_empty(), "user-space leaf IPs must exist");

    // 3. Resolve the leaf IPs against the CHILD's module map + the
    //    fixture's ELF symbols.
    let maps = read_maps(child.id()).expect("child maps");
    let fixture_path = maps
        .iter()
        .find(|m| m.pathname.ends_with("pf-fixture-cpu"))
        .map(|m| m.pathname.clone())
        .expect("fixture binary in child maps");
    let mut cache: std::collections::HashMap<String, pf_linux::symbols::ModuleSymbols> =
        std::collections::HashMap::new();
    let resolved = pf_linux::symbols::resolve_ips(&leaf_ips, &maps, &mut cache);
    assert_eq!(resolved.len(), leaf_ips.len());

    // 4. Diagnostics BEFORE asserts (attribution evidence).
    let mut leaf_symbol_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for r in resolved.iter().flatten() {
        if !r.symbol.is_empty() {
            *leaf_symbol_counts.entry(r.symbol.clone()).or_insert(0) += 1;
        }
    }
    let mut module_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for r in resolved.iter().flatten() {
        *module_counts.entry(r.module.clone()).or_insert(0) += 1;
    }
    for (m, c) in &module_counts {
        println!("module {m}: {c} leaf frames");
    }
    for (sym, c) in &leaf_symbol_counts {
        println!("symbol {sym}: {c}");
    }

    // 5. Known-hotspot verification: the top LEAF symbol must be the
    //    fixture's spin loop (the child does nothing else on-CPU).
    let top = leaf_symbol_counts
        .iter()
        .max_by_key(|(_, c)| **c)
        .map(|(n, _)| n.clone())
        .expect("at least one resolved leaf symbol");
    // The fixture's known hot path: spin_ms plus its inlined Range
    // iterator (spec_next / forward_unchecked / next) — all inside the
    // fixture's own crate. libc/startup frames must NOT dominate.
    assert!(
        top.contains("pf_test_support"),
        "top leaf symbol `{top}` is not the fixture's known hot path          (leaves: {leaf_symbol_counts:?})"
    );
    assert!(
        !top.contains("libc") && !top.contains("lang_start"),
        "startup frames must not be the hot leaf: {top}"
    );

    // 6. The fixture module must own the resolved leaf frames.
    let module_hits = resolved
        .iter()
        .flatten()
        .filter(|r| r.module == fixture_path)
        .count();
    assert!(module_hits > 0, "fixture module frames must resolve");

    let _ = child.kill();
    let _ = child.wait();
    let _ = fixture;
}
