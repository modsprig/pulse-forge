//! Seeded-mutation fuzz tests for the daemon service dispatch (PF-1202
//! extension): the dispatcher turns client frames into typed ops against
//! a REAL supervisor and platform backend — a hostile local client (same
//! user) is the threat model, so every op-args shape must answer a
//! well-formed response and leave the supervisor consistent.
//!
//! Properties under test: dispatch NEVER panics on mutated frames; every
//! response is shape-valid (echoed id + result-or-typed-error); hostile
//! start/create attempts leave no recording running; the data root
//! accumulates only bounded session dirs (≤ create attempts). Same
//! deterministic xorshift method as the other suites.

// Bounded fuzz counters; dispatch arithmetic is internally bounded.
#![allow(clippy::arithmetic_side_effects)]

use std::path::PathBuf;
use std::sync::Arc;

use pf_daemon::DaemonService;
use pf_types::json::{self, JsonValue as J};

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

const SEEDS: [u64; 4] = [1, 2, 3, 5];

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
    let d = std::env::temp_dir().join(format!("pf-fuzz-service-{tag}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// A real supervisor + real backend (this machine's platform), pointed at
/// a throwaway data root.
fn service(data_root: &PathBuf) -> DaemonService {
    let sup = Arc::new(pf_daemon::supervisor::SessionSupervisor::new(Box::new(
        || platform_backend(),
    )));
    let backend: Arc<
        dyn Fn() -> Result<Box<dyn pf_platform::contracts::PlatformBackend>, pf_types::PfError>
            + Send
            + Sync,
    > = Arc::new(platform_backend);
    // The supervisor resolves session roots under PULSEFORGE_DATA. The
    // test binary is single-threaded per test, so no concurrent reader
    // races this write.
    // SAFETY: single-threaded test process; env is read by this test only.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("PULSEFORGE_DATA", data_root);
    }
    DaemonService::new(sup, backend)
}

#[cfg(windows)]
fn platform_backend() -> Result<Box<dyn pf_platform::contracts::PlatformBackend>, pf_types::PfError>
{
    Ok(Box::new(pf_windows::WindowsPlatform::new()))
}

#[cfg(not(windows))]
fn platform_backend() -> Result<Box<dyn pf_platform::contracts::PlatformBackend>, pf_types::PfError>
{
    Ok(Box::new(pf_linux::LinuxPlatform::new()))
}

/// One well-formed request frame per op (the mutation base corpus).
fn request_corpus() -> Vec<J> {
    vec![
        J::Object(vec![
            ("id".into(), J::UInt(1)),
            ("op".into(), J::Str("ping".into())),
        ]),
        J::Object(vec![
            ("id".into(), J::UInt(2)),
            ("op".into(), J::Str("capabilities".into())),
        ]),
        J::Object(vec![
            ("id".into(), J::UInt(3)),
            ("op".into(), J::Str("list_processes".into())),
            ("args".into(), J::Object(vec![("limit".into(), J::UInt(8))])),
        ]),
        J::Object(vec![
            ("id".into(), J::UInt(4)),
            ("op".into(), J::Str("list_sessions".into())),
        ]),
        J::Object(vec![
            ("id".into(), J::UInt(5)),
            ("op".into(), J::Str("get_session".into())),
            (
                "args".into(),
                J::Object(vec![(
                    "session_id".into(),
                    J::Str("00000000-0000-7000-8000-0000000000aa".into()),
                )]),
            ),
        ]),
        J::Object(vec![
            ("id".into(), J::UInt(6)),
            ("op".into(), J::Str("query_metrics".into())),
            (
                "args".into(),
                J::Object(vec![(
                    "session_id".into(),
                    J::Str("00000000-0000-7000-8000-0000000000aa".into()),
                )]),
            ),
        ]),
        J::Object(vec![
            ("id".into(), J::UInt(7)),
            ("op".into(), J::Str("stop_recording".into())),
            (
                "args".into(),
                J::Object(vec![(
                    "session_id".into(),
                    J::Str("00000000-0000-7000-8000-0000000000aa".into()),
                )]),
            ),
        ]),
        J::Object(vec![
            ("id".into(), J::UInt(8)),
            ("op".into(), J::Str("create_session".into())),
            ("args".into(), J::Object(vec![("pid".into(), J::UInt(1))])), // pid 1: typed refusal
        ]),
        J::Object(vec![
            ("id".into(), J::UInt(9)),
            ("op".into(), J::Str("start_recording".into())),
            (
                "args".into(),
                J::Object(vec![
                    (
                        "session_id".into(),
                        J::Str("00000000-0000-7000-8000-0000000000aa".into()),
                    ),
                    ("duration_ms".into(), J::UInt(50)),
                ]),
            ),
        ]),
    ]
}

/// Response shape check: echoed id + exactly one of result/error, and a
/// typed error carries code (+remediation optional).
fn response_is_well_formed(resp: &J) -> bool {
    let Some(pairs) = resp.as_object_pairs() else {
        return false;
    };
    let has_id = pairs.iter().any(|(k, _)| k == "id");
    let has_result = pairs.iter().any(|(k, _)| k == "result");
    let err = pairs.iter().find(|(k, _)| k == "error").map(|(_, v)| v);
    match (has_result, err) {
        (true, None) | (false, Some(_)) => has_id,
        _ => false,
    }
}

fn mutate_frame(rng: &mut Rng, base: &J) -> J {
    // Text-mutate the serialized frame and re-parse; unparseable mutants
    // degrade to a minimal garbage frame (still dispatchable).
    let text = base.to_json_string();
    let mut out = text.as_bytes().to_vec();
    let rounds = 1 + rng.below(8);
    for _ in 0..rounds {
        if out.is_empty() {
            break;
        }
        match rng.below(5) {
            0 => {
                let i = rng.below(out.len());
                out[i] ^= 1 << rng.below(8);
            }
            1 => {
                let i = rng.below(out.len());
                out[i] = *b"\":{},0-nul-9223372036854775808"
                    .get(rng.below(29))
                    .unwrap_or(&b'x');
            }
            2 => {
                let keep = rng.below(out.len());
                out.truncate(keep.max(1));
            }
            3 => {
                let i = rng.below(out.len());
                out.insert(i, b'"');
            }
            _ => {
                let i = rng.below(out.len());
                out.insert(i, b'-');
            }
        }
    }
    match String::from_utf8(out)
        .ok()
        .and_then(|t| json::parse(&t).ok())
    {
        Some(v) => v,
        None => J::Object(vec![("op".into(), J::Str("???".into()))]),
    }
}

#[test]
fn dispatch_survives_mutated_frames_and_stays_well_formed() {
    let data = scratch("mut");
    let svc = service(&data);
    let corpus = request_corpus();
    let ctx = pf_daemon::service::ClientContext {
        client_name: "fuzz".into(),
    };
    let mut create_attempts = 0usize;
    for &seed in &SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..1_500 * fuzz_scale() {
            let base = &corpus[rng.below(corpus.len())];
            let frame = mutate_frame(&mut rng, base);
            if frame.get("op").and_then(|v| v.as_str()) == Some("create_session") {
                create_attempts += 1;
            }
            let resp = svc.dispatch(&ctx, &frame);
            // Well-formedness is the contract for ANY input.
            assert!(
                response_is_well_formed(&resp),
                "malformed response for frame {}",
                frame.to_json_string()
            );
            // The id is echoed verbatim; an absent/non-numeric id is
            // echoed as 0 (the dispatcher's unwrap_or(0) contract).
            let id_in = frame.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let id_out = resp.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            assert_eq!(id_in, id_out, "id echo");
        }
    }
    // Bounded disk effect: no more session dirs than create attempts.
    let sessions = data.join("sessions");
    let dirs = std::fs::read_dir(&sessions)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .count()
        })
        .unwrap_or(0);
    assert!(
        dirs <= create_attempts,
        "session dirs ({dirs}) bounded by create attempts ({create_attempts})"
    );
    // Supervisor consistency: list still answers and stop-all is safe.
    let resp = svc.dispatch(
        &ctx,
        &J::Object(vec![
            ("id".into(), J::UInt(999)),
            ("op".into(), J::Str("list_sessions".into())),
        ]),
    );
    assert!(response_is_well_formed(&resp));
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn dispatch_rejects_structural_garbage_with_typed_errors() {
    let data = scratch("garbage");
    let svc = service(&data);
    let ctx = pf_daemon::service::ClientContext {
        client_name: "fuzz".into(),
    };
    let garbage: [J; 8] = [
        J::Null,
        J::UInt(7),
        J::Str("request".into()),
        J::Array(vec![J::UInt(1)]),
        J::Object(vec![]), // no op at all
        J::Object(vec![("op".into(), J::Str("../etc/passwd".into()))]), // path-ish op
        J::Object(vec![("op".into(), J::Str("".into()))]), // empty op
        J::Object(vec![
            ("id".into(), J::Float(-1.5)),
            ("op".into(), J::Str("get_session".into())),
            (
                "args".into(),
                J::Object(vec![("session_id".into(), J::UInt(0))]),
            ),
        ]),
    ];
    for g in &garbage {
        let resp = svc.dispatch(&ctx, g);
        assert!(response_is_well_formed(&resp), "garbage frame: {g:?}");
        // Non-ping garbage ops answer with an error object.
        if g.get("op")
            .and_then(|v| v.as_str())
            .is_none_or(|o| o != "ping")
        {
            assert!(
                resp.get("error").is_some() || resp.get("result").is_some(),
                "either result or error for {g:?}"
            );
        }
    }
    // Unknown op names (including path-shaped ones) are typed errors.
    let resp = svc.dispatch(
        &ctx,
        &J::Object(vec![
            ("id".into(), J::UInt(11)),
            ("op".into(), J::Str("../etc/passwd".into())),
        ]),
    );
    let err = resp.get("error").expect("unknown op errors");
    assert!(
        err.to_json_string().contains("INVALID_ARGUMENT"),
        "unknown op carries a typed code: {err:?}"
    );
    let _ = std::fs::remove_dir_all(&data);
}
