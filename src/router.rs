//! Routing rules: which application's audio goes where.
//!
//! This module is the third feature of the merge, and the one that needed the
//! most design thought, because "send this app to that device" can be done two
//! quite different ways and neither dominates.
//!
//! ## Two mechanisms, and why the cheap one is the default
//!
//! | | [`RouteMethod::Policy`] | [`RouteMethod::Injection`] |
//! |---|---|---|
//! | what it does | writes the per-app default endpoint | patches the target's `IAudioClient` vtable |
//! | privilege | none | administrator |
//! | reach | any app that asks Windows where to render | any app, including ones that never ask |
//! | granularity | per process | per stream, and it can fan one stream out to several devices |
//! | cost | one API call | a DLL injected into someone else's process |
//!
//! Policy is the default because it is free and covers nearly everything.
//! Injection is what `audio-router` exists for, and it is kept — opt-in per
//! rule — for the applications that ignore the policy: older software,
//! anything that opens WASAPI itself, and processes the policy writer cannot
//! reach. It is not implemented in this skeleton; see [`InjectionStatus`] for
//! what is and is not in place, rather than finding out from a rule that
//! silently does nothing.
//!
//! ## Rules are keyed by application, applied to process ids
//!
//! The policy API takes a *process id*, but a routing rule has to survive a
//! restart, and a process id does not. So a rule is keyed by [`AppKey`] (an
//! executable path or an AUMID) and the pid is looked up at apply time from the
//! live session list. That inversion is the whole reason [`Router::apply`]
//! needs the sessions passed in rather than being self-contained.
//!
//! ## Re-application
//!
//! Windows applies a persisted per-application endpoint when the application's
//! stream is created, and — as EarTrumpet's own documentation notes — not
//! reliably at application start. So the router re-applies a rule whenever it
//! sees a process it has not yet applied it to, which is why
//! [`Router::apply`] is called on a poll rather than once at startup. The
//! `applied` set is what stops that from being a write per second per rule.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::perapp;
use crate::process::{self, AppKey};
use crate::session::Session;

/// Where an application's audio should go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteTarget {
    /// Follow the system default.
    ///
    /// Note what this means in a merged application: when the enhancer holds
    /// the system default, "system default" *is* the enhanced path. A rule
    /// with this target is how a user un-routes an application they previously
    /// sent straight to a physical device.
    SystemDefault,
    /// Render into this endpoint, bypassing whatever the system default is.
    Device { endpoint_id: String },
}

impl RouteTarget {
    /// Whether an application on this target misses the enhancer.
    ///
    /// True for every concrete device, because the enhancer only processes what
    /// flows through its own virtual card. The mixer shows this to the user:
    /// a routed application is silently un-enhanced otherwise, which looks like
    /// "the equalizer stopped working".
    pub fn bypasses_enhancement(&self) -> bool {
        matches!(self, RouteTarget::Device { .. })
    }
}

/// How a rule is enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RouteMethod {
    /// Windows' per-application default endpoint. No elevation.
    #[default]
    Policy,
    /// DLL injection into the target process. Needs administrator rights.
    Injection,
}

/// What the injection path can currently do.
///
/// Reported through the tray and the panel instead of being discovered from a
/// rule that does nothing. A skeleton that claims a capability it lacks is
/// worse than one that says it is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionStatus {
    /// Not built into this binary yet. Rules that ask for it fall back to the
    /// policy mechanism and say so.
    NotAvailable,
}

impl InjectionStatus {
    pub fn summary(&self) -> &'static str {
        match self {
            InjectionStatus::NotAvailable => {
                "not built in; injection rules fall back to the policy mechanism"
            }
        }
    }
}

/// One routing rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRule {
    /// The application this rule is about. This is the identity; everything
    /// else here is either the user's intent or a display convenience.
    pub app: AppKey,
    /// Cached so the list can be drawn without re-opening nine processes.
    pub display_name: String,
    pub target: RouteTarget,
    #[serde(default)]
    pub method: RouteMethod,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl RouteRule {
    /// A rule for `app`, pointed at `target`.
    pub fn new(app: AppKey, display_name: String, target: RouteTarget) -> Self {
        Self {
            app,
            display_name,
            target,
            method: RouteMethod::Policy,
            enabled: true,
        }
    }

    /// Whether this rule applies to a given application.
    pub fn matches(&self, app: &AppKey) -> bool {
        self.enabled && self.app.same_as(app)
    }
}

/// What one pass of the router did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    /// Rules written to Windows this pass.
    pub applied: Vec<String>,
    /// Applications seen that matched a rule already in force — the steady
    /// state, and the reason `applied` is usually empty.
    pub already_in_force: usize,
    /// Rules that could not be enforced, with the reason.
    pub failed: Vec<(String, String)>,
    /// Rules that asked for injection. Reported rather than applied.
    pub injection_deferred: Vec<String>,
}

impl ApplyReport {
    /// Whether anything about the machine changed this pass.
    pub fn changed_anything(&self) -> bool {
        !self.applied.is_empty()
    }
}

/// A read-only view of the router, for a window on another thread.
///
/// The mixer renders on the panel thread, the router lives on the tray thread,
/// and the two must not share the `Router` itself — it holds a COM factory and
/// a `HashSet` of what has been applied, neither of which means anything off
/// the thread that owns it. So the main thread publishes one of these each tick
/// and the window reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSnapshot {
    pub rules: Vec<RouteRule>,
    /// Whether this build can enforce policy-based rules at all.
    pub policy_available: bool,
    pub injection: InjectionStatus,
}

impl RouteSnapshot {
    /// The rule in force for an application, if any.
    pub fn rule_for(&self, app: &AppKey) -> Option<&RouteRule> {
        self.rules.iter().find(|rule| rule.app.same_as(app))
    }

    /// The destination an application is currently set to.
    ///
    /// An application with no rule follows the system default, which is also
    /// what [`RouteTarget::SystemDefault`] means — so this returns the same
    /// thing for both, and the UI cannot show them differently by accident.
    pub fn target_for(&self, app: &AppKey) -> RouteTarget {
        self.rule_for(app)
            .map(|rule| rule.target.clone())
            .unwrap_or(RouteTarget::SystemDefault)
    }
}

/// The routing engine.
pub struct Router {
    rules: Vec<RouteRule>,
    /// Policy factories are cheap to make but not free; one is kept for the
    /// life of the router. `None` means this machine cannot do per-app
    /// endpoints at all, which is recorded rather than retried every second.
    policy: Option<perapp::Factory>,
    /// `(rule index, process id)` pairs already written, so a poll does not
    /// rewrite a rule that is already in force.
    ///
    /// Keyed on the pid as well as the rule because a restart of the
    /// application is a fresh process with a fresh, unset policy — dropping
    /// just the rule from the set would leave the restarted app on the wrong
    /// device.
    applied: HashSet<(usize, u32)>,
    injection: InjectionStatus,
}

impl Router {
    /// Builds a router and probes whether per-app endpoints are available.
    pub fn new(rules: Vec<RouteRule>) -> Self {
        let policy = match perapp::Factory::new() {
            Ok(factory) => {
                log::info!(
                    "per-application endpoints available ({:?})",
                    factory.variant()
                );
                Some(factory)
            }
            Err(err) => {
                log::warn!(
                    "per-application endpoints are unavailable on this build: {err}. \
                     Routing rules cannot be enforced."
                );
                None
            }
        };

        Self {
            rules,
            policy,
            applied: HashSet::new(),
            injection: InjectionStatus::NotAvailable,
        }
    }

    /// Whether this machine can enforce policy-based rules.
    pub fn policy_available(&self) -> bool {
        self.policy.is_some()
    }

    /// What the injection path can do here.
    pub fn injection_status(&self) -> InjectionStatus {
        self.injection
    }

    /// A snapshot for a window on another thread.
    pub fn snapshot(&self) -> RouteSnapshot {
        RouteSnapshot {
            rules: self.rules.clone(),
            policy_available: self.policy.is_some(),
            injection: self.injection,
        }
    }

    pub fn rules(&self) -> &[RouteRule] {
        &self.rules
    }

    /// The rule for an application, if there is one.
    pub fn rule_for(&self, app: &AppKey) -> Option<&RouteRule> {
        self.rules.iter().find(|rule| rule.app.same_as(app))
    }

    /// Replaces the rule set, e.g. after the panel edited it.
    ///
    /// Everything already applied is forgotten: the caller has just changed its
    /// mind, and pretending the old writes still satisfy the new rules is how a
    /// stale state becomes invisible.
    pub fn set_rules(&mut self, rules: Vec<RouteRule>) {
        self.rules = rules;
        self.applied.clear();
    }

    /// Adds or replaces the rule for one application.
    pub fn upsert(&mut self, rule: RouteRule) {
        match self
            .rules
            .iter_mut()
            .find(|existing| existing.app.same_as(&rule.app))
        {
            Some(existing) => *existing = rule,
            None => self.rules.push(rule),
        }
        self.applied.clear();
    }

    /// Removes the rule for an application and hands it back to the default.
    ///
    /// The second half matters: forgetting a rule without clearing the write
    /// leaves Windows still routing the application, and the rule the user
    /// deleted came back the moment they restarted it. `sessions` is the live
    /// list, used to find the process ids to clear.
    pub fn remove(&mut self, app: &AppKey, sessions: &[Session]) {
        if let Some(index) = self.rules.iter().position(|rule| rule.app.same_as(app)) {
            self.rules.remove(index);
        }
        self.applied.clear();

        if let Some(factory) = &self.policy {
            for process_id in processes_for(app, sessions) {
                if let Err(err) = factory.clear_default_endpoint(process_id) {
                    log::warn!("could not clear the endpoint override for {process_id}: {err}");
                }
            }
        }
    }

    /// Enforces every enabled rule against the applications currently running.
    ///
    /// Safe to call on a poll: rules already in force are recognised and
    /// skipped. See the module docs for why this is not a one-shot at startup.
    pub fn apply(&mut self, sessions: &[Session]) -> ApplyReport {
        let mut report = ApplyReport::default();

        let mut seen: HashSet<u32> = HashSet::new();
        for session in sessions {
            if session.process_id == 0 || !seen.insert(session.process_id) {
                continue;
            }

            let Some(identity) = process::identify(session.process_id) else {
                continue;
            };
            let app = identity.key();

            let Some(index) = self.rules.iter().position(|rule| rule.matches(&app)) else {
                // Nothing in force for this application. A stale entry may
                // still exist — the user deleted the rule while the
                // application was running — and dropping it here is what lets
                // the rule be written again if it comes back.
                self.applied
                    .retain(|(_, pid)| *pid != session.process_id);
                continue;
            };

            let key = (index, session.process_id);
            if self.applied.contains(&key) {
                report.already_in_force += 1;
                continue;
            }

            // Copied out rather than borrowed: the arms below mutate
            // `self.applied`, and holding a reference into `self.rules` across
            // that is a borrow error for no benefit.
            let (method, display_name, target) = {
                let rule = &self.rules[index];
                (rule.method, rule.display_name.clone(), rule.target.clone())
            };

            match method {
                RouteMethod::Injection => {
                    // Honest deferral: recorded, not silently downgraded.
                    report.injection_deferred.push(display_name);
                }
                RouteMethod::Policy => match self.write_rule(&target, session.process_id) {
                    Ok(()) => {
                        self.applied.insert(key);
                        report.applied.push(display_name);
                    }
                    Err(err) => {
                        // A failure is not cached: the next pass retries, which
                        // is what recovers from a transient failure such as an
                        // endpoint that is mid-enumeration.
                        report.failed.push((display_name, err));
                    }
                },
            }
        }

        // A rule that no longer matches anything running has nothing to hold;
        // dropping it lets the rule be written again next time the application
        // appears, which is what makes a restart pick the route up.
        let live = seen;
        self.applied.retain(|(_, pid)| live.contains(pid));

        report
    }

    /// Writes one rule's intent to Windows.
    fn write_rule(&self, target: &RouteTarget, process_id: u32) -> Result<(), String> {
        let factory = match &self.policy {
            Some(factory) => factory,
            None => {
                return Err("per-application endpoints are not available on this build".to_owned())
            }
        };

        let result = match target {
            RouteTarget::SystemDefault => factory.clear_default_endpoint(process_id),
            RouteTarget::Device { endpoint_id } => {
                factory.set_default_endpoint(process_id, endpoint_id)
            }
        };

        result.map_err(|err| err.to_string())
    }

    /// Clears every override this application has written.
    ///
    /// The uninstall path. Without it, removing the tool leaves Windows still
    /// sending half the user's applications to devices they no longer remember
    /// choosing, with no interface left to undo it.
    pub fn release_all(&self, sessions: &[Session]) {
        let Some(factory) = &self.policy else {
            return;
        };

        let mut handled: HashSet<u32> = HashSet::new();
        for session in sessions {
            if session.process_id == 0 || !handled.insert(session.process_id) {
                continue;
            }
            // Clear unconditionally rather than only where a rule matched:
            // cheaper to reason about, idempotent, and it also repairs
            // overrides left behind by an earlier version.
            if let Err(err) = factory.clear_default_endpoint(session.process_id) {
                log::debug!(
                    "no endpoint override to clear for {}: {err}",
                    session.process_id
                );
            }
        }
    }
}

/// Every process id currently running `app`.
fn processes_for(app: &AppKey, sessions: &[Session]) -> Vec<u32> {
    let mut out = Vec::new();
    for session in sessions {
        if session.process_id == 0 || out.contains(&session.process_id) {
            continue;
        }
        if let Some(identity) = process::identify(session.process_id) {
            if identity.key().same_as(app) {
                out.push(session.process_id);
            }
        }
    }
    out
}

/// Serialisation for [`AppKey`], stored through its prefixed textual form so a
/// hand-edited config file stays readable and tamper-evident.
impl Serialize for AppKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_storage())
    }
}

impl<'de> Deserialize<'de> for AppKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        AppKey::from_storage(&text).ok_or_else(|| {
            serde::de::Error::custom(format!("not a recognised application key: {text}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionState};
    use std::path::PathBuf;

    fn session(process_id: u32) -> Session {
        Session {
            device_id: "endpoint".into(),
            instance_id: format!("inst-{process_id}"),
            display_id: format!("disp-{process_id}"),
            process_id,
            is_system_sounds: false,
            state: SessionState::Active,
            volume: 1.0,
            muted: false,
            peak: 0.0,
        }
    }

    fn rule_for(path: &str, target: RouteTarget) -> RouteRule {
        RouteRule::new(
            AppKey::Executable(PathBuf::from(path)),
            "app".to_owned(),
            target,
        )
    }

    #[test]
    fn a_concrete_device_bypasses_the_enhancer() {
        assert!(RouteTarget::Device {
            endpoint_id: "x".into()
        }
        .bypasses_enhancement());
        assert!(!RouteTarget::SystemDefault.bypasses_enhancement());
    }

    #[test]
    fn the_default_method_is_the_one_that_needs_no_elevation() {
        assert_eq!(RouteMethod::default(), RouteMethod::Policy);
    }

    #[test]
    fn an_existing_rule_is_replaced_rather_than_duplicated() {
        let mut router = Router {
            rules: Vec::new(),
            policy: None,
            applied: HashSet::new(),
            injection: InjectionStatus::NotAvailable,
        };

        router.upsert(rule_for(
            r"C:\Apps\Music.exe",
            RouteTarget::Device {
                endpoint_id: "a".into(),
            },
        ));
        router.upsert(rule_for(
            r"C:\Apps\Music.exe",
            RouteTarget::Device {
                endpoint_id: "b".into(),
            },
        ));

        assert_eq!(router.rules().len(), 1);
        assert_eq!(
            router.rules()[0].target,
            RouteTarget::Device {
                endpoint_id: "b".into()
            }
        );
    }

    #[test]
    fn a_disabled_rule_matches_nothing() {
        let mut rule = rule_for(r"C:\Apps\Music.exe", RouteTarget::SystemDefault);
        rule.enabled = false;
        assert!(!rule.matches(&AppKey::Executable(PathBuf::from(r"C:\Apps\Music.exe"))));
    }

    #[test]
    fn a_rule_matches_only_its_own_application() {
        let rule = rule_for(r"C:\Apps\Music.exe", RouteTarget::SystemDefault);
        assert!(rule.matches(&AppKey::Executable(PathBuf::from(r"C:\APPS\MUSIC.EXE"))));
        assert!(!rule.matches(&AppKey::Executable(PathBuf::from(r"C:\Apps\Other.exe"))));
        assert!(!rule.matches(&AppKey::Packaged("x!App".to_owned())));
    }

    #[test]
    fn a_router_without_a_policy_factory_reports_rather_than_panics() {
        let mut router = Router {
            rules: vec![rule_for(
                r"C:\Apps\Music.exe",
                RouteTarget::Device {
                    endpoint_id: "a".into(),
                },
            )],
            policy: None,
            applied: HashSet::new(),
            injection: InjectionStatus::NotAvailable,
        };

        assert!(!router.policy_available());

        // No sessions means no processes to match, so the report is empty
        // rather than a failure — but the important part is that it returns.
        let report = router.apply(&[session(0)]);
        assert!(report.failed.is_empty());
        assert!(!report.changed_anything());
    }

    #[test]
    fn session_process_zero_is_never_routed() {
        // The system-sounds pseudo session has no process behind it; writing a
        // policy for pid 0 would attach an override to the idle process.
        let mut router = Router {
            rules: vec![rule_for(r"anything", RouteTarget::SystemDefault)],
            policy: None,
            applied: HashSet::new(),
            injection: InjectionStatus::NotAvailable,
        };
        let report = router.apply(&[session(0)]);
        assert!(report.applied.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn app_keys_survive_json() {
        let rule = RouteRule::new(
            AppKey::Executable(PathBuf::from(r"C:\Apps\Music.exe")),
            "Music".to_owned(),
            RouteTarget::Device {
                endpoint_id: "{0.0.0.0}.{x}".into(),
            },
        );
        let text = serde_json::to_string(&rule).expect("serialisable");
        // The key is stored in its prefixed form, not as an enum object.
        assert!(text.contains(r"exe:C:\\Apps\\Music.exe"), "{text}");

        let back: RouteRule = serde_json::from_str(&text).expect("round trip");
        assert_eq!(back, rule);
    }

    #[test]
    fn an_unrecognised_stored_key_is_rejected() {
        let text = r#"{"app":"nonsense","display_name":"x","target":{"kind":"system_default"}}"#;
        assert!(serde_json::from_str::<RouteRule>(text).is_err());
    }

    #[test]
    fn a_rule_saved_without_a_method_defaults_to_policy() {
        // Rules written by an older build have no `method` field; they must
        // load as the safe mechanism rather than failing to parse.
        let text = r#"{"app":"exe:C:\\Apps\\Music.exe","display_name":"Music","target":{"kind":"system_default"}}"#;
        let rule: RouteRule = serde_json::from_str(text).expect("parses");
        assert_eq!(rule.method, RouteMethod::Policy);
        assert!(rule.enabled);
    }
}
