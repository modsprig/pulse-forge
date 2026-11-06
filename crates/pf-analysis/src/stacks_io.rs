//! Session stack-artifact loaders shared by the daemon query ops (PF-905)
//! and the GUI stacks panel (PF-906). OS-neutral: `stacks.jsonl` is
//! produced by the Linux perf pipeline, but callers everywhere compile
//! and answer with a typed "no stacks" error when artifacts are absent.

use std::path::Path;

use pf_types::json::JsonValue;
use pf_types::{ErrorCode, PfError};

use crate::stacks::SampledStack;

/// Symbol-coverage quality of a session's stacks (PF-905 output fields).
/// `None` = the artifact does not say (unavailable is rendered as null,
/// never as zero).
#[derive(Clone, Copy, Debug, Default)]
pub struct CoverageQuality {
    pub frame_identity_coverage: Option<f64>,
    pub frames_resolved: Option<u64>,
    pub frames_total: Option<u64>,
}

/// Load a session's sampled stacks as outermost-first analysis samples.
/// The file is bounded by construction (the recorder caps persisted
/// samples); frames are callee-first on disk, flipped here.
pub fn load_session_stacks(dir: &Path) -> Result<Vec<SampledStack>, PfError> {
    Ok(load_session_stacks_timed(dir)?
        .into_iter()
        .map(|t| t.stack)
        .collect())
}

/// One sampled stack with its perf timestamp and thread id (PF-906
/// time + thread filtering). `tid == 0` means the line carried no tid
/// (legacy schema) and renders as "unknown" in thread filters.
#[derive(Clone, Debug)]
pub struct TimedStack {
    pub time_ns: u64,
    pub tid: u64,
    pub stack: SampledStack,
}

/// Like [`load_session_stacks`], keeping each sample's `time_ns` so views
/// can filter by time (PF-906). Lines lacking a timestamp are skipped
/// honestly (legacy schemas never carried one).
pub fn load_session_stacks_timed(dir: &Path) -> Result<Vec<TimedStack>, PfError> {
    let path = dir.join("stacks.jsonl");
    let text = std::fs::read_to_string(&path).map_err(|_| {
        PfError::new(
            ErrorCode::CapabilityUnavailable,
            "session has no sampled stacks (counters-only recording, or stacks \
             persistence failed)",
        )
    })?;
    let mut samples = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        // Fast path: the strict byte scanner accepts exactly the shape
        // persist_stacks emits today; anything else falls back to the
        // full JSON parser below. The scanner can only REJECT — never
        // misparse — so equivalence is structural, not hoped for.
        if let Some((time_ns, tid, frames)) = try_scan_line(line) {
            let mut frames = frames;
            frames.reverse();
            samples.push(TimedStack {
                time_ns,
                tid,
                stack: crate::stacks::SampledStack::from_resolved_owned(frames, 1),
            });
            continue;
        }
        let v = parse_line(line, &path)?;
        // One walk over the top-level object (frames + time_ns + tid —
        // three .get() scans per line was three walks).
        let mut frames_json: Option<&[JsonValue]> = None;
        let mut time_ns: Option<u64> = None;
        let mut tid = 0u64;
        if let JsonValue::Object(pairs) = &v {
            for (k, val) in pairs {
                match k.as_str() {
                    "frames" => frames_json = val.as_array(),
                    "time_ns" => time_ns = val.as_u64(),
                    "tid" => tid = val.as_u64().unwrap_or(0),
                    _ => {}
                }
            }
        }
        let Some(frames_json) = frames_json else {
            continue; // legacy line without resolved frames
        };
        let mut frames: Vec<crate::stacks::ResolvedInput> = Vec::with_capacity(frames_json.len());
        for fr in frames_json {
            // One walk per frame object: the extractor needs 5 of the ~7
            // fields — five .get() calls were five linear scans each.
            let (mut module, mut symbol, mut offset, mut file, mut line) = ("", "", 0u64, "", 0u64);
            if let JsonValue::Object(pairs) = fr {
                for (k, val) in pairs {
                    match k.as_str() {
                        "module" => module = val.as_str().unwrap_or(""),
                        "symbol" => symbol = val.as_str().unwrap_or(""),
                        "offset" => offset = val.as_u64().unwrap_or(0),
                        "file" => file = val.as_str().unwrap_or(""),
                        "line" => line = val.as_u64().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            frames.push((
                module.to_string(),
                symbol.to_string(),
                offset,
                if file.is_empty() {
                    None
                } else {
                    Some(file.to_string())
                },
                u32::try_from(line).ok().filter(|l| *l > 0),
            ));
        }
        // Disk order is callee-first (innermost first); analysis wants
        // outermost-first. Reverse in place and consume the tuples — no
        // per-frame re-clone and no per-line flip() clone.
        frames.reverse();
        let st = SampledStack::from_resolved_owned(frames, 1);
        // Lines without a timestamp are skipped honestly (legacy schemas
        // never carried one; the current writer always does).
        if let Some(time_ns) = time_ns {
            samples.push(TimedStack {
                time_ns,
                tid,
                stack: st,
            });
        }
    }
    Ok(samples)
}

/// Read the symbol-coverage quality fields from `stacks-summary.json`.
pub fn coverage_quality(dir: &Path) -> CoverageQuality {
    let Some(text) = std::fs::read_to_string(dir.join("stacks-summary.json")).ok() else {
        return CoverageQuality::default();
    };
    let Some(v) = pf_types::json::parse(text.trim()).ok() else {
        return CoverageQuality::default();
    };
    CoverageQuality {
        frame_identity_coverage: summary_u64(&v, "frames_resolved")
            .zip(summary_u64(&v, "frames_total"))
            .map(|(r, t)| if t == 0 { 0.0 } else { r as f64 / t as f64 }),
        frames_resolved: summary_u64(&v, "frames_resolved"),
        frames_total: summary_u64(&v, "frames_total"),
    }
}

fn summary_u64(v: &JsonValue, key: &str) -> Option<u64> {
    v.get(key).and_then(|x| x.as_u64())
}

fn parse_line(line: &str, path: &Path) -> Result<JsonValue, PfError> {
    pf_types::json::parse(line).map_err(|e| {
        PfError::new(
            ErrorCode::PartialData,
            format!("{} line unparseable: {e}", path.display()),
        )
    })
}

// --- Fast-path line scanner ------------------------------------------------
//
// `persist_stacks` (pf-daemon) emits one strict byte shape per line; the
// scanner below accepts EXACTLY that shape and returns None on any
// deviation, deferring to the full JSON parser. It is a pure accelerator:
// it can only reject, never misparse — equivalence to the fallback is
// structural and differential-fuzz-tested.

/// Scan a plain u64 (digits only, ≤ 19 digits, overflow-checked).
// Cursor arithmetic is bounded by the digit-loop length check.
#[allow(clippy::arithmetic_side_effects)]
fn scan_u64(b: &[u8], p: &mut usize) -> Option<u64> {
    let start = *p;
    while matches!(b.get(*p), Some(c) if c.is_ascii_digit()) {
        *p += 1;
    }
    if *p == start || *p - start > 20 {
        return None; // u64::MAX is 20 digits; overflow handled by checked math
    }
    // JSON number grammar: no multi-digit leading zeros (`0` alone is
    // valid; `0…` with more digits is not — the writer never emits it).
    if b[start] == b'0' && *p - start > 1 {
        return None;
    }
    let mut v: u64 = 0;
    for &c in &b[start..*p] {
        v = v.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(v)
}

/// Require the literal `s` at the cursor.
// Cursor arithmetic is bounded by the literal length check.
#[allow(clippy::arithmetic_side_effects)]
fn scan_lit(b: &[u8], p: &mut usize, s: &str) -> bool {
    if b.get(*p..p.saturating_add(s.len())) == Some(s.as_bytes()) {
        *p += s.len();
        true
    } else {
        false
    }
}

/// Scan an escape-free JSON string at the cursor (writer-emitted strings
/// carry no backslash unless the raw path needed escaping — those lines
/// reject to the fallback). The slice sits between ASCII delimiters in a
/// valid UTF-8 buffer, so it is codepoint-aligned valid UTF-8.
// Cursor arithmetic is bounded by byte-length checks on every read.
#[allow(clippy::arithmetic_side_effects)]
fn scan_str(b: &[u8], p: &mut usize) -> Option<String> {
    if b.get(*p) != Some(&b'"') {
        return None;
    }
    *p += 1;
    let start = *p;
    while let Some(&c) = b.get(*p) {
        if c == b'"' {
            let s = std::str::from_utf8(&b[start..*p]).ok()?;
            *p += 1;
            return Some(s.to_string());
        }
        if c == b'\\' || c < 0x20 {
            return None; // escapes: fall back to the full parser
        }
        *p += 1;
    }
    None
}

// Cursor arithmetic is bounded by the 4-byte null check.
#[allow(clippy::arithmetic_side_effects)]
fn scan_null(b: &[u8], p: &mut usize) -> bool {
    if b.get(*p..*p + 4) == Some(b"null") {
        *p += 4;
        true
    } else {
        false
    }
}

/// Parsed line tuple: (time_ns, tid, frames in disk order).
type ScannedLine = (u64, u64, Vec<crate::stacks::ResolvedInput>);

/// Scan the writer's line: `{"time_ns":<u64>,"tid":<u64>,"ips":[<u64>,…],
/// "frames":[{"module":<str>,"symbol":<str|null>,"offset":<u64>,
/// "inlined":<bool>,"file":<str|null>,"line":<u64|null>},…]}` with fixed
/// key order. Field semantics mirror the full parser exactly: empty file
/// → None, line ≤ 0 → None, `symbol: null` → empty symbol.
// Cursor arithmetic is bounded by literal/length checks at every step.
#[allow(clippy::arithmetic_side_effects)]
fn try_scan_line(line: &str) -> Option<ScannedLine> {
    let b = line.as_bytes();
    let mut p = 0usize;
    // Leading/trailing whitespace is tolerated (the loader's trim() filter
    // allows it and json::parse skips it).
    while matches!(b.get(p), Some(b' ') | Some(b'\t') | Some(b'\r')) {
        p += 1;
    }
    if !scan_lit(b, &mut p, "{\"time_ns\":") {
        return None;
    }
    let time_ns = scan_u64(b, &mut p)?;
    if !scan_lit(b, &mut p, ",\"tid\":") {
        return None;
    }
    let tid = scan_u64(b, &mut p)?;
    if !scan_lit(b, &mut p, ",\"ips\":[") {
        return None;
    }
    if b.get(p) == Some(&b']') {
        p += 1;
    } else {
        // Strict grammar: value (',' value)* — a per-byte loop would
        // accept `[1,,2]`, which the full parser rejects.
        loop {
            scan_u64(b, &mut p)?;
            match b.get(p).copied()? {
                b',' => p += 1,
                b']' => {
                    p += 1;
                    break;
                }
                _ => return None,
            }
        }
    }
    if !scan_lit(b, &mut p, ",\"frames\":[") {
        return None;
    }
    let mut frames: Vec<crate::stacks::ResolvedInput> = Vec::new();
    if b.get(p) == Some(&b']') {
        p += 1;
    } else {
        loop {
            if !scan_lit(b, &mut p, "{\"module\":") {
                return None;
            }
            let module = scan_str(b, &mut p)?;
            if !scan_lit(b, &mut p, ",\"symbol\":") {
                return None;
            }
            let symbol = if scan_null(b, &mut p) {
                String::new()
            } else {
                scan_str(b, &mut p)?
            };
            if !scan_lit(b, &mut p, ",\"offset\":") {
                return None;
            }
            let offset = scan_u64(b, &mut p)?;
            if !scan_lit(b, &mut p, ",\"inlined\":") {
                return None;
            }
            if b.get(p..p + 4) == Some(b"true") {
                p += 4;
            } else if b.get(p..p + 5) == Some(b"false") {
                p += 5;
            } else {
                return None;
            }
            if !scan_lit(b, &mut p, ",\"file\":") {
                return None;
            }
            let file = if scan_null(b, &mut p) {
                None
            } else {
                match scan_str(b, &mut p)? {
                    s if s.is_empty() => None, // fallback filters "" → None
                    s => Some(s),
                }
            };
            if !scan_lit(b, &mut p, ",\"line\":") {
                return None;
            }
            let line_no = if scan_null(b, &mut p) {
                None
            } else {
                u32::try_from(scan_u64(b, &mut p)?).ok().filter(|l| *l > 0)
            };
            if !scan_lit(b, &mut p, "}") {
                return None;
            }
            frames.push((module, symbol, offset, file, line_no));
            match b.get(p).copied()? {
                b',' => p += 1,
                b']' => {
                    p += 1;
                    break;
                }
                _ => return None,
            }
        }
    }
    if !scan_lit(b, &mut p, "}") {
        return None;
    }
    while matches!(b.get(p), Some(b' ') | Some(b'\t') | Some(b'\r')) {
        p += 1;
    }
    if p != b.len() {
        return None; // trailing content: full parser decides (typed error)
    }
    Some((time_ns, tid, frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full parser's per-line result, isolated from the loader (the
    /// same extraction the fallback path performs).
    fn full_parse(line: &str) -> Option<ScannedLine> {
        let v = pf_types::json::parse(line).ok()?;
        let mut frames_json: Option<&[JsonValue]> = None;
        let mut time_ns: Option<u64> = None;
        let mut tid = 0u64;
        if let JsonValue::Object(pairs) = &v {
            for (k, val) in pairs {
                match k.as_str() {
                    "frames" => frames_json = val.as_array(),
                    "time_ns" => time_ns = val.as_u64(),
                    "tid" => tid = val.as_u64().unwrap_or(0),
                    _ => {}
                }
            }
        }
        let frames_json = frames_json?;
        let time_ns = time_ns?;
        let mut frames = Vec::with_capacity(frames_json.len());
        for fr in frames_json {
            let (mut module, mut symbol, mut offset, mut file, mut line) = ("", "", 0u64, "", 0u64);
            if let JsonValue::Object(pairs) = fr {
                for (k, val) in pairs {
                    match k.as_str() {
                        "module" => module = val.as_str().unwrap_or(""),
                        "symbol" => symbol = val.as_str().unwrap_or(""),
                        "offset" => offset = val.as_u64().unwrap_or(0),
                        "file" => file = val.as_str().unwrap_or(""),
                        "line" => line = val.as_u64().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            frames.push((
                module.to_string(),
                symbol.to_string(),
                offset,
                if file.is_empty() {
                    None
                } else {
                    Some(file.to_string())
                },
                u32::try_from(line).ok().filter(|l| *l > 0),
            ));
        }
        Some((time_ns, tid, frames))
    }

    /// One fixture frame: (module, symbol, offset, inlined, file, line).
    type FixtureFrame<'a> = (&'a str, &'a str, u64, bool, Option<&'a str>, Option<u32>);

    /// One line exactly as the persist writer emits it (frames callee-first).
    fn writer_line(time_ns: u64, tid: u64, ips: &[u64], frames: &[FixtureFrame<'_>]) -> String {
        let ips_s: Vec<String> = ips.iter().map(|i| i.to_string()).collect();
        let fr: Vec<String> = frames
            .iter()
            .map(|(m, s, o, inl, f, l)| {
                let sym = if s.is_empty() { "null".to_string() } else { format!("\"{s}\"") };
                let (fj, lj) = match (f, l) {
                    (Some(f), Some(l)) if *l > 0 => (format!("\"{f}\""), l.to_string()),
                    _ => ("null".to_string(), "null".to_string()),
                };
                format!("{{\"module\":\"{m}\",\"symbol\":{sym},\"offset\":{o},\"inlined\":{inl},\"file\":{fj},\"line\":{lj}}}")
            })
            .collect();
        format!(
            "{{\"time_ns\":{time_ns},\"tid\":{tid},\"ips\":[{}],\"frames\":[{}]}}",
            ips_s.join(","),
            fr.join(",")
        )
    }

    #[test]
    fn scanner_matches_full_parser_on_writer_shapes() {
        let cases = [
            writer_line(
                1_000,
                42,
                &[0x1000, 0x2000],
                &[
                    (
                        "/usr/lib/liba.so",
                        "inner_fn",
                        16,
                        true,
                        Some("src/a.rs"),
                        Some(10),
                    ),
                    (
                        "/usr/lib/liba.so",
                        "outer_fn",
                        0,
                        false,
                        Some("src/a.rs"),
                        Some(22),
                    ),
                ],
            ),
            writer_line(2_000, 7, &[], &[]),
            // null symbol / null file / null line / zero line / empty file
            writer_line(
                3_000,
                9,
                &[1],
                &[
                    ("/usr/bin/app", "", 8, false, None, None),
                    ("/usr/bin/app", "f2", 16, true, Some(""), Some(0)),
                    ("/usr/bin/app", "f3", 24, true, Some("x.rs"), Some(5)),
                ],
            ),
            // unicode identifiers and long numbers
            writer_line(
                18_446_744_073_709_551_615,
                18_446_744_073_709_551_615,
                &[0xffff_ffff_ffff],
                &[(
                    "/usr/日本語/libß.so",
                    "∑→∑",
                    0xdead_beef_cafe,
                    false,
                    Some("∑.rs"),
                    Some(1),
                )],
            ),
        ];
        for line in &cases {
            let fast = try_scan_line(line);
            let full = full_parse(line);
            assert!(fast.is_some(), "scanner must accept writer shape: {line}");
            assert_eq!(fast, full, "differential mismatch on: {line}");
        }
    }

    #[test]
    fn scanner_rejects_deviations_and_fallback_still_decides() {
        let rejects = [
            // escaped string (writer escapes any path needing it)
            writer_line(1, 1, &[1], &[("/usr/a\\b.so", "f", 0, false, Some("x.rs"), Some(1))]),
            // different key order
            "{\"tid\":1,\"time_ns\":2,\"ips\":[],\"frames\":[]}".to_string(),
            // extra unknown key
            "{\"time_ns\":1,\"tid\":2,\"cpu\":0,\"ips\":[],\"frames\":[]}".to_string(),
            // negative / float numbers
            "{\"time_ns\":-1,\"tid\":2,\"ips\":[],\"frames\":[]}".to_string(),
            "{\"time_ns\":1.5,\"tid\":2,\"ips\":[],\"frames\":[]}".to_string(),
            // leading zero multi-digit is fine, but a lone '+' is not
            "{\"time_ns\":+1,\"tid\":2,\"ips\":[],\"frames\":[]}".to_string(),
            // string in ips
            "{\"time_ns\":1,\"tid\":2,\"ips\":[\"x\"],\"frames\":[]}".to_string(),
            // trailing garbage (the full parser answers with a typed error)
            "{\"time_ns\":1,\"tid\":2,\"ips\":[],\"frames\":[]} x".to_string(),
            // control byte inside a string
            "{\"time_ns\":1,\"tid\":2,\"ips\":[],\"frames\":[{\"module\":\"a\u{7}\",\"symbol\":\"s\",\"offset\":0,\"inlined\":false,\"file\":null,\"line\":null}]}".to_string(),
        ];
        for line in &rejects {
            assert!(
                try_scan_line(line).is_none(),
                "scanner must reject deviation: {line}"
            );
        }
        // The fallback (full parser) still rules on what it can: negative
        // time_ns fails the round trip → the loader errors; a reordered
        // but valid document loads via the fallback.
        assert!(full_parse("{\"tid\":1,\"time_ns\":2,\"ips\":[],\"frames\":[]}").is_some());
        assert!(pf_types::json::parse("{\"time_ns\":-1,\"tid\":2}").is_ok());
    }

    /// Differential fuzz: mutated writer lines — whenever the scanner
    /// ACCEPTS, its output must equal the full parser's.
    #[test]
    fn differential_fuzz_scanner_vs_full_parser() {
        struct Rng(u64);
        impl Rng {
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
        let mut rng = Rng(0x5eed | 1);
        for case in 0..30_000u64 {
            let line = writer_line(
                (case + 1) * 1000,
                4242 + case % 3,
                &[(0x400000 + case * 8), 0x500000 + case],
                &[
                    (
                        "/usr/lib/liba.so",
                        &format!("fn_{case}"),
                        case * 16,
                        true,
                        Some("src/a.rs"),
                        Some(u32::try_from((case % 300) + 1).unwrap_or(1)),
                    ),
                    ("/usr/lib/liba.so", "", case * 16, false, None, None),
                ],
            );
            let mutated: String = {
                let mut b = line.as_bytes().to_vec();
                let rounds = 1 + rng.below(8);
                for _ in 0..rounds {
                    if b.is_empty() {
                        break;
                    }
                    match rng.below(4) {
                        0 => {
                            let i = rng.below(b.len());
                            b[i] ^= 1 << rng.below(8);
                        }
                        1 => {
                            let keep = rng.below(b.len());
                            b.truncate(keep.max(1));
                        }
                        2 => {
                            let i = rng.below(b.len());
                            b[i] = *b"\"0,}:\\".get(rng.below(6)).unwrap_or(&b'x');
                        }
                        _ => {
                            let i = rng.below(b.len());
                            b.insert(i, b' ');
                        }
                    }
                }
                String::from_utf8(b).unwrap_or(line.clone())
            };
            let fast = try_scan_line(&mutated);
            let full = full_parse(&mutated);
            if let (Some(f), Some(g)) = (&fast, &full) {
                assert_eq!(f, g, "differential mismatch on accepted: {mutated}");
            } else if fast.is_some() && full.is_none() {
                panic!("scanner accepted a line the full parser rejects: {mutated}");
            }
            // Scanner rejects are always safe (fallback decides), and a
            // None from BOTH is fine (unparseable line → typed error).
        }
    }
}
