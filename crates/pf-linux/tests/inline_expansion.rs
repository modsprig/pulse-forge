//! PF-1005 known-hotspot verification: DWARF inline-frame expansion over a
//! REAL binary compiled by the pinned toolchain. The fixture is built with
//! `rustc -O -g` (optimization inlines `inline_inner` into `outer` while
//! keeping debug info); chains resolved over `outer`'s code range must
//! surface the inlinee — statically, without sampling noise.
//!
//! Graceful degradation is asserted too: the same fixture compiled WITHOUT
//! debug info yields single-entry chains (enclosing symbol only).

#![cfg(target_os = "linux")]
#![allow(clippy::arithmetic_side_effects)]

use std::path::PathBuf;
use std::process::Command;

use pf_linux::symbols::{MapsEntry, ModuleSymbols, resolve_ip_chains};

const FIXTURE_SRC: &str = r#"
#[inline(never)]
fn blackbox(x: u64) -> u64 {
    x.wrapping_add(1)
}

#[inline(always)]
fn inline_inner(x: u64) -> u64 {
    blackbox(x).wrapping_mul(3)
}

#[inline(never)]
fn outer(seed: u64) -> u64 {
    let mut acc = seed;
    for _ in 0..200_000 {
        acc = acc.wrapping_add(inline_inner(acc));
        acc = acc.wrapping_mul(6364136223846793005).wrapping_add(1);
    }
    acc
}

pub fn main() {
    // Prevent constant folding: derive the seed from the environment.
    let seed = std::env::args().count() as u64 + 7;
    std::process::exit((outer(seed) % 2) as i32);
}
"#;

fn compile(tag: &str, extra: &[&str]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pf-inline-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("inline_fixture.rs");
    std::fs::write(&src, FIXTURE_SRC).unwrap();
    let out = dir.join("inline_fixture");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let status = Command::new(&rustc)
        .arg("--edition")
        .arg("2021")
        .args(extra)
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("run {rustc}: {e}"));
    assert!(status.success(), "fixture compilation failed");
    out
}

/// Fabricated non-PIE-style module map: load base 0x400000, generous span.
fn fabricate_maps(bin: &std::path::Path) -> Vec<MapsEntry> {
    vec![MapsEntry {
        start: 0x400000,
        end: 0x800000,
        pathname: bin.display().to_string(),
    }]
}

#[test]
fn inline_frames_expand_from_real_dwarf() {
    let bin = compile("dwarf", &["-C", "opt-level=2", "-C", "debuginfo=2"]);

    let syms_inline = pf_linux::dwarf::inline_ranges(&bin);
    let syms = ModuleSymbols {
        funcs: pf_linux::symbols::elf_functions(&bin).expect("symtab"),
        inline_index: pf_linux::symbols::InlineIndex::of(&syms_inline),
        inline: syms_inline,
        lines: None,
        symbols_file: None,
    };
    assert!(
        !syms.inline.is_empty(),
        "the -g fixture must yield inline ranges (DWARF v4/v5 path)"
    );
    // The known inlinee appears in the DWARF table.
    assert!(
        syms.inline
            .iter()
            .any(|r| r.chain.iter().any(|n| n.contains("inline_inner"))),
        "inline_inner must be an extracted inlinee: {:?}",
        syms.inline.first()
    );

    // Resolve chains across `outer`'s code range (bounded scan): at least
    // one IP must expand to [inline_inner, …, outer], innermost-first.
    let maps = fabricate_maps(&bin);
    let mut cache: std::collections::HashMap<String, ModuleSymbols> =
        std::collections::HashMap::new();
    let outer = syms
        .funcs
        .iter()
        .find(|f| f.name.contains("outer") && f.size > 0)
        .expect("outer must exist in the symtab");
    let mut ips = Vec::new();
    let steps = 256usize;
    for i in 0..steps {
        let rel = outer.value + (outer.size * i as u64) / steps as u64;
        ips.push(0x400000 + rel);
    }
    let chains = resolve_ip_chains(&ips, &maps, &mut cache);
    let mut expanded = 0usize;
    let mut found_inner = false;
    for chain in chains.iter().flatten() {
        if chain.len() >= 2 {
            expanded += 1;
            if chain[0].symbol.contains("inline_inner") {
                found_inner = true;
                // The enclosing frame is the last entry.
                assert!(
                    chain.last().unwrap().symbol.contains("outer"),
                    "chain must end at the enclosing symbol: {chain:?}"
                );
                eprintln!(
                    "E2E-INLINE-CHAIN {} -> {}",
                    chain[0].symbol,
                    chain.last().unwrap().symbol
                );
            }
        }
    }
    assert!(
        found_inner,
        "at least one IP in `outer` must expand to inline_inner (expanded {expanded} of {})",
        chains.len()
    );
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}

/// The 1-based line of `marker` inside FIXTURE_SRC (the compiler maps the
/// hot loop to this line).
fn source_line_of(marker: &str) -> usize {
    FIXTURE_SRC
        .lines()
        .position(|l| l.contains(marker))
        .expect("marker in fixture source")
        + 1 // FIXTURE_SRC starts with a newline; lines() is 0-based
}

#[test]
fn line_map_attributes_ips_to_the_hot_loop() {
    let bin = compile("lines", &["-C", "opt-level=2", "-C", "debuginfo=2"]);
    let maps = fabricate_maps(&bin);
    let mut cache: std::collections::HashMap<String, ModuleSymbols> =
        std::collections::HashMap::new();
    let syms_inline = pf_linux::dwarf::inline_ranges(&bin);
    let syms = ModuleSymbols {
        funcs: pf_linux::symbols::elf_functions(&bin).expect("symtab"),
        inline_index: pf_linux::symbols::InlineIndex::of(&syms_inline),
        inline: syms_inline,
        lines: pf_linux::line::line_map(&bin),
        symbols_file: None,
    };
    let lines = syms
        .lines
        .as_ref()
        .expect("the -g fixture must carry a line map");
    assert!(!lines.truncated, "small fixture: row cap never hit");
    let outer = syms
        .funcs
        .iter()
        .find(|f| f.name.contains("outer") && f.size > 0)
        .expect("outer in symtab");

    // Resolve chains across outer's range; the sampled PCs inside the hot
    // loop must attribute to the loop's source line (the innermost frame's
    // code: inlined `inline_inner` body or the loop line in `outer`).
    let fn_outer_line = source_line_of("fn outer");
    let fn_end_line = source_line_of("pub fn main");
    let mut found: Option<(String, u32)> = None;
    let steps = 64usize;
    for i in 0..steps {
        let rel = outer.value + (outer.size * i as u64) / steps as u64;
        let ips = vec![0x400000 + rel];
        let chains = resolve_ip_chains(&ips, &maps, &mut cache);
        let chain = chains[0].as_ref().expect("resolved");
        let leaf = chain.first().unwrap();
        if let (Some(file), Some(line)) = (&leaf.file, leaf.line) {
            assert!(
                file.starts_with('/') && file.ends_with("inline_fixture.rs"),
                "comp-dir must make the source path absolute, got {file}"
            );
            assert!(
                (line as usize) >= fn_outer_line && (line as usize) <= fn_end_line,
                "line {line} must fall inside `outer`'s source span"
            );
            found = Some((file.clone(), line));
            break;
        }
    }
    let (file, line) = found.expect("at least one IP must carry file:line");
    eprintln!("E2E-LINE {file}:{line}");
    let _ = fn_end_line;
}

#[test]
fn split_debug_debuglink_recovers_symbols_and_sources() {
    let bin = compile("split", &["-C", "opt-level=2", "-C", "debuginfo=2"]);
    // Split the debug info out, strip the binary, and attach the link —
    // the standard objcopy split-debug workflow.
    let debug_file = bin.with_file_name("inline_fixture.debug");
    let st = Command::new("objcopy")
        .args(["--only-keep-debug"])
        .arg(&bin)
        .arg(&debug_file)
        .status()
        .expect("run objcopy (binutils)");
    assert!(st.success(), "objcopy --only-keep-debug failed");
    let st = Command::new("strip").arg(&bin).status().expect("run strip");
    assert!(st.success(), "strip failed");
    let st = Command::new("objcopy")
        .args(["--add-gnu-debuglink"])
        .arg(&debug_file)
        .arg(&bin)
        .status()
        .expect("run objcopy");
    assert!(st.success(), "add-gnu-debuglink failed");

    // Resolution goes through load_module_symbols (cache miss): the
    // debuglink must recover symtab + inline chains + line info.
    let maps = fabricate_maps(&bin);
    let mut cache: std::collections::HashMap<String, ModuleSymbols> =
        std::collections::HashMap::new();
    let outer = pf_linux::symbols::elf_functions(&debug_file)
        .expect("symtab")
        .into_iter()
        .find(|f| f.name.contains("outer") && f.size > 0)
        .expect("outer in the split debug file's symtab");
    let steps = 32usize;
    let mut recovered_symbol = false;
    let mut recovered_source = false;
    for i in 0..steps {
        let rel = outer.value + (outer.size * i as u64) / steps as u64;
        let ips = vec![0x400000 + rel];
        let chains = resolve_ip_chains(&ips, &maps, &mut cache);
        let chain = chains[0].as_ref().expect("resolved");
        if chain.last().unwrap().symbol.contains("outer") {
            recovered_symbol = true;
        }
        if let Some(f) = &chain.first().unwrap().file {
            // comp-dir resolution must survive the split-debug flow too.
            if f.starts_with('/') && f.ends_with("inline_fixture.rs") {
                recovered_source = true;
            }
        }
        if recovered_symbol && recovered_source {
            break;
        }
    }
    assert!(
        recovered_symbol,
        "debuglink recovery must restore enclosing symbols"
    );
    assert!(
        recovered_source,
        "debuglink recovery must restore .debug_line source info"
    );
    // The cache records where the data came from.
    let syms = cache.values().next().expect("cached module");
    assert!(
        syms.symbols_file
            .as_deref()
            .is_some_and(|f| f.ends_with(".debug")),
        "symbols_file must point at the split debug file, got {:?}",
        syms.symbols_file
    );
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}

#[test]
fn build_id_note_parses_and_drives_the_layout() {
    let bin = compile("buildid", &["-C", "opt-level=2", "-C", "debuginfo=2"]);
    // rustc emits a GNU build-id; the parser reads it and the layout
    // helpers turn it into the standard gdb path shape.
    let id = pf_linux::elfio::build_id(&bin).expect("build-id note present");
    assert!(id.len() >= 4, "build-id too short: {id:?}");
    let hex = pf_linux::elfio::build_id_hex(&id);
    assert_eq!(hex.len(), id.len() * 2);
    let candidates = pf_linux::elfio::build_id_candidates(&bin, &id);
    assert_eq!(candidates.len(), 2);
    let expected_rel = format!(".build-id/{}/{}.debug", &hex[..2], &hex[2..]);
    assert!(
        candidates[0].ends_with(std::path::Path::new(&expected_rel)),
        "sibling layout expected: {:?}",
        candidates[0]
    );
    assert!(
        candidates[1]
            .to_str()
            .is_some_and(|p| p.starts_with("/usr/lib/debug/.build-id/")),
        "system layout expected: {:?}",
        candidates[1]
    );
    // A binary stripped of the note (objcopy removes it with -R) parses as None.
    let no_note = bin.with_file_name("inline_fixture.nonote");
    std::fs::copy(&bin, &no_note).expect("copy");
    let st = Command::new("objcopy")
        .args(["-R", ".note.gnu.build-id"])
        .arg(&no_note)
        .status()
        .expect("run objcopy");
    assert!(st.success(), "objcopy -R failed");
    assert!(
        pf_linux::elfio::build_id(&no_note).is_none(),
        "a note-less binary must parse as None"
    );
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}

#[test]
fn stripped_binary_recovers_via_build_id_layout() {
    let bin = compile("bidsplit", &["-C", "opt-level=2", "-C", "debuginfo=2"]);
    // Standard split, then place the debug file at the build-id layout
    // NEXT TO the stripped binary (no system-path writes in tests).
    let debug_file = bin.with_file_name("inline_fixture.debug");
    let st = Command::new("objcopy")
        .args(["--only-keep-debug"])
        .arg(&bin)
        .arg(&debug_file)
        .status()
        .expect("run objcopy");
    assert!(st.success());
    let st = Command::new("strip").arg(&bin).status().expect("run strip");
    assert!(st.success());

    let id = pf_linux::elfio::build_id(&bin).expect("stripped binary keeps its build-id");
    let hex = pf_linux::elfio::build_id_hex(&id);
    let (xx, rest) = hex.split_at(2);
    let layout_dir = bin.parent().unwrap().join(format!(".build-id/{xx}"));
    std::fs::create_dir_all(&layout_dir).expect("create .build-id dir");
    std::fs::rename(&debug_file, layout_dir.join(format!("{rest}.debug")))
        .expect("move debug file into the build-id layout");

    // Resolution goes through load_module_symbols (cache miss): the
    // build-id layout must recover symtab + inline chains + line info.
    let maps = fabricate_maps(&bin);
    let mut cache: std::collections::HashMap<String, ModuleSymbols> =
        std::collections::HashMap::new();
    let debug_at = layout_dir.join(format!("{rest}.debug"));
    let outer = pf_linux::symbols::elf_functions(&debug_at)
        .expect("symtab")
        .into_iter()
        .find(|f| f.name.contains("outer") && f.size > 0)
        .expect("outer in the split debug file's symtab");
    let steps = 32usize;
    let mut recovered_symbol = false;
    let mut recovered_source = false;
    for i in 0..steps {
        let rel = outer.value + (outer.size * i as u64) / steps as u64;
        let chains = resolve_ip_chains(&[0x400000 + rel], &maps, &mut cache);
        let chain = chains[0].as_ref().expect("resolved");
        if chain.last().unwrap().symbol.contains("outer") {
            recovered_symbol = true;
        }
        if let Some(f) = &chain.first().unwrap().file {
            // comp-dir resolution must survive the split-debug flow too.
            if f.starts_with('/') && f.ends_with("inline_fixture.rs") {
                recovered_source = true;
            }
        }
        if recovered_symbol && recovered_source {
            break;
        }
    }
    assert!(
        recovered_symbol && recovered_source,
        "build-id recovery must restore symbols and sources"
    );
    let syms = cache.values().next().expect("cached module");
    assert!(
        syms.symbols_file
            .as_deref()
            .is_some_and(|f| f.contains(".build-id/")),
        "symbols_file must point into the build-id layout, got {:?}",
        syms.symbols_file
    );
    // A mismatched build-id layout is never trusted: drop in a DIFFERENT
    // binary's debug file and resolution must degrade to honest emptiness.
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}

#[test]
fn stripped_binary_degrades_to_enclosing_symbol_only() {
    // `-C debuginfo=0` still emits full DWARF on this toolchain, so the
    // honest no-info case is a STRIPPED binary (no .debug_info at all).
    let bin = compile("stripped", &["-C", "opt-level=2", "-C", "debuginfo=2"]);
    let stripped = bin.with_file_name("inline_fixture.stripped");
    std::fs::copy(&bin, &stripped).expect("copy for strip");
    let st = Command::new("strip")
        .arg(&stripped)
        .status()
        .expect("run strip (binutils)");
    assert!(st.success(), "strip failed");
    let syms_inline = pf_linux::dwarf::inline_ranges(&stripped);
    let syms = ModuleSymbols {
        funcs: pf_linux::symbols::elf_functions(&stripped).expect("symtab"),
        inline_index: pf_linux::symbols::InlineIndex::of(&syms_inline),
        inline: syms_inline,
        lines: pf_linux::line::line_map(&stripped),
        symbols_file: None,
    };
    assert!(
        syms.inline.is_empty(),
        "a stripped binary must yield no inline ranges, got {}",
        syms.inline.len()
    );
    assert!(
        syms.lines.is_none(),
        "a stripped binary must yield no line map"
    );
    // strip removes .symtab AND .debug_info: resolving any mapped IP must
    // yield the honest single-entry chain with an empty symbol (the caller
    // renders module+offset — never an invented name).
    let maps = fabricate_maps(&stripped);
    let mut cache: std::collections::HashMap<String, ModuleSymbols> =
        std::collections::HashMap::new();
    let chains = resolve_ip_chains(&[0x400000 + 0x1000], &maps, &mut cache);
    let chain = chains[0].as_ref().expect("chain present for mapped ip");
    assert_eq!(chain.len(), 1, "no info: single enclosing frame");
    assert!(chain[0].symbol.is_empty(), "stripped: no invented name");
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}
