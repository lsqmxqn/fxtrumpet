//! Taking over — and giving back — the system's default playback device.
//!
//! ## Why this is not optional
//!
//! FxTrumpet's audio chain only works when the *system default output* is FxSound's
//! virtual card:
//!
//! ```text
//! default output = virtual card  ->  loopback capture  ->  DSP  ->  physical card
//! ```
//!
//! The virtual card is a dead end. It has no speaker of its own; audio written
//! to it goes nowhere unless somebody captures it. So if the default output is
//! still the physical card, applications render straight past the enhancer, the
//! virtual card receives nothing, and FxTrumpet cheerfully runs its whole graph
//! over pure silence. Every visible signal says the app is healthy — a built
//! graph, a loaded preset, no errors — and the user hears no difference at all.
//!
//! That is the failure this module removes. FxSound's own app does the same
//! thing on start and undoes it on exit; without it the product simply does not
//! work, which is why it is not behind an "advanced" switch.
//!
//! ## Not fighting the user
//!
//! Two behaviours are deliberately *not* implemented:
//!
//! * The takeover happens once, on start (or when the user asks). If they then
//!   switch to their TV or headset — a deliberate act — FxTrumpet notices, says so
//!   in the tooltip, and lets them. Re-asserting every second would make the
//!   machine impossible to route.
//! * Nothing is switched while the virtual card is absent or inactive. A
//!   half-installed driver must not be able to mute the machine.
//!
//! ## Surviving a crash
//!
//! A process that is killed cannot run its restore path, and a machine left
//! pointed at the virtual card has no sound at all. That is bad enough to be
//! worth a repair path: the displaced endpoint is written to the config before
//! the switch, and cleared only after a clean restore. So a non-empty
//! `previous_default_id` on start *is* the crash marker — see [`recover`].

use crate::device::{self, DeviceInfo};

/// How much of the takeover actually happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngageOutcome {
    /// The default output now points at the virtual card, displacing `previous`.
    Engaged { previous: String },
    /// It already pointed at the virtual card; nothing was changed.
    AlreadyRouted,
    /// FxSound's virtual card is not installed, or not active. Nothing to do.
    NoVirtualCard,
    /// The switch was attempted and failed.
    Failed(String),
}

impl EngageOutcome {
    /// Whether the machine's routing was changed by this call.
    pub fn changed(&self) -> bool {
        matches!(self, Self::Engaged { .. })
    }
}

/// Owns the difference between the machine's routing and the one FxTrumpet needs.
///
/// Held by the application rather than being a set of free functions so that
/// "did we do this?" has a definite answer, and so the restore cannot be
/// forgotten by a caller that lost track.
#[derive(Debug, Default)]
pub struct Routing {
    /// The endpoint displaced by the takeover, while it is displaced.
    previous: Option<String>,
    /// Whether the default output is currently pointing at the virtual card
    /// because of us.
    engaged: bool,
}

impl Routing {
    /// Nothing taken over yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Undoes a takeover that a previous run could not undo itself.
    ///
    /// Call this before [`Self::engage`] with the config's remembered endpoint.
    /// It only acts when the evidence is unambiguous — the client left a note,
    /// *and* the machine is currently pointed at the virtual card — because
    /// guessing here would mean switching a working machine's output for no
    /// reason.
    pub fn recover(remembered: Option<&str>) -> bool {
        let Some(remembered) = remembered else {
            return false;
        };
        let (Some(card), Ok(default)) = (device::find_virtual_device(), device::default_render_device())
        else {
            return false;
        };
        if default.id != card.id {
            // Either a clean exit already restored it, or the user pointed the
            // output at the card on purpose. Not ours to undo.
            return false;
        }
        if !device::try_restore_default(remembered) {
            return false;
        }
        log::warn!(
            "a previous run left the default output on the virtual sound card \
             (it did not exit cleanly); restored '{}'",
            endpoint_name(remembered)
        );
        true
    }

    /// Points the default output at the virtual card, if it is not already.
    pub fn engage(&mut self) -> EngageOutcome {
        let Some(card) = device::find_virtual_device() else {
            return EngageOutcome::NoVirtualCard;
        };
        if card.state != device::DeviceState::Active {
            log::warn!("the virtual sound card is present but {}; not switching to it", card.state);
            return EngageOutcome::NoVirtualCard;
        }

        match device::default_render_device() {
            Ok(current) if current.id == card.id => {
                self.engaged = true;
                return EngageOutcome::AlreadyRouted;
            }
            Ok(current) => {
                // Remembered before the switch, so a crash still leaves a trail
                // to follow. Overwritten on a re-engage: if the user had since
                // moved to headphones, *those* are what a restore owes them.
                self.previous = Some(current.id.clone());
            }
            Err(err) => {
                // No default at all — unusual, but switching to the card is
                // still an improvement over silence. Nothing to restore to.
                log::warn!("could not read the current default output: {err}");
                self.previous = None;
            }
        }

        match device::set_default_device(&card.id) {
            Ok(()) => {
                self.engaged = true;
                log::info!(
                    "output switched to the virtual sound card so audio is enhanced \
                     (was '{}', restored on exit)",
                    self.previous.as_deref().map(endpoint_name).unwrap_or_else(|| "—".to_owned())
                );
                EngageOutcome::Engaged {
                    previous: self.previous.clone().unwrap_or_default(),
                }
            }
            Err(err) => {
                let message = err.to_string();
                self.previous = None;
                log::error!("could not switch the default output to the virtual card: {message}");
                EngageOutcome::Failed(message)
            }
        }
    }

    /// Puts the displaced endpoint back, if there is one.
    ///
    /// Safe to call more than once, and safe to call when nothing was taken
    /// over. Returns whether the machine's routing was actually changed back.
    pub fn release(&mut self) -> bool {
        let was_engaged = std::mem::replace(&mut self.engaged, false);
        let Some(previous) = self.previous.take() else {
            return false;
        };

        // The displaced device may have been unplugged while we held the card.
        // Restoring a missing endpoint fails, so fall back to any active
        // physical output rather than leaving the machine on the dead end.
        let target = match device::device_by_id(&previous) {
            Some(info) if info.state == device::DeviceState::Active => Some(info.id),
            _ => {
                log::warn!(
                    "the output displaced by FxTrumpet ('{}') is gone; picking another",
                    endpoint_name(&previous)
                );
                first_physical_device().map(|info| info.id)
            }
        };

        let Some(target) = target else {
            log::error!(
                "no physical output to restore to; the default is still the virtual sound card"
            );
            return false;
        };

        if !device::try_restore_default(&target) {
            return false;
        }
        if was_engaged {
            log::info!("output restored to '{}'", endpoint_name(&target));
        }
        true
    }

    /// Whether the default output points at the virtual card thanks to us.
    pub fn engaged(&self) -> bool {
        self.engaged
    }

    /// The displaced endpoint, for the config's crash marker.
    pub fn previous(&self) -> Option<&str> {
        self.previous.as_deref()
    }
}

/// Whether the system's default output is the virtual card right now.
///
/// Read from the hardware rather than from [`Routing`], so it stays true when
/// the user routed the output themselves, or when the app just started and has
/// not looked yet.
pub fn default_is_routed_through_card() -> bool {
    match (device::find_virtual_device(), device::default_render_device()) {
        (Some(card), Ok(default)) => default.id == card.id,
        _ => false,
    }
}

/// Puts the default output back on a real device, with or without a marker.
///
/// [`Routing::recover`] is the normal path and needs the crash marker. This is
/// the belt-and-braces version for when the marker is gone — a hand-edited
/// config, or one written before the marker existed — but the machine is still
/// pointing at the dead end. In that case any active physical output beats
/// silence.
///
/// Returns whether the routing was changed. Used by `fxtrumpet --restore-output`,
/// which is what the uninstaller runs: a tray application cannot be asked to
/// quit politely, so an uninstall has to kill it, and a machine left on the
/// virtual card would otherwise have no sound *and* no startup entry left to
/// repair it.
pub fn rescue_output(remembered: Option<&str>) -> bool {
    if Routing::recover(remembered) {
        return true;
    }
    if !default_is_routed_through_card() {
        return false;
    }

    let Some(target) = first_physical_device() else {
        log::error!(
            "no physical output to fall back to; the default is still the virtual sound card"
        );
        return false;
    };

    log::warn!(
        "the default output is still the virtual sound card but nothing says what it displaced; \
         falling back to '{}'",
        target.name
    );
    if device::try_restore_default(&target.id) {
        log::info!("output restored to '{}'", target.name);
        true
    } else {
        false
    }
}

/// The first active non-virtual render endpoint, by name.
fn first_physical_device() -> Option<DeviceInfo> {
    device::render_devices()
        .ok()?
        .into_iter()
        .filter(|info| !info.is_virtual)
        .min_by_key(|info| info.name.to_lowercase())
}

/// A device's name for the log, or the raw id when the device is already gone.
fn endpoint_name(id: &str) -> String {
    device::device_by_id(id)
        .map(|info| info.name)
        .unwrap_or_else(|| id.to_owned())
}

/// Removes every per-application output override Windows is holding.
///
/// The companion to [`rescue_output`], and part of the same escape hatch: the
/// default device is only half of what FxTrumpet writes. A machine can have the
/// default back on a real card and still have half its applications pinned to
/// devices the user picked months ago, with no interface left to unpin them
/// after an uninstall.
///
/// **Not called on a normal exit.** A rule is the user's configuration rather
/// than a takeover, and clearing it every time the tray closed would mean
/// re-routing everything on every start. It also deliberately does not read the
/// config: the overrides live in Windows, not in the file, so they are cleared
/// whether or not the file still lists them — which is also what repairs
/// overrides left behind by a config that has since been deleted or hand-edited.
///
/// Returns how many processes were visited, for the log line.
pub fn release_per_app_overrides() -> usize {
    let router = crate::router::Router::new(Vec::new());
    if !router.policy_available() {
        // Nothing this build could ever have written, and probing said so at
        // construction. Reported rather than silently returning zero, because
        // "0 cleared" and "cannot clear" are different answers.
        log::warn!("per-application overrides cannot be cleared on this build");
        return 0;
    }

    let sessions = crate::session::all_sessions();
    let mut visited = std::collections::HashSet::new();
    for session in &sessions {
        if session.process_id != 0 {
            visited.insert(session.process_id);
        }
    }

    router.release_all(&sessions);
    visited.len()
}
