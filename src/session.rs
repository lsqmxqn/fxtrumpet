//! Audio sessions: one row per application stream, per endpoint.
//!
//! This is the backend of the mixer. Everything the user sees as "an app in the
//! list" is an `IAudioSessionControl2` on some render endpoint, wrapped here in
//! a plain struct so the UI never holds a COM reference.
//!
//! ## Why sessions are enumerated per endpoint, not globally
//!
//! Windows has no global session list. `IAudioSessionManager2` is obtained from
//! an `IMMDevice`, and its enumerator returns exactly the sessions rendering
//! into *that* endpoint. This is not an implementation detail we can wish away
//! — it is also how per-app routing becomes observable. When an app is routed
//! to a different device its session moves lists, so `device_id` on a
//! [`Session`] is the ground truth for "where is this app playing", and the
//! mixer did not have to be told anything to know it.
//!
//! ## Why the setters re-enumerate
//!
//! [`set_volume`] and [`set_mute`] look the session up by instance id every
//! time instead of caching `ISimpleAudioVolume` pointers. Caching would mean
//! holding COM references across UI frames and keeping them alive for as long
//! as an app runs; the moment one goes stale (the app restarts, the endpoint is
//! unplugged) the failure mode is a slider that silently does nothing. A
//! re-enumeration costs a fraction of a millisecond and happens only on a user
//! action, so the trade is all upside.
//!
//! ## Expired sessions and the S_FALSE trap
//!
//! A session in the `Expired` state is a stream the app has closed but Windows
//! has not reaped yet — the "zombie" EarTrumpet documents. They are filtered
//! out here rather than in the UI, because every consumer wants that answer.
//!
//! `IsSystemSoundsSession` is a different hazard: it answers through the
//! *HRESULT* (`S_OK` yes, `S_FALSE` no) rather than through an out-parameter.
//! Both are success codes, so a `Result`-shaped wrapper would report "yes" for
//! every session. It is therefore compared against `S_OK` explicitly, against
//! a raw `HRESULT`.

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    AudioSessionState, AudioSessionStateActive, AudioSessionStateExpired, IAudioSessionControl2,
    IAudioSessionEnumerator, IAudioSessionManager2, ISimpleAudioVolume,
};
use windows::Win32::System::Com::CLSCTX_ALL;

use crate::device;

/// Reported when the session a caller asked for has already gone.
///
/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`, written out rather than imported.
/// windows-rs does export an `E_NOTFOUND`, but only under
/// `Win32::Data::HtmlHelp` with a different value — reaching for that name here
/// would be a false lead for anyone reading the error path.
pub const E_SESSION_NOT_FOUND: windows::core::HRESULT =
    windows::core::HRESULT(0x8007_0490u32 as i32);

/// Windows' session state, narrowed to the three values the API defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// The stream is playing.
    Active,
    /// The stream exists but is not currently rendering.
    Inactive,
    /// Closed by the application; Windows has not reaped it yet.
    Expired,
}

impl From<AudioSessionState> for SessionState {
    fn from(state: AudioSessionState) -> Self {
        if state == AudioSessionStateActive {
            SessionState::Active
        } else if state == AudioSessionStateExpired {
            SessionState::Expired
        } else {
            SessionState::Inactive
        }
    }
}

/// One render stream on one endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The endpoint this stream is rendering into.
    pub device_id: String,
    /// `GetSessionInstanceIdentifier` — unique per stream, and the key the
    /// setters use to find this session again. `GetSessionIdentifier` is *not*
    /// unique (two streams from one app share it), which is exactly the
    /// distinction EarTrumpet's hidden per-session sliders exist to expose.
    pub instance_id: String,
    /// `GetSessionIdentifier` — groups the streams of one application.
    pub display_id: String,
    /// Owning process. `0` for the system-sounds pseudo session.
    pub process_id: u32,
    /// Windows' "System Sounds" session, which has no process behind it.
    pub is_system_sounds: bool,
    pub state: SessionState,
    /// 0.0 – 1.0.
    pub volume: f32,
    pub muted: bool,
    /// Peak amplitude of the stream, 0.0 – 1.0, for the meter.
    pub peak: f32,
}

impl Session {
    /// Whether this stream is worth putting on screen.
    ///
    /// Inactive sessions are kept: an app that is not currently playing is
    /// exactly the one a user opens the mixer to adjust *before* it starts.
    pub fn is_visible(&self) -> bool {
        self.state != SessionState::Expired
    }
}

/// Opens an endpoint and returns its session manager.
fn session_manager(endpoint_id: &str) -> windows::core::Result<IAudioSessionManager2> {
    let enumerator = device::enumerator()?;
    let wide = crate::ffi::to_wide(std::ffi::OsStr::new(endpoint_id));
    // SAFETY: live enumerator; the id is NUL-terminated. The endpoint is owned
    // by this scope and released on drop.
    let endpoint = unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }?;
    // SAFETY: live endpoint. `IAudioSessionManager2` is a documented activation
    // target for a render endpoint.
    unsafe { endpoint.Activate(CLSCTX_ALL, None) }
}

/// Walks the session list of one endpoint, calling `visit` for each.
///
/// The single traversal used by both the reader and the two setters, so they
/// cannot drift apart in how they decide a session is "the same session".
fn for_each_session(
    endpoint_id: &str,
    mut visit: impl FnMut(&IAudioSessionControl2) -> bool,
) -> windows::core::Result<()> {
    let manager = session_manager(endpoint_id)?;
    // SAFETY: live manager.
    let sessions: IAudioSessionEnumerator = unsafe { manager.GetSessionEnumerator() }?;
    // SAFETY: live enumerator.
    let count = unsafe { sessions.GetCount() }?;

    for index in 0..count {
        // SAFETY: `index < count`, verified above.
        let Ok(control) = (unsafe { sessions.GetSession(index) }) else {
            continue;
        };
        // A session that is not an `IAudioSessionControl2` has no process id and
        // no instance id, so there is nothing to do with it.
        let Ok(control) = control.cast::<IAudioSessionControl2>() else {
            continue;
        };
        if !visit(&control) {
            break;
        }
    }

    Ok(())
}

/// Reads everything interesting about one session control.
///
/// Returns `None` when the session is expired, or when its volume interface —
/// which is what makes a session worth showing — cannot be obtained.
fn read_session(control: &IAudioSessionControl2, device_id: &str) -> Option<Session> {
    // SAFETY: `control` is a live interface pointer.
    let state = SessionState::from(unsafe { control.GetState() }.ok()?);
    if state == SessionState::Expired {
        return None;
    }

    // SAFETY: live interface. `ISimpleAudioVolume` is reached by
    // QueryInterface on the session object.
    let volume: ISimpleAudioVolume = control.cast().ok()?;

    // SAFETY: live interface.
    let instance_id = take_com_string(unsafe { control.GetSessionInstanceIdentifier() }.ok()?)?;

    // SAFETY: live interface. Non-unique by design; used only to group rows.
    let display_id = unsafe { control.GetSessionIdentifier() }
        .ok()
        .and_then(take_com_string)
        .unwrap_or_else(|| instance_id.clone());

    // SAFETY: live interface. See the module docs — the answer arrives in the
    // HRESULT, and `S_FALSE` ("not the system sounds session") is a success
    // code, so this cannot be `.is_ok()`.
    let is_system_sounds = unsafe { control.IsSystemSoundsSession() } == S_OK;

    // SAFETY: live interface. Zero for the system-sounds session.
    let process_id = unsafe { control.GetProcessId() }.unwrap_or(0);

    // Meters are best-effort: a device that does not support metering fails
    // here, and the row is still perfectly usable without a level bar.
    let peak = control
        .cast::<IAudioMeterInformation>()
        .ok()
        .and_then(|meter| {
            // SAFETY: live interface.
            unsafe { meter.GetPeakValue() }.ok()
        })
        .unwrap_or(0.0);

    // SAFETY: live interface.
    let volume_level = unsafe { volume.GetMasterVolume() }.unwrap_or(1.0);

    Some(Session {
        device_id: device_id.to_owned(),
        instance_id,
        display_id,
        process_id,
        is_system_sounds,
        state,
        volume: volume_level,
        // SAFETY: live interface. `GetMute` reports through a BOOL
        // out-parameter, unlike `IsSystemSoundsSession`.
        muted: unsafe { volume.GetMute() }
            .map(|flag| flag.as_bool())
            .unwrap_or(false),
        peak,
    })
}

/// Copies a COM-allocated wide string out and frees the original.
///
/// The two id getters on `IAudioSessionControl2` return strings the *caller*
/// owns, and `PWSTR` has no `Drop` — so a plain `to_string()` copy leaks one
/// allocation per session per enumeration. That matters here in a way it would
/// not in a one-shot tool: the mixer re-enumerates every 120 ms for as long as
/// its window is open, and the volume setters go through the same path on every
/// frame of a slider drag.
///
/// Returns `None` only for malformed UTF-16, which Windows does not produce for
/// these ids.
fn take_com_string(pointer: windows::core::PWSTR) -> Option<String> {
    // SAFETY: `pointer` came from a getter that allocates with the task
    // allocator, and it is NUL-terminated for as long as it is alive. `to_string`
    // is unsafe because it dereferences the pointer; the copy happens before the
    // free below, and the freed memory is not read afterwards.
    let text = unsafe { pointer.to_string() }.ok();
    crate::process::free_com_string(pointer.0);
    text
}

/// Every live session rendering into `device_id`.
///
/// Requires COM on the calling thread. An endpoint that is not present yields
/// an empty list rather than an error: the mixer polls this continuously, and a
/// device disappearing between the enumeration and the query is normal.
pub fn sessions_on(device_id: &str) -> Vec<Session> {
    let mut out = Vec::new();

    let result = for_each_session(device_id, |control| {
        if let Some(session) = read_session(control, device_id) {
            out.push(session);
        }
        true
    });

    if let Err(err) = result {
        // A failed enumeration means the endpoint went away between the device
        // list being read and this call, which is normal on a machine whose
        // default device is being switched.
        log::debug!("could not list sessions on {device_id}: {err}");
    }

    out
}

/// Every live session on every active render endpoint.
///
/// Endpoints that fail to enumerate are skipped rather than failing the whole
/// call: one misbehaving device must not empty the mixer.
pub fn all_sessions() -> Vec<Session> {
    let devices = match device::render_devices() {
        Ok(devices) => devices,
        Err(err) => {
            log::warn!("could not enumerate render devices for the mixer: {err}");
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for endpoint in devices {
        out.append(&mut sessions_on(&endpoint.id));
    }
    out
}

/// Sets one stream's volume.
///
/// See the module docs for why this re-enumerates instead of caching.
pub fn set_volume(device_id: &str, instance_id: &str, level: f32) -> windows::core::Result<()> {
    with_session(device_id, instance_id, |volume| {
        // SAFETY: live interface. A null event context means "do not notify us
        // of our own change", which is what we want — the UI reads the value
        // back on its next poll.
        unsafe { volume.SetMasterVolume(level.clamp(0.0, 1.0), std::ptr::null()) }
    })
}

/// Mutes or unmutes one stream.
pub fn set_mute(device_id: &str, instance_id: &str, muted: bool) -> windows::core::Result<()> {
    with_session(device_id, instance_id, |volume| {
        // SAFETY: live interface; null event context as above.
        unsafe { volume.SetMute(muted, std::ptr::null()) }
    })
}

/// Finds a session by instance id and runs `action` against its volume
/// interface.
///
/// A session that has since disappeared is reported as `E_SESSION_NOT_FOUND`
/// rather than silently succeeding: the caller is a user action, and "the slider
/// did nothing and said nothing" is the failure worth avoiding.
///
/// `action` is `FnMut` rather than `FnOnce` even though it is meant to run at
/// most once. The traversal that finds the session is itself an `FnMut` — it
/// visits every session until it is told to stop — so it *could* reach a second
/// session carrying the same instance id (a duplicate the audio engine produced
/// while a stream was being torn down), and an `FnOnce` there would be a borrow
/// error rather than a behaviour. Applying the change twice is the right
/// outcome and is idempotent.
fn with_session(
    device_id: &str,
    instance_id: &str,
    mut action: impl FnMut(&ISimpleAudioVolume) -> windows::core::Result<()>,
) -> windows::core::Result<()> {
    let mut found: Option<windows::core::Result<()>> = None;

    for_each_session(device_id, |control| {
        // SAFETY: live interface.
        let Ok(raw) = (unsafe { control.GetSessionInstanceIdentifier() }) else {
            return true;
        };
        if take_com_string(raw).as_deref() != Some(instance_id) {
            return true;
        }

        found = Some(
            control
                .cast::<ISimpleAudioVolume>()
                .and_then(|volume| action(&volume)),
        );
        false
    })?;

    found.unwrap_or_else(|| Err(E_SESSION_NOT_FOUND.into()))
}

/// The COM event context used when this application changes a session volume.
///
/// Exposed so a future session-events implementation can recognise its own
/// notifications and skip the echo.
pub const EVENT_CONTEXT: GUID = GUID::from_u128(0x6d5f0a1e_9b34_4c77_8f21_0b7a2e5c9d10);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_narrowing_maps_all_three_values() {
        assert_eq!(
            SessionState::from(AudioSessionStateActive),
            SessionState::Active
        );
        assert_eq!(
            SessionState::from(AudioSessionStateExpired),
            SessionState::Expired
        );
        // `Inactive` is the zero value, so it is also the fallback for anything
        // the API grows later.
        assert_eq!(
            SessionState::from(AudioSessionState::default()),
            SessionState::Inactive
        );
    }

    #[test]
    fn expired_sessions_are_not_visible() {
        let base = Session {
            device_id: "d".into(),
            instance_id: "i".into(),
            display_id: "i".into(),
            process_id: 1,
            is_system_sounds: false,
            state: SessionState::Inactive,
            volume: 1.0,
            muted: false,
            peak: 0.0,
        };
        assert!(base.is_visible(), "an inactive app must still be adjustable");
        assert!(
            !Session {
                state: SessionState::Expired,
                ..base
            }
            .is_visible()
        );
    }
}
