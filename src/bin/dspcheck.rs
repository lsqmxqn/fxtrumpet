//! M1 smoke test — proves the vendored FxSound DSP builds, loads a real `.fac`
//! preset, and measurably alters audio.
//!
//! This is the gate for milestone M1. It does not touch the audio driver, the
//! WASAPI layer, or the tray, so it fails fast and in isolation if the vendored
//! engine cannot be compiled or driven.
//!
//! Three things are asserted, because each one has bitten this integration:
//!   1. a real `.fac` file parses and populates the EQ and effect state;
//!   2. `set_power(true)` reads back as on — upstream's getter is inverted and
//!      the C shim corrects it, so a regression here means the shim broke;
//!   3. the engine actually changes the signal, i.e. power is really engaged.
//!
//! Run:
//!   cargo run --release --bin dspcheck
//!   cargo run --release --bin dspcheck -- "path\to\Music.fac"

use fxtrumpet::ffi::{self, Dsp};

// `.fac` preset names are UTF-8 (Music.fac's name line is `E9 9F B3 E4 B9 90`,
// i.e. 音乐), but a stock Windows console starts on the ANSI codepage — 936
// here — so Rust's UTF-8 stdout renders as mojibake ("音乐" -> "闊充箰"). The
// engine reads the name correctly either way; only the terminal lies. Switching
// the console to UTF-8 keeps this diagnostic trustworthy. No dependency on the
// `windows` crate: one raw import is enough for a test binary.
#[cfg(windows)]
extern "system" {
    fn SetConsoleOutputCP(w_code_page_id: u32) -> i32;
}

const SAMPLE_RATE: i32 = 48_000;
const CHANNELS: i32 = 2;
const BITS_PER_SAMPLE: i32 = 32; // float32, always
const BLOCK_FRAMES: usize = 480; // 10 ms at 48 kHz
const TEST_TONE_HZ: f64 = 1000.0;
const TEST_TONE_AMPLITUDE: f64 = 0.5;

fn main() {
    #[cfg(windows)]
    unsafe {
        SetConsoleOutputCP(65001);
    }

    let preset_path = std::env::args().nth(1).unwrap_or_else(default_preset_hint);

    println!("FxTrumpet · DSP smoke test");
    println!("  preset        : {preset_path}");
    println!("  format        : {BITS_PER_SAMPLE}-bit float, {CHANNELS}ch, {SAMPLE_RATE} Hz");
    println!("  block         : {BLOCK_FRAMES} frames\n");

    let dsp = Dsp::new().expect("dfxdsp_create returned null");
    println!("engine created, {} effect slots reported", ffi::effect_count());

    // Upstream documents that the first setSignalFormat() call after construction
    // can report failure. Calling it twice and only trusting the second attempt
    // matches what the FxSound app itself does.
    let first = dsp.set_signal_format(BITS_PER_SAMPLE, CHANNELS, SAMPLE_RATE, BITS_PER_SAMPLE);
    let second = dsp.set_signal_format(BITS_PER_SAMPLE, CHANNELS, SAMPLE_RATE, BITS_PER_SAMPLE);
    println!("set_signal_format: first={first} second={second}  (first may legitimately be false)");

    let loaded = dsp.load_preset(std::path::Path::new(&preset_path));
    println!("load_preset      : {loaded}");
    if !loaded {
        eprintln!("\ncould not load the preset — check the path");
        std::process::exit(1);
    }

    if let Some(name) = dsp.preset_name(std::path::Path::new(&preset_path)) {
        println!("preset name      : {name}");
    }

    dsp.set_power(true);
    let power_on = dsp.is_power_on();
    println!("power            : {power_on}  (round-trip of set_power(true))");
    if !power_on {
        eprintln!(
            "\nFAIL: set_power(true) did not read back as on — the shim's inversion of \
             upstream's isPowerOn() is wrong"
        );
        std::process::exit(1);
    }
    println!();

    report_eq(&dsp);
    report_effects(&dsp);

    let (before_rms, after_rms) = run_tone_through(&dsp);

    println!("\nsignal check (1 kHz sine at {:.0} dBFS)", 20.0 * TEST_TONE_AMPLITUDE.log10());
    println!("  RMS in        : {before_rms:.6}");
    println!("  RMS out       : {after_rms:.6}");
    let delta_db = 20.0 * (after_rms / before_rms).log10();
    println!("  change        : {delta_db:+.2} dB");

    let mut bands = [0.0f32; 10];
    dsp.spectrum(&mut bands);
    println!("\nspectrum bins     : {}", format_floats(&bands));
    println!("processed audio   : {} ms", dsp.total_processed_ms());

    if (after_rms - before_rms).abs() < f64::EPSILON {
        eprintln!("\nFAIL: the engine passed audio through untouched — preset or power is not applied");
        std::process::exit(1);
    }

    // Release the engine explicitly instead of leaving it to process exit, so a
    // teardown fault is attributable. Upstream's destructor does a large amount
    // of ad-hoc freeing — DfxDspPrivate::~DfxDspPrivate -> dfxpFreeAll, which
    // walks a dozen quantiser handles and bails out early on the first failure —
    // and any mistake there surfaces as a crash on shutdown rather than at the
    // point of the bug.
    println!("\nreleasing engine...");
    drop(dsp);
    println!("engine released");

    println!("\nOK: engine compiled, parsed a real preset, and altered the signal");
}

fn default_preset_hint() -> String {
    // NexBox ships a trimmed preset set that is convenient for testing.
    r"..\NexBox\src-tauri\resources\binaries\fxvad\presets\Music.fac".to_string()
}

fn report_eq(dsp: &Dsp) {
    let bands = dsp.num_bands();
    println!("EQ bands         : {bands}");
    for band in 0..bands {
        let (min, max) = dsp.band_freq_range(band).unwrap_or((0.0, 0.0));
        println!(
            "  band {:>2}  {:>8.1} Hz  {:>+5.1} dB   range {:>7.1} .. {:<7.1}",
            band,
            dsp.band_freq(band),
            dsp.band_gain(band),
            min,
            max
        );
    }
}

/// Prints both scales on purpose: the getter is normalised 0..1 while the setter
/// takes slider units 0..10, so a mismatch here is the most likely integration bug.
fn report_effects(dsp: &Dsp) {
    let slots: [(ffi::DfxEffectId, &str); 5] = [
        (ffi::DFX_EFFECT_FIDELITY, "Fidelity"),
        (ffi::DFX_EFFECT_AMBIENCE, "Ambience"),
        (ffi::DFX_EFFECT_SURROUND, "Surround"),
        (ffi::DFX_EFFECT_DYNAMIC_BOOST, "DynamicBoost"),
        (ffi::DFX_EFFECT_BASS, "Bass"),
    ];
    println!("\neffects");
    println!("  {:<13} {:>9} {:>10} {:>9}", "", "get 0-1", "set 0-10", "fac Main");
    for (id, label) in slots {
        let normalised = dsp.effect(id);
        println!(
            "  {:<13} {:>9.3} {:>10.2} {:>9.0}",
            label,
            normalised,
            normalised * 10.0,
            (normalised * 127.0).round()
        );
    }
}

/// Feeds a steady test tone through the engine in realistic block sizes and
/// returns the RMS before and after processing.
fn run_tone_through(dsp: &Dsp) -> (f64, f64) {
    let mut phase = 0.0f64;
    let phase_step = 2.0 * std::f64::consts::PI * TEST_TONE_HZ / SAMPLE_RATE as f64;
    let mut buffer = vec![0.0f32; BLOCK_FRAMES * CHANNELS as usize];

    let mut sum_in = 0.0f64;
    let mut sum_out = 0.0f64;
    let mut count = 0usize;

    // Let the engine's internal filters settle before measuring, otherwise the
    // initial transient dominates the numbers.
    for block in 0..40 {
        for frame in 0..BLOCK_FRAMES {
            let sample = (TEST_TONE_AMPLITUDE * phase.sin()) as f32;
            phase += phase_step;
            for channel in 0..CHANNELS as usize {
                buffer[frame * CHANNELS as usize + channel] = sample;
            }
        }

        let measure = block >= 20;
        if measure {
            sum_in += buffer.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
        }

        // Process in place, exactly as FxSound does.
        dsp.process(&mut buffer, BLOCK_FRAMES as i32);

        if measure {
            sum_out += buffer.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
            count += buffer.len();
        }
    }

    let rms = |sum: f64| (sum / count as f64).sqrt();
    (rms(sum_in), rms(sum_out))
}

fn format_floats(values: &[f32]) -> String {
    values
        .iter()
        .map(|v| format!("{v:.3}"))
        .collect::<Vec<_>>()
        .join(", ")
}
