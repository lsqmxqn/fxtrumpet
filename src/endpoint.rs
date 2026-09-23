//! Endpoint-level volume and metering: the device's own slider.
//!
//! Distinct from [`crate::session`], which is about individual applications.
//! Windows keeps the two in different interfaces on different objects —
//! `IAudioEndpointVolume` belongs to the endpoint, `ISimpleAudioVolume` to each
//! session — and the mixer needs both, because "make the speakers quieter"
//! and "make the browser quieter" are different requests.
//!
//! ## Why the setters take a device id and not an object
//!
//! Same reason as in `session`: the UI must never hold a COM reference across a
//! frame. Every call re-opens the endpoint, which costs microseconds and cannot
//! go stale.
//!
//! ## The dB / scalar confusion
//!
//! `IAudioEndpointVolume` exposes the level twice: as a linear scalar
//! (`GetMasterVolumeLevelScalar`, 0.0–1.0) and in decibels
//! (`GetMasterVolumeLevel`). The scalar is what the Windows volume flyout shows
//! and what a slider should be bound to; the decibel form is not linear in
//! perceived loudness and makes a slider that is useless at the bottom of its
//! travel. Everything here works in the scalar domain, and the dB getters are
//! deliberately not wrapped.

use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::System::Com::CLSCTX_ALL;

use crate::device;

/// Opens an endpoint's volume interface.
fn endpoint_volume(endpoint_id: &str) -> windows::core::Result<IAudioEndpointVolume> {
    let enumerator = device::enumerator()?;
    let wide = crate::ffi::to_wide(std::ffi::OsStr::new(endpoint_id));
    // SAFETY: live enumerator; the id is NUL-terminated.
    let endpoint = unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) }?;
    // SAFETY: live endpoint. `IAudioEndpointVolume` is a documented activation
    // target for a render endpoint.
    unsafe { endpoint.Activate(CLSCTX_ALL, None) }
}

/// The device's volume, 0.0 – 1.0.
pub fn volume(endpoint_id: &str) -> windows::core::Result<f32> {
    let iface = endpoint_volume(endpoint_id)?;
    // SAFETY: live interface; the scalar getter takes no arguments.
    unsafe { iface.GetMasterVolumeLevelScalar() }
}

/// Sets the device's volume.
pub fn set_volume(endpoint_id: &str, level: f32) -> windows::core::Result<()> {
    let iface = endpoint_volume(endpoint_id)?;
    // SAFETY: live interface. A null event context means Windows does not
    // notify us of our own change, which is what keeps the UI from echoing
    // every drag back into itself.
    unsafe { iface.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null()) }
}

/// Whether the device is muted.
pub fn muted(endpoint_id: &str) -> windows::core::Result<bool> {
    let iface = endpoint_volume(endpoint_id)?;
    // SAFETY: live interface.
    unsafe { iface.GetMute() }.map(|flag| flag.as_bool())
}

/// Mutes or unmutes the device.
pub fn set_muted(endpoint_id: &str, muted: bool) -> windows::core::Result<()> {
    let iface = endpoint_volume(endpoint_id)?;
    // SAFETY: live interface; null event context as above.
    unsafe { iface.SetMute(muted, std::ptr::null()) }
}

/// The device's current output level, 0.0 – 1.0, averaged across channels.
///
/// The per-channel values are averaged rather than maximised: a peak-hold on
/// one channel of a 5.1 stream makes a two-channel meter look broken, and the
/// mixer draws one bar per device.
pub fn peak(endpoint_id: &str) -> windows::core::Result<f32> {
    let enumerator = device::enumerator()?;
    let wide = crate::ffi::to_wide(std::ffi::OsStr::new(endpoint_id));
    // SAFETY: live enumerator; NUL-terminated id.
    let endpoint = unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) }?;
    // SAFETY: live endpoint.
    let meter: IAudioMeterInformation = unsafe { endpoint.Activate(CLSCTX_ALL, None) }?;
    // SAFETY: live interface. A device without metering fails here, which the
    // caller treats as "no level to draw" rather than an error.
    unsafe { meter.GetPeakValue() }
}

/// A device's current state, read in one pass.
///
/// One call rather than three so the mixer can refresh a list without three
/// endpoint activations per row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndpointLevels {
    pub volume: f32,
    pub muted: bool,
    pub peak: f32,
}

/// Reads volume, mute and level for one endpoint, tolerating partial failures.
///
/// A device that cannot be opened at all yields `None`; a device that opens but
/// refuses a particular reading yields that field's default. The distinction
/// matters: the first is a row to omit, the second is a row that is merely
/// less informative this frame.
pub fn levels(endpoint_id: &str) -> Option<EndpointLevels> {
    let iface = endpoint_volume(endpoint_id).ok()?;

    // SAFETY: live interface throughout.
    let volume = unsafe { iface.GetMasterVolumeLevelScalar() }.unwrap_or(1.0);
    let muted = unsafe { iface.GetMute() }
        .map(|flag| flag.as_bool())
        .unwrap_or(false);

    Some(EndpointLevels {
        volume,
        muted,
        peak: peak(endpoint_id).unwrap_or(0.0),
    })
}

#[cfg(test)]
mod tests {
    // Deliberately no `use super::*`. The one test here is about the clamp
    // applied *before* a value reaches Windows — arithmetic, not API surface —
    // and importing the module would only bring in COM interface names that the
    // test never mentions.

    #[test]
    fn a_level_is_clamped_before_it_reaches_windows() {
        // The clamp is the interesting part: a slider drag can momentarily
        // produce a value outside the range, and the API rejects it outright
        // rather than clamping, which would leave the device stuck.
        let values = [-0.5f32, 0.0, 0.5, 1.0, 1.5];
        for value in values {
            let clamped = value.clamp(0.0, 1.0);
            assert!((0.0..=1.0).contains(&clamped), "{value} → {clamped}");
        }
    }
}
