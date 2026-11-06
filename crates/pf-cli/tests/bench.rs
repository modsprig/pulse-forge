//! Repeated-run bench integration test (PF-1101..1104/1108): two spec
//! files, real recorded repetitions, exact report shape, and a known
//! longer workload showing a larger wall median.

#![cfg(windows)]

use std::process::Command;

#[test]
fn bench_reports_exact_repeated_comparison_for_known_speedup() {
    // Workload A spins 1200 ms; workload B spins 2400 ms — the "known
    // speedup" fixture in reverse (B is the slower candidate). The report
    // must show candidate wall median ~2x baseline within tolerance.
    let dir = std::env::temp_dir().join(format!("pf-bench-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let spec_a = dir.join("a.json");
    let spec_b = dir.join("b.json");
    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pulseforge"))
        .display()
        .to_string()
        .replace(char::from_u32(0x5C).unwrap(), "/");
    // Use pulseforge's own bench-spin as the workload (direct argv launch).
    std::fs::write(
        &spec_a,
        format!(
            r#"{{"name":"fast","exe":"{exe}","args":["bench-spin","--ms","1200"],"warmup":0,"repetitions":2,"duration_ms":2000,"interval_ms":250,"env_allowlist":[]}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        &spec_b,
        format!(
            r#"{{"name":"slow","exe":"{exe}","args":["bench-spin","--ms","2400"],"warmup":0,"repetitions":2,"duration_ms":3200,"interval_ms":250,"env_allowlist":[]}}"#
        ),
    )
    .unwrap();

    let out_dir = dir.join("sessions");
    let out = Command::new(&exe)
        .args([
            "bench",
            "--a",
            spec_a.to_str().unwrap(),
            "--b",
            spec_b.to_str().unwrap(),
            "--out",
            out_dir.to_str().unwrap(),
        ])
        .env("PULSEFORGE_DATA", &dir)
        .output()
        .expect("run bench");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // stdout must be a single pure-JSON document (children silenced).
    let text = String::from_utf8_lossy(&out.stdout);
    let report = pf_types::json::parse(text.trim())
        .unwrap_or_else(|e| panic!("bench stdout must be pure JSON: {e}"));

    let wall = report
        .get("metrics")
        .and_then(|m| m.get("wall_duration"))
        .expect("wall metric");
    let base = wall
        .get("baseline")
        .and_then(|b| b.get("median"))
        .and_then(|v| v.as_f64())
        .expect("baseline median");
    let cand = wall
        .get("candidate")
        .and_then(|b| b.get("median"))
        .and_then(|v| v.as_f64())
        .expect("candidate median");
    // 2x within 35% tolerance (scheduler noise on CI-class machines).
    let ratio = cand / base;
    assert!(
        (1.65..=2.35).contains(&ratio),
        "expected ~2x wall ratio, got {ratio} (base {base}, cand {cand})"
    );

    // Runs and provenance present.
    assert_eq!(
        report
            .get("runs")
            .and_then(|r| r.get("baseline"))
            .and_then(|v| v.as_u64()),
        Some(2)
    );
    let prov = report.get("provenance").expect("provenance");
    for side in ["baseline", "candidate"] {
        assert!(
            prov.get(side)
                .and_then(|p| p.get("exe_sha256_prefix"))
                .is_some()
        );
        assert_eq!(
            prov.get(side)
                .and_then(|p| p.get("env_policy"))
                .and_then(|v| v.as_str()),
            Some("none forwarded")
        );
    }
    // Caveats carried.
    assert!(
        report
            .get("caveats")
            .and_then(|c| c.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    );

    // Both sides persisted inspectable sessions.
    let sessions = std::fs::read_dir(&out_dir).expect("sessions dir").count();
    assert_eq!(sessions, 4, "2 reps x 2 sides = 4 persisted sessions");

    let _ = std::fs::remove_dir_all(&dir);
}
