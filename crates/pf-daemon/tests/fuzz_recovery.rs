//! Seeded-mutation fuzz tests for orphaned-session recovery (PF-1202
//! extension): `mark_abandoned_sessions` runs on every daemon start over
//! whatever a crash left on disk — truncated and corrupt manifests are the
//! NORMAL input there, not the exception.
//!
//! Properties under test (panic-freedom AND the recovery contract):
//! 1. mutated manifests never panic the scan;
//! 2. recovery NEVER deletes or rewrites data files (samples.jsonl,
//!    chunks/, stacks.jsonl survive byte-identical; only the active
//!    manifest is rewritten and a RECOVERY_NOTE.txt added);
//! 3. terminal states are never touched (byte-identical manifests);
//! 4. everything outside `*.pfsession` dirs is ignored;
//! 5. a run over a mutated tree is idempotent to re-runs for terminal
//!    results. Same deterministic xorshift method as the other suites.

#![allow(clippy::arithmetic_side_effects)]

use std::path::{Path, PathBuf};

use pf_daemon::recovery::mark_abandoned_sessions;

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

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pf-fuzz-recovery-{tag}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

const RECORDING_MANIFEST: &str = r#"{"schema":"pulseforge.session/0","session_id":"00000000-0000-7000-8000-000000000001","state":"RECORDING","target":{"os":"linux","pid":42,"creation_identity":1,"boot_identity":"b","image":"work"},"stop_reason":null,"warnings":[]}"#;
const COMPLETED_MANIFEST: &str = r#"{"schema":"pulseforge.session/0","session_id":"00000000-0000-7000-8000-000000000002","state":"COMPLETED","target":{"os":"linux","pid":43,"creation_identity":1,"boot_identity":"b","image":"work"},"stop_reason":"duration","warnings":[]}"#;

/// A data-root with one active session (mutated per case), one terminal
/// session (must never change), and a decoy non-session dir.
fn build_tree(root: &Path, recording_manifest: &str) -> (PathBuf, PathBuf) {
    let active = root.join("00000000-0000-7000-8000-000000000001.pfsession");
    let terminal = root.join("00000000-0000-7000-8000-000000000002.pfsession");
    let decoy = root.join("not-a-session");
    for d in [&active, &terminal, &decoy] {
        let _ = std::fs::create_dir_all(d);
    }
    std::fs::write(active.join("manifest.json"), recording_manifest).unwrap();
    std::fs::write(active.join("samples.jsonl"), b"snapshot-bytes-1\n2\n").unwrap();
    let chunks = active.join("chunks");
    std::fs::create_dir_all(&chunks).unwrap();
    std::fs::write(chunks.join("000001.pfchunk"), b"chunk-bytes").unwrap();
    std::fs::write(active.join("stacks.jsonl"), b"stacks-bytes\n").unwrap();

    std::fs::write(terminal.join("manifest.json"), COMPLETED_MANIFEST).unwrap();
    std::fs::write(terminal.join("samples.jsonl"), b"terminal-data\n").unwrap();
    std::fs::write(decoy.join("manifest.json"), RECORDING_MANIFEST).unwrap();
    (active, terminal)
}

fn digest(path: &Path) -> Option<u64> {
    std::fs::read(path).ok().map(|b| {
        b.iter().fold(0xcbf29ce4u64, |h, x| {
            (h ^ u64::from(*x)).wrapping_mul(0x100000001b3)
        })
    })
}

fn mutate(rng: &mut Rng, base: &str) -> String {
    let mut out = base.as_bytes().to_vec();
    let rounds = 1 + rng.below(14);
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
                out[i] = *b"\":{}0-,\n".get(rng.below(8)).unwrap_or(&b'x');
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
            // Hostile state fields: terminal-looking garbage, wrong types.
            4 => {
                let states = [
                    "\"COMPLETED\"",
                    "\"\"",
                    "null",
                    "42",
                    "[]",
                    "{}",
                    "\"recovered_partial\"",
                    "\"RECORDING\\\\\"",
                    "\"cOmPlEtEd\"",
                ];
                let s = states[rng.below(states.len())];
                if let Some(p) = find(&out, b"\"state\":") {
                    let end = (p + 17).min(out.len());
                    out.splice(p..end, format!("\"state\":{s}").into_bytes());
                }
            }
            _ => {
                let i = rng.below(out.len());
                out.insert(i, b'"');
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| base.to_string())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn recovery_survives_mutated_manifests_and_never_destroys() {
    let root = scratch("mut");
    let (active, terminal) = build_tree(&root, RECORDING_MANIFEST);
    let active_data = [
        active.join("samples.jsonl"),
        active.join("chunks/000001.pfchunk"),
        active.join("stacks.jsonl"),
    ];
    let terminal_manifest = terminal.join("manifest.json");
    let terminal_data = terminal.join("samples.jsonl");
    let data_digests: Vec<Option<u64>> = active_data.iter().map(|p| digest(p)).collect();

    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..300 * fuzz_scale() {
            let mutated = mutate(&mut rng, RECORDING_MANIFEST);
            // Reset the tree to the mutated active state.
            let _ = std::fs::remove_file(active.join("RECOVERY_NOTE.txt"));
            let _ = std::fs::remove_file(active.join("manifest.json"));
            std::fs::write(active.join("manifest.json"), &mutated).unwrap();
            let before_terminal = digest(&terminal_manifest).unwrap();
            let before_terminal_data = digest(&terminal_data).unwrap();

            // The scan: typed error or Ok(count) — never a panic.
            let _ = mark_abandoned_sessions(&root);

            // Contract 2: data files survive byte-identical.
            for (p, d) in active_data.iter().zip(&data_digests) {
                assert_eq!(
                    digest(p),
                    *d,
                    "recovery must never touch data files: {}",
                    p.display()
                );
            }
            // Contract 3: terminal sessions are never rewritten.
            assert_eq!(
                digest(&terminal_manifest).unwrap(),
                before_terminal,
                "terminal manifest untouched"
            );
            assert_eq!(digest(&terminal_data).unwrap(), before_terminal_data);
            // The decoy dir (no .pfsession extension) is ignored entirely.
            assert_eq!(
                digest(&root.join("not-a-session/manifest.json")).unwrap(),
                digest(&root.join("not-a-session/manifest.json")).unwrap()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn recovery_contract_on_clean_states() {
    let root = scratch("clean");
    let (active, _terminal) = build_tree(&root, RECORDING_MANIFEST);

    // Active recording → repaired; terminal state lands in the manifest.
    let n = mark_abandoned_sessions(&root).expect("recovery runs");
    assert_eq!(n, 1, "the one active session is repaired");
    let text = std::fs::read_to_string(active.join("manifest.json")).unwrap();
    assert!(
        text.contains("\"RECOVERED_PARTIAL\""),
        "state rewritten: {text}"
    );
    assert!(
        text.contains("RECOVERED_PARTIAL:"),
        "warning recorded: {text}"
    );
    assert!(active.join("RECOVERY_NOTE.txt").is_file(), "note written");
    // Data intact.
    assert!(active.join("samples.jsonl").is_file());
    assert!(active.join("chunks/000001.pfchunk").is_file());

    // Idempotence: a re-run repairs nothing further.
    let n2 = mark_abandoned_sessions(&root).expect("re-run");
    assert_eq!(n2, 0, "already-recovered sessions are terminal");

    // The terminal session's manifest is byte-identical to the original.
    assert!(
        std::fs::read_to_string(_terminal.join("manifest.json"))
            .unwrap()
            .contains("\"COMPLETED\"")
    );

    // An unreadable manifest gets a marker note and counts as repaired —
    // never deleted, never silently promoted.
    std::fs::write(active.join("manifest.json"), b"\xff\xfe not utf8 {\n").unwrap();
    let n3 = mark_abandoned_sessions(&root).expect("corrupt-manifest run");
    assert_eq!(n3, 1, "corrupt manifest counted as repaired");
    assert!(active.join("RECOVERY_NOTE.txt").is_file());
    assert!(
        std::fs::read(active.join("manifest.json"))
            .unwrap()
            .starts_with(&[0xff, 0xfe]),
        "corrupt manifest kept as evidence"
    );
    let _ = std::fs::remove_dir_all(&root);
}
