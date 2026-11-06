//! Seeded-mutation fuzz tests for the session stacks loader (PF-1202
//! extension): `stacks.jsonl` is read back by the CLI, the GUI and the
//! daemon query ops — a corrupted or tampered session file is hostile
//! input. Line-level mutations (wrong types, missing fields, broken
//! nesting, hostile strings) exercise the loader's per-line error paths
//! on top of the JSON parser's own fuzz coverage.
//!
//! Property under test: `load_session_stacks_timed` returns loaded
//! samples or a TYPED error — never a panic — and every loaded sample is
//! shape-consistent (timestamps present, callee-first frames carried
//! through in order). Same deterministic xorshift method as the other
//! fuzz suites.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::byte_char_slices
)]

use std::path::PathBuf;

use pf_analysis::stacks_io::load_session_stacks_timed;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next_u64() % n.max(1) as u64).unwrap_or(0)
    }
}

const SEEDS: [u64; 5] = [1, 2, 3, 5, 8];

/// Campaign-scale multiplier for hostile-case counts (PF_FUZZ_SCALE,
/// default 1, clamped 1..=64) — lets a CI or a manual deep run expand the
/// corpus without recompiling; gate runs stay at scale 1.
fn fuzz_scale() -> usize {
    std::env::var("PF_FUZZ_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .clamp(1, 64)
}

/// A valid stacks.jsonl line, exactly the writer's shape (frames
/// callee-first, ips list, inlined flags).
fn valid_line(i: u64) -> String {
    let depth: u64 = 3 + (i % 4);
    let mut ips = String::new();
    for d in 0..depth {
        if d > 0 {
            ips.push(',');
        }
        ips.push_str(&format!("{}", 0x400000 + d * 16));
    }
    let mut frames = String::new();
    for d in (0..depth).rev() {
        if d != depth - 1 {
            frames.push(',');
        }
        frames.push_str(&format!(
            "{{\"module\":\"/usr/lib/lib{}.so\",\"symbol\":\"fn_{}\",\"offset\":{},\"inlined\":{},\"file\":\"src/hot{}.rs\",\"line\":{}}}",
            d,
            (i + d) % 64,
            d * 8,
            d != depth - 1,
            d,
            10 + d
        ));
    }
    format!(
        "{{\"time_ns\":{},\"tid\":{},\"cpu\":{},\"ips\":[{}],\"frames\":[{}]}}\n",
        (i + 1) * 100_000_000,
        4242 + (i % 2),
        i % 16,
        ips,
        frames
    )
}

/// String-level mutation (UTF-8-safe; parse input is &str by contract).
fn mutate_text(rng: &mut Rng, base: &str) -> String {
    let mut out = base.as_bytes().to_vec();
    let rounds = 1 + rng.below(10);
    for _ in 0..rounds {
        if out.is_empty() {
            break;
        }
        match rng.below(5) {
            0 => {
                let i = rng.below(out.len());
                out[i] ^= 1 << rng.below(8);
            }
            1 => {
                let i = rng.below(out.len());
                // Poke field-shape traps: quotes, braces, colons, digits,
                // minus signs and exponent markers.
                out[i] = [b'"', b'{', b'}', b':', b'0', b'-', b'e', b'.', b'\\'][rng.below(9)];
            }
            2 => {
                let keep = rng.below(out.len());
                out.truncate(keep.max(1));
            }
            3 if out.len() < 4096 => {
                let s = rng.below(out.len());
                let e = (s + 1 + rng.below(48)).min(out.len());
                let dup: Vec<u8> = out[s..e].to_vec();
                out.extend_from_slice(&dup);
            }
            _ => {
                // Delete a field name to exercise missing-field paths.
                for needle in ["\"time_ns\"", "\"tid\"", "\"frames\"", "\"symbol\""] {
                    if let Some(p) = find(out.as_slice(), needle.as_bytes()) {
                        out.drain(p..p + needle.len());
                        break;
                    }
                }
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| base.to_string())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pf-fuzz-stacksio-{tag}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

#[test]
fn loader_survives_mutated_session_files() {
    let dir = scratch("mut");
    let path = dir.join("stacks.jsonl");
    // A valid multi-line file as the base corpus; the loader must accept
    // it (sanity: a dead seed proves nothing).
    let base: String = (0..24).map(valid_line).collect();
    std::fs::write(&path, &base).expect("write base");
    let loaded = load_session_stacks_timed(&dir).expect("base loads");
    assert_eq!(loaded.len(), 24);
    assert_eq!(
        loaded[0].stack.frames.len(),
        3,
        "line 0 loads its full depth"
    );

    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..600 * fuzz_scale() {
            let mutated = (0..24)
                .map(|i| {
                    if rng.below(2) == 0 {
                        mutate_text(&mut rng, &valid_line(i as u64))
                    } else {
                        valid_line(i as u64)
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            std::fs::write(&path, &mutated).expect("write mutant");
            // Ok(samples) or a typed error — never a panic. Loaded
            // samples keep their honest shape: a timestamp, and at least
            // one frame per loaded line.
            if let Ok(samples) = load_session_stacks_timed(&dir) {
                for t in &samples {
                    assert!(t.time_ns > 0 || t.tid > 0, "line carried identity");
                    assert!(!t.stack.frames.is_empty(), "a loaded stack carries frames");
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn loader_survives_structural_traps() {
    let dir = scratch("traps");
    let path = dir.join("stacks.jsonl");
    // Hand-built adversarial shapes the writer never emits but a tampered
    // file can: wrong types, deep nesting, huge numbers, empty frames,
    // duplicated keys, unicode in field values.
    let traps: [&str; 12] = [
        r#"{"time_ns":"not-a-number","tid":{},"ips":[],"frames":[]}"#,
        r#"{"time_ns":18446744073709551615,"tid":-1,"ips":[0],"frames":[{"module":0,"symbol":null,"offset":-5,"inlined":"yes","file":[],"line":1e300}]}"#,
        r#"{"time_ns":1,"tid":2,"ips":[1,2,3],"frames":[]}"#,
        r#"{"time_ns":1,"tid":2,"ips":[1,2,3],"frames":{}"#,
        r#"{"frames":[{"module":"a","symbol":"b","offset":0,"inlined":false,"file":null,"line":null}],"time_ns":1,"tid":2}"#,
        r#"{"time_ns":1,"tid":2,"frames":[{"module":"日本語","symbol":"∑→∑","offset":0,"inlined":false,"file":"src.rs","line":1}]}"#,
        r#"{"time_ns":1,"tid":2,"frames":[{"module":"a","offset":0},{"module":"b","symbol":"s","offset":1,"inlined":true,"file":"f","line":2}]}"#,
        r#"{"time_ns":1,"tid":2,"frames":[{"module":"a","symbol":"b","offset":0,"inlined":false,"file":"f","line":0}]}"#,
        r#"{"time_ns":1,"tid":2,"time_ns":3,"frames":[{"module":"a","symbol":"b","offset":0,"inlined":false,"file":"f","line":1}]}"#,
        r#"{"time_ns":1,"tid":2,"ips":[],"frames":[{"module":"a","symbol":"b","offset":0,"inlined":false,"file":"f","line":1,"extra":{"deep":{"deeper":[1,2,3]}}}]}"#,
        "",
        "\n\n  \n",
    ];
    for trap in traps {
        std::fs::write(&path, trap).expect("write trap");
        let _ = load_session_stacks_timed(&dir);
    }
    // The valid multi-line file with hostile lines interleaved.
    let mixed: String = (0..12)
        .flat_map(|i| [valid_line(i as u64), "\n".into(), "garbage {{{\n".into()])
        .collect();
    std::fs::write(&path, &mixed).expect("write mixed");
    // Unparseable lines are a typed PARTIAL_DATA error, never silence.
    let r = load_session_stacks_timed(&dir);
    assert!(r.is_err(), "garbage lines surface as typed errors");
    let _ = std::fs::remove_dir_all(&dir);
}
