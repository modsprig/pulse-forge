//! Seeded-mutation fuzz tests for the metrics JSON layer (PF-1202
//! extension): `CounterSnapshot::from_json` decodes session files and
//! report payloads — hostile field types and values must answer Ok with a
//! faithful snapshot or a typed error, never a panic, and whatever it
//! accepts must be canonically stable (re-serialize → re-parse → equal).
//! Same deterministic xorshift method as the other suites.

#![allow(clippy::arithmetic_side_effects)]

use pf_types::CounterSnapshot;
use pf_types::json::{self, JsonValue as J};

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

fn valid_snapshot(i: u64) -> CounterSnapshot {
    CounterSnapshot {
        mono_ns: i * 250_000_000,
        cpu_user_ns: Some(i * 1_000_003),
        cpu_kernel_ns: Some(i * 500_007),
        resident_bytes: Some(48_000_000 + (i % 8_192) * 4_096),
        resident_peak_bytes: Some(64_000_000),
        private_commit_bytes: Some(32_000_000),
        private_commit_peak_bytes: Some(40_000_000),
        io_read_ops: Some(i * 3),
        io_write_ops: Some(i * 2),
        io_read_bytes: Some(i * 65_536),
        io_write_bytes: Some(i * 32_768),
        thread_count: Some(6),
        handle_count: Some(200),
        page_faults: Some(i * 41),
    }
}

/// String-level mutation (UTF-8-safe).
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
                out[i] = *b"\":,0-e.nu".get(rng.below(9)).unwrap_or(&b'x');
            }
            2 => {
                let keep = rng.below(out.len());
                out.truncate(keep.max(1));
            }
            3 if out.len() < 8192 => {
                let s = rng.below(out.len());
                let e = (s + 1 + rng.below(32)).min(out.len());
                let dup: Vec<u8> = out[s..e].to_vec();
                out.extend_from_slice(&dup);
            }
            _ => {
                // Field-name damage: wrong types land in from_json.
                for needle in ["\"resident_bytes\"", "\"mono_ns\"", "\"cpu_user_ns\""] {
                    if let Some(p) = find(out.as_slice(), needle.as_bytes()) {
                        let end = (p + needle.len()).min(out.len());
                        out.drain(p..end);
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

#[test]
fn snapshot_from_json_survives_mutations_and_stays_canonical() {
    // Sanity: a valid snapshot round-trips exactly (a dead seed proves
    // nothing).
    let base = valid_snapshot(7);
    let text = base.to_json().to_json_string();
    let v = json::parse(&text).expect("own output parses");
    let back = CounterSnapshot::from_json(&v).expect("own output decodes");
    assert_eq!(back, base);

    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..(6_000 * fuzz_scale() as u64) {
            let doc = valid_snapshot(case).to_json().to_json_string();
            let mutated = mutate_text(&mut rng, &doc);
            let Ok(v) = json::parse(&mutated) else {
                continue;
            };
            // Ok(snapshot) or typed error — never a panic.
            if let Ok(snap) = CounterSnapshot::from_json(&v) {
                // Canonical stability: whatever from_json accepts must
                // re-serialize, re-parse and re-decode to the SAME value.
                let reser = snap.to_json().to_json_string();
                let v2 = json::parse(&reser).expect("canonical form parses");
                let snap2 = CounterSnapshot::from_json(&v2).expect("canonical form decodes");
                assert_eq!(snap, snap2, "canonical roundtrip broken: {reser}");
            }
        }
    }
}

#[test]
fn snapshot_from_json_rejects_wrong_shapes_with_typed_errors() {
    let wrong: [J; 7] = [
        J::Null,
        J::UInt(5),
        J::Str("snapshot".into()),
        J::Array(vec![]),
        J::Object(vec![("mono_ns".into(), J::Str("not-a-number".into()))]),
        J::Object(vec![("mono_ns".into(), J::Float(-0.5))]),
        J::Object(vec![("mono_ns".into(), J::UInt(1))]), // missing nothing — this one is VALID; sanity below
    ];
    for (i, v) in wrong.iter().enumerate() {
        let r = CounterSnapshot::from_json(v);
        if i == 6 {
            assert!(
                r.is_ok(),
                "a mono_ns-only snapshot is valid (absent = null)"
            );
        } else if i == 5 {
            // Non-integer mono_ns is a typed error, not a panic or a guess.
            assert!(r.is_err(), "float mono_ns rejected");
        } else {
            assert!(r.is_err(), "shape {i} must be a typed error");
        }
    }
    // The valid mono_ns-only snapshot keeps absent counters honestly
    // absent (never zero).
    let s = CounterSnapshot::from_json(&wrong[6]).expect("valid");
    assert!(s.resident_bytes.is_none(), "absent counter stays absent");
}
