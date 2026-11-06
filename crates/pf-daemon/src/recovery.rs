//! Orphaned active-session recovery (roadmap PF-407).
//!
//! A daemon crash can leave sessions whose manifests claim a non-terminal
//! state (`RECORDING`/`STOPPING`/…). On the next daemon start we scan the
//! session root and rewrite those manifests to `RECOVERED_PARTIAL` with an
//! explicit warning. Sealed sample data is preserved as evidence; a stale
//! `.partial` file is evidence too and is never promoted silently.

use std::path::Path;

use pf_types::json::{self, JsonValue};
use pf_types::{ErrorCode, PfError, SessionState};

/// Scan `root` for `*.pfsession` manifests in non-terminal states and mark
/// them recovered. Returns the number of repaired sessions. Corrupt
/// manifests are reported as errors but never stop the scan.
#[allow(clippy::arithmetic_side_effects)] // counter bounded by directory size
pub fn mark_abandoned_sessions(root: &Path) -> Result<usize, PfError> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut repaired = 0usize;
    let entries = std::fs::read_dir(root).map_err(PfError::from)?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let is_session_dir = path.is_dir()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "pfsession");
        if !is_session_dir {
            continue;
        }
        let manifest_path = path.join("manifest.json");
        let text = match std::fs::read(&manifest_path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(t) => t,
                // Non-UTF8 (a torn multi-byte write is a real crash
                // leftover): same treatment as an unparseable manifest —
                // marker note, data kept as evidence. Silently skipping
                // would make a torn manifest indistinguishable from a
                // missing one.
                Err(_) => {
                    let _ = std::fs::write(
                        path.join("RECOVERY_NOTE.txt"),
                        "manifest.json was not valid UTF-8 at daemon recovery;                          data files kept as-is
",
                    );
                    repaired += 1;
                    continue;
                }
            },
            Err(_) => continue, // no manifest: nothing authoritative to repair
        };
        if text.len() > 4 * 1024 * 1024 {
            return Err(PfError::new(
                ErrorCode::ResourceExhausted,
                format!("manifest exceeds 4 MiB: {}", manifest_path.display()),
            ));
        }
        let Ok(mut manifest) = json::parse(&text) else {
            // Corrupt manifest: keep the file as evidence, write a marker.
            let _ = std::fs::write(
                path.join("RECOVERY_NOTE.txt"),
                "manifest.json was unreadable at daemon recovery; data files kept as-is\n",
            );
            repaired += 1;
            continue;
        };
        let is_active = {
            let state_str = manifest.get("state").and_then(|v| v.as_str());
            !matches!(
                state_str,
                Some("COMPLETED")
                    | Some("FAILED")
                    | Some("CANCELLED")
                    | Some("RECOVERED_PARTIAL")
                    | None
            )
        };
        if !is_active {
            continue;
        }
        let state_str = manifest
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN")
            .to_string();
        rewrite_recovered(&mut manifest);
        // Atomic rewrite via .partial.
        let tmp = manifest_path.with_extension("json.partial");
        if std::fs::write(&tmp, format!("{}\n", manifest.to_json_string())).is_ok() {
            let _ = std::fs::rename(&tmp, &manifest_path);
            let _ = std::fs::write(
                path.join("RECOVERY_NOTE.txt"),
                format!(
                    "daemon restarted while this session was {state_str}; \
                     sealed samples preserved and state rewritten to RECOVERED_PARTIAL\n"
                ),
            );
            repaired += 1;
        }
    }
    Ok(repaired)
}

fn rewrite_recovered(manifest: &mut JsonValue) {
    if let Some(pairs) = manifest.as_object_pairs_mut() {
        for (k, v) in pairs.iter_mut() {
            if k == "state" {
                *v = JsonValue::Str(SessionState::RecoveredPartial.as_str().into());
            }
        }
        let warnings = pairs
            .iter_mut()
            .find(|(k, _)| k == "warnings")
            .map(|(_, v)| v);
        if let Some(JsonValue::Array(items)) = warnings {
            items.push(JsonValue::Str(
                "RECOVERED_PARTIAL: the owning daemon exited before finalization; \
                 data may end mid-recording and no duration guarantees apply"
                    .into(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(root: &Path, id: &str, state: &str) -> std::path::PathBuf {
        let dir = root.join(format!("{id}.pfsession"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(r#"{{"schema":"pulseforge.session/0","session_id":"{id}","state":"{state}","warnings":[]}}"#),
        )
        .unwrap();
        std::fs::write(dir.join("samples.jsonl"), "{\"mono_ns\":1}\n").unwrap();
        dir
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pf-recovery-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn recording_sessions_are_marked_recovered_partial() {
        let root = temp_root("active");
        make_session(&root, "01", "RECORDING");
        make_session(&root, "02", "STOPPING");
        let n = mark_abandoned_sessions(&root).unwrap();
        assert_eq!(n, 2);
        let text = std::fs::read_to_string(root.join("01.pfsession/manifest.json")).unwrap();
        let v = json::parse(&text).unwrap();
        assert_eq!(
            v.get("state").and_then(|s| s.as_str()),
            Some("RECOVERED_PARTIAL")
        );
        let warns = v.get("warnings").and_then(|w| w.as_array()).unwrap();
        assert!(
            warns
                .iter()
                .any(|w| w.as_str().unwrap_or("").contains("RECOVERED_PARTIAL"))
        );
        assert!(
            root.join("01.pfsession/samples.jsonl").is_file(),
            "data kept"
        );
        assert!(root.join("01.pfsession/RECOVERY_NOTE.txt").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn terminal_sessions_are_untouched() {
        let root = temp_root("terminal");
        make_session(&root, "03", "COMPLETED");
        make_session(&root, "04", "FAILED");
        let n = mark_abandoned_sessions(&root).unwrap();
        assert_eq!(n, 0);
        let v =
            json::parse(&std::fs::read_to_string(root.join("03.pfsession/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(v.get("state").and_then(|s| s.as_str()), Some("COMPLETED"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_manifest_is_marked_without_destruction() {
        let root = temp_root("corrupt");
        let dir = root.join("05.pfsession");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), "{broken").unwrap();
        std::fs::write(dir.join("samples.jsonl"), "{}\n").unwrap();
        let n = mark_abandoned_sessions(&root).unwrap();
        assert_eq!(n, 1);
        assert!(dir.join("samples.jsonl").is_file(), "evidence kept");
        assert!(
            dir.join("manifest.json").is_file(),
            "original corrupt file kept"
        );
        assert!(dir.join("RECOVERY_NOTE.txt").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_root_is_zero() {
        assert_eq!(mark_abandoned_sessions(Path::new("Z:/none")).unwrap(), 0);
    }
}
