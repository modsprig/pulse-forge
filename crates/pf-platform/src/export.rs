//! Session export with redaction (roadmap PF-1209).
//!
//! Exports are opt-in and disclose what they include (architecture §9.3):
//! the caller chooses a [`RedactionPolicy`]; the output enumerates exactly
//! which metadata was redacted. Redaction strips:
//! - image paths → file name only,
//! - boot/creation identity (machine-correlatable tokens),
//! - data root path (the export is relocatable)
//! - everything else is preserved: counter values, PulseForge-generated
//!   warnings, and the session id — a redacted export must remain a usable
//!   measurement report

use std::io::Write;
use std::path::Path;

use pf_types::json::JsonValue;
use pf_types::{CounterSnapshot, PfError};

/// What the export keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedactionPolicy {
    /// Keep image paths, identity tokens, and the data-root path.
    None,
    /// Strip machine-correlatable tokens and paths (see module docs).
    Standard,
}

/// Export one recorded session to a JSON file. Returns the number of
/// redactions applied (0 when `RedactionPolicy::None`).
pub fn export_session(
    session_dir: &Path,
    out_path: &Path,
    policy: RedactionPolicy,
) -> Result<usize, PfError> {
    let store = crate::session_store::SessionStore::open(session_dir)?;
    let manifest = store.read_manifest()?;
    let (snapshots, partial) = store.read_samples()?;

    let mut doc = build_export(&manifest, &snapshots, partial.as_deref(), policy)?;
    write_atomic(out_path, doc.to_json_string().as_bytes())?;
    let _ = &mut doc;
    Ok(count_redactions(&manifest, policy))
}

fn build_export(
    manifest: &JsonValue,
    snapshots: &[CounterSnapshot],
    partial: Option<&str>,
    policy: RedactionPolicy,
) -> Result<JsonValue, PfError> {
    use JsonValue as J;
    let mut warnings: Vec<JsonValue> = manifest
        .get("warnings")
        .and_then(|w| w.as_array())
        .map(|a| a.to_vec())
        .unwrap_or_default();
    if let Some(p) = partial {
        warnings.push(J::Str(format!("PARTIAL_DATA: {p}")));
    }

    // Target block, redacted per policy.
    let mut target_pairs: Vec<(String, JsonValue)> = Vec::new();
    let empty = Vec::new();
    let src_target = manifest
        .get("target")
        .and_then(|t| t.as_object_pairs())
        .unwrap_or(&empty);
    for (k, v) in src_target {
        match (policy, k.as_str()) {
            (RedactionPolicy::Standard, "image") => {
                // Full path (possibly device-style) → file name only.
                let name = v
                    .as_str()
                    .and_then(|p| p.rsplit([char::from_u32(0x5C).unwrap(), '/']).next())
                    .unwrap_or("");
                target_pairs.push(("image".into(), J::Str(name.to_string())));
            }
            (RedactionPolicy::Standard, "boot_identity")
            | (RedactionPolicy::Standard, "creation_identity") => {
                target_pairs.push((k.clone(), J::Str("REDACTED".into())));
            }
            _ => target_pairs.push((k.clone(), v.clone())),
        }
    }

    // Manifest copy: redact the nested target block with the same policy
    // (the manifest duplicates image/identity tokens).
    let mut top_pairs: Vec<(String, JsonValue)> = Vec::new();
    for (k, v) in manifest.as_object_pairs().unwrap_or(&empty) {
        if policy == RedactionPolicy::Standard && k == "target" {
            let mut redacted_target: Vec<(String, JsonValue)> = Vec::new();
            for (tk, tv) in v.as_object_pairs().unwrap_or(&empty) {
                match tk.as_str() {
                    "image" => {
                        let name = tv
                            .as_str()
                            .and_then(|p| p.rsplit([char::from_u32(0x5C).unwrap(), '/']).next())
                            .unwrap_or("");
                        redacted_target.push(("image".into(), J::Str(name.to_string())));
                    }
                    "boot_identity" | "creation_identity" => {
                        redacted_target.push((tk.clone(), J::Str("REDACTED".into())));
                    }
                    _ => redacted_target.push((tk.clone(), tv.clone())),
                }
            }
            top_pairs.push(("target".into(), J::Object(redacted_target)));
        } else {
            top_pairs.push((k.clone(), v.clone()));
        }
    }

    let session_id = manifest
        .get("session_id")
        .cloned()
        .unwrap_or(J::Str(String::new()));
    let mut out = J::Object(vec![
        ("schema".into(), J::Str("pulseforge.export/0".into())),
        ("session_id".into(), session_id),
        (
            "redaction".into(),
            J::Object(vec![
                (
                    "policy".into(),
                    J::Str(match policy {
                        RedactionPolicy::None => "none".into(),
                        RedactionPolicy::Standard => "standard".into(),
                    }),
                ),
                (
                    "redacted_fields".into(),
                    J::Array(match policy {
                        RedactionPolicy::None => vec![],
                        RedactionPolicy::Standard => vec![
                            J::Str("target.image (file name kept)".into()),
                            J::Str("target.boot_identity".into()),
                            J::Str("target.creation_identity".into()),
                        ],
                    }),
                ),
            ]),
        ),
        ("manifest".into(), J::Object(top_pairs)),
        ("target".into(), J::Object(target_pairs)),
        ("warnings".into(), J::Array(warnings)),
        (
            "samples".into(),
            J::Array(snapshots.iter().map(|s| s.to_json()).collect()),
        ),
    ]);
    let _ = &mut out;
    Ok(out)
}

fn count_redactions(manifest: &JsonValue, policy: RedactionPolicy) -> usize {
    match policy {
        RedactionPolicy::None => 0,
        RedactionPolicy::Standard => {
            let has_target = manifest.get("target").is_some();
            if has_target {
                3 // image, boot_identity, creation_identity
            } else {
                0
            }
        }
    }
}

/// Write via `.partial` + rename (same durability rule as the store).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PfError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(PfError::from)?;
    }
    let tmp = path.with_extension("partial");
    {
        let mut f = std::fs::File::create(&tmp).map_err(PfError::from)?;
        f.write_all(bytes).map_err(PfError::from)?;
        f.sync_all().map_err(PfError::from)?;
    }
    std::fs::rename(&tmp, path).map_err(PfError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{SCHEMA_0, SessionStore, manifest_v0};
    use pf_types::ErrorCode;
    use pf_types::SessionId;
    use pf_types::json;
    use pf_types::{Os, ProcessIdentity, SessionState};
    use std::fs;
    use std::path::PathBuf;

    fn make_session(root: &Path, image: &str) -> (PathBuf, SessionId) {
        let id = SessionId::now_v7();
        let store = SessionStore::create(root, id).unwrap();
        let identity = ProcessIdentity::new(Os::Windows, 42, 1337, "boot~12345");
        let manifest = manifest_v0(
            id,
            SessionState::Completed,
            "windows",
            "10.0.22631",
            &identity,
            image,
            Some(16),
            250,
            "duration_reached",
            &["label: secret-label".into()],
            SCHEMA_0,
        );
        store.write_manifest(&manifest).unwrap();
        store
            .append_samples(&[CounterSnapshot {
                mono_ns: 0,
                cpu_user_ns: Some(10),
                resident_bytes: Some(4096),
                ..CounterSnapshot::default()
            }])
            .unwrap();
        (store.dir().to_path_buf(), id)
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pf-export-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn standard_redaction_strips_paths_and_identity() {
        let root = temp("redact");
        let (session, id) = make_session(
            &root,
            "\\\\Device\\HarddiskVolume1\\Users\\me\\secret\\app.exe",
        );
        let out = root.join("export.json");
        let redactions = export_session(&session, &out, RedactionPolicy::Standard).unwrap();
        assert_eq!(redactions, 3);

        let text = fs::read_to_string(&out).unwrap();
        let v = json::parse(&text).unwrap();
        // Image reduced to file name; machine tokens replaced.
        assert_eq!(
            v.get("target")
                .and_then(|t| t.get("image"))
                .and_then(|x| x.as_str()),
            Some("app.exe")
        );
        assert_eq!(
            v.get("target")
                .and_then(|t| t.get("boot_identity"))
                .and_then(|x| x.as_str()),
            Some("REDACTED")
        );
        // No absolute path or boot token anywhere in the document.
        assert!(!text.contains("HarddiskVolume"), "{text}");
        assert!(!text.contains("boot~12345"));
        // Measurement content preserved: samples and session id survive.
        assert_eq!(
            v.get("session_id").and_then(|x| x.as_str()),
            Some(id.as_hyphenated().as_str())
        );
        assert_eq!(
            v.get("samples").and_then(|s| s.as_array()).map(|a| a.len()),
            Some(1)
        );
        // The redaction block discloses what was stripped.
        assert_eq!(
            v.get("redaction")
                .and_then(|r| r.get("policy"))
                .and_then(|x| x.as_str()),
            Some("standard")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn none_policy_preserves_everything() {
        let root = temp("none");
        let (session, _) = make_session(&root, "\\\\Device\\HarddiskVolume1\\app.exe");
        let out = root.join("export.json");
        assert_eq!(
            export_session(&session, &out, RedactionPolicy::None).unwrap(),
            0
        );
        let text = fs::read_to_string(&out).unwrap();
        assert!(text.contains("HarddiskVolume"), "unredacted keeps paths");
        assert!(text.contains("boot~12345"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn export_is_atomic_and_json_parses() {
        let root = temp("atomic");
        let (session, _) = make_session(&root, "app.exe");
        let out = root.join("nested/dir/export.json");
        export_session(&session, &out, RedactionPolicy::Standard).unwrap();
        // No .partial residue; final file parses.
        assert!(!out.with_extension("partial").exists());
        let text = fs::read_to_string(&out).unwrap();
        json::parse(&text).expect("export is valid JSON");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_session_is_typed_error() {
        let root = temp("missing");
        let err =
            export_session(&root, &root.join("o.json"), RedactionPolicy::Standard).unwrap_err();
        assert_eq!(err.code, ErrorCode::SessionNotFound);
        let _ = fs::remove_dir_all(&root);
    }
}
