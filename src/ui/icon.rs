//! The tray icon.
//!
//! The bitmap is drawn in code rather than shipped as a file, so the repository
//! stays free of binary assets that nobody can review in a diff. The artwork
//! itself lives in [`super::icon_raster`] because `build.rs` rasterises the same
//! file into the executable's `.ico` — the tray glyph and the one on the
//! shortcut are the same drawing by construction.

use tray_icon::Icon;

use super::icon_raster::{render_rgba, DESIGN_SIZE};

/// Draws the tray icon: a rounded tile with five equaliser bars.
///
/// 32 px is the large-icon size on a 100 % DPI taskbar; the shell downsamples
/// for the notification area.
pub fn tray_icon() -> Icon {
    let rgba = render_rgba(DESIGN_SIZE);
    Icon::from_rgba(rgba, DESIGN_SIZE, DESIGN_SIZE).expect("icon buffer matches the declared dimensions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_drawable() {
        // Panics inside the builder if the buffer size is wrong.
        let _ = tray_icon();
    }
}
