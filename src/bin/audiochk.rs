//! M2 smoke test: does the audio path hold together?
//!
//! Run it with no arguments for a read-only report: endpoints, the virtual
//! sound card's state, and every `.fac` FxTrumpet can see. This changes nothing
//! and needs no privileges.
//!
//! ```text
//! cargo run --release --bin audiochk
//! cargo run --release --bin audiochk -- --seconds 5
//! cargo run --release --bin audiochk -- --seconds 5 --source <id> --sink <id>
//! ```
//!
//! With `--seconds N` it also starts the real engine and reports what happened:
//! captured frames, rendered frames, underruns, drops. If the virtual sound
//! card is not installed the engine will refuse to build a graph — that refusal
//! and its reason are the point of running it.
//!
//! `--source` / `--sink` override the endpoint choice for one run. The default
//! graph captures FxSound's virtual card and renders to the physical output,
//! which only carries audio while something is playing *into* the card. Pointing
//! the capture at the ordinary output instead makes the path testable without
//! reconfiguring the machine's default device.

use std::time::{Duration, Instant};

use fxtrumpet::device::{self, ComGuard};
use fxtrumpet::{driver, preset};

fn main() {
    // A console application: keep the log on stderr and make sure UTF-8
    // survives, since preset names are Chinese.
    // SAFETY: no arguments; the call only changes this console's code page.
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
    }

    // SAFETY: COM is initialised for the lifetime of the guard, which lives
    // until the end of `main`.
    let _com = match ComGuard::mta() {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("could not initialise COM: {err}");
            std::process::exit(2);
        }
    };

    let seconds: Option<f64> = flag("--seconds").and_then(|value| value.parse().ok());
    let source = flag("--source");
    let sink = flag("--sink");

    println!("FxTrumpet audio check");
    println!("==================");

    report_devices();
    report_driver();
    report_presets();

    if let Some(seconds) = seconds {
        run_engine(seconds, source, sink);
    } else {
        println!();
        println!(
            "Pass --seconds N to run the audio graph for N seconds \n             (optional: --source <endpoint id> --sink <endpoint id> to override the choice)."
        );
    }
}

/// Reads the value of a `--flag value` pair, if present.
fn flag(name: &str) -> Option<String> {
    std::env::args().skip_while(|arg| arg != name).nth(1)
}

/// Lists every render endpoint and marks the default and the virtual card.
fn report_devices() {
    println!();
    println!("Render endpoints");
    println!("----------------");

    let devices = match device::render_devices() {
        Ok(devices) => devices,
        Err(err) => {
            println!("  enumeration failed: {err}");
            return;
        }
    };

    if devices.is_empty() {
        println!("  (none)");
        return;
    }

    for entry in &devices {
        let mut tags = Vec::new();
        if entry.is_default {
            tags.push("default");
        }
        if entry.is_virtual {
            tags.push("VIRTUAL CARD");
        }
        if entry.state != device::DeviceState::Active {
            tags.push("inactive");
        }
        let tag = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        println!("  {}{}", entry.display(), tag);
        println!("      {}", entry.id);
    }

    let inactive = device::all_render_devices()
        .map(|all| all.len().saturating_sub(devices.len()))
        .unwrap_or(0);
    if inactive > 0 {
        println!("  (+{inactive} disabled/unplugged endpoint(s) not listed)");
    }
}

/// Reports what the driver layer can see.
fn report_driver() {
    println!();
    println!("Virtual sound card");
    println!("------------------");

    let status = driver::status();
    println!("  state      : {}", status.summary());
    println!("  package    : {}", yes_no(status.package_staged));
    println!("  device node: {}", yes_no(status.device_present));
    println!("  endpoint   : {}", yes_no(status.endpoint_active));
    println!("  elevated   : {}", yes_no(driver::is_elevated()));

    match &status.source_dir {
        Some(dir) => println!("  files      : {}", dir.display()),
        None => println!("  files      : not found next to the executable"),
    }

    if let Some(published) = driver::published_package_name() {
        println!("  staged as  : {published}");
    }
}

/// Parses every discoverable preset.
fn report_presets() {
    println!();
    println!("Presets");
    println!("-------");

    let library = preset::library();
    println!(
        "  {} file(s) on disk, {} compiled into this binary",
        library.len(),
        preset::embedded_count()
    );

    if library.is_empty() {
        // Nothing has been unpacked yet. Parse straight out of the binary so
        // this stays a read-only check — and so the parser is exercised
        // against every real `.fac` shipped, not just the one under test.
        let mut parsed = preset::embedded_presets();
        parsed.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        println!("  (folder empty; parsing the embedded copies in place)");
        println!("  {} of {} embedded file(s) parsed", parsed.len(), preset::embedded_count());
        for item in parsed.iter().take(30) {
            println!(
                "  {:<20} {:>2} band(s)  {}",
                item.name,
                item.bands.len(),
                item.effect_summary()
            );
        }
    } else {
        for entry in library.iter().take(30) {
            match preset::FacPreset::from_file(&entry.path) {
                Ok(parsed) => println!(
                    "  {:<20} {:>2} band(s)  {}",
                    parsed.name,
                    parsed.bands.len(),
                    parsed.effect_summary()
                ),
                Err(err) => println!("  {:<20} unreadable: {err}", entry.name),
            }
        }
    }

    if library.len() > 30 {
        println!("  ... and {} more", library.len() - 30);
    }

    // The trap this guards against: the `.fac` Main index is not the Effect
    // enum ordinal, and getting it wrong is silent.
    let mut check = preset::MAIN_SLOT_TO_EFFECT
        .iter()
        .zip(preset::MAIN_SLOT_LABELS.iter())
        .filter_map(|(effect, label)| effect.map(|value| format!("{label}->{value}")))
        .collect::<Vec<_>>();
    check.sort();
    println!("  slot map   : {}", check.join(" "));
}

/// Starts the engine and watches it for `seconds`.
fn run_engine(seconds: f64, source: Option<String>, sink: Option<String>) {
    println!();
    println!("Audio graph");
    println!("-----------");

    let mut config = fxtrumpet::Config::default();
    if source.is_some() || sink.is_some() {
        println!("  overrides: --source {} --sink {}",
            source.as_deref().unwrap_or("(auto)"),
            sink.as_deref().unwrap_or("(auto)"));
    }
    config.source_device_id = source;
    config.sink_device_id = sink;

    let engine = match fxtrumpet::AudioEngine::start(config) {
        Ok(engine) => engine,
        Err(err) => {
            println!("  could not start the engine thread: {err}");
            return;
        }
    };

    // Give the thread a moment to build (or fail to build) its graph before
    // the first report, otherwise the first line is always "starting".
    //
    // `handle()` hands back an owned `EngineHandle`, so it has to be bound
    // before `status()` can borrow from it.
    let handle = engine.handle();
    let started = Instant::now();
    let deadline = Duration::from_secs_f64(seconds.max(0.5));
    let mut last_captured = 0u64;
    let mut last_rendered = 0u64;
    let mut last_print = Instant::now();

    while started.elapsed() < deadline {
        std::thread::sleep(Duration::from_millis(100));

        if last_print.elapsed() >= Duration::from_millis(500) {
            last_print = Instant::now();
            let status = handle.status();
            let captured = status.captured_frames();
            let rendered = status.rendered_frames();

            println!(
                "  {:>5.1}s  running={:<5} cap {:>9} (+{:<7})  ren {:>9} (+{:<7})  under {:>6}  drop {:>6}  buf {:>3} ms",
                started.elapsed().as_secs_f64(),
                status.is_running(),
                captured,
                captured.saturating_sub(last_captured),
                rendered,
                rendered.saturating_sub(last_rendered),
                status.underruns(),
                status.overruns(),
                status.latency_ms(),
            );
            last_captured = captured;
            last_rendered = rendered;
        }
    }

    let status = handle.status();
    println!();
    println!("  source : {}", status.source_description().unwrap_or_else(|| "—".to_owned()));
    println!("  sink   : {}", status.sink_description().unwrap_or_else(|| "—".to_owned()));
    println!("  result : {}", status.summary());
    if let Some(error) = status.last_error() {
        println!("  error  : {error}");
    }
    println!("  peak   : {:.3}", status.peak_level());
    println!("  clipped: {} sample(s)", status.clipped_samples());

    engine.shutdown();
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}
