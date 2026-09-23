//! Read-only report on the parts of FxTrumpet that came from EarTrumpet and
//! Audio Router: audio sessions, application identity, and the per-application
//! endpoint API.
//!
//! `audioenv` answers "is the enhancer actually in the path"; this answers "can
//! FxTrumpet see what is playing, and can it move it". The two are independent —
//! the mixer works perfectly on a machine with no virtual sound card, and the
//! enhancer works on a machine where per-application routing is unavailable.
//!
//! ```text
//! cargo run --release --bin mixerchk
//! ```
//!
//! It changes nothing and needs no privileges: every call is a read. The one
//! thing it cannot check without changing the machine is whether a *write* to
//! the per-application policy is honoured, so it reports only whether the
//! interface could be reached at all.
//!
//! Run it while something is playing. An empty application list is the correct
//! answer on a silent machine, and is also what a broken session layer would
//! print — which is why the device section comes first: if the devices are
//! listed and the applications are not, the session layer is the suspect.

use std::collections::BTreeMap;

use fxtrumpet::config::Config;
use fxtrumpet::device::{self, ComGuard};
use fxtrumpet::endpoint;
use fxtrumpet::perapp;
use fxtrumpet::process::{self, AppKey};
use fxtrumpet::router::{RouteTarget, Router};
use fxtrumpet::session::{self, Session};

fn main() {
    // Application and device names are routinely not ASCII. Without this the
    // report is mojibake on a console using the legacy code page.
    // SAFETY: no arguments; the call only changes this console's code page.
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
    }

    // SAFETY: COM lives for the whole of `main`. MTA rather than STA: session
    // enumeration and the per-application policy are both free-threaded, and
    // this process owns no windows.
    let _com = match ComGuard::mta() {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("could not initialise COM: {err}");
            std::process::exit(2);
        }
    };

    println!("FxTrumpet mixer report");
    println!("=====================");

    let devices = report_devices();
    report_policy();
    report_sessions(&devices);
    report_rules();

    println!();
    println!("Nothing above was changed.");
}

/// A device, in the two fields the rest of the report needs.
#[derive(Clone)]
struct Device {
    id: String,
    display: String,
}

/// The output devices, their own volume and their level meters.
fn report_devices() -> Vec<Device> {
    println!();
    println!("Output devices  (* = system default, V = virtual card)");

    let all = match device::all_render_devices() {
        Ok(devices) => devices,
        Err(err) => {
            println!("  enumeration failed: {err}");
            return Vec::new();
        }
    };

    if all.is_empty() {
        println!("  (none)");
        return Vec::new();
    }

    let mut out = Vec::new();
    for info in all {
        if info.state != device::DeviceState::Active {
            // Inactive endpoints are listed for completeness — the mixer's own
            // device card draws only the active ones, and a difference between
            // the two lists would be the thing worth noticing.
            println!("   (inactive) {}", info.name);
            continue;
        }

        let marker = match (info.is_default, info.is_virtual) {
            (true, true) => "*V",
            (true, false) => "* ",
            (false, true) => " V",
            (false, false) => "  ",
        };

        match endpoint::levels(&info.id) {
            Some(levels) => println!(
                "  {marker} {:<40} vol {:>3.0}%{}  peak {:>5.1}%",
                trim(&info.name, 40),
                levels.volume * 100.0,
                if levels.muted { "  MUTED" } else { "       " },
                levels.peak * 100.0,
            ),
            // A device that opens but has no volume interface is normal for
            // some HDMI and virtual endpoints; it is a row without a slider
            // rather than a failure.
            None => println!("  {marker} {:<40} (no volume interface)", trim(&info.name, 40)),
        }

        // Read both strings before either is moved: `info.id` and the display
        // name come out of the same struct, and taking the id first would leave
        // the name borrowed from a partially moved value.
        let display = info.display();
        out.push(Device {
            id: info.id,
            display,
        });
    }

    out
}

/// Whether per-application output routing can be done at all here.
fn report_policy() {
    println!();
    println!("Per-application endpoint API");

    if !perapp::is_supported() {
        println!("  unsupported: this is Windows 10 before 1803, or older.");
        return;
    }

    match perapp::Factory::new() {
        Ok(factory) => {
            println!("  available: {:?}", factory.variant());
            println!("  mechanism: system policy (no elevation needed)");
        }
        Err(err) => {
            // Not an error to act on: the machine simply cannot do this, and
            // the mixer says so in its header instead of failing.
            println!("  UNavailable: {err}");
            println!("  -> routing rules cannot be enforced; everything follows the default.");
        }
    }
}

/// Every session, grouped the way the mixer groups them.
fn report_sessions(devices: &[Device]) {
    println!();
    println!("Applications currently holding audio sessions");

    let sessions = session::all_sessions();
    if sessions.is_empty() {
        println!("  (none — nothing is playing, or no application has opened a stream)");
        return;
    }

    // Grouped by stable application key, which is what the mixer's rows are
    // keyed on: one application can own several streams — one per device, or
    // several from a browser's per-tab players — and they are one row.
    let mut groups: BTreeMap<String, (AppKey, Vec<&Session>)> = BTreeMap::new();
    for session in &sessions {
        let key = AppKey::of_process(session.process_id)
            // A session whose process has already been reaped has no identity.
            // The mixer hides these; printing them here is how you find out
            // that the *identity* lookup is what broke rather than the session
            // layer.
            .unwrap_or(AppKey::SystemSounds);
        groups
            .entry(key.to_storage())
            .or_insert_with(|| (key.clone(), Vec::new()))
            .1
            .push(session);
    }

    for (storage, (key, streams)) in &groups {
        let name = streams
            .first()
            .map(|session| process::display_name(session.process_id))
            .unwrap_or_else(|| "?".to_owned());

        let volume = streams
            .iter()
            .map(|session| session.volume)
            .fold(0.0f32, f32::max);
        let peak = streams
            .iter()
            .map(|session| session.peak)
            .fold(0.0f32, f32::max);

        println!(
            "  {:<34} {:>5} stream(s)  vol {:>3.0}%{}  peak {:>5.1}%",
            trim(&name, 34),
            streams.len(),
            volume * 100.0,
            if streams.iter().all(|session| session.muted) {
                "  MUTED"
            } else {
                "       "
            },
            peak * 100.0,
        );
        println!("      key: {storage}");
        if matches!(key, AppKey::SystemSounds) {
            println!("      note: no process behind this session");
        }

        for session in streams {
            let device = devices
                .iter()
                .find(|device| device.id == session.device_id)
                .map(|device| device.display.clone())
                .unwrap_or_else(|| session.device_id.clone());
            println!(
                "      {:?} on {}",
                session.state, device
            );
        }
    }
}

/// The configured rules, resolved against the devices and the sessions.
fn report_rules() {
    println!();
    println!("Configured routing rules");

    let config = Config::load();
    if config.routes.is_empty() {
        println!("  (none — every application follows the system default, i.e. the enhanced path)");
        return;
    }

    let router = Router::new(config.routes.clone());
    let snapshot = router.snapshot();
    let devices = device::all_render_devices().unwrap_or_default();

    for rule in &snapshot.rules {
        let destination = match &rule.target {
            RouteTarget::SystemDefault => "follow system (enhanced)".to_owned(),
            RouteTarget::Device { endpoint_id } => devices
                .iter()
                .find(|info| &info.id == endpoint_id)
                .map(|info| info.display())
                // A rule pointing at a device that is not plugged in is the
                // interesting case: the rule is intact and does nothing until
                // the device comes back, which is deliberate.
                .unwrap_or_else(|| format!("{endpoint_id}  <-- device not present")),
        };

        println!(
            "  {:<34} -> {}{}",
            trim(&rule.display_name, 34),
            destination,
            if rule.enabled { "" } else { "   (disabled)" },
        );
        println!("      app    : {}", rule.app.to_storage());
        println!("      method : {:?}", rule.method);
        if rule.target.bypasses_enhancement() {
            println!("      note   : this application BYPASSES the enhancer");
        }
    }
}

/// Shortens a label to `width` characters, since the console has no elision.
///
/// Counts characters rather than bytes, so a Chinese application name is not cut
/// into mojibake by a byte-length limit.
fn trim(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}
