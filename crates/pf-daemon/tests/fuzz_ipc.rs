//! Seeded-mutation fuzz tests for the daemon IPC frame codec and the
//! handshake gate (PF-1202 extension): the local socket peer is the
//! threat model (same-user processes), so every byte that arrives on the
//! transport is untrusted until framed, size-capped and parsed.
//!
//! Property under test: `read_frame` over mutated traffic returns a valid
//! JSON value or a TYPED error — never a panic, never an allocation beyond
//! the 4 MiB frame cap — and `accept_handshake` over mutated handshakes
//! answers Ok or a typed error. Dependency-free, same deterministic
//! xorshift method as the other fuzz suites.

#![allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]

use std::io::Cursor;

use pf_daemon::ipc::{
    MAX_FRAME_BYTES, PROTOCOL_VERSION, accept_handshake, read_frame, request, response_ok,
    write_frame,
};
use pf_types::json::JsonValue as J;

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

const SEEDS: [u64; 6] = [1, 2, 3, 5, 8, 13];

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

/// Mutate a byte buffer: bit flips, byte pokes, truncation, duplication,
/// splices, and hostile length-prefix pokes (the framing field itself).
fn mutate_bytes(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut out = base.to_vec();
    let rounds = 1 + rng.below(16);
    for _ in 0..rounds {
        if out.is_empty() {
            break;
        }
        match rng.below(6) {
            0 => {
                let i = rng.below(out.len());
                out[i] ^= 1 << rng.below(8);
            }
            1 => {
                let i = rng.below(out.len());
                out[i] = (rng.next_u64() & 0xFF) as u8;
            }
            2 => {
                let keep = rng.below(out.len());
                out.truncate(keep.max(1));
            }
            3 if out.len() < 4096 => {
                let s = rng.below(out.len());
                let e = (s + 1 + rng.below(64)).min(out.len());
                let dup: Vec<u8> = out[s..e].to_vec();
                out.extend_from_slice(&dup);
            }
            4 if out.len() > 8 => {
                let dst = rng.below(out.len() - 1);
                let src = rng.below(out.len());
                let n = (1 + rng.below(8)).min(out.len() - dst);
                for k in 0..n {
                    out[dst + k] = out[(src + k) % out.len()];
                }
            }
            // Poke the length prefix toward the cap: the reader must
            // answer with a typed size error or truncated-frame error,
            // never attempt a huge allocation.
            _ if out.len() >= 4 => {
                let v = match rng.below(3) {
                    0 => (MAX_FRAME_BYTES as u32) + rng.below(1024) as u32,
                    1 => u32::MAX,
                    _ => rng.next_u64() as u32,
                };
                out[0..4].copy_from_slice(&v.to_le_bytes());
            }
            _ => {}
        }
    }
    out
}

/// String-level mutation: bytes first, UTF-8 verified before use (parse
/// input is &str by contract).
fn mutate_text(rng: &mut Rng, base: &str) -> String {
    let mut out = base.as_bytes().to_vec();
    let rounds = 1 + rng.below(12);
    for _ in 0..rounds {
        if out.is_empty() {
            break;
        }
        match rng.below(4) {
            0 => {
                let i = rng.below(out.len());
                out[i] ^= 1 << rng.below(8);
            }
            1 => {
                let keep = rng.below(out.len());
                out.truncate(keep.max(1));
            }
            2 if out.len() < 8192 => {
                let s = rng.below(out.len());
                let e = (s + 1 + rng.below(32)).min(out.len());
                let dup: Vec<u8> = out[s..e].to_vec();
                out.extend_from_slice(&dup);
            }
            _ => {
                let i = rng.below(out.len());
                out.insert(i, b'{');
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| base.to_string())
}

fn sample_frames() -> Vec<J> {
    vec![
        J::Object(vec![
            ("v".into(), J::UInt(PROTOCOL_VERSION as u64)),
            ("client".into(), J::Str("fuzz".into())),
        ]),
        request(7, "list", J::Object(vec![("limit".into(), J::UInt(10))])),
        response_ok(
            7,
            J::Object(vec![(
                "rows".into(),
                J::Array((0..8).map(J::UInt).collect()),
            )]),
        ),
        J::Object(vec![
            ("op".into(), J::Str("start".into())),
            (
                "args".into(),
                J::Object(vec![("pid".into(), J::UInt(4242))]),
            ),
        ]),
    ]
}

#[test]
fn read_frame_survives_mutated_traffic() {
    // Encode valid frames (length prefix + JSON body) as the mutation
    // base: hostile peers poke exactly these bytes.
    let mut bases: Vec<Vec<u8>> = Vec::new();
    for f in sample_frames() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &f).expect("valid frame writes");
        bases.push(buf);
    }
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for base in &bases {
            for _ in 0..4_000 * fuzz_scale() {
                let mutated = mutate_bytes(&mut rng, base);
                let mut cur = Cursor::new(mutated);
                // Typed error or a parsed value — never a panic, never an
                // allocation beyond the cap (the reader enforces it).
                let _ = read_frame(&mut cur);
            }
        }
    }
}

#[test]
fn read_frame_survives_random_soups() {
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..8_000 * fuzz_scale() {
            let len = 1 + rng.below(256);
            let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            let mut cur = Cursor::new(bytes);
            let _ = read_frame(&mut cur);
        }
    }
}

#[test]
fn handshake_gate_survives_mutated_handshakes() {
    // The real client handshake shape (client_handshake's own output),
    // serialized — the mutation base must be protocol-true.
    let valid = pf_daemon::ipc::client_handshake("fuzz");
    let text = valid.to_json_string();
    // Sanity: the gate accepts its own handshake.
    assert!(
        accept_handshake(&valid).is_ok(),
        "gate accepts valid handshake"
    );
    let mut rng = Rng::new(42);
    for _ in 0..12_000 * fuzz_scale() {
        // Mutate the handshake TEXT (UTF-8-safe), re-parse, feed the
        // gate: Ok(reply) or a typed error — never a panic.
        let mutated = mutate_text(&mut rng, &text);
        if let Ok(v) = pf_types::json::parse(&mutated) {
            let _ = accept_handshake(&v);
        }
    }
    // Plain wrong-shaped documents answer with typed errors.
    let wrong = J::Object(vec![("v".into(), J::Str("not-a-number".into()))]);
    assert!(accept_handshake(&wrong).is_err());
    assert!(accept_handshake(&J::Null).is_err());
}
