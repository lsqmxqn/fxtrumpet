//! The audio thread: loopback capture → DSP → render.
//!
//! ## Topology
//!
//! ```text
//!   applications ──▶ system default endpoint
//!                          │
//!                          │ (this is FxSound's virtual card once installed)
//!                          ▼
//!                    loopback capture          ┐
//!                          │                   │  this module
//!                    DSP (fxdsp static lib)    │
//!                          │                   │
//!                    render to the real card   ┘
//!                          │
//!                          ▼
//!                       speakers
//! ```
//!
//! The virtual card does no processing at all — it only exists so that all
//! system audio arrives at one place we can tap. Processing happens here, in
//! user mode. See `docs/设计方案.md` §2.
//!
//! ## Why there is a ring buffer
//!
//! The two endpoints have independent clocks. Even when both are configured for
//! 48 kHz they drift by a few parts per million, so a straight
//! capture-block → render-block copy accumulates error until it either
//! underruns or overflows. The ring absorbs that drift, and [`Drift`] steers
//! the read position so the average fill level stays near the target.
//!
//! ## Threading rules
//!
//! The audio thread must never block, allocate, or take a contended lock:
//!
//! - parameters arrive as atomics ([`SharedParams`]), read with `Relaxed`;
//! - commands arrive on a channel and are drained with `try_recv`, never `recv`;
//! - everything that needs to allocate (device enumeration, preset files) is
//!   done *outside* the process loop, at (re)build time.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};

/// `KSDATAFORMAT_SUBTYPE_PCM`.
///
/// `windows` exports `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` but not its PCM
/// sibling, so the GUID is spelled out here. Value from `mmreg.h`
/// (`MEDIASUBTYPE_PCM` in Media Foundation has the same bytes).
const KSDATAFORMAT_SUBTYPE_PCM: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);

use crate::config::Config;
use crate::device::{self, ComGuard, DeviceEvent, DeviceInfo, DeviceWatcher};
use crate::ffi::{self, Dsp};

/// `WAVE_FORMAT_EXTENSIBLE` — the tag used by essentially every shared-mode
/// mix format since Vista.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `AUDCLNT_S_BUFFER_EMPTY` — returned by `GetBuffer` when the capture side has
/// nothing left this iteration. Not an error.
const AUDCLNT_S_BUFFER_EMPTY: i32 = 0x0889_0001u32 as i32;

/// How long to wait on the audio events before doing a housekeeping pass
/// anyway. Keeps command handling and shutdown responsive even if a driver
/// stops signalling its event.
const WAIT_TIMEOUT_MS: u32 = 100;

/// Longest we will wait for the two streams to fill before declaring an
/// underrun and pushing silence.
const MAX_UNDERRUN_SILENCE_FRAMES: usize = 48_000;

/// A sample format as WASAPI describes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamFormat {
    pub rate: u32,
    pub channels: u16,
    pub container_bits: u16,
    pub valid_bits: u16,
    /// The encoding, narrowed to the three that actually occur.
    pub kind: SampleKind,
}

/// The encodings FxTrumpet converts between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    Float32,
    /// 16-bit signed, container 16.
    Pcm16,
    /// 24 bits of audio in a 32-bit container. Common for studio hardware.
    Pcm24In32,
    /// 32-bit signed, container 32.
    Pcm32,
}

impl std::fmt::Display for StreamFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} Hz, {} ch, {}/{}{}",
            self.rate,
            self.channels,
            self.valid_bits,
            self.container_bits,
            match self.kind {
                SampleKind::Float32 => " float",
                SampleKind::Pcm16 => " int",
                SampleKind::Pcm24In32 => " int",
                SampleKind::Pcm32 => " int",
            }
        )
    }
}

impl StreamFormat {
    /// Bytes per frame.
    pub fn frame_bytes(&self) -> usize {
        self.container_bits as usize / 8 * self.channels as usize
    }

    /// Reads a `WAVEFORMATEX` (possibly a `WAVEFORMATEXTENSIBLE`) into the
    /// narrow form above.
    ///
    /// SAFETY: `format` must point at a valid `WAVEFORMATEX` whose `cbSize`
    /// honestly describes the trailing bytes.
    unsafe fn from_raw(format: *const WAVEFORMATEX) -> Option<Self> {
        let base = format.as_ref()?;
        let channels = base.nChannels;
        let rate = base.nSamplesPerSec;
        let container_bits = base.wBitsPerSample;
        let mut valid_bits = base.wBitsPerSample;

        let kind = if base.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            let ext = &*(format as *const WAVEFORMATEXTENSIBLE);
            valid_bits = ext.Samples.wValidBitsPerSample;
            let sub = ext.SubFormat;
            if sub == windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                SampleKind::Float32
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                match container_bits {
                    16 => SampleKind::Pcm16,
                    32 if valid_bits == 24 => SampleKind::Pcm24In32,
                    32 => SampleKind::Pcm32,
                    _ => return None,
                }
            } else {
                return None;
            }
        } else if base.wFormatTag == 3 {
            // WAVE_FORMAT_IEEE_FLOAT
            SampleKind::Float32
        } else if base.wFormatTag == 1 {
            // WAVE_FORMAT_PCM
            match container_bits {
                16 => SampleKind::Pcm16,
                32 if valid_bits == 24 => SampleKind::Pcm24In32,
                32 => SampleKind::Pcm32,
                _ => return None,
            }
        } else {
            return None;
        };

        Some(Self {
            rate,
            channels,
            container_bits,
            valid_bits,
            kind,
        })
    }
}

/// Decodes a raw byte block into float samples in `-1.0..=1.0`.
fn decode_to_f32(bytes: &[u8], format: StreamFormat, out: &mut Vec<f32>) {
    match format.kind {
        SampleKind::Float32 => {
            out.reserve(bytes.len() / 4);
            for chunk in bytes.chunks_exact(4) {
                out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
        }
        SampleKind::Pcm16 => {
            out.reserve(bytes.len() / 2);
            for chunk in bytes.chunks_exact(2) {
                let value = i16::from_le_bytes([chunk[0], chunk[1]]);
                out.push(f32::from(value) / 32_768.0);
            }
        }
        SampleKind::Pcm24In32 => {
            // 24 significant bits left-aligned in a 32-bit container. The low
            // byte is padding, so shifting right by 8 keeps the sign.
            out.reserve(bytes.len() / 4);
            for chunk in bytes.chunks_exact(4) {
                let value = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out.push((value >> 8) as f32 / 8_388_608.0);
            }
        }
        SampleKind::Pcm32 => {
            out.reserve(bytes.len() / 4);
            for chunk in bytes.chunks_exact(4) {
                let value = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out.push(value as f32 / 2_147_483_648.0);
            }
        }
    }
}

/// Encodes float samples back into the endpoint's native format.
///
/// Input values are clamped rather than allowed to wrap: the DSP can and does
/// push past full scale on bass-heavy presets, and wrapping a sample turns a
/// loud passage into a burst of noise.
fn encode_from_f32(samples: &[f32], format: StreamFormat, out: &mut [u8]) {
    let clamp = |value: f32| value.clamp(-1.0, 1.0);

    match format.kind {
        SampleKind::Float32 => {
            for (chunk, sample) in out.chunks_exact_mut(4).zip(samples.iter()) {
                chunk.copy_from_slice(&sample.to_le_bytes());
            }
        }
        SampleKind::Pcm16 => {
            for (chunk, sample) in out.chunks_exact_mut(2).zip(samples.iter()) {
                let scaled = (clamp(*sample) * 32_767.0).round() as i32;
                chunk.copy_from_slice(&(scaled as i16).to_le_bytes());
            }
        }
        SampleKind::Pcm24In32 => {
            for (chunk, sample) in out.chunks_exact_mut(4).zip(samples.iter()) {
                let scaled = (clamp(*sample) * 8_388_607.0).round() as i32;
                chunk.copy_from_slice(&(scaled << 8).to_le_bytes());
            }
        }
        SampleKind::Pcm32 => {
            for (chunk, sample) in out.chunks_exact_mut(4).zip(samples.iter()) {
                let scaled = (f64::from(clamp(*sample)) * 2_147_483_647.0).round() as i64;
                chunk.copy_from_slice(&(scaled as i32).to_le_bytes());
            }
        }
    }
}

/// A frame-count-preserving channel map between two channel counts.
///
/// Up-mixing duplicates channel 0 into the extra outputs; down-mixing averages
/// every input channel. Both are deliberately crude: a real matrix would
/// need to know the speaker layout, and the endpoint pair here is almost always
/// stereo→stereo anyway. The point is to never feed the engine a channel count
/// it disagrees with.
fn map_channels(input: &[f32], from: usize, to: usize, out: &mut Vec<f32>) {
    if from == to {
        out.extend_from_slice(input);
        return;
    }

    let frames = input.len() / from.max(1);
    out.reserve(frames * to);

    if to > from {
        for frame in input.chunks_exact(from) {
            for index in 0..to {
                out.push(frame[index.min(from - 1)]);
            }
        }
    } else {
        // Every input channel has to contribute: averaging only the first `to`
        // of them would silently drop the right half of a stereo pair.
        for frame in input.chunks_exact(from) {
            let averaged: f32 = frame.iter().sum::<f32>() / from as f32;
            for _ in 0..to {
                out.push(averaged);
            }
        }
    }
}

/// Linear-interpolating resampler with carried phase.
///
/// Only used when the two endpoints genuinely disagree on rate, which happens
/// when a virtual card is fixed at 48 kHz and a Bluetooth sink negotiates
/// 44.1 kHz. A straight copy in that case produces pitch shift; a resampler
/// produces correct audio.
struct StreamResampler {
    in_rate: u32,
    out_rate: u32,
    channels: usize,
    /// Fractional read position, in frames, into the virtual stream formed by
    /// `history` followed by the current block.
    phase: f64,
    /// The final frame of the previous block, needed to interpolate across the
    /// block boundary.
    history: Vec<f32>,
}

impl StreamResampler {
    fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        Self {
            in_rate,
            out_rate,
            channels,
            phase: 0.0,
            history: vec![0.0; channels],
        }
    }

    /// Resamples `input` (frames × channels) into `output`.
    fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if self.in_rate == self.out_rate {
            output.extend_from_slice(input);
            return;
        }

        let channels = self.channels;
        let in_frames = input.len() / channels;
        if in_frames == 0 {
            return;
        }

        // The virtual stream is [history (1 frame)] ++ input (in_frames).
        let step = f64::from(self.in_rate) / f64::from(self.out_rate);
        let total = in_frames + 1;

        while self.phase + 1.0 < total as f64 {
            let index = self.phase.floor() as usize;
            let frac = (self.phase - index as f64) as f32;

            let frame = |offset: usize| -> &[f32] {
                if offset == 0 {
                    &self.history[..]
                } else {
                    let start = (offset - 1) * channels;
                    &input[start..start + channels]
                }
            };

            let a = frame(index);
            let b = frame(index + 1);
            for channel in 0..channels {
                output.push(a[channel] + (b[channel] - a[channel]) * frac);
            }

            self.phase += step;
        }

        // Re-base the phase onto the new history frame and remember the last
        // input frame for the next call.
        self.phase -= in_frames as f64;
        if self.phase < 0.0 {
            self.phase = 0.0;
        }
        let tail = &input[(in_frames - 1) * channels..];
        self.history.copy_from_slice(tail);
    }
}

/// A single-threaded ring buffer of interleaved float frames.
///
/// Both producer (capture) and consumer (render) live in the audio thread, so
/// this needs no synchronisation at all.
struct RingBuffer {
    data: Vec<f32>,
    channels: usize,
    read_frame: usize,
    len_frames: usize,
    capacity_frames: usize,
}

impl RingBuffer {
    fn new(capacity_frames: usize, channels: usize) -> Self {
        Self {
            data: vec![0.0; capacity_frames * channels],
            channels,
            read_frame: 0,
            len_frames: 0,
            capacity_frames,
        }
    }

    fn available(&self) -> usize {
        self.len_frames
    }

    /// Appends one frame. Returns `false` if the buffer was full and the
    /// oldest frame had to be discarded to make room.
    fn push_frame(&mut self, frame: &[f32]) -> bool {
        // Capacity is a hard bound, and keeping the newest audio is better than
        // letting the ring grow, so a full buffer evicts its oldest frame.
        let had_room = self.len_frames < self.capacity_frames;
        if !had_room {
            self.read_frame = (self.read_frame + 1) % self.capacity_frames;
            self.len_frames -= 1;
        }
        let write_frame = (self.read_frame + self.len_frames) % self.capacity_frames;
        let base = write_frame * self.channels;
        self.data[base..base + self.channels].copy_from_slice(frame);
        self.len_frames += 1;
        had_room
    }

    /// Pops up to `frames` frames into `out`. Returns how many were written.
    fn pop_frames(&mut self, count: usize, out: &mut [f32]) -> usize {
        let take = count.min(self.len_frames);
        for index in 0..take {
            let frame = (self.read_frame + index) % self.capacity_frames;
            let base = frame * self.channels;
            out[index * self.channels..(index + 1) * self.channels]
                .copy_from_slice(&self.data[base..base + self.channels]);
        }
        self.read_frame = (self.read_frame + take) % self.capacity_frames;
        self.len_frames -= take;
        take
    }
}

/// Parameters the UI thread writes and the audio thread reads.
///
/// Every field is an atomic so the audio thread never waits on the UI. Floats
/// travel as their bit patterns, which is exact and lock-free.
pub struct SharedParams {
    enabled: AtomicBool,
    eq_on: AtomicBool,
    /// Effect levels on the 0..=10 slider scale, indexed by `DfxEffectId`.
    effects: [AtomicU32; ffi::DFX_EFFECT_COUNT as usize],
    /// Band gains in dB, indexed by band.
    band_gains: [AtomicU32; MAX_BANDS],
    /// Centre frequencies in Hz. Written once at setup; kept for readback.
    band_freqs: [AtomicU32; MAX_BANDS],
    num_bands: AtomicUsize,
    balance: AtomicU32,
    master_gain: AtomicU32,
    normalization: AtomicU32,
    volume_leveling: AtomicU32,
    filter_q: AtomicU32,
    /// Bumped whenever any of the above changes, so the audio thread can skip
    /// re-applying unchanged values.
    generation: AtomicU64,
}

/// Upper bound on EQ bands. Upstream's largest grid is 31.
pub const MAX_BANDS: usize = 31;

impl Default for SharedParams {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            eq_on: AtomicBool::new(true),
            effects: std::array::from_fn(|_| AtomicU32::new(0)),
            band_gains: std::array::from_fn(|_| AtomicU32::new(0)),
            band_freqs: std::array::from_fn(|_| AtomicU32::new(0)),
            num_bands: AtomicUsize::new(10),
            balance: AtomicU32::new(0.0f32.to_bits()),
            master_gain: AtomicU32::new(0.0f32.to_bits()),
            normalization: AtomicU32::new(0.0f32.to_bits()),
            volume_leveling: AtomicU32::new(0.0f32.to_bits()),
            filter_q: AtomicU32::new(1.0f32.to_bits()),
            generation: AtomicU64::new(1),
        }
    }
}

/// Stores an `f32` into an `AtomicU32` slot.
fn store_f32(slot: &AtomicU32, value: f32) {
    slot.store(value.to_bits(), Ordering::Relaxed);
}

/// Loads an `f32` from an `AtomicU32` slot.
fn load_f32(slot: &AtomicU32) -> f32 {
    f32::from_bits(slot.load(Ordering::Relaxed))
}

impl SharedParams {
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        self.bump();
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_eq_on(&self, on: bool) {
        self.eq_on.store(on, Ordering::Relaxed);
        self.bump();
    }

    pub fn eq_on(&self) -> bool {
        self.eq_on.load(Ordering::Relaxed)
    }

    pub fn set_effect(&self, effect: i32, slider_value: f32) {
        if let Some(slot) = self.effects.get(effect as usize) {
            store_f32(slot, slider_value);
            self.bump();
        }
    }

    pub fn effect(&self, effect: i32) -> f32 {
        self.effects
            .get(effect as usize)
            .map(load_f32)
            .unwrap_or(0.0)
    }

    pub fn set_band_gain(&self, band: usize, db: f32) {
        if let Some(slot) = self.band_gains.get(band) {
            store_f32(slot, db);
            self.bump();
        }
    }

    pub fn band_gain(&self, band: usize) -> f32 {
        self.band_gains.get(band).map(load_f32).unwrap_or(0.0)
    }

    pub fn set_band_freq(&self, band: usize, hz: f32) {
        if let Some(slot) = self.band_freqs.get(band) {
            store_f32(slot, hz);
        }
    }

    pub fn band_freq(&self, band: usize) -> f32 {
        self.band_freqs.get(band).map(load_f32).unwrap_or(0.0)
    }

    pub fn set_num_bands(&self, bands: usize) {
        self.num_bands
            .store(bands.clamp(1, MAX_BANDS), Ordering::Relaxed);
        self.bump();
    }

    pub fn num_bands(&self) -> usize {
        self.num_bands.load(Ordering::Relaxed)
    }

    pub fn set_balance(&self, db: f32) {
        store_f32(&self.balance, db);
        self.bump();
    }

    pub fn balance(&self) -> f32 {
        load_f32(&self.balance)
    }

    pub fn set_master_gain(&self, db: f32) {
        store_f32(&self.master_gain, db);
        self.bump();
    }

    pub fn master_gain(&self) -> f32 {
        load_f32(&self.master_gain)
    }

    pub fn set_normalization(&self, db: f32) {
        store_f32(&self.normalization, db);
        self.bump();
    }

    pub fn normalization(&self) -> f32 {
        load_f32(&self.normalization)
    }

    pub fn set_volume_leveling(&self, db: f32) {
        store_f32(&self.volume_leveling, db);
        self.bump();
    }

    pub fn volume_leveling(&self) -> f32 {
        load_f32(&self.volume_leveling)
    }

    pub fn set_filter_q(&self, q: f32) {
        store_f32(&self.filter_q, q);
        self.bump();
    }

    pub fn filter_q(&self) -> f32 {
        load_f32(&self.filter_q)
    }

    /// Marks the parameters dirty.
    pub fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// Number of spectrum bars published to the UI. Matches what upstream's
/// `getSpectrumBandValues` is normally asked for.
pub const SPECTRUM_BANDS: usize = 10;

/// What the tray and panel display. Cheap to poll from the UI thread.
#[derive(Debug, Default)]
pub struct EngineStatus {
    running: AtomicBool,
    /// Whether the virtual card is present — i.e. whether the graph can work.
    virtual_present: AtomicBool,
    /// Frames captured since the graph was built.
    captured_frames: AtomicU64,
    /// Frames rendered since the graph was built.
    rendered_frames: AtomicU64,
    /// Moments where the renderer wanted data the capture side had not produced.
    underruns: AtomicU64,
    /// Captured frames discarded because the ring was full.
    overruns: AtomicU64,
    /// Graph rebuilds since start.
    rebuilds: AtomicU64,
    /// Samples the DSP pushed past full scale and had to be clamped.
    clipped_samples: AtomicU64,
    /// Ring occupancy in milliseconds, updated once per render cycle.
    latency_ms: AtomicU32,
    /// Peak output level, linear 0.0..=1.0, decayed each render cycle.
    peak_level: AtomicU32,
    /// Spectrum magnitudes, each 0.0..=1.0.
    spectrum: [AtomicU32; SPECTRUM_BANDS],
    /// Human-readable format strings, set at build time.
    source: Mutex<Option<String>>,
    sink: Mutex<Option<String>>,
    /// The `.fac` file the user has selected.
    ///
    /// Lives here rather than only in the config because the tray *and* the
    /// panel can both change it, and each has to be able to see the other's
    /// choice. The app owns the reconciliation against the config file.
    active_preset: Mutex<Option<String>>,
    /// Bumped every time a preset file is written into the preset folder.
    ///
    /// The save itself happens on the audio thread — only the engine holds the
    /// `DfxDsp` whose state is being serialised — so the tray and the panel
    /// cannot learn about it by watching their own call paths. They compare
    /// this against the value they last saw and rescan when it moves, which
    /// also covers a file dropped in by hand, since `App::refresh` rescans on
    /// the same signal.
    presets_revision: AtomicU64,
    /// Outcome of the most recent save: `(filename, succeeded)`.
    ///
    /// Read by the panel to report what happened. A `Mutex` rather than an
    /// atomic because it is two values that must agree with each other, and it
    /// is only touched once per user click.
    last_save: Mutex<Option<(String, bool)>>,
    /// Last error worth showing the user.
    last_error: Mutex<Option<String>>,
}

impl EngineStatus {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn virtual_present(&self) -> bool {
        self.virtual_present.load(Ordering::Relaxed)
    }

    pub fn captured_frames(&self) -> u64 {
        self.captured_frames.load(Ordering::Relaxed)
    }

    pub fn rendered_frames(&self) -> u64 {
        self.rendered_frames.load(Ordering::Relaxed)
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }

    pub fn rebuilds(&self) -> u64 {
        self.rebuilds.load(Ordering::Relaxed)
    }

    pub fn clipped_samples(&self) -> u64 {
        self.clipped_samples.load(Ordering::Relaxed)
    }

    pub fn latency_ms(&self) -> u32 {
        self.latency_ms.load(Ordering::Relaxed)
    }

    /// Peak output level, linear 0.0..=1.0.
    pub fn peak_level(&self) -> f32 {
        load_f32(&self.peak_level)
    }

    /// One spectrum bar, 0.0..=1.0.
    pub fn spectrum(&self, index: usize) -> f32 {
        self.spectrum
            .get(index)
            .map(load_f32)
            .unwrap_or(0.0)
    }

    pub fn source_description(&self) -> Option<String> {
        self.source.lock().ok()?.clone()
    }

    pub fn sink_description(&self) -> Option<String> {
        self.sink.lock().ok()?.clone()
    }

    /// The `.fac` file currently selected, as a path string.
    pub fn active_preset(&self) -> Option<String> {
        self.active_preset.lock().ok()?.clone()
    }

    /// Records a new selection.
    ///
    /// Called by the tray and the panel alike, so that whichever one the user
    /// touched, the other catches up on its next refresh.
    pub fn set_active_preset(&self, path: Option<&std::path::Path>) {
        if let Ok(mut slot) = self.active_preset.lock() {
            *slot = path.map(|path| path.to_string_lossy().into_owned());
        }
    }

    /// How many times the preset folder has gained or lost a file.
    ///
    /// Polled, not consumed: every reader keeps its own last-seen value, so the
    /// tray and the panel can both notice without racing each other.
    pub fn presets_revision(&self) -> u64 {
        self.presets_revision.load(Ordering::Relaxed)
    }

    /// Publishes the outcome of a preset save and bumps the folder revision.
    ///
    /// Called from the audio thread right after the engine has written the
    /// file, which is the only place that knows whether the write worked.
    fn note_preset_written(&self, filename: String, ok: bool) {
        if let Ok(mut slot) = self.last_save.lock() {
            *slot = Some((filename, ok));
        }
        self.presets_revision.fetch_add(1, Ordering::Relaxed);
    }

    /// The outcome of the most recent save, if there has been one.
    pub fn last_save(&self) -> Option<(String, bool)> {
        self.last_save.lock().ok()?.clone()
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok()?.clone()
    }

    fn set_error(&self, message: impl Into<String>) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(message.into());
        }
    }

    fn clear_error(&self) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = None;
        }
    }

    /// A compact summary for logs and the tray tooltip.
    pub fn summary(&self) -> String {
        if !self.is_running() {
            return "stopped".to_owned();
        }
        if !self.virtual_present() {
            return "no virtual sound card".to_owned();
        }
        let underruns = self.underruns();
        if underruns == 0 {
            format!("{} frames rendered, clean", self.rendered_frames())
        } else {
            format!("{} frames rendered, {underruns} underrun frame(s)", self.rendered_frames())
        }
    }
}

/// Commands the control side sends to the audio thread.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    SetEnabled(bool),
    SetEqOn(bool),
    SetNumBands(usize),
    SetBandGain { band: usize, db: f32 },
    SetEffect { effect: i32, slider: f32 },
    SetBalance(f32),
    SetMasterGain(f32),
    SetNormalization(f32),
    SetVolumeLeveling(f32),
    SetFilterQ(f32),
    /// Apply a `.fac` file. Parsed and validated off-thread; the audio thread
    /// only performs the engine call.
    LoadPreset { path: PathBuf, preset: crate::preset::FacPreset },
    /// Persist the engine's current state to a `.fac` file.
    ///
    /// `path` is the **directory** to write into, not the file: the engine
    /// builds the filename from `name` and appends `.fac` to it.
    SavePreset { name: String, path: PathBuf },
    /// Tear the graph down and build it again, e.g. after the user picked a
    /// different endpoint.
    Rebuild,
    /// Stop the thread.
    Shutdown,
}

/// Why a rebuild happened, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildReason {
    Startup,
    Requested,
    DefaultDeviceChanged,
    DeviceUnplugged,
    Error,
}

impl std::fmt::Display for RebuildReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RebuildReason::Startup => "startup",
            RebuildReason::Requested => "requested",
            RebuildReason::DefaultDeviceChanged => "default device changed",
            RebuildReason::DeviceUnplugged => "device removed",
            RebuildReason::Error => "recovering from an error",
        })
    }
}

/// The control-side handle. Cloneable, `Send`, safe to hold in the tray thread.
#[derive(Clone)]
pub struct EngineHandle {
    commands: Sender<EngineCommand>,
    params: Arc<SharedParams>,
    status: Arc<EngineStatus>,
    /// Set by the tray to ask the audio thread to stop.
    shutdown: Arc<AtomicBool>,
}

impl EngineHandle {
    /// Sends a command, ignoring the failure case where the thread has already
    /// exited.
    pub fn send(&self, command: EngineCommand) {
        if self.commands.send(command).is_err() {
            log::debug!("audio thread is gone; dropping command");
        }
    }

    /// Shared parameters, for the UI to read current values from.
    pub fn params(&self) -> &Arc<SharedParams> {
        &self.params
    }

    /// Live status, for the tray tooltip and the panel's meters.
    pub fn status(&self) -> &Arc<EngineStatus> {
        &self.status
    }

    /// Asks the audio thread to stop. Returns once the flag is set; joining
    /// happens in [`AudioEngine::shutdown`].
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.send(EngineCommand::Shutdown);
    }
}

/// A handle with no audio thread behind it.
///
/// Exists for tests that need the type but not the audio path: the panel's
/// window lifecycle is worth testing on its own, and starting a real engine
/// would make that test depend on the machine having sound hardware. Commands
/// sent through it land in a channel whose receiver is already gone, which
/// [`EngineHandle::send`] treats as "the thread has exited" and drops.
#[cfg(test)]
pub(crate) fn detached_handle() -> EngineHandle {
    let (commands, _receiver) = channel();
    EngineHandle {
        commands,
        params: Arc::new(SharedParams::default()),
        status: Arc::new(EngineStatus::default()),
        shutdown: Arc::new(AtomicBool::new(false)),
    }
}

/// Owns the audio thread.
pub struct AudioEngine {
    handle: EngineHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioEngine {
    /// Starts the audio thread. It builds its own graph and keeps it alive,
    /// rebuilding in response to commands and device changes.
    ///
    /// The config is cloned in, so later edits to the on-disk file do not
    /// silently retarget a running engine.
    pub fn start(config: Config) -> std::io::Result<Self> {
        let (commands, receiver) = channel();
        let params = Arc::new(SharedParams::default());
        let status = Arc::new(EngineStatus::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = EngineHandle {
            commands,
            params: Arc::clone(&params),
            status: Arc::clone(&status),
            shutdown: Arc::clone(&shutdown),
        };

        let thread_params = Arc::clone(&params);
        let thread_status = Arc::clone(&status);
        let thread_shutdown = Arc::clone(&shutdown);

        let thread = std::thread::Builder::new()
            .name("fxtrumpet-audio".to_owned())
            .spawn(move || {
                run_audio_thread(config, thread_params, thread_status, thread_shutdown, receiver);
            })?;

        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }

    /// The control handle.
    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }

    /// Stops the thread and waits for it.
    pub fn shutdown(mut self) {
        self.handle.request_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.handle.request_shutdown();
        if let Some(thread) = self.thread.take() {
            // Best-effort: never block a drop indefinitely on a stuck driver.
            let _ = thread.join();
        }
    }
}

// ── the audio thread ──────────────────────────────────────────────────────

/// One opened endpoint pair. The event handles live in [`Events`], which owns
/// them; keeping them in one place avoids a double `CloseHandle`.
struct Graph {
    /// Kept alive: dropping these stops the streams.
    capture_client: IAudioClient,
    render_client: IAudioClient,
    capture: IAudioCaptureClient,
    render: IAudioRenderClient,
    source_format: StreamFormat,
    sink_format: StreamFormat,
    /// Frames of the render endpoint's buffer.
    render_buffer_frames: usize,
    dsp_channels: usize,
}

/// Owns the two event handles and closes them on drop.
struct Events {
    capture: HANDLE,
    render: HANDLE,
}

impl Drop for Events {
    fn drop(&mut self) {
        // SAFETY: both handles were created by CreateEventW and are owned here.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.capture);
            let _ = windows::Win32::Foundation::CloseHandle(self.render);
        }
    }
}

/// Formats an endpoint as a log-friendly string.
fn describe(device: &DeviceInfo) -> String {
    format!("{} [{}]", device.display(), device.id)
}

/// Opens one endpoint's `IAudioClient` and returns it with its mix format.
fn open_client(
    device_id: &str,
) -> Result<(IAudioClient, StreamFormat, String), String> {
    // SAFETY: COM is initialised on the audio thread before this is called.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|err| format!("no MMDevice enumerator: {err}"))?;

        let wide = ffi::to_wide(std::ffi::OsStr::new(device_id));
        let device = enumerator
            .GetDevice(PCWSTR(wide.as_ptr()))
            .map_err(|err| format!("endpoint {device_id} is not available: {err}"))?;

        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|err| format!("could not activate IAudioClient: {err}"))?;

        let raw_format = client
            .GetMixFormat()
            .map_err(|err| format!("could not read the mix format: {err}"))?;

        let parsed = StreamFormat::from_raw(raw_format);
        let name = device::device_by_id(device_id)
            .map(|info| info.display())
            .unwrap_or_else(|| device_id.to_owned());
        CoTaskMemFree(Some(raw_format as *const _));

        let format = parsed.ok_or_else(|| {
            "the endpoint uses a sample format FxTrumpet cannot convert (expected float32 or PCM)"
                .to_owned()
        })?;
        Ok((client, format, name))
    }
}

/// Builds a capture stream in loopback mode on `device_id`.
fn build_capture(
    device_id: &str,
    buffer_ms: u32,
) -> Result<(IAudioClient, IAudioCaptureClient, StreamFormat, HANDLE, String), String> {
    let (client, format, name) = open_client(device_id)?;

    // SAFETY: the client is live; the mix format is re-read here so the
    // Initialize call gets exactly the bytes the endpoint asked for.
    unsafe {
        let raw_format = client
            .GetMixFormat()
            .map_err(|err| format!("could not re-read the mix format: {err}"))?;

        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        let buffer_hns = i64::from(buffer_ms.max(10)) * 10_000;

        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            buffer_hns,
            0,
            raw_format,
            None,
        );
        CoTaskMemFree(Some(raw_format as *const _));
        init.map_err(|err| format!("could not initialise loopback capture: {err}"))?;

        let event = CreateEventW(None, false, false, None)
            .map_err(|err| format!("could not create the capture event: {err}"))?;
        client
            .SetEventHandle(event)
            .map_err(|err| format!("could not arm the capture event: {err}"))?;

        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|err| format!("could not get IAudioCaptureClient: {err}"))?;

        Ok((client, capture, format, event, name))
    }
}

/// Builds a render stream on `device_id`.
fn build_render(
    device_id: &str,
    buffer_ms: u32,
) -> Result<(IAudioClient, IAudioRenderClient, u32, StreamFormat, HANDLE, String), String> {
    let (client, format, name) = open_client(device_id)?;

    // SAFETY: as in build_capture.
    unsafe {
        let raw_format = client
            .GetMixFormat()
            .map_err(|err| format!("could not re-read the mix format: {err}"))?;

        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        let buffer_hns = i64::from(buffer_ms.max(10)) * 10_000;

        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            buffer_hns,
            0,
            raw_format,
            None,
        );
        CoTaskMemFree(Some(raw_format as *const _));
        init.map_err(|err| format!("could not initialise rendering: {err}"))?;

        let event = CreateEventW(None, false, false, None)
            .map_err(|err| format!("could not create the render event: {err}"))?;
        client
            .SetEventHandle(event)
            .map_err(|err| format!("could not arm the render event: {err}"))?;

        let frames = client
            .GetBufferSize()
            .map_err(|err| format!("could not read the render buffer size: {err}"))?;

        let render: IAudioRenderClient = client
            .GetService()
            .map_err(|err| format!("could not get IAudioRenderClient: {err}"))?;

        Ok((client, render, frames, format, event, name))
    }
}

/// Chooses the capture endpoint: the configured one, else the virtual card,
/// else the current system default.
fn resolve_source(config: &Config, status: &EngineStatus) -> Result<DeviceInfo, String> {
    // Probed once, up front, and recorded from the hardware rather than from
    // whichever endpoint wins below. The tray and the panel read this as "is the
    // virtual card installed?", and forcing a different capture source must not
    // make the card look absent.
    let virtual_card = device::find_virtual_device();
    status
        .virtual_present
        .store(virtual_card.is_some(), Ordering::Relaxed);

    if let Some(id) = &config.source_device_id {
        if let Some(info) = device::device_by_id(id) {
            if info.state == device::DeviceState::Active {
                return Ok(info);
            }
            log::warn!("configured source {} is not active; falling back", id);
        }
    }

    if let Some(virtual_card) = virtual_card {
        return Ok(virtual_card);
    }

    let fallback = device::default_render_device()
        .map_err(|err| format!("no default render endpoint: {err}"))?;
    log::warn!(
        "FxSound's virtual card is not installed; capturing '{}' directly. \
         Nothing will be enhanced until the card is installed — see the tray menu.",
        fallback.name
    );
    Ok(fallback)
}

/// Chooses the render endpoint: the configured one, else the one the takeover
/// displaced, else the current default, else the first physical device.
///
/// The middle case is the one that is easy to get wrong. Once [`crate::routing`]
/// has pointed the *system default* at the virtual card, "the current default"
/// is the very device we are capturing from — useless as a render target. The
/// only correct answer is the physical output that was default before the
/// switch, which is why the takeover records it.
fn resolve_sink(config: &Config, source: &DeviceInfo) -> Result<DeviceInfo, String> {
    if let Some(id) = &config.sink_device_id {
        if let Some(info) = device::device_by_id(id) {
            if info.state == device::DeviceState::Active && info.id != source.id {
                return Ok(info);
            }
        }
    }

    if let Some(id) = &config.previous_default_id {
        if let Some(info) = device::device_by_id(id) {
            if info.state == device::DeviceState::Active && info.id != source.id && !info.is_virtual {
                return Ok(info);
            }
        }
    }

    if let Ok(default) = device::default_render_device() {
        if default.id != source.id && !default.is_virtual {
            return Ok(default);
        }
    }

    let devices = device::render_devices().map_err(|err| format!("enumeration failed: {err}"))?;
    devices
        .into_iter()
        .filter(|device| device.id != source.id && !device.is_virtual)
        .min_by_key(|device| device.name.to_lowercase())
        .ok_or_else(|| {
            "no physical output device to render to — install a sound card or plug in headphones"
                .to_owned()
        })
}

/// Opens both streams and returns a ready-to-run graph.
fn build_graph(config: &Config, status: &EngineStatus) -> Result<(Graph, Events), String> {
    let source = resolve_source(config, status)?;
    let sink = resolve_sink(config, &source)?;

    if source.id == sink.id {
        return Err(
            "the capture and render endpoints are the same device, which would feed back on itself; \
             install the virtual sound card or pick a different output"
                .to_owned(),
        );
    }

    let buffer_ms = config.buffer_ms.clamp(20, 500);

    let (capture_client, capture, source_format, capture_event, source_name) =
        build_capture(&source.id, buffer_ms)?;

    let (render_client, render, render_buffer_frames, sink_format, render_event, sink_name) =
        match build_render(&sink.id, buffer_ms) {
            Ok(parts) => parts,
            Err(err) => {
                // SAFETY: capture_client owns capture_event; close it before
                // unwinding so we do not leak the handle.
                unsafe {
                    let _ = windows::Win32::Foundation::CloseHandle(capture_event);
                }
                return Err(err);
            }
        };

    // The engine works on a single channel count; pick the wider of the two and
    // map both ends onto it.
    let dsp_channels = source_format.channels.max(sink_format.channels) as usize;

    let graph = Graph {
        capture_client,
        render_client,
        capture,
        render,
        source_format,
        sink_format,
        render_buffer_frames: render_buffer_frames as usize,
        dsp_channels,
    };

    *status.source.lock().unwrap() = Some(format!("{source_name} ({source_format})"));
    *status.sink.lock().unwrap() = Some(format!("{sink_name} ({sink_format})"));

    log::info!(
        "audio graph: {} -> {} ({} -> {}, {} ch)",
        describe(&source),
        describe(&sink),
        source_format,
        sink_format,
        dsp_channels
    );

    if source_format.rate != sink_format.rate {
        log::warn!(
            "sample rate mismatch: capturing at {} Hz and rendering at {} Hz; \
             {}",
            source_format.rate,
            sink_format.rate,
            if config.resample_on_rate_mismatch {
                "resampling with a linear interpolator"
            } else {
                "DSP bypassed — audio will pass through unprocessed"
            }
        );
    }

    Ok((
        graph,
        Events {
            capture: capture_event,
            render: render_event,
        },
    ))
}

/// Applies the shared parameters to the engine, but only when they changed.
struct ParamApplier {
    last_generation: u64,
    applied_power: Option<bool>,
    applied_eq_on: Option<bool>,
    applied_bands: usize,
    applied_effects: [Option<f32>; ffi::DFX_EFFECT_COUNT as usize],
    applied_gains: [Option<f32>; MAX_BANDS],
    applied_balance: Option<f32>,
    applied_master: Option<f32>,
    applied_normalization: Option<f32>,
    applied_volume_leveling: Option<f32>,
    applied_filter_q: Option<f32>,
}

impl ParamApplier {
    fn new() -> Self {
        Self {
            last_generation: 0,
            applied_power: None,
            applied_eq_on: None,
            applied_bands: 0,
            applied_effects: [None; ffi::DFX_EFFECT_COUNT as usize],
            applied_gains: [None; MAX_BANDS],
            applied_balance: None,
            applied_master: None,
            applied_normalization: None,
            applied_volume_leveling: None,
            applied_filter_q: None,
        }
    }

    /// Pushes changed values into the engine.
    ///
    /// Cheap in the common case: one atomic load and an early return.
    fn apply(&mut self, dsp: &Dsp, params: &SharedParams) {
        let generation = params.generation();
        if generation == self.last_generation {
            return;
        }
        self.last_generation = generation;

        let power = params.is_enabled();
        if self.applied_power != Some(power) {
            dsp.set_power(power);
            self.applied_power = Some(power);
        }

        let eq_on = params.eq_on.load(Ordering::Relaxed);
        if self.applied_eq_on != Some(eq_on) {
            dsp.set_eq_on(eq_on);
            self.applied_eq_on = Some(eq_on);
        }

        let bands = params.num_bands();
        if self.applied_bands != bands {
            dsp.set_num_bands(bands as i32);
            self.applied_bands = bands;
            // Force a full gain refresh: the band count changed the grid.
            self.applied_gains = [None; MAX_BANDS];

            // Publish the new grid. `set_num_bands` recomputes every centre
            // frequency inside the engine, and the panel draws its curve from
            // `band_freq`; without this read-back those slots keep whatever the
            // previous grid left there (zero, on a fresh start), and the plot
            // bails out as empty. `apply_preset` does the same thing for the
            // same reason.
            for band in 0..bands.min(MAX_BANDS) {
                params.set_band_freq(band, dsp.band_freq(band as i32) as f32);
                params.set_band_gain(band, dsp.band_gain(band as i32) as f32);
            }
        }

        for effect in 0..ffi::DFX_EFFECT_COUNT {
            let value = params.effect(effect);
            if self.applied_effects[effect as usize] != Some(value) {
                dsp.set_effect(effect, f64::from(value));
                self.applied_effects[effect as usize] = Some(value);
            }
        }

        for band in 0..bands.min(MAX_BANDS) {
            let gain = params.band_gain(band);
            if self.applied_gains[band] != Some(gain) {
                dsp.set_band_gain(band as i32, f64::from(gain));
                self.applied_gains[band] = Some(gain);
            }
        }

        macro_rules! sync {
            ($field:ident, $getter:ident, $setter:ident) => {{
                let value = params.$getter();
                if self.$field != Some(value) {
                    dsp.$setter(f64::from(value));
                    self.$field = Some(value);
                }
            }};
        }

        sync!(applied_balance, balance, set_balance);
        sync!(applied_master, master_gain, set_master_gain);
        sync!(applied_normalization, normalization, set_normalization);
        sync!(applied_volume_leveling, volume_leveling, set_volume_leveling);
        sync!(applied_filter_q, filter_q, set_filter_q);
    }
}

/// The preset that should be active, plus the file it came from.
///
/// Held by the audio thread across graph rebuilds: a rebuild makes a fresh
/// `Dsp`, so the last loaded preset has to be re-applied. Keeping it here also
/// means a `.fac` chosen while the graph was down is not lost.
type PendingPreset = Option<(PathBuf, crate::preset::FacPreset)>;

/// The audio thread entry point.
///
/// Runs the graph, and restarts it when something invalidates it. Never
/// returns until shutdown is requested, so the tray can rely on the engine
/// being alive.
fn run_audio_thread(
    config: Config,
    params: Arc<SharedParams>,
    status: Arc<EngineStatus>,
    shutdown: Arc<AtomicBool>,
    commands: Receiver<EngineCommand>,
) {
    // MTA, not STA: this thread has no message loop to pump.
    let _com = match ComGuard::mta() {
        Ok(guard) => guard,
        Err(err) => {
            status.set_error(format!("could not initialise COM on the audio thread: {err}"));
            return;
        }
    };

    // The watcher is created on this thread so its callbacks can be received
    // here without any cross-thread plumbing.
    let watcher = match DeviceWatcher::new() {
        Ok(watcher) => Some(watcher),
        Err(err) => {
            log::warn!("hot-plug notifications unavailable ({err}); rebuilds will be manual");
            None
        }
    };

    // The DSP is only ever touched from this thread.
    if let Err(err) = set_audio_thread_priority() {
        log::debug!("could not raise the audio thread priority: {err}");
    }

    let mut reason = RebuildReason::Startup;
    let mut pending_preset: PendingPreset = None;

    while !shutdown.load(Ordering::Relaxed) {
        // A fresh engine per graph: the DSP caches the signal format and
        // filter state, and reusing it across an endpoint change has bitten
        // upstream's own code (the mono Bluetooth crash).
        let dsp = match Dsp::new() {
            Some(dsp) => dsp,
            None => {
                status.set_error("could not allocate the DSP engine");
                return;
            }
        };

        match build_graph(&config, &status) {
            Ok((graph, events)) => {
                log::info!("audio graph ready ({reason})");
                status.rebuilds.fetch_add(1, Ordering::Relaxed);
                status.clear_error();
                status.running.store(true, Ordering::Relaxed);

                dsp.set_signal_format(
                    32,
                    graph.dsp_channels as i32,
                    graph.source_format.rate as i32,
                    graph.source_format.valid_bits as i32,
                );

                // Re-apply the preset: this `Dsp` has never seen it.
                if let Some((path, preset)) = pending_preset.clone() {
                    apply_preset(&dsp, &params, &path, &preset);
                }

                let outcome = run_graph(
                    &graph,
                    &events,
                    &dsp,
                    &config,
                    &params,
                    &status,
                    &shutdown,
                    &commands,
                    watcher.as_ref(),
                    &mut pending_preset,
                );

                status.running.store(false, Ordering::Relaxed);

                match outcome {
                    Some(next) => reason = next,
                    None => break, // shutdown was requested
                }
            }
            Err(err) => {
                // Without a graph there is nothing to process, but the thread
                // stays alive so the user can still fix the endpoints and the
                // engine will pick the change up.
                log::error!("audio graph unavailable: {err}");
                status.set_error(err);
                status.running.store(false, Ordering::Relaxed);
                if !wait_for_retry(&shutdown, &commands, &mut reason, &mut pending_preset) {
                    break;
                }
            }
        }
    }

    log::info!("audio thread stopped");
}

/// Waits for a retry trigger after a failed build.
///
/// Returns `false` when the thread should exit. Preset commands are absorbed
/// rather than dropped, so a `.fac` chosen while the engine is down still takes
/// effect once a graph exists.
fn wait_for_retry(
    shutdown: &AtomicBool,
    commands: &Receiver<EngineCommand>,
    reason: &mut RebuildReason,
    pending_preset: &mut PendingPreset,
) -> bool {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return false;
        }
        match commands.recv_timeout(Duration::from_millis(500)) {
            Ok(EngineCommand::Shutdown) => return false,
            Ok(EngineCommand::Rebuild) => {
                *reason = RebuildReason::Requested;
                return true;
            }
            Ok(EngineCommand::LoadPreset { path, preset }) => {
                *pending_preset = Some((path, preset));
                continue;
            }
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                *reason = RebuildReason::Error;
                return true;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Runs one graph until it fails or is invalidated. Returns the reason for the
/// next rebuild, or `None` when the thread should stop.
#[allow(clippy::too_many_arguments)]
fn run_graph(
    graph: &Graph,
    events: &Events,
    dsp: &Dsp,
    config: &Config,
    params: &SharedParams,
    status: &EngineStatus,
    shutdown: &AtomicBool,
    commands: &Receiver<EngineCommand>,
    watcher: Option<&DeviceWatcher>,
    pending_preset: &mut PendingPreset,
) -> Option<RebuildReason> {
    let mut applier = ParamApplier::new();
    applier.apply(dsp, params);

    // Start both streams. Render first: a capture-only graph would fill the
    // ring with nowhere to drain it.
    // SAFETY: both clients are live and initialised.
    unsafe {
        if let Err(err) = graph.render_client.Start() {
            status.set_error(format!("could not start rendering: {err}"));
            return Some(RebuildReason::Error);
        }
        if let Err(err) = graph.capture_client.Start() {
            status.set_error(format!("could not start capture: {err}"));
            let _ = graph.render_client.Stop();
            return Some(RebuildReason::Error);
        }
    }

    let ring_frames = (u64::from(graph.sink_format.rate) * u64::from(config.buffer_ms.max(20))
        / 1000) as usize;
    let mut ring = RingBuffer::new(ring_frames.max(1024), graph.dsp_channels);

    // Scratch buffers, allocated once. The audio loop must not allocate.
    let mut decoded: Vec<f32> = Vec::with_capacity(64 * 1024);
    let mut mapped: Vec<f32> = Vec::with_capacity(64 * 1024);
    let mut resampled: Vec<f32> = Vec::with_capacity(64 * 1024);
    let mut render_scratch: Vec<f32> = Vec::with_capacity(64 * 1024);
    let mut encoded: Vec<u8> = Vec::with_capacity(256 * 1024);

    let mut resampler = (graph.source_format.rate != graph.sink_format.rate
        && config.resample_on_rate_mismatch)
        .then(|| {
            StreamResampler::new(
                graph.source_format.rate,
                graph.sink_format.rate,
                graph.dsp_channels,
            )
        });

    let bypass_dsp = config.bypass_mono_devices
        && (graph.source_format.channels == 1 || graph.sink_format.channels == 1);

    if bypass_dsp {
        log::warn!(
            "endpoint is mono ({} in / {} out); DSP bypassed, as upstream does for the same case",
            graph.source_format.channels,
            graph.sink_format.channels
        );
    }

    // Set on every path that leaves the loop below, so the `None` here only
    // pins the type; rustc would otherwise warn the assignment is unread.
    #[allow(unused_assignments)]
    let mut next_reason: Option<RebuildReason> = None;
    let mut meter_tick: u32 = 0;

    'outer: loop {
        if shutdown.load(Ordering::Relaxed) {
            return None;
        }

        // 1. Drain commands. Never blocks.
        loop {
            match commands.try_recv() {
                Ok(EngineCommand::Shutdown) => {
                    return None;
                }
                Ok(EngineCommand::Rebuild) => {
                    next_reason = Some(RebuildReason::Requested);
                    break 'outer;
                }
                Ok(EngineCommand::LoadPreset { path, preset }) => {
                    apply_preset(dsp, params, &path, &preset);
                    *pending_preset = Some((path, preset));
                    applier.last_generation = 0; // force a refresh
                }
                Ok(EngineCommand::SavePreset { name, path }) => {
                    // `path` is the *directory*: the engine appends ".fac" to
                    // the name and joins the two itself (see `valsSave`), so
                    // passing a full file path would land the file in a
                    // subdirectory named after the preset.
                    let ok = dsp.save_preset(&name, &path);
                    if !ok {
                        log::error!("could not write preset {name:?} to {}", path.display());
                    }
                    // Reported back so the panel can say what happened: the
                    // write happens here, on the audio thread, and the UI has
                    // no other way to tell success from failure.
                    status.note_preset_written(format!("{name}.fac"), ok);
                }
                Ok(EngineCommand::SetEnabled(on)) => params.set_enabled(on),
                Ok(EngineCommand::SetEqOn(on)) => params.set_eq_on(on),
                Ok(EngineCommand::SetNumBands(bands)) => params.set_num_bands(bands),
                Ok(EngineCommand::SetBandGain { band, db }) => params.set_band_gain(band, db),
                Ok(EngineCommand::SetEffect { effect, slider }) => {
                    params.set_effect(effect, slider)
                }
                Ok(EngineCommand::SetBalance(db)) => params.set_balance(db),
                Ok(EngineCommand::SetMasterGain(db)) => params.set_master_gain(db),
                Ok(EngineCommand::SetNormalization(db)) => params.set_normalization(db),
                Ok(EngineCommand::SetVolumeLeveling(db)) => params.set_volume_leveling(db),
                Ok(EngineCommand::SetFilterQ(q)) => params.set_filter_q(q),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return None,
            }
        }

        // 2. Hot-plug events: only take the cheap decision here, and let the
        //    rebuild happen through the normal path.
        if let Some(watcher) = watcher {
            while let Some(event) = watcher.try_recv() {
                if !event.requires_rebuild() {
                    continue;
                }
                match event {
                    DeviceEvent::StateChanged { endpoint_id } => {
                        let gone = endpoint_id
                            .as_deref()
                            .is_some_and(|id| device::device_by_id(id).is_none());
                        if gone {
                            next_reason = Some(RebuildReason::DeviceUnplugged);
                            break 'outer;
                        }
                    }
                    DeviceEvent::DefaultChanged { .. } => {
                        next_reason = Some(RebuildReason::DefaultDeviceChanged);
                        break 'outer;
                    }
                    DeviceEvent::PropertyChanged { .. } => {}
                }
            }
        }

        // 3. Apply changed parameters.
        applier.apply(dsp, params);

        // 4. Wait for either stream, or the housekeeping timeout.
        // SAFETY: both handles are live for the duration of `events`.
        let handles = [events.capture, events.render];
        let signalled = unsafe { WaitForMultipleObjects(&handles, false, WAIT_TIMEOUT_MS) };

        let capture_ready = signalled == WAIT_OBJECT_0
            || signalled == WAIT_EVENT(WAIT_OBJECT_0.0 + 1)
            || signalled == WAIT_TIMEOUT;
        let render_ready = signalled == WAIT_EVENT(WAIT_OBJECT_0.0 + 1)
            || signalled == WAIT_TIMEOUT;

        // 5. Drain the capture side into the ring.
        if capture_ready {
            if let Err(err) = pump_capture(
                graph,
                &mut ring,
                &mut decoded,
                &mut mapped,
                &mut resampled,
                resampler.as_mut(),
                params,
                status,
            ) {
                log::error!("capture failed: {err}");
                status.set_error(err);
                next_reason = Some(RebuildReason::Error);
                break 'outer;
            }
        }

        // 6. Fill the render side from the ring.
        if render_ready {
            if let Err(err) = pump_render(
                graph,
                &mut ring,
                &mut render_scratch,
                &mut encoded,
                dsp,
                bypass_dsp,
                status,
            ) {
                log::error!("render failed: {err}");
                status.set_error(err);
                next_reason = Some(RebuildReason::Error);
                break 'outer;
            }

            let occupancy_frames = ring.available() as u64;
            let millis = occupancy_frames * 1000 / u64::from(graph.sink_format.rate.max(1));
            status
                .latency_ms
                .store(millis.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);

            // The spectrum comes from inside the engine, so it can only be read
            // here. Eight render cycles is roughly 20 ms at a 128-frame buffer —
            // fast enough to look live, slow enough to be free.
            meter_tick = meter_tick.wrapping_add(1);
            if meter_tick.is_multiple_of(8) {
                let mut bars = [0.0f32; SPECTRUM_BANDS];
                dsp.spectrum(&mut bars);
                for (index, value) in bars.iter().enumerate() {
                    store_f32(&status.spectrum[index], value.clamp(0.0, 1.0));
                }
            }
        }
    }

    // SAFETY: both clients were started above and are still live. `Reset` is
    // called on the capture client only — the render client is discarded with
    // the graph, so it needs no reset.
    unsafe {
        let _ = graph.capture_client.Stop();
        let _ = graph.render_client.Stop();
        let _ = graph.capture_client.Reset();
    }
    next_reason
}

/// Applies a parsed preset: engine first, then mirror the values into the
/// shared parameters so the UI shows what is actually playing.
fn apply_preset(
    dsp: &Dsp,
    params: &SharedParams,
    path: &std::path::Path,
    preset: &crate::preset::FacPreset,
) {
    if !dsp.load_preset(path) {
        log::error!("the engine rejected preset {}", path.display());
        return;
    }

    // The engine is the source of truth for the result, not the file: the
    // loader maps a 10-band curve onto the current grid and applies its own
    // quantisation. Read back rather than assuming.
    params.set_eq_on(true);
    for effect in 0..ffi::DFX_EFFECT_COUNT {
        let value = dsp.effect(effect) as f32 * 10.0; // normalised -> slider
        params.set_effect(effect, value);
    }
    let bands = dsp.num_bands() as usize;
    params.set_num_bands(bands);
    for band in 0..bands.min(MAX_BANDS) {
        params.set_band_gain(band, dsp.band_gain(band as i32) as f32);
        params.set_band_freq(band, dsp.band_freq(band as i32) as f32);
    }
    params.set_balance(dsp.balance() as f32);
    params.set_master_gain(dsp.master_gain() as f32);
    params.set_normalization(dsp.normalization() as f32);
    params.set_volume_leveling(dsp.volume_leveling() as f32);
    params.set_filter_q(dsp.filter_q() as f32);

    log::info!(
        "preset '{}' loaded ({} bands, {})",
        preset.name,
        bands,
        preset.effect_summary()
    );
}

/// Reads every available capture packet into the ring.
#[allow(clippy::too_many_arguments)]
fn pump_capture(
    graph: &Graph,
    ring: &mut RingBuffer,
    decoded: &mut Vec<f32>,
    mapped: &mut Vec<f32>,
    resampled: &mut Vec<f32>,
    mut resampler: Option<&mut StreamResampler>,
    _params: &SharedParams,
    status: &EngineStatus,
) -> Result<(), String> {
    let mut packets = 0usize;

    loop {
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;

        // SAFETY: `data` receives a pointer into the capture buffer that is
        // valid until ReleaseBuffer, which is always called below.
        let result = unsafe {
            graph
                .capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
        };

        if let Err(err) = result {
            if err.code().0 == AUDCLNT_S_BUFFER_EMPTY {
                break;
            }
            return Err(format!("capture GetBuffer failed: {err}"));
        }

        if frames == 0 {
            // SAFETY: 0 frames released is a no-op but keeps the contract tidy.
            unsafe { graph.capture.ReleaseBuffer(0) }
                .map_err(|err| format!("capture ReleaseBuffer failed: {err}"))?;
            break;
        }

        decoded.clear();
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
            decoded.resize(frames as usize * graph.source_format.channels as usize, 0.0);
        } else {
            // SAFETY: `data` points at frames * channels samples of the
            // endpoint's native format, as reported in source_format.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    data as *const u8,
                    frames as usize * graph.source_format.frame_bytes(),
                )
            };
            decode_to_f32(bytes, graph.source_format, decoded);
        }

        // SAFETY: frames came from GetBuffer and have not been released yet.
        unsafe { graph.capture.ReleaseBuffer(frames) }
            .map_err(|err| format!("capture ReleaseBuffer failed: {err}"))?;

        packets += 1;

        // Channel-map, then resample, then push.
        mapped.clear();
        map_channels(
            decoded,
            graph.source_format.channels as usize,
            graph.dsp_channels,
            mapped,
        );

        let staged: &[f32] = match resampler.as_deref_mut() {
            Some(resampler) => {
                resampled.clear();
                resampler.process(mapped, resampled);
                resampled
            }
            None => mapped,
        };

        for frame in staged.chunks_exact(graph.dsp_channels) {
            if !ring.push_frame(frame) {
                status.overruns.fetch_add(1, Ordering::Relaxed);
            }
            status.captured_frames.fetch_add(1, Ordering::Relaxed);
        }
    }

    if packets == 0 {
        log::trace!("capture produced nothing this cycle");
    }
    Ok(())
}

/// Fills the render buffer from the ring, running the DSP over it in place.
fn pump_render(
    graph: &Graph,
    ring: &mut RingBuffer,
    scratch: &mut Vec<f32>,
    encoded: &mut Vec<u8>,
    dsp: &Dsp,
    bypass_dsp: bool,
    status: &EngineStatus,
) -> Result<(), String> {
    // SAFETY: live client.
    let padding = unsafe { graph.render_client.GetCurrentPadding() }
        .map_err(|err| format!("GetCurrentPadding failed: {err}"))?;

    let available = graph
        .render_buffer_frames
        .saturating_sub(padding as usize);
    if available == 0 {
        return Ok(());
    }

    let frames = available.min(MAX_UNDERRUN_SILENCE_FRAMES);

    // SAFETY: frames <= the buffer size, which is what GetBuffer requires.
    let raw = unsafe { graph.render.GetBuffer(frames as u32) }
        .map_err(|err| format!("render GetBuffer failed: {err}"))?;

    scratch.clear();
    scratch.resize(frames * graph.dsp_channels, 0.0);

    let got = ring.pop_frames(frames, scratch);
    if got < frames {
        // Underrun: the tail stays whatever `resize` filled it with.
        status
            .underruns
            .fetch_add((frames - got) as u64, Ordering::Relaxed);
    }

    if !bypass_dsp {
        dsp.process(scratch, frames as i32);
    }

    // Metering. The peak decays between blocks so a transient stays visible
    // for a few frames instead of flickering at block rate, and over-full-scale
    // samples are counted rather than silently clamped — a user pushing Bass to
    // 10 wants to know the output is being limited.
    let mut peak = 0.0f32;
    let mut clipped = 0u64;
    for sample in scratch.iter() {
        if *sample > 1.0 || *sample < -1.0 {
            clipped += 1;
        }
        let magnitude = sample.abs();
        if magnitude > peak {
            peak = magnitude;
        }
    }
    if clipped > 0 {
        status.clipped_samples.fetch_add(clipped, Ordering::Relaxed);
    }
    let decayed = (status.peak_level() * 0.85).max(peak.min(1.0));
    store_f32(&status.peak_level, decayed);

    // Map the engine's channel count onto the sink's, then encode.
    let sink_channels = graph.sink_format.channels as usize;

    encoded.clear();
    encoded.resize(frames * graph.sink_format.frame_bytes(), 0);

    if graph.dsp_channels == sink_channels {
        encode_from_f32(scratch, graph.sink_format, encoded);
    } else {
        let mut remapped = Vec::with_capacity(frames * sink_channels);
        map_channels(scratch, graph.dsp_channels, sink_channels, &mut remapped);
        encode_from_f32(&remapped, graph.sink_format, encoded);
    }

    // SAFETY: `raw` points at `frames` frames of writable render memory, and
    // `encoded` was sized to exactly that.
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), raw, encoded.len());
    }

    // SAFETY: `frames` frames were written; releasing them hands them to the
    // audio engine.
    unsafe { graph.render.ReleaseBuffer(frames as u32, 0) }
        .map_err(|err| format!("render ReleaseBuffer failed: {err}"))?;

    status
        .rendered_frames
        .fetch_add(frames as u64, Ordering::Relaxed);
    Ok(())
}

/// Raises this thread's scheduling priority and joins the audio class.
///
/// Failure is not fatal: the graph still runs, just with more jitter under
/// load.
fn set_audio_thread_priority() -> Result<(), String> {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };

    // SAFETY: pseudo-handle for the current thread; always valid.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)
            .map_err(|err| format!("SetThreadPriority: {err}"))?;
    }

    // avrt.dll is loaded on demand: it is present everywhere Vista+ but
    // linking it statically would force the dependency at load time.
    join_audio_class();
    Ok(())
}

/// Calls `AvSetMmThreadCharacteristics("Audio")` if `avrt.dll` is available.
///
/// Resolved dynamically because `avrt` is not in every toolchain's import
/// libraries, and its absence is not a reason to refuse to start.
fn join_audio_class() {
    use windows::core::PCSTR;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    type AvSetMmThreadCharacteristicsW = unsafe extern "system" fn(PCWSTR, *mut u32) -> HANDLE;

    let name = ffi::to_wide(std::ffi::OsStr::new("avrt.dll"));
    // SAFETY: `name` is NUL-terminated. The module stays loaded for the lifetime
    // of the process, which is what we want.
    let module = match unsafe { LoadLibraryW(PCWSTR(name.as_ptr())) } {
        Ok(module) => module,
        Err(_) => return,
    };

    // The export name is ASCII, so a byte literal is both simpler and immune
    // to macro availability differences across `windows` versions.
    const SYMBOL: &[u8] = b"AvSetMmThreadCharacteristicsW\0";
    // `FARPROC` is `Option<fn>`, so a missing export arrives as `None` rather
    // than an error code.
    // SAFETY: `SYMBOL` is NUL-terminated and `module` is a live HMODULE.
    let Some(address) = (unsafe { GetProcAddress(module, PCSTR(SYMBOL.as_ptr())) }) else {
        return;
    };

    // SAFETY: the resolved symbol has the documented signature for this export.
    let function: AvSetMmThreadCharacteristicsW = unsafe { std::mem::transmute(address) };
    let task = ffi::to_wide(std::ffi::OsStr::new("Audio"));
    let mut index = 0u32;
    // SAFETY: valid NUL-terminated task name and a valid out-pointer.
    let handle = unsafe { function(PCWSTR(task.as_ptr()), &mut index) };
    if handle.is_invalid() {
        log::debug!("AvSetMmThreadCharacteristics(\"Audio\") was refused");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that construct a [`Dsp`].
    ///
    /// The vendored engine keeps its band count in a **process-global**, not in
    /// the handle: `DFXP_GRAPHIC_EQ_NUM_BANDS` (`DfxDspEq.cpp:32`) is written by
    /// `GraphicEqSetNumBands` (`GraphicEqSet.cpp:154`) and read by every
    /// `dfxpEq` entry point. Two engines in one process therefore share that
    /// one integer, and a `sos` resize can land while another handle is reading
    /// the arrays it governs. The symptom is a hang: the tests below run in
    /// parallel by default and two of them wedged for over a minute apiece
    /// before this lock existed, while each passed on its own.
    ///
    /// `ffi::Dsp` already documents that the engine must be driven from a single
    /// thread. This is the stricter condition that it also cannot be
    /// *instantiated* twice at once, which only ever bites in tests — the app
    /// builds exactly one engine, on its audio thread.
    ///
    /// A poisoned lock is tolerated rather than propagated: the point is
    /// mutual exclusion, and failing a second test because the first one
    /// panicked would hide its own result.
    static ENGINE_LOCK: Mutex<()> = Mutex::new(());

    /// Runs `body` with no other engine-owning test in flight.
    fn with_engine<T>(body: impl FnOnce() -> T) -> T {
        let _guard = ENGINE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        body()
    }

    #[test]
    fn resampler_preserves_duration() {
        let mut resampler = StreamResampler::new(48_000, 44_100, 2);

        // 1000 frames at 48 kHz should come out as ~919 frames at 44.1 kHz.
        let input: Vec<f32> = (0..2000).map(|index| (index % 100) as f32 / 100.0).collect();
        let mut output = Vec::new();
        resampler.process(&input, &mut output);

        let frames = output.len() / 2;
        assert!(
            (900..=940).contains(&frames),
            "expected about 919 frames, got {frames}"
        );
    }

    #[test]
    fn resampler_is_transparent_at_equal_rates() {
        let mut resampler = StreamResampler::new(48_000, 48_000, 2);
        let input = vec![0.25f32; 64];
        let mut output = Vec::new();
        resampler.process(&input, &mut output);
        assert_eq!(output, input);
    }

    #[test]
    fn ring_buffer_reports_the_oldest_frame_when_full() {
        let mut ring = RingBuffer::new(2, 1);
        assert!(ring.push_frame(&[1.0]));
        assert!(ring.push_frame(&[2.0]));
        // Third push must evict 1.0 and report the drop.
        assert!(!ring.push_frame(&[3.0]));

        let mut out = [0.0f32; 2];
        assert_eq!(ring.pop_frames(2, &mut out), 2);
        assert_eq!(out, [2.0, 3.0]);
    }

    #[test]
    fn mono_to_stereo_duplicates() {
        let mut out = Vec::new();
        map_channels(&[0.5, 0.25], 1, 2, &mut out);
        assert_eq!(out, [0.5, 0.5, 0.25, 0.25]);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let mut out = Vec::new();
        map_channels(&[1.0, 0.0, 0.5, 0.5], 2, 1, &mut out);
        assert_eq!(out, [0.5, 0.5]);
    }

    #[test]
    fn encode_clamps_instead_of_wrapping() {
        let format = StreamFormat {
            rate: 48_000,
            channels: 1,
            container_bits: 16,
            valid_bits: 16,
            kind: SampleKind::Pcm16,
        };
        let mut out = [0u8; 4];
        encode_from_f32(&[4.0, -4.0], format, &mut out);
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), i16::MIN + 1);
    }

    #[test]
    fn params_round_trip_through_atomics() {
        let params = SharedParams::default();
        params.set_effect(ffi::DFX_EFFECT_BASS, 6.5);
        assert!((params.effect(ffi::DFX_EFFECT_BASS) - 6.5).abs() < f32::EPSILON);

        params.set_band_gain(3, -4.25);
        assert!((params.band_gain(3) + 4.25).abs() < f32::EPSILON);

        // Out-of-range indices must be ignored rather than panic: the audio
        // thread reads these.
        params.set_effect(99, 1.0);
        params.set_band_gain(999, 1.0);
    }

    /// Saving is the one place the panel's "save as preset" can go wrong
    /// silently, so the whole contract is pinned here.
    ///
    /// Two halves, both non-obvious:
    ///
    /// * [`EngineCommand::SavePreset`] carries a **directory**, because the
    ///   engine appends `.fac` to the name and joins the two itself. Handing it
    ///   a file path makes the write fail and, before this test existed, nothing
    ///   said so.
    /// * The bytes it writes have to come back through *our* parser, since that
    ///   parser is what builds the preset list the new file has to appear in.
    ///
    /// Needs no audio hardware — `Dsp::new` only allocates — so it runs in CI.
    #[test]
    fn saving_a_preset_writes_a_file_our_parser_can_read_back() {
        with_engine(|| {
            let dir = std::env::temp_dir().join(format!("fxtrumpet-save-check-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the scratch directory");

            let dsp = Dsp::new().expect("DfxDsp allocation failed");
            // Upstream documents that the first call after construction can
            // fail; `dspcheck` does the same two-call dance.
            dsp.set_signal_format(32, 2, 48_000, 32);
            dsp.set_signal_format(32, 2, 48_000, 32);

            // Something to save, so the file cannot be "correct" by being empty.
            dsp.set_effect(ffi::DFX_EFFECT_BASS, 7.0);

            // A space and a non-ASCII character on purpose: that is what the
            // panel hands over, and the engine has to write it as UTF-8 in the
            // name line.
            let name = "Save check 测试";
            assert!(
                dsp.save_preset(name, &dir),
                "save_preset refused a directory"
            );

            let written = dir.join(format!("{name}.fac"));
            assert!(
                written.is_file(),
                "save_preset(name, dir) should produce {:?} — if this fails, the \
                 argument meanings have changed and the panel is writing elsewhere",
                written
            );

            let parsed = crate::preset::FacPreset::from_file(&written).expect("our parser reads it");
            assert_eq!(parsed.name, name, "the name line did not survive the round trip");

            let bass = parsed
                .effect_slider_value(ffi::DFX_EFFECT_BASS)
                .expect("Bass has a Main slot");
            assert!(
                (bass - 7.0).abs() < 0.2,
                "the save did not capture the live state: Bass came back as {bass}, expected 7.0"
            );

            // And the trap itself: a *file* path lands one level too deep, so
            // the wrapper must refuse it up front. It has to return `false`
            // quickly — letting it reach the engine has been observed to hang,
            // which is why the guard lives in `ffi::Dsp::save_preset` and is
            // asserted here so a future change to either meaning is loud.
            assert!(
                !dsp.save_preset("nested", &dir.join("nested.fac")),
                "passing a file path to save_preset should be refused"
            );
            // Nothing must have been created a level too deep.
            assert!(!dir.join("nested.fac").exists());

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// Every band count the panel offers must come back with a full grid, on
    /// the way up *and* on the way back down.
    ///
    /// The panel draws its curve from `band_freq` and bails out to an empty
    /// plot the moment any band reports zero, so a grid that is only partially
    /// published is indistinguishable from no grid at all. That was the actual
    /// defect behind "15 and 31 bands come up blank": the band count took
    /// effect inside the engine, but the applier never copied the recomputed
    /// frequencies back into `SharedParams`, so the panel saw zeros.
    ///
    /// Drives the real [`ParamApplier`], not a copy of its body. An earlier
    /// version of this test called `set_num_bands` and the read-back inline and
    /// passed even with the fix removed, which made it worse than no test: it
    /// implied the applier was covered when nothing was exercising it. Going
    /// through `ParamApplier::apply` is the whole point — that call is what the
    /// audio thread makes.
    ///
    /// The walk covers three things one `Dsp` cannot be split across:
    ///
    /// * a forward pass through every offered count,
    /// * revisiting counts, which exercises the applier's "same as last time"
    ///   early-out, and
    /// * shrinking, which drives a different branch inside the vendored
    ///   `GraphicEqSetNumBands` (it remaps the old gains onto the smaller
    ///   layout, `GraphicEqSet.cpp:207-245`).
    ///
    /// One engine for all of it, deliberately. The vendored DSP is not
    /// multi-instance safe — see [`ENGINE_LOCK`] — and splitting these into
    /// three `#[test]`s that each built their own `Dsp` was measured to wedge
    /// the third one. The app itself builds exactly one engine and keeps it for
    /// the process's life, so one engine here is also the more faithful model.
    #[test]
    fn band_counts_round_trip_with_a_full_grid() {
        with_engine(|| {
            let dsp = Dsp::new().expect("DfxDsp allocation failed");
            dsp.set_signal_format(32, 2, 48_000, 32);
            dsp.set_signal_format(32, 2, 48_000, 32);

            let params = SharedParams::default();
            let mut applier = ParamApplier::new();

            // The forward pass, then a backward pass, then a few revisits.
            // 31 -> 20 is the case the panel's own list can produce by hand.
            let walk: Vec<usize> = crate::ui::panel::BAND_CHOICES
                .iter()
                .copied()
                .chain(crate::ui::panel::BAND_CHOICES.iter().rev().copied())
                .chain([5, 10, 5, 31, 31, 15, 5])
                .collect();

            for (step, chosen) in walk.iter().copied().enumerate() {
                params.set_num_bands(chosen);
                applier.apply(&dsp, &params);

                let bands = params.num_bands();
                assert_eq!(
                    bands, chosen,
                    "step {step}: the applier clamped {chosen} away"
                );

                for band in 0..bands {
                    let hz = params.band_freq(band);
                    assert!(
                        hz > 0.0,
                        "step {step}, {chosen} bands: band {band} came back as \
                         {hz} Hz, which makes the plot bail out as empty"
                    );
                }

                // Ascending: the plot sorts nothing, and a crossed grid would
                // draw a bow-tie. Cheap to check, and it is the invariant the
                // curve drawing relies on.
                let freqs: Vec<f32> = (0..bands).map(|band| params.band_freq(band)).collect();
                assert!(
                    freqs.windows(2).all(|pair| pair[0] < pair[1]),
                    "step {step}, {chosen} bands are not ascending: {freqs:?}"
                );

                // The engine has to agree with what we published, or the read
                // back above was measuring our own copy.
                assert_eq!(
                    dsp.num_bands() as usize,
                    chosen,
                    "step {step}: the applier never pushed {chosen} bands on"
                );
                for band in 0..bands {
                    let our = params.band_freq(band);
                    let theirs = dsp.band_freq(band as i32) as f32;
                    assert!(
                        (our - theirs).abs() < 0.5,
                        "step {step}, band {band}: we hold {our} Hz but the \
                         engine is at {theirs} Hz"
                    );
                }
            }
        });
    }
}
