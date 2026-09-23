//! Rust bindings for the vendored FxSound DSP engine.
//!
//! The raw layer mirrors `capi/dfxdsp_capi.h` one-to-one. The linking itself is
//! set up by `build.rs` via the `cc` crate, so no `#[link]` attribute is needed
//! here.
//!
//! `Dsp` is a thin RAII wrapper for the common case. It is deliberately not
//! `Sync`: upstream's engine must be driven from a single thread, and Rust
//! should refuse to let you move it into a second one by accident.

use std::os::raw::{c_double, c_float, c_int, c_void};
use std::path::Path;

pub const DFXDSP_OK: c_int = 0;
pub const DFXDSP_ERR: c_int = -1;

/// Sound effect slots, mirroring `DfxDsp::Effect`.
///
/// Kept as plain integers rather than a Rust enum so that a value arriving from
/// C cannot trigger invalid-enum undefined behaviour.
pub type DfxEffectId = c_int;

pub const DFX_EFFECT_FIDELITY: DfxEffectId = 0;
pub const DFX_EFFECT_AMBIENCE: DfxEffectId = 1;
pub const DFX_EFFECT_SURROUND: DfxEffectId = 2;
pub const DFX_EFFECT_DYNAMIC_BOOST: DfxEffectId = 3;
pub const DFX_EFFECT_BASS: DfxEffectId = 4;
pub const DFX_EFFECT_COUNT: DfxEffectId = 5;

#[repr(C)]
pub struct DfxDspOpaque {
    _private: [u8; 0],
}

pub type DfxHandle = *mut DfxDspOpaque;

extern "C" {
    pub fn dfxdsp_create() -> DfxHandle;
    pub fn dfxdsp_destroy(h: DfxHandle);

    pub fn dfxdsp_set_signal_format(
        h: DfxHandle,
        bits_per_sample: c_int,
        channels: c_int,
        sample_rate: c_int,
        valid_bits: c_int,
    ) -> c_int;

    pub fn dfxdsp_process(h: DfxHandle, inout: *mut c_float, num_frames: c_int);
    pub fn dfxdsp_process_separate(
        h: DfxHandle,
        input: *const c_float,
        output: *mut c_float,
        num_frames: c_int,
    );

    pub fn dfxdsp_load_preset(h: DfxHandle, utf16_path: *const u16) -> c_int;
    pub fn dfxdsp_save_preset(
        h: DfxHandle,
        utf16_name: *const u16,
        utf16_path: *const u16,
    ) -> c_int;
    pub fn dfxdsp_export_preset(
        h: DfxHandle,
        utf16_src_path: *const u16,
        utf16_name: *const u16,
        utf16_export_path: *const u16,
    ) -> c_int;
    pub fn dfxdsp_preset_name(
        h: DfxHandle,
        utf16_path: *const u16,
        out_name: *mut u16,
        out_name_capacity: c_int,
    ) -> c_int;

    pub fn dfxdsp_set_power(h: DfxHandle, on: c_int);
    pub fn dfxdsp_is_power_on(h: DfxHandle) -> c_int;
    pub fn dfxdsp_set_eq_on(h: DfxHandle, on: c_int);

    pub fn dfxdsp_num_bands(h: DfxHandle) -> c_int;
    pub fn dfxdsp_set_num_bands(h: DfxHandle, num_bands: c_int);
    pub fn dfxdsp_band_freq(h: DfxHandle, band: c_int) -> c_double;
    pub fn dfxdsp_set_band_freq(h: DfxHandle, band: c_int, freq: c_double);
    pub fn dfxdsp_band_freq_range(
        h: DfxHandle,
        band: c_int,
        out_min: *mut c_double,
        out_max: *mut c_double,
    ) -> c_int;
    pub fn dfxdsp_band_gain(h: DfxHandle, band: c_int) -> c_double;
    pub fn dfxdsp_set_band_gain(h: DfxHandle, band: c_int, db: c_double);

    pub fn dfxdsp_effect(h: DfxHandle, effect: DfxEffectId) -> c_double;
    pub fn dfxdsp_set_effect(h: DfxHandle, effect: DfxEffectId, value: c_double);

    pub fn dfxdsp_balance(h: DfxHandle) -> c_double;
    pub fn dfxdsp_set_balance(h: DfxHandle, db: c_double);
    pub fn dfxdsp_master_gain(h: DfxHandle) -> c_double;
    pub fn dfxdsp_set_master_gain(h: DfxHandle, db: c_double);
    pub fn dfxdsp_normalization(h: DfxHandle) -> c_double;
    pub fn dfxdsp_set_normalization(h: DfxHandle, db: c_double);
    pub fn dfxdsp_volume_leveling(h: DfxHandle) -> c_double;
    pub fn dfxdsp_set_volume_leveling(h: DfxHandle, db: c_double);
    pub fn dfxdsp_filter_q(h: DfxHandle) -> c_double;
    pub fn dfxdsp_set_filter_q(h: DfxHandle, q: c_double);

    pub fn dfxdsp_spectrum(h: DfxHandle, out_bands: *mut c_float, count: c_int);
    pub fn dfxdsp_total_processed_ms(h: DfxHandle) -> u32;
    pub fn dfxdsp_reset_processed_ms(h: DfxHandle);

    pub fn dfxdsp_effect_count() -> c_int;
}

/// Safe-ish RAII handle around the C++ `DfxDsp` instance.
///
/// **At most one of these may be alive per process.** The vendored engine
/// keeps its band count in a process-global, not in the handle:
/// `DFXP_GRAPHIC_EQ_NUM_BANDS` (`DfxDspEq.cpp:32`) is written by
/// `GraphicEqSetNumBands` (`GraphicEqSet.cpp:154`) and read by every `dfxpEq`
/// entry point. Two engines therefore share that one integer, and a `sos`
/// section resize started by one handle lands in the arrays the other is
/// reading. The observed symptom is a hang, not a crash.
///
/// The app is built around this: exactly one engine is created, on the audio
/// thread, and kept for the life of the process. The constraint is only easy to
/// violate in tests, where each `#[test]` that builds its own `Dsp` looks
/// harmless — see the `ENGINE_LOCK` note in `engine::tests`.
pub struct Dsp {
    handle: DfxHandle,
}

impl Dsp {
    /// Creates the engine. Returns `None` if the C++ allocation failed.
    pub fn new() -> Option<Self> {
        let handle = unsafe { dfxdsp_create() };
        if handle.is_null() {
            None
        } else {
            Some(Dsp { handle })
        }
    }

    /// Must be called whenever the stream format changes, and once before the
    /// first `process` call.
    ///
    /// `bits_per_sample` is 32 — the engine always works in float32 even though
    /// upstream spells the buffer type `short int *` (see DfxDspPrivate.cpp:184).
    ///
    /// The first call after construction is expected to report failure upstream;
    /// that is known behaviour, not an error condition.
    pub fn set_signal_format(
        &self,
        bits_per_sample: i32,
        channels: i32,
        sample_rate: i32,
        valid_bits: i32,
    ) -> bool {
        let rc = unsafe {
            dfxdsp_set_signal_format(
                self.handle,
                bits_per_sample,
                channels,
                sample_rate,
                valid_bits,
            )
        };
        rc == DFXDSP_OK
    }

    /// In-place processing of interleaved float32 samples.
    ///
    /// `num_frames` is frames (samples per channel), not the total sample count.
    /// Call only from the audio thread, and never allocate or lock around it.
    pub fn process(&self, interleaved: &mut [f32], num_frames: i32) {
        debug_assert!(
            num_frames >= 0 && (num_frames as usize) <= interleaved.len(),
            "num_frames {num_frames} exceeds buffer of {} samples",
            interleaved.len()
        );
        unsafe { dfxdsp_process(self.handle, interleaved.as_mut_ptr(), num_frames) }
    }

    pub fn load_preset(&self, path: &Path) -> bool {
        let wide = to_wide(path.as_os_str());
        unsafe { dfxdsp_load_preset(self.handle, wide.as_ptr()) == DFXDSP_OK }
    }

    /// Writes the live engine state as `<name>.fac` **inside `dir`**.
    ///
    /// `dir` must already exist and be a directory. The engine composes the
    /// final path itself (`valsSave` does `swprintf(L"%s\\%s", dir, name)`) and
    /// appends `.fac` to the name, so passing a *file* path would target
    /// `<file>\<name>.fac` — which cannot be created. Upstream does not fail
    /// cleanly in that case: the call has been observed to never return with a
    /// non-directory path, which would wedge the engine thread mid-command and
    /// permanently stop preset saves. We therefore refuse anything that is not
    /// an existing directory *before* crossing the FFI boundary, so a caller
    /// gets a fast `false` instead of a hang.
    ///
    /// This is a syscall on the engine thread, never the audio thread.
    pub fn save_preset(&self, name: &str, dir: &Path) -> bool {
        if name.is_empty() || !dir.is_dir() {
            return false;
        }
        let wide_name = to_wide_str(name);
        let wide_path = to_wide(dir.as_os_str());
        unsafe { dfxdsp_save_preset(self.handle, wide_name.as_ptr(), wide_path.as_ptr()) == DFXDSP_OK }
    }

    /// Reads a preset's display name without applying it.
    pub fn preset_name(&self, path: &Path) -> Option<String> {
        let wide = to_wide(path.as_os_str());
        let mut buffer = vec![0u16; 256];
        let written = unsafe {
            dfxdsp_preset_name(
                self.handle,
                wide.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as c_int,
            )
        };
        if written < 0 {
            return None;
        }
        buffer.truncate(written as usize);
        Some(String::from_utf16_lossy(&buffer))
    }

    pub fn set_power(&self, on: bool) {
        unsafe { dfxdsp_set_power(self.handle, on as c_int) }
    }

    /// True when processing is active.
    ///
    /// The C shim inverts upstream's getter, which reads the BYPASS button and
    /// therefore reports the opposite of what its name suggests.
    pub fn is_power_on(&self) -> bool {
        unsafe { dfxdsp_is_power_on(self.handle) != 0 }
    }

    /// Switches the equaliser section on or off independently of the overall
    /// power state: the effects keep working with the EQ flat.
    pub fn set_eq_on(&self, on: bool) {
        unsafe { dfxdsp_set_eq_on(self.handle, on as c_int) }
    }

    /// The raw handle, for the handful of shim calls that have no wrapper yet.
    ///
    /// Prefer the typed methods; this exists so a wrapper can be added in one
    /// place instead of two.
    pub fn raw(&self) -> DfxHandle {
        self.handle
    }

    pub fn num_bands(&self) -> i32 {
        unsafe { dfxdsp_num_bands(self.handle) }
    }

    pub fn set_num_bands(&self, bands: i32) {
        unsafe { dfxdsp_set_num_bands(self.handle, bands) }
    }

    pub fn band_freq(&self, band: i32) -> f64 {
        unsafe { dfxdsp_band_freq(self.handle, band) }
    }

    pub fn set_band_freq(&self, band: i32, freq: f64) {
        unsafe { dfxdsp_set_band_freq(self.handle, band, freq) }
    }

    /// Allowed centre-frequency range for a band, used to clamp UI edits.
    pub fn band_freq_range(&self, band: i32) -> Option<(f64, f64)> {
        let mut min = 0.0f64;
        let mut max = 0.0f64;
        let rc = unsafe {
            dfxdsp_band_freq_range(self.handle, band, &mut min as *mut f64, &mut max as *mut f64)
        };
        if rc == DFXDSP_OK {
            Some((min, max))
        } else {
            None
        }
    }

    pub fn band_gain(&self, band: i32) -> f64 {
        unsafe { dfxdsp_band_gain(self.handle, band) }
    }

    pub fn set_band_gain(&self, band: i32, db: f64) {
        unsafe { dfxdsp_set_band_gain(self.handle, band, db) }
    }

    /// Stereo balance in dB.
    pub fn balance(&self) -> f64 {
        unsafe { dfxdsp_balance(self.handle) }
    }

    pub fn set_balance(&self, db: f64) {
        unsafe { dfxdsp_set_balance(self.handle, db) }
    }

    /// Output master gain in dB.
    pub fn master_gain(&self) -> f64 {
        unsafe { dfxdsp_master_gain(self.handle) }
    }

    pub fn set_master_gain(&self, db: f64) {
        unsafe { dfxdsp_set_master_gain(self.handle, db) }
    }

    /// Normalisation (loudness) amount in dB.
    pub fn normalization(&self) -> f64 {
        unsafe { dfxdsp_normalization(self.handle) }
    }

    pub fn set_normalization(&self, db: f64) {
        unsafe { dfxdsp_set_normalization(self.handle, db) }
    }

    /// Volume-levelling amount in dB.
    pub fn volume_leveling(&self) -> f64 {
        unsafe { dfxdsp_volume_leveling(self.handle) }
    }

    pub fn set_volume_leveling(&self, db: f64) {
        unsafe { dfxdsp_set_volume_leveling(self.handle, db) }
    }

    /// Filter Q shared by the EQ bands.
    pub fn filter_q(&self) -> f64 {
        unsafe { dfxdsp_filter_q(self.handle) }
    }

    pub fn set_filter_q(&self, q: f64) {
        unsafe { dfxdsp_set_filter_q(self.handle, q) }
    }

    /// Reads an effect level, returned on the **normalised 0.0..=1.0** scale.
    ///
    /// Not symmetric with [`Dsp::set_effect`], which takes 0.0..=10.0. That
    /// asymmetry is upstream's (DfxDspPrivate.cpp:231 vs :254), not a mistake
    /// here — see the note in `capi/dfxdsp_capi.h`.
    pub fn effect(&self, effect: DfxEffectId) -> f64 {
        unsafe { dfxdsp_effect(self.handle, effect) }
    }

    /// Writes an effect level on the **slider 0.0..=10.0** scale.
    pub fn set_effect(&self, effect: DfxEffectId, value: f64) {
        unsafe { dfxdsp_set_effect(self.handle, effect, value) }
    }

    /// Converts a `.fac` preset's `Main` value (MIDI 0..=127) into the value
    /// [`Dsp::set_effect`] expects.
    ///
    /// The loader writes `Main` straight into the engine's normalised field, so
    /// `Main / 127.0 == effect()`. Setting it back needs `Main / 12.7`.
    pub fn effect_value_from_fac_main(fac_main: f64) -> f64 {
        fac_main / 12.7
    }

    /// Fills `bands` with spectrum magnitudes for the visualiser.
    pub fn spectrum(&self, bands: &mut [f32]) {
        unsafe {
            dfxdsp_spectrum(self.handle, bands.as_mut_ptr(), bands.len() as c_int)
        }
    }

    pub fn total_processed_ms(&self) -> u32 {
        unsafe { dfxdsp_total_processed_ms(self.handle) }
    }
}

impl Default for Dsp {
    fn default() -> Self {
        Dsp::new().expect("DfxDsp allocation failed")
    }
}

impl Drop for Dsp {
    fn drop(&mut self) {
        unsafe { dfxdsp_destroy(self.handle) }
    }
}

/// Public method surface check used by the M1 smoke test.
pub fn effect_count() -> i32 {
    unsafe { dfxdsp_effect_count() }
}

/// Converts an OS path to a NUL-terminated UTF-16 buffer.
pub fn to_wide(path: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.encode_wide().chain(std::iter::once(0)).collect()
}

fn to_wide_str(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Helper for FFI-unaware callers that only have a `*mut c_void` probe.
pub fn is_null(handle: *mut c_void) -> bool {
    handle.is_null()
}
