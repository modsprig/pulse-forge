//! Hermetic environment helpers for headless integration tests.
//!
//! The GUI persists its theme preference under the platform data root.
//! Headless tests must never read or write the developer's real settings,
//! so test binaries point `PULSEFORGE_DATA` at a throwaway per-process
//! directory before constructing any app.

use std::sync::Once;

static HERMETIC: Once = Once::new();

/// Redirect `PULSEFORGE_DATA` to a throwaway temp dir for this test
/// process. Idempotent: the first caller wins, later calls are no-ops, so
/// every test can call it unconditionally before building an app.
///
/// Rust 2024 makes `set_var` unsafe (ambient env mutation is racy by
/// nature); here a single call per test process, before any app exists,
/// is the accepted test-harness pattern.
#[allow(unsafe_code)] // std::env::set_var is unsafe in edition 2024
pub fn hermetic_data_root() {
    HERMETIC.call_once(|| {
        let dir = std::env::temp_dir().join(format!(
            "pf-gui-test-data-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: runs at most once per test process; no app has read the
        // override yet because tests call this before building one.
        unsafe {
            std::env::set_var(pf_platform::paths::ENV_OVERRIDE, &dir);
        }
    });
}
