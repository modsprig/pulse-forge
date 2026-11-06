//! PF-1305: contract fixtures — the machine-facing outputs (`capabilities`,
//! `report`) must keep their documented shape. The example files in
//! `tests/schemas/` are pinned: a key that disappears (or changes meaning)
//! breaks this test, forcing a deliberate schema-version bump instead of a
//! silent contract change for downstream integrations (agents, dashboards).

use std::process::Command;

use pf_types::json::{self, JsonValue};

fn exe() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_pulseforge"))
}

fn fixture(name: &str) -> JsonValue {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/schemas")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("contract fixture {name} missing: {e}"));
    json::parse(&text).unwrap_or_else(|e| panic!("fixture {name} is not JSON: {e}"))
}

/// The set of keys a downstream consumer may rely on. Additions are
/// backwards compatible; removals/renames are breaking.
fn assert_keys(actual: &JsonValue, expected: &JsonValue, prefix: &str) {
    let Some(expected_pairs) = expected.as_object_pairs() else {
        return;
    };
    for (key, expected_value) in expected_pairs {
        let path = format!("{prefix}.{key}");
        let actual_child = actual
            .get(key)
            .unwrap_or_else(|| panic!("contract break at {path}: key missing from output"));
        if expected_value.as_object_pairs().is_some() {
            assert_keys(actual_child, expected_value, &path);
        } // leaf: presence is the contract; values vary per run
    }
}

#[test]
fn capabilities_output_matches_pinned_contract() {
    let out = Command::new(exe())
        .arg("capabilities")
        .output()
        .expect("run capabilities");
    let actual = json::parse(String::from_utf8_lossy(&out.stdout).trim())
        .expect("capabilities output is JSON");
    let pinned = fixture("capabilities.example.json");
    assert_keys(&actual, &pinned, "$");
}

#[test]
fn report_output_matches_pinned_contract() {
    // Record a real (tiny) session, then re-render its report and check
    // the shape against the pinned fixture.
    let dir = std::env::temp_dir().join(format!("pf-contract-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let record = Command::new(exe())
        .args([
            "record",
            "--run",
            &exe().display().to_string(),
            "--duration-ms",
            "1200",
            "--interval-ms",
            "250",
            "--out",
            dir.to_str().unwrap(),
            "--",
            "bench-spin",
            "--ms",
            "1500",
        ])
        .output()
        .expect("run record");
    assert!(
        record.status.success(),
        "{}",
        String::from_utf8_lossy(&record.stderr)
    );

    let session = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "pfsession")
        })
        .expect("session dir");

    let out = Command::new(exe())
        .args(["report", "--session", session.to_str().unwrap()])
        .output()
        .expect("run report");
    assert!(out.status.success());
    let actual =
        json::parse(String::from_utf8_lossy(&out.stdout).trim()).expect("report output is JSON");
    let pinned = fixture("report.example.json");
    assert_keys(&actual, &pinned, "$");

    // The metric set itself is contractual: all 16 canonical metrics.
    let metrics = actual
        .get("metrics")
        .and_then(|m| m.as_object_pairs())
        .unwrap();
    assert_eq!(metrics.len(), 16, "canonical metric count changed");

    let _ = std::fs::remove_dir_all(&dir);
}
