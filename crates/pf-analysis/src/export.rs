//! Speedscope export (roadmap PF-909): emit a session's sampled stacks as
//! the [speedscope file format](https://www.speedscope.app/file-format-schema.json)
//! so independent viewers can open PulseForge recordings.
//!
//! Honesty contract:
//! - a "sampled" profile with `unit: "none"` and weight 1 per sample —
//!   sample counts are never relabeled as elapsed time;
//! - stack order follows the format (bottom/root first), which is the
//!   flipped (outermost-first) order `stacks_io` already produces;
//! - unresolved frames keep their identity (`module+0xoff` / `<unresolved>`)
//!   instead of being silently dropped;
//! - input is bounded (`MAX_SAMPLES`); truncation is reported by the
//!   caller-visible [`ExportReport::truncated`], never hidden inside the
//!   emitted document.

use std::path::Path;

use pf_types::json::JsonValue;
use pf_types::{ErrorCode, PfError};

use crate::stacks::SampledStack;
use crate::stacks_io;

/// Hard cap on exported samples (the recorder persists at most 20 000).
pub const MAX_SAMPLES: usize = 100_000;

/// Bounded export result: the document plus the honesty counters a caller
/// must surface.
#[derive(Debug)]
pub struct ExportReport {
    /// The speedscope document (serialize with `to_json_string`).
    pub doc: JsonValue,
    /// Samples included in the export.
    pub samples_included: u64,
    /// Total samples present in the session.
    pub samples_total: u64,
    /// Unique frames interned into `shared.frames`.
    pub frames: u64,
    /// True when samples were dropped by the [`MAX_SAMPLES`] cap.
    pub truncated: bool,
}

/// Build the speedscope document for a session's stacks.
///
/// `profile_name` is human-facing (e.g. the session id + image);
/// `exporter` identifies the producing build (e.g. "pulseforge@0.5.0").
pub fn speedscope_export(
    dir: &Path,
    profile_name: &str,
    exporter: &str,
) -> Result<ExportReport, PfError> {
    let samples_all: Vec<SampledStack> = stacks_io::load_session_stacks(dir)?;
    if samples_all.is_empty() {
        return Err(PfError::new(
            ErrorCode::PartialData,
            "session carries no aggregatable stack samples",
        ));
    }
    let samples_total = samples_all.len();
    let truncated = samples_total > MAX_SAMPLES;
    let samples: &[SampledStack] = if truncated {
        &samples_all[..MAX_SAMPLES]
    } else {
        &samples_all
    };

    // Intern frame names in first-seen order (deterministic).
    let mut names: Vec<String> = Vec::new();
    let mut index: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    // Frame indices are bounded by the unique-name count; u32 covers the
    // hard caps by construction (MAX_SAMPLES x stacks is far below 2^32).
    #[allow(clippy::cast_possible_truncation)]
    fn intern(
        name: &str,
        names: &mut Vec<String>,
        index: &mut std::collections::HashMap<String, u32>,
    ) -> u32 {
        if let Some(&i) = index.get(name) {
            return i;
        }
        let i = names.len() as u32;
        names.push(name.to_string());
        index.insert(name.to_string(), i);
        i
    }
    let mut out_samples: Vec<JsonValue> = Vec::with_capacity(samples.len());
    let mut out_weights: Vec<JsonValue> = Vec::with_capacity(samples.len());
    let mut total_weight = 0u64;
    for s in samples {
        // speedscope sampled stacks run bottom (root) → top (leaf);
        // stacks_io yields outermost-first, which is exactly that order.
        let mut stack: Vec<JsonValue> = Vec::with_capacity(s.frames.len());
        for f in &s.frames {
            let i = intern(&f.key, &mut names, &mut index);
            stack.push(JsonValue::UInt(u64::from(i)));
        }
        total_weight = total_weight.saturating_add(s.weight);
        out_samples.push(JsonValue::Array(stack));
        out_weights.push(JsonValue::UInt(s.weight));
    }

    let shared_frames: Vec<JsonValue> = names
        .iter()
        .map(|n| JsonValue::Object(vec![("name".to_string(), JsonValue::Str(n.clone()))]))
        .collect();

    let doc = JsonValue::Object(vec![
        (
            "$schema".to_string(),
            JsonValue::Str("https://www.speedscope.app/file-format-schema.json".into()),
        ),
        (
            "shared".to_string(),
            JsonValue::Object(vec![(
                "frames".to_string(),
                JsonValue::Array(shared_frames),
            )]),
        ),
        (
            "profiles".to_string(),
            JsonValue::Array(vec![JsonValue::Object(vec![
                ("type".to_string(), JsonValue::Str("sampled".into())),
                ("name".to_string(), JsonValue::Str(profile_name.to_string())),
                ("unit".to_string(), JsonValue::Str("none".into())),
                ("startValue".to_string(), JsonValue::UInt(0)),
                ("endValue".to_string(), JsonValue::UInt(total_weight)),
                ("samples".to_string(), JsonValue::Array(out_samples)),
                ("weights".to_string(), JsonValue::Array(out_weights)),
            ])]),
        ),
        ("activeProfileIndex".to_string(), JsonValue::UInt(0)),
        ("exporter".to_string(), JsonValue::Str(exporter.to_string())),
        ("name".to_string(), JsonValue::Str(profile_name.to_string())),
    ]);

    Ok(ExportReport {
        frames: names.len() as u64,
        samples_included: samples.len() as u64,
        samples_total: samples_total as u64,
        truncated,
        doc,
    })
}

/// Frame name policy shared with the CLI/tests: symbol → module+0xoff →
/// `<unresolved>` (mirrors `SampledStack::from_resolved`).
pub fn frame_display_name(module: &str, symbol: &str, offset: u64) -> String {
    if !symbol.is_empty() {
        symbol.to_string()
    } else if !module.is_empty() {
        format!("{module}+0x{offset:x}")
    } else {
        "<unresolved>".to_string()
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    fn write_synthetic_session(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let frame = |m: &str, s: &str, o: u64| {
            format!(
                r#"{{"module":"{m}","symbol":{},"offset":{o}}}"#,
                if s.is_empty() {
                    "null".to_string()
                } else {
                    format!(r#""{s}""#)
                }
            )
        };
        let line = |frames: &[String]| {
            format!(
                r#"{{"time_ns":1,"tid":7,"ips":[1],"frames":[{}]}}"#,
                frames.join(",")
            )
        };
        let hot = frame("app", "hot_fn", 0x30);
        let main = frame("app", "main", 0x10);
        let worker = frame("app", "worker", 0x20);
        let unresolved = frame("libx.so", "", 0x60);
        // Daemon convention: frames are persisted INNERMOST-first (the
        // leaf IP first); the exporter flips to bottom-first.
        let mut lines = Vec::new();
        for _ in 0..3 {
            lines.push(line(&[hot.clone(), worker.clone(), main.clone()]));
        }
        lines.push(line(&[hot.clone(), main.clone()]));
        lines.push(line(&[unresolved, main.clone()]));
        std::fs::write(dir.join("stacks.jsonl"), lines.join("\n")).unwrap();
    }

    /// Spec-conformance checklist for a speedscope "sampled" profile,
    /// encoded as one validator so both unit tests and e2e share it.
    pub fn assert_valid_speedscope(doc: &JsonValue) -> (usize, usize, u64) {
        let obj = doc.as_object_pairs().expect("top-level object");
        // Required top-level keys (file format v0.0.1+).
        for key in ["$schema", "shared", "profiles"] {
            assert!(
                obj.iter().any(|(k, _)| k == key),
                "missing top-level key {key}"
            );
        }
        assert!(
            doc.get("$schema")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.ends_with("file-format-schema.json")),
            "$schema must point at the speedscope schema"
        );
        // shared.frames: every frame is an object with a string "name".
        let frames = doc
            .get("shared")
            .and_then(|s| s.get("frames"))
            .and_then(|f| f.as_array())
            .expect("shared.frames array");
        for f in frames {
            assert!(f.get("name").and_then(|n| n.as_str()).is_some(), "{f:?}");
        }
        // profiles: sampled profile invariants.
        let profiles = doc
            .get("profiles")
            .and_then(|p| p.as_array())
            .expect("profiles array");
        assert!(!profiles.is_empty(), "at least one profile");
        let mut total_weight = 0u64;
        for p in profiles {
            assert_eq!(p.get("type").and_then(|v| v.as_str()), Some("sampled"));
            assert!(p.get("name").and_then(|v| v.as_str()).is_some());
            assert_eq!(p.get("unit").and_then(|v| v.as_str()), Some("none"));
            assert_eq!(p.get("startValue").and_then(|v| v.as_u64()), Some(0));
            let end = p
                .get("endValue")
                .and_then(|v| v.as_u64())
                .expect("endValue");
            let samples = p
                .get("samples")
                .and_then(|v| v.as_array())
                .expect("samples");
            let weights = p
                .get("weights")
                .and_then(|v| v.as_array())
                .expect("weights");
            assert_eq!(samples.len(), weights.len(), "one weight per sample");
            let mut sum = 0u64;
            for (s, w) in samples.iter().zip(weights.iter()) {
                let idxs = s.as_array().expect("sample is a frame index array");
                assert!(!idxs.is_empty(), "samples are non-empty stacks");
                for i in idxs {
                    let idx = i.as_u64().expect("frame index is an integer");
                    assert!(
                        (idx as usize) < frames.len(),
                        "frame index {idx} out of range ({})",
                        frames.len()
                    );
                }
                sum = sum.saturating_add(w.as_u64().expect("weight is an integer"));
            }
            assert_eq!(end, sum, "endValue must equal the summed weights");
            total_weight = total_weight.saturating_add(sum);
        }
        (profiles.len(), frames.len(), total_weight)
    }

    #[test]
    fn export_is_spec_conformant_and_honest() {
        let dir = std::env::temp_dir().join(format!("pf-export-{}", std::process::id()));
        write_synthetic_session(&dir);
        let report = speedscope_export(&dir, "synthetic session", "pulseforge@test").unwrap();
        assert_eq!(report.samples_included, 5);
        assert_eq!(report.samples_total, 5);
        assert!(!report.truncated);
        // 4 unique frame names: main, worker, hot_fn, libx.so+0x60.
        assert_eq!(report.frames, 4);
        let (_, _, total_weight) = assert_valid_speedscope(&report.doc);
        assert_eq!(total_weight, 5, "weight 1 per sample, unit none");
        // Stack order: bottom-first — main precedes hot_fn in every sample.
        let samples = report
            .doc
            .get("profiles")
            .and_then(|p| p.as_array())
            .and_then(|p| p.first())
            .and_then(|p| p.get("samples"))
            .and_then(|s| s.as_array())
            .unwrap();
        let first = samples[0].as_array().unwrap();
        let name_of = |i: u64| {
            report
                .doc
                .get("shared")
                .and_then(|s| s.get("frames"))
                .and_then(|f| f.as_array())
                .unwrap()[i as usize]
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap()
                .to_string()
        };
        assert_eq!(name_of(first[0].as_u64().unwrap()), "main");
        assert_eq!(name_of(first[2].as_u64().unwrap()), "hot_fn");
        // Unresolved frame keeps its honest identity.
        let last = samples[samples.len() - 1].as_array().unwrap();
        assert_eq!(name_of(last[1].as_u64().unwrap()), "libx.so+0x60");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_stacks_is_a_typed_error_not_zeros() {
        let dir = std::env::temp_dir().join(format!("pf-export-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = speedscope_export(&dir, "x", "pulseforge@test").unwrap_err();
        assert_eq!(err.code, ErrorCode::CapabilityUnavailable);
        // A file that exists but yields no aggregatable lines is PartialData.
        std::fs::write(dir.join("stacks.jsonl"), "").unwrap();
        let err = speedscope_export(&dir, "x", "pulseforge@test").unwrap_err();
        assert_eq!(err.code, ErrorCode::PartialData);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------
// pprof export (PF-909 pprof slice): the profile.proto wire format, encoded
// by hand (zero-dep). Honesty contract mirrors the speedscope slice:
// sample counts are counts (sample_type "samples"/"count"), unresolved
// frames keep their module+offset identity, truncation is reported.
// Independent-viewer round-trip (go tool pprof) is NOT VERIFIED until a
// host provides the tool — the emitted structure is validated by a
// wire-format decoder that walks the schema field-by-field.
// ---------------------------------------------------------------------------

/// Minimal protobuf writer: varints, tags, length-delimited fields.
pub mod pb {
    /// ULEB128 varint.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn varint(buf: &mut Vec<u8>, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                buf.push(byte);
                return;
            }
            buf.push(byte | 0x80);
        }
    }

    /// Field tag (field number << 3 | wire type).
    pub fn tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
        varint(buf, ((field as u64) << 3) | u64::from(wire));
    }

    /// Wire type 0: varint field (int64/uint64/enum).
    pub fn uint64_field(buf: &mut Vec<u8>, field: u32, v: u64) {
        tag(buf, field, 0);
        varint(buf, v);
    }

    /// Wire type 2: length-delimited field (bytes/string/message).
    pub fn bytes_field(buf: &mut Vec<u8>, field: u32, payload: &[u8]) {
        tag(buf, field, 2);
        varint(buf, payload.len() as u64);
        buf.extend_from_slice(payload);
    }

    /// Encode a nested message into a fresh buffer and append it.
    pub fn nested<F: FnOnce(&mut Vec<u8>)>(buf: &mut Vec<u8>, field: u32, f: F) {
        let mut inner = Vec::new();
        f(&mut inner);
        bytes_field(buf, field, &inner);
    }
}

use std::collections::HashMap;

/// Hard cap keeping the encoder bounded.
pub const PPROF_MAX_LOCATIONS: usize = 200_000;

/// One decoded protobuf field: (field number, wire type, varint value,
/// payload for wire type 2).
pub type PbField = (u32, u8, u64, Vec<u8>);

/// Field numbers of profile.proto, used by both encoder and decoder tests.
pub mod pprof_fields {
    pub const PROFILE_SAMPLE_TYPE: u32 = 1;
    pub const PROFILE_SAMPLE: u32 = 2;
    pub const PROFILE_MAPPING: u32 = 3;
    pub const PROFILE_LOCATION: u32 = 4;
    pub const PROFILE_FUNCTION: u32 = 5;
    pub const PROFILE_STRING_TABLE: u32 = 6;
    pub const PROFILE_PERIOD_TYPE: u32 = 11;
    pub const PROFILE_PERIOD: u32 = 12;
    pub const VALUE_TYPE_TYPE: u32 = 1;
    pub const VALUE_TYPE_UNIT: u32 = 2;
    pub const SAMPLE_LOCATION_ID: u32 = 1;
    pub const SAMPLE_VALUE: u32 = 2;
    pub const LOCATION_ID: u32 = 1;
    pub const LOCATION_MAPPING_ID: u32 = 2;
    pub const LOCATION_ADDRESS: u32 = 3;
    pub const LOCATION_LINE: u32 = 4;
    pub const LINE_FUNCTION_ID: u32 = 1;
    pub const LINE_LINE: u32 = 2;
    pub const FUNCTION_ID: u32 = 1;
    pub const FUNCTION_NAME: u32 = 2;
    pub const FUNCTION_SYSTEM_NAME: u32 = 3;
    pub const FUNCTION_FILENAME: u32 = 4;
    pub const MAPPING_ID: u32 = 1;
    pub const MAPPING_FILENAME: u32 = 5;
}

/// Bounded pprof export result (binary document + honesty counters).
#[derive(Debug)]
pub struct PprofReport {
    /// The serialized `profile.proto` document.
    pub doc: Vec<u8>,
    pub samples_included: u64,
    pub samples_total: u64,
    pub locations: u64,
    pub functions: u64,
    /// True when samples were dropped by the [`MAX_SAMPLES`] cap.
    pub truncated: bool,
}

struct Intern {
    entries: Vec<String>,
    index: HashMap<String, u64>,
}

// Indices are bounded by the unique-string count.
#[allow(clippy::arithmetic_side_effects)]
impl Intern {
    fn new() -> Intern {
        Intern {
            entries: vec![String::new()], // string_table[0] must be ""
            index: HashMap::new(),
        }
    }
    fn intern(&mut self, s: &str) -> u64 {
        if let Some(&i) = self.index.get(s) {
            return i;
        }
        let i = self.entries.len() as u64;
        self.entries.push(s.to_string());
        self.index.insert(s.to_string(), i);
        i
    }
}

/// Build the pprof document for a session's sampled stacks.
// Line values come from u32 sources (never negative); the protobuf tag
// shift is the wire format itself.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn pprof_export(dir: &Path) -> Result<PprofReport, PfError> {
    let samples_all: Vec<SampledStack> = stacks_io::load_session_stacks(dir)?;
    if samples_all.is_empty() {
        return Err(PfError::new(
            ErrorCode::PartialData,
            "session carries no aggregatable stack samples",
        ));
    }
    let samples_total = samples_all.len();
    let truncated = samples_total > MAX_SAMPLES;
    let samples: &[SampledStack] = if truncated {
        &samples_all[..MAX_SAMPLES]
    } else {
        &samples_all
    };

    let mut strings = Intern::new();
    let mut function_ids: HashMap<String, u64> = HashMap::new();
    let mut functions: Vec<(u64, u64, u64)> = Vec::new(); // (id, name idx, file idx)
    let mut location_ids: HashMap<(u64, u64, u64, u64, u64), u64> = HashMap::new();
    let mut locations: Vec<(u64, u64, u64, u64, u64, u64)> = Vec::new(); // (id, mapping, fn, file, line, offset)
    let mut mapping_ids: HashMap<String, u64> = HashMap::new();
    let mut mappings: Vec<u64> = Vec::new(); // mapping filename indices

    let st_samples = strings.intern("samples");
    let st_count = strings.intern("count");

    // Samples: pprof expects leaf-FIRST location ids (leaf-to-root).
    let mut out_samples: Vec<(Vec<u64>, u64)> = Vec::with_capacity(samples.len());
    for s in samples {
        let mut ids = Vec::with_capacity(s.frames.len());
        for frame in s.frames.iter().rev() {
            let mapping = match mapping_ids.get(&frame.module) {
                Some(&id) => id,
                None => {
                    let id = mappings.len() as u64 + 1;
                    let fidx = strings.intern(&frame.module);
                    mappings.push(fidx);
                    mapping_ids.insert(frame.module.clone(), id);
                    id
                }
            };
            let fname = match function_ids.get(&frame.key) {
                Some(&id) => id,
                None => {
                    let id = functions.len() as u64 + 1;
                    let nidx = strings.intern(&frame.key);
                    let fidx = strings.intern(frame.file.as_deref().unwrap_or(""));
                    functions.push((id, nidx, fidx));
                    function_ids.insert(frame.key.clone(), id);
                    id
                }
            };
            let fidx = strings.intern(frame.file.as_deref().unwrap_or(""));
            let line = i64::from(frame.line.unwrap_or(0));
            // Location identity: (mapping, function, file, line). The
            // address is the frame's MODULE-RELATIVE offset — with the
            // mapping anchored at memory_start 0, go pprof resolves it
            // against the real module file (an independent symbolization
            // cross-check of our own attribution).
            let key = (mapping, fname, fidx, line as u64, frame.offset);
            let id = match location_ids.get(&key) {
                Some(&id) => id,
                None => {
                    let id = locations.len() as u64 + 1;
                    locations.push((id, key.0, key.1, key.2, key.3, key.4));
                    location_ids.insert(key, id);
                    id
                }
            };
            ids.push(id);
        }
        out_samples.push((ids, s.weight));
    }

    if locations.len() > PPROF_MAX_LOCATIONS {
        return Err(PfError::new(
            ErrorCode::ResourceExhausted,
            format!(
                "session exceeds the {}-location pprof cap",
                PPROF_MAX_LOCATIONS
            ),
        ));
    }

    // Serialize (deterministic table order).
    let mut buf = Vec::new();
    for entry in &strings.entries {
        pb::bytes_field(
            &mut buf,
            pprof_fields::PROFILE_STRING_TABLE,
            entry.as_bytes(),
        );
    }
    pb::nested(&mut buf, pprof_fields::PROFILE_SAMPLE_TYPE, |b| {
        pb::uint64_field(b, pprof_fields::VALUE_TYPE_TYPE, st_samples);
        pb::uint64_field(b, pprof_fields::VALUE_TYPE_UNIT, st_count);
    });
    for (i, fidx) in mappings.iter().enumerate() {
        let id = i as u64 + 1;
        pb::nested(&mut buf, pprof_fields::PROFILE_MAPPING, |b| {
            pb::uint64_field(b, pprof_fields::MAPPING_ID, id);
            pb::uint64_field(b, pprof_fields::MAPPING_FILENAME, *fidx);
        });
    }
    for (id, nidx, fidx) in &functions {
        pb::nested(&mut buf, pprof_fields::PROFILE_FUNCTION, |b| {
            pb::uint64_field(b, pprof_fields::FUNCTION_ID, *id);
            pb::uint64_field(b, pprof_fields::FUNCTION_NAME, *nidx);
            pb::uint64_field(b, pprof_fields::FUNCTION_SYSTEM_NAME, *nidx);
            if *fidx > 0 {
                pb::uint64_field(b, pprof_fields::FUNCTION_FILENAME, *fidx);
            }
        });
    }
    for (id, mapping, fid, _file_idx, line, offset) in &locations {
        pb::nested(&mut buf, pprof_fields::PROFILE_LOCATION, |b| {
            pb::uint64_field(b, pprof_fields::LOCATION_ID, *id);
            pb::uint64_field(b, pprof_fields::LOCATION_MAPPING_ID, *mapping);
            if *offset > 0 {
                pb::uint64_field(b, pprof_fields::LOCATION_ADDRESS, *offset);
            }
            pb::nested(b, pprof_fields::LOCATION_LINE, |lb| {
                pb::uint64_field(lb, pprof_fields::LINE_FUNCTION_ID, *fid);
                if *line > 0 {
                    pb::uint64_field(lb, pprof_fields::LINE_LINE, *line);
                }
            });
        });
    }
    for (ids, weight) in &out_samples {
        pb::nested(&mut buf, pprof_fields::PROFILE_SAMPLE, |b| {
            for id in ids {
                pb::uint64_field(b, pprof_fields::SAMPLE_LOCATION_ID, *id);
            }
            pb::uint64_field(b, pprof_fields::SAMPLE_VALUE, *weight);
        });
    }
    // period_type samples/count, period = 1: each sample is one sample —
    // never relabeled as elapsed time.
    pb::nested(&mut buf, pprof_fields::PROFILE_PERIOD_TYPE, |b| {
        pb::uint64_field(b, pprof_fields::VALUE_TYPE_TYPE, st_samples);
        pb::uint64_field(b, pprof_fields::VALUE_TYPE_UNIT, st_count);
    });
    pb::uint64_field(&mut buf, pprof_fields::PROFILE_PERIOD, 1);

    Ok(PprofReport {
        doc: buf,
        samples_included: samples.len() as u64,
        samples_total: samples_total as u64,
        locations: locations.len() as u64,
        functions: functions.len() as u64,
        truncated,
    })
}

/// Spec-conformance validation of a pprof document (PF-909): a wire-format
/// decoder that walks field tags/wire types exactly as profile.proto
/// declares them and checks referential integrity across the string,
/// function, location and sample tables. Used by tests AND available to
/// callers post-export.
#[derive(Debug)]
pub struct PprofValidation {
    pub string_table: Vec<String>,
    /// leaf function name → summed sample count.
    pub by_leaf: HashMap<String, u64>,
    pub samples: u64,
}

// Index comparisons are bounded by table lengths; tag shifts are the
// wire format.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn validate_pprof_document(doc: &[u8]) -> Result<PprofValidation, PfError> {
    struct PbReader<'a> {
        b: &'a [u8],
        pos: usize,
    }

    impl<'a> PbReader<'a> {
        fn new(b: &'a [u8]) -> PbReader<'a> {
            PbReader { b, pos: 0 }
        }
        fn varint(&mut self) -> Option<u64> {
            let mut out = 0u64;
            let mut shift = 0u32;
            loop {
                let byte = *self.b.get(self.pos)?;
                self.pos += 1;
                out |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return Some(out);
                }
                shift += 7;
                if shift > 63 {
                    return None;
                }
            }
        }
        fn field(&mut self) -> Option<PbField> {
            let tag = self.varint()?;
            let field = (tag >> 3) as u32;
            let wire = (tag & 7) as u8;
            match wire {
                0 => Some((field, wire, self.varint()?, Vec::new())),
                2 => {
                    let len = self.varint()? as usize;
                    let end = self.pos.checked_add(len)?;
                    let payload = self.b.get(self.pos..end)?.to_vec();
                    self.pos = end;
                    Some((field, wire, 0, payload))
                }
                _ => None,
            }
        }
        fn at_end(&self) -> bool {
            self.pos >= self.b.len()
        }
    }

    fn fields_of(payload: &[u8]) -> Result<Vec<PbField>, PfError> {
        let mut r = PbReader::new(payload);
        let mut out = Vec::new();
        while !r.at_end() {
            out.push(r.field().ok_or_else(|| {
                PfError::new(ErrorCode::PartialData, "malformed pprof wire format")
            })?);
        }
        Ok(out)
    }

    let mut strings: Vec<String> = Vec::new();
    let mut sample_type_seen = false;
    let mut samples: Vec<(Vec<u64>, u64)> = Vec::new();
    let mut locations: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut functions: HashMap<u64, u64> = HashMap::new();
    let mut period_type_seen = false;
    let mut period_value: Option<u64> = None;

    for (field, _wire, v, payload) in fields_of(doc)? {
        match field {
            pprof_fields::PROFILE_STRING_TABLE => {
                strings.push(
                    String::from_utf8(payload).map_err(|_| {
                        PfError::new(ErrorCode::PartialData, "pprof strings not utf8")
                    })?,
                );
            }
            pprof_fields::PROFILE_SAMPLE_TYPE => {
                let vt = fields_of(&payload)?;
                if vt.len() != 2 {
                    return Err(PfError::new(
                        ErrorCode::PartialData,
                        "ValueType needs type+unit",
                    ));
                }
                for (f, w, val, _) in &vt {
                    if *w != 0
                        || !matches!(
                            *f,
                            pprof_fields::VALUE_TYPE_TYPE | pprof_fields::VALUE_TYPE_UNIT
                        )
                        || (*val as usize) >= strings.len()
                    {
                        return Err(PfError::new(ErrorCode::PartialData, "malformed ValueType"));
                    }
                }
                sample_type_seen = true;
            }
            pprof_fields::PROFILE_SAMPLE => {
                let mut ids = Vec::new();
                let mut value = None;
                for (f, w, val, _) in fields_of(&payload)? {
                    match f {
                        pprof_fields::SAMPLE_LOCATION_ID if w == 0 => ids.push(val),
                        pprof_fields::SAMPLE_VALUE if w == 0 && val > 0 => value = Some(val),
                        _ => return Err(PfError::new(ErrorCode::PartialData, "malformed sample")),
                    }
                }
                if ids.is_empty() || value.is_none() {
                    return Err(PfError::new(
                        ErrorCode::PartialData,
                        "sample needs locations and a positive count",
                    ));
                }
                samples.push((ids, value.unwrap()));
            }
            pprof_fields::PROFILE_LOCATION => {
                let mut id = None;
                let mut lines = Vec::new();
                for (f, w, val, payload) in fields_of(&payload)? {
                    match f {
                        pprof_fields::LOCATION_ID => id = Some(val),
                        pprof_fields::LOCATION_MAPPING_ID => {
                            if val < 1 {
                                return Err(PfError::new(
                                    ErrorCode::PartialData,
                                    "mapping id must be >= 1",
                                ));
                            }
                        }
                        pprof_fields::LOCATION_LINE => {
                            if w != 2 {
                                return Err(PfError::new(
                                    ErrorCode::PartialData,
                                    "Line is a nested message",
                                ));
                            }
                            let mut fid = None;
                            for (lf, _lw, lval, _lp) in fields_of(&payload)? {
                                match lf {
                                    pprof_fields::LINE_FUNCTION_ID => fid = Some(lval),
                                    pprof_fields::LINE_LINE => {}
                                    _ => {
                                        return Err(PfError::new(
                                            ErrorCode::PartialData,
                                            "malformed Line",
                                        ));
                                    }
                                }
                            }
                            lines.push(fid.ok_or_else(|| {
                                PfError::new(ErrorCode::PartialData, "Line lacks function")
                            })?);
                        }
                        pprof_fields::LOCATION_ADDRESS => {} // 0 = unclaimed
                        _ => {
                            return Err(PfError::new(
                                ErrorCode::PartialData,
                                "unexpected location field",
                            ));
                        }
                    }
                }
                let id =
                    id.ok_or_else(|| PfError::new(ErrorCode::PartialData, "location lacks id"))?;
                if lines.is_empty() {
                    return Err(PfError::new(ErrorCode::PartialData, "location lacks lines"));
                }
                locations.insert(id, lines);
            }
            pprof_fields::PROFILE_FUNCTION => {
                let mut id = None;
                let mut name = None;
                for (f, _w, val, _) in fields_of(&payload)? {
                    match f {
                        pprof_fields::FUNCTION_ID => id = Some(val),
                        pprof_fields::FUNCTION_NAME => name = Some(val),
                        pprof_fields::FUNCTION_SYSTEM_NAME => {}
                        pprof_fields::FUNCTION_FILENAME => {}
                        _ => {
                            return Err(PfError::new(
                                ErrorCode::PartialData,
                                "unexpected function field",
                            ));
                        }
                    }
                }
                let (id, name) = (
                    id.ok_or_else(|| PfError::new(ErrorCode::PartialData, "function lacks id"))?,
                    name.ok_or_else(|| {
                        PfError::new(ErrorCode::PartialData, "function lacks name")
                    })?,
                );
                if (name as usize) >= strings.len() {
                    return Err(PfError::new(
                        ErrorCode::PartialData,
                        "function name index out of range",
                    ));
                }
                functions.insert(id, name);
            }
            pprof_fields::PROFILE_MAPPING => {
                let mut id = None;
                let mut file = None;
                for (f, _w, val, _) in fields_of(&payload)? {
                    match f {
                        pprof_fields::MAPPING_ID => id = Some(val),
                        pprof_fields::MAPPING_FILENAME => file = Some(val),
                        _ => {
                            return Err(PfError::new(
                                ErrorCode::PartialData,
                                "unexpected mapping field",
                            ));
                        }
                    }
                }
                if id.is_none() || file.is_none() || (file.unwrap() as usize) >= strings.len() {
                    return Err(PfError::new(ErrorCode::PartialData, "malformed mapping"));
                }
            }
            pprof_fields::PROFILE_PERIOD_TYPE => period_type_seen = true,
            pprof_fields::PROFILE_PERIOD => period_value = Some(v),
            _ => {
                return Err(PfError::new(
                    ErrorCode::PartialData,
                    "unexpected top-level profile field",
                ));
            }
        }
    }

    if strings.is_empty() || !strings[0].is_empty() {
        return Err(PfError::new(
            ErrorCode::PartialData,
            "string_table[0] must be empty",
        ));
    }
    if !sample_type_seen || !period_type_seen || period_value != Some(1) {
        return Err(PfError::new(
            ErrorCode::PartialData,
            "sample_type/period_type/period contract violated",
        ));
    }
    if samples.is_empty() {
        return Err(PfError::new(ErrorCode::PartialData, "no samples"));
    }
    for (ids, _value) in &samples {
        for id in ids {
            let lines = locations.get(id).ok_or_else(|| {
                PfError::new(ErrorCode::PartialData, "sample location unresolved")
            })?;
            for fid in lines {
                if !functions.contains_key(fid) {
                    return Err(PfError::new(
                        ErrorCode::PartialData,
                        "line function unresolved",
                    ));
                }
            }
        }
    }
    let mut by_leaf: HashMap<String, u64> = HashMap::new();
    let mut total = 0u64;
    for (ids, value) in &samples {
        let fid = locations[&ids[0]][0];
        let name = strings[functions[&fid] as usize].clone();
        *by_leaf.entry(name).or_insert(0) += *value;
        total += *value;
    }
    Ok(PprofValidation {
        string_table: strings,
        by_leaf,
        samples: total,
    })
}

#[cfg(test)]
mod pprof_tests {
    use super::*;

    fn write_synthetic_session(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let frame = |m: &str, s: &str, o: u64, file: &str, line: u32| {
            let sym = if s.is_empty() {
                "null".to_string()
            } else {
                format!(r#""{s}""#)
            };
            let f = if file.is_empty() {
                "null".to_string()
            } else {
                format!(r#""{file}""#)
            };
            format!(
                r#"{{"module":"{m}","symbol":{sym},"offset":{o},"inlined":false,"file":{f},"line":{line}}}"#
            )
        };
        let line = |frames: &[String]| {
            format!(
                r#"{{"time_ns":1,"tid":7,"ips":[1],"frames":[{}]}}"#,
                frames.join(",")
            )
        };
        // The daemon persists frames innermost-first.
        let mut lines = Vec::new();
        for _ in 0..3 {
            lines.push(line(&[
                frame("app", "hot_fn", 0x30, "app.rs", 12),
                frame("app", "worker", 0x20, "app.rs", 20),
                frame("app", "main", 0x10, "app.rs", 30),
            ]));
        }
        lines.push(line(&[
            frame("app", "hot_fn", 0x30, "app.rs", 12),
            frame("app", "main", 0x10, "app.rs", 30),
        ]));
        lines.push(line(&[
            frame("libx.so", "", 0x60, "", 0),
            frame("app", "main", 0x10, "app.rs", 30),
        ]));
        std::fs::write(dir.join("stacks.jsonl"), lines.join("\n")).unwrap();
    }

    #[test]
    fn pprof_document_is_spec_conformant_and_honest() {
        let dir = std::env::temp_dir().join(format!("pf-pprof-{}", std::process::id()));
        write_synthetic_session(&dir);
        let report = pprof_export(&dir).expect("export");
        assert_eq!(report.samples_included, 5);
        assert!(!report.truncated);
        let validation = validate_pprof_document(&report.doc).expect("spec conformance");
        let strings = validation.string_table;
        let by_leaf = validation.by_leaf;
        assert_eq!(by_leaf.get("hot_fn"), Some(&4), "{by_leaf:?}");
        // Leaves only: `main` is never a leaf in this fixture.
        assert_eq!(by_leaf.get("main"), None, "{by_leaf:?}");
        let total: u64 = by_leaf.values().sum();
        assert_eq!(total, 5, "one count per sample");
        // The unresolved frame keeps its module+offset identity.
        assert!(
            strings.iter().any(|s| s == "libx.so+0x60"),
            "unresolved frames keep module+offset: {strings:?}"
        );
        // The vocabulary and the count unit are present.
        assert!(strings.contains(&"samples".to_string()));
        assert!(strings.contains(&"count".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pprof_missing_stacks_is_a_typed_error() {
        let dir = std::env::temp_dir().join(format!("pf-pprof-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = pprof_export(&dir).unwrap_err();
        assert_eq!(err.code, ErrorCode::CapabilityUnavailable);
        std::fs::write(dir.join("stacks.jsonl"), "").unwrap();
        let err = pprof_export(&dir).unwrap_err();
        assert_eq!(err.code, ErrorCode::PartialData);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
