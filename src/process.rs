//! Process identity: what to call an audio session's owner, and what to draw
//! next to it.
//!
//! The mixer is handed a process id and nothing else. Everything a human
//! recognises — a name, a picture, a stable key to hang a routing rule on —
//! has to be derived from that number, and the number itself is useless across
//! restarts. This module does that derivation and caches it.
//!
//! ## Three kinds of application, three ways to name them
//!
//! | kind | how it is named |
//! |---|---|
//! | ordinary Win32 program | `QueryFullProcessImageNameW` → `rustc.exe` |
//! | packaged (UWP / Store) app | `GetApplicationUserModelId` → `Microsoft.WindowsCalculator_8wekyb3d8bbwe!App` |
//! | system sounds pseudo-session | there is no process at all |
//!
//! Packaged apps are the reason this is not a one-liner. Their executable lives
//! under `WindowsApps` as something like `Calculator.exe`, which is both an
//! unhelpful label and an unstable one — the path is version-stamped by the
//! store and changes on every update. The Application User Model ID is the
//! identity the shell itself uses, so that is what a routing rule is anchored
//! to.
//!
//! ## Why the identity is keyed, not the pid
//!
//! [`AppKey`] is what callers persist. A pid is recycled by Windows within
//! seconds and means nothing after a reboot; an executable path or an AUMID is
//! stable for as long as the application is installed. Routing rules written
//! against a pid would silently attach themselves to whichever process
//! inherited the number next, which is the kind of bug that looks like
//! "sometimes my music goes to the headset for no reason".
//!
//! ## Icons
//!
//! GDI hands back an `HICON`, which is a device-dependent handle that is
//! useless to a GPU-drawn UI. [`icon_for`] converts it to straight RGBA once and
//! caches the result per [`AppKey`], so the browser with nine processes pays for
//! one icon and not nine.
//!
//! The conversion has one wrinkle worth knowing about: icons authored before
//! 32-bit alpha became normal carry their transparency in a separate 1-bit AND
//! mask, and drawing one of those into a 32-bit surface leaves every alpha byte
//! at zero — an invisible icon. When that happens the mask is read back and
//! turned into an alpha channel, which is what the "no alpha anywhere" branch in
//! [`icon_pixels`] is for.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW,
    ReleaseDC, SelectObject, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
// `DestroyIcon` belongs to WindowsAndMessaging, not to the Shell module it
// looks like it should live in — the `SHGetFileInfoW` call that produces the
// handles is in Shell, the call that frees them is not.
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, DrawIconEx, GetIconInfo, HICON, ICONINFO, DI_NORMAL,
};

/// Longest Application User Model ID Windows will produce.
const MAX_AUMID: usize = 130;

/// How an application is identified across runs.
///
/// This is the key a routing rule stores. See the module docs for why it is not
/// a process id.
#[derive(Debug, Clone)]
pub enum AppKey {
    /// A packaged application, named by its Application User Model ID.
    Packaged(String),
    /// An ordinary program, named by its executable path (compared
    /// case-insensitively — Windows paths are).
    Executable(PathBuf),
    /// A session with no process behind it, i.e. the system sounds session.
    SystemSounds,
}

/// Equality *is* [`AppKey::same_as`], not a second answer to the same question.
///
/// Derived equality compared `Executable` paths byte for byte, which made
/// `C:\Apps\Music.exe` and `c:\apps\MUSIC.EXE` two different applications: two
/// rows in the mixer where the user sees one program, two icons, and a routing
/// rule that quietly stopped matching when a launcher handed the path over with
/// different capitalisation. `same_as` already treated them as one; `==` did
/// not, and the mix of the two is what makes a bug like that hard to see.
impl PartialEq for AppKey {
    fn eq(&self, other: &Self) -> bool {
        self.same_as(other)
    }
}

impl Eq for AppKey {}

impl Hash for AppKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // A tag per variant, so a packaged id that happens to spell the same
        // characters as a path cannot land in the same bucket as that path.
        match self {
            Self::Packaged(aumid) => {
                0u8.hash(state);
                aumid.hash(state);
            }
            Self::SystemSounds => 1u8.hash(state),
            Self::Executable(path) => {
                2u8.hash(state);
                // ASCII-only folding, to match the `eq_ignore_ascii_case` in
                // `same_as`. Folding beyond ASCII (`to_lowercase`) would hash
                // two values differently that `same_as` calls equal, which is
                // the one invariant a `Hash` impl must not break.
                path.to_string_lossy().to_ascii_lowercase().hash(state);
            }
        }
    }
}

impl AppKey {
    /// The identity of a process, or `None` if it has already exited.
    pub fn of_process(process_id: u32) -> Option<Self> {
        let identity = identify(process_id)?;
        Some(identity.key())
    }

    /// A stable, human-readable string for persistence.
    ///
    /// Prefixed so a hand-edited config cannot have an AUMID silently read back
    /// as an executable path.
    pub fn to_storage(&self) -> String {
        match self {
            AppKey::Packaged(aumid) => format!("aumid:{aumid}"),
            AppKey::Executable(path) => format!("exe:{}", path.to_string_lossy()),
            AppKey::SystemSounds => "system-sounds".to_owned(),
        }
    }

    /// Parses what [`AppKey::to_storage`] wrote.
    pub fn from_storage(text: &str) -> Option<Self> {
        if text == "system-sounds" {
            return Some(AppKey::SystemSounds);
        }
        if let Some(aumid) = text.strip_prefix("aumid:") {
            return (!aumid.is_empty()).then(|| AppKey::Packaged(aumid.to_owned()));
        }
        text.strip_prefix("exe:")
            .filter(|path| !path.is_empty())
            .map(|path| AppKey::Executable(PathBuf::from(path)))
    }

    /// Whether two keys name the same application.
    ///
    /// Executable paths are compared case-insensitively; everything else is
    /// compared exactly.
    ///
    /// This is the definition that `PartialEq` defers to, not the other way
    /// round — see the note on that impl. So it must not use `==` for its own
    /// comparison: doing that recursed until the stack ran out, which is a
    /// spectacularly unhelpful way for an equality test to fail.
    pub fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (AppKey::Executable(a), AppKey::Executable(b)) => {
                a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
            }
            (AppKey::Packaged(a), AppKey::Packaged(b)) => a == b,
            (AppKey::SystemSounds, AppKey::SystemSounds) => true,
            _ => false,
        }
    }
}

/// What is known about a running process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdentity {
    pub process_id: u32,
    /// `None` for a packaged app whose image path the caller may not read.
    pub executable: Option<PathBuf>,
    /// What to put on screen.
    pub display_name: String,
    /// The shell's identity for a packaged app.
    pub app_user_model_id: Option<String>,
}

impl AppIdentity {
    /// The stable key for this application.
    pub fn key(&self) -> AppKey {
        if let Some(aumid) = &self.app_user_model_id {
            return AppKey::Packaged(aumid.clone());
        }
        match &self.executable {
            Some(path) => AppKey::Executable(path.clone()),
            // A process we could open but whose path we could not read, and
            // which is not packaged. There is no stable identity to key on, so
            // fall back to the display name rather than dropping the row.
            None => AppKey::Packaged(self.display_name.clone()),
        }
    }
}

/// Decoded icon, ready for a GPU texture upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconPixels {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, top row first.
    pub rgba: Vec<u8>,
}

/// Process identity, keyed by process id.
fn identity_cache() -> &'static Mutex<HashMap<u32, Option<AppIdentity>>> {
    static CACHE: OnceLock<Mutex<HashMap<u32, Option<AppIdentity>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Decoded icons, keyed by application rather than by process.
fn icon_cache() -> &'static Mutex<HashMap<AppKey, Option<IconPixels>>> {
    static CACHE: OnceLock<Mutex<HashMap<AppKey, Option<IconPixels>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reads the executable path of a process.
///
/// `PROCESS_QUERY_LIMITED_INFORMATION` is the least privilege that answers
/// this, and it is granted across integrity levels where `PROCESS_QUERY_INFORMATION`
/// is not — without it, every elevated application would show up unnamed.
fn executable_of(process_id: u32) -> Option<PathBuf> {
    // SAFETY: no inherited handle; the returned handle is owned here and closed
    // on the way out.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;

    let mut buffer = vec![0u16; 1024];
    let mut length = buffer.len() as u32;
    // SAFETY: `handle` is live with query rights; the buffer and its length are
    // consistent, as the API requires.
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    // SAFETY: closing the handle opened above.
    unsafe {
        let _ = CloseHandle(handle);
    }

    if result.is_err() {
        return None;
    }

    buffer.truncate(length as usize);
    let text = String::from_utf16_lossy(&buffer);
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// Reads a packaged app's Application User Model ID.
///
/// Returns `None` for an ordinary process — that is the common answer, not an
/// error.
fn app_user_model_id(process_id: u32) -> Option<String> {
    // SAFETY: as `executable_of`.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;

    let mut buffer = vec![0u16; MAX_AUMID];
    let mut length = buffer.len() as u32;
    // SAFETY: `handle` is live with query rights; the buffer and its length are
    // consistent. The call reports through `WIN32_ERROR` rather than an
    // `HRESULT` — it is an appmodel API, not a COM one.
    let result = unsafe {
        GetApplicationUserModelId(handle, &mut length, Some(windows::core::PWSTR(buffer.as_mut_ptr())))
    };
    // SAFETY: closing the handle opened above.
    unsafe {
        let _ = CloseHandle(handle);
    }

    // `APPMODEL_ERROR_NO_APPLICATION` (15700) is the normal reply for a Win32
    // process, i.e. the overwhelmingly common path through this function.
    if result != ERROR_SUCCESS {
        return None;
    }

    buffer.truncate(length as usize);
    let text = String::from_utf16_lossy(&buffer)
        .trim_end_matches('\0')
        .to_owned();
    (!text.is_empty()).then_some(text)
}

/// The file name, used as a display name when nothing better exists.
fn file_stem_name(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown application".to_owned())
}

/// Identifies a process, caching the answer.
///
/// A `None` is cached too: an exited process stays exited, and the alternative
/// is re-opening a dead handle on every mixer refresh.
pub fn identify(process_id: u32) -> Option<AppIdentity> {
    if process_id == 0 {
        return None;
    }

    if let Ok(cache) = identity_cache().lock() {
        if let Some(cached) = cache.get(&process_id) {
            return cached.clone();
        }
    }

    let identity = identify_uncached(process_id);

    if let Ok(mut cache) = identity_cache().lock() {
        cache.insert(process_id, identity.clone());
    }

    identity
}

fn identify_uncached(process_id: u32) -> Option<AppIdentity> {
    let aumid = app_user_model_id(process_id);
    let executable = executable_of(process_id);

    // A packaged app whose image path is unreadable is still identifiable by
    // its AUMID — and the AUMID's last segment is a usable label.
    let display_name = match (&aumid, &executable) {
        (Some(aumid), _) => aumid
            .rsplit('!')
            .next()
            .filter(|segment| !segment.is_empty())
            .unwrap_or(aumid)
            .to_owned(),
        (None, Some(path)) => file_stem_name(path),
        (None, None) => return None,
    };

    Some(AppIdentity {
        process_id,
        executable,
        display_name,
        app_user_model_id: aumid,
    })
}

/// Drops cache entries for processes that no longer exist.
///
/// Called between mixer refreshes. Without it the identity cache grows for the
/// life of the session — one entry per process the user ever played audio from,
/// which on a machine that has been up for a week is a real number.
pub fn forget_exited(process_ids: &[u32]) {
    let mut cache = match identity_cache().lock() {
        Ok(cache) => cache,
        Err(_) => return,
    };

    // `OpenProcess` failing is the only portable test for "gone", and it is
    // also what `identify` already pays for.
    cache.retain(|pid, _| {
        if process_ids.contains(pid) {
            return true;
        }
        // SAFETY: probe only; the handle is closed immediately.
        match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, *pid) } {
            Ok(handle) => {
                // SAFETY: closing the probe handle.
                unsafe {
                    let _ = CloseHandle(handle);
                }
                true
            }
            Err(_) => false,
        }
    });
}

/// Decodes an application icon, caching by [`AppKey`].
pub fn icon_for(identity: &AppIdentity) -> Option<IconPixels> {
    let key = identity.key();

    if let Ok(cache) = icon_cache().lock() {
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
    }

    let pixels = identity
        .executable
        .as_deref()
        .and_then(|path| icon_for_path(path))
        .and_then(|hicon| icon_pixels(hicon));

    if let Ok(mut cache) = icon_cache().lock() {
        cache.insert(key, pixels.clone());
    }

    pixels
}

/// The shell's icon for a file, as an `HICON` this function owns.
///
/// SAFETY: the caller must `DestroyIcon` the result.
fn icon_for_path(path: &Path) -> Option<HICON> {
    let wide = crate::ffi::to_wide(path.as_os_str());
    let mut info = SHFILEINFOW::default();

    // SAFETY: the path is NUL-terminated; `info` is a correctly sized output
    // structure. `SHGFI_LARGEICON` asks for 32×32 — the small variant is 16×16
    // and looks like mush once the panel scales it for a HiDPI display.
    let result = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            Some(&mut info as *mut SHFILEINFOW),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };

    (result != 0 && !info.hIcon.is_invalid()).then_some(info.hIcon)
}

/// Renders an `HICON` into tightly packed RGBA8.
fn icon_pixels(hicon: HICON) -> Option<IconPixels> {
    struct IconGuard(HICON);
    impl Drop for IconGuard {
        fn drop(&mut self) {
            // SAFETY: the handle came from `SHGetFileInfoW` with `SHGFI_ICON`,
            // which transfers ownership to us.
            unsafe {
                let _ = DestroyIcon(self.0);
            }
        }
    }
    let _guard = IconGuard(hicon);

    // SAFETY: `hicon` is live. `info` receives two bitmaps that we own and must
    // delete.
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(hicon, &mut info) }.ok()?;

    struct BitmapGuard(HBITMAP);
    impl Drop for BitmapGuard {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: an HBITMAP we own.
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(self.0.0));
                }
            }
        }
    }
    let _colour_guard = BitmapGuard(info.hbmColor);
    let mask_guard = BitmapGuard(info.hbmMask);

    // The colour bitmap carries the size; a monochrome icon has none, and the
    // mask is then the only source of dimensions.
    let (width, height) = {
        let mut bitmap = BITMAP::default();
        // SAFETY: `bitmap` is a correctly sized output structure.
        let source = if !info.hbmColor.is_invalid() {
            info.hbmColor
        } else {
            info.hbmMask
        };
        let copied = unsafe {
            GetObjectW(
                HGDIOBJ(source.0),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bitmap as *mut _ as *mut std::ffi::c_void),
            )
        };
        if copied == 0 || bitmap.bmWidth <= 0 || bitmap.bmHeight <= 0 {
            return None;
        }
        // A monochrome icon's mask holds the image in its upper half, so it
        // reports twice the real height.
        let height = if info.hbmColor.is_invalid() {
            bitmap.bmHeight / 2
        } else {
            bitmap.bmHeight
        };
        (bitmap.bmWidth as u32, height as u32)
    };

    let mut rgba = draw_into_rgba(hicon, width, height)?;

    // Icons without an alpha channel — the 1-bit-mask era — come out fully
    // transparent through `DrawIconEx`, i.e. invisible. Rebuild alpha from the
    // mask instead.
    if rgba.chunks_exact(4).all(|pixel| pixel[3] == 0) {
        apply_mask_alpha(&mut rgba, width, height, mask_guard.0);
    }

    Some(IconPixels {
        width,
        height,
        rgba,
    })
}

/// Draws an icon into a top-down 32-bit DIB section and reads the pixels back.
fn draw_into_rgba(hicon: HICON, width: u32, height: u32) -> Option<Vec<u8>> {
    // SAFETY: no source DC; a memory DC compatible with the screen is what
    // `CreateCompatibleDC(None)` produces.
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return None;
    }
    let dc: HDC = unsafe { CreateCompatibleDC(Some(screen)) };
    unsafe {
        ReleaseDC(None, screen);
    }
    if dc.is_invalid() {
        return None;
    }

    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        // Negative height = top-down, so the first row in memory is the top row
        // and no flip is needed afterwards.
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let info = BITMAPINFO {
        bmiHeader: header,
        ..Default::default()
    };

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `dc` is live; `info` describes a valid 32-bit top-down surface;
    // `bits` receives the address of the pixel buffer inside the section.
    let section = unsafe { CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) };
    let section = match section {
        Ok(section) if !bits.is_null() => section,
        _ => {
            // SAFETY: `dc` came from CreateCompatibleDC.
            unsafe {
                let _ = DeleteDC(dc);
            }
            return None;
        }
    };

    // SAFETY: both objects are live and of the right types for SelectObject.
    let previous = unsafe { SelectObject(dc, HGDIOBJ(section.0)) };
    // SAFETY: `dc` is live and selected onto the DIB section; DrawIconEx
    // composites the icon with its mask, so the transparent pixels are left
    // untouched.
    unsafe {
        let _ = DrawIconEx(dc, 0, 0, hicon, width as i32, height as i32, 0, None, DI_NORMAL);
    }
    // SAFETY: restoring and then destroying what we created.
    unsafe {
        SelectObject(dc, previous);
    }

    let count = (width as usize) * (height as usize);
    // SAFETY: `bits` points at `count` pixels of 32-bit BGRA, allocated by
    // CreateDIBSection above and alive until the section is deleted.
    let source = unsafe { std::slice::from_raw_parts(bits as *const u8, count * 4) };

    let mut rgba = Vec::with_capacity(count * 4);
    for pixel in source.chunks_exact(4) {
        // GDI gives BGRA in memory order; egui wants RGBA.
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }

    // SAFETY: deleting the objects created in this function, after restoring
    // the DC's original bitmap.
    unsafe {
        let _ = DeleteObject(HGDIOBJ(section.0));
        let _ = DeleteDC(dc);
    }

    Some(rgba)
}

/// Fills in alpha from a monochrome AND mask.
///
/// In the mask a set bit means "transparent". Rows are bottom-up and padded to
/// a 4-byte boundary, which is why this is not a flat bit walk.
fn apply_mask_alpha(rgba: &mut [u8], width: u32, height: u32, mask: HBITMAP) {
    let row_bytes = (width.div_ceil(32) * 4) as usize;
    let mut buffer = vec![0u8; row_bytes * height as usize];

    // SAFETY: `mask` may be an invalid handle for icons that have no mask, in
    // which case `GetDIBits` fails and the buffer stays zero — every pixel is
    // then treated as opaque, which is the right default.
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return;
        }
        let dc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        if dc.is_invalid() {
            return;
        }

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: height as i32,
                biPlanes: 1,
                biBitCount: 1,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut info = info;

        let scanned = if mask.is_invalid() {
            0
        } else {
            GetDIBits(
                dc,
                mask,
                0,
                height,
                Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        let _ = DeleteDC(dc);

        if scanned == 0 {
            return;
        }
    }

    for y in 0..height as usize {
        // GetDIBits fills bottom-up; `rgba` is top-down.
        let source_row = height as usize - 1 - y;
        for x in 0..width as usize {
            let byte = buffer[source_row * row_bytes + x / 8];
            // MSB first within each byte, per the DDB convention.
            let transparent = byte & (0x80 >> (x % 8)) != 0;
            rgba[(y * width as usize + x) * 4 + 3] = if transparent { 0 } else { 255 };
        }
    }
}

/// Releases the raw handle of an `HICON` without destroying it.
///
/// Only used by the diagnostics binary, to prove the handle is the shell's and
/// not ours.
pub fn peek_icon_handle(path: &Path) -> Option<isize> {
    let icon = icon_for_path(path)?;
    let raw = icon.0 as isize;
    // SAFETY: the handle was created for us by SHGetFileInfoW and this function
    // deliberately hands ownership back to nobody, so it must be destroyed.
    unsafe {
        let _ = DestroyIcon(icon);
    }
    Some(raw)
}

/// The display name for a process id, without the rest of [`identify`].
pub fn display_name(process_id: u32) -> String {
    identify(process_id)
        .map(|identity| identity.display_name)
        .unwrap_or_else(|| format!("PID {process_id}"))
}

/// Frees a string that a Windows API allocated with `CoTaskMemAlloc`.
///
/// Not used by this module's own paths — it exists so the per-app endpoint
/// module, which does receive such strings, does not need its own copy of the
/// cast.
pub fn free_com_string(pointer: *mut u16) {
    if !pointer.is_null() {
        // SAFETY: the caller guarantees `pointer` came from a COM call that
        // allocates with the task allocator.
        unsafe { CoTaskMemFree(Some(pointer as *const std::ffi::c_void)) };
    }
}

/// A handle that is valid only while the process is alive.
///
/// Used by the diagnostics binary to prove [`identify`] reports a dead process
/// as absent rather than inventing a name for it.
pub fn is_alive(process_id: u32) -> bool {
    // SAFETY: probe only.
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) } {
        Ok(handle) => {
            // SAFETY: closing the probe handle.
            unsafe {
                let _ = CloseHandle(handle);
            }
            true
        }
        Err(_) => false,
    }
}

/// Keeps `HANDLE` in this module's public surface for the diagnostics binary.
pub type ProcessHandle = HANDLE;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn keys_round_trip_through_storage() {
        let cases = [
            AppKey::Packaged("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".to_owned()),
            AppKey::Executable(PathBuf::from(r"C:\Program Files\App\app.exe")),
            AppKey::SystemSounds,
        ];
        for key in cases {
            let stored = key.to_storage();
            assert_eq!(
                AppKey::from_storage(&stored).as_ref(),
                Some(&key),
                "round trip failed for {stored}"
            );
        }
    }

    #[test]
    fn an_empty_payload_is_not_a_key() {
        assert_eq!(AppKey::from_storage("aumid:"), None);
        assert_eq!(AppKey::from_storage("exe:"), None);
        assert_eq!(AppKey::from_storage("nonsense"), None);
        assert_eq!(AppKey::from_storage(""), None);
    }

    #[test]
    fn executable_paths_compare_case_insensitively() {
        let a = AppKey::Executable(PathBuf::from(r"C:\Apps\Music.exe"));
        let b = AppKey::Executable(PathBuf::from(r"c:\apps\MUSIC.EXE"));
        assert!(a.same_as(&b));
        assert_eq!(a, b, "a case difference must still hash the same way");
    }

    /// `Eq` and `Hash` have to agree, and `assert_eq!` cannot see it.
    ///
    /// This is the test that would have caught the shape of the original bug:
    /// `AppKey`'s `PartialEq` was derived while `same_as` folded case, so the
    /// two disagreed and nothing that only compared values would notice. The
    /// mixer's icon cache is a `HashMap<AppKey, _>`, where the disagreement
    /// shows up as the same program getting two rows and two icons.
    #[test]
    fn equal_keys_share_a_hash_bucket() {
        let mut set: HashSet<AppKey> = HashSet::new();
        assert!(set.insert(AppKey::Executable(PathBuf::from(r"C:\Apps\Music.exe"))));
        assert!(
            !set.insert(AppKey::Executable(PathBuf::from(r"c:\apps\MUSIC.EXE"))),
            "one program spelled two ways must not become two entries"
        );
        assert_eq!(set.len(), 1);

        // The variants stay distinct even when their payloads spell the same
        // characters, which is what the per-variant tag in `Hash` is for.
        let mut mixed: HashSet<AppKey> = HashSet::new();
        mixed.insert(AppKey::Packaged("x!App".to_owned()));
        mixed.insert(AppKey::Executable(PathBuf::from("x!App")));
        mixed.insert(AppKey::SystemSounds);
        assert_eq!(mixed.len(), 3);
    }

    #[test]
    fn a_packaged_key_is_not_a_path_key() {
        let a = AppKey::Packaged("x!App".to_owned());
        let b = AppKey::Executable(PathBuf::from("x!App"));
        assert!(!a.same_as(&b));
    }

    #[test]
    fn system_sounds_never_matches_anything_else() {
        assert!(AppKey::SystemSounds.same_as(&AppKey::SystemSounds));
        assert!(!AppKey::SystemSounds.same_as(&AppKey::Executable(PathBuf::from("x"))));
    }

    #[test]
    fn the_display_name_falls_back_to_the_file_stem() {
        assert_eq!(
            file_stem_name(Path::new(r"C:\Apps\Spotify.exe")),
            "Spotify"
        );
        // A path with no file name at all still produces something printable.
        assert_eq!(file_stem_name(Path::new("")), "Unknown application");
    }

    #[test]
    fn a_packaged_app_is_named_by_the_last_aumid_segment() {
        let identity = AppIdentity {
            process_id: 1,
            executable: None,
            display_name: "Calculator".to_owned(),
            app_user_model_id: Some("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".to_owned()),
        };
        assert_eq!(
            identity.key(),
            AppKey::Packaged("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".to_owned())
        );
    }
}
