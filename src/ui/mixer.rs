//! The mixer and routing window.
//!
//! The second of the two windows FxTrumpet can open. It is separate from the
//! tuning panel rather than a tab on it, for a reason that is mostly about the
//! shape of the thing: the tuning panel is sized so its whole content fits on
//! screen without scrolling, and a tab strip would have pushed both halves past
//! that. It is also what EarTrumpet does — shaping the sound and steering it are
//! different jobs, reached for at different moments.
//!
//! ## What is on it
//!
//! 1. **Applications** — one row per application with a live stream: volume,
//!    mute, level, and *where it is playing*. Changing that last column is what
//!    creates a routing rule, which is where EarTrumpet's session list and Audio
//!    Router's per-application routing turn out to be the same list read two
//!    ways.
//! 2. **Output devices** — every active endpoint, its own volume and mute, and
//!    which one the system default is on.
//! 3. **Routing rules** — everything the application list has accumulated, with
//!    its mechanism and an on/off switch.
//!
//! ## Threading
//!
//! This window renders on the panel thread; the audio engine, the router and the
//! config live on the main (tray) thread. Two consequences shape the module:
//!
//! - **Reads go to Windows directly.** Sessions, endpoints and icons are plain
//!   Core Audio and shell calls with no cross-thread state, so this thread reads
//!   them itself. Polling here rather than asking the main thread keeps the
//!   meter smooth and the main thread's tick cheap.
//! - **Writes are messages.** Anything that changes configuration — a route, the
//!   default device — goes to the main thread as a [`MixerCommand`], because
//!   that is where the router and the config file live.
//!
//! Routing state is read through a [`RouteSnapshot`] the main thread publishes,
//! rather than by sharing the `Router` itself: that object owns a COM factory
//! and a record of what it has already written, neither of which means anything
//! off the thread that owns it.
//!
//! ## Polling, and why the sliders do not fight it
//!
//! Windows reports volume but not *who* changed it. A slider whose value is
//! re-read from the hardware every frame therefore snaps back under the user's
//! finger for the frames between the drag and the write landing. The rule here
//! is the simple one: while the pointer is on a slider the value belongs to the
//! UI, and the poll only writes into rows nobody is touching.
//!
//! ## Why the UI pass is split from the state pass
//!
//! Every row's controls are drawn inside an `egui` closure, and a closure that
//! captured `&mut self` would have to borrow the icon cache, the device list and
//! the rule list all at once while also reading the snapshot — which the borrow
//! checker correctly refuses. So each card is built in two passes: this module's
//! own state is read into plain locals, the closure works only on those, and the
//! results are written back afterwards.

use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::device;
use crate::endpoint::{self, EndpointLevels};
use crate::i18n;
use crate::process::{self, AppKey};
use crate::router::{InjectionStatus, RouteMethod, RouteSnapshot, RouteTarget};
use crate::session::{self, Session, SessionState};
use crate::ui::theme::{self, radius, space, Palette};
use crate::ui::window_chrome;
use crate::ui::window_shape::RoundedWindow;

/// How often the session list, device list and meters are re-read.
///
/// Faster than the main thread's one-second tick because this drives a meter;
/// slow enough that a stationary window is not enumerating sessions at frame
/// rate. `request_repaint_after` uses the same value, so this is the window's
/// actual frame budget rather than a second poll layered on top of one.
const POLL_INTERVAL: Duration = Duration::from_millis(120);

/// Opening size. The content decides its own height, as in the tuning panel.
///
/// Public because the window thread builds this window's `NativeOptions`, and
/// the sizes belong next to the layout they describe rather than in the host —
/// same reason [`crate::ui::panel`] keeps its own.
pub const DEFAULT_WIDTH: f32 = 780.0;
pub const OPENING_HEIGHT: f32 = 640.0;
pub const MIN_HEIGHT: f32 = 380.0;

/// Wide enough for the device column plus the gesture-sized controls beside it;
/// narrower than this and the three per-application controls wrap.
pub const MIN_WIDTH: f32 = 460.0;

/// Icon edge length in an application row. The window icon comes from the
/// window thread, which builds one for both windows.
const ROW_ICON: f32 = 20.0;

/// Something the user asked the main thread to do.
#[derive(Debug, Clone, PartialEq)]
pub enum MixerCommand {
    /// Point one application at a destination, creating or replacing its rule.
    SetAppTarget {
        app: AppKey,
        display_name: String,
        target: RouteTarget,
    },
    /// Set the volume of every stream an application owns.
    ///
    /// Passed as the full stream list rather than as an application key so the
    /// main thread does not have to re-enumerate sessions to act on it: the
    /// window has just read that list, and a second read could disagree with it.
    SetAppVolume {
        streams: Vec<(String, String)>,
        level: f32,
    },
    /// Mute or unmute every stream an application owns.
    SetAppMute {
        streams: Vec<(String, String)>,
        muted: bool,
    },
    /// Turn a rule off or on without forgetting it.
    SetRuleEnabled { app: AppKey, enabled: bool },
    /// Forget a rule and hand the application back to the system default.
    RemoveRule { app: AppKey },
    /// Change the system's default output device.
    SetDefaultDevice { endpoint_id: String },
    /// Change a device's own volume.
    SetDeviceVolume { endpoint_id: String, level: f32 },
    /// Mute or unmute a device.
    SetDeviceMuted { endpoint_id: String, muted: bool },
}

/// What the mixer is given when it opens.
pub struct MixerShared {
    /// Read-only routing state, republished by the main thread each tick.
    pub routes: Arc<Mutex<RouteSnapshot>>,
    /// The channel back to the main thread.
    pub commands: mpsc::Sender<MixerCommand>,
}

impl MixerShared {
    /// Everything the mixer needs, from the main thread's own copies.
    ///
    /// Both windows take their shared state through a constructor like this one
    /// so the two entry points — the tray menu and `--mixer` — cannot disagree
    /// about what the window is handed.
    pub fn new(
        routes: Arc<Mutex<RouteSnapshot>>,
        commands: mpsc::Sender<MixerCommand>,
    ) -> Self {
        Self { routes, commands }
    }
}

/// One application row, as plain data.
///
/// Deliberately carries no `&self` and no handle: it is what the UI closure is
/// allowed to see. The icon is uploaded before the closure runs and passed in as
/// a texture handle, because uploading needs `&mut self` and a `Context`.
struct AppRow {
    key: AppKey,
    display_name: String,
    /// The process behind the row, used once to fetch its icon.
    process_id: u32,
    /// Every stream the application owns, as `(device id, instance id)`.
    streams: Vec<(String, String)>,
    /// The endpoint its audio is arriving at, as observed rather than as
    /// requested. See [`crate::perapp`] for why those are different questions.
    playing_on: String,
    volume: f32,
    muted: bool,
    peak: f32,
}

/// One device row, as plain data.
struct DeviceRow {
    id: String,
    display: String,
    is_default: bool,
    /// Whether this is the enhancer's own virtual card.
    is_virtual: bool,
    levels: EndpointLevels,
}

/// One rule row, as plain data.
struct RuleRow {
    app: AppKey,
    display_name: String,
    destination: String,
    method: RouteMethod,
    enabled: bool,
}

/// An application's destination, resolved to a label plus the raw target.
struct Destination {
    label: String,
    target: RouteTarget,
}

/// The mixer window.
struct MixerApp {
    shared: MixerShared,
    devices: Vec<DeviceRow>,
    apps: Vec<AppRow>,
    rules: Vec<RuleRow>,
    /// Decoded icons, keyed by application. Uploaded once each.
    icons: HashMap<AppKey, Option<egui::TextureHandle>>,
    /// Routing state as last published by the main thread.
    snapshot: RouteSnapshot,
    /// Whether a slider is currently held, which suspends the poll's writes into
    /// the values it would otherwise overwrite.
    pointer_busy: bool,
    last_poll: Instant,
    shape: RoundedWindow,
    fitted: bool,
    /// A one-line note under the footer.
    note: Option<String>,
}

impl MixerApp {
    fn new(shared: MixerShared) -> Self {
        let snapshot = read_snapshot(&shared);
        Self {
            shared,
            devices: Vec::new(),
            apps: Vec::new(),
            rules: Vec::new(),
            icons: HashMap::new(),
            snapshot,
            pointer_busy: false,
            // Far enough in the past that the first frame polls.
            last_poll: Instant::now() - POLL_INTERVAL,
            shape: RoundedWindow::new(),
            fitted: false,
            note: None,
        }
    }

    /// Re-reads the machine. Runs on the panel thread, needs COM on it.
    fn poll(&mut self) {
        self.last_poll = Instant::now();
        self.snapshot = read_snapshot(&self.shared);

        let live = match device::render_devices() {
            Ok(devices) => devices,
            Err(err) => {
                log::debug!("the mixer could not enumerate devices: {err}");
                Vec::new()
            }
        };

        // The slider positions are taken from the hardware only when nobody is
        // holding one — see the module docs.
        let previous: HashMap<String, f32> = self
            .devices
            .iter()
            .map(|row| (row.id.clone(), row.levels.volume))
            .collect();

        self.devices = live
            .into_iter()
            .map(|info| {
                // Read the display name before `info.id` is moved out: the two
                // come from the same struct, so taking the id first would leave
                // the name borrowed from a partially moved value.
                let display = info.display();
                let mut levels = endpoint::levels(&info.id).unwrap_or(EndpointLevels {
                    volume: 1.0,
                    muted: false,
                    peak: 0.0,
                });
                if self.pointer_busy {
                    if let Some(held) = previous.get(&info.id) {
                        levels.volume = *held;
                    }
                }
                DeviceRow {
                    id: info.id,
                    display,
                    is_default: info.is_default,
                    is_virtual: info.is_virtual,
                    levels,
                }
            })
            .collect();

        if self.pointer_busy {
            // Nothing else is re-read mid-drag: the session grouping would
            // rebuild the rows and drop the dragged slider out from under the
            // pointer.
            return;
        }

        let sessions = session::all_sessions();
        self.apps = group_by_application(&sessions);
        process::forget_exited(&sessions.iter().map(|s| s.process_id).collect::<Vec<_>>());

        // Device names for the rule list, which stores raw endpoint ids.
        let names: HashMap<&str, &str> = self
            .devices
            .iter()
            .map(|row| (row.id.as_str(), row.display.as_str()))
            .collect();

        self.rules = self
            .snapshot
            .rules
            .iter()
            .map(|rule| RuleRow {
                app: rule.app.clone(),
                display_name: rule.display_name.clone(),
                destination: match &rule.target {
                    RouteTarget::SystemDefault => i18n::t().mixer.follow_system.to_owned(),
                    RouteTarget::Device { endpoint_id } => names
                        .get(endpoint_id.as_str())
                        .map(|name| (*name).to_owned())
                        .unwrap_or_else(|| endpoint_id.clone()),
                },
                method: rule.method,
                enabled: rule.enabled,
            })
            .collect();
    }

    /// Uploads any icon a row needs but the cache does not have.
    ///
    /// Separate from the UI pass because it needs `&mut self` and a `Context`,
    /// and the UI pass is not allowed either.
    fn prepare_icons(&mut self, ctx: &egui::Context) {
        for row in &self.apps {
            if self.icons.contains_key(&row.key) {
                continue;
            }

            let texture = process::identify(row.process_id)
                .as_ref()
                .and_then(process::icon_for)
                .map(|pixels| {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [pixels.width as usize, pixels.height as usize],
                        &pixels.rgba,
                    );
                    ctx.load_texture(
                        format!("fxtrumpet-icon-{}", row.key.to_storage()),
                        image,
                        egui::TextureOptions::LINEAR,
                    )
                });

            self.icons.insert(row.key.clone(), texture);
        }
    }

    /// The destination of every application, resolved against the snapshot.
    fn destinations(&self) -> Vec<(AppKey, Destination)> {
        self.apps
            .iter()
            .map(|row| {
                let target = self.snapshot.target_for(&row.key);
                let label = match &target {
                    RouteTarget::SystemDefault => i18n::t().mixer.follow_system.to_owned(),
                    RouteTarget::Device { endpoint_id } => self
                        .devices
                        .iter()
                        .find(|device| &device.id == endpoint_id)
                        .map(|device| device.display.clone())
                        .unwrap_or_else(|| endpoint_id.clone()),
                };
                (
                    row.key.clone(),
                    Destination { label, target },
                )
            })
            .collect()
    }

    fn send(&mut self, command: MixerCommand) {
        if self.shared.commands.send(command).is_err() {
            // The main thread is gone, which means the process is shutting down.
            self.note = Some("the application is shutting down".to_owned());
        }
    }

    /// Resizes the window to fit its content, once.
    ///
    /// Once only, as in the tuning panel: after that the size belongs to whoever
    /// dragged the frame.
    fn fit_to_content(&mut self, ctx: &egui::Context, content: egui::Vec2, chrome: f32) {
        if self.fitted {
            return;
        }
        self.fitted = true;

        let monitor = ctx
            .input(|input| input.viewport().monitor_size)
            .map(|size| size.y);
        let wanted = (content.y + chrome).max(MIN_HEIGHT);
        let height = match monitor {
            Some(available) => wanted.min(available - 80.0).max(MIN_HEIGHT),
            None => wanted,
        };

        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            DEFAULT_WIDTH,
            height,
        )));
    }
}

/// A horizontal level bar, drawn rather than widgetised.
///
/// egui has no meter, and a progress bar carries the wrong affordance — it looks
/// draggable.
fn meter(ui: &mut egui::Ui, palette: &Palette, value: f32, width: f32) {
    let height = 4.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(2), palette.sunken);
    let value = value.clamp(0.0, 1.0);
    if value > 0.0 {
        let filled = egui::Rect::from_min_size(rect.min, egui::vec2(width * value, height));
        painter.rect_filled(
            filled,
            egui::CornerRadius::same(2),
            theme::level_colour(value, palette),
        );
    }
}

/// Reads the published routing snapshot, falling back to an unavailable one.
fn read_snapshot(shared: &MixerShared) -> RouteSnapshot {
    match shared.routes.lock() {
        Ok(snapshot) => snapshot.clone(),
        Err(_) => RouteSnapshot {
            rules: Vec::new(),
            policy_available: false,
            injection: InjectionStatus::NotAvailable,
        },
    }
}

/// Groups sessions into one row per application.
///
/// The unit of this list is the application, not the stream, and that is not
/// only EarTrumpet's model: a routing rule is per application, so a list keyed
/// on anything finer would be a list whose rows the user could not create a rule
/// from.
fn group_by_application(sessions: &[Session]) -> Vec<AppRow> {
    // Insertion order is preserved so rows do not shuffle themselves between
    // polls — a `HashMap` alone would make the list jump while the user is
    // aiming at it.
    let mut order: Vec<AppKey> = Vec::new();
    let mut groups: HashMap<AppKey, AppRow> = HashMap::new();

    for session in sessions {
        if session.state == SessionState::Expired {
            continue;
        }

        let (key, display_name, process_id) = if session.is_system_sounds
            || session.process_id == 0
        {
            (
                AppKey::SystemSounds,
                i18n::t().mixer.system_sounds.to_owned(),
                0,
            )
        } else {
            match process::identify(session.process_id) {
                Some(identity) => (
                    identity.key(),
                    identity.display_name,
                    session.process_id,
                ),
                // A session whose process cannot be identified is skipped rather
                // than shown as a nameless row: it is either ending or beyond
                // our reach, and either way it is not routable.
                None => continue,
            }
        };

        let entry = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            AppRow {
                key,
                display_name,
                process_id,
                streams: Vec::new(),
                playing_on: session.device_id.clone(),
                volume: session.volume,
                muted: session.muted,
                peak: 0.0,
            }
        });

        entry
            .streams
            .push((session.device_id.clone(), session.instance_id.clone()));
        entry.peak = entry.peak.max(session.peak);
        // The volume shown is the loudest stream, not the first: with two
        // streams at 0.2 and 1.0, showing 0.2 and then applying it to every
        // stream would quieten an application that was fine.
        if session.volume > entry.volume {
            entry.volume = session.volume;
            entry.muted = session.muted;
            entry.playing_on = session.device_id.clone();
        }
    }

    let mut out: Vec<AppRow> = order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .collect();
    out.sort_by(|a, b| {
        // Silent applications last, then alphabetically: the row a user wants is
        // almost always one that is currently playing.
        let silent = |row: &AppRow| row.peak <= 0.0001;
        silent(a)
            .cmp(&silent(b))
            .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
    });
    out
}

impl eframe::App for MixerApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // The tuning panel was asked for while this mixer is up. See the panel's
        // own call site for why closing is the switch.
        crate::ui::panel::step_aside_if_superseded(&ctx, crate::ui::panel::WindowKind::Mixer);

        let maximized = window_chrome::is_maximized(&ctx);

        // Before anything is laid out, so the frame about to be presented is
        // already clipped. The tuning panel documents why at length.
        self.shape.apply(
            frame,
            ctx.pixels_per_point(),
            radius::CARD as f32,
            maximized,
        );

        if self.last_poll.elapsed() >= POLL_INTERVAL {
            self.poll();
        }
        self.prepare_icons(&ctx);

        let palette = theme::palette(&ctx);
        let text = &i18n::t().mixer;

        let caption_height = egui::Panel::top("caption")
            .exact_size(window_chrome::CAPTION_HEIGHT)
            .show_separator_line(false)
            .frame(egui::Frame::default().fill(palette.surface))
            .show(ui, |ui| window_chrome::caption(ui, palette))
            .response
            .rect
            .height();

        let header_height = egui::Panel::top("header")
            .show_separator_line(false)
            .frame(
                egui::Frame::default()
                    .fill(palette.surface)
                    .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(text.title)
                            .size(15.0)
                            .strong()
                            .color(palette.text),
                    );
                    if !self.snapshot.policy_available {
                        theme::status_pill(
                            ui,
                            palette,
                            palette.warning,
                            text.policy_unavailable,
                        );
                    }
                });
            })
            .response
            .rect
            .height();

        let footer_height = egui::Panel::bottom("footer")
            .show_separator_line(false)
            .frame(
                egui::Frame::default()
                    .fill(palette.surface)
                    .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8)),
            )
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(text.routing_hint)
                        .size(11.0)
                        .color(palette.text_weak),
                );
                if let Some(note) = &self.note {
                    ui.label(egui::RichText::new(note).size(11.0).color(palette.warning));
                }
            })
            .response
            .rect
            .height();

        let mut commands: Vec<MixerCommand> = Vec::new();

        let content = egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(palette.bg)
                    .inner_margin(egui::Margin::same(space::M as i8)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.apps_card(ui, palette, &mut commands);
                        ui.add_space(space::S);
                        self.devices_card(ui, palette, &mut commands);
                        ui.add_space(space::S);
                        self.rules_card(ui, palette, &mut commands);
                    })
            })
            .inner;

        self.fit_to_content(
            &ctx,
            content.content_size,
            caption_height + header_height + footer_height,
        );

        window_chrome::resize_border(&ctx, maximized);
        window_chrome::outline(&ctx, palette, maximized);

        for command in commands {
            self.send(command);
        }

        ctx.request_repaint_after(POLL_INTERVAL);
    }
}

/// What the application row's controls did this frame.
#[derive(Default)]
struct AppEdits {
    /// `(row index, new volume)` for a slider that moved.
    volumes: Vec<(usize, f32)>,
    /// `(row index, new muted state)`.
    mutes: Vec<(usize, bool)>,
    pointer_busy: bool,
}

impl MixerApp {
    /// Applications, with their volume, level and destination.
    fn apps_card(
        &mut self,
        ui: &mut egui::Ui,
        palette: &Palette,
        commands: &mut Vec<MixerCommand>,
    ) {
        let text = &i18n::t().mixer;

        // Pass one: everything the closure needs, copied out. See the module
        // docs for why this is not done inline.
        //
        // `destinations` is read once and zipped with the rows rather than
        // looked up per row: `apps` and `destinations` are both keyed off
        // `self.apps` in order, so the pairing is positional and a second
        // `target_for` per row would be the same answer computed twice.
        let destinations = self.destinations();
        let rows: Vec<RowView> = self
            .apps
            .iter()
            .zip(destinations.iter())
            .map(|(row, (_, destination))| RowView {
                key: row.key.clone(),
                display_name: row.display_name.clone(),
                streams: row.streams.len(),
                volume: row.volume,
                muted: row.muted,
                peak: row.peak,
                icon: self.icons.get(&row.key).cloned().flatten(),
                destination: destination.label.clone(),
            })
            .collect();
        let devices: Vec<(String, String)> = self
            .devices
            .iter()
            .map(|row| (row.id.clone(), row.display.clone()))
            .collect();

        let mut edits = AppEdits::default();

        theme::card(ui, palette, |ui| {
            theme::card_title(ui, palette, text.apps);

            if rows.is_empty() {
                ui.label(
                    egui::RichText::new(text.no_apps)
                        .size(11.0)
                        .color(palette.text_faint),
                );
                return;
            }

            for (index, row) in rows.iter().enumerate() {
                let target = &destinations[index].1.target;
                let bypasses = target.bypasses_enhancement();

                ui.horizontal(|ui| {
                    // Icon, or a placeholder of the same width so the names line
                    // up whether or not an application has one.
                    match &row.icon {
                        Some(texture) => {
                            ui.add(
                                egui::Image::new(texture)
                                    .fit_to_exact_size(egui::vec2(ROW_ICON, ROW_ICON)),
                            );
                        }
                        None => {
                            ui.add_space(ROW_ICON);
                        }
                    }

                    // Name, with the stream count and the bypass warning under
                    // it. Two lines rather than one long one: "this application
                    // is not being enhanced" is the single most confusing thing
                    // about a routed application, and it has to be attached to
                    // the name — that is where the eye already is.
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(egui::RichText::new(&row.display_name).size(12.0).color(palette.text));
                        let mut second = text.streams(row.streams);
                        if bypasses {
                            // `streams` is empty for a single stream, so the
                            // separator is only added when there is something to
                            // separate — otherwise the line opens with a stray
                            // " · ".
                            if !second.is_empty() {
                                second.push_str(" · ");
                            }
                            second.push_str(text.bypasses);
                        }
                        if !second.is_empty() {
                            ui.label(egui::RichText::new(second).size(10.0).color(
                                if bypasses { palette.warning } else { palette.text_faint },
                            ));
                        }
                    });

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            // Destination. Rightmost, because it is the column
                            // that makes this more than a volume mixer.
                            egui::ComboBox::from_id_salt(format!(
                                "route-{}",
                                row.key.to_storage()
                            ))
                            .selected_text(row.destination.clone())
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        *target == RouteTarget::SystemDefault,
                                        text.follow_system,
                                    )
                                    .clicked()
                                {
                                    commands.push(MixerCommand::SetAppTarget {
                                        app: row.key.clone(),
                                        display_name: row.display_name.clone(),
                                        target: RouteTarget::SystemDefault,
                                    });
                                }
                                for (endpoint_id, display) in &devices {
                                    let selected = matches!(
                                        target,
                                        RouteTarget::Device { endpoint_id: id }
                                            if id == endpoint_id
                                    );
                                    if ui.selectable_label(selected, display).clicked() {
                                        commands.push(MixerCommand::SetAppTarget {
                                            app: row.key.clone(),
                                            display_name: row.display_name.clone(),
                                            target: RouteTarget::Device {
                                                endpoint_id: endpoint_id.clone(),
                                            },
                                        });
                                    }
                                }
                            });

                            if ui
                                .selectable_label(
                                    row.muted,
                                    if row.muted { "🔇" } else { "🔊" },
                                )
                                .clicked()
                            {
                                edits.mutes.push((index, !row.muted));
                            }

                            let mut level = row.volume;
                            let response = ui.add(
                                egui::Slider::new(&mut level, 0.0..=1.0)
                                    .show_value(false)
                                    .trailing_fill(true),
                            );
                            if response.is_pointer_button_down_on() {
                                edits.pointer_busy = true;
                            }
                            if response.changed() {
                                edits.volumes.push((index, level));
                            }

                            meter(ui, palette, row.peak, 48.0);
                        },
                    );
                });
                ui.add_space(space::XS);
            }
        });

        // Pass two: write the edits back and turn them into commands.
        self.pointer_busy = edits.pointer_busy;
        for (index, level) in edits.volumes {
            if let Some(row) = self.apps.get_mut(index) {
                row.volume = level;
                commands.push(MixerCommand::SetAppVolume {
                    streams: row.streams.clone(),
                    level,
                });
            }
        }
        for (index, muted) in edits.mutes {
            if let Some(row) = self.apps.get_mut(index) {
                row.muted = muted;
                commands.push(MixerCommand::SetAppMute {
                    streams: row.streams.clone(),
                    muted,
                });
            }
        }
    }

    /// Output devices: the system default, and each device's own volume.
    fn devices_card(
        &mut self,
        ui: &mut egui::Ui,
        palette: &Palette,
        commands: &mut Vec<MixerCommand>,
    ) {
        let text = &i18n::t().mixer;

        let rows: Vec<DeviceView> = self
            .devices
            .iter()
            .map(|row| DeviceView {
                display: row.display.clone(),
                is_default: row.is_default,
                is_virtual: row.is_virtual,
                volume: row.levels.volume,
                muted: row.levels.muted,
                peak: row.levels.peak,
            })
            .collect();

        let mut volumes: Vec<(usize, f32)> = Vec::new();
        let mut mutes: Vec<(usize, bool)> = Vec::new();
        let mut defaults: Vec<usize> = Vec::new();
        let mut pointer_busy = false;

        theme::card(ui, palette, |ui| {
            theme::card_title(ui, palette, text.devices);

            if rows.is_empty() {
                ui.label(
                    egui::RichText::new(text.no_devices)
                        .size(11.0)
                        .color(palette.text_faint),
                );
                return;
            }

            for (index, row) in rows.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&row.display).size(12.0).color(
                        if row.is_default { palette.text } else { palette.text_weak },
                    ));
                    if row.is_default {
                        theme::status_pill(ui, palette, palette.success, text.is_default);
                    }
                    if row.is_virtual {
                        theme::status_pill(ui, palette, palette.accent, text.virtual_card);
                    }

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui.button(text.set_default).clicked() {
                                defaults.push(index);
                            }
                            if ui
                                .selectable_label(
                                    row.muted,
                                    if row.muted { "🔇" } else { "🔊" },
                                )
                                .clicked()
                            {
                                mutes.push((index, !row.muted));
                            }

                            let mut level = row.volume;
                            let response = ui.add(
                                egui::Slider::new(&mut level, 0.0..=1.0)
                                    .show_value(false)
                                    .trailing_fill(true),
                            );
                            if response.is_pointer_button_down_on() {
                                pointer_busy = true;
                            }
                            if response.changed() {
                                volumes.push((index, level));
                            }

                            meter(ui, palette, row.peak, 40.0);
                        },
                    );
                });
                ui.add_space(space::XS);
            }
        });

        self.pointer_busy |= pointer_busy;
        for (index, level) in volumes {
            if let Some(row) = self.devices.get_mut(index) {
                row.levels.volume = level;
                commands.push(MixerCommand::SetDeviceVolume {
                    endpoint_id: row.id.clone(),
                    level,
                });
            }
        }
        for (index, muted) in mutes {
            if let Some(row) = self.devices.get_mut(index) {
                row.levels.muted = muted;
                commands.push(MixerCommand::SetDeviceMuted {
                    endpoint_id: row.id.clone(),
                    muted,
                });
            }
        }
        for index in defaults {
            if let Some(row) = self.devices.get(index) {
                commands.push(MixerCommand::SetDefaultDevice {
                    endpoint_id: row.id.clone(),
                });
            }
        }
    }

    /// The accumulated routing rules.
    fn rules_card(
        &mut self,
        ui: &mut egui::Ui,
        palette: &Palette,
        commands: &mut Vec<MixerCommand>,
    ) {
        let text = &i18n::t().mixer;

        let rows: Vec<RuleView> = self
            .rules
            .iter()
            .map(|row| RuleView {
                display_name: row.display_name.clone(),
                destination: row.destination.clone(),
                method: row.method,
                enabled: row.enabled,
            })
            .collect();

        let mut toggles: Vec<(usize, bool)> = Vec::new();
        let mut removals: Vec<usize> = Vec::new();

        theme::card(ui, palette, |ui| {
            theme::card_title(ui, palette, text.routing);

            if rows.is_empty() {
                ui.label(
                    egui::RichText::new(text.no_routes)
                        .size(11.0)
                        .color(palette.text_faint),
                );
                return;
            }

            for (index, row) in rows.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&row.display_name)
                            .size(12.0)
                            .color(palette.text),
                    );
                    ui.label(
                        egui::RichText::new(format!("→ {}", row.destination))
                            .size(11.0)
                            .color(palette.text_weak),
                    );
                    theme::status_pill(
                        ui,
                        palette,
                        if row.method == RouteMethod::Injection {
                            palette.warning
                        } else {
                            palette.text_faint
                        },
                        text.method_label(row.method),
                    );

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui.button(text.remove).clicked() {
                                removals.push(index);
                            }
                            let mut enabled = row.enabled;
                            if ui
                                .add(egui::Checkbox::without_text(&mut enabled))
                                .changed()
                            {
                                toggles.push((index, enabled));
                            }
                        },
                    );
                });

                // An injection rule that nothing can enforce says so. Silent
                // non-enforcement is the failure mode this status exists to
                // avoid.
                if row.method == RouteMethod::Injection {
                    ui.label(
                        egui::RichText::new(text.injection_unavailable)
                            .size(10.0)
                            .color(palette.warning),
                    );
                }
                ui.add_space(space::XS);
            }
        });

        for (index, enabled) in toggles {
            if let Some(row) = self.rules.get(index) {
                commands.push(MixerCommand::SetRuleEnabled {
                    app: row.app.clone(),
                    enabled,
                });
            }
        }
        for index in removals {
            if let Some(row) = self.rules.get(index) {
                commands.push(MixerCommand::RemoveRule { app: row.app.clone() });
            }
        }
    }
}

/// An application row, as the UI closure sees it.
struct RowView {
    key: AppKey,
    display_name: String,
    streams: usize,
    volume: f32,
    muted: bool,
    peak: f32,
    icon: Option<egui::TextureHandle>,
    destination: String,
}

/// A device row, as the UI closure sees it.
///
/// No endpoint id: the closure identifies a row by its index, and the id is
/// read back off `self.devices` when the command is built. Carrying it here as
/// well would be a second copy that could disagree with the first.
struct DeviceView {
    display: String,
    is_default: bool,
    is_virtual: bool,
    volume: f32,
    muted: bool,
    peak: f32,
}

/// A rule row, as the UI closure sees it.
///
/// No application key, for the same reason [`DeviceView`] carries no id.
struct RuleView {
    display_name: String,
    destination: String,
    method: RouteMethod,
    enabled: bool,
}

/// Builds the mixer's `eframe` application.
///
/// Called from the window thread, which is the only thread that can host an
/// `eframe` window: winit permits one event loop per process.
///
/// The theme and the CJK face are *not* installed here. They are installed once
/// by the window thread before either window is constructed, and doing it a
/// second time would rebuild a font atlas that is already correct.
pub fn build_app(shared: MixerShared) -> Box<dyn eframe::App> {
    Box::new(MixerApp::new(shared))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(process_id: u32, device: &str, instance: &str, volume: f32, peak: f32) -> Session {
        Session {
            device_id: device.to_owned(),
            instance_id: instance.to_owned(),
            display_id: instance.to_owned(),
            process_id,
            is_system_sounds: false,
            state: SessionState::Active,
            volume,
            muted: false,
            peak,
        }
    }

    #[test]
    fn the_system_sounds_session_gets_its_own_row() {
        let mut sounds = session(0, "dev", "sys", 1.0, 0.0);
        sounds.is_system_sounds = true;

        let rows = group_by_application(&[sounds]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, AppKey::SystemSounds);
    }

    #[test]
    fn expired_sessions_are_dropped_before_grouping() {
        let mut stale = session(1, "dev", "inst", 1.0, 0.0);
        stale.state = SessionState::Expired;
        assert!(group_by_application(&[stale]).is_empty());
    }

    #[test]
    fn an_unidentifiable_process_is_left_out_rather_than_shown_unnamed() {
        // Pid 0xFFFFFFF0 cannot exist, so `identify` returns `None` and the row
        // is skipped instead of appearing as "PID ...".
        assert!(group_by_application(&[session(0xFFFF_FFF0, "dev", "x", 1.0, 0.0)]).is_empty());
    }

    #[test]
    fn a_destination_is_the_system_default_until_a_rule_says_otherwise() {
        let snapshot = RouteSnapshot {
            rules: Vec::new(),
            policy_available: true,
            injection: InjectionStatus::NotAvailable,
        };
        let app = AppKey::Executable(std::path::PathBuf::from(r"C:\Apps\Music.exe"));
        assert_eq!(snapshot.target_for(&app), RouteTarget::SystemDefault);
    }

    #[test]
    fn sounding_applications_sort_before_silent_ones() {
        // The ordering is by measurement, not by name: a user opening the mixer
        // wants the thing making noise at the top.
        let mut quiet = AppRow {
            key: AppKey::Packaged("a!App".into()),
            display_name: "Aaa".into(),
            process_id: 1,
            streams: Vec::new(),
            playing_on: "d".into(),
            volume: 1.0,
            muted: false,
            peak: 0.0,
        };
        let mut loud = AppRow {
            key: AppKey::Packaged("b!App".into()),
            display_name: "Zzz".into(),
            process_id: 2,
            streams: Vec::new(),
            playing_on: "d".into(),
            volume: 1.0,
            muted: false,
            peak: 0.5,
        };
        quiet.peak = 0.0;
        loud.peak = 0.5;

        let silent = |row: &AppRow| row.peak <= 0.0001;
        let mut rows = vec![quiet, loud];
        rows.sort_by(|a, b| {
            silent(a)
                .cmp(&silent(b))
                .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
        });
        assert_eq!(rows[0].display_name, "Zzz");
    }
}
