//! PF-1208: disk-schema compatibility — readers must accept older and
//! tolerate unknown/future encodings without reinterpretation, and reject
//! version mismatches with typed errors rather than guessing.

#![allow(clippy::arithmetic_side_effects)]

use std::fs;
use std::path::PathBuf;

use pf_storage::{ChunkReader, ChunkWriter};
// Test arithmetic on synthetic series bounded by explicit caps.

use pf_types::CounterSnapshot;

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-compat-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn snap(i: u64) -> CounterSnapshot {
    CounterSnapshot {
        mono_ns: i * 1_000_000,
        cpu_user_ns: Some(i * 100),
        resident_bytes: Some(4096 + i),
        thread_count: Some(3),
        ..CounterSnapshot::default()
    }
}

/// A schema-0 session written by "an older build": minimal manifest with
/// only the fields v0.1 wrote, samples as JSONL.
#[test]
fn original_schema0_jsonl_sessions_stay_readable() {
    let root = dir("v0-jsonl");
    let session = root.join("01a10000-0000-7000-8000-000000000001.pfsession");
    fs::create_dir_all(&session).unwrap();
    // Minimal v0.1 manifest (no logical_cpus, no report_path — fields the
    // current writer adds).
    fs::write(
        session.join("manifest.json"),
        r#"{"schema":"pulseforge.session/0","session_id":"01a10000-0000-7000-8000-000000000001","state":"COMPLETED","target":{"os":"windows","pid":42,"creation_identity":99,"boot_identity":"boot~1"},"interval_ms":250,"stop_reason":"duration_reached","warnings":[]}"#,
    )
    .unwrap();
    fs::write(
        session.join("samples.jsonl"),
        "{\"mono_ns\":0,\"cpu_user_ns\":0,\"thread_count\":3}\n{\"mono_ns\":1000000,\"cpu_user_ns\":100,\"resident_bytes\":4097}\n",
    )
    .unwrap();

    // The chunk reader finds nothing (this session predates chunks) — the
    // JSONL path must serve the data instead.
    let reader = ChunkReader::open(&session).unwrap();
    assert_eq!(reader.total_records(), 0);
    let store = pf_platform::session_store::SessionStore::open(&session).unwrap();
    let (samples, partial) = store.read_samples().unwrap();
    assert!(partial.is_none());
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[1].cpu_user_ns, Some(100));
    assert_eq!(samples[1].resident_bytes, Some(4097));
    let _ = fs::remove_dir_all(&root);
}

/// Unknown manifest fields (a hypothetical v0.2 extension) must be ignored
/// on read — never a hard failure, never reinterpretation of known fields.
#[test]
fn unknown_manifest_fields_are_tolerated() {
    let root = dir("unknown-fields");
    let session = root.join("01a10000-0000-7000-8000-000000000002.pfsession");
    fs::create_dir_all(&session).unwrap();
    fs::write(
        session.join("manifest.json"),
        r#"{"schema":"pulseforge.session/0","session_id":"01a10000-0000-7000-8000-000000000002","state":"COMPLETED","target":{"os":"windows","pid":7,"creation_identity":1,"boot_identity":"boot~1","some_future_field":{"x":1}},"interval_ms":250,"stop_reason":"duration_reached","warnings":[],"future_extension":true}"#,
    )
    .unwrap();
    let store = pf_platform::session_store::SessionStore::open(&session).unwrap();
    let manifest = store.read_manifest().unwrap();
    // Known fields intact.
    assert_eq!(
        manifest.get("state").and_then(|v| v.as_str()),
        Some("COMPLETED")
    );
    // Unknown field preserved in the parsed value (never dropped silently).
    assert_eq!(
        manifest.get("future_extension").and_then(|v| v.as_bool()),
        Some(true)
    );
    let _ = fs::remove_dir_all(&root);
}

/// A chunk whose format version is NEWER than this build must be rejected
/// with a typed salvage note — not skipped silently, not guessed at.
#[test]
fn newer_chunk_version_is_rejected_not_guessed() {
    let root = dir("newer-version");
    {
        let mut w = ChunkWriter::open(&root).unwrap();
        for i in 0..10u64 {
            w.push(&snap(i)).unwrap();
        }
        w.seal().unwrap();
    }
    let chunk = root.join("chunks/metrics-0000000001.pfc");
    let mut bytes = fs::read(&chunk).unwrap();
    // Patch format version 1 -> 99 (offset: magic 4 + version at 4..6).
    bytes[4] = 99;
    fs::write(&chunk, &bytes).unwrap();

    let reader = ChunkReader::open(&root).unwrap();
    assert_eq!(reader.corrupt_chunk_count(), 1);
    assert!(
        reader
            .salvage_notes()
            .iter()
            .any(|n| n.contains("unsupported chunk version 99")),
        "{:?}",
        reader.salvage_notes()
    );
    assert_eq!(reader.total_records(), 0, "unknown version is never parsed");
    let _ = fs::remove_dir_all(&root);
}

/// Current-build sessions keep reading their own output (self-compat) and
/// the schema constant stays pinned.
#[test]
fn current_schema_roundtrip_and_constant() {
    assert_eq!(
        pf_platform::session_store::SCHEMA_0.as_str(),
        "pulseforge.session/0"
    );
    let root = dir("self");
    {
        let mut w = ChunkWriter::open(&root).unwrap();
        for i in 0..5u64 {
            w.push(&snap(i)).unwrap();
        }
        w.seal().unwrap();
    }
    let r = ChunkReader::open(&root).unwrap();
    assert_eq!(r.total_records(), 5);
    assert_eq!(r.corrupt_chunk_count(), 0);
    let _ = fs::remove_dir_all(&root);
}
