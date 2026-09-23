// Shared artwork for the tray icon and the executable's icon resource.
//
// One rasteriser, two consumers:
//
//   * `ui::icon` wraps `render_rgba` in a `tray_icon::Icon` for the
//     notification area;
//   * `build.rs` `include!`s this very file to draw the multi-size `.ico` that
//     `rc.exe` bakes into the binary, so the shortcut, the taskbar and the
//     Alt-Tab thumbnail show the same glyph as the tray.
//
// Keeping a single implementation is the point: two copies would drift, and the
// mistake would only be visible on a user's desktop.
//
// The file therefore has no dependencies beyond `std` and no inner (`//!`)
// attributes — an `include!`d file must open with items, not with module
// attributes.

/// The edge length the artwork was drawn against. Every coordinate below is a
/// fraction of this, so one set of numbers serves every size.
pub const DESIGN_SIZE: u32 = 32;

/// Sizes embedded in the executable's icon group, smallest first.
///
/// 16 px is the Explorer list view and the Alt-Tab badge, 32 px the desktop and
/// the default, 48/64/128 the icon views, 256 the "extra large icons" view and
/// the taskbar thumbnail. The shell picks the nearest entry and rescales, so the
/// set only has to be dense enough to avoid a visibly blurry common case.
pub const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// Renders the icon at `size` px, returning straight-alpha RGBA in rows of
/// `size` pixels, top-down.
///
/// The palette leans on FxSound's teal so the icon reads as "the audio thing"
/// next to other tray entries, without copying their actual artwork.
pub fn render_rgba(size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let scale = size as f32 / DESIGN_SIZE as f32;

    // A dark rounded tile, inset slightly so its corners never fight the
    // bitmap edge (the per-primitive feathering would otherwise be clipped).
    let radius = 7.0 * scale;
    let tile = [
        scale,
        scale,
        (DESIGN_SIZE - 2) as f32 * scale,
        (DESIGN_SIZE - 2) as f32 * scale,
    ];
    fill_rounded_rect(&mut rgba, size, tile, radius, [0x12, 0x20, 0x3a, 0xff]);

    // Five bars, centred vertically, with the outer ones shortest so the
    // silhouette reads as a waveform rather than a bar chart.
    let bar_height = [8.0f32, 14.0, 20.0, 13.0, 6.0];
    let centre_y = size as f32 / 2.0;

    for (index, height) in bar_height.iter().enumerate() {
        let x = (6.0 + index as f32 * 4.0) * scale;
        let bar = *height * scale;
        let top = centre_y - bar / 2.0;
        // 2.5 px in the design grid leaves a 1 px gap; a touch over 1 px at the
        // 16 px size keeps the antialiasing from swallowing the thinnest bar.
        let width = (2.5 * scale).max(1.0);
        let blend = index as f32 / (bar_height.len() - 1) as f32;
        let colour = lerp_rgb([0x3d, 0xdc, 0x97], [0x38, 0xbd, 0xf8], blend);

        fill_rounded_rect(
            &mut rgba,
            size,
            [x, top, width, bar],
            (scale).max(0.5),
            [colour[0], colour[1], colour[2], 0xff],
        );
    }

    rgba
}

/// Packs `sizes` into a complete `.ico` file.
///
/// Entries are 32 bpp `BI_RGB` bitmaps rather than PNG. PNG entries would be
/// smaller for the 256 px image, but writing one means writing a deflate
/// compressor, and an `.ico` this shape is what the shell expects from a
/// desktop application anyway.
pub fn render_ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = sizes.iter().map(|&size| (size, bmp_entry(size))).collect();

    let mut out = Vec::new();
    // ICONDIR: reserved, type 1 (icon), entry count.
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    // ICONDIRENTRY is 16 bytes each, and the pixel data follows the directory.
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in &images {
        // 0 is how the format spells 256: the field is one byte wide.
        let edge = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(edge);
        out.push(edge);
        out.push(0); // palette size: 0 for true colour
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }

    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

/// One `.ico` entry: a `BITMAPINFOHEADER`, the pixels bottom-up as BGRA, then
/// the 1 bpp AND mask.
fn bmp_entry(size: u32) -> Vec<u8> {
    let rgba = render_rgba(size);

    // The mask is required by the format even though the alpha channel makes it
    // redundant; leaving it zeroed means "no transparent pixels", which is what
    // a 32 bpp entry wants.
    let mask_row = size.div_ceil(32) * 4;
    let mask_len = (mask_row * size) as usize;

    let mut out = Vec::with_capacity(40 + (size * size * 4) as usize + mask_len);

    // BITMAPINFOHEADER. Height is doubled because it has to account for the
    // mask that follows the pixels.
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&0u32.to_le_bytes()); // image size: implied by BI_RGB
    out.extend_from_slice(&0i32.to_le_bytes()); // horizontal resolution
    out.extend_from_slice(&0i32.to_le_bytes()); // vertical resolution
    out.extend_from_slice(&0u32.to_le_bytes()); // palette entries used
    out.extend_from_slice(&0u32.to_le_bytes()); // palette entries required

    for y in (0..size).rev() {
        for x in 0..size {
            let pixel = ((y * size + x) * 4) as usize;
            out.push(rgba[pixel + 2]); // blue
            out.push(rgba[pixel + 1]); // green
            out.push(rgba[pixel]); // red
            out.push(rgba[pixel + 3]); // alpha
        }
    }

    out.resize(out.len() + mask_len, 0);
    out
}

/// Fills a rounded rectangle into a straight-alpha RGBA buffer.
///
/// Coverage is computed from the distance to the rounded rectangle's boundary,
/// which gives one pixel of feathering without needing a rasteriser.
fn fill_rounded_rect(rgba: &mut [u8], size: u32, rect: [f32; 4], radius: f32, colour: [u8; 4]) {
    let [x, y, width, height] = rect;
    let min_x = x.floor().max(0.0) as u32;
    let max_x = ((x + width).ceil().max(0.0) as u32).min(size);
    let min_y = y.floor().max(0.0) as u32;
    let max_y = ((y + height).ceil().max(0.0) as u32).min(size);

    for py in min_y..max_y {
        for px in min_x..max_x {
            let coverage = rounded_rect_coverage(px as f32 + 0.5, py as f32 + 0.5, rect, radius);
            if coverage <= 0.0 {
                continue;
            }
            let offset = ((py * size + px) * 4) as usize;
            blend_pixel(&mut rgba[offset..offset + 4], colour, coverage);
        }
    }
}

/// Signed coverage of a point against a rounded rectangle, in `0.0..=1.0`.
fn rounded_rect_coverage(px: f32, py: f32, rect: [f32; 4], radius: f32) -> f32 {
    let [x, y, width, height] = rect;
    let half_w = width / 2.0;
    let half_h = height / 2.0;
    let cx = x + half_w;
    let cy = y + half_h;

    let dx = (px - cx).abs() - (half_w - radius);
    let dy = (py - cy).abs() - (half_h - radius);
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    let inside = dx.max(dy).min(0.0);
    let distance = outside + inside - radius;

    // 1 px of feather centred on the boundary.
    (0.5 - distance).clamp(0.0, 1.0)
}

/// Alpha-blends `colour` over the pixel at `dst` with `coverage`.
fn blend_pixel(dst: &mut [u8], colour: [u8; 4], coverage: f32) {
    let src_alpha = f32::from(colour[3]) / 255.0 * coverage;
    if src_alpha <= 0.0 {
        return;
    }
    let dst_alpha = f32::from(dst[3]) / 255.0;
    let out_alpha = src_alpha + dst_alpha * (1.0 - src_alpha);

    for channel in 0..3 {
        let src = f32::from(colour[channel]);
        let existing = f32::from(dst[channel]);
        let blended = (src * src_alpha + existing * dst_alpha * (1.0 - src_alpha)) / out_alpha;
        dst[channel] = blended.round().clamp(0.0, 255.0) as u8;
    }
    dst[3] = (out_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
}

/// Linear blend between two RGB triples.
fn lerp_rgb(from: [u8; 3], to: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    std::array::from_fn(|channel| {
        let a = f32::from(from[channel]);
        let b = f32::from(to[channel]);
        (a + (b - a) * t).round() as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_is_one_well_inside_and_zero_well_outside() {
        let rect = [4.0, 4.0, 16.0, 16.0];
        assert!((rounded_rect_coverage(12.0, 12.0, rect, 4.0) - 1.0).abs() < 1e-6);
        assert_eq!(rounded_rect_coverage(0.5, 0.5, rect, 4.0), 0.0);
        assert_eq!(rounded_rect_coverage(100.0, 12.0, rect, 4.0), 0.0);
    }

    #[test]
    fn colour_interpolation_hits_both_ends() {
        assert_eq!(lerp_rgb([0, 0, 0], [255, 128, 64], 0.0), [0, 0, 0]);
        assert_eq!(lerp_rgb([0, 0, 0], [255, 128, 64], 1.0), [255, 128, 64]);
    }

    #[test]
    fn every_rendered_size_has_the_declared_length() {
        for size in ICO_SIZES {
            let rgba = render_rgba(size);
            assert_eq!(rgba.len(), (size * size * 4) as usize, "size {size}");
        }
    }

    #[test]
    fn the_rendered_icon_actually_has_pixels() {
        // Regression guard for the arithmetic above: a scale factor of zero, or
        // a clip that collapses to nothing, would still produce a correctly
        // *sized* buffer. It just would not be visible.
        for size in ICO_SIZES {
            let rgba = render_rgba(size);
            let opaque = rgba.chunks_exact(4).filter(|pixel| pixel[3] > 128).count();
            let ratio = opaque as f64 / (size * size) as f64;
            assert!(ratio > 0.5, "size {size}: only {ratio:.2} of the tile is opaque");
        }
    }

    #[test]
    fn the_ico_directory_describes_every_entry() {
        let ico = render_ico(&ICO_SIZES);

        // ICONDIR
        assert_eq!(u16::from_le_bytes([ico[0], ico[1]]), 0, "reserved");
        assert_eq!(u16::from_le_bytes([ico[2], ico[3]]), 1, "type is icon");
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        assert_eq!(count, ICO_SIZES.len());

        // Every entry must point at its own payload inside the file, and the
        // payload must be a BITMAPINFOHEADER whose height is doubled.
        let mut expected_offset = 6 + 16 * count;
        for (index, size) in ICO_SIZES.iter().enumerate() {
            let at = 6 + 16 * index;
            let edge = if *size >= 256 { 0 } else { *size as u8 };
            assert_eq!(ico[at], edge, "entry {index} width");
            assert_eq!(ico[at + 1], edge, "entry {index} height");
            assert_eq!(u16::from_le_bytes([ico[at + 4], ico[at + 5]]), 1, "planes");
            assert_eq!(u16::from_le_bytes([ico[at + 6], ico[at + 7]]), 32, "bpp");

            let length = u32::from_le_bytes(ico[at + 8..at + 12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(ico[at + 12..at + 16].try_into().unwrap()) as usize;
            assert_eq!(offset, expected_offset, "entry {index} offset");

            let header = u32::from_le_bytes(ico[offset..offset + 4].try_into().unwrap());
            assert_eq!(header, 40, "entry {index} header size");
            let height = i32::from_le_bytes(ico[offset + 8..offset + 12].try_into().unwrap());
            assert_eq!(height, (*size as i32) * 2, "entry {index} doubled height");

            expected_offset += length;
        }

        assert_eq!(expected_offset, ico.len(), "the directory accounts for the whole file");
    }
}
