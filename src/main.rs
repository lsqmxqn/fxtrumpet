//! FxTrumpet — entry point.
//!
//! Responsibilities, in order:
//!
//! 1. Turn on logging. A tray application has no console, so this has to happen
//!    before anything can fail visibly.
//! 2. Handle the elevated helper invocations (`--install-driver`,
//!    `--remove-driver`). These are separate short-lived processes spawned by
//!    an unelevated tray via UAC, so they exit here.
//!
//!    `--panel` is also accepted: it opens the tuning panel as soon as the app
//!    is up, which is how the window is smoke-tested without driving the tray.
//! 3. Enforce a single instance — two copies would open the same endpoint twice
//!    and fight over the default device.
//! 4. Join the STA apartment and run the tray message loop.

// No console window, in any profile.
//
// Debug builds used to keep it, on the theory that `cargo run` should print the
// log inline. That made the console a *debug-only* defect: a tray application
// that opens a console window is wrong for the same reason in either profile,
// and testing it in debug is exactly how it gets missed. The log is written to
// `fxtrumpet.log` by `init_logging` regardless, so nothing is lost.
#![windows_subsystem = "windows"]

use fxtrumpet::device::ComGuard;
use fxtrumpet::{app, autostart, config, driver, routing, ui};

/// Name of the single-instance mutex. The `Local\` prefix scopes it to the
/// user's session, so two different users can each run their own copy.
const INSTANCE_MUTEX: &str = r"Local\FxTrumpet.SingleInstance";

/// Opens the tuning panel on startup instead of waiting for a tray click.
const ARG_PANEL: &str = "--panel";

/// Opens the mixer on startup instead of waiting for a tray click.
///
/// The mixer is the part of FxTrumpet that was EarTrumpet and audio-router, so
/// having a way in that does not go through the notification area is what makes
/// its window testable on a machine whose tray is not reachable by a script.
const ARG_MIXER: &str = "--mixer";

/// Hands the system's default output back to a real device, then exits.
///
/// The escape hatch for "there is no sound". Also what the uninstaller runs:
/// `uninstall.ps1` has to kill the tray process (there is no IPC to ask it
/// nicely), which skips the normal restore, and removing the startup entry
/// afterwards would take the automatic repair with it.
const ARG_RESTORE_OUTPUT: &str = "--restore-output";

fn main() {
    init_logging();

    // Elevated helper invocations do their job and exit; they never reach the
    // tray. Must come before the single-instance check, since the elevated copy
    // is a second process and would otherwise be turned away.
    if driver::handle_elevated_arguments() {
        return;
    }

    // The tray owns windows, so it needs an STA. The audio thread initialises
    // its own MTA apartment separately.
    //
    // This has to come before the restore below: device enumeration goes through
    // `IMMDeviceEnumerator`, and without an apartment `CoCreateInstance` fails
    // with CO_E_NOTINITIALIZED — which would make the escape hatch report "there
    // is nothing to undo" on a machine that has no sound.
    let _com = match ComGuard::sta() {
        Ok(guard) => guard,
        Err(err) => {
            log::error!("could not initialise COM on the main thread: {err}");
            return;
        }
    };

    // Handled before the single-instance check on purpose. As an escape hatch it
    // has to work even when a stuck copy is holding the mutex. If a healthy copy
    // is running it will take the output back a moment later, which is correct —
    // that copy is supposed to own it.
    if std::env::args().any(|arg| arg == ARG_RESTORE_OUTPUT) {
        let mut config = config::Config::load();
        let changed = routing::rescue_output(config.previous_default_id.as_deref());
        if changed {
            // The marker has been honoured; leaving it set would make every
            // later start repeat a recovery that is no longer owed.
            config.previous_default_id = None;
            if let Err(err) = config.save() {
                log::warn!("could not clear the crash-recovery marker: {err}");
            }
        }

        // Per-application overrides are cleared as well, and this is the only
        // place that does it. They are never undone on exit — a rule is the
        // user's configuration, and clearing it every time the tray closed
        // would mean re-routing everything on every start. But an uninstall
        // leaves Windows still sending half the applications to devices the
        // user has no interface left to change, so the escape hatch has to be
        // the one that wipes them.
        let cleared = routing::release_per_app_overrides();

        log::info!(
            "{ARG_RESTORE_OUTPUT}: {}; {} per-application override(s) cleared",
            if changed {
                "the default output was handed back to a real device"
            } else {
                "the default output was not pointing at the virtual sound card"
            },
            cleared
        );
        return;
    }

    // SAFETY: the returned handle is deliberately held for the process
    // lifetime; closing it would release the singleton.
    let _instance = match acquire_single_instance() {
        Some(guard) => guard,
        None => {
            log::warn!("FxTrumpet is already running; exiting");
            return;
        }
    };

    let (mut config, warning) = config::Config::load_or_default();
    if let Some(warning) = warning {
        log::warn!("{warning}");
    }

    // Autostart is on by default, and it is part of the audio recovery path
    // rather than a convenience: if the machine logs in without FxTrumpet while the
    // default output still points at the virtual sound card, there is no sound
    // at all. So the registry is *reconciled* with what the config asks for
    // instead of merely read — a fresh install registers itself here, and an
    // entry left over from a moved executable is rewritten. The one case that is
    // left alone is an entry the user switched off in Task Manager.
    config.autostart = autostart::apply(config.autostart);

    let mut app = match app::App::start(config) {
        Ok(app) => app,
        Err(err) => {
            log::error!("FxTrumpet could not start: {err}");
            return;
        }
    };

    // The panel is checked first so that passing both flags opens the panel:
    // the process has one event loop and one window at a time, and the later
    // flag would otherwise silently close the window the earlier one asked for.
    if std::env::args().any(|arg| arg == ARG_PANEL) {
        log::info!("{ARG_PANEL} given; opening the tuning panel");
        app.open_panel_now();
    } else if std::env::args().any(|arg| arg == ARG_MIXER) {
        log::info!("{ARG_MIXER} given; opening the mixer");
        app.open_mixer_now();
    }

    log::info!("FxTrumpet running (tray icon should be visible)");
    ui::run_message_loop(|| app.tick());

    log::info!("shutting down");
    app.shutdown();
}

/// Holds the single-instance mutex for the process lifetime.
struct InstanceGuard(windows::Win32::Foundation::HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateMutexW and is owned here.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Claims the single-instance mutex, or returns `None` if another copy holds it.
fn acquire_single_instance() -> Option<InstanceGuard> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::System::Threading::CreateMutexW;

    let name = fxtrumpet::ffi::to_wide(std::ffi::OsStr::new(INSTANCE_MUTEX));

    // SAFETY: a named mutex with default security attributes and no initial
    // ownership; the name is NUL-terminated.
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.ok()?;

    // CreateMutexW succeeds even when the name is taken, so the disambiguator
    // is GetLastError.
    // SAFETY: no arguments, no side effects.
    let already_running = unsafe { GetLastError() }
        == windows::Win32::Foundation::ERROR_ALREADY_EXISTS;

    if already_running {
        // SAFETY: releasing the extra handle we just opened.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        None
    } else {
        Some(InstanceGuard(handle))
    }
}

/// Starts file logging under `%APPDATA%\FxTrumpet\fxtrumpet.log`.
///
/// Falls back to stderr when the file cannot be created, because losing the log
/// is much better than losing the application.
fn init_logging() {
    use simplelog::{ConfigBuilder, LevelFilter, WriteLogger};

    // The console, when there is one, is usually CP936 on a Chinese Windows —
    // UTF-8 output would render as mojibake. Only relevant to the debug build.
    #[cfg(debug_assertions)]
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
    }

    let level = match std::env::var("FXTRUMPET_LOG").ok().as_deref() {
        Some("off") => LevelFilter::Off,
        Some("error") => LevelFilter::Error,
        Some("warn") => LevelFilter::Warn,
        Some("debug") => LevelFilter::Debug,
        Some("trace") => LevelFilter::Trace,
        _ => LevelFilter::Info,
    };

    let log_config = ConfigBuilder::new()
        .set_time_format_rfc3339()
        .build();

    let dir = config::app_data_dir();
    let _ = std::fs::create_dir_all(&dir);

    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config::log_path());

    match opened {
        Ok(file) => {
            let _ = WriteLogger::init(level, log_config.clone(), file);
        }
        Err(_) => {
            let _ = simplelog::TermLogger::init(
                level,
                log_config,
                simplelog::TerminalMode::Stderr,
                simplelog::ColorChoice::Auto,
            );
        }
    }

    log::info!(
        "{} v{} starting (pid {})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );
}
