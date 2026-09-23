//! Application wiring: the tray, the engine, and the state that connects them.
//!
//! Keeping this separate from `main` means the event-loop callback stays a
//! one-liner and the behaviour is testable without a message pump.

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::device;
use crate::driver;
use crate::endpoint;
use crate::engine::{AudioEngine, EngineHandle, EngineStatus, SharedParams};
use crate::i18n::{self, Lang};
use crate::preset::{self, PresetEntry};
use crate::router::{RouteRule, RouteSnapshot, Router};
use crate::routing::{EngageOutcome, Routing};
use crate::session;
use crate::ui::mixer::{MixerCommand, MixerShared};
use crate::ui::panel::{self, PanelShared, WindowKind};
use crate::ui::tray::{Tray, TrayAction};

/// How often the tooltip and the driver menu are refreshed.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// The running application.
pub struct App {
    config: Config,
    engine: AudioEngine,
    handle: EngineHandle,
    tray: Tray,
    presets: Vec<PresetEntry>,
    active_preset: Option<PathBuf>,
    /// Owns the difference between the machine's output routing and the one
    /// FxTrumpet needs. Releasing it on exit is what stops the app from leaving a
    /// silent machine behind.
    routing: Routing,
    /// Whether a takeover has already been attempted for the virtual card's
    /// current appearance. Cleared when the card disappears, so its return is a
    /// fresh reason to try.
    routing_attempted: bool,
    last_refresh: Instant,
    /// The preset-folder revision this app has already reacted to.
    ///
    /// The panel saves presets by asking the engine to write the file, which
    /// happens on the audio thread, so the tray cannot learn about it from its
    /// own call paths. It compares this against
    /// [`EngineStatus::presets_revision`] once a second instead.
    seen_presets_revision: u64,

    /// Per-application output routing: who plays where.
    ///
    /// Lives here rather than in the mixer window because it owns a policy
    /// factory and a record of what has been written to Windows, and neither of
    /// those means anything off this thread. The window only ever sees
    /// [`RouteSnapshot`].
    router: Router,
    /// The router's state as the window reads it.
    ///
    /// Behind a mutex rather than an atomic because it is a list, and shared by
    /// `Arc` rather than re-sent because the window keeps its handle across
    /// frames. Written once a second and on every edit; read at 8 Hz.
    route_snapshot: Arc<Mutex<RouteSnapshot>>,
    /// What the mixer asked for, drained at the top of every tick.
    mixer_rx: mpsc::Receiver<MixerCommand>,
    /// The matching sender, cloned into each window the mixer opens.
    mixer_tx: mpsc::Sender<MixerCommand>,
}

impl App {
    /// Prepares everything and puts the tray icon on screen.
    pub fn start(mut config: Config) -> Result<Self, String> {
        // Before the tray is built, because every one of its labels is drawn
        // from the string table and there is no way to relabel an existing
        // menu item — see `ui::tray`.
        let language = i18n::resolve(&config.language);
        i18n::set(language);
        log::info!(
            "interface language: {} ({})",
            language.endonym(),
            config.language
        );

        // Bundled presets are unpacked before anything reads the folder, so the
        // first run already has a populated menu.
        if let Err(err) = preset::unpack_embedded_presets() {
            log::warn!("could not unpack the bundled presets: {err}");
        }

        let presets = preset::library();
        log::info!(
            "{} preset(s) available ({} embedded)",
            presets.len(),
            preset::embedded_count()
        );

        // Pick the preset to start with: the remembered one, else Music if it
        // survived, else whatever sorted first.
        let active = config
            .active_preset
            .as_ref()
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .or_else(|| {
                presets
                    .iter()
                    .find(|entry| entry.name.contains("音乐") || entry.name.contains("Music"))
                    .map(|entry| entry.path.clone())
            })
            .or_else(|| presets.first().map(|entry| entry.path.clone()));

        // Routing comes before the engine, in that order, because the engine's
        // choice of render endpoint depends on knowing which physical device
        // the takeover displaced. Doing it the other way round makes the first
        // graph render to an arbitrary device and then rebuild.
        let mut routing = Routing::new();
        Routing::recover(config.previous_default_id.as_deref());
        if config.take_over_default {
            match routing.engage() {
                EngageOutcome::Engaged { .. } | EngageOutcome::AlreadyRouted => {}
                EngageOutcome::NoVirtualCard => log::info!(
                    "not switching the default output: FxSound's virtual sound card is not active"
                ),
                EngageOutcome::Failed(err) => {
                    log::error!("could not route audio through FxTrumpet: {err}");
                }
            }
        } else {
            log::info!("take_over_default is off; leaving the system output alone");
        }
        // Re-recorded every start so a crash always leaves a usable marker, and
        // blanked when there is nothing to restore.
        config.previous_default_id = routing.previous().map(str::to_owned);
        // Written out here rather than left to the next unrelated save.
        // This file is the *only* record of where the output came from, so it
        // has to hit the disk before the process can be killed while holding
        // the default device — which is exactly what a crash is.
        if let Err(err) = config.save() {
            log::warn!("could not save the crash-recovery marker: {err}");
        }

        let engine = match AudioEngine::start(config.clone()) {
            Ok(engine) => engine,
            Err(err) => {
                routing.release();
                return Err(format!("could not start the audio engine: {err}"));
            }
        };
        let handle = engine.handle();

        // Push the remembered on/off state before anything reads it.
        handle.params().set_enabled(config.enabled);

        // Per-application routing. Built here rather than lazily on the first
        // mixer open so a machine that cannot do per-app endpoints says so in
        // the log at startup, and so the first poll is already enforcing the
        // rules the user left behind.
        let (mixer_tx, mixer_rx) = mpsc::channel();
        let router = Router::new(config.routes.clone());
        if !router.rules().is_empty() {
            log::info!(
                "{} per-application routing rule(s) loaded (policy {})",
                router.rules().len(),
                if router.policy_available() {
                    "available"
                } else {
                    "unavailable"
                }
            );
        }
        let route_snapshot = Arc::new(Mutex::new(router.snapshot()));

        let driver_status = driver::status();
        log::info!("driver: {}", driver_status.summary());
        if !driver_status.endpoint_active {
            log::warn!(
                "FxSound's virtual sound card was not found. FxTrumpet will pass audio through \
                 unprocessed until it is installed (tray menu: install virtual sound card)."
            );
        }

        let tray = match Tray::new(
            &presets,
            config.enabled,
            crate::autostart::is_enabled(),
            driver_status.endpoint_active,
        ) {
            Ok(tray) => tray,
            Err(err) => {
                routing.release();
                return Err(err);
            }
        };

        let mut app = Self {
            engine,
            handle,
            tray,
            presets,
            active_preset: None,
            routing,
            routing_attempted: config.take_over_default,
            last_refresh: Instant::now() - REFRESH_INTERVAL,
            seen_presets_revision: 0,
            router,
            route_snapshot,
            mixer_rx,
            mixer_tx,
            config,
        };

        if let Some(path) = active {
            app.activate_preset(&path);
        }
        app.refresh();

        Ok(app)
    }

    /// Runs one pass of the event loop. Returns `true` when the app should exit.
    pub fn tick(&mut self) -> bool {
        // The mixer's requests first, so an edit made in the window has reached
        // the routing engine before this tick's refresh republishes the
        // snapshot. The other order makes the window redraw its own change as
        // not having happened for up to a second.
        self.pump_mixer();

        if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.refresh();
        }

        // The menu is asked first, then the icon's own clicks. A right-click
        // emits a click event as well as opening the menu, and the menu event is
        // the one carrying meaning; asking in this order means a stray click
        // event cannot shadow a menu choice.
        let action = self.tray.poll().or_else(|| self.tray.poll_icon());

        match action {
            Some(TrayAction::Quit) => return true,
            Some(TrayAction::ToggleEnabled) => self.toggle_enabled(),
            Some(TrayAction::ToggleAutostart) => self.toggle_autostart(),
            Some(TrayAction::SelectPreset(path)) => self.activate_preset(&path),
            Some(TrayAction::OpenPanel) => self.open_panel(),
            Some(TrayAction::OpenMixer) => self.open_mixer(),
            Some(TrayAction::InstallDriver) => self.request_driver_install(),
            Some(TrayAction::RemoveDriver) => self.request_driver_removal(),
            Some(TrayAction::ReloadPresets) => self.reload_presets(),
            Some(TrayAction::RouteOutput) => self.route_output(),
            Some(TrayAction::SetLanguage(language)) => self.set_language(language),
            None => {}
        }
        false
    }

    /// Shuts the engine down cleanly and hands the output device back.
    ///
    /// Order matters. The engine stops first, so the moment the default output
    /// returns to the physical card there is no second copy of the audio still
    /// rendering into it — restoring first would put the unenhanced stream and
    /// the enhanced one on the same speaker for a moment, which is audible as
    /// an echo.
    pub fn shutdown(mut self) {
        self.engine.shutdown();

        if self.routing.release() {
            // Cleared only after a successful restore, so the marker keeps
            // pointing at the displaced device for as long as one is owed.
            self.config.previous_default_id = None;
        }
        if let Err(err) = self.config.save() {
            log::warn!("could not save the config: {err}");
        }
    }

    /// Refreshes the tooltip and the driver-dependent menu state.
    ///
    /// Polled rather than event-driven because the interesting facts
    /// ("is the endpoint there?", "is the card the default?") change through
    /// paths that do not notify us — the user installing FxSound itself, a
    /// driver update, Device Manager actions.
    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        self.reconcile_selection();
        self.pump_routing();

        // An owned handle rather than a borrow: the late takeover below needs
        // `&mut self`, and a reference into `self` would still be live here.
        let status = std::sync::Arc::clone(self.handle.status());

        // A preset was written by the panel (the engine does the writing, so
        // nothing on this thread saw it happen). Rescanning here is what puts
        // the new preset into the tray menu; the panel rescans its own copy off
        // the same counter.
        let revision = status.presets_revision();
        if revision != self.seen_presets_revision {
            self.seen_presets_revision = revision;
            self.reload_presets();
        }

        let driver_present = status.virtual_present() || device::virtual_device_present();
        self.tray.set_driver_present(driver_present);

        // A late takeover. The card can appear after start — the user installs
        // the driver from this very menu, for instance — and until it does,
        // there is nothing to route to.
        //
        // Guarded by `routing_attempted` so a card that is present but unusable
        // (disabled in Device Manager, say) is not retried once a second for
        // the life of the process. The flag is cleared whenever the card goes
        // away, so its reappearance is a fresh reason to try.
        //
        // Attempted only while `engaged` is false, which is also what stops
        // this from fighting a user who deliberately moved the output
        // elsewhere: the switch happens once, and a later change by hand is
        // theirs to keep.
        if !driver_present {
            self.routing_attempted = false;
        } else if self.config.take_over_default
            && self.handle.params().is_enabled()
            && !self.routing.engaged()
            && !self.routing_attempted
        {
            self.routing_attempted = true;
            self.engage_routing();
        }

        // Whether the output is actually flowing through the enhancer. Read
        // from the hardware, so it is also wrong when the user routed audio
        // past FxTrumpet by hand, not just when something failed.
        let routed = crate::routing::default_is_routed_through_card();
        self.tray.set_routed(routed);

        let text = &i18n::t().tray;
        let tooltip = if !driver_present {
            text.tooltip_no_card.to_owned()
        } else if !self.handle.params().is_enabled() {
            text.tooltip_disabled.to_owned()
        } else if status.is_running() {
            // The distinction that matters to a user reporting "nothing
            // changed": audio is being processed, or the enhancer is being
            // bypassed by the system's own routing.
            let route_lost = self.config.take_over_default && !routed;
            text.tooltip_playing(
                self.active_name().as_deref(),
                status.last_error().as_deref(),
                route_lost,
            )
        } else {
            text.tooltip_idle.to_owned()
        };
        self.tray.set_tooltip(&tooltip);
    }

    /// Copies selection state that the panel may have changed into the config
    /// file and the tray menu.
    ///
    /// The panel writes straight to the shared atomics — that is what keeps
    /// slider drags free of message passing — so anything the tray *displays*
    /// has to be reconciled here instead of assumed to be in sync. Polled from
    /// [`Self::refresh`], which [`Self::tick`] runs once a second.
    ///
    /// Tray-originated changes never reach the bodies below: the tray handlers
    /// update both sides, so there is nothing left to reconcile and the log
    /// lines stay truthful about where the change came from.
    fn reconcile_selection(&mut self) {
        let enabled = self.handle.params().is_enabled();
        if enabled != self.config.enabled {
            self.config.enabled = enabled;
            self.tray.set_enabled_checked(enabled);
            log::info!(
                "processing {} (changed in the panel)",
                if enabled { "enabled" } else { "disabled" }
            );
            self.save_config();
        }

        let selected = self.handle.status().active_preset().map(PathBuf::from);
        if self.active_preset != selected {
            self.active_preset = selected;
            self.tray.set_active_preset(self.active_preset.as_deref());
            self.config.active_preset = self
                .active_preset
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned());
            log::info!(
                "preset changed in the panel: {}",
                self.config.active_preset.as_deref().unwrap_or("none")
            );
            self.save_config();
        }
    }

    /// Persists the config, warning rather than failing when the file is not
    /// writable.
    fn save_config(&mut self) {
        if let Err(err) = self.config.save() {
            log::warn!("could not save the config: {err}");
        }
    }

    /// The active preset's display name.
    fn active_name(&self) -> Option<String> {
        let active = self.active_preset.as_ref()?;
        self.presets
            .iter()
            .find(|entry| &entry.path == active)
            .map(|entry| entry.name.clone())
    }

    fn toggle_enabled(&mut self) {
        self.config.enabled = !self.config.enabled;
        self.handle.params().set_enabled(self.config.enabled);
        self.tray.set_enabled_checked(self.config.enabled);

        log::info!("processing {}", if self.config.enabled { "enabled" } else { "disabled" });
        self.save_config();
        self.refresh();
    }

    fn toggle_autostart(&mut self) {
        let wanted = !crate::autostart::is_enabled();
        // Reconciled rather than written blindly: `apply` reads the registry
        // back afterwards, so a policy that blocked the write cannot leave the
        // config claiming something the machine is not going to do, and
        // switching it back on also clears Task Manager's disabled marker.
        self.config.autostart = crate::autostart::apply(wanted);
        self.save_config();
        self.tray.set_autostart_checked(self.config.autostart);
    }

    /// Loads a preset into the engine and records it.
    fn activate_preset(&mut self, path: &std::path::Path) {
        let preset = match preset::FacPreset::from_file(path) {
            Ok(preset) => preset,
            Err(err) => {
                log::error!("could not read {}: {err}", path.display());
                return;
            }
        };

        self.handle.send(crate::engine::EngineCommand::LoadPreset {
            path: path.to_path_buf(),
            preset,
        });

        // Published so an already-open panel follows the tray's choice, the
        // same way [`Self::reconcile_selection`] lets the tray follow the
        // panel's.
        self.handle.status().set_active_preset(Some(path));

        self.active_preset = Some(path.to_path_buf());
        self.tray.set_active_preset(self.active_preset.as_deref());

        self.config.active_preset = Some(path.to_string_lossy().into_owned());
        self.save_config();
    }

    fn reload_presets(&mut self) {
        self.presets = preset::library();
        self.tray.set_presets(&self.presets);
        self.tray.set_active_preset(self.active_preset.as_deref());
        log::info!("preset list refreshed: {} entries", self.presets.len());
    }

    /// Switches the interface language and relabels the tray menu in place.
    ///
    /// The tray is relabelled, not rebuilt. It used to be destroyed and
    /// recreated around a belief that `muda` had no text setter; it has, and a
    /// rebuild is a strictly worse way to change a word — it can fail, it
    /// blinks the icon out of the notification area, and the failure mode of
    /// the error path is a process with no user interface at all.
    ///
    /// The panel needs none of this: it re-reads the string table each frame,
    /// so an open panel changes language on its next repaint.
    fn set_language(&mut self, language: Lang) {
        // Picking the language that is already in force is a no-op for the
        // translation, but not for the menu: Windows toggles a checkmark on the
        // click *before* the application sees the event, so clicking the ticked
        // entry has just cleared its own tick. Re-assert the ticks and return —
        // there is nothing else to redo, and no reason to write the config
        // again.
        if language == i18n::current() {
            self.tray.refresh_language_ticks();
            return;
        }

        i18n::set(language);
        // See `Tray::retitle` for what has to be rewritten and why the route
        // entry is not simply assigned.
        self.tray.retitle();

        // Recorded as an explicit choice, replacing "auto": the user has now
        // said which language they want, and a later change to the system
        // language should not undo it.
        self.config.language = language.code().to_owned();
        self.save_config();
        log::info!("interface language switched to {}", language.endonym());

        self.refresh();
    }

    /// Points the system's default output at the virtual card, once.
    ///
    /// Shared by the late-takeover path in [`Self::refresh`] and the tray item,
    /// so both report identically.
    fn engage_routing(&mut self) {
        match self.routing.engage() {
            EngageOutcome::Engaged { .. } => {
                self.config.previous_default_id = self.routing.previous().map(str::to_owned);
                self.save_config();
            }
            EngageOutcome::AlreadyRouted => {}
            EngageOutcome::NoVirtualCard => {
                log::warn!("cannot route audio through FxTrumpet: no active virtual sound card");
            }
            EngageOutcome::Failed(err) => {
                log::error!("could not route audio through FxTrumpet: {err}");
            }
        }
    }

    /// The tray's "route output through FxTrumpet" item.
    ///
    /// The manual retry, for when the automatic switch was declined (the card
    /// appeared while disabled) or when the user moved the output away and has
    /// changed their mind.
    fn route_output(&mut self) {
        self.config.take_over_default = true;
        self.routing_attempted = true;
        self.engage_routing();
        self.refresh();
    }

    /// Opens the tuning panel, as if the tray item had been clicked.
    ///
    /// Public so `main` can honour `--panel`: a GUI window cannot be opened
    /// from a test harness otherwise, and it makes the panel smoke-testable
    /// without clicking through the tray.
    pub fn open_panel_now(&mut self) {
        self.open_panel();
    }

    fn open_panel(&mut self) {
        if panel::is_open(WindowKind::Panel) {
            return;
        }
        let shared = PanelShared::new(self.handle.clone(), self.presets.clone());
        if !panel::open_panel(shared) {
            log::warn!("the tuning panel is already open");
        }
    }

    /// Opens the mixer, as if the tray item had been clicked.
    ///
    /// Public for the same reason [`Self::open_panel_now`] is: `--mixer` cannot
    /// otherwise be exercised without clicking through the tray.
    pub fn open_mixer_now(&mut self) {
        self.open_mixer();
    }

    fn open_mixer(&mut self) {
        if panel::is_open(WindowKind::Mixer) {
            return;
        }
        let shared = MixerShared::new(
            Arc::clone(&self.route_snapshot),
            self.mixer_tx.clone(),
        );
        if !panel::open_mixer(shared) {
            log::warn!("the mixer is already open");
        }
    }

    /// Drains the mixer's inbox and acts on everything waiting.
    ///
    /// Drained in a loop rather than one per tick: a slider drag emits a
    /// command per frame the pointer moves, and acting on one of them per
    /// quarter-second would make the drag lag behind the pointer.
    fn pump_mixer(&mut self) {
        while let Ok(command) = self.mixer_rx.try_recv() {
            self.handle_mixer_command(command);
        }
    }

    /// Carries out one request from the mixer.
    ///
    /// Every arm answers on the machine, not in the window: the window's next
    /// poll reads the result back, so a write that Windows refused shows up as
    /// the control springing back rather than as a lie. Editing a rule also
    /// publishes a fresh snapshot immediately, so the window does not wait up to
    /// a second to see its own edit.
    fn handle_mixer_command(&mut self, command: MixerCommand) {
        match command {
            MixerCommand::SetAppTarget {
                app,
                display_name,
                target,
            } => {
                // `upsert` rather than a push: pointing an application at a
                // second device must move the rule, not add a contradictory one.
                log::info!("routing {display_name} to {target:?}");
                self.router.upsert(RouteRule::new(app, display_name, target));
                self.persist_routes();
            }

            MixerCommand::SetRuleEnabled { app, enabled } => {
                // Cloned out before `upsert`, which needs `&mut self` while the
                // rule is still borrowed from it.
                let existing = self
                    .router
                    .rule_for(&app)
                    .cloned()
                    .map(|mut rule| {
                        rule.enabled = enabled;
                        rule
                    });
                match existing {
                    Some(rule) => {
                        log::info!("rule for {} {}", rule.display_name, if enabled { "enabled" } else { "disabled" });
                        self.router.upsert(rule);
                        self.persist_routes();
                    }
                    None => log::warn!("no rule to toggle for {app:?}"),
                }
            }

            MixerCommand::RemoveRule { app } => {
                // `remove` takes the live session list because it has to undo
                // the policy on the processes that are running right now, and
                // only they can be undone — an application that has already
                // exited has nothing left to clear.
                let sessions = session::all_sessions();
                self.router.remove(&app, &sessions);
                log::info!("rule removed for {app:?}");
                self.persist_routes();
            }

            MixerCommand::SetAppVolume { streams, level } => {
                for (device_id, instance_id) in &streams {
                    if let Err(err) = session::set_volume(device_id, instance_id, level) {
                        // Expected for any application that closed between the
                        // window's poll and this call, which is why it is a
                        // debug line rather than a warning.
                        log::debug!("could not set a stream's volume: {err}");
                    }
                }
            }

            MixerCommand::SetAppMute { streams, muted } => {
                for (device_id, instance_id) in &streams {
                    if let Err(err) = session::set_mute(device_id, instance_id, muted) {
                        log::debug!("could not mute a stream: {err}");
                    }
                }
            }

            MixerCommand::SetDefaultDevice { endpoint_id } => {
                match device::set_default_device(&endpoint_id) {
                    Ok(()) => {
                        log::info!("default output changed to {endpoint_id}");
                        // The takeover's idea of where audio came from is now
                        // stale: the user has just moved it themselves.
                        self.config.previous_default_id = None;
                        self.save_config();
                    }
                    Err(err) => log::error!("could not change the default output: {err}"),
                }
            }

            MixerCommand::SetDeviceVolume { endpoint_id, level } => {
                if let Err(err) = endpoint::set_volume(&endpoint_id, level) {
                    log::debug!("could not set the device volume: {err}");
                }
            }

            MixerCommand::SetDeviceMuted { endpoint_id, muted } => {
                if let Err(err) = endpoint::set_muted(&endpoint_id, muted) {
                    log::debug!("could not mute the device: {err}");
                }
            }
        }
    }

    /// Enforces the routing rules against what is playing right now.
    ///
    /// Once a second, from [`Self::refresh`], and not more often: the pass
    /// enumerates every audio session and touches Windows' per-application
    /// policy, which is far too much work at the message loop's tick rate for a
    /// table that changes when a user opens an application.
    ///
    /// Only applications that are actually alive can be routed — the policy is
    /// keyed on a process id — so this is a convergence loop rather than a
    /// one-shot. Nothing is undone when an application exits, which is why a
    /// rule keeps working across a restart.
    fn pump_routing(&mut self) {
        let sessions = session::all_sessions();
        let report = self.router.apply(&sessions);

        if report.changed_anything() {
            log::info!(
                "routing rules applied: {} written, {} already in force",
                report.applied.len(),
                report.already_in_force
            );
        }
        for (rule, reason) in &report.failed {
            log::warn!("could not apply the rule for {rule}: {reason}");
        }
        for rule in &report.injection_deferred {
            log::debug!("rule for {rule} asked for injection, which is not built in");
        }

        // Republished whether or not anything changed, so a window that opened
        // between two passes starts from the truth rather than from a default.
        self.publish_routes();
    }

    /// Writes the current rule set into the config file.
    ///
    /// Kept separate from [`Self::publish_routes`] because they answer different
    /// questions and have different callers: this one is the users' intent
    /// outliving the process, that one is the window's view of this pass.
    fn persist_routes(&mut self) {
        self.config.routes = self.router.rules().to_vec();
        self.save_config();
        self.publish_routes();
    }

    /// Hands the window a fresh view of the router.
    fn publish_routes(&self) {
        match self.route_snapshot.lock() {
            Ok(mut guard) => *guard = self.router.snapshot(),
            // A poisoned lock means the window panicked mid-read. The snapshot
            // is a plain clone and nothing it holds is invalid, so recovering is
            // right: refusing to publish would freeze the window's routing view
            // for the rest of the process.
            Err(poisoned) => *poisoned.into_inner() = self.router.snapshot(),
        }
    }

    /// Asks for elevation and re-runs this executable to install the driver.
    fn request_driver_install(&mut self) {
        if driver::is_elevated() {
            match driver::install_with_default_guard() {
                Ok(outcome) => log::info!("driver install: {outcome:?}"),
                Err(err) => log::error!("driver install failed: {err}"),
            }
            self.refresh();
            return;
        }

        log::info!("requesting elevation to install the virtual sound card");
        if let Err(err) = driver::relaunch_elevated(&[driver::ARG_INSTALL_DRIVER]) {
            log::error!("{err}");
        }
    }

    fn request_driver_removal(&mut self) {
        if driver::is_elevated() {
            match driver::uninstall() {
                Ok(()) => log::info!("driver removed"),
                Err(err) => log::error!("driver removal failed: {err}"),
            }
            self.refresh();
            return;
        }

        log::info!("requesting elevation to remove the virtual sound card");
        if let Err(err) = driver::relaunch_elevated(&[driver::ARG_REMOVE_DRIVER]) {
            log::error!("{err}");
        }
    }

    /// Shared parameters, exposed for the binaries in `src/bin`.
    pub fn params(&self) -> &std::sync::Arc<SharedParams> {
        self.handle.params()
    }

    /// Live status, exposed for the binaries in `src/bin`.
    pub fn status(&self) -> &std::sync::Arc<EngineStatus> {
        self.handle.status()
    }

    /// The control handle, exposed for the binaries in `src/bin`.
    pub fn handle(&self) -> &EngineHandle {
        &self.handle
    }
}
