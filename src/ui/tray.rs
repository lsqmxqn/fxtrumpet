//! The notification-area icon and its menu.
//!
//! This is the whole user interface for normal use: FxTrumpet has no main window,
//! and the only reason a window ever exists is the tuning panel
//! ([`super::panel`]).
//!
//! ## Message pump requirement
//!
//! `tray-icon` and `muda` deliver events through Win32 messages, so the thread
//! that created the icon must be pumping. [`super::run_message_loop`] does
//! that, and also drains the menu and icon event queues this module reads.
//!
//! ## How the menu is grouped
//!
//! Four groups, in the order a user's questions arrive:
//!
//! 1. **Open something.** The two things people come here for. Left-click opens
//!    the tuning panel too — see [`Tray::poll_icon`] — so the panel entry is for
//!    discoverability rather than for the only route in; the mixer has no
//!    left-click shortcut because the double-click gesture is not spare.
//! 2. **What it is doing.** Processing on/off, whether the output is routed,
//!    and the preset list.
//! 3. **Settings.** Start with Windows, interface language.
//! 4. **Maintenance.** Driver install/remove, then quit.
//!
//! The panel and the mixer are one entry each rather than a submenu: there are
//! exactly two windows, they are both one click away, and a submenu would cost
//! two clicks to reach the thing that was already the shortest path.
//!
//! `Rescan presets` lives *inside* the presets submenu, where it belongs: it
//! used to sit between the driver entries, six rows below the list it refreshes.
//!
//! ## Why the two windows are not both open at once
//!
//! `ui::panel` explains the winit constraint; what matters here is that asking
//! for one window while the other is up *switches* rather than being refused.
//! The tray items therefore stay enabled and honest — pressing "Mixer" while the
//! panel is showing does what it says.
//!
//! ## Why the route entry states its state instead of greying out
//!
//! Greying out "Route output via FxTrumpet" once it had been done made "already
//! correct" look identical to "not available". The item now reads
//! `Output routed via FxTrumpet` with a tick when it has been done, so the
//! disabled state answers the question the user opened the menu to ask.
//!
//! ## Why both driver items always exist
//!
//! Showing only the applicable one means removing and re-adding menu entries
//! whenever the driver state changes, which invalidates ids and races with the
//! event queue. Instead both are always present and one is disabled — the same
//! thing the Sound control panel does.
//!
//! ## Language
//!
//! Every label comes from [`crate::i18n`] and is a single language, not the
//! bilingual `启用音效 / Enabled` this used to show. A menu is scanned from the
//! screen edge, where a wide label is clipped rather than wrapped, so doubling
//! its length to serve a reader who only needs half of it was costing the one
//! thing the menu is short of.
//!
//! Switching language retitles the existing items in place. It used to rebuild
//! the whole tray, on the belief that `muda` had no setter for an item's text;
//! it does ([`MenuItem::set_text`]), and a rebuild is a strictly worse way to
//! change a word — it can fail, and the failure mode is losing the only UI the
//! process has.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, MenuItemKind, PredefinedMenuItem, Submenu,
};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::i18n::{self, Lang};
use crate::preset::PresetEntry;

/// Something the user asked for through the tray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    /// Enable or disable processing.
    ToggleEnabled,
    /// Enable or disable running at logon.
    ToggleAutostart,
    /// Switch to a `.fac` file.
    SelectPreset(PathBuf),
    /// Open the tuning panel.
    OpenPanel,
    /// Open the mixer: per-application volumes and per-application output
    /// device routing.
    OpenMixer,
    /// Install the virtual sound card (triggers an elevation prompt).
    InstallDriver,
    /// Remove the virtual sound card.
    RemoveDriver,
    /// Re-scan the preset folder.
    ReloadPresets,
    /// Point the system's default output at the virtual sound card, so audio
    /// actually flows through the enhancer.
    RouteOutput,
    /// Draw the interface in a different language.
    SetLanguage(Lang),
    /// Leave.
    Quit,
}

/// A preset's menu entry, kept so it can be ticked later.
struct PresetMenuItem {
    id: MenuId,
    path: PathBuf,
    item: CheckMenuItem,
}

/// Owns the tray icon and its menu.
pub struct Tray {
    icon: TrayIcon,

    open_item: MenuItem,
    mixer_item: MenuItem,
    enabled_item: CheckMenuItem,
    route_item: CheckMenuItem,
    preset_menu: Submenu,
    preset_items: Vec<PresetMenuItem>,
    /// The disabled `（无预设）` placeholder, present only while the list is
    /// empty. Kept so a language switch can retitle it.
    preset_empty: Option<MenuItem>,
    rescan_item: MenuItem,
    autostart_item: CheckMenuItem,
    language_menu: Submenu,
    /// The per-language entries, taken back out of the submenu's own child list.
    ///
    /// They have to be held rather than looked up later: the tick on the current
    /// language is the only thing in this menu that says *which* language is in
    /// force, and `Submenu::items()` cannot be used to recover them.
    ///
    /// `Submenu` is thread-bound — it stores its children inside a
    /// `ManuallyDrop` wrapper with a hand-written `unsafe impl Send`, so
    /// `items()` is only sound on the thread that created them, and dropping the
    /// clones it returns anywhere else is undefined behaviour. `app.rs`
    /// deliberately switches language from the message-loop thread while the
    /// tray was built on the UI thread, so reading them back there would be
    /// exactly the cross-thread access that invariant forbids.
    ///
    /// (`MenuId` is `Arc<str>` and would be safe, but a `MenuId` alone cannot be
    /// ticked — `set_checked` lives on the item.)
    language_items: Vec<(Lang, CheckMenuItem)>,
    install_item: MenuItem,
    remove_item: MenuItem,
    quit_item: MenuItem,

    /// Menu id -> action, for the fixed entries.
    actions: HashMap<MenuId, TrayAction>,

    /// The two facts [`Self::refresh_route`] needs, kept because the route
    /// entry's label and enabled state depend on both and they arrive from
    /// different places.
    routed: Cell<bool>,
    driver_present: Cell<bool>,
}

/// Builds the language submenu, returning it together with the entries that
/// have to be ticked later.
///
/// The entries are endonyms — "中文" and "English" — so the way out of a language
/// you cannot read is legible in that language. They are also the one pair of
/// labels that never needs retitling, which is why they used not to be stored at
/// all; the tick is the part that has to be kept, because it is the only
/// on-screen record of which language is in force.
///
/// The items are recovered from [`Submenu::items`] rather than kept from the
/// handles created here. `Submenu::append` takes its argument by reference and
/// clones it into the child list, and the clone *shares* the original's id and
/// shared state — reticking either copy works. Going through the child list is
/// still better: it is what the user actually sees, so a failed append shows up
/// as a short list rather than as a handle that is ticked but not on screen.
/// `items()` is sound here because this is the thread that created them; the
/// returned clones must not be dropped elsewhere, which the note on
/// `Tray::language_items` spells out.
fn build_language_menu(
    label: &str,
) -> Result<(Submenu, Vec<(Lang, CheckMenuItem)>), String> {
    let menu = Submenu::new(label, true);
    let current = i18n::current();
    for lang in Lang::ALL {
        let item = CheckMenuItem::new(lang.endonym(), true, lang == current, None);
        menu.append(&item).map_err(stringify)?;
    }

    let items: Vec<(Lang, CheckMenuItem)> = menu
        .items()
        .into_iter()
        .filter_map(|kind| match kind {
            MenuItemKind::Check(item) => Lang::ALL
                .iter()
                .copied()
                .find(|lang| lang.endonym() == item.text())
                .map(|lang| (lang, item)),
            _ => None,
        })
        .collect();

    // A short list would mean an append did not land, and a ticked item that is
    // not in the child list is invisible rather than merely wrong.
    assert_eq!(
        items.len(),
        Lang::ALL.len(),
        "every language entry must be recoverable from the submenu"
    );

    Ok((menu, items))
}

/// Ticks `current` and unticks everything else.
///
/// Split out from [`Tray::refresh_language_ticks`] so it can be exercised
/// without a live tray icon — [`Tray::new`] builds one, which needs a desktop
/// session, so a test that went through the `Tray` would be one of the
/// `#[ignore]`d ones and the regression would go untested in CI.
fn tick_languages(items: &[(Lang, CheckMenuItem)], current: Lang) {
    for (lang, item) in items {
        // Set *and* clear. Only setting is the original bug: Windows leaves a
        // checkmark until it is explicitly taken off, so the old language kept
        // its tick and two entries were ticked at once.
        item.set_checked(*lang == current);
    }
}

impl Tray {
    /// Builds the icon and menu, in the language [`i18n::current`] reports.
    pub fn new(
        presets: &[PresetEntry],
        enabled: bool,
        autostart: bool,
        driver_present: bool,
    ) -> Result<Self, String> {
        let text = &i18n::t().tray;
        let menu = Menu::new();

        let open_item = MenuItem::new(text.panel, true, None);
        let mixer_item = MenuItem::new(text.mixer, true, None);
        let enabled_item = CheckMenuItem::new(text.enabled, true, enabled, None);
        let route_item = CheckMenuItem::new(text.route_through, true, false, None);
        let preset_menu = Submenu::new(text.presets, true);
        let rescan_item = MenuItem::new(text.rescan, true, None);
        let autostart_item = CheckMenuItem::new(text.autostart, true, autostart, None);
        let install_item = MenuItem::new(text.install_driver, true, None);
        let remove_item = MenuItem::new(text.remove_driver, true, None);
        let quit_item = MenuItem::new(text.quit, true, None);

        let (language_menu, language_items) = build_language_menu(text.language)?;

        let mut actions: HashMap<MenuId, TrayAction> = [
            (open_item.id().clone(), TrayAction::OpenPanel),
            (mixer_item.id().clone(), TrayAction::OpenMixer),
            (enabled_item.id().clone(), TrayAction::ToggleEnabled),
            (route_item.id().clone(), TrayAction::RouteOutput),
            (rescan_item.id().clone(), TrayAction::ReloadPresets),
            (autostart_item.id().clone(), TrayAction::ToggleAutostart),
            (install_item.id().clone(), TrayAction::InstallDriver),
            (remove_item.id().clone(), TrayAction::RemoveDriver),
            (quit_item.id().clone(), TrayAction::Quit),
        ]
        .into_iter()
        .collect();

        for (lang, item) in &language_items {
            actions.insert(item.id().clone(), TrayAction::SetLanguage(*lang));
        }

        let separator = || PredefinedMenuItem::separator();

        // Group 1: the panel.
        menu.append(&open_item).map_err(stringify)?;
        menu.append(&mixer_item).map_err(stringify)?;
        menu.append(&separator()).map_err(stringify)?;
        // Group 2: state.
        menu.append(&enabled_item).map_err(stringify)?;
        menu.append(&route_item).map_err(stringify)?;
        menu.append(&preset_menu).map_err(stringify)?;
        menu.append(&separator()).map_err(stringify)?;
        // Group 3: settings.
        menu.append(&autostart_item).map_err(stringify)?;
        menu.append(&language_menu).map_err(stringify)?;
        menu.append(&separator()).map_err(stringify)?;
        // Group 4: maintenance, then the exit.
        menu.append(&install_item).map_err(stringify)?;
        menu.append(&remove_item).map_err(stringify)?;
        menu.append(&separator()).map_err(stringify)?;
        menu.append(&quit_item).map_err(stringify)?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("FxTrumpet")
            .with_icon(super::icon::tray_icon())
            // The menu is on right-click only, freeing left-click for the
            // panel. On Windows the two are independent, so this does not cost
            // the menu.
            .with_menu_on_left_click(false)
            .build()
            .map_err(stringify)?;

        let mut tray = Self {
            icon,
            open_item,
            mixer_item,
            enabled_item,
            route_item,
            preset_menu,
            preset_items: Vec::new(),
            preset_empty: None,
            rescan_item,
            autostart_item,
            language_menu,
            language_items,
            install_item,
            remove_item,
            quit_item,
            actions,
            routed: Cell::new(false),
            driver_present: Cell::new(driver_present),
        };
        tray.set_presets(presets);
        tray.set_driver_present(driver_present);
        Ok(tray)
    }

    /// Replaces the preset submenu contents.
    ///
    /// The submenu is emptied and rebuilt rather than reconciled: `Submenu` has
    /// no bulk clear and removal takes the item rather than an index, so
    /// tracking which of four kinds of child is stale costs more than rebuilding
    /// a list that is at most a few dozen rows.
    pub fn set_presets(&mut self, presets: &[PresetEntry]) {
        let existing = self.preset_menu.items().len();
        for _ in 0..existing {
            self.preset_menu.remove_at(0);
        }
        self.preset_items.clear();
        self.preset_empty = None;

        if presets.is_empty() {
            let empty = MenuItem::new(i18n::t().tray.no_presets, false, None);
            if self.preset_menu.append(&empty).is_ok() {
                self.preset_empty = Some(empty);
            }
        } else {
            for entry in presets {
                // The display name comes from inside the file; fall back to the
                // filename when a preset has an empty name line.
                let label = if entry.name.trim().is_empty() {
                    entry
                        .path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "?".to_owned())
                } else {
                    entry.name.clone()
                };

                let item = CheckMenuItem::new(&label, true, false, None);
                let id = item.id().clone();
                if self.preset_menu.append(&item).is_ok() {
                    self.preset_items.push(PresetMenuItem {
                        id,
                        path: entry.path.clone(),
                        item,
                    });
                }
            }
        }

        // Rescanning is a preset operation, so it lives with the presets —
        // pinned below a separator so it does not read as one of them.
        let _ = self.preset_menu.append(&PredefinedMenuItem::separator());
        let _ = self.preset_menu.append(&self.rescan_item);
    }

    /// Ticks the given preset and unticks the rest.
    pub fn set_active_preset(&self, path: Option<&Path>) {
        for entry in &self.preset_items {
            entry
                .item
                .set_checked(path.is_some_and(|wanted| wanted == entry.path));
        }
    }

    /// Disables whichever driver action does not apply.
    pub fn set_driver_present(&self, present: bool) {
        self.driver_present.set(present);
        self.install_item.set_enabled(!present);
        self.remove_item.set_enabled(present);
        // The route entry is only meaningful while there is something to route
        // to, so it follows this too.
        self.refresh_route();
    }

    /// States whether audio is currently going through the enhancer.
    ///
    /// Not a toggle: there is no useful "unroute" (quitting FxTrumpet already does
    /// that), so the entry describes the situation and is pressable exactly when
    /// pressing it would change something.
    pub fn set_routed(&self, routed: bool) {
        self.routed.set(routed);
        self.refresh_route();
    }

    /// Keeps the checkbox in sync when the state changed elsewhere (the panel
    /// can toggle processing too).
    pub fn set_enabled_checked(&self, on: bool) {
        self.enabled_item.set_checked(on);
    }

    pub fn set_autostart_checked(&self, on: bool) {
        self.autostart_item.set_checked(on);
    }

    /// Redraws the route entry from the two facts it depends on.
    fn refresh_route(&self) {
        let text = &i18n::t().tray;
        if self.routed.get() {
            // The tick plus the wording carry the meaning; the item is disabled
            // because there is nothing left to press, not because it is
            // unavailable.
            self.route_item.set_text(text.route_through_done);
            self.route_item.set_checked(true);
            self.route_item.set_enabled(false);
        } else {
            self.route_item.set_text(text.route_through);
            self.route_item.set_checked(false);
            self.route_item.set_enabled(self.driver_present.get());
        }
    }

    /// Rewrites every label from the current language table, in place.
    ///
    /// Preset names come from the files rather than the table, so they are left
    /// alone; only the empty-list placeholder needs a new word.
    pub fn retitle(&self) {
        let text = &i18n::t().tray;
        self.open_item.set_text(text.panel);
        self.mixer_item.set_text(text.mixer);
        self.enabled_item.set_text(text.enabled);
        self.preset_menu.set_text(text.presets);
        self.rescan_item.set_text(text.rescan);
        self.autostart_item.set_text(text.autostart);
        self.language_menu.set_text(text.language);
        self.install_item.set_text(text.install_driver);
        self.remove_item.set_text(text.remove_driver);
        self.quit_item.set_text(text.quit);
        if let Some(empty) = &self.preset_empty {
            empty.set_text(text.no_presets);
        }
        // The language entries never change their own wording, but which one is
        // ticked does move — that tick is the only on-screen record of the
        // current language.
        self.refresh_language_ticks();
        // The route entry's wording is state-dependent, so it is not simply
        // assigned here.
        self.refresh_route();
    }

    /// Ticks the language in force and clears the others.
    ///
    /// Each language entry is an independent checkmark, and Windows leaves a
    /// checkmark exactly as it was set — nothing clears its siblings. So the
    /// tick has to be moved by hand, in both directions: set it on the new
    /// language *and* clear it off the old one. Doing only the first is what
    /// used to leave two ticks showing.
    ///
    /// Callable from outside because Windows also toggles an item's checkmark
    /// on the click, before the application ever sees the event. So the state
    /// arriving here is never the state to trust — it is re-derived from
    /// [`i18n::current`] on every language change *and* on re-picking the
    /// language already in force, which is otherwise a no-op.
    pub fn refresh_language_ticks(&self) {
        tick_languages(&self.language_items, i18n::current());
    }

    /// Updates the hover tooltip.
    pub fn set_tooltip(&self, text: &str) {
        let _ = self.icon.set_tooltip(Some(text));
    }

    /// Reads one pending menu event.
    ///
    /// Non-blocking: the caller is a message loop that has to stay responsive.
    /// Events for ids we do not know are discarded rather than forwarded, so a
    /// stale queue entry cannot trigger a surprise action.
    pub fn poll(&self) -> Option<TrayAction> {
        let receiver = MenuEvent::receiver();
        while let Ok(event) = receiver.try_recv() {
            if let Some(action) = self.actions.get(&event.id) {
                return Some(action.clone());
            }
            if let Some(entry) = self
                .preset_items
                .iter()
                .find(|entry| entry.id == event.id)
            {
                return Some(TrayAction::SelectPreset(entry.path.clone()));
            }
        }
        None
    }

    /// Reads the icon's own mouse events, collapsing them to one request.
    ///
    /// Left-click opens the panel, which is the Windows convention for a tray
    /// app's primary action and turns an icon with no window into a one-click
    /// way into the tuning panel. The menu is on right-click and is unaffected —
    /// `tray-icon` gates the two separately.
    ///
    /// The whole queue is drained before deciding: a double-click arrives as two
    /// clicks plus a double-click, and a request per event would try to open the
    /// panel three times. Wanting it any number of times is wanting it once.
    pub fn poll_icon(&self) -> Option<TrayAction> {
        let receiver = TrayIconEvent::receiver();
        let mut open = false;

        while let Ok(event) = receiver.try_recv() {
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => open = true,
                // Everything else — moves, hovers, right-clicks that are about
                // to open the menu — is not ours to act on.
                _ => {}
            }
        }

        open.then_some(TrayAction::OpenPanel)
    }
}

/// Flattens a `tray_icon` error into a `String` for the caller's log.
fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ticked set, as the menu would draw it.
    fn ticks(items: &[(Lang, CheckMenuItem)]) -> Vec<Lang> {
        items
            .iter()
            .filter(|(_, item)| item.is_checked())
            .map(|(lang, _)| *lang)
            .collect()
    }

    /// Exactly one entry is ticked, and it is `lang`.
    ///
    /// This is the assertion the reported bug needed: the old code only ever
    /// ticked the new language, so after a switch both were ticked.
    fn assert_only(items: &[(Lang, CheckMenuItem)], lang: Lang) {
        assert_eq!(ticks(items), vec![lang], "expected only {lang:?} to be ticked");
    }

    /// The language that is not `lang`.
    fn the_other(lang: Lang) -> Lang {
        Lang::ALL
            .iter()
            .copied()
            .find(|l| *l != lang)
            .expect("the language table holds more than one language")
    }

    /// Pins [`i18n::current`] for the duration of a test, and puts it back.
    ///
    /// `CURRENT` is a process-global `AtomicU8`, and the tests in this binary
    /// run in parallel threads, so a test that only *reads* `current()` is
    /// reading whatever another thread has momentarily set it to —
    /// `i18n::tests::selecting_a_language_is_visible_through_t` walks it
    /// through `En` and then `Zh`. That made these tests order-dependent: they
    /// passed in the full suite (where the interleaving happened to suit them)
    /// and failed when the module was run on its own. Pinning the value is what
    /// makes them deterministic; restoring it is what keeps them from being the
    /// cause of someone else's flake.
    ///
    /// A poisoned lock is deliberately *not* treated as a failure — a panic in
    /// one test would otherwise cascade into every later one, hiding the real
    /// error behind a wall of poison reports.
    fn with_language<T>(lang: Lang, body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = i18n::current();
        i18n::set(lang);
        let result = body();
        i18n::set(before);
        result
    }

    #[test]
    fn the_language_menu_lists_every_language_once() {
        let (_menu, items) = build_language_menu("Language").expect("submenu");

        // Order matters: it is the order the switcher presents.
        let listed: Vec<Lang> = items.iter().map(|(lang, _)| *lang).collect();
        assert_eq!(listed, Lang::ALL.to_vec());

        // The labels are endonyms, so the set of texts is the set of endonyms —
        // this is what the lookup in `build_language_menu` matches on, and it
        // only stays unambiguous while that holds.
        let texts: Vec<String> = items.iter().map(|(_, item)| item.text()).collect();
        let expected: Vec<String> = Lang::ALL.iter().map(|l| l.endonym().to_owned()).collect();
        assert_eq!(texts, expected);
    }

    /// Whichever language is in force when the menu is built is the ticked one.
    ///
    /// Run for *both* languages, so the assertion cannot be satisfied by a
    /// menu that always ticks the same entry.
    #[test]
    fn the_initial_tick_is_on_the_language_in_force() {
        for lang in Lang::ALL {
            with_language(lang, || {
                let (_menu, items) = build_language_menu("Language").expect("submenu");
                assert_only(&items, lang);
            });
        }
    }

    /// Switching languages moves the tick instead of adding one.
    ///
    /// Drives the same three steps the app does — flip the language, re-tick,
    /// put it back — because the failure was never in one step alone.
    #[test]
    fn switching_language_moves_the_tick_rather_than_adding_one() {
        let original = Lang::Zh;
        let other = the_other(original);

        with_language(original, || {
            let (_menu, items) = build_language_menu("Language").expect("submenu");
            assert_only(&items, original);

            i18n::set(other);
            tick_languages(&items, i18n::current());
            // The regression: before the fix this was [original, other].
            assert_only(&items, other);

            i18n::set(original);
            tick_languages(&items, i18n::current());
            assert_only(&items, original);
        });
    }

    /// Re-picking the language already in force must not clear its own tick.
    ///
    /// Windows toggles the item's checkmark on the click, so by the time the
    /// action arrives the tick has already gone; the app has to put it back.
    /// This is the second half of the report — "clicking English again removes
    /// the tick even though English is still in force".
    #[test]
    fn re_picking_the_current_language_restores_its_tick() {
        // Both languages get a turn: the bug cleared the tick of whichever was
        // in force, so a test that only checked one would miss it on the other.
        for current in Lang::ALL {
            with_language(current, || {
                let (_menu, items) = build_language_menu("Language").expect("submenu");
                let (_, item) = items
                    .iter()
                    .find(|(lang, _)| *lang == current)
                    .expect("the current language has an entry");

                // What Windows leaves behind after the click toggled it off. The
                // other entry is left ticked on purpose: the buggy
                // implementation only *sets* the tick, which happens to be
                // enough when nothing else is ticked, so a blank slate would let
                // it pass. The real desktop state is "the wrong entry is ticked",
                // and only a clearing implementation repairs that.
                item.set_checked(false);
                let (other_lang, other_item) = items
                    .iter()
                    .find(|(lang, _)| *lang != current)
                    .expect("more than one language");
                other_item.set_checked(true);
                assert_eq!(
                    ticks(&items),
                    vec![*other_lang],
                    "precondition: the other entry is ticked and the current one is not"
                );

                tick_languages(&items, i18n::current());
                assert_only(&items, current);
            });
        }
    }

    #[test]
    fn every_language_entry_has_a_switch_action() {
        // `build_language_menu` and the action map are filled from the same
        // list, so a mismatch would mean an entry that silently does nothing.
        let (_menu, items) = build_language_menu("Language").expect("submenu");
        assert_eq!(
            items.len(),
            Lang::ALL.len(),
            "a language entry with no action would be a dead menu item"
        );
    }
}

