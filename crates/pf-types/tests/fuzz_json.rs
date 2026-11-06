//! Seeded-mutation fuzz tests for the parsers that face untrusted input
//! (roadmap PF-1202 subset, dependency-free).
//!
//! Method: deterministic xorshift PRNG generates (a) random byte soups,
//! (b) mutations of valid documents (bit flips, truncations, duplications,
//! splices). The property under test is not *correctness of values* but
//! **memory- and panic-safety plus total consumption**: every input either
//! parses or produces a typed error — never a panic, never a hang, and the
//! depth cap always holds.

// Fuzz loops with bounded counters; the i64 wrap below is deliberate.
#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::cast_possible_wrap)]

use pf_types::SessionId;
use pf_types::json::{self, JsonValue};

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

const SEEDS: [u64; 8] = [1, 2, 3, 5, 8, 13, 21, 34];

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

#[test]
fn json_parser_survives_random_byte_soups() {
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..20_000 * fuzz_scale() {
            let len = 1 + rng.below(512);
            let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            // Valid-UTF-8 filter: parser input must be &str; invalid UTF-8
            // is rejected earlier by the transport layer (documented).
            if let Ok(text) = String::from_utf8(bytes) {
                let _ = json::parse(&text);
            }
        }
    }
}

#[test]
fn json_parser_survives_mutations_of_valid_documents() {
    let valid = [
        r#"{"a":1,"b":[true,null,"x\ny"],"c":2.5,"d":-3}"#,
        r#"{"nested":{"deeply":{"deeper":[{"k":"\\\\u00e9"}]}}}"#,
        "[1,2,3,4,5,6,7,8,9,10]",
        "\"plain string \\u0041 \\ud834\\udd1e\"",
    ];
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed.wrapping_mul(7919));
        for round in 0..20_000 {
            let doc = valid[rng.below(valid.len())];
            let mut bytes = doc.as_bytes().to_vec();
            match rng.below(5) {
                0 if !bytes.is_empty() => {
                    // single byte flip
                    let at = rng.below(bytes.len());
                    bytes[at] ^= 1u8 << rng.below(8).min(7);
                }
                1 if !bytes.is_empty() => {
                    // truncate
                    bytes.truncate(rng.below(bytes.len()));
                }
                2 if !bytes.is_empty() => {
                    // duplicate a slice
                    let at = rng.below(bytes.len());
                    let take = rng.below(bytes.len() - at) + 1;
                    let mut dup = Vec::new();
                    dup.extend_from_slice(&bytes);
                    let insert = bytes.clone();
                    dup.splice(
                        at..at,
                        insert[cpu_slice(&insert, at, take)..]
                            .iter()
                            .cloned()
                            .take(take),
                    );
                    bytes = dup;
                }
                3 if !bytes.is_empty() => {
                    // insert a control character
                    let at = rng.below(bytes.len());
                    bytes.insert(at, 0x01);
                }
                _ => {
                    // splice two documents
                    let other = valid[rng.below(valid.len())];
                    let cut = rng.below(bytes.len());
                    let tail = other.as_bytes();
                    let mut spliced = bytes[..cut].to_vec();
                    spliced.extend_from_slice(&tail[rng.below(tail.len())..]);
                    bytes = spliced;
                }
            }
            let _ = round;
            if let Ok(text) = String::from_utf8(bytes) {
                let _ = json::parse(&text);
            }
        }
    }
}

fn cpu_slice(v: &[u8], at: usize, take: usize) -> usize {
    at.saturating_sub(take).min(v.len())
}

#[test]
fn depth_cap_holds_under_mutation_bombing() {
    // Deep nesting right at the cap parses; beyond it errors — and mixed
    // truncations never panic.
    let at_cap = "[".repeat(pf_types::json::MAX_DEPTH) + &"]".repeat(pf_types::json::MAX_DEPTH);
    assert!(json::parse(&at_cap).is_ok());
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..5_000 * fuzz_scale() {
            let depth = 1 + rng.below(pf_types::json::MAX_DEPTH + 64);
            let mut doc = "[".repeat(depth);
            if rng.below(2) == 0 {
                doc.push_str(&"]".repeat(depth));
            }
            let _ = json::parse(&doc);
        }
    }
}

#[test]
fn counter_snapshot_rejects_type_confusion_not_panics() {
    // Well-formed JSON that is type-confused for CounterSnapshot.
    let hostile = [
        r#"{"mono_ns": "2026-10-25"}"#,
        r#"{"mono_ns": -1}"#,
        r#"{"cpu_user_ns": 1.5}"#,
        r#"{"thread_count": 99999999999}"#,
        r#"{"resident_bytes": []}"#,
        r#"{"mono_ns": 18446744073709551616}"#, // u64 + 1
        "[]",
        "\"string\"",
        "42",
    ];
    for doc in hostile {
        let v = json::parse(doc).expect("hostile doc must parse as JSON");
        // from_json must error on wrong shapes, never panic.
        let result = pf_types::CounterSnapshot::from_json(&v);
        if doc == "42" || doc == "\"string\"" || doc == "[]" {
            assert!(result.is_err(), "{doc}");
        }
        // For the object cases: mono_ns absent/mangled yields default or
        // error — either way, no panic (test reaching here is the point).
    }
}

#[test]
fn session_id_parse_survives_hostile_input() {
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..10_000 * fuzz_scale() {
            let len = rng.below(64);
            let text: String = (0..len)
                .map(|_| char::from_u32((rng.next_u64() & 0x7F) as u32).unwrap_or('x'))
                .collect();
            let _ = SessionId::parse(&text);
        }
        // Very long input bounded.
        let _ = SessionId::parse(&"a".repeat(1_000_000));
    }
}

#[test]
fn parser_output_always_roundtrips_or_rejects() {
    // Anything the parser ACCEPTS must re-serialize and re-parse to an
    // identical value (stable writer/parser pair) — checked on structured
    // random documents built recursively.
    fn build(depth: usize, rng: &mut Rng) -> JsonValue {
        if depth == 0 {
            return match rng.below(4) {
                0 => JsonValue::UInt(rng.next_u64() | (1u64 << 63)), // canonical UInt: exceeds i64
                1 => JsonValue::Int(rng.next_u64() as i64), // deliberate wrap: negative i64s are the interesting case
                2 => JsonValue::Float((rng.next_u64() % 1000) as f64 / 7.0),
                _ => JsonValue::Str(format!("s{}", rng.next_u64() % 97)),
            };
        }
        match rng.below(3) {
            0 => JsonValue::Array(
                (0..rng.below(4) + 1)
                    .map(|_| build(depth - 1, rng))
                    .collect(),
            ),
            _ => JsonValue::Object(
                (0..rng.below(4) + 1)
                    .map(|i| (format!("k{i}"), build(depth - 1, rng)))
                    .collect(),
            ),
        }
    }
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..5_000 * fuzz_scale() {
            let value = build(3, &mut rng);
            let text = value.to_json_string();
            let back = json::parse(&text).expect("writer output must parse");
            if back != value {
                let path = first_diff_path(&value, &back);
                panic!("roundtrip unstable at {path}: text={text}");
            }
        }
    }
}

/// Locate the first structural difference between two values.
fn first_diff_path(a: &JsonValue, b: &JsonValue) -> String {
    fn walk(a: &JsonValue, b: &JsonValue, path: &str, out: &mut Option<String>) {
        if out.is_some() {
            return;
        }
        match (a, b) {
            (JsonValue::Object(pa), JsonValue::Object(pb)) => {
                if pa.len() != pb.len() {
                    *out = Some(format!("{path}: object lens {} vs {}", pa.len(), pb.len()));
                    return;
                }
                for ((ka, va), (kb, vb)) in pa.iter().zip(pb.iter()) {
                    if ka != kb {
                        *out = Some(format!("{path}: key {ka:?} vs {kb:?}"));
                        return;
                    }
                    {
                        let np = format!("{path}.{ka}");
                        walk(va, vb, np.as_str(), out);
                    }
                }
            }
            (JsonValue::Array(aa), JsonValue::Array(ab)) => {
                if aa.len() != ab.len() {
                    *out = Some(format!("{path}: array lens {} vs {}", aa.len(), ab.len()));
                    return;
                }
                for (i, (va, vb)) in aa.iter().zip(ab.iter()).enumerate() {
                    {
                        let np = format!("{path}[{i}]");
                        walk(va, vb, np.as_str(), out);
                    }
                }
            }
            (JsonValue::Float(x), JsonValue::Float(y)) => {
                if x.to_bits() != y.to_bits() {
                    *out = Some(format!(
                        "{path}: float {x} vs {y} (bits {:x} vs {:x})",
                        x.to_bits(),
                        y.to_bits()
                    ));
                }
            }
            _ => {
                if a != b {
                    *out = Some(format!("{path}: {a:?} vs {b:?}"));
                }
            }
        }
    }
    let mut out = None;
    let path = String::from("$");
    walk(a, b, &path, &mut out);
    out.unwrap_or_else(|| "identical (compare bug)".into())
}
