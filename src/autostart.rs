//! Run-at-logon registration.
//!
//! Uses `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` rather than Task
//! Scheduler: it needs no elevation, it is what the user sees in Task Manager's
//! Startup tab, and they can disable it there without fighting us.
//!
//! The value is the full path in quotes — without quoting, an install path
//! containing a space (which "Program Files" guarantees) would be parsed as
//! "C:\Program" plus arguments.
//!
//! ## Why this is load-bearing, not a convenience
//!
//! FxTrumpet has to be running for audio to work at all: it points the system's
//! default output at the virtual sound card and drains it (see
//! [`crate::routing`]). If the machine restarts while FxTrumpet holds the default
//! output — or the user simply logs back in — the output is still on the virtual
//! card and nothing is draining it, so the machine is silent until FxTrumpet comes
//! back. Autostart is therefore on by default: it is the recovery path for the
//! one failure mode where the user hears nothing at all.
//!
//! ## Two registry locations, not one
//!
//! `Run` says the entry exists. Task Manager's Startup tab stores its on/off
//! state separately, under `Explorer\StartupApproved\Run`, and Windows will not
//! launch an entry the user turned off there even though it is still listed.
//! Both are read here so [`is_enabled`] answers the question that actually
//! matters — "will Windows start us?" — and [`set_enabled(true)`] clears the
//! marker, which is what makes re-enabling from the tray work.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_BINARY, REG_SZ, REG_VALUE_TYPE,
};

/// Registry value name. Also what appears in Task Manager's Startup list.
const VALUE_NAME: &str = "FxTrumpet";

/// Key holding the per-user startup entries.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Key holding Task Manager's per-entry on/off state.
const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// What the registry says about our startup entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupState {
    /// A `Run` value exists for us.
    pub registered: bool,
    /// The user switched the entry off in Task Manager's Startup tab.
    pub disabled_by_user: bool,
}

impl StartupState {
    /// Whether Windows will actually launch FxTrumpet at logon.
    pub fn effective(&self) -> bool {
        self.registered && !self.disabled_by_user
    }
}

/// Reads both registry locations.
pub fn state() -> StartupState {
    let registered = registered_command().is_some();
    StartupState {
        registered,
        // Only meaningful while the entry exists. A marker orphaned by someone
        // deleting the `Run` value by hand must not block a fresh registration,
        // which is why the survivor is checked before the marker.
        disabled_by_user: registered && is_disabled_in_task_manager(),
    }
}

/// Whether FxTrumpet would start at logon.
pub fn is_enabled() -> bool {
    state().effective()
}

/// The registered command line, if any.
pub fn registered_command() -> Option<String> {
    let key = open_key(RUN_KEY, KEY_QUERY_VALUE).ok()?;
    let result = read_string(&key, VALUE_NAME);
    // SAFETY: `key` came from RegOpenKeyExW and is owned here.
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

/// The command line we would register for the running executable.
fn expected_command() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("could not determine the executable path: {err}"))?;
    Ok(format!("\"{}\" --tray", exe.display()))
}

/// Whether the registered entry still points at *this* copy of FxTrumpet.
///
/// Moving the installation (or, during development, running the `target/`
/// build) would otherwise leave the entry launching a path that no longer
/// exists — silently, since nothing about a startup entry is visible until it
/// fails.
fn points_at_this_executable() -> bool {
    let Ok(expected) = expected_command() else {
        return true; // Nothing we could write, so leave what is there alone.
    };
    registered_command()
        .is_some_and(|registered| registered.trim().eq_ignore_ascii_case(&expected))
}

/// Registers or unregisters the current executable.
///
/// Re-registering always rewrites the path, so moving the executable and
/// toggling the option fixes the entry instead of leaving a stale one.
///
/// Enabling also clears Task Manager's disabled marker. Without that step a
/// user who once switched the entry off would find the tray checkbox turning
/// itself back off on the next start: the `Run` value would be present, and
/// Windows still would not launch it.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if !enabled {
        return remove_run_value();
    }

    write_run_value()?;
    clear_disabled_marker()
}

/// The action [`reconcile`] took, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconcile {
    /// Registered, pointed at this executable, and not disabled: nothing to do.
    AlreadyEnabled,
    /// The entry was created.
    Enabled,
    /// The entry pointed somewhere else (the install moved) and was rewritten.
    Repaired,
    /// The user disabled it in Task Manager. Left exactly as they left it.
    DisabledByUser,
    /// Not registered, and not wanted: nothing to do.
    AlreadyDisabled,
    /// The entry was removed.
    Disabled,
}

/// Brings the registry in line with `desired`, without ever fighting the user.
///
/// Called once at startup. The subtle case is a non-empty `disabled_by_user`:
/// that is an explicit decision made in Task Manager, so it wins over the
/// default, and it also stops us from deleting the entry they only switched
/// off.
pub fn reconcile(desired: bool) -> Result<Reconcile, String> {
    let current = state();

    match (desired, current.registered, current.disabled_by_user) {
        (true, true, false) if points_at_this_executable() => Ok(Reconcile::AlreadyEnabled),
        (true, true, false) => set_enabled(true).map(|()| Reconcile::Repaired),
        (true, false, _) => set_enabled(true).map(|()| Reconcile::Enabled),
        (true, true, true) => Ok(Reconcile::DisabledByUser),
        (false, false, _) | (false, _, true) => Ok(Reconcile::AlreadyDisabled),
        (false, true, false) => set_enabled(false).map(|()| Reconcile::Disabled),
    }
}

/// Reconciles the registry with `desired` and reports what is actually in
/// force.
///
/// Never fails. A policy that blocks the write leaves autostart off, which is
/// strictly better than refusing to start over it.
pub fn apply(desired: bool) -> bool {
    match reconcile(desired) {
        Ok(Reconcile::Enabled) => log::info!("registered to start at logon"),
        Ok(Reconcile::Repaired) => {
            log::info!("startup entry pointed at another copy of FxTrumpet; rewrote it")
        }
        Ok(Reconcile::Disabled) => log::info!("removed the start-at-logon entry"),
        Ok(Reconcile::DisabledByUser) => log::info!(
            "the start-at-logon entry is switched off in Task Manager; leaving it as the user set it"
        ),
        Ok(Reconcile::AlreadyEnabled | Reconcile::AlreadyDisabled) => {}
        Err(err) => log::warn!("could not reconcile the start-at-logon entry: {err}"),
    }

    is_enabled()
}

/// Writes the `Run` value for this executable.
fn write_run_value() -> Result<(), String> {
    let command = expected_command()?;

    let key = open_key(RUN_KEY, KEY_SET_VALUE)?;
    let wide_name = wide(VALUE_NAME);
    let wide_command = wide(&command);

    // `RegSetValueExW` takes a byte slice, and REG_SZ is UTF-16 in
    // little-endian order — which is what `to_le_bytes` gives us. Building the
    // buffer byte by byte avoids an `unsafe` pointer cast on `&[u16]`.
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(wide_command.as_slice()));
    for unit in &wide_command {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }

    // SAFETY: `key` is an open key with write access; both strings outlive the
    // call, and REG_SZ wants exactly the NUL-terminated UTF-16 buffer we built.
    let result = unsafe {
        RegSetValueExW(
            key,
            PCWSTR(wide_name.as_ptr()),
            None,
            REG_SZ,
            Some(bytes.as_slice()),
        )
    };
    // SAFETY: closing the key we opened.
    unsafe {
        let _ = RegCloseKey(key);
    }

    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("RegSetValueExW failed with {result:?}"))
    }
}

/// Removes the `Run` value. Succeeds when there is nothing to remove.
fn remove_run_value() -> Result<(), String> {
    let key = open_key(RUN_KEY, KEY_SET_VALUE)?;
    let wide_name = wide(VALUE_NAME);

    // SAFETY: `key` is open with write access; the name is NUL-terminated.
    let result = unsafe { RegDeleteValueW(key, PCWSTR(wide_name.as_ptr())) };
    // SAFETY: closing the key we opened.
    unsafe {
        let _ = RegCloseKey(key);
    }

    if result == ERROR_SUCCESS || result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("RegDeleteValueW failed with {result:?}"))
    }
}

/// Deletes Task Manager's on/off state for our entry, which is what turns a
/// previously disabled entry back on.
fn clear_disabled_marker() -> Result<(), String> {
    // The key itself only exists once the user has used the Startup tab, so a
    // missing key means there is nothing to clear.
    let Ok(key) = open_key(APPROVED_KEY, KEY_SET_VALUE) else {
        return Ok(());
    };
    let wide_name = wide(VALUE_NAME);

    // SAFETY: `key` is open with write access; the name is NUL-terminated.
    let result = unsafe { RegDeleteValueW(key, PCWSTR(wide_name.as_ptr())) };
    // SAFETY: closing the key we opened.
    unsafe {
        let _ = RegCloseKey(key);
    }

    if result == ERROR_SUCCESS || result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("could not clear the startup-tab state ({result:?})"))
    }
}

/// Reads Task Manager's on/off state for our entry.
fn is_disabled_in_task_manager() -> bool {
    let Ok(key) = open_key(APPROVED_KEY, KEY_QUERY_VALUE) else {
        return false;
    };
    let data = read_binary(&key, VALUE_NAME);
    // SAFETY: closing the key we opened.
    unsafe {
        let _ = RegCloseKey(key);
    }
    data.is_some_and(|bytes| is_disabled_marker(&bytes))
}

/// Interprets Task Manager's startup blob.
///
/// It is twelve bytes: a flags byte followed by the `FILETIME` at which the
/// state last changed. Bit 0 of the flags byte is the disabled bit, so `0x02`
/// means enabled and `0x03` means disabled (`0x06`/`0x07` appear on some builds
/// once startup impact has been measured).
fn is_disabled_marker(data: &[u8]) -> bool {
    data.first().is_some_and(|flags| flags & 0x01 == 0x01)
}

/// Opens a key under `HKCU` with the requested access.
fn open_key(
    path: &str,
    access: windows::Win32::System::Registry::REG_SAM_FLAGS,
) -> Result<HKEY, String> {
    let wide = wide(path);
    let mut key = HKEY::default();

    // SAFETY: the path is NUL-terminated and `key` receives the handle.
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide.as_ptr()),
            None,
            access,
            &mut key,
        )
    };

    if result == ERROR_SUCCESS {
        Ok(key)
    } else {
        Err(format!("could not open HKCU\\{path} ({result:?})"))
    }
}

/// Reads a `REG_SZ` value.
fn read_string(key: &HKEY, name: &str) -> Option<String> {
    let (kind, bytes) = read_raw(key, name)?;
    if kind != REG_SZ {
        return None;
    }

    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units).trim_end_matches('\0').to_owned())
}

/// Reads a `REG_BINARY` value.
fn read_binary(key: &HKEY, name: &str) -> Option<Vec<u8>> {
    let (kind, bytes) = read_raw(key, name)?;
    (kind == REG_BINARY).then_some(bytes)
}

/// Reads a value of any type, returning its type and raw bytes.
fn read_raw(key: &HKEY, name: &str) -> Option<(REG_VALUE_TYPE, Vec<u8>)> {
    let wide_name = wide(name);
    let mut buffer = vec![0u8; 1024];
    let mut size = buffer.len() as u32;
    let mut kind = REG_SZ;

    // SAFETY: `key` is open; `buffer` is a correctly sized output buffer and
    // `size` carries its length in bytes, as the API requires.
    let result = unsafe {
        RegQueryValueExW(
            *key,
            PCWSTR(wide_name.as_ptr()),
            None,
            Some(&mut kind),
            Some(buffer.as_mut_ptr()),
            Some(&mut size),
        )
    };

    if result != ERROR_SUCCESS {
        return None;
    }

    buffer.truncate((size as usize).min(buffer.len()));
    Some((kind, buffer))
}

/// UTF-16 with a NUL terminator.
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disabled_bit_is_the_low_bit_of_the_first_byte() {
        // 0x02/0x06 are what Windows writes for an enabled entry.
        assert!(!is_disabled_marker(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!is_disabled_marker(&[0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        // ... and 0x03/0x07 for one the user switched off.
        assert!(is_disabled_marker(&[0x03, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]));
        assert!(is_disabled_marker(&[0x07, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]));
    }

    #[test]
    fn a_truncated_or_missing_blob_is_not_treated_as_disabled() {
        assert!(!is_disabled_marker(&[]));
    }

    #[test]
    fn effective_is_registered_and_not_switched_off() {
        let cases = [
            (StartupState { registered: false, disabled_by_user: false }, false),
            (StartupState { registered: false, disabled_by_user: true }, false),
            (StartupState { registered: true, disabled_by_user: false }, true),
            (StartupState { registered: true, disabled_by_user: true }, false),
        ];
        for (state, wanted) in cases {
            assert_eq!(state.effective(), wanted, "{state:?}");
        }
    }

    #[test]
    fn the_expected_command_quotes_the_path() {
        // A quoted path is the whole reason this module writes the value by
        // hand; an unquoted "Program Files" would be split into two arguments.
        let command = expected_command().expect("current_exe is available in tests");
        assert!(command.starts_with('"'), "{command}");
        assert!(command.ends_with("\" --tray"), "{command}");
    }
}
