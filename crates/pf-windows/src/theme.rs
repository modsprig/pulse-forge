//! OS personalization queries used by the GUI (`Theme::System`).
//!
//! Reads the Windows "Apps use light theme" toggle from the per-user
//! registry (the same key the Settings app writes). No elevation, no
//! network, best-effort: `None` means "could not determine" and callers
//! fall back to the dark default rather than guessing.

//! Raw FFI (registry read) is this module's purpose, like `ffi.rs`.

#![allow(unsafe_code)]

use crate::ffi;

const HKEY_CURRENT_USER: ffi::Handle = 0x8000_0001_u32 as ffi::Handle;
const RRF_RT_REG_DWORD: u32 = 0x0000_0010;

const PERSONALIZE_SUBKEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
const APPS_USE_LIGHT_VALUE: &str = "AppsUseLightTheme";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Does the user's Windows personalization ask apps for the light theme?
///
/// `Some(true)` — light theme requested; `Some(false)` — dark theme
/// requested; `None` — the key/value is missing or unreadable (older
/// Windows, policy-restricted registry) and the caller should fall back
/// to the profiler default (dark) rather than guessing.
pub fn apps_use_light_theme() -> Option<bool> {
    let subkey = wide(PERSONALIZE_SUBKEY);
    let value = wide(APPS_USE_LIGHT_VALUE);
    let mut data: u32 = 0;
    let mut data_len = u32::try_from(std::mem::size_of::<u32>()).ok()?;
    let mut reg_type: u32 = 0;
    // SAFETY: all pointers reference local variables sized for the query;
    // RegGetValueW writes at most `pcb_data` bytes into `pv_data`.
    let status = unsafe {
        ffi::RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut reg_type,
            (&mut data as *mut u32).cast(),
            &mut data_len,
        )
    };
    if status != 0 {
        return None;
    }
    Some(data != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personalization_toggle_is_readable_or_none() {
        // On a normal desktop session this is Some(_); the contract under
        // test is only that it never panics and never lies about failure.
        let _ = apps_use_light_theme();
    }
}
