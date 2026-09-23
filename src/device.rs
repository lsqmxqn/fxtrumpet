//! Audio endpoint discovery, default-device control, and change notification.
//!
//! Everything here is thin glue over Core Audio. Three pieces are worth
//! calling out because they are the ones that bite:
//!
//! 1. [`ComGuard`] — Core Audio is COM. Every thread that touches it must
//!    initialise an apartment, and the audio thread in particular has to use
//!    **MTA**: an STA would try to pump a message loop that the audio thread
//!    does not have.
//!
//! 2. [`set_default_device`] — Windows has no documented API for changing the
//!    default endpoint. `IPolicyConfig` is undocumented but has been stable
//!    since Vista; it is declared here by hand, see the comment on
//!    `IID_IPOLICY_CONFIG`.
//!
//! 3. [`DeviceWatcher`] — hot-plug, UAC prompts and exclusive-mode grabs all
//!    arrive as `IMMNotificationClient` callbacks. They arrive **on a COM
//!    thread we do not own**, so the callback only forwards an event; the
//!    engine is rebuilt later, on its own thread.

use std::sync::mpsc::{channel, Receiver, Sender};

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eConsole, eMultimedia, eRender, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
    DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED, DEVICE_STATE_NOTPRESENT, DEVICE_STATE_UNPLUGGED,
    EDataFlow, ERole,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

/// Second half of the `PKEY_Device_*` family; only the pid varies.
const FMTID_DEVICE: GUID = GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0);

/// `PKEY_Device_FriendlyName` — what the user sees in the Sound control panel.
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: FMTID_DEVICE,
    pid: 14,
};

/// `PKEY_Device_DeviceDesc` — the driver-provided description, used as a
/// fallback when the friendly name is absent.
const PKEY_DEVICE_DEVICE_DESC: PROPERTYKEY = PROPERTYKEY {
    fmtid: FMTID_DEVICE,
    pid: 2,
};

/// `PKEY_DeviceInterface_FriendlyName` — the adapter name, e.g. "Speakers".
const PKEY_DEVICE_INTERFACE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xb3f8fa53_0004_438e_9003_51a46e139bfc),
    pid: 6,
};

/// Substrings that identify FxSound's virtual card. The INF declares the
/// hardware id as `Root\FXVAD` and the wave endpoint as
/// "FxSound Audio Enhancer" (fxvad.inf:135,141).
const VIRTUAL_DEVICE_MARKERS: [&str; 3] = ["fxsound", "fxvad", "audio enhancer"];

/// `CLSID_CPolicyConfigClient` — undocumented, but the object every
/// default-device changer ends up using.
const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// `IID_IPolicyConfig`.
///
/// Windows exposes no supported way to set the default audio endpoint.
/// `IPolicyConfig` has shipped in every release since Vista and its vtable
/// layout has never changed, which is why every application that needs this
/// (including FxSound itself) declares it by hand. The alternative is telling
/// the user to open the Sound control panel and click, which is what FxTrumpet is
/// trying to avoid.
const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

/// A render endpoint as FxTrumpet sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Endpoint id, stable across reboots. This is what gets persisted.
    pub id: String,
    /// Presentable name.
    pub name: String,
    /// `PKEY_DeviceInterface_FriendlyName`, when available. Disambiguates two
    /// identical-looking endpoints.
    pub adapter: Option<String>,
    /// Current state, as a short string.
    pub state: DeviceState,
    /// Whether the endpoint is currently the default for any role.
    pub is_default: bool,
    /// Whether this looks like FxSound's virtual card.
    pub is_virtual: bool,
}

impl DeviceInfo {
    /// Name plus adapter, for menus where two "Speakers" would be confusing.
    pub fn display(&self) -> String {
        match &self.adapter {
            Some(adapter) if !adapter.is_empty() && !self.name.contains(adapter.as_str()) => {
                format!("{} ({adapter})", self.name)
            }
            _ => self.name.clone(),
        }
    }
}

/// Endpoint state, narrowed from the Win32 bitmask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Active,
    Disabled,
    NotPresent,
    Unplugged,
    Unknown,
}

impl std::fmt::Display for DeviceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DeviceState::Active => "active",
            DeviceState::Disabled => "disabled",
            DeviceState::NotPresent => "not-present",
            DeviceState::Unplugged => "unplugged",
            DeviceState::Unknown => "unknown",
        })
    }
}

impl From<DEVICE_STATE> for DeviceState {
    fn from(state: DEVICE_STATE) -> Self {
        let raw = state.0;
        if raw & DEVICE_STATE_ACTIVE.0 != 0 {
            DeviceState::Active
        } else if raw & DEVICE_STATE_DISABLED.0 != 0 {
            DeviceState::Disabled
        } else if raw & DEVICE_STATE_NOTPRESENT.0 != 0 {
            DeviceState::NotPresent
        } else if raw & DEVICE_STATE_UNPLUGGED.0 != 0 {
            DeviceState::Unplugged
        } else {
            DeviceState::Unknown
        }
    }
}

/// Initialises COM for the lifetime of the value.
///
/// `CoUninitialize` must be called exactly as many times as `CoInitializeEx`
/// succeeded, hence the RAII rather than a bare call. If COM was already
/// initialised with the *other* apartment model, `CoInitializeEx` returns
/// `RPC_E_CHANGED_MODE`; that is not an error for us — the thread already has
/// COM, which is all the Core Audio calls need — so the guard records that it
/// must not uninitialise.
pub struct ComGuard {
    owns_initialisation: bool,
}

impl ComGuard {
    /// Initialises the thread's apartment as multi-threaded.
    pub fn mta() -> windows::core::Result<Self> {
        Self::init(COINIT_MULTITHREADED)
    }

    /// Initialises the thread's apartment as single-threaded.
    ///
    /// Used on the UI thread where a message loop exists. Never on the audio
    /// thread — an STA there would deadlock waiting for a pump that never runs.
    pub fn sta() -> windows::core::Result<Self> {
        Self::init(COINIT_APARTMENTTHREADED)
    }

    fn init(model: windows::Win32::System::Com::COINIT) -> windows::core::Result<Self> {
        // SAFETY: `pv_reserved` must be null per the API contract.
        let hr = unsafe { CoInitializeEx(None, model) };
        if hr.is_ok() {
            Ok(Self {
                owns_initialisation: true,
            })
        } else if hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            // Already initialised in the other model. Usable, but we must not
            // uninitialise: that would drop a reference we never took.
            Ok(Self {
                owns_initialisation: false,
            })
        } else {
            Err(hr.into())
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owns_initialisation {
            // SAFETY: paired with the successful CoInitializeEx above, on the
            // same thread, exactly once.
            unsafe { CoUninitialize() }
        }
    }
}

/// Creates the MMDevice enumerator.
///
/// Public because the mixer needs to open a *specific* endpoint by id rather
/// than the default one. Requires COM on the calling thread (see [`ComGuard`]).
pub fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    // SAFETY: COM must already be initialised on this thread (see ComGuard).
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// Reads a string property, trimming the trailing separator Windows sometimes
/// appends. Returns `None` when the property is absent or non-string.
///
/// `IPropertyStore::GetValue` hands back an owned `PROPVARIANT`, so it has to
/// be released with `PropVariantClear` — without that, every enumeration leaks
/// one string per endpoint, which for a tray tool that re-enumerates on every
/// hot-plug is a slow but real leak.
fn string_property(device: &IMMDevice, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: `device` is a live interface pointer; `key` is a valid
    // PROPERTYKEY. The returned PROPVARIANT is owned by us and cleared below.
    unsafe {
        let store: IPropertyStore = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value: PROPVARIANT = store.GetValue(key).ok()?;

        let text = if value.Anonymous.Anonymous.vt == VT_LPWSTR {
            let wide = value.Anonymous.Anonymous.Anonymous.pwszVal;
            if wide.is_null() {
                None
            } else {
                wide.to_string().ok()
            }
        } else {
            None
        };

        // Releases the string the property store allocated.
        let _ = PropVariantClear(&mut value);

        let trimmed = text?.trim_matches('\0').trim().to_owned();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

/// Reads the endpoint id.
///
/// `GetId` returns CoTaskMem-allocated memory and, in windows-rs, a `PWSTR`
/// that the wrapper does **not** free — hence the explicit `CoTaskMemFree`.
unsafe fn endpoint_id(device: &IMMDevice) -> Option<String> {
    let id = device.GetId().ok()?;
    let text = id.to_string().ok();
    windows::Win32::System::Com::CoTaskMemFree(Some(id.0 as *const _));
    text
}

/// Whether a set of names looks like FxSound's virtual card.
fn looks_virtual(names: &[Option<&str>]) -> bool {
    names
        .iter()
        .flatten()
        .any(|name| {
            let lowered = name.to_ascii_lowercase();
            VIRTUAL_DEVICE_MARKERS
                .iter()
                .any(|marker| lowered.contains(marker))
        })
}

/// Builds a [`DeviceInfo`] from a raw endpoint.
fn describe(device: &IMMDevice, default_ids: &[String]) -> Option<DeviceInfo> {
    // SAFETY: `device` is a live interface pointer; the property reads are
    // read-only and copy their results out.
    unsafe {
        let id = endpoint_id(device)?;
        let name = string_property(device, &PKEY_DEVICE_FRIENDLY_NAME)
            .or_else(|| string_property(device, &PKEY_DEVICE_DEVICE_DESC))
            .unwrap_or_else(|| "Unknown endpoint".to_owned());
        let adapter = string_property(device, &PKEY_DEVICE_INTERFACE_FRIENDLY_NAME);

        let state = device
            .GetState()
            .map(DeviceState::from)
            .unwrap_or(DeviceState::Unknown);

        let is_virtual = looks_virtual(&[Some(name.as_str()), adapter.as_deref()]);

        Some(DeviceInfo {
            is_default: default_ids.iter().any(|candidate| candidate == &id),
            id,
            name,
            adapter,
            state,
            is_virtual,
        })
    }
}

/// The ids currently used as default for the console, multimedia and
/// communications roles. Any of them counts as "the default" for our purposes.
fn default_ids(enumerator: &IMMDeviceEnumerator) -> Vec<String> {
    [eConsole, eMultimedia, ERole(2) /* eCommunications */]
        .into_iter()
        .filter_map(|role| {
            // SAFETY: enumerator is live; GetDefaultAudioEndpoint with
            // eRender/role is a valid, read-only query.
            let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, role) }.ok()?;
            // SAFETY: live interface pointer.
            unsafe { endpoint_id(&device) }
        })
        .collect()
}

/// Collects render endpoints, optionally including non-active ones.
fn collect(enumerator: &IMMDeviceEnumerator, include_inactive: bool) -> Vec<DeviceInfo> {
    let mask = if include_inactive {
        DEVICE_STATE_ACTIVE.0
            | DEVICE_STATE_DISABLED.0
            | DEVICE_STATE_NOTPRESENT.0
            | DEVICE_STATE_UNPLUGGED.0
    } else {
        DEVICE_STATE_ACTIVE.0
    };

    // SAFETY: live enumerator; EnumAudioEndpoints is read-only.
    let collection: IMMDeviceCollection =
        match unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE(mask)) } {
            Ok(collection) => collection,
            Err(_) => return Vec::new(),
        };

    // SAFETY: live collection pointer.
    let count = unsafe { collection.GetCount() }.unwrap_or(0);
    let defaults = default_ids(enumerator);

    (0..count)
        .filter_map(|index| {
            // SAFETY: index < count, verified above.
            let device = unsafe { collection.Item(index) }.ok()?;
            // SAFETY: live interface pointer.
            describe(&device, &defaults)
        })
        .collect()
}

/// All active render endpoints, default first, then virtual cards, then the rest.
pub fn render_devices() -> windows::core::Result<Vec<DeviceInfo>> {
    let enumerator = enumerator()?;
    let mut devices = collect(&enumerator, false);

    devices.sort_by_key(|device| {
        (
            !device.is_default,  // default first
            device.is_virtual,   // physical before virtual
            device.name.to_lowercase(),
        )
    });
    Ok(devices)
}

/// All render endpoints including disabled and unplugged ones.
pub fn all_render_devices() -> windows::core::Result<Vec<DeviceInfo>> {
    let enumerator = enumerator()?;
    Ok(collect(&enumerator, true))
}

/// The current default render endpoint.
pub fn default_render_device() -> windows::core::Result<DeviceInfo> {
    let enumerator = enumerator()?;
    // SAFETY: live enumerator; eRender/eConsole is the role the shell uses.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }?;
    let defaults = default_ids(&enumerator);
    // SAFETY: live interface pointer.
    describe(&device, &defaults).ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
}

/// Looks up one endpoint by id.
pub fn device_by_id(id: &str) -> Option<DeviceInfo> {
    all_render_devices()
        .ok()?
        .into_iter()
        .find(|device| device.id == id)
}

/// Finds FxSound's virtual card among the active render endpoints.
pub fn find_virtual_device() -> Option<DeviceInfo> {
    render_devices()
        .ok()?
        .into_iter()
        .find(|device| device.is_virtual)
}

/// Whether FxSound's virtual card is currently installed and active.
pub fn virtual_device_present() -> bool {
    find_virtual_device().is_some()
}

// ── IPolicyConfig ─────────────────────────────────────────────────────────

/// Manual declaration of the undocumented `IPolicyConfig` vtable.
///
/// Only `SetDefaultEndpoint` is ever called, but every preceding slot must be
/// present because the vtable is addressed by index. The signatures of the
/// unused members are recorded for documentation and are never invoked.
#[repr(C)]
struct IPolicyConfigVtbl {
    query_interface:
        unsafe extern "system" fn(*mut std::ffi::c_void, *const GUID, *mut *mut std::ffi::c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    get_mix_format: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, *mut *mut std::ffi::c_void) -> i32,
    get_device_format:
        unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32, *mut *mut std::ffi::c_void) -> i32,
    reset_device_format: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR) -> i32,
    set_device_format: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        PCWSTR,
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
    ) -> i32,
    get_processing_period:
        unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32, *mut i64, *mut i64) -> i32,
    set_processing_period: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, *mut i64) -> i32,
    get_share_mode: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, *mut std::ffi::c_void) -> i32,
    set_share_mode: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, *mut std::ffi::c_void) -> i32,
    get_property_value: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        PCWSTR,
        *const PROPERTYKEY,
        *mut std::ffi::c_void,
    ) -> i32,
    set_property_value: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        PCWSTR,
        *const PROPERTYKEY,
        *mut std::ffi::c_void,
    ) -> i32,
    set_default_endpoint: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32) -> i32,
    set_endpoint_visibility: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32) -> i32,
}

/// An owned `IPolicyConfig` reference.
///
/// Both words are needed, and conflating them is the trap this had to be
/// debugged out of. COM's ABI is two levels of indirection: the interface
/// pointer identifies the *object*, and the object's first word is the pointer
/// to its vtable. Reading the table straight off the interface pointer instead
/// lands in the object's own fields - a real vtable here is a dense run of
/// pointers into the same DLL, whereas the object's second word was already
/// unrelated booking data - and the "function pointer" found at the slot meant
/// for `SetDefaultEndpoint` is garbage. Calling it is an access violation, not
/// a failed call, which is why this is worth two named fields and a comment.
#[repr(C)]
struct IPolicyConfig {
    /// The interface pointer. Passed as `this` on every call.
    object: *mut std::ffi::c_void,
    /// The vtable read out of `object`.
    vtbl: *const IPolicyConfigVtbl,
}

impl IPolicyConfig {
    /// Creates the policy config object and queries the undocumented interface.
    fn new() -> windows::core::Result<Self> {
        // SAFETY: COM is initialised on this thread. The class is a registered
        // in-proc server present since Vista.
        let unknown: windows::core::IUnknown =
            unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }?;

        let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: manual QueryInterface; `raw` receives an owned reference.
        // `Interface::query` reports through an HRESULT rather than a `Result`,
        // hence the explicit `.ok()`.
        unsafe { unknown.query(&IID_IPOLICY_CONFIG, &mut raw) }.ok()?;

        if raw.is_null() {
            return Err(windows::Win32::Foundation::E_NOINTERFACE.into());
        }

        // SAFETY: `raw` is a non-null interface pointer, so it addresses a COM
        // object whose first word - and therefore the first word at `raw` - is
        // that object's vtable pointer.
        let vtbl = unsafe { *(raw as *const *const IPolicyConfigVtbl) };

        Ok(Self { object: raw, vtbl })
    }

    /// Switches the default endpoint for one role.
    fn set_default_endpoint(&self, endpoint_id: &str, role: ERole) -> windows::core::Result<()> {
        let wide = crate::ffi::to_wide(std::ffi::OsStr::new(endpoint_id));
        // SAFETY: `self.vtbl` came from a successful QueryInterface, so every
        // slot is a valid function pointer. Calling slot 13 (SetDefaultEndpoint)
        // with a NUL-terminated id and a valid role matches the interface.
        let hr = unsafe { ((*self.vtbl).set_default_endpoint)(self.object, PCWSTR(wide.as_ptr()), role.0) };
        if hr < 0 {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(hr)));
        }
        Ok(())
    }
}

impl Drop for IPolicyConfig {
    fn drop(&mut self) {
        // SAFETY: `self.object` is the reference QueryInterface handed us, and
        // `self.vtbl` is its vtable. Release exactly the once.
        unsafe {
            ((*self.vtbl).release)(self.object);
        }
    }
}

// IPolicyConfig is used and dropped on one thread at a time; the COM object
// itself is free-threaded.
unsafe impl Send for IPolicyConfig {}

/// Makes `endpoint_id` the default render device for every role.
///
/// All three roles are set, not just console: leaving communications on the
/// old device means VoIP applications keep bypassing the enhancer, which reads
/// as "it works for music but not for Discord".
pub fn set_default_device(endpoint_id: &str) -> windows::core::Result<()> {
    let policy = IPolicyConfig::new()?;
    for role in [eConsole, eMultimedia, ERole(2)] {
        policy.set_default_endpoint(endpoint_id, role)?;
    }
    Ok(())
}

/// Restores a previously captured default endpoint, logging but not failing if
/// the device has since disappeared.
pub fn try_restore_default(endpoint_id: &str) -> bool {
    match set_default_device(endpoint_id) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("could not restore default endpoint {endpoint_id}: {err}");
            false
        }
    }
}

// ── change notification ───────────────────────────────────────────────────

/// Something happened to the audio devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A different endpoint became the default for a render role.
    DefaultChanged { endpoint_id: Option<String> },
    /// An endpoint was added, removed, enabled, disabled or unplugged.
    StateChanged { endpoint_id: Option<String> },
    /// A property of an endpoint changed (usually its name).
    PropertyChanged { endpoint_id: Option<String> },
}

impl DeviceEvent {
    /// Whether this event should make the engine re-evaluate its endpoints.
    ///
    /// Property changes are excluded: renaming a device is not a reason to
    /// rebuild the audio graph, and rebuilding on every name change would
    /// glitch playback for a purely cosmetic edit.
    pub fn requires_rebuild(&self) -> bool {
        matches!(
            self,
            DeviceEvent::DefaultChanged { .. } | DeviceEvent::StateChanged { .. }
        )
    }
}

#[windows::core::implement(IMMNotificationClient)]
struct Notifier {
    sender: Sender<DeviceEvent>,
}

/// Reads a possibly-null `PCWSTR` that the caller guarantees is either null or
/// NUL-terminated.
fn narrow_id(ptr: &PCWSTR) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        // SAFETY: the API contract for these callbacks is a NUL-terminated
        // string (or null), valid for the duration of the call.
        Some(unsafe { ptr.to_string() }.unwrap_or_default()).filter(|id| !id.is_empty())
    }
}

impl IMMNotificationClient_Impl for Notifier_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        // A send failure means the watcher was dropped: the callback is still
        // registered for a moment, which is fine, just ignore it.
        let _ = self.sender.send(DeviceEvent::StateChanged {
            endpoint_id: narrow_id(id),
        });
        Ok(())
    }

    fn OnDeviceAdded(&self, id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.sender.send(DeviceEvent::StateChanged {
            endpoint_id: narrow_id(id),
        });
        Ok(())
    }

    fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.sender.send(DeviceEvent::StateChanged {
            endpoint_id: narrow_id(id),
        });
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        id: &PCWSTR,
    ) -> windows::core::Result<()> {
        // Only render endpoints matter; capture-side changes cannot break our
        // graph.
        if flow == eRender {
            let _ = self.sender.send(DeviceEvent::DefaultChanged {
                endpoint_id: narrow_id(id),
            });
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        let _ = self.sender.send(DeviceEvent::PropertyChanged {
            endpoint_id: narrow_id(id),
        });
        Ok(())
    }
}

/// Subscribes to endpoint notifications for as long as the value is alive.
///
/// The registration is dropped with the value, so the callback can never fire
/// into a half-torn-down engine.
pub struct DeviceWatcher {
    client: IMMNotificationClient,
    receiver: Receiver<DeviceEvent>,
}

impl DeviceWatcher {
    /// Registers for notifications. Requires COM on the calling thread.
    pub fn new() -> windows::core::Result<Self> {
        let (sender, receiver) = channel();
        let notifier: IMMNotificationClient = Notifier { sender }.into();
        let enumerator = enumerator()?;
        // SAFETY: live enumerator and a live callback object.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&notifier) }?;
        Ok(Self {
            client: notifier,
            receiver,
        })
    }

    /// Blocks until an event arrives, or the watcher is dropped.
    ///
    /// Callers should prefer [`DeviceWatcher::try_recv`] on a thread that has
    /// other work to do, or [`DeviceWatcher::recv_timeout`] when polling is
    /// acceptable.
    pub fn recv(&self) -> Option<DeviceEvent> {
        self.receiver.recv().ok()
    }

    /// Non-blocking poll.
    pub fn try_recv(&self) -> Option<DeviceEvent> {
        self.receiver.try_recv().ok()
    }

    /// Waits up to `timeout` for an event.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<DeviceEvent> {
        self.receiver.recv_timeout(timeout).ok()
    }

    /// Unregisters explicitly. Called automatically on drop; use this when the
    /// caller needs the callback to stop *before* dropping.
    pub fn unregister(&self) {
        if let Ok(enumerator) = enumerator() {
            // SAFETY: unregistering a client we registered; idempotent enough
            // that a stale registration would only log.
            let _ = unsafe { enumerator.UnregisterEndpointNotificationCallback(&self.client) };
        }
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.unregister();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_marker_matching() {
        assert!(looks_virtual(&[Some("FxSound Audio Enhancer")]));
        assert!(looks_virtual(&[None, Some("Speakers (FxSound Audio Enhancer)")]));
        assert!(!looks_virtual(&[Some("Speakers (Realtek(R) Audio)")]));
        assert!(!looks_virtual(&[None, None]));
    }

    #[test]
    fn property_changes_do_not_force_a_rebuild() {
        assert!(DeviceEvent::DefaultChanged { endpoint_id: None }.requires_rebuild());
        assert!(DeviceEvent::StateChanged { endpoint_id: None }.requires_rebuild());
        assert!(!DeviceEvent::PropertyChanged { endpoint_id: None }.requires_rebuild());
    }
}
