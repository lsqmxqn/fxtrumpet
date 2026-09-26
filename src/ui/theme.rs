//! The panel's visual design system.
//!
//! Everything the panel draws reads its colours and metrics from here, so a
//! change of taste is a change to one file rather than a hunt through
//! `panel.rs` for hex literals.
//!
//! ## Why a palette per theme instead of one palette
//!
//! egui defaults to [`egui::ThemePreference::System`], which is the right
//! behaviour for a utility that sits next to other Windows apps — but it means
//! the panel renders light on a light desktop, and *any* colour written as a
//! literal is wrong half the time. The panel used to paint its spectrum meter
//! on a hardcoded near-black, which is why the meter read as a hole punched in
//! a light window. So: two palettes, chosen by the theme egui is actually
//! rendering with, and custom painting asks for [`palette`] rather than
//! deciding for itself.
//!
//! ## Contrast
//!
//! Text colours are picked for a measured contrast ratio against the surface
//! they sit on, not by eye:
//!
//! * `text` clears 13:1 on both themes — headings and values.
//! * `text_weak` clears 6:1 — labels, secondary lines. Comfortably past
//!   WCAG AA's 4.5:1 for body text.
//! * `text_faint` is ~4:1 and only for the plot's graticule labels, which are
//!   supplementary to the numbers printed beside them.
//!
//! `success` / `warning` / `danger` are chosen to pass 4.5:1 **as text**, which
//! is deliberate: the status indicator is a coloured dot *plus* a word in the
//! normal text colour, so the dot carries the hue and the word carries the
//! meaning. A dot alone would be unreadable to a colour-blind user, and a
//! bright-on-white green would have failed as text.
//!
//! ## Why `weak_text_color` is set explicitly
//!
//! egui derives weak text as `text * weak_text_alpha` (0.6). On a light theme
//! that lands around 3:1 — which is what made the old footer's device lines and
//! counters so hard to read. [`apply`] overrides it with a solid colour instead
//! of a fade, so "secondary" stops meaning "faint".
//!
//! ## Why `TextStyle::Small` is redefined
//!
//! egui's default is **9 points**. That is a footnote size, and the panel's
//! whole lower half was set in it. [`apply`] raises it; nothing else in this
//! file depends on egui's defaults.

use eframe::egui::{
    self, Color32, CornerRadius, Margin, Response, RichText, Sense, Shadow, Stroke, StrokeKind,
    Theme, Ui, Vec2,
};

/// The 4-point spacing scale. Every gap in the panel is one of these.
pub mod space {
    /// Label to control, icon to text.
    pub const XS: f32 = 4.0;
    /// Between the rows of a list.
    pub const S: f32 = 8.0;
    /// Card padding, and between cards.
    pub const M: f32 = 12.0;
    /// Between the major regions of the window.
    pub const L: f32 = 16.0;
    /// Breathing room above a section heading.
    pub const XL: f32 = 24.0;
}

/// Corner radii, in points.
pub mod radius {
    /// Meter bars, the EQ readout pill: fills too small for [`radius::CONTROL`].
    pub const SMALL: u8 = 3;
    /// Cards.
    pub const CARD: u8 = 8;
    /// Buttons, inputs, slider rails.
    pub const CONTROL: u8 = 6;
    /// The status pill and the toggle track: half the height, so it reads round.
    pub const PILL: u8 = 10;
}

/// The type scale. Every size the interface names comes from here.
///
/// Two of these mirror the text styles [`apply`] installs — `SMALL` is what
/// `TextStyle::Small` resolves to and `HEADING` what `Heading` does — so a
/// label can ask for `.small()` or a painter can ask for
/// `FontId::proportional(font::SMALL)` and both land on the same line height.
/// The point of having them named anyway is the other three sizes: a call site
/// that needs a size that is *not* one of the five text styles still has to
/// pick from this scale, not invent a sixth size.
pub mod font {
    /// Plot-axis labels and other marks drawn inside a fixed-size drawing.
    ///
    /// Below `SMALL` on purpose, and only for graphics: real text never gets
    /// this small, because it sits beside numbers that are readable at `SMALL`
    /// and the smaller face is what keeps the axis from colliding with itself.
    pub const MICRO: f32 = 10.0;
    /// Hints, secondary lines, empty states — `TextStyle::Small`.
    pub const SMALL: f32 = 11.5;
    /// Row names, the caption's title, the status pill's word.
    pub const NAME: f32 = 12.5;
    /// Body text, buttons, card headings — `TextStyle::Body` and `Button`.
    pub const BODY: f32 = 13.0;
    /// Window titles and card titles promoted above their cards —
    /// `TextStyle::Heading`.
    pub const HEADING: f32 = 15.5;
}

/// One theme's worth of colour.
///
/// Fields are named for the *role* they play, not the colour they are, so the
/// two palettes can differ in hue without any call site caring.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// True when this is the dark palette.
    pub dark: bool,

    /// The window behind the cards.
    pub bg: Color32,
    /// Cards, and anything raised off the window.
    pub surface: Color32,
    /// Wells: slider rails, text edits, the meter trough, the EQ plot.
    pub sunken: Color32,
    /// Resting control fill: buttons, the off-state toggle track.
    pub control: Color32,
    /// Control fill under the pointer.
    pub control_hover: Color32,
    /// Control fill while held.
    pub control_active: Color32,

    /// Card outlines and control edges.
    pub border: Color32,
    /// Edges that need to be seen: the slider rail, an unfocused input.
    pub border_strong: Color32,

    /// Headings, values, primary text.
    pub text: Color32,
    /// Labels and secondary lines.
    pub text_weak: Color32,
    /// Graticule labels and other supplementary marks.
    pub text_faint: Color32,

    /// The one accent: selection, slider fill, the EQ curve, focus.
    pub accent: Color32,
    /// Text and glyphs drawn *on* the accent.
    pub on_accent: Color32,

    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,

    /// Drop shadow for menus and combo popups.
    pub shadow: Color32,
}

/// Light theme.
///
/// The window is a shade off white and the cards are pure white, so a card
/// reads as raised without needing a shadow to say so.
pub const LIGHT: Palette = Palette {
    dark: false,

    bg: Color32::from_rgb(0xF2, 0xF4, 0xF7),
    surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    sunken: Color32::from_rgb(0xE9, 0xEC, 0xF1),
    control: Color32::from_rgb(0xF7, 0xF9, 0xFB),
    control_hover: Color32::from_rgb(0xEC, 0xF0, 0xF5),
    control_active: Color32::from_rgb(0xE1, 0xE7, 0xEE),

    border: Color32::from_rgb(0xE3, 0xE7, 0xED),
    border_strong: Color32::from_rgb(0xCF, 0xD6, 0xDF),

    text: Color32::from_rgb(0x14, 0x1A, 0x21),
    text_weak: Color32::from_rgb(0x58, 0x61, 0x72),
    text_faint: Color32::from_rgb(0x7B, 0x84, 0x92),

    accent: Color32::from_rgb(0x25, 0x63, 0xEB),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),

    success: Color32::from_rgb(0x0F, 0x7B, 0x4F),
    warning: Color32::from_rgb(0x9A, 0x64, 0x00),
    danger: Color32::from_rgb(0xC0, 0x39, 0x2B),

    shadow: Color32::from_rgba_premultiplied(0x0F, 0x17, 0x24, 0x1E),
};

/// Dark theme.
///
/// Near-black window with a lifted card, matching Windows 11's dark surfaces
/// closely enough to sit beside them without looking foreign.
pub const DARK: Palette = Palette {
    dark: true,

    bg: Color32::from_rgb(0x14, 0x17, 0x1C),
    surface: Color32::from_rgb(0x1C, 0x20, 0x26),
    sunken: Color32::from_rgb(0x23, 0x28, 0x2F),
    control: Color32::from_rgb(0x24, 0x2A, 0x32),
    control_hover: Color32::from_rgb(0x2C, 0x33, 0x3C),
    control_active: Color32::from_rgb(0x34, 0x3C, 0x47),

    border: Color32::from_rgb(0x2A, 0x30, 0x38),
    border_strong: Color32::from_rgb(0x3A, 0x42, 0x4C),

    text: Color32::from_rgb(0xE9, 0xEC, 0xF1),
    text_weak: Color32::from_rgb(0xA3, 0xAD, 0xBA),
    text_faint: Color32::from_rgb(0x7C, 0x87, 0x97),

    accent: Color32::from_rgb(0x5B, 0x9D, 0xFF),
    on_accent: Color32::from_rgb(0x0B, 0x12, 0x1E),

    success: Color32::from_rgb(0x3D, 0xDC, 0x97),
    warning: Color32::from_rgb(0xF5, 0xA6, 0x23),
    danger: Color32::from_rgb(0xFF, 0x6B, 0x5E),

    shadow: Color32::from_rgba_premultiplied(0x00, 0x00, 0x00, 0x59),
};

/// The palette matching the theme egui is rendering with right now.
///
/// Custom painting has to go through this rather than naming `LIGHT` or `DARK`:
/// the panel follows the system theme, so which one is correct is only knowable
/// at paint time.
pub fn palette(ctx: &egui::Context) -> &'static Palette {
    if ctx.theme() == Theme::Dark {
        &DARK
    } else {
        &LIGHT
    }
}

/// Installs the design system for both themes.
///
/// Called once when the window is created. Both themes are styled up front, not
/// just the active one, so switching the system theme while the panel is open
/// does not flash an unstyled frame.
pub fn apply(ctx: &egui::Context) {
    for (theme, palette) in [(Theme::Light, &LIGHT), (Theme::Dark, &DARK)] {
        ctx.set_visuals_of(theme, visuals(palette));
        ctx.style_mut_of(theme, |style| {
            let spacing = &mut style.spacing;
            // Half of egui's default: its generous gaps are tuned for a window
            // full of loose widgets, and this one is a dense instrument panel.
            spacing.item_spacing = Vec2::new(space::S, space::XS + 2.0);
            spacing.button_padding = Vec2::new(12.0, 5.0);
            spacing.interact_size.y = 22.0;
            spacing.slider_rail_height = 6.0;
            spacing.slider_width = 150.0;
            spacing.icon_width = 16.0;
            spacing.menu_margin = Margin::symmetric(6, 6);
            spacing.menu_spacing = 3.0;

            let styles = &mut style.text_styles;
            // egui's Small is 9pt, which is where the panel's illegible
            // secondary text came from. See the module docs.
            styles.insert(
                egui::TextStyle::Small,
                egui::FontId::proportional(font::SMALL),
            );
            styles.insert(egui::TextStyle::Body, egui::FontId::proportional(font::BODY));
            styles.insert(
                egui::TextStyle::Button,
                egui::FontId::proportional(font::BODY),
            );
            // 15.5, not egui's 18: a card heading is a signpost, not a banner,
            // and at 18 it competed with the values it was labelling.
            styles.insert(
                egui::TextStyle::Heading,
                egui::FontId::proportional(font::HEADING),
            );
            styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(12.5));
        });
    }
}

/// Turns a palette into egui's `Visuals`.
///
/// Starts from egui's own light/dark defaults and overrides what matters,
/// rather than building a `Visuals` from nothing: the struct has ~30 fields and
/// the ones left alone are the ones whose defaults are already fine.
fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.dark_mode = p.dark;
    // A solid colour, where egui's default is a 60%-alpha fade of the text
    // colour. See the module docs — this single line is most of the difference
    // between the old footer and the new one.
    v.weak_text_color = Some(p.text_weak);

    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.sunken;
    v.faint_bg_color = p.sunken;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(radius::CARD);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 20,
        spread: 0,
        color: p.shadow,
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: p.shadow,
    };

    // The slider's filled portion and text selection both read from here.
    v.selection.bg_fill = p.accent;
    v.selection.stroke = Stroke::new(1.0, p.on_accent);
    v.hyperlink_color = p.accent;
    // Without this a slider is a bare rail with a handle floating on it, and
    // the current value is only knowable by reading the number beside it.
    v.slider_trailing_fill = true;

    let w = &mut v.widgets;

    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text_weak);
    w.noninteractive.corner_radius = CornerRadius::same(radius::CONTROL);

    // `bg_fill` is the slider rail and the checkbox box; `weak_bg_fill` is a
    // button's fill. Splitting them is what lets the rail be a visible well
    // while buttons stay near the surface.
    w.inactive.bg_fill = p.sunken;
    w.inactive.weak_bg_fill = p.control;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = CornerRadius::same(radius::CONTROL);

    w.hovered.bg_fill = p.sunken;
    w.hovered.weak_bg_fill = p.control_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = CornerRadius::same(radius::CONTROL);

    w.active.bg_fill = p.sunken;
    w.active.weak_bg_fill = p.control_active;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = CornerRadius::same(radius::CONTROL);

    // An open combo box looks like a held button.
    w.open = w.active;

    v
}

/// Draws a card: the panel's one grouping device.
///
/// A border and a fill, not a shadow. Shadows on a light theme at this size
/// mostly add mud, and the fill difference already reads as raised.
pub fn card<R>(
    ui: &mut Ui,
    palette: &Palette,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> R {
    let frame = egui::Frame::default()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.border))
        .corner_radius(CornerRadius::same(radius::CARD))
        .inner_margin(Margin::symmetric(space::M as i8, space::M as i8));

    frame.show(ui, add_contents).inner
}

/// A card heading, with the card's top padding already accounted for.
///
/// Deliberately a step below the window title: at the old size every heading
/// competed with the values underneath it and nothing read as a group.
pub fn card_title(ui: &mut Ui, palette: &Palette, text: &str) {
    ui.label(
        RichText::new(text)
            .size(font::BODY)
            .strong()
            .color(palette.text),
    );
    ui.add_space(space::S);
}

/// A right-aligned value, in a monospace face so digits line up column to
/// column.
///
/// Call inside a [`Layout::right_to_left`] region; it fills whatever width is
/// left. Proportional digits are different widths, so a column of them is
/// ragged even when every row is right-aligned.
pub fn value(ui: &mut Ui, palette: &Palette, text: &str) {
    ui.label(
        RichText::new(text)
            .monospace()
            .color(palette.text_weak),
    );
}

/// A horizontal green/amber/red level bar, used for the spectrum.
pub fn level_colour(value: f32, palette: &Palette) -> Color32 {
    if value < 0.7 {
        if palette.dark {
            Color32::from_rgb(0x3D, 0xDC, 0x97)
        } else {
            Color32::from_rgb(0x12, 0xA0, 0x6A)
        }
    } else if value < 0.9 {
        if palette.dark {
            Color32::from_rgb(0xF5, 0xA6, 0x23)
        } else {
            Color32::from_rgb(0xC8, 0x7C, 0x00)
        }
    } else if palette.dark {
        Color32::from_rgb(0xFF, 0x6B, 0x5E)
    } else {
        Color32::from_rgb(0xD0, 0x39, 0x2B)
    }
}

/// An on/off switch.
///
/// A switch rather than a checkbox because "processing is on" is a state, not a
/// choice from a set — and because a 36×20 target with a whole knob to hit is
/// easier to click than a 14×14 box.
///
/// Returns the response; the caller reads `changed()` off `on`.
pub fn toggle(ui: &mut Ui, palette: &Palette, id: egui::Id, on: &mut bool, label: &str) -> Response {
    let size = Vec2::new(36.0, 20.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());

    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    // Keyboard parity with a checkbox. Only fires if focus actually landed
    // here, so this is a no-op rather than a surprise when it did not.
    if response.has_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter))
    {
        *on = !*on;
        response.mark_changed();
    }

    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, label)
    });

    if ui.is_rect_visible(rect) {
        // Animated so the knob slides; the id has to be stable across frames and
        // unique per switch, which is why the caller passes it in.
        let t = ui.ctx().animate_bool_with_time(id, *on, 0.10);

        let track = if *on { palette.accent } else { palette.control };
        let edge = if *on {
            palette.accent
        } else {
            palette.border_strong
        };
        ui.painter()
            .rect_filled(rect, CornerRadius::same(radius::PILL), track);
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(radius::PILL),
            Stroke::new(1.0, edge),
            StrokeKind::Inside,
        );

        let inset = 3.0;
        let knob_radius = (rect.height() - inset * 2.0) / 2.0;
        let travel = rect.width() - inset * 2.0 - knob_radius * 2.0;
        let centre_x = rect.left() + inset + knob_radius + travel * t;
        let knob = if *on {
            palette.on_accent
        } else {
            let c = palette.text_weak;
            // On a light theme a mid-grey knob on a near-white track reads as
            // disabled, so the off knob is a step darker there.
            if palette.dark {
                c
            } else {
                Color32::from_rgb(0x6B, 0x74, 0x80)
            }
        };
        ui.painter().circle_filled(
            egui::pos2(centre_x, rect.center().y),
            knob_radius,
            knob,
        );
    }

    response
}

/// A coloured dot followed by a word, for the panel header.
///
/// The dot is the graphic and the word is the message — see the module docs on
/// why the semantics are not carried by colour alone.
pub fn status_pill(ui: &mut Ui, palette: &Palette, colour: Color32, text: &str) {
    let height = 22.0;
    let width = ui
        .painter()
        .layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(font::NAME),
            palette.text,
        )
        .size()
        .x
        + 30.0;

    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(radius::PILL),
        palette.sunken,
    );

    let dot = egui::pos2(rect.left() + 11.0, rect.center().y);
    ui.painter()
        .circle_filled(dot, 3.5, colour);
    // A halo, so a small dot still reads as "lit" at a glance.
    ui.painter().circle_stroke(
        dot,
        6.5,
        Stroke::new(1.0, colour.gamma_multiply(0.35)),
    );

    ui.painter().text(
        egui::pos2(dot.x + 9.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(font::NAME),
        palette.text,
    );
}

/// A label that shrinks to fit and shows the full text on hover.
///
/// Device names are long enough that a five-inch window cannot show two of them
/// honestly, and wrapping them turns one line into three. Truncating and
/// offering the whole string on hover keeps the footer one line tall.
pub fn elided(ui: &mut Ui, palette: &Palette, prefix: &str, text: &str) {
    let response = ui.add(
        egui::Label::new(
            RichText::new(format!("{prefix}  {text}"))
                .small()
                .color(palette.text_weak),
        )
        .truncate()
        .sense(Sense::hover()),
    );
    response.on_hover_text(text);
}

/// A mute/unmute icon button.
///
/// A speaker drawn in vectors rather than an emoji glyph: an emoji's weight,
/// optical size and even existence depend on which font the system happens to
/// hand over, and a row of them next to themed controls reads as borrowed from
/// somewhere else. The drawn glyph picks up the palette like everything else —
/// `text_weak` when sound is flowing, `danger` with a slash when muted, so the
/// state survives a glance as well as a colour-blind user's read.
///
/// The interaction mirrors [`toggle`]: the id has to be stable across frames
/// and unique per row, the state is flipped here and the caller reads
/// `changed()`, and Space/Enter do what a click would so the control is not a
/// keyboard dead end.
pub fn mute_button(
    ui: &mut Ui,
    palette: &Palette,
    id: egui::Id,
    muted: &mut bool,
    label: &str,
) -> Response {
    let size = Vec2::new(26.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());

    if response.clicked() {
        *muted = !*muted;
        response.mark_changed();
    }
    if response.has_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter))
    {
        *muted = !*muted;
        response.mark_changed();
    }

    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), *muted, label)
    });

    if ui.is_rect_visible(rect) {
        let fill = if response.is_pointer_button_down_on() {
            Some(palette.control_active)
        } else if response.hovered() {
            Some(palette.control_hover)
        } else {
            None
        };
        if let Some(fill) = fill {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(radius::CONTROL), fill);
        }

        // The state change animates: the waves fade out as the slash fades in,
        // so a click reads as a transition rather than a swap.
        let t = ui
            .ctx()
            .animate_value_with_time(id, if *muted { 1.0 } else { 0.0 }, 0.10);
        let ink = if *muted { palette.danger } else { palette.text_weak };
        draw_speaker(ui.painter(), rect.center(), ink, t);
    }

    response.on_hover_text(label.to_owned())
}

/// The speaker glyph: a filled horn, sound waves when unmuted, a slash when
/// not.
///
/// Hand-plotted rather than borrowed from a font for the same reason the
/// window buttons' glyphs are — see [`crate::ui::window_chrome`] on why four
/// shapes are not worth a font. The waves are two arc polylines, which is what
/// egui's line primitives can express without a clip. `muted` is the animated
/// transition, 0 to 1: the waves and the slash crossfade, so the mark is
/// neither one nor the other only while the click is still settling.
fn draw_speaker(painter: &egui::Painter, centre: egui::Pos2, ink: Color32, muted: f32) {
    let (cx, cy) = (centre.x, centre.y);

    // The horn: a box plus the cone flaring off it, as one filled polygon.
    let horn = [
        egui::pos2(cx - 7.0, cy - 2.5),
        egui::pos2(cx - 4.5, cy - 2.5),
        egui::pos2(cx - 1.5, cy - 6.0),
        egui::pos2(cx - 1.5, cy + 6.0),
        egui::pos2(cx - 4.5, cy + 2.5),
        egui::pos2(cx - 7.0, cy + 2.5),
    ];
    painter.add(egui::Shape::convex_polygon(
        horn.to_vec(),
        ink,
        Stroke::NONE,
    ));

    if muted < 1.0 {
        // Two sound waves, each an arc of a circle centred near the horn's
        // mouth, swept from 45° above the horizontal to 45° below.
        let wave_ink = ink.gamma_multiply(0.9 * (1.0 - muted));
        for radius in [3.5, 6.5] {
            let points: Vec<egui::Pos2> = (-45..=45)
                .step_by(15)
                .map(|degrees| {
                    let angle = (degrees as f32).to_radians();
                    egui::pos2(
                        cx - 1.0 + radius * angle.cos(),
                        cy + radius * angle.sin(),
                    )
                })
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(1.5, wave_ink)));
        }
    }

    if muted > 0.0 {
        // The slash, corner to corner of the glyph box: the conventional
        // "this is silenced" mark, and in `danger` it reads without the hue.
        painter.line_segment(
            [egui::pos2(cx - 6.0, cy + 6.0), egui::pos2(cx + 6.0, cy - 6.0)],
            Stroke::new(1.5, ink.gamma_multiply(muted)),
        );
    }
}

/// An empty list's one-line message, centred and quiet.
///
/// "No applications", "no devices", "no rules" — every list in the mixer has
/// one, and drawing them through one helper is what keeps them from drifting
/// into three sizes and three alignments.
pub fn empty_state(ui: &mut Ui, palette: &Palette, text: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(space::XS);
        ui.label(
            RichText::new(text)
                .small()
                .color(palette.text_faint),
        );
        ui.add_space(space::XS);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative luminance, per WCAG 2.1.
    fn luminance(colour: Color32) -> f32 {
        let channel = |raw: u8| {
            let c = raw as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(colour.r()) + 0.7152 * channel(colour.g()) + 0.0722 * channel(colour.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The palette's whole reason for existing is that its text is readable, so
    /// the ratios are asserted rather than eyeballed. A palette edit that
    /// quietly drops a label below AA fails here.
    #[test]
    fn text_clears_the_contrast_it_claims() {
        for (name, p) in [("light", &LIGHT), ("dark", &DARK)] {
            assert!(
                contrast(p.text, p.surface) >= 7.0,
                "{name}: text on surface is only {:.1}:1",
                contrast(p.text, p.surface)
            );
            assert!(
                contrast(p.text, p.bg) >= 7.0,
                "{name}: text on the window is only {:.1}:1",
                contrast(p.text, p.bg)
            );
            assert!(
                contrast(p.text_weak, p.surface) >= 4.5,
                "{name}: weak text on surface is only {:.1}:1",
                contrast(p.text_weak, p.surface)
            );
            assert!(
                contrast(p.text_weak, p.bg) >= 4.5,
                "{name}: weak text on the window is only {:.1}:1",
                contrast(p.text_weak, p.bg)
            );
            // The status colours double as text, so they are held to the text
            // bar and not the 3:1 graphics one.
            for (label, colour) in [
                ("success", p.success),
                ("warning", p.warning),
                ("danger", p.danger),
                ("accent", p.accent),
            ] {
                assert!(
                    contrast(colour, p.surface) >= 4.5,
                    "{name}: {label} on surface is only {:.1}:1",
                    contrast(colour, p.surface)
                );
            }
            assert!(
                contrast(p.on_accent, p.accent) >= 4.5,
                "{name}: text on the accent is only {:.1}:1",
                contrast(p.on_accent, p.accent)
            );
        }
    }

    /// Controls have to be visible against what they sit on, or the panel is a
    /// featureless grey rectangle — which is what the old one looked like.
    #[test]
    fn controls_are_distinguishable_from_their_background() {
        for (name, p) in [("light", &LIGHT), ("dark", &DARK)] {
            assert_ne!(p.control, p.surface, "{name}: the control fill is invisible");
            assert_ne!(p.sunken, p.surface, "{name}: the well fill is invisible");
            assert_ne!(p.border, p.surface, "{name}: card borders are invisible");
            assert_ne!(
                p.control_hover, p.control,
                "{name}: hovering changes nothing"
            );
            // A metre or two of contrast is enough for a border; this catches a
            // typo that makes it the same colour to 8 bits.
            assert!(
                contrast(p.border_strong, p.surface) >= 1.4,
                "{name}: strong borders are only {:.2}:1",
                contrast(p.border_strong, p.surface)
            );
        }
    }

    /// The two themes must actually differ, or "follow the system" is a no-op.
    #[test]
    fn the_themes_are_opposites() {
        // A compile-time check: which palette claims to be dark is a constant,
        // and if someone flips the flag the build should stop, not the test.
        const { assert!(!LIGHT.dark && DARK.dark) };
        assert!(luminance(LIGHT.bg) > luminance(DARK.bg));
        assert!(luminance(LIGHT.text) < luminance(DARK.text));
        // Light text on a light window would be the failure mode of mixing the
        // two palettes up.
        assert!(contrast(LIGHT.text, LIGHT.bg) > contrast(DARK.text, LIGHT.bg));
    }

    /// The meter's colour ramp is a convention, not decoration: it has to reach
    /// the danger hue before full scale and stay there.
    #[test]
    fn the_level_ramp_escalates() {
        for p in [&LIGHT, &DARK] {
            assert_eq!(level_colour(0.0, p), level_colour(0.5, p));
            assert_ne!(level_colour(0.5, p), level_colour(0.8, p));
            assert_ne!(level_colour(0.8, p), level_colour(1.0, p));
            assert_eq!(level_colour(0.95, p), level_colour(1.0, p));
        }
        // And the light ramp has to be darker to be visible on a light trough.
        assert!(luminance(level_colour(0.5, &LIGHT)) < luminance(level_colour(0.5, &DARK)));
    }
}
