//! Rounds the tuning panel's outer corners.
//!
//! Windows 11 rounds a top-level window for you. Windows 10 does not — there is
//! no `DWMWA_WINDOW_CORNER_PREFERENCE` before build 22000 — and FxTrumpet has to
//! run on both, so on Windows 10 the corners have to be cut by hand.
//!
//! The only lever Windows 10 leaves is the window region. `SetWindowRgn` clips a
//! window to an arbitrary shape, corners included, and the compositor shows
//! whatever is behind the cut. Two consequences worth knowing before touching
//! this:
//!
//! * A region is a one-bit mask, so the corners come out stepped rather than
//!   blended. At an eight-point radius — the same one [`super::theme::radius::CARD`]
//!   gives a card — the steps are about a physical pixel, which does not read as
//!   jagged at normal viewing distance but is not the compositor's antialiasing
//!   either.
//! * The region clips the *whole* window. The panel draws its own caption now
//!   (see [`super::window_chrome`]) and the window has no decorations, so the
//!   window rectangle and the client area happen to be the same rectangle — but
//!   the region is still sized from `GetWindowRect`, which is the rectangle
//!   Windows itself measures a region against. The two agree today and would
//!   stop agreeing the day anything puts a system frame back.
//! * A maximised window is left square. Its corners are the corners of the
//!   screen, so cutting them would take four bites out of the desktop and show
//!   whatever is behind the window through them. [`RoundedWindow::apply`] clears
//!   the region for as long as the window is maximised, and cuts a fresh one
//!   when it is restored.
//!
//! On Windows 11 the DWM has already rounded the frame, so this repeats a shape
//! the compositor chose itself. It is applied on every version anyway rather
//! than behind a build-number check: one code path, and no branch that only ever
//! runs on the machine nobody is testing on. It also matters more than it used
//! to: the DWM rounds a *decorated* window by default, and stops once the
//! decorations are gone.
//!
//! The tray menu is deliberately *not* rounded. It is a Win32 popup menu drawn
//! by the system, and the one hook that could have caught its first frame
//! (`EVENT_SYSTEM_MENUPOPUPSTART`) is delivered after the menu is already up.
//!
//! ## The one-pixel strip along the top and left
//!
//! An undecorated window here used to show a two-to-three pixel band of grey
//! along its top and left edges. It was not the panel's own drawing: three
//! separate things stacked up, and only the first was ours.
//!
//! **The client rectangle is one pixel lower than the window.** egui-winit asks
//! for an undecorated drop shadow whenever the decorations are off
//! (`egui-winit-0.35.0/src/lib.rs`: `with_undecorated_shadow(!decorations…)`),
//! and winit answers that marker by moving the client rectangle down to leave
//! room for the shadow:
//!
//! ```text
//! } else if window_flags.contains(WindowFlags::MARKER_UNDECORATED_SHADOW) {
//!     params.rgrc[0].top += 1;
//!     params.rgrc[0].bottom += 1;
//! }
//! ```
//!
//! So the topmost pixel belongs to the non-client area, egui never paints there,
//! and what Windows leaves behind is a neutral grey row (`(227, 227, 227)` on the
//! light theme — a colour in no palette). [`RoundedWindow::clear_window_edge`]
//! calls `set_undecorated_shadow(false)`, which is the supported way to say the
//! same thing, and the shift goes away.
//!
//! **`WS_EX_WINDOWEDGE` is left set by winit.** `WindowState::window_flags`
//! starts `style_ex` at `WS_EX_WINDOWEDGE | WS_EX_ACCEPTFILES`, and the line that
//! takes it off again sits *inside* the `WindowFlags::CHILD` branch, so a
//! top-level undecorated window never reaches it. That flag asks the system to
//! paint a raised edge around the client area — the left-hand column of the same
//! strip. Clearing it through `GWL_EXSTYLE` does **not** work: the write is
//! accepted (`SetWindowLongPtrW` returns the old value with no error) and the
//! re-read shows the flag back, because Windows re-derives it as part of the
//! frame. With the shadow off, the one-pixel client inset is gone and the edge
//! the flag draws is covered by the panel's own fill, so it is left alone rather
//! than fought.
//!
//! **The panel's own hairline was drawn half a pixel in.** `shrink(0.5)` with
//! `StrokeKind::Inside` puts a one-point stroke across `[0.5, 1.5]`, so pixel 0
//! held bare panel fill and the line was feathered over two pixels. See
//! [`super::window_chrome::outline`], which now snaps the stroke onto the
//! outermost pixel instead.

use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use std::ffi::c_void;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
use windows_core::Free as _;

/// Cuts the panel window's corners off with a window region, unless the window
/// is maximised.
///
/// Stateful because `SetWindowRgn` is a one-shot call: the clip stays in force
/// until it is replaced, so the region only needs rebuilding when the window
/// changes size or when it is maximised or restored. Rebuilding it every frame
/// would allocate and hand over a GDI object thirty times a second to say the
/// same thing.
#[derive(Default)]
pub struct RoundedWindow {
    /// The panel's window, resolved on the first frame and then kept.
    hwnd: Option<HWND>,
    /// The physical size the region currently in force was cut for.
    ///
    /// `None` means no region is in force: either none has been cut yet, or the
    /// one that was has been cleared because the window is maximised.
    applied: Option<(i32, i32)>,
    /// Whether the undecorated drop shadow has been turned off yet.
    ///
    /// Stateful because `set_undecorated_shadow` re-runs `WM_NCCALCSIZE`, and
    /// there is no reason to shift the client rectangle back and forth every
    /// frame to ask for the same answer.
    shadow_off: bool,
}

impl RoundedWindow {
    /// A shaper that will find its window on the first frame it is shown one.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rounds the corners of `frame`'s window to `radius` points.
    ///
    /// `pixels_per_point` is egui's current scale; the radius is a point value
    /// and a region is measured in physical pixels, so taking the point value
    /// straight would clip the wrong amount on a scaled display.
    ///
    /// `maximized` removes the region instead of setting one, and is the only
    /// reason this is not a no-op after its first successful call.
    pub fn apply(
        &mut self,
        frame: &eframe::Frame,
        pixels_per_point: f32,
        radius: f32,
        maximized: bool,
    ) {
        let Some(window) = self.window(frame) else {
            return;
        };

        // Before the region, because turning the shadow off changes the client
        // rectangle, and the region is cut from the window rectangle the new
        // geometry settles on.
        if !self.shadow_off {
            self.shadow_off = clear_undecorated_shadow(frame);
        }

        if maximized {
            if self.applied.is_some() {
                // SAFETY: a live HWND, and a null region, which is the
                // documented way to say "no region": the window reverts to its
                // own rectangle. Nothing is owned or freed by this call.
                unsafe {
                    let _ = SetWindowRgn(window, None, true);
                }
                // Forgotten so that restoring the window cuts a fresh region
                // rather than trusting one that is no longer there.
                self.applied = None;
            }
            return;
        }

        let mut rect = RECT::default();
        // SAFETY: `window` is a live HWND copied out of the frame, and `rect` is
        // a local the call is documented to fill in.
        if unsafe { GetWindowRect(window, &mut rect) }.is_err() {
            return;
        }

        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 || self.applied == Some((width, height)) {
            return;
        }

        let diameter = physical(radius * 2.0, pixels_per_point);

        // SAFETY: both calls take values this function owns — the same live HWND
        // as above, and a region created on the line above. `SetWindowRgn` takes
        // ownership of the region when it succeeds and leaves it to the caller
        // when it fails, so the failure arm frees it and nothing else does.
        unsafe {
            // The right and bottom edges are exclusive, so a region that is to
            // cover `width` pixels is asked for `width + 1`.
            let mut region = CreateRoundRectRgn(0, 0, width + 1, height + 1, diameter, diameter);
            if region.is_invalid() {
                return;
            }

            if SetWindowRgn(window, Some(region), true) == 0 {
                region.free();
                return;
            }
        }

        self.applied = Some((width, height));
    }

    /// The panel's window, resolved once and remembered.
    ///
    /// `Frame` is where eframe hands out the platform handle, and it is borrowed
    /// for a single frame, so the raw `HWND` is copied out the first time it is
    /// available. It stays valid for as long as the window exists, which is as
    /// long as this object does: both belong to the panel thread, and the panel
    /// is torn down with this app.
    fn window(&mut self, frame: &eframe::Frame) -> Option<HWND> {
        if self.hwnd.is_none() {
            let handle = frame.window_handle().ok()?;
            let RawWindowHandle::Win32(raw) = handle.as_raw() else {
                // Not Windows. Nothing here applies, and there is no other
                // backend to fall back to — this build only targets Windows, so
                // this arm exists for the type checker rather than for a
                // platform that could be reached.
                return None;
            };
            self.hwnd = Some(HWND(raw.hwnd.get() as *mut c_void));
        }

        self.hwnd
    }
}

/// Turns the undecorated drop shadow off, which is what removes the window's
/// one-pixel top strip. Returns whether it had a window to do it to.
///
/// See the module docs for the whole chain. The short version: egui-winit sets
/// `WindowAttributesExtWindows::with_undecorated_shadow(true)` whenever the
/// decorations are off (`egui-winit-0.35.0/src/lib.rs`, `create_window`), and
/// winit answers that marker by shifting the client rectangle **down one pixel**
/// to leave room for the shadow it is adding:
///
/// ```text
/// } else if window_flags.contains(WindowFlags::MARKER_UNDECORATED_SHADOW) {
///     params.rgrc[0].top += 1;
///     params.rgrc[0].bottom += 1;
/// }
/// ```
///
/// The pixel that shift gives up is not part of the client area, so egui never
/// paints on it — and what Windows leaves there, on this build, is that neutral
/// grey row. Together with the panel's own hairline it is the strip that gets
/// reported as "two to three pixels along the left and top".
///
/// The shadow being given up is worth nothing here anyway: an undecorated
/// window clipped by `SetWindowRgn` is not rounded by the DWM on Windows 10, so
/// the shadow that ships with the one-pixel inset arrives clipped to our own
/// rectangle and is barely visible. [`super::window_chrome::outline`] is the edge
/// the user actually sees.
///
/// Done once. Unlike the window style, this is not something winit re-derives
/// behind our back: it is set when the window is created and again only if
/// `ViewportCommand::Decorations` arrives, which this app never sends.
fn clear_undecorated_shadow(frame: &eframe::Frame) -> bool {
    use winit::platform::windows::WindowExtWindows as _;

    let Some(window) = frame.winit_window() else {
        // Headless, or a backend with no winit window. False rather than true, so
        // a later frame tries again.
        return false;
    };

    window.set_undecorated_shadow(false);
    true
}

/// A length in points, as whole physical pixels, never zero.
///
/// Zero is not a usable argument to either call here: a zero-size region is
/// invalid, and a zero diameter is a square corner written the slow way.
fn physical(points: f32, pixels_per_point: f32) -> i32 {
    (points * pixels_per_point).round().max(1.0) as i32
}
