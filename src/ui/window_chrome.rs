//! The panel's own window chrome: a title bar, and the border that resizes the
//! window.
//!
//! ## Why the window has no system decorations
//!
//! The panel used to wear the system caption: on Windows 10 a grey title bar
//! with a hard one-point edge and three square system buttons, sitting directly
//! on top of an interface built from eight-point rounded cards on a soft grey
//! field. Two design languages in one window, and the seam is the first thing
//! the eye lands on.
//!
//! The caption cannot be talked out of it. `DWMWA_CAPTION_COLOR` is Windows 11
//! build 22000 and up, so on Windows 10 there is no colour to set; the buttons
//! are drawn by the non-client part of the frame, which an application never
//! gets to paint; and the shape of the buttons is decided by the theme's visual
//! style, which can only be swapped for another one of the system's. So the
//! window is created with `with_decorations(false)` and the caption is drawn
//! here, out of the same tokens as everything below it — [`space::M`] of
//! padding, [`Palette::surface`] behind, [`Palette::control_hover`] under the
//! pointer.
//!
//! ## What that gives up, and what replaces it
//!
//! The whole of the non-client area, which the OS was handling for free. Each
//! of these is a place where this module could be quietly worse than the thing
//! it replaced:
//!
//! * **Moving the window** — [`ViewportCommand::StartDrag`], on the empty part
//!   of the bar. Windows' own trick of restoring a maximised window when its
//!   caption is dragged comes along with it, because `StartDrag` hands the drag
//!   to the same OS move loop the system caption uses.
//! * **Minimise, maximise, close** — three buttons, drawn here. The maximise one
//!   has to flip to a *restore* glyph when the window is already maximised,
//!   which is the state the system button kept for us.
//! * **Double-click to maximise** — the gesture every Windows user tries on a
//!   title bar without thinking about it.
//! * **Resizing** — the one that is easy to forget, because the window still has
//!   a size grip in the visual style it inherited. An undecorated winit window
//!   has *no* resize border: its `WM_NCCALCSIZE` handler returns the window
//!   rectangle as the entire client area, so there is no non-client frame left
//!   for Windows to hit-test, and `DefWindowProc` answers `HTCLIENT` for every
//!   pixel of the window. [`resize_border`] is the replacement.
//!
//! ## Why the resize zones are `Area`s on their own layer
//!
//! They overlap the panel: the caption's buttons touch the top edge, and the
//! close button's corner is *in* the top-right one. If the zones were tested
//! from input rather than allocated as widgets, a press there would start a
//! resize **and** be handed to the button underneath — and since the resize
//! hands the mouse to an OS modal loop, the button would never see its release
//! and would be left stuck down. Making them widgets on the foreground layer
//! settles it the way Windows does: the topmost hit wins and the button never
//! learns about the press.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Order, Pos2, Rect, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};
use eframe::egui::{viewport::ResizeDirection, ViewportCommand};

use super::theme::{self, space, Palette};

/// Height of the caption, in points.
///
/// 32 is the Windows 11 caption height. Taken from the system rather than
/// invented, so a FxTrumpet window sits at the same height as the windows around
/// it; the buttons are sized from it rather than the other way round.
pub const CAPTION_HEIGHT: f32 = 32.0;

/// How wide each of the three window buttons is.
///
/// 46 against a 32-point bar, which is what Windows uses for the same bar. The
/// ratio is what makes them feel like the buttons every other window has, and a
/// wider target is the one place a caption is allowed to be generous.
const BUTTON_WIDTH: f32 = 46.0;

/// The name at the left of the caption.
///
/// The same string the window is created with, so the taskbar, Alt-Tab and the
/// caption cannot disagree.
const TITLE: &str = "FxTrumpet";

/// Size of that name, in points.
///
/// Smaller than the card headings below it (13) and than a body label. A
/// caption is identity, not content: it should be legible at a glance and never
/// in competition with the numbers the panel exists to show.
const TITLE_SIZE: f32 = 12.5;

/// The box a button's glyph is drawn inside, in points.
const GLYPH: f32 = 10.0;

/// Glyph stroke width, in points.
///
/// One point, like the theme's control borders. The system draws its glyphs at
/// one physical pixel, which at 100 % scaling is the same thing.
const GLYPH_STROKE: f32 = 1.0;

/// How thick the invisible resize border is, in points.
///
/// Thinner than the system's eight: this border sits *inside* the window, so
/// every point of it is a point of the panel that can no longer be clicked, and
/// it is not the only way to resize (the keyboard and the system menu still
/// work). Five is enough to grab without thinking and small enough that the
/// caption's corners still behave like buttons.
const EDGE: f32 = 5.0;

/// How thick the window's own edge is, in points. See [`outline`].
const STROKE: f32 = 1.0;

/// The close button's fill under the pointer.
///
/// Not [`Palette::danger`], and deliberately not per-theme. This red is a
/// Windows convention rather than a semantic colour, and the palette's `danger`
/// is picked to be legible *as text* on a card — on the dark theme that makes
/// it light enough that a white glyph on it comes to 2.8:1. This fill carries a
/// white glyph at 5.4:1 on either theme.
const CLOSE_FILL: Color32 = Color32::from_rgb(0xC4, 0x2B, 0x1C);

/// The same, while the button is held.
const CLOSE_FILL_HELD: Color32 = Color32::from_rgb(0xA6, 0x24, 0x18);

/// Whether the window is maximised right now.
///
/// `None` before the platform has told us, which is treated as "not maximised":
/// a normal window is the right shape for the frame the flag has not arrived
/// on, and the rounded corners are re-cut the moment it does.
pub fn is_maximized(ctx: &egui::Context) -> bool {
    ctx.input(|input| input.viewport().maximized.unwrap_or(false))
}

/// Draws the caption: the name on the left, the three window buttons on the
/// right, and a drag region across everything between them.
///
/// Call from a top panel of exactly [`CAPTION_HEIGHT`] and full window width:
/// the layout is measured off [`Ui::max_rect`], because the buttons have to end
/// flush with the window's own edge and no amount of item spacing gets that
/// right.
pub fn caption(ui: &mut Ui, palette: &Palette) {
    let window = ui.max_rect();
    let layout = layout(window);
    let ctx = ui.ctx().clone();
    let maximized = is_maximized(&ctx);
    let painter = ui.painter().clone();

    // The name, drawn at the left edge of whatever the buttons left. Drawn
    // rather than laid out so that it lands on the same centre line as the
    // glyphs opposite, whatever the font's metrics do to a row's height.
    painter.text(
        Pos2::new(layout.drag.left() + space::M, window.center().y),
        Align2::LEFT_CENTER,
        TITLE,
        FontId::proportional(TITLE_SIZE),
        palette.text,
    );

    // Moving the window, and the double-click. `click_and_drag` because a
    // double-click needs the click half of it; the single click it also accepts
    // does nothing on purpose — a caption that reacted to one would be a
    // surprise, not a feature.
    let drag = ui.interact(
        layout.drag,
        ui.id().with("caption-drag"),
        Sense::click_and_drag(),
    );
    if drag.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if drag.double_clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }

    if button(
        ui,
        &painter,
        layout.minimize,
        "caption-minimize",
        Glyph::Minimize,
        palette,
        false,
    )
    .clicked()
    {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }

    let (glyph, wanted) = if maximized {
        (Glyph::Restore, false)
    } else {
        (Glyph::Maximize, true)
    };
    if button(
        ui,
        &painter,
        layout.maximize,
        "caption-maximize",
        glyph,
        palette,
        false,
    )
    .clicked()
    {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(wanted));
    }

    if button(
        ui,
        &painter,
        layout.close,
        "caption-close",
        Glyph::Close,
        palette,
        true,
    )
    .clicked()
    {
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
}

/// Where the caption's parts go, for a window of this size.
///
/// Pure arithmetic, apart from the drawing, so the tests can state the two
/// properties that matter — the buttons are inside the window and do not
/// overlap each other, and between them and the edge they leave a drag region
/// that reaches the other side — without standing up a window to see it.
struct Layout {
    minimize: Rect,
    maximize: Rect,
    close: Rect,
    drag: Rect,
}

/// The dimensions [`Layout`] is built from, and the buttons' order.
///
/// Right to left, as Windows has them: close at the corner, then maximise, then
/// minimise.
fn layout(window: Rect) -> Layout {
    let nth = |column: f32| {
        let right = window.right() - BUTTON_WIDTH * column;
        Rect::from_min_max(
            Pos2::new(right - BUTTON_WIDTH, window.top()),
            Pos2::new(right, window.bottom()),
        )
    };

    let close = nth(0.0);
    let maximize = nth(1.0);
    let minimize = nth(2.0);

    let drag = Rect::from_min_max(window.min, Pos2::new(minimize.left(), window.bottom()));

    Layout {
        minimize,
        maximize,
        close,
        drag,
    }
}

/// One window button: the hover fill, the glyph, and the response.
///
/// The fill is a plain rectangle on purpose, with no corner radius, and it is
/// meant to look clipped at the top of the window: the close button's fill
/// reaches the window's own corner and the window is rounded there, so the
/// shape that ends up on screen is the rounded corner plus a square fill —
/// which is what Windows 11 draws and what a radius here would get wrong.
///
/// `danger` selects the one button whose fill is not a palette control colour.
fn button(
    ui: &Ui,
    painter: &egui::Painter,
    rect: Rect,
    id: &str,
    glyph: Glyph,
    palette: &Palette,
    danger: bool,
) -> egui::Response {
    let response = ui.interact(rect, ui.id().with(id), Sense::click());

    let fill = if response.is_pointer_button_down_on() {
        if danger {
            CLOSE_FILL_HELD
        } else {
            palette.control_active
        }
    } else if response.hovered() {
        if danger {
            CLOSE_FILL
        } else {
            palette.control_hover
        }
    } else {
        Color32::TRANSPARENT
    };

    if fill != Color32::TRANSPARENT {
        painter.rect_filled(rect, CornerRadius::ZERO, fill);
    }

    let ink = if danger && response.hovered() {
        // White on the red, which is the only fill here that is dark enough to
        // need it: the neutral fills take the window's own text colour.
        Color32::WHITE
    } else {
        palette.text
    };
    glyph.draw(painter, rect.center(), ink);

    response
}

/// The glyphs, drawn rather than typed.
///
/// A font would have to be the system's symbol face — the panel installs a CJK
/// face and a Latin one, neither of which has these marks — and a glyph from a
/// different face would arrive at a different weight and optical size from the
/// one-point strokes around it. Four shapes is not worth a font.
#[derive(Clone, Copy)]
enum Glyph {
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl Glyph {
    fn draw(self, painter: &egui::Painter, centre: Pos2, ink: Color32) {
        let stroke = Stroke::new(GLYPH_STROKE, ink);
        let half = GLYPH / 2.0;

        match self {
            Glyph::Minimize => {
                painter.line_segment(
                    [
                        Pos2::new(centre.x - half, centre.y),
                        Pos2::new(centre.x + half, centre.y),
                    ],
                    stroke,
                );
            }
            Glyph::Maximize => {
                painter.rect_stroke(
                    Rect::from_center_size(centre, Vec2::splat(GLYPH)),
                    // Slightly rounded, as the system draws it — and no more, or
                    // at ten points it reads as a circle.
                    CornerRadius::same(2),
                    stroke,
                    StrokeKind::Inside,
                );
            }
            Glyph::Restore => {
                // The front sheet, and the two edges of the back one that it
                // does not cover. Drawing the back sheet whole would run its
                // outline through the front one, and the overlap is the whole
                // reason the mark reads as two windows rather than a mess.
                let gap = 2.0;
                let front = Rect::from_min_size(
                    Pos2::new(centre.x - half, centre.y - half + gap),
                    Vec2::splat(GLYPH - gap),
                );
                let back = front.translate(Vec2::new(gap, -gap));

                painter.rect_stroke(front, CornerRadius::same(1), stroke, StrokeKind::Inside);
                painter.line_segment([back.left_top(), back.right_top()], stroke);
                painter.line_segment(
                    [back.right_top(), Pos2::new(back.right(), front.top())],
                    stroke,
                );
            }
            Glyph::Close => {
                let a = Pos2::new(centre.x - half, centre.y - half);
                let b = Pos2::new(centre.x + half, centre.y + half);
                painter.line_segment([a, Pos2::new(b.x, a.y)], stroke);
                painter.line_segment([Pos2::new(a.x, b.y), b], stroke);
            }
        }
    }
}

/// The invisible border that resizes the window, standing in for the system
/// frame an undecorated window does not have. See the module docs.
///
/// Skipped while maximised: there is nothing to resize, and eight zones along
/// the screen's own edges would swallow clicks meant for the desktop and the
/// taskbar.
pub fn resize_border(ctx: &egui::Context, maximized: bool) {
    if maximized {
        return;
    }

    let window = ctx.input(|input| input.viewport_rect());
    // Two zones have to fit on an edge with something between them, or the
    // corners would swallow the sides. Below this the border is simply not
    // offered; the window can still be resized from the keyboard and the system
    // menu, and no panel this size is worth a border that fights itself.
    if window.width() < 4.0 * EDGE || window.height() < 4.0 * EDGE {
        return;
    }

    for (index, (rect, direction)) in zones(window).into_iter().enumerate() {
        // One area per zone rather than one area holding eight widgets: each
        // area's own rect is then the zone, which is the least that can go wrong
        // on the first frame, before egui has learned where the area ended up.
        let _ = egui::Area::new(Id::new(("fxtrumpet-resize", index)))
            .order(Order::Foreground)
            .fixed_pos(rect.min)
            .default_size(rect.size())
            .show(ctx, |ui| {
                let (_, response) = ui.allocate_exact_size(rect.size(), Sense::drag());
                let response = response.on_hover_cursor(cursor(direction));

                // `drag_started`, not `clicked`: `BeginResize` posts a
                // non-client button-down and the OS then runs its own modal
                // loop, which waits for a release that has already happened if
                // this were fired on a click — leaving the window stuck to the
                // pointer until the next one.
                if response.drag_started() && ui.input(|input| input.pointer.primary_down()) {
                    ui.ctx()
                        .send_viewport_cmd(ViewportCommand::BeginResize(direction));
                }
            });
    }
}

/// The eight grab zones, in window coordinates: four corners, then four sides.
///
/// Corner zones are square and the sides are what is left between them, so no
/// two zones overlap and the border is exactly tiled. Overlap would matter more
/// here than it looks: the layers are hit-tested topmost-first, so whichever of
/// two zones was allocated last would silently own the shared corner and dragging
/// it would resize along one axis only.
fn zones(window: Rect) -> [(Rect, ResizeDirection); 8] {
    let (left, top) = (window.left(), window.top());
    let (right, bottom) = (window.right(), window.bottom());

    let nw = Rect::from_min_max(Pos2::new(left, top), Pos2::new(left + EDGE, top + EDGE));
    let ne = Rect::from_min_max(Pos2::new(right - EDGE, top), Pos2::new(right, top + EDGE));
    let sw = Rect::from_min_max(Pos2::new(left, bottom - EDGE), Pos2::new(left + EDGE, bottom));
    let se = Rect::from_min_max(
        Pos2::new(right - EDGE, bottom - EDGE),
        Pos2::new(right, bottom),
    );

    let north = Rect::from_min_max(Pos2::new(nw.right(), top), Pos2::new(ne.left(), nw.bottom()));
    let south = Rect::from_min_max(
        Pos2::new(sw.right(), sw.top()),
        Pos2::new(se.left(), se.bottom()),
    );
    let west = Rect::from_min_max(Pos2::new(left, nw.bottom()), Pos2::new(nw.right(), sw.top()));
    let east = Rect::from_min_max(
        Pos2::new(ne.left(), ne.bottom()),
        Pos2::new(ne.right(), se.top()),
    );

    [
        (nw, ResizeDirection::NorthWest),
        (ne, ResizeDirection::NorthEast),
        (sw, ResizeDirection::SouthWest),
        (se, ResizeDirection::SouthEast),
        (north, ResizeDirection::North),
        (south, ResizeDirection::South),
        (west, ResizeDirection::West),
        (east, ResizeDirection::East),
    ]
}

/// The arrow a zone shows under the pointer.
fn cursor(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North => CursorIcon::ResizeNorth,
        ResizeDirection::South => CursorIcon::ResizeSouth,
        ResizeDirection::East => CursorIcon::ResizeEast,
        ResizeDirection::West => CursorIcon::ResizeWest,
        ResizeDirection::NorthEast => CursorIcon::ResizeNorthEast,
        ResizeDirection::NorthWest => CursorIcon::ResizeNorthWest,
        ResizeDirection::SouthEast => CursorIcon::ResizeSouthEast,
        ResizeDirection::SouthWest => CursorIcon::ResizeSouthWest,
    }
}

/// Draws the one-point edge the system frame used to draw.
///
/// An undecorated window gets no drop shadow, so without this it is a flat
/// rectangle of colour with nothing at all to say where it ends — and next to a
/// shadowed window it reads as unfinished rather than minimal.
///
/// [`Palette::border_strong`] rather than [`Palette::border`]: this is the edge
/// between the window and the *desktop*, not between a card and the window, and
/// the faint one that is right for a card disappears here against a light
/// wallpaper. It is the palette's own name for "an edge that has to be seen".
///
/// The radius is the one [`super::window_shape`] cuts out of the window; if the
/// two ever disagreed, the edge would either float inside the corner or be
/// clipped by it.
///
/// ## Why the rectangle is snapped rather than shrunk
///
/// This used to be `viewport_rect().shrink(0.5)` with [`StrokeKind::Inside`],
/// which covers the span `[0.5, 1.5]` points. At 100 % scaling that straddles
/// two physical pixels, and — worse — it leaves pixel 0 covered by nothing but
/// the panel's own fill. The seam between that one-pixel band of fill and the
/// hairline beside it *is* visible: on the left and top edges it reads as a two
/// to three pixel strip, because the fill gap and the feathered line are
/// different colours sitting next to each other.
///
/// The stroke is instead placed so that it exactly covers the window's outermost
/// physical pixel: the rectangle is snapped outward to a whole pixel, and then
/// shrunk by half a stroke *in pixels*, so `[0, 1]` in physical pixels is the
/// stroke and nothing of the panel's fill survives outside it. See [`edge_rect`].
pub fn outline(ctx: &egui::Context, palette: &Palette, maximized: bool) {
    if maximized {
        return;
    }

    let (rect, pixels_per_point) = ctx.input(|input| {
        (input.viewport_rect(), input.pixels_per_point())
    });
    let rect = edge_rect(rect, STROKE, pixels_per_point);

    ctx.layer_painter(egui::LayerId::new(
        Order::Foreground,
        Id::new("fxtrumpet-outline"),
    ))
    .rect_stroke(
        rect,
        CornerRadius::same(theme::radius::CARD),
        Stroke::new(STROKE, palette.border_strong),
        StrokeKind::Inside,
    );
}

/// The rectangle whose [`StrokeKind::Inside`] stroke lands exactly on the
/// window's outermost physical pixel.
///
/// Two steps, and the order matters:
///
/// * The viewport's own edges are rounded to whole physical pixels. egui's
///   rectangles are in points and a window is in pixels; at a fractional scale
///   (`pixels_per_point = 1.25`) the two disagree, and an unsnapped edge lands
///   partway through a pixel and is feathered across two.
/// * Then half a stroke is taken off each side, still in pixels, so the stroke —
///   which [`StrokeKind::Inside`] draws inward from the rectangle — occupies
///   `[0, 1]` rather than `[0.5, 1.5]`.
///
/// The half-stroke offset is deliberately applied *after* the snap and not
/// snapped again: `snap(v + half) - half` is not `snap(v)` when `half` is 0.5,
/// because rounding 0.5 goes up and the two errors do not cancel. Snapping the
/// viewport edge and then offsetting in pixels keeps the stroke's outer edge on
/// the pixel boundary it was snapped to.
///
/// The result is the rectangle to hand to `rect_stroke`, not the edge's
/// footprint: the stroke drawn inside this rectangle is the footprint.
fn edge_rect(viewport: Rect, stroke: f32, pixels_per_point: f32) -> Rect {
    let scale = if pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let half = stroke / 2.0;

    // Points -> pixels, snap to a whole pixel, pixels -> points.
    let snap = |value: f32| (value * scale).round() / scale;

    Rect::from_min_max(
        Pos2::new(snap(viewport.left()), snap(viewport.top())),
        Pos2::new(snap(viewport.right()), snap(viewport.bottom())),
    )
    .shrink(half)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window of about the size the panel opens at.
    fn window() -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(660.0, 820.0))
    }

    /// The buttons are flush with the corner the system puts them in, and none
    /// of them overlaps another.
    #[test]
    fn the_window_buttons_sit_in_the_corner_without_overlapping() {
        let window = window();
        let layout = layout(window);

        assert!(
            layout.close.right_top() == window.right_top(),
            "the close button is not in the window's corner"
        );
        for button in [layout.close, layout.maximize, layout.minimize] {
            assert!(window.contains_rect(button), "{button:?} escapes the window");
            assert_eq!(button.top(), window.top(), "a button is off the top edge");
            assert_eq!(
                button.bottom(),
                window.bottom(),
                "a button is off the bottom edge"
            );
        }
        assert!(
            layout.minimize.right() <= layout.maximize.left()
                && layout.maximize.right() <= layout.close.left(),
            "the buttons overlap, so one of them would never be clickable"
        );
    }

    /// Everything the buttons do not use is draggable — including the strip
    /// behind the name, which is where a user aims when they want to move a
    /// window.
    #[test]
    fn the_rest_of_the_caption_drags_the_window() {
        let window = window();
        let layout = layout(window);

        assert_eq!(layout.drag.left(), window.left());
        assert_eq!(layout.drag.right(), layout.minimize.left());
        assert_eq!(layout.drag.height(), window.height());
        assert!(
            layout.drag.width() > 2.0 * space::M,
            "the drag region is too small to hit"
        );
    }

    /// The border is tiled exactly by its zones: no gaps to fall through, and no
    /// two zones claiming the same pixel.
    ///
    /// Zones that merely *touch* are the point, not a mistake — they share an
    /// edge and no area — which is why this asks for the intersection's area
    /// rather than `Rect::intersects`: that one counts a shared edge, and every
    /// pair of neighbours here shares one.
    #[test]
    fn the_resize_zones_tile_the_border() {
        let window = window();
        let zones = zones(window);

        let mut area = 0.0;
        for (index, (rect, _)) in zones.iter().enumerate() {
            area += rect.width() * rect.height();
            assert!(window.contains_rect(*rect), "{rect:?} escapes the window");
            for (other, _) in &zones[index + 1..] {
                let shared = rect.intersect(*other);
                assert!(
                    shared.width() <= 0.0 || shared.height() <= 0.0,
                    "{rect:?} and {other:?} share {shared:?}"
                );
            }
        }

        let (w, h) = (window.width(), window.height());
        let border = 2.0 * EDGE * w + 2.0 * EDGE * h - 4.0 * EDGE * EDGE;
        assert!(
            (area - border).abs() < 1e-3,
            "the zones cover {area} of a {border} border"
        );
    }

    /// Each zone names the edge it is actually on. A corner that reported a side
    /// direction would resize along one axis while the pointer pulled along two,
    /// which is the kind of thing that looks like lag rather than a bug.
    #[test]
    fn every_resize_zone_names_the_edge_it_is_on() {
        let window = window();
        let (left, top) = (window.left(), window.top());
        let (right, bottom) = (window.right(), window.bottom());

        for (rect, direction) in zones(window) {
            let expected = match direction {
                ResizeDirection::NorthWest => rect.left() == left && rect.top() == top,
                ResizeDirection::NorthEast => rect.right() == right && rect.top() == top,
                ResizeDirection::SouthWest => rect.left() == left && rect.bottom() == bottom,
                ResizeDirection::SouthEast => rect.right() == right && rect.bottom() == bottom,
                ResizeDirection::North => rect.top() == top && rect.width() > EDGE,
                ResizeDirection::South => rect.bottom() == bottom && rect.width() > EDGE,
                ResizeDirection::West => rect.left() == left && rect.height() > EDGE,
                ResizeDirection::East => rect.right() == right && rect.height() > EDGE,
            };
            assert!(expected, "{direction:?} is not on the edge in {rect:?}");
        }
    }

    /// A click on the close button has to reach the platform as a close.
    ///
    /// Runs headless but real: three frames of actual pointer input against the
    /// caption itself, the press and the release split across frames the way a
    /// click always is. Nothing here asserts that a `Response` said "clicked" —
    /// it asserts the command came out the far end, which is the only part the
    /// user can tell apart.
    #[test]
    fn clicking_close_asks_the_window_to_close() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        let window = window();
        let at = layout(window).close.center();

        let frame = |event: egui::Event| {
            let full = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(window),
                    events: vec![event],
                    ..Default::default()
                },
                |ui| caption(ui, theme::palette(&ctx)),
            );
            // The commands come out of the viewport output, not the platform
            // output: the latter carries the clipboard and URL commands, and
            // the frame drains the per-viewport list into this one.
            full.viewport_output
                .into_values()
                .flat_map(|viewport| viewport.commands)
                .collect::<Vec<_>>()
        };

        // Before the press, so the widget exists and has been hovered once;
        // egui decides what is under the pointer on the frame it learns about
        // it, and a click on a widget it has never seen can be missed.
        let moved = frame(egui::Event::PointerMoved(at)).is_empty();

        let mut close = false;
        for pressed in [true, false] {
            close |= frame(egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            })
            .iter()
            .any(|command| matches!(command, ViewportCommand::Close));
        }

        assert!(moved, "merely moving the pointer closed the window");
        assert!(close, "clicking the close button did not close the window");
    }

    /// Runs one frame per entry in `events` (a `None` is a frame with no input)
    /// against [`resize_border`], and collects every viewport command they
    /// produced between them.
    fn resize_frames(
        ctx: &egui::Context,
        window: Rect,
        maximized: bool,
        events: Vec<Option<egui::Event>>,
    ) -> Vec<ViewportCommand> {
        let mut commands = Vec::new();
        for event in events {
            let full = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(window),
                    events: event.into_iter().collect(),
                    ..Default::default()
                },
                |_| resize_border(ctx, maximized),
            );
            commands.extend(
                full.viewport_output
                    .into_values()
                    .flat_map(|viewport| viewport.commands),
            );
        }
        commands
    }

    /// A pointer event at `pos`, pressed or released.
    fn pointer_at(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// How many frames of no input a fresh border needs before the pointer is
    /// really over it.
    ///
    /// Not padding, and measured rather than guessed: an `Area` is hit-tested
    /// from the list the *previous* frame built, so a zone that first exists on
    /// frame 1 is invisible to the pointer on frame 2 and only live from frame
    /// 3. In the running panel that is 60 ms of startup nobody can see, but a
    /// test that presses on frame 2 measures the warm-up instead of the border.
    const SETTLE_FRAMES: usize = 2;

    /// The frames a test needs to press on a settled border: point at it, let it
    /// settle, press, release.
    fn point_settle_press_release(corner: Pos2) -> Vec<Option<egui::Event>> {
        let mut events = vec![Some(egui::Event::PointerMoved(corner))];
        events.extend(std::iter::repeat_n(None, SETTLE_FRAMES));
        events.push(Some(pointer_at(corner, true)));
        events.push(Some(pointer_at(corner, false)));
        events
    }

    /// A press on the border actually asks the platform for a resize.
    ///
    /// The tests above say the zones are in the right places; this one says they
    /// are *reachable*. An `Area` that never gets allocated, or a `Sense` that
    /// never reports a drag, would satisfy every one of them and still leave a
    /// window nobody can resize — which, in an undecorated window, is the whole
    /// of the resize affordance.
    #[test]
    fn pressing_the_border_asks_for_a_resize() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        let window = window();
        let corner = zones(window)[0].0.center();
        let commands = resize_frames(&ctx, window, false, point_settle_press_release(corner));

        assert!(
            commands.iter().any(|command| matches!(
                command,
                ViewportCommand::BeginResize(ResizeDirection::NorthWest)
            )),
            "a press in the top-left corner asked for {commands:?}"
        );
    }

    /// The zones win against whatever they overlap.
    ///
    /// This is the whole reason they are widgets on the foreground layer rather
    /// than coordinates tested against the pointer. The caption's buttons touch
    /// the top edge, so a press in the corner they share reaches one of them;
    /// if the button saw it too, the press would start a resize *and* press the
    /// button — and since a resize hands the mouse to an OS modal loop, the
    /// button would never see its release and would be left stuck down.
    #[test]
    fn a_resize_zone_wins_against_a_widget_underneath_it() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        let window = window();
        let corner = zones(window)[0].0.center();

        // A stand-in for the panel: an ordinary widget in the background layer,
        // sitting exactly where the corner zone sits on top of it.
        let panel = Rect::from_min_size(Pos2::ZERO, Vec2::splat(64.0));

        let mut under = None;
        for event in point_settle_press_release(corner) {
            let mut hovered = false;
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(window),
                    events: event.into_iter().collect(),
                    ..Default::default()
                },
                |ui| {
                    hovered = ui.interact(panel, Id::new("panel"), Sense::click()).hovered();
                    resize_border(&ctx, false);
                },
            );
            under = Some(hovered);
        }

        assert_eq!(
            under,
            Some(false),
            "the widget under the corner still saw the pointer"
        );
    }

    /// While maximised there is no border to grab: the zones would sit on the
    /// screen's own edges and swallow clicks meant for the desktop.
    #[test]
    fn a_maximised_window_offers_no_resize_zones() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        let window = window();
        let corner = zones(window)[0].0.center();
        let commands = resize_frames(&ctx, window, true, point_settle_press_release(corner));

        assert!(
            commands
                .iter()
                .all(|command| !matches!(command, ViewportCommand::BeginResize(_))),
            "a maximised window still offered a resize: {commands:?}"
        );
    }

    /// The outline's stroke must cover the window's outermost physical pixel and
    /// nothing else.
    ///
    /// This is the regression test for the two-to-three pixel strip along the
    /// left and top edges. The old `shrink(0.5)` put the stroke at `[0.5, 1.5]`,
    /// which left pixel 0 as bare panel fill and feathered the line over pixels
    /// 1 and 2. Asserting on the *footprint* — the rectangle the stroke covers —
    /// is what makes this a test of the pixels rather than of the arithmetic.
    #[test]
    fn the_outline_covers_the_outermost_pixel_and_no_more() {
        let viewport = window();

        for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0] {
            let rect = edge_rect(viewport, STROKE, scale);
            let rect = rect.expand(STROKE / 2.0); // StrokeKind::Inside -> the footprint

            // The footprint starts on the window's edge, to within a pixel.
            for (edge, found, wanted) in [
                ("left", rect.left(), viewport.left()),
                ("top", rect.top(), viewport.top()),
                ("right", rect.right(), viewport.right()),
                ("bottom", rect.bottom(), viewport.bottom()),
            ] {
                let error_px = (found - wanted).abs() * scale;
                assert!(
                    error_px < 0.5,
                    "at {scale}x the {edge} of the outline is {error_px:.2} px off the \
                     window edge, so the edge pixel is not fully covered"
                );
            }

            // And it is exactly one point thick — which is one physical pixel at
            // 100 % scaling and proportionally more above it. A system border
            // scales with the display in the same way, so the assertion is on
            // the point thickness and not on a pixel count.
            for (edge, span) in [
                ("left", (rect.left(), rect.left() + STROKE)),
                ("top", (rect.top(), rect.top() + STROKE)),
            ] {
                let points = span.1 - span.0;
                assert!(
                    (points - STROKE).abs() < f32::EPSILON,
                    "at {scale}x the {edge} stroke is {points} points, not {STROKE}"
                );
                // At 100 % that is one pixel; the tests above are what keep it
                // on the outermost one.
                if scale == 1.0 {
                    assert!(
                        (points * scale - 1.0).abs() < 0.5,
                        "at 1x the {edge} stroke covers {points} px, not one"
                    );
                }
            }
        }
    }

    /// The snapped rectangle stays inside the window, at every scale.
    ///
    /// The snap rounds both edges by the same rule, so a window an odd number of
    /// pixels wide keeps its width rather than growing one — which matters
    /// because the stroke is drawn *inside* this rectangle and anything outside
    /// the window would be cut off by the window region.
    #[test]
    fn the_outline_never_escapes_the_window() {
        for width in [400.0_f32, 401.0, 660.0, 661.0, 1024.0] {
            let viewport = Rect::from_min_size(Pos2::new(30.0, 70.0), Vec2::new(width, 820.0));

            for scale in [1.0_f32, 1.25, 1.5, 2.0] {
                let rect = edge_rect(viewport, STROKE, scale);

                assert!(
                    viewport.contains_rect(rect),
                    "at {scale}x and {width} wide, {rect:?} escapes {viewport:?}"
                );
                // The footprint, which is what is actually painted.
                let footprint = rect.expand(STROKE / 2.0);
                let slack = 0.5 / scale;
                assert!(
                    footprint.left() >= viewport.left() - slack
                        && footprint.top() >= viewport.top() - slack
                        && footprint.right() <= viewport.right() + slack
                        && footprint.bottom() <= viewport.bottom() + slack,
                    "at {scale}x the painted outline {footprint:?} is off the window"
                );
            }
        }
    }

    /// A degenerate scale must not produce a NaN or inverted rectangle.
    #[test]
    fn the_outline_survives_a_useless_scale() {
        let viewport = window();

        for scale in [0.0_f32, -1.0] {
            let rect = edge_rect(viewport, STROKE, scale);

            assert!(rect.width() > 0.0 && rect.height() > 0.0, "{rect:?}");
            assert!(rect.is_finite(), "{rect:?}");
            assert!(viewport.contains_rect(rect), "{rect:?}");
        }
    }
}
