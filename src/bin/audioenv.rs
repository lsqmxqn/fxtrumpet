//! Read-only audio environment report.
//!
//! This exists to answer one question that the log line cannot: **is FxSound's
//! virtual card the system's default playback device?**
//!
//! FxTrumpet's chain is
//!
//! ```text
//! default output = virtual card  ->  loopback capture  ->  DSP  ->  physical card
//! ```
//!
//! so if the default output is still the physical card, applications render
//! straight past the enhancer: FxTrumpet builds its graph, logs a healthy
//! `virtual -> physical` pair, and processes pure silence. Nothing sounds
//! wrong, and nothing sounds enhanced either. That failure mode is invisible
//! from the log, which is why this tool prints the two sides side by side.
//!
//! It changes nothing and needs no privileges.
//!
//! ```text
//! cargo run --release --bin audioenv
//! cargo run --release --bin audioenv -- --route-test 5
//! ```
//!
//! `--route-test N` exercises the one part of the diagnosis that cannot be read
//! off the current state: whether FxTrumpet can take the default output over *and
//! give it back*. It switches the default to the virtual card, holds it for N
//! seconds, restores it, and exits non-zero if the machine was not handed back
//! exactly as it was found. The restore is attempted even on failure paths,
//! because leaving a machine pointed at the virtual card means leaving it mute.

use fxtrumpet::config::Config;
use fxtrumpet::device::{self, ComGuard, DeviceInfo};
use fxtrumpet::routing::{self, EngageOutcome, Routing};

fn main() {
    // Preset and endpoint names are Chinese; keep the console in UTF-8.
    // SAFETY: no arguments; the call only changes this console's code page.
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
    }

    // SAFETY: COM lives for the whole of `main`.
    let _com = match ComGuard::mta() {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("could not initialise COM: {err}");
            std::process::exit(2);
        }
    };

    println!("FxTrumpet audio environment");
    println!("========================");

    let default = device::default_render_device().ok();
    let virtual_card = device::find_virtual_device();

    // ── render endpoints ────────────────────────────────────────────────
    println!();
    println!("Render endpoints (marker: * = default, V = virtual card)");
    match device::all_render_devices() {
        Ok(devices) => {
            if devices.is_empty() {
                println!("  (none)");
            }
            for device in &devices {
                let marker = match (device.is_default, device.is_virtual) {
                    (true, true) => "*V",
                    (true, false) => "* ",
                    (false, true) => " V",
                    (false, false) => "  ",
                };
                println!("  {marker} [{}] {}", device.state, device.name);
                println!("      {}", device.id);
            }
        }
        Err(err) => println!("  enumeration failed: {err}"),
    }

    // ── the diagnosis ───────────────────────────────────────────────────
    println!();
    println!("Diagnosis");

    let default_name = default
        .as_ref()
        .map(DeviceInfo::display)
        .unwrap_or_else(|| "(no default render endpoint)".to_owned());
    println!("  system default output : {default_name}");

    match (&default, &virtual_card) {
        (Some(default), Some(card)) if default.id == card.id => {
            println!("  virtual card is default : YES");
            println!("  -> audio flows through FxTrumpet. Enhancement can be heard.");
        }
        (Some(default), Some(card)) => {
            println!("  virtual card is default : NO");
            println!("      default is        : {}", default.name);
            println!("      virtual card is   : {}", card.name);
            println!("  -> applications render straight to the physical card.");
            println!("     FxTrumpet captures the virtual card, which receives nothing,");
            println!("     so it processes silence: it looks healthy and does nothing.");
        }
        (_, None) => {
            println!("  virtual card is default : n/a");
            println!("  -> FxSound's virtual sound card is NOT installed, so there is");
            println!("     nothing to capture from. Install the driver first.");
        }
        (None, Some(_)) => {
            println!("  virtual card is default : unknown (no default endpoint)");
        }
    }

    // ── what the engine would pick right now ────────────────────────────
    //
    // Mirrors `engine::resolve_source` / `resolve_sink` without opening a
    // stream, so the report shows the choice the next start would make.
    println!();
    println!("Graph the engine would build");
    let config = Config::load();
    match engine_choice(&config) {
        Some((source, sink)) => {
            println!("  capture : {}", source.display());
            println!("            {}", source.id);
            println!("  render  : {}", sink.display());
            println!("            {}", sink.id);
            if source.id == sink.id {
                println!("  -> invalid: the same endpoint on both sides would feed back.");
            }
        }
        None => println!("  (could not resolve a sink; see the errors above)"),
    }

    println!();
    println!(
        "config: source_device_id={:?} sink_device_id={:?} enabled={}",
        config.source_device_id, config.sink_device_id, config.enabled
    );
    println!(
        "        take_over_default={} previous_default_id={:?}",
        config.take_over_default, config.previous_default_id
    );

    if has_flag("--restore-output") {
        std::process::exit(if restore_output(&config) { 0 } else { 1 });
    }

    if let Some(seconds) = flag("--route-test").and_then(|value| value.parse().ok()) {
        let ok = route_test(seconds);
        std::process::exit(if ok { 0 } else { 1 });
    }
    println!();
    println!("Pass --route-test N to verify the output switch and its restore,");
    println!("or --restore-output to undo one by hand.");
}

/// Puts the output back where the config says it was taken from.
///
/// The escape hatch for the worst failure this design can produce: a machine
/// left pointed at the virtual sound card with nothing capturing it has no
/// sound at all. `Routing::recover` handles that on the next start; this
/// handles it without starting the app.
fn restore_output(config: &Config) -> bool {
    println!();
    println!("Restoring the default output");
    println!("----------------------------");

    let before = device::default_render_device().ok();
    println!(
        "  now                          : {}",
        before.as_ref().map(DeviceInfo::display).unwrap_or_else(|| "—".to_owned())
    );
    println!(
        "  remembered endpoint          : {:?}",
        config.previous_default_id
    );

    // Exactly what `fxtrumpet --restore-output` runs, so the two cannot disagree:
    // the crash marker first, and the first active physical output as a fallback
    // when the marker is gone but the machine is still on the dead end.
    let changed = routing::rescue_output(config.previous_default_id.as_deref());
    println!("  changed                      : {changed}");

    let after = device::default_render_device().ok();
    println!(
        "  after                        : {}",
        after.as_ref().map(DeviceInfo::display).unwrap_or_else(|| "—".to_owned())
    );

    // Success is defined by where the machine ended up, not by whether anything
    // moved: an already-correct output needs no undo.
    let fixed = !routing::default_is_routed_through_card();
    if !fixed {
        println!("  -> STILL on the virtual sound card: check the Windows sound settings");
    }
    fixed
}

/// Reads the value of a `--flag value` pair, if present.
fn flag(name: &str) -> Option<String> {
    std::env::args().skip_while(|arg| arg != name).nth(1)
}

/// Whether a bare switch was given.
///
/// Separate from [`flag`] on purpose: `flag` returns the argument *after* the
/// name, so it answers `None` for a switch that takes no value — which reads as
/// "not given" and silently skips the branch.
fn has_flag(name: &str) -> bool {
    std::env::args().any(|arg| arg == name)
}

/// Exercises the takeover and the restore, and checks the machine came back.
///
/// Every exit path runs the restore: a diagnostic that can leave the system
/// mute is worse than no diagnostic.
fn route_test(seconds: f64) -> bool {
    let config = Config::load();
    println!();
    println!("Routing round trip ({seconds}s hold)");
    println!("-----------------------------------");

    let before = device::default_render_device().ok();
    println!(
        "  before      : {}",
        before.as_ref().map(DeviceInfo::display).unwrap_or_else(|| "—".to_owned())
    );
    println!(
        "  recovered   : {}",
        Routing::recover(config.previous_default_id.as_deref())
    );

    let mut routing = Routing::new();
    let outcome = routing.engage();
    println!("  engage      : {outcome:?}");
    println!(
        "  now routed  : {}",
        routing::default_is_routed_through_card()
    );

    match &outcome {
        EngageOutcome::NoVirtualCard => {
            println!("  -> nothing to test: no active virtual sound card.");
            return true;
        }
        EngageOutcome::Failed(err) => {
            println!("  -> the switch failed: {err}");
            routing.release();
            return false;
        }
        EngageOutcome::AlreadyRouted | EngageOutcome::Engaged { .. } => {}
    }

    std::thread::sleep(std::time::Duration::from_secs_f64(seconds.clamp(0.0, 60.0)));

    let released = routing.release();
    println!("  released    : {released}");

    let after = device::default_render_device().ok();
    println!(
        "  after       : {}",
        after.as_ref().map(DeviceInfo::display).unwrap_or_else(|| "—".to_owned())
    );

    let restored = match (&before, &after) {
        (Some(before), Some(after)) => before.id == after.id,
        _ => false,
    };
    println!(
        "  restored    : {restored} {}",
        if restored {
            "(the machine is back where it started)"
        } else {
            "<-- MISMATCH: check the Windows sound settings"
        }
    );
    restored
}

/// Reproduces the engine's endpoint choice so the report matches reality.
fn engine_choice(config: &Config) -> Option<(DeviceInfo, DeviceInfo)> {
    let source = match &config.source_device_id {
        Some(id) => match device::device_by_id(id) {
            Some(info) if info.state == device::DeviceState::Active => info,
            _ => device::find_virtual_device().or_else(|| device::default_render_device().ok())?,
        },
        None => device::find_virtual_device().or_else(|| device::default_render_device().ok())?,
    };

    let sink = if let Some(id) = &config.sink_device_id {
        match device::device_by_id(id) {
            Some(info) if info.state == device::DeviceState::Active => info,
            _ => pick_sink(&source)?,
        }
    } else {
        pick_sink(&source)?
    };

    Some((source, sink))
}

fn pick_sink(source: &DeviceInfo) -> Option<DeviceInfo> {
    if let Ok(default) = device::default_render_device() {
        if default.id != source.id && !default.is_virtual {
            return Some(default);
        }
    }
    device::render_devices()
        .ok()?
        .into_iter()
        .filter(|device| device.id != source.id && !device.is_virtual)
        .min_by_key(|device| device.name.to_lowercase())
}
