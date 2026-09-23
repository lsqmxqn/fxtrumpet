//! The virtual sound card: detect it, install it, remove it.
//!
//! FxTrumpet does not ship a driver it compiled. Windows has required a kernel
//! driver signature since 10 1607 with Secure Boot, so a locally built
//! `fxvad.sys` cannot be loaded at all. What is distributed instead is
//! FxSound's already-signed `inf`/`sys`/`cat` triple — see
//! [`docs/设计方案.md`](../../docs/设计方案.md) trap 1.
//!
//! ## The two-sided installation problem
//!
//! Installing the card has a failure mode that is much worse than not
//! installing it: once `Root\FXVAD` exists, Windows often makes it the default
//! playback device. Audio then flows into a virtual sink that nothing is
//! draining, and the user's machine goes silent. [`install_with_default_guard`]
//! captures the current default first and restores it afterwards.
//!
//! Every mutating entry point here needs administrator rights. They do not
//! self-elevate silently — [`relaunch_elevated`] is a separate, explicit step
//! so the caller can ask the user first.

use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Hardware id declared by `fxvad.inf:19` (`Root\%DEV_NAME%`, `DEV_NAME=FXVAD`).
pub const HARDWARE_ID: &str = "Root\\FXVAD";

/// Device class GUID for `Class=Media` in the INF.
const CLASS_GUID_MEDIA: windows::core::GUID =
    windows::core::GUID::from_u128(0x4d36e96c_e325_11ce_bfc1_08002be10318);

/// How the virtual sound card looks right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverStatus {
    /// The driver package is staged in the driver store.
    pub package_staged: bool,
    /// A `Root\FXVAD` device node exists.
    pub device_present: bool,
    /// An audio endpoint for the card is active — i.e. it actually works.
    pub endpoint_active: bool,
    /// The `inf`/`sys`/`cat` files we would install from, if found.
    pub source_dir: Option<PathBuf>,
}

impl DriverStatus {
    /// Whether the card is usable.
    ///
    /// Deliberately keyed on the *endpoint*, not on the driver package: a
    /// staged package with no device does nothing for audio, and treating it as
    /// "installed" would leave the user with a silent machine and a green tick.
    pub fn is_usable(&self) -> bool {
        self.endpoint_active
    }

    /// A one-line summary for logs and the tray tooltip.
    pub fn summary(&self) -> String {
        match (self.package_staged, self.device_present, self.endpoint_active) {
            (_, _, true) => "virtual sound card active".to_owned(),
            (_, true, false) => "device present but no audio endpoint (driver not started?)".to_owned(),
            (true, false, false) => "driver staged, device not created".to_owned(),
            (false, false, false) => "virtual sound card not installed".to_owned(),
        }
    }
}

/// Reads the current driver state. Read-only; safe to call any time.
pub fn status() -> DriverStatus {
    let device_present = root_device_exists();
    let endpoint_active = crate::device::virtual_device_present();
    DriverStatus {
        package_staged: device_present || endpoint_active || published_package_name().is_some(),
        device_present,
        endpoint_active,
        source_dir: locate_driver_files(),
    }
}

/// Whether the current process is running elevated.
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: standard token probe. Every handle opened is closed below, and a
    // failure at any step is reported as "not elevated" rather than panicking —
    // assuming least privilege is the safe default.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Whether a `Root\FXVAD` device node exists.
///
/// Matched on each devnode's *hardware id* rather than on a device instance ID.
/// The node may have been created with `DICD_GENERATE_ID` — which makes SetupAPI
/// pick an instance name like `ROOT\0001` — or by the FxSound installer under
/// its own name, so the instance ID is not something to key on. The hardware id
/// is `Root\FXVAD` either way, because that is what the INF matches.
///
/// Uses SetupAPI rather than `reg.exe` or WMI: those are blocked in some
/// sandboxes and are far slower to start.
fn root_device_exists() -> bool {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
        SetupDiGetDeviceRegistryPropertyW, DIGCF_ALLCLASSES, DIGCF_PRESENT, SP_DEVINFO_DATA,
        SPDRP_HARDWAREID,
    };

    let wanted = HARDWARE_ID.to_ascii_lowercase();

    // SAFETY: the device information set is destroyed on every exit path, and
    // each buffer handed to SetupAPI is sized from the length the API reported
    // on the immediately preceding call.
    unsafe {
        let Ok(set) = SetupDiGetClassDevsW(
            None,
            windows::core::PCWSTR::null(),
            None,
            DIGCF_ALLCLASSES | DIGCF_PRESENT,
        ) else {
            return false;
        };

        let mut index = 0u32;
        let found = loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(set, index, &mut data).is_err() {
                break false;
            }
            index += 1;

            // Ask for the size only. This call is expected to fail with
            // ERROR_INSUFFICIENT_BUFFER; that is the documented handshake.
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceRegistryPropertyW(
                set,
                &data,
                SPDRP_HARDWAREID,
                None,
                None,
                Some(&mut needed),
            );
            if needed == 0 || needed > 8192 {
                continue;
            }

            let mut buffer = vec![0u8; needed as usize];
            if SetupDiGetDeviceRegistryPropertyW(
                set,
                &data,
                SPDRP_HARDWAREID,
                None,
                Some(buffer.as_mut_slice()),
                Some(&mut needed),
            )
            .is_err()
            {
                continue;
            }

            // SPDRP_HARDWAREID is a REG_MULTI_SZ, encoded UTF-16.
            let units: Vec<u16> = buffer
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            let text = String::from_utf16_lossy(&units).to_ascii_lowercase();
            if text.split('\0').any(|entry| entry.trim() == wanted) {
                break true;
            }
        };

        let _ = SetupDiDestroyDeviceInfoList(set);
        found
    }
}

/// The published (`oemNN.inf`) name of the FxSound driver package, if staged.
///
/// Parses `pnputil /enum-drivers` structurally instead of matching labels: the
/// tool's output is localised (this machine prints Chinese), but the *values*
/// — `fxvad.inf` and `oem12.inf` — are not.
pub fn published_package_name() -> Option<String> {
    let output = run_hidden("pnputil", &["/enum-drivers"]).ok()?;
    let text = String::from_utf8_lossy(&output);

    for block in text.split("\n\n").flat_map(|part| part.split("\r\n\r\n")) {
        let values: Vec<&str> = block
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(_, value)| value.trim())
            .collect();

        let is_ours = values
            .iter()
            .any(|value| value.eq_ignore_ascii_case("fxvad.inf"));
        if !is_ours {
            continue;
        }

        if let Some(published) = values.iter().find(|value| {
            value.len() > 8
                && value.to_ascii_lowercase().starts_with("oem")
                && value.to_ascii_lowercase().ends_with(".inf")
        }) {
            return Some((*published).to_owned());
        }
    }
    None
}

/// Marker file that identifies the bundled driver directory.
const DRIVER_INF_NAME: &str = "fxvad.inf";
const DRIVER_SYS_NAME: &str = "fxvad.sys";
const DRIVER_CAT_NAME: &str = "fxvadntamd64.cat";

/// Locates the `inf`/`sys`/`cat` triple to install from.
///
/// Search order is deployment-shaped: next to the executable first (what a
/// packaged build ships), then the development tree so a debug build works
/// without staging anything.
pub fn locate_driver_files() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("driver"));
            candidates.push(dir.join("fxvad"));
            candidates.push(dir.join("resources").join("fxvad"));
            candidates.push(dir.to_path_buf());
        }
    }

    // Development fallback. CARGO_MANIFEST_DIR is baked in at compile time and
    // points at fxtrumpet/, so the sibling NexBox checkout is two levels up.
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("NexBox")
            .join("src-tauri")
            .join("resources")
            .join("binaries")
            .join("fxvad"),
    );

    candidates.into_iter().find(|dir| {
        dir.join(DRIVER_INF_NAME).is_file()
            && dir.join(DRIVER_SYS_NAME).is_file()
            && dir.join(DRIVER_CAT_NAME).is_file()
    })
}

/// Result of an install attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The card is now active.
    Installed,
    /// It was already active; nothing was done.
    AlreadyInstalled,
    /// The driver package was staged but the endpoint has not appeared yet.
    /// Usually resolves after a rescan; reported rather than treated as failure.
    StagedPendingRescan,
}

/// Installs the virtual sound card, restoring the previous default endpoint
/// afterwards.
///
/// Must be called elevated. On a non-elevated process this returns an error
/// immediately rather than popping a UAC prompt behind the user's back — see
/// [`relaunch_elevated`] for the explicit path.
pub fn install_with_default_guard() -> Result<InstallOutcome, String> {
    if !is_elevated() {
        return Err("administrator rights are required to install the driver".to_owned());
    }

    let Some(dir) = locate_driver_files() else {
        return Err(format!(
            "could not find {DRIVER_INF_NAME}/{DRIVER_SYS_NAME}/{DRIVER_CAT_NAME} next to the executable"
        ));
    };

    if crate::device::virtual_device_present() {
        return Ok(InstallOutcome::AlreadyInstalled);
    }

    // Capture the current default *before* the device appears, because Windows
    // may silently reassign it the moment the virtual card registers.
    let previous_default = crate::device::default_render_device()
        .ok()
        .map(|device| device.id);

    let inf = dir.join(DRIVER_INF_NAME);
    log::info!("installing driver package from {}", inf.display());

    // 1. Stage the package in the driver store.
    let staged = run_elevated_tool("pnputil", &["/add-driver", &inf.to_string_lossy(), "/install"])?;
    log::debug!("pnputil /add-driver -> {}", staged.trim());

    // 2. Create the root-enumerated device node and bind the driver to it.
    //    `/add-driver` alone only affects *existing* matching devices, and a
    //    root-enumerated device does not exist until something creates it.
    if !root_device_exists() {
        create_root_device()?;
    }

    // 3. Re-impose the user's previous default if Windows moved it.
    if let Some(previous) = previous_default {
        if let Ok(current) = crate::device::default_render_device() {
            if current.id != previous && current.is_virtual {
                log::warn!(
                    "Windows switched the default endpoint to '{}' during install; restoring",
                    current.name
                );
                crate::device::try_restore_default(&previous);
            }
        }
    }

    if crate::device::virtual_device_present() {
        Ok(InstallOutcome::Installed)
    } else {
        Ok(InstallOutcome::StagedPendingRescan)
    }
}

/// Removes the virtual sound card and its driver package.
///
/// Must be called elevated. Restores the default endpoint to a real device
/// first, otherwise Windows is left pointing at an endpoint that is about to
/// disappear.
pub fn uninstall() -> Result<(), String> {
    if !is_elevated() {
        return Err("administrator rights are required to remove the driver".to_owned());
    }

    // Point playback somewhere real before the virtual card goes away.
    if let Ok(current) = crate::device::default_render_device() {
        if current.is_virtual {
            if let Ok(devices) = crate::device::render_devices() {
                if let Some(physical) = devices.into_iter().find(|device| !device.is_virtual) {
                    log::info!("moving default output to '{}' before uninstall", physical.name);
                    crate::device::try_restore_default(&physical.id);
                }
            }
        }
    }

    let Some(published) = published_package_name() else {
        return Err("no staged FxSound driver package found".to_owned());
    };

    log::info!("removing driver package {published}");
    let removed = run_elevated_tool(
        "pnputil",
        &["/delete-driver", &published, "/uninstall", "/force"],
    )?;
    log::debug!("pnputil /delete-driver -> {}", removed.trim());
    Ok(())
}

/// Creates the `Root\FXVAD` device node via SetupAPI.
///
/// Root-enumerated devices have no parent bus to discover them, so an installer
/// has to create the node explicitly. This is the sequence every virtual
/// device installer uses:
///
/// 1. `SetupDiCreateDeviceInfo` — make an empty devnode with a generated id.
/// 2. `SetupDiSetDeviceRegistryProperty(SPDRP_HARDWAREID)` — tell Windows what
///    it is, so INF matching can find `Root\FXVAD`.
/// 3. `SetupDiCallClassInstaller(DIF_REGISTERDEVICE)` — commit it.
///
/// SAFETY contract for every call below: the device information set and
/// `SP_DEVINFO_DATA` are created here, used, and destroyed here; the wide
/// strings outlive each call.
unsafe fn create_root_device_inner() -> windows::core::Result<()> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
        SetupDiDestroyDeviceInfoList, SetupDiSetDeviceRegistryPropertyW, DICD_GENERATE_ID,
        DIF_REGISTERDEVICE, SP_DEVINFO_DATA,
    };

    let instance = crate::ffi::to_wide(std::ffi::OsStr::new("FXVAD"));
    let hardware_id = crate::ffi::to_wide(std::ffi::OsStr::new(HARDWARE_ID));

    // SPDRP_HARDWAREID wants a REG_MULTI_SZ: strings each NUL-terminated, then
    // a final extra NUL that closes the list.
    let mut multi_sz: Vec<u16> = hardware_id.clone();
    multi_sz.push(0);

    let class_guid = CLASS_GUID_MEDIA;

    let set = SetupDiCreateDeviceInfoList(Some(&class_guid), None)?;

    let mut data = SP_DEVINFO_DATA {
        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };

    let created = SetupDiCreateDeviceInfoW(
        set,
        windows::core::PCWSTR(instance.as_ptr()),
        &class_guid,
        None,
        None,
        DICD_GENERATE_ID,
        Some(&mut data),
    );

    if let Err(err) = created {
        let _ = SetupDiDestroyDeviceInfoList(set);
        return Err(err);
    }

    let bytes = std::slice::from_raw_parts(
        multi_sz.as_ptr() as *const u8,
        std::mem::size_of_val(multi_sz.as_slice()),
    );

    let result = SetupDiSetDeviceRegistryPropertyW(
        set,
        &mut data,
        windows::Win32::Devices::DeviceAndDriverInstallation::SPDRP_HARDWAREID,
        Some(bytes),
    )
    .and_then(|()| SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, Some(&data)));

    let _ = SetupDiDestroyDeviceInfoList(set);
    result
}

/// Creates the root device node, logging rather than propagating SetupAPI
/// errors that mean "already there".
fn create_root_device() -> Result<(), String> {
    // SAFETY: see create_root_device_inner.
    unsafe { create_root_device_inner() }.map_err(|err| {
        format!("could not create the {HARDWARE_ID} device node: {err}", HARDWARE_ID = HARDWARE_ID)
    })
}

/// Runs a privileged helper and collects its output.
///
/// Only ever called from an already-elevated process: spawning through
/// `ShellExecuteW` would discard stdout, and `pnputil`'s output is the only
/// way to tell "staged fine" from "policy blocked it".
fn run_elevated_tool(program: &str, args: &[&str]) -> Result<String, String> {
    if !is_elevated() {
        return Err(format!("{program} must run elevated"));
    }

    let output = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|err| format!("could not run {program}: {err}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        return Err(format!(
            "{program} exited with {}: {}{}",
            output.status.code().unwrap_or(-1),
            stdout.trim(),
            stderr.trim()
        ));
    }
    Ok(stdout + &stderr)
}

/// `CREATE_NO_WINDOW` — keeps a console flash out of the tray app's face.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Runs a command with no console window, returning stdout.
fn run_hidden(program: &str, args: &[&str]) -> Result<Vec<u8>, std::io::Error> {
    let output = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    Ok(output.stdout)
}

/// Relaunches this executable elevated, passing `args` through.
///
/// Used when the user asks to install the driver from a normally-privileged
/// tray process: the app re-runs itself with `runas`, and the elevated copy
/// performs the install and exits. Returns only the fact that the prompt was
/// shown; the actual result arrives in the elevated process.
pub fn relaunch_elevated(args: &[&str]) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = std::env::current_exe().map_err(|err| format!("cannot locate own path: {err}"))?;
    let exe_wide = crate::ffi::to_wide(exe.as_os_str());

    let parameters = args
        .iter()
        .map(|arg| {
            // Quote arguments so a path with spaces survives the round trip.
            if arg.contains(' ') {
                format!("\"{arg}\"")
            } else {
                (*arg).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let params_wide = crate::ffi::to_wide(std::ffi::OsStr::new(&parameters));

    let verb = crate::ffi::to_wide(std::ffi::OsStr::new("runas"));

    // SAFETY: all four strings are NUL-terminated and outlive the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(exe_wide.as_ptr()),
            PCWSTR(params_wide.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW returns a value <= 32 on failure, not an HRESULT.
    let code = result.0 as usize;
    if code <= 32 {
        return Err(format!(
            "could not relaunch elevated (ShellExecuteW returned {code}); the user may have declined the prompt"
        ));
    }
    Ok(())
}

/// Command-line flag that tells an elevated relaunch to install the driver.
pub const ARG_INSTALL_DRIVER: &str = "--install-driver";

/// Command-line flag that tells an elevated relaunch to remove the driver.
pub const ARG_REMOVE_DRIVER: &str = "--remove-driver";

/// Handles the elevated-relaunch entry points. Returns `true` if the process
/// was a helper invocation and should exit immediately.
///
/// Keeping this in `driver` rather than `main` means the elevated path is
/// testable and the flag names live next to the code that emits them.
pub fn handle_elevated_arguments() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|arg| arg == ARG_INSTALL_DRIVER) {
        match install_with_default_guard() {
            Ok(outcome) => log::info!("driver install finished: {outcome:?}"),
            Err(err) => log::error!("driver install failed: {err}"),
        }
        return true;
    }

    if args.iter().any(|arg| arg == ARG_REMOVE_DRIVER) {
        match uninstall() {
            Ok(()) => log::info!("driver removed"),
            Err(err) => log::error!("driver removal failed: {err}"),
        }
        return true;
    }

    false
}
