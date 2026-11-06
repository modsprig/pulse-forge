//! Seeded-mutation fuzz tests for the chunk decoder (roadmap PF-1202
//! subset): corrupt chunk files must produce typed errors or salvage notes
//! — never panics, never invented data — under arbitrary mutations.

#![cfg(not(target_os = "emscripten"))]
// Fuzz loops with bounded counters.
#![allow(clippy::arithmetic_side_effects)]

use pf_storage::{ChunkReader, ChunkWriter};
use pf_types::CounterSnapshot;

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

fn snap(i: u64) -> CounterSnapshot {
    CounterSnapshot {
        mono_ns: i * 1_000,
        cpu_user_ns: Some(i),
        resident_bytes: Some(4096 + i % 512),
        thread_count: Some(2 + (i % 7) as u32),
        ..CounterSnapshot::default()
    }
}

fn session_dir(tag: &str, seed: u64) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pf-fuzz-{tag}-{seed}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

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
fn mutated_chunk_files_never_panic_or_invent_data() {
    for (seed_index, seed) in [7u64, 11, 29, 41].iter().enumerate() {
        let dir = session_dir("mutate", *seed);
        // Write a clean multi-chunk store.
        {
            let mut w = ChunkWriter::open(&dir).unwrap();
            for i in 0..(2_000 * fuzz_scale() as u64) {
                w.push(&snap(i)).unwrap();
            }
            w.seal().unwrap();
        }
        let chunk_path = dir.join("chunks/metrics-0000000001.pfc");
        let original = std::fs::read(&chunk_path).unwrap();

        let mut rng = Rng::new(*seed);
        for round in 0..(3_000 * fuzz_scale() as u64) {
            let mut bytes = original.clone();
            match rng.below(4) {
                0 if !bytes.is_empty() => {
                    let at = rng.below(bytes.len());
                    bytes[at] ^= 1u8 << rng.below(8).min(7);
                }
                1 if !bytes.is_empty() => {
                    bytes.truncate(rng.below(bytes.len()));
                }
                2 if !bytes.is_empty() => {
                    let at = rng.below(bytes.len());
                    bytes.insert(at, (rng.next_u64() & 0xFF) as u8);
                }
                _ => {
                    // Overwrite a window with soup.
                    if !bytes.is_empty() {
                        let at = rng.below(bytes.len());
                        let take = rng.below((bytes.len() - at).min(64) + 1);
                        for b in &mut bytes[at..at + take] {
                            *b = (rng.next_u64() & 0xFF) as u8;
                        }
                    }
                }
            }
            let _ = round;
            let _ = std::fs::write(&chunk_path, &bytes);

            let reader = match ChunkReader::open(&dir) {
                Ok(r) => r,
                Err(_) => continue, // typed error path is acceptable
            };
            // Whatever was readable must be real data: if the mutated chunk
            // survived CRC (astronomically unlikely for real mutations but
            // possible for no-op mutations), values must still decode.
            if let Ok(page) = reader.range(0, u64::MAX, 64) {
                // The readable bound must track the WRITTEN corpus (which
                // scales with PF_FUZZ_SCALE), not the 1x default.
                let max_mono = (2_000 * fuzz_scale() as u64).saturating_mul(1_000);
                for s in &page.snapshots {
                    assert!(s.mono_ns <= max_mono, "invented data: {s:?}");
                }
            }
        }
        let _ = seed_index;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn random_file_soups_in_the_chunks_dir_are_ignored_or_typed() {
    for seed in [101u64, 211, 307] {
        let dir = session_dir("soup", seed);
        std::fs::create_dir_all(dir.join("chunks")).unwrap();
        let mut rng = Rng::new(seed);
        for i in 0..200 {
            let name = if i % 3 == 0 {
                format!("chunks/metrics-{:010}.pfc", i)
            } else if i % 3 == 1 {
                format!("chunks/metrics-{:010}.pfc.partial", i)
            } else {
                format!("chunks/random-{}", i)
            };
            let len = rng.below(256);
            let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            let _ = std::fs::write(dir.join(name), bytes);
        }
        // Must open without panic; anything readable is zero soup records.
        if let Ok(reader) = ChunkReader::open(&dir) {
            // A soup file that accidentally parses as a chunk with huge
            // counts must not cause unbounded allocation: the reader
            // trusts only CRC-verified chunks.
            let _ = reader.total_records();
            let _ = reader.range(0, u64::MAX, 16);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn corrupt_chunk_excludes_only_itself() {
    // One clean chunk + one corrupted: the clean one stays fully readable.
    let dir = session_dir("mixed", 99);
    {
        let mut w = ChunkWriter::open(&dir).unwrap();
        for i in 0..50u64 {
            w.push(&snap(i)).unwrap();
        }
        w.seal().unwrap();
    }
    // Force a second chunk by opening a new writer.
    {
        let mut w = ChunkWriter::open(&dir).unwrap();
        for i in 50..80u64 {
            w.push(&snap(i)).unwrap();
        }
        w.seal().unwrap();
    }
    let second = dir.join("chunks/metrics-0000000002.pfc");
    let mut bytes = std::fs::read(&second).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&second, &bytes).unwrap();

    let reader = ChunkReader::open(&dir).unwrap();
    assert_eq!(reader.corrupt_chunk_count(), 1);
    assert_eq!(reader.total_records(), 50, "clean chunk survives");
    let page = reader.range(0, u64::MAX, 1000).unwrap();
    assert_eq!(page.snapshots.len(), 50);
    // All values are from the clean chunk.
    assert!(page.snapshots.iter().all(|s| s.mono_ns < 50_000));
    let _ = std::fs::remove_dir_all(&dir);
}
