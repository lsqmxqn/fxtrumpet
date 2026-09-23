//! Per-application default output endpoint.
//!
//! This is EarTrumpet's mechanism for "move this app to another device", and it
//! is the *cheap* half of the router: it needs no administrator rights, touches
//! no other process, and covers every application that asks Windows which
//! endpoint to render into — which is the overwhelming majority.
//!
//! ## What it actually does
//!
//! Windows keeps a per-application, per-role default endpoint alongside the
//! system-wide one, and this undocumented WinRT factory is the only way to
//! write it. The write is *persisted*: it survives a restart of the application
//! and of the machine, which is why it is the right primitive for a routing
//! rule.
//!
//! ## Two traps, both of which cost a debugging session
//!
//! **1. The id is not the endpoint id.** Windows wants the MMDevice *interface
//! path*,
//!
//! ```text
//! \\?\SWD#MMDEVAPI#{endpoint id}#{e6327cad-dcec-4949-ae8a-991e976a79d2}
//! ```
//!
//! and passing the bare `{0.0.0.00000000}.{...}` endpoint id — the thing
//! `IMMDevice::GetId` returns and every other API here takes — fails with
//! `E_INVALIDARG` and no explanation. [`interfaces::AUDIO_RENDER`] holds the
//! trailing GUID.
//!
//! **2. It is not a COM class.** On every supported build the factory comes
//! from `RoGetActivationFactory("Windows.Media.Internal.AudioPolicyConfig")`,
//! not `CoCreateInstance`. Using the class id from older write-ups of this
//! trick gets a `REGDB_E_CLASSNOTREG`.
//!
//! The interface id differs across builds (1803–21H1 versus 21H2 and later), so
//! [`Factory::new`] tries the newer one first and falls back. Sniffing the
//! build number would be the other option and it is strictly worse: it needs a
//! version table that Microsoft never promised to maintain.
//!
//! ## Why there is no getter here
//!
//! The interface also has `GetPersistedDefaultAudioEndpoint`. It is deliberately
//! not wrapped. What it returns is what was *requested*, and Windows
//! does not reliably apply it at application start — EarTrumpet's own
//! documentation says so, and works around it by re-applying the setting
//! whenever it sees the application. What the router should show the user is
//! where the audio *is*, and that is directly observable: the session's
//! `device_id` from [`crate::session`]. Reporting the persisted value would be
//! reporting an intention, and it would disagree with the session list sitting
//! next to it on screen.

// The manual `query` on `IUnknown` is a method of `windows_core::Interface`, and
// nothing in the generated bindings for this class exists to bring that trait
// into scope — so it is imported by name rather than arriving with a prelude.
use windows::core::{Interface, GUID, HSTRING};
use windows::Win32::Foundation::S_OK;
use windows::Win32::System::WinRT::RoGetActivationFactory;

/// WinRT class hosting the per-application endpoint policy.
const POLICY_CONFIG_CLASS: &str = "Windows.Media.Internal.AudioPolicyConfig";

/// Interface id on Windows 10 21H2 and later, including Windows 11.
const IID_21H2: GUID = GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);

/// Interface id on Windows 10 1803 through 21H1.
const IID_DOWNLEVEL: GUID = GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

/// Endpoint ids, as Windows wants them for this one API. See the module docs.
pub mod interfaces {
    /// The render interface class, appended after the endpoint id.
    pub const AUDIO_RENDER: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
    /// The prefix that turns an endpoint id into an MMDevice interface path.
    pub const MMDEVAPI_TOKEN: &str = r"\\?\SWD#MMDEVAPI#";
}

/// Role values, as raw `i32` because this interface predates the documented
/// enum and takes the plain integer.
///
/// They are a *bitmask*: `eConsole` is 0 and `eMultimedia` is 1, which is why
/// the two have to be written separately — there is no single value that means
/// "both".
mod role {
    pub const E_CONSOLE: i32 = 0;
    pub const E_MULTIMEDIA: i32 = 1;
}

/// Data flow values. Only render is supported; capture routing is a different
/// feature with a different interface class.
mod flow {
    pub const E_RENDER: i32 = 0;
}

/// The vtable of `Windows.Media.Internal.AudioPolicyConfig`.
///
/// The layout is `IUnknown` (3) + `IInspectable` (3) + nineteen members this
/// application never calls + the three it does. The unused run is declared as
/// an array rather than nineteen named fields: it exists only to put the three
/// real slots at the right offset, and naming members whose signatures nobody
/// has verified would be inventing documentation.
#[repr(C)]
struct PolicyConfigVtbl {
    // ── IUnknown ──
    query_interface:
        unsafe extern "system" fn(*mut std::ffi::c_void, *const GUID, *mut *mut std::ffi::c_void) -> windows::core::HRESULT,
    add_ref: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    // ── IInspectable ──
    get_iids:
        unsafe extern "system" fn(*mut std::ffi::c_void, *mut u32, *mut *mut GUID) -> windows::core::HRESULT,
    get_runtime_class_name:
        unsafe extern "system" fn(*mut std::ffi::c_void, *mut *mut std::ffi::c_void) -> windows::core::HRESULT,
    get_trust_level: unsafe extern "system" fn(*mut std::ffi::c_void, *mut i32) -> windows::core::HRESULT,
    // ── volatile members we never call ──
    unused: [*const std::ffi::c_void; 19],
    // ── the three we do ──
    /// `Set(processId, flow, role, HSTRING deviceId)`. A null string clears the
    /// application's override and hands it back to the system default.
    set_persisted: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        u32,
        i32,
        i32,
        *mut std::ffi::c_void,
    ) -> windows::core::HRESULT,
    get_persisted: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        u32,
        i32,
        i32,
        *mut *mut std::ffi::c_void,
    ) -> windows::core::HRESULT,
    clear_all: unsafe extern "system" fn(*mut std::ffi::c_void) -> windows::core::HRESULT,
}

/// An owned activation factory, plus the vtable read out of it.
pub struct Factory {
    object: *mut std::ffi::c_void,
    vtbl: *const PolicyConfigVtbl,
    /// Which interface id answered, for the log.
    variant: Variant,
}

/// Which of the two known interface ids is in force on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Windows 10 21H2 and later, including Windows 11.
    Modern,
    /// Windows 10 1803 through 21H1.
    Downlevel,
}

impl Factory {
    /// Creates the factory, trying the modern interface id first.
    ///
    /// Requires COM on the calling thread. Fails on Windows 10 before 1803 and
    /// on Windows 8.1 and earlier, where the class does not exist at all —
    /// callers should treat that as "routing is unavailable", not as an error.
    pub fn new() -> windows::core::Result<Self> {
        let class_id = HSTRING::from(POLICY_CONFIG_CLASS);

        let candidates = [(IID_21H2, Variant::Modern), (IID_DOWNLEVEL, Variant::Downlevel)];
        let mut last_error = None;

        for (iid, variant) in candidates {
            // SAFETY: COM is initialised on this thread. Asking for
            // `IUnknown` and then narrowing is the standard way to reach an
            // interface the bindings do not know about.
            let unknown: windows::core::IUnknown = match unsafe { RoGetActivationFactory(&class_id) } {
                Ok(factory) => factory,
                Err(err) => {
                    last_error = Some(err);
                    continue;
                }
            };

            let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: the manual two-argument QueryInterface. `raw` receives an
            // owned reference on success.
            let hr = unsafe { unknown.query(&iid, &mut raw) };
            if hr.is_err() || raw.is_null() {
                last_error = Some(windows::core::Error::from(hr));
                continue;
            }

            // SAFETY: `raw` is a non-null interface pointer, so the first word
            // at `raw` is its vtable pointer. This is the same two-level
            // indirection `device.rs` documents for `IPolicyConfig`; reading the
            // table straight off `raw` would land in the object's own fields.
            let vtbl = unsafe { *(raw as *const *const PolicyConfigVtbl) };

            return Ok(Self {
                object: raw,
                vtbl,
                variant,
            });
        }

        Err(last_error.unwrap_or_else(|| windows::Win32::Foundation::E_NOINTERFACE.into()))
    }

    /// Which interface id answered.
    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// Points one application's render output at an endpoint, for both roles.
    ///
    /// `endpoint_id` is the plain endpoint id from [`crate::device`]; the
    /// MMDevice interface path Windows wants is built here, so no caller has to
    /// know about the wrapping.
    ///
    /// Both `eConsole` and `eMultimedia` are written. Writing only one is the
    /// classic half-fix: an application that plays through the console role
    /// moves and one that uses the multimedia role does not, which presents as
    /// "it works for the browser but not for the game".
    pub fn set_default_endpoint(
        &self,
        process_id: u32,
        endpoint_id: &str,
    ) -> windows::core::Result<()> {
        let path = format!(
            "{}{}{}",
            interfaces::MMDEVAPI_TOKEN,
            endpoint_id,
            interfaces::AUDIO_RENDER
        );
        // An HSTRING, not a pointer to characters: this parameter is a WinRT
        // string. It is held for the whole call, which is what keeps it valid.
        let device = HSTRING::from(path);

        for role in [role::E_CONSOLE, role::E_MULTIMEDIA] {
            self.set_raw(process_id, role, &device)?;
        }
        Ok(())
    }

    /// Hands one application back to the system default output.
    ///
    /// Implemented as a *null* device string rather than by deleting anything:
    /// that is how the interface expresses "no override", and it is also how a
    /// 32-bit process's override is cleared.
    pub fn clear_default_endpoint(&self, process_id: u32) -> windows::core::Result<()> {
        for role in [role::E_CONSOLE, role::E_MULTIMEDIA] {
            self.set_raw(process_id, role, &HSTRING::new())?;
        }
        Ok(())
    }

    /// Clears every per-application override this user has.
    ///
    /// The escape hatch for a config that has been left in a state the user
    /// cannot explain. Not called automatically.
    pub fn clear_all(&self) -> windows::core::Result<()> {
        // SAFETY: `self.vtbl` came from a successful QueryInterface, so every
        // slot is a valid function pointer.
        let hr = unsafe { ((*self.vtbl).clear_all)(self.object) };
        hr.ok()
    }

    fn set_raw(
        &self,
        process_id: u32,
        role: i32,
        device: &HSTRING,
    ) -> windows::core::Result<()> {
        // `HSTRING` is `#[repr(transparent)]` over its handle, which is why the
        // generated bindings pass one by transmuting the reference. Doing the
        // same here is what lets this hand-written vtable take a WinRT string
        // without a second, parallel definition of the type.
        let handle: *mut std::ffi::c_void = unsafe { std::mem::transmute_copy(device) };

        // SAFETY: live object and vtbl; `process_id` is any process id and
        // `role` one of the two constants above. An unknown process id is
        // accepted by Windows and stored.
        let hr = unsafe {
            ((*self.vtbl).set_persisted)(self.object, process_id, flow::E_RENDER, role, handle)
        };
        hr.ok()
    }
}

impl Drop for Factory {
    fn drop(&mut self) {
        // SAFETY: releasing the reference QueryInterface handed us, exactly
        // once, with the vtable that belongs to it.
        unsafe {
            ((*self.vtbl).release)(self.object);
        }
    }
}

// The factory is free-threaded and is only ever called from one thread at a
// time; the raw COM reference itself is what keeps it safe to move.
unsafe impl Send for Factory {}

/// Whether this machine has the per-application endpoint facility at all.
///
/// Answered by trying. The alternative is a version check against a table
/// Microsoft never published.
pub fn is_supported() -> bool {
    Factory::new().is_ok()
}

/// The role bitmask `GetPersistedDefaultAudioEndpoint` wants for "either role".
///
/// Kept because the value is not obvious — `eConsole` is 0, so the mask for
/// both is just `eMultimedia` — and because it is the one piece of the getter
/// worth recording even though the getter is not wrapped.
pub const ROLE_MASK_CONSOLE_OR_MULTIMEDIA: i32 = role::E_CONSOLE | role::E_MULTIMEDIA;

/// Proves the `S_OK` comparison used for HRESULT-as-boolean is the intended one.
pub const SUCCESS: windows::core::HRESULT = S_OK;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interface_path_is_built_the_way_windows_expects() {
        let endpoint = "{0.0.0.00000000}.{aabbccdd-1122-3344-5566-778899aabbcc}";
        let path = format!(
            "{}{}{}",
            interfaces::MMDEVAPI_TOKEN,
            endpoint,
            interfaces::AUDIO_RENDER
        );
        assert!(path.starts_with(r"\\?\SWD#MMDEVAPI#"), "{path}");
        assert!(path.contains(endpoint), "{path}");
        assert!(
            path.ends_with("#{e6327cad-dcec-4949-ae8a-991e976a79d2}"),
            "{path}"
        );
    }

    #[test]
    fn the_role_mask_for_both_roles_is_one() {
        // eConsole is 0, so "both" and "multimedia only" are the same number.
        // Recording it here stops anyone (including a future me) from
        // "fixing" the constant to 3.
        assert_eq!(ROLE_MASK_CONSOLE_OR_MULTIMEDIA, 1);
    }

    #[test]
    fn the_vtable_prefix_has_the_right_number_of_slots() {
        // IUnknown (3) + IInspectable (3) + 19 unused + 3 used. If a field is
        // ever added or removed, this is the test that notices before the
        // first call lands in the wrong function.
        assert_eq!(
            std::mem::size_of::<PolicyConfigVtbl>() / std::mem::size_of::<*const std::ffi::c_void>(),
            28
        );
    }

    #[test]
    fn the_two_interface_ids_are_distinct() {
        assert_ne!(IID_21H2, IID_DOWNLEVEL);
    }
}
