//! Tray icon and tuning panel.
//!
//! The application has no main window. This module owns the one thread that
//! does have a Win32 message queue, because `tray-icon` and `muda` deliver
//! their events as window messages and nothing works unless that queue is
//! pumped.

pub mod icon;
pub mod icon_raster;
pub mod mixer;
pub mod panel;
pub mod theme;
pub mod tray;
pub mod window_chrome;
pub mod window_shape;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage, MSG,
    MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT,
};

/// How long the loop will sleep with nothing to do before running the tick
/// anyway. Only needs to be short enough for status updates; tray clicks wake
/// the loop immediately through the message queue.
const IDLE_TICK_MS: u32 = 250;

/// Runs the tray message loop until `tick` returns `true`.
///
/// ## Why not a `while` + `sleep` loop
///
/// The naive version — drain messages, do work, `Sleep(10)` — wakes the CPU 100
/// times a second whether or not anything happened, which is exactly the
/// "low power" property FxTrumpet exists to have. `MsgWaitForMultipleObjectsEx`
/// blocks the thread properly and returns the instant a message arrives, so an
/// idle FxTrumpet costs nothing.
///
/// `tick` runs after every batch of messages, and also on the idle timeout so
/// meters and status stay fresh.
pub fn run_message_loop(mut tick: impl FnMut() -> bool) {
    let mut message = MSG::default();

    loop {
        // SAFETY: `message` is a valid MSG; PeekMessage with a null window
        // filter and a 0..0 range retrieves any message for this thread.
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: `message` was filled in by PeekMessageW above.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        if tick() {
            return;
        }

        // SAFETY: waiting on the thread's own message queue with no handles.
        // Returns as soon as a message is posted, or after the timeout.
        unsafe {
            MsgWaitForMultipleObjectsEx(None, IDLE_TICK_MS, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }
}

/// A null window handle, for APIs that take an optional owner.
pub const NO_WINDOW: Option<HWND> = None;
