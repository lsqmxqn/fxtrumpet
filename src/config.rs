//! Persistent settings and the well-known paths FxTrumpet owns.
//!
//! Everything lives under `%APPDATA%\FxTrumpet`. The file is plain JSON so a user
//! can hand-edit it, and every field has a default so a partial (or corrupt)
//! file still loads — losing settings is never a reason to refuse to start.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Directory name under `%APPDATA%`.
pub const APP_DIR_NAME: &str = "FxTrumpet";

/// Default inter-stream buffer. Large enough to absorb the clock drift between
/// two independent audio devices for a while, small enough that a pause in the
/// source is still felt as prompt.
pub const DEFAULT_BUFFER_MS: u32 = 120;

/// Settings persisted across runs.
///
/// All fields default, so `serde` can fill in anything missing. That matters:
/// a config written by an older build must keep working after an upgrade adds
/// fields, and a truncated file must not brick the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Master switch, mirroring the tray "Enabled" item.
    pub enabled: bool,

    /// Whether the app registers itself to start with Windows.
    pub autostart: bool,

    /// Endpoint id captured for replay. `None` means "decide at startup":
    /// prefer the FxSound virtual card, fall back to the system default.
    pub source_device_id: Option<String>,

    /// Where processed audio is rendered. `None` means "the device that was
    /// the system default before FxTrumpet took over".
    pub sink_device_id: Option<String>,

    /// Whether to point the system's default output at the virtual sound card
    /// while running, so audio is actually routed through the enhancer, and to
    /// put it back on exit.
    ///
    /// On by default, and it has to be: with the default output left on the
    /// physical card, FxTrumpet captures a device nothing is playing into and
    /// processes silence - it looks perfectly healthy and does nothing. See
    /// [`crate::routing`].
    pub take_over_default: bool,

    /// The endpoint the takeover displaced.
    ///
    /// Written *before* the switch and cleared only after a clean restore, so a
    /// non-empty value on start means a previous run was killed while holding
    /// the default output. That is the machine's only way back to sound, which
    /// is why it is persisted rather than kept in memory.
    pub previous_default_id: Option<String>,

    /// Path of the preset to activate on startup.
    pub active_preset: Option<String>,

    /// EQ resolution handed to the engine. One of 5/10/15/20/31.
    pub num_bands: usize,

    /// Inter-stream buffer depth in milliseconds.
    pub buffer_ms: u32,

    /// When the two ends disagree on sample rate, resample instead of
    /// bypassing. See `engine::Resampler`.
    pub resample_on_rate_mismatch: bool,

    /// Skip DSP entirely for mono endpoints. Upstream's engine misbehaves on
    /// them (see the 2016 Bluetooth patch in AudioPassthruPrivate.cpp:545).
    pub bypass_mono_devices: bool,

    /// Log verbosity: "off" | "error" | "warn" | "info" | "debug" | "trace".
    pub log_level: String,

    /// Interface language: "auto" | "zh" | "en".
    ///
    /// `"auto"` defers to the Windows UI language, and it is stored as a
    /// *string* rather than as a resolved value so that a user who set it
    /// explicitly keeps that choice, while a user who never touched it follows
    /// their system when they change it. See [`crate::i18n::resolve`].
    pub language: String,

    /// Per-application output routing.
    ///
    /// Each rule is keyed by a stable application identity — an executable path
    /// or an Application User Model ID — never by process id. An empty list is
    /// the normal state: routing is opt-in, and an application with no rule
    /// follows the system default, which is the enhanced path.
    ///
    /// See [`crate::router`] for the two mechanisms a rule can use.
    #[serde(default)]
    pub routes: Vec<crate::router::RouteRule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            autostart: true,
            source_device_id: None,
            sink_device_id: None,
            take_over_default: true,
            previous_default_id: None,
            active_preset: None,
            num_bands: 10,
            buffer_ms: DEFAULT_BUFFER_MS,
            resample_on_rate_mismatch: true,
            bypass_mono_devices: true,
            log_level: "info".to_owned(),
            language: crate::i18n::AUTO.to_owned(),
            routes: Vec::new(),
        }
    }
}

impl Config {
    /// Reads the config, falling back to defaults for anything unreadable.
    ///
    /// Returns the config and, when a file existed but could not be used, the
    /// parse error so the caller can log it. Never fails: an unstartable app is
    /// worse than a defaulted one.
    pub fn load_or_default() -> (Self, Option<String>) {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(cfg) => (cfg, None),
                Err(err) => (
                    Config::default(),
                    Some(format!("config at {} is not valid JSON: {err}", path.display())),
                ),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
            Err(err) => (
                Config::default(),
                Some(format!("config at {} could not be read: {err}", path.display())),
            ),
        }
    }

    pub fn load() -> Self {
        Self::load_or_default().0
    }

    /// Writes the config, creating the directory if needed.
    ///
    /// Written through a temporary file and renamed so a crash mid-write cannot
    /// leave a half-serialised config behind.
    pub fn save(&self) -> std::io::Result<()> {
        let dir = app_data_dir();
        std::fs::create_dir_all(&dir)?;

        let text = serde_json::to_string_pretty(self)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        let final_path = config_path();
        let temp_path = final_path.with_extension("json.tmp");
        std::fs::write(&temp_path, text)?;
        // Windows rename fails if the destination exists, so drop it first.
        // This is a tiny non-atomic window, but the alternative (leaving no
        // config at all) is strictly worse.
        let _ = std::fs::remove_file(&final_path);
        std::fs::rename(&temp_path, &final_path)
    }
}

/// `%APPDATA%\FxTrumpet`, or the current directory if `APPDATA` is unset.
pub fn app_data_dir() -> PathBuf {
    match std::env::var_os("APPDATA") {
        Some(base) => Path::new(&base).join(APP_DIR_NAME),
        None => PathBuf::from(APP_DIR_NAME),
    }
}

/// `%APPDATA%\FxTrumpet\config.json`.
pub fn config_path() -> PathBuf {
    app_data_dir().join("config.json")
}

/// `%APPDATA%\FxTrumpet\presets` — where bundled presets are unpacked and where
/// user-supplied `.fac` files are picked up.
pub fn presets_dir() -> PathBuf {
    app_data_dir().join("presets")
}

/// `%APPDATA%\FxTrumpet\fxtrumpet.log`.
pub fn log_path() -> PathBuf {
    app_data_dir().join("fxtrumpet.log")
}
