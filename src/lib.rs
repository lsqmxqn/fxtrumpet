//! FxTrumpet — a tray-resident audio hub.
//!
//! One tray icon, three capabilities that used to be three programs:
//!
//! | capability | inherited from | module |
//! |---|---|---|
//! | FxSound-grade audio enhancement | FxMini (itself built on FxSound's own DSP) | [`engine`], [`ffi`], [`preset`], [`routing`], [`driver`] |
//! | per-application volume mixer and device switching | EarTrumpet | [`session`], [`process`], [`perapp`] |
//! | per-application output routing | Audio Router | [`router`], [`perapp`] |
//!
//! The audio DSP is not reimplemented: it is FxSound's own engine, vendored
//! under `vendor/dsp` and compiled into a static library by `build.rs`, then
//! reached through the C ABI in `capi/dfxdsp_capi.h`.
//!
//! Module map:
//!
//! | module | job |
//! |---|---|
//! | [`ffi`] | the C ABI, plus a thin RAII wrapper over the engine |
//! | [`config`] | settings and the `%APPDATA%\FxTrumpet` paths |
//! | [`preset`] | `.fac` parsing, the embedded library, the preset folder |
//! | [`device`] | endpoint discovery, default-device switching, hot-plug events |
//! | [`engine`] | the audio thread: loopback capture → DSP → render |
//! | [`driver`] | detecting, installing and removing the virtual sound card |
//! | [`autostart`] | the `HKCU\...\Run` entry |
//! | [`session`] | audio sessions per endpoint — the mixer's data |
//! | [`endpoint`] | a render endpoint's own volume, mute and level meter |
//! | [`process`] | process id → application identity, icon and stable key |
//! | [`perapp`] | Windows' per-application default endpoint (no elevation) |
//! | [`router`] | routing rules: which application goes to which device |
//! | [`i18n`] | every user-visible string, in Chinese and English |
//! | [`ui`] | the tray icon, the tuning panel and the mixer |
//! | [`app`] | wiring between the tray, the engine, the router and the config |
//!
//! The binaries in `src/bin` are milestone smoke tests; the shipped application
//! is `src/main.rs`.

pub mod app;
pub mod autostart;
pub mod config;
pub mod device;
pub mod driver;
pub mod endpoint;
pub mod engine;
pub mod ffi;
pub mod i18n;
pub mod perapp;
pub mod preset;
pub mod process;
pub mod router;
pub mod routing;
pub mod session;
pub mod ui;

pub use config::Config;
pub use engine::{AudioEngine, EngineHandle, EngineStatus};
pub use ffi::Dsp;
pub use preset::{FacPreset, PresetEntry};
