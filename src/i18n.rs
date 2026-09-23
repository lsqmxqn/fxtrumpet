//! User-visible text, in Chinese and English.
//!
//! ## Why a struct of `&'static str` and not a lookup table
//!
//! Both languages are defined as one `const` per language, typed as [`Strings`].
//! A missing translation is then a *compile* error rather than a blank label or
//! a runtime fallback, and there is no key string to typo. The cost is that
//! adding a string touches both constants — which is the point.
//!
//! ## Why the language is a process-wide atomic
//!
//! The tray menu is built on the main thread; the tuning panel renders on its
//! own thread. Both have to agree on the language, and neither owns the other.
//! One relaxed atomic read per frame is cheaper than plumbing a language
//! through every call site, and it makes a switch take effect in an already-open
//! panel on its next repaint with no message passing — the same reason
//! [`crate::engine::SharedParams`] is made of atomics.
//!
//! ## What is deliberately *not* translated
//!
//! The log file, and anything the engine or a driver API hands back. Logs stay
//! in English so that a user reporting a problem can be asked for `fxtrumpet.log`
//! and the lines mean the same thing no matter what the UI is set to. Also
//! untranslated: `dB`, `Hz` and `kHz`, which are symbols rather than words.
//!
//! ## Adding a language
//!
//! Add a variant to [`Lang`], a `const` for its [`Strings`], and the arm in
//! [`Lang::table`]. Everything else follows from the compiler complaining.

use std::sync::atomic::{AtomicU8, Ordering};

/// A language the interface can be drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

/// The config spelling that means "ask the system".
///
/// `"auto"` is deliberately not a [`Lang`]: it is an instruction, and it is
/// resolved once at startup by [`resolve`].
pub const AUTO: &str = "auto";

impl Lang {
    /// Every language, in the order the switcher lists them.
    pub const ALL: [Lang; 2] = [Lang::Zh, Lang::En];

    /// The config-file spelling.
    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// A language's name *in that language*, for the switcher.
    ///
    /// Deliberately not translated: someone who has landed in the wrong
    /// language is looking for the word they recognise, and "Chinese" does not
    /// help a reader who only knows 中文.
    pub fn endonym(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "English",
        }
    }

    /// Parses a config-file spelling. `None` for anything unrecognised,
    /// including `"auto"`.
    pub fn from_code(code: &str) -> Option<Self> {
        match code.trim().to_ascii_lowercase().as_str() {
            "zh" | "cn" | "zh-cn" | "zh-hans" => Some(Lang::Zh),
            "en" | "en-us" | "en-gb" => Some(Lang::En),
            _ => None,
        }
    }

    /// Whether the config asks for this language to be chosen automatically.
    pub fn is_auto(code: &str) -> bool {
        code.trim().eq_ignore_ascii_case(AUTO)
    }

    /// What the operating system is set to.
    ///
    /// `GetUserDefaultUILanguage` is the language of the *interface*, not the
    /// locale — a user in Shanghai running an English Windows wants English,
    /// and this is the API that says so. Any Chinese variant (Simplified,
    /// Traditional, either region) maps to [`Lang::Zh`]; the strings here are
    /// Simplified, which a Traditional reader can also read.
    pub fn from_system() -> Self {
        use windows::Win32::Globalization::GetUserDefaultUILanguage;

        // The low byte of a LANGID is the primary language id, where 0x04 is
        // Chinese. Masking rather than comparing whole values is what covers
        // all eight Chinese sublanguages without a lookup table.
        let langid = unsafe { GetUserDefaultUILanguage() };
        if (langid & 0x00FF) == 0x04 {
            Lang::Zh
        } else {
            Lang::En
        }
    }

    /// This language's strings.
    pub fn table(self) -> &'static Strings {
        match self {
            Lang::Zh => &ZH,
            Lang::En => &EN,
        }
    }
}

/// The process-wide language, stored as an index.
///
/// An index rather than the enum's discriminant so that inserting a variant in
/// the middle of [`Lang`] cannot silently change what a previously stored byte
/// means. `index_of`/`lang_of` are the only places that mapping lives.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Sets the language for the whole process.
///
/// Takes effect immediately in the panel, which re-reads it every frame, and
/// after the tray menu is rebuilt — see `app::App::set_language`, which does
/// both.
pub fn set(lang: Lang) {
    CURRENT.store(index_of(lang), Ordering::Relaxed);
}

/// The current language.
pub fn current() -> Lang {
    lang_of(CURRENT.load(Ordering::Relaxed))
}

/// The current language's strings. The one call most code makes.
pub fn t() -> &'static Strings {
    current().table()
}

/// Turns a config value into a language, consulting the system for `"auto"`.
///
/// An unrecognised value is treated as `"auto"` rather than as an error: a
/// hand-edited config with a typo in it should still show *a* language, not
/// refuse to start.
pub fn resolve(config_code: &str) -> Lang {
    Lang::from_code(config_code).unwrap_or_else(Lang::from_system)
}

fn index_of(lang: Lang) -> u8 {
    match lang {
        Lang::Zh => 0,
        Lang::En => 1,
    }
}

fn lang_of(index: u8) -> Lang {
    match index {
        1 => Lang::En,
        _ => Lang::Zh,
    }
}

/// Every string the interface can show.
pub struct Strings {
    pub tray: TrayText,
    pub panel: PanelText,
    pub mixer: MixerText,
}

/// The tray menu.
///
/// Kept terse on purpose: the menu is scanned rather than read, and it is drawn
/// against the screen edge where a long label gets clipped rather than wrapped.
/// The entries are single-language — the bilingual "启用音效 / Enabled" labels
/// this replaced were twice as wide as they needed to be and still only told
/// half the users what they meant.
pub struct TrayText {
    pub enabled: &'static str,
    pub route_through: &'static str,
    /// Shown *instead of* [`Self::route_through`] once audio is already going
    /// through the virtual card.
    ///
    /// The entry used to be greyed out in that state, which reads as "you
    /// cannot use this" — indistinguishable from a broken install. Naming the
    /// state instead means the disabled item answers the question the user came
    /// to ask, which is "is my audio actually being enhanced?".
    pub route_through_done: &'static str,
    pub panel: &'static str,
    /// Opens the mixer and routing window.
    pub mixer: &'static str,
    pub presets: &'static str,
    pub no_presets: &'static str,
    pub autostart: &'static str,
    pub install_driver: &'static str,
    pub remove_driver: &'static str,
    pub rescan: &'static str,
    pub language: &'static str,
    pub quit: &'static str,

    /// Tooltip fragments; composed by [`TrayText::tooltip_playing`].
    pub no_preset: &'static str,
    pub not_enhanced: &'static str,
    pub tooltip_idle: &'static str,
    pub tooltip_disabled: &'static str,
    pub tooltip_no_card: &'static str,
}

impl TrayText {
    /// The tray tooltip while audio is being processed, e.g.
    /// `FxTrumpet — 音乐 — 输出未经增强（默认设备不是虚拟声卡）`.
    ///
    /// Composed here rather than in `app` so the separators and the sentence
    /// order stay next to the words they glue together.
    pub fn tooltip_playing(
        &self,
        preset: Option<&str>,
        error: Option<&str>,
        not_enhanced: bool,
    ) -> String {
        let mut text = format!("FxTrumpet — {}", preset.unwrap_or(self.no_preset));
        if let Some(error) = error {
            text.push_str(" (");
            text.push_str(error);
            text.push(')');
        }
        if not_enhanced {
            text.push_str(" — ");
            text.push_str(self.not_enhanced);
        }
        text
    }
}

/// The mixer and routing window.
///
/// A third surface rather than a tab on the tuning panel. The two do different
/// jobs — one shapes the sound, the other decides where sound goes — and the
/// tuning panel is already sized so its whole content fits on one screen; a tab
/// strip would have made both halves scroll.
pub struct MixerText {
    pub title: &'static str,
    pub devices: &'static str,
    pub apps: &'static str,
    pub routing: &'static str,

    /// The "no explicit destination" entry in an application's device list.
    pub follow_system: &'static str,
    /// Shown next to an application whose audio bypasses the enhancer.
    pub bypasses: &'static str,
    pub set_default: &'static str,
    pub is_default: &'static str,
    /// Labels the enhancer's own virtual card in the device list.
    pub virtual_card: &'static str,
    /// The row Windows has for sounds that belong to no application.
    pub system_sounds: &'static str,

    pub no_devices: &'static str,
    pub no_apps: &'static str,
    pub no_routes: &'static str,

    /// Explains that the cheap routing mechanism is unavailable on this build.
    pub policy_unavailable: &'static str,
    /// The mechanism column in the rule list.
    pub method_policy: &'static str,
    pub method_injection: &'static str,
    /// Why an injection rule is not doing anything yet.
    pub injection_unavailable: &'static str,
    /// Shown once under the rule list, explaining what rules do.
    pub routing_hint: &'static str,

    pub remove: &'static str,
    pub mute: &'static str,
    pub unmute: &'static str,

    /// The noun after the count, e.g. `条流` / `streams`. Both languages put it
    /// after the number, which is why this is a suffix and not a format string.
    pub streams_suffix: &'static str,
}

impl MixerText {
    /// `" · 3 条流"` / `" · 3 streams"`, or empty when an application has only
    /// one stream and there is nothing to disambiguate.
    pub fn streams(&self, n: usize) -> String {
        if n <= 1 {
            String::new()
        } else {
            format!(" · {n} {}", self.streams_suffix)
        }
    }

    /// The label on a rule's mechanism badge.
    pub fn method_label(&self, method: crate::router::RouteMethod) -> &'static str {
        match method {
            crate::router::RouteMethod::Policy => self.method_policy,
            crate::router::RouteMethod::Injection => self.method_injection,
        }
    }
}

/// The tuning panel.
pub struct PanelText {
    pub enabled: &'static str,
    pub status_processing: &'static str,
    pub status_idle: &'static str,
    pub status_no_card: &'static str,

    pub preset: &'static str,
    pub no_selection: &'static str,

    pub effects: &'static str,
    pub effect_fidelity: &'static str,
    pub effect_surround: &'static str,
    pub effect_ambience: &'static str,
    pub effect_dynamic_boost: &'static str,
    pub effect_bass: &'static str,

    pub equalizer: &'static str,
    pub bands: &'static str,
    /// One line explaining the curve gesture. The curve is the panel's only
    /// control with no affordance borrowed from a widget the user has met
    /// before, so it says so rather than relying on discovery.
    pub eq_hint: &'static str,
    /// Button that flattens every band back to 0 dB.
    pub eq_reset: &'static str,

    pub output: &'static str,
    pub balance: &'static str,
    pub master_gain: &'static str,
    pub normalization: &'static str,
    pub volume_leveling: &'static str,
    pub filter_q: &'static str,

    pub spectrum: &'static str,

    pub save_name_hint: &'static str,
    pub save_button: &'static str,
    pub save_help: &'static str,
    /// The line the footer shows where a save result will appear, so the footer
    /// keeps its height and the reader learns where presets land.
    pub save_where: &'static str,

    /// Prefixes for the two device lines. Written out per language rather than
    /// left as `in:`/`out:`: the line is elided now, so the few pixels the
    /// longer word costs buy a reader who does not parse the abbreviations.
    pub device_in_prefix: &'static str,
    pub device_out_prefix: &'static str,

    /// Footer counters. The number is appended by the methods below.
    pub latency_label: &'static str,
    pub underruns_label: &'static str,
    pub drops_label: &'static str,
    /// Unit for the *exact* underrun figure, which is a count of frames.
    pub frames_unit: &'static str,

    /// Which scale [`PanelText::abbreviate`] should group large counters by.
    ///
    /// A property of the language, not of any one string, so it belongs in the
    /// table rather than being inferred from the text.
    pub groups_by_ten_thousand: bool,

    /// Save-result messages.
    pub saved_ok: &'static str,
    pub save_name_required: &'static str,
    pub save_name_invalid: &'static str,
    pub save_overwrote: &'static str,
    pub save_failed: &'static str,
}

impl PanelText {
    /// `输入  <device>`, or `输入  —` when there is nothing to report.
    pub fn input_line(&self, description: Option<&str>) -> String {
        self.device_line(self.device_in_prefix, description)
    }

    /// `输出  <device>`, or `输出  —`.
    pub fn output_line(&self, description: Option<&str>) -> String {
        self.device_line(self.device_out_prefix, description)
    }

    fn device_line(&self, label: &str, description: Option<&str>) -> String {
        format!("{label}  {}", description.unwrap_or("—"))
    }

    /// A frequency on the equalizer's axis: `31 Hz`, `1.0 kHz`.
    ///
    /// Shared by the curve's axis and its readout so the two cannot disagree
    /// about where a band is.
    pub fn frequency(&self, hz: f32) -> String {
        if hz >= 1000.0 {
            format!("{:.1} kHz", hz / 1000.0)
        } else {
            format!("{hz:.0} Hz")
        }
    }

    /// A gain with an explicit sign and one decimal, so a column of them is the
    /// same width and `+` versus `-` is visible at a glance.
    pub fn gain(&self, db: f32) -> String {
        // Rounded before the sign is chosen: a band sitting at -0.04 dB would
        // otherwise print "-0.0", which reads as a cut that is not there.
        let rounded = (db * 10.0).round() / 10.0;
        if rounded == 0.0 {
            "0.0".to_owned()
        } else {
            format!("{rounded:+.1}")
        }
    }

    /// A gain with its unit, for the curve readout.
    pub fn gain_db(&self, db: f32) -> String {
        format!("{} dB", self.gain(db))
    }

    pub fn latency(&self, ms: u32) -> String {
        format!("{} {ms} ms", self.latency_label)
    }

    /// The footer's underrun figure, abbreviated.
    ///
    /// The raw number is a **frame count**, which at 48 kHz reaches eight
    /// digits within minutes — the old footer printed `欠载 24302880`, which is
    /// a fact nobody can act on. [`Self::stats_detail`] carries the exact value
    /// for a hover tooltip.
    pub fn underruns(&self, frames: u64) -> String {
        format!("{} {}", self.underruns_label, self.abbreviate(frames))
    }

    pub fn drops(&self, count: u64) -> String {
        format!("{} {}", self.drops_label, self.abbreviate(count))
    }

    /// The same three numbers, unrounded, for the hover tooltip.
    pub fn stats_detail(&self, latency_ms: u32, underrun_frames: u64, drops: u64) -> String {
        format!(
            "{}\n{} {} {}\n{} {}",
            self.latency(latency_ms),
            self.underruns_label,
            underrun_frames,
            self.frames_unit,
            self.drops_label,
            drops
        )
    }

    /// Abbreviates a large counter using this language's own scale.
    ///
    /// Thousands groups differ by locale — English counts in K/M, Chinese in
    /// 万/亿 — so this cannot be one implementation shared by both tables.
    fn abbreviate(&self, n: u64) -> String {
        if self.groups_by_ten_thousand {
            if n >= 100_000_000 {
                format!("{:.2} 亿", n as f64 / 100_000_000.0)
            } else if n >= 10_000 {
                format!("{:.1} 万", n as f64 / 10_000.0)
            } else {
                n.to_string()
            }
        } else if n >= 1_000_000_000 {
            format!("{:.1}B", n as f64 / 1_000_000_000.0)
        } else if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 10_000 {
            format!("{:.1}K", n as f64 / 1000.0)
        } else {
            n.to_string()
        }
    }

    /// The message shown after a save, given the file that was written and
    /// whether it replaced something.
    pub fn saved(&self, filename: &str, overwrote: bool) -> String {
        let template = if overwrote {
            self.save_overwrote
        } else {
            self.saved_ok
        };
        template.replace("{}", filename)
    }
}

/// Chinese (Simplified).
static ZH: Strings = Strings {
    tray: TrayText {
        enabled: "启用音效",
        route_through: "输出走 FxTrumpet",
        route_through_done: "输出已走 FxTrumpet",
        panel: "调音面板…",
        mixer: "混音器与路由…",
        presets: "预设",
        no_presets: "（无预设）",
        autostart: "开机自启",
        install_driver: "安装虚拟声卡…",
        remove_driver: "卸载虚拟声卡",
        rescan: "重新扫描预设",
        language: "语言",
        quit: "退出",

        no_preset: "无预设",
        not_enhanced: "输出未经增强（默认设备不是虚拟声卡）",
        tooltip_idle: "FxTrumpet — 空闲",
        tooltip_disabled: "FxTrumpet — 已停用",
        tooltip_no_card: "FxTrumpet — 未安装虚拟声卡",
    },
    mixer: MixerText {
        title: "混音器与路由",
        devices: "输出设备",
        apps: "应用程序",
        routing: "路由规则",

        follow_system: "跟随系统（经过增强）",
        bypasses: "绕过增强",
        set_default: "设为默认",
        is_default: "当前默认",
        virtual_card: "虚拟声卡",
        system_sounds: "系统声音",

        no_devices: "没有可用的输出设备",
        no_apps: "当前没有应用在播放声音",
        no_routes: "暂无规则。在下方把某个应用指到指定设备即可建立规则。",

        policy_unavailable: "此系统不支持按应用路由（需要 Windows 10 1803 及以上）",
        method_policy: "系统策略",
        method_injection: "注入",
        injection_unavailable: "注入式路由尚未构建进来，该规则会退回系统策略",
        routing_hint: "「跟随系统」的应用走虚拟声卡，即经过增强；指到具体设备的应用会绕过增强。",

        remove: "删除",
        mute: "静音",
        unmute: "取消静音",

        streams_suffix: "条流",
    },
    panel: PanelText {
        enabled: "启用",
        status_processing: "处理中",
        status_idle: "空闲",
        status_no_card: "未安装虚拟声卡",

        preset: "预设",
        no_selection: "—",

        effects: "音效",
        effect_fidelity: "保真度",
        effect_surround: "环绕",
        effect_ambience: "空间感",
        effect_dynamic_boost: "动态增强",
        effect_bass: "低音",

        equalizer: "均衡器",
        bands: "频段数",
        eq_hint: "拖动圆点调增益，双击归零",
        eq_reset: "归零",

        output: "输出",
        balance: "左右平衡",
        master_gain: "总增益",
        normalization: "响度归一",
        volume_leveling: "音量均衡",
        filter_q: "滤波器 Q 值",

        spectrum: "频谱",

        save_name_hint: "预设名称",
        save_button: "保存",
        save_help: "把当前的音效、均衡与输出设置存成一个 .fac 文件，之后可在托盘菜单或上方列表中选用。",
        save_where: "存入 %APPDATA%\\FxTrumpet\\presets",
        device_in_prefix: "输入",
        device_out_prefix: "输出",
        latency_label: "延迟",
        underruns_label: "欠载",
        drops_label: "丢帧",
        frames_unit: "帧",
        groups_by_ten_thousand: true,

        saved_ok: "已保存 {}",
        save_name_required: "请先填写预设名称",
        save_name_invalid: "名称含文件名不允许的字符（\\ / : * ? \" < > |），或与系统保留名重名",
        save_overwrote: "已覆盖 {}",
        save_failed: "保存失败，详见日志",
    },
};

/// English.
static EN: Strings = Strings {
    tray: TrayText {
        enabled: "Enabled",
        route_through: "Route output via FxTrumpet",
        route_through_done: "Output routed via FxTrumpet",
        panel: "Tuning panel…",
        mixer: "Mixer & routing…",
        presets: "Presets",
        no_presets: "(none found)",
        autostart: "Start with Windows",
        install_driver: "Install sound card…",
        remove_driver: "Remove sound card",
        rescan: "Rescan presets",
        language: "Language",
        quit: "Quit",

        no_preset: "no preset",
        not_enhanced: "output is NOT enhanced (default device is not the virtual card)",
        tooltip_idle: "FxTrumpet — idle",
        tooltip_disabled: "FxTrumpet — disabled",
        tooltip_no_card: "FxTrumpet — virtual sound card not installed",
    },
    mixer: MixerText {
        title: "Mixer & routing",
        devices: "Output devices",
        apps: "Applications",
        routing: "Routing rules",

        follow_system: "Follow system (enhanced)",
        bypasses: "bypasses enhancement",
        set_default: "Make default",
        is_default: "Default",
        virtual_card: "virtual card",
        system_sounds: "System Sounds",

        no_devices: "No output devices available",
        no_apps: "No application is playing audio",
        no_routes: "No rules yet. Point an application at a device below to create one.",

        policy_unavailable: "Per-application routing is unavailable on this build (needs Windows 10 1803 or later)",
        method_policy: "system policy",
        method_injection: "injection",
        injection_unavailable: "injection routing is not built in yet; this rule falls back to the system policy",
        routing_hint: "An application on \"follow system\" goes through the virtual card, i.e. enhanced. One pointed at a device bypasses the enhancer.",

        remove: "Remove",
        mute: "Mute",
        unmute: "Unmute",

        streams_suffix: "streams",
    },
    panel: PanelText {
        enabled: "Enabled",
        status_processing: "processing",
        status_idle: "idle",
        status_no_card: "no virtual sound card",

        preset: "Preset",
        no_selection: "—",

        effects: "Effects",
        effect_fidelity: "Fidelity",
        effect_surround: "Surround",
        effect_ambience: "Ambience",
        effect_dynamic_boost: "Dynamic boost",
        effect_bass: "Bass",

        equalizer: "Equalizer",
        bands: "Bands",
        eq_hint: "Drag a dot to set its gain; double-click to reset it",
        eq_reset: "Flatten",

        output: "Output",
        balance: "Balance",
        master_gain: "Master gain",
        normalization: "Normalization",
        volume_leveling: "Volume leveling",
        filter_q: "Filter Q",

        spectrum: "Spectrum",

        save_name_hint: "Preset name",
        save_button: "Save",
        save_help: "Stores the current effects, equalizer and output settings as a .fac file, \
                    selectable from the tray menu or the list above.",
        save_where: "Saved into %APPDATA%\\FxTrumpet\\presets",
        device_in_prefix: "In",
        device_out_prefix: "Out",
        latency_label: "latency",
        underruns_label: "underruns",
        drops_label: "drops",
        frames_unit: "frames",
        groups_by_ten_thousand: false,

        saved_ok: "Saved {}",
        save_name_required: "Give the preset a name first",
        save_name_invalid: "The name has a character Windows forbids in a filename \
                            (\\ / : * ? \" < > |), or collides with a reserved device name",
        save_overwrote: "Overwrote {}",
        save_failed: "Could not save; see the log",
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_codes_round_trip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
    }

    #[test]
    fn aliases_and_junk() {
        assert_eq!(Lang::from_code("ZH-CN"), Some(Lang::Zh));
        assert_eq!(Lang::from_code(" en "), Some(Lang::En));
        // `auto` must not resolve to a language here — that is `resolve`'s job.
        assert_eq!(Lang::from_code(AUTO), None);
        assert!(Lang::is_auto(AUTO));
        assert!(Lang::is_auto(" Auto "));
        assert!(!Lang::is_auto("zh"));
    }

    #[test]
    fn every_string_is_present_in_both_languages() {
        // The types make a *missing* field impossible; this catches the other
        // mistake, a field left as an empty string because a translation was
        // forgotten.
        for lang in Lang::ALL {
            let strings = lang.table();
            let tray = &strings.tray;
            for (name, value) in [
                ("enabled", tray.enabled),
                ("route_through", tray.route_through),
                ("route_through_done", tray.route_through_done),
                ("panel", tray.panel),
                ("mixer", tray.mixer),
                ("presets", tray.presets),
                ("no_presets", tray.no_presets),
                ("autostart", tray.autostart),
                ("install_driver", tray.install_driver),
                ("remove_driver", tray.remove_driver),
                ("rescan", tray.rescan),
                ("language", tray.language),
                ("quit", tray.quit),
                ("no_preset", tray.no_preset),
                ("not_enhanced", tray.not_enhanced),
                ("tooltip_idle", tray.tooltip_idle),
                ("tooltip_disabled", tray.tooltip_disabled),
                ("tooltip_no_card", tray.tooltip_no_card),
            ] {
                assert!(!value.trim().is_empty(), "{} tray.{name} is empty", lang.code());
            }

            let panel = &strings.panel;
            for (name, value) in [
                ("enabled", panel.enabled),
                ("status_processing", panel.status_processing),
                ("status_idle", panel.status_idle),
                ("status_no_card", panel.status_no_card),
                ("preset", panel.preset),
                ("no_selection", panel.no_selection),
                ("effects", panel.effects),
                ("effect_fidelity", panel.effect_fidelity),
                ("effect_surround", panel.effect_surround),
                ("effect_ambience", panel.effect_ambience),
                ("effect_dynamic_boost", panel.effect_dynamic_boost),
                ("effect_bass", panel.effect_bass),
                ("equalizer", panel.equalizer),
                ("bands", panel.bands),
                ("eq_hint", panel.eq_hint),
                ("eq_reset", panel.eq_reset),
                ("output", panel.output),
                ("balance", panel.balance),
                ("master_gain", panel.master_gain),
                ("normalization", panel.normalization),
                ("volume_leveling", panel.volume_leveling),
                ("filter_q", panel.filter_q),
                ("spectrum", panel.spectrum),
                ("save_name_hint", panel.save_name_hint),
                ("save_button", panel.save_button),
                ("save_help", panel.save_help),
                ("save_where", panel.save_where),
                ("device_in_prefix", panel.device_in_prefix),
                ("device_out_prefix", panel.device_out_prefix),
                ("latency_label", panel.latency_label),
                ("underruns_label", panel.underruns_label),
                ("drops_label", panel.drops_label),
                ("frames_unit", panel.frames_unit),
                ("saved_ok", panel.saved_ok),
                ("save_name_required", panel.save_name_required),
                ("save_name_invalid", panel.save_name_invalid),
                ("save_overwrote", panel.save_overwrote),
                ("save_failed", panel.save_failed),
            ] {
                assert!(!value.trim().is_empty(), "{} panel.{name} is empty", lang.code());
            }

            // The mixer's words. `virtual_card` and `system_sounds` are in the
            // list because they are what a user reads when deciding which
            // device a row is pointed at, and an empty one there would leave a
            // pill with no label rather than a visibly missing translation.
            let mixer = &strings.mixer;
            for (name, value) in [
                ("title", mixer.title),
                ("devices", mixer.devices),
                ("apps", mixer.apps),
                ("routing", mixer.routing),
                ("follow_system", mixer.follow_system),
                ("bypasses", mixer.bypasses),
                ("set_default", mixer.set_default),
                ("is_default", mixer.is_default),
                ("virtual_card", mixer.virtual_card),
                ("system_sounds", mixer.system_sounds),
                ("no_devices", mixer.no_devices),
                ("no_apps", mixer.no_apps),
                ("no_routes", mixer.no_routes),
                ("policy_unavailable", mixer.policy_unavailable),
                ("method_policy", mixer.method_policy),
                ("method_injection", mixer.method_injection),
                ("injection_unavailable", mixer.injection_unavailable),
                ("routing_hint", mixer.routing_hint),
                ("remove", mixer.remove),
                ("mute", mixer.mute),
                ("unmute", mixer.unmute),
                ("streams_suffix", mixer.streams_suffix),
            ] {
                assert!(!value.trim().is_empty(), "{} mixer.{name} is empty", lang.code());
            }
        }
    }

    #[test]
    fn the_save_messages_actually_interpolate() {
        // `PanelText::saved` substitutes by hand because a format template
        // cannot be a runtime value, so a template that lost its `{}` would
        // silently drop the filename.
        for lang in Lang::ALL {
            let panel = &lang.table().panel;
            for template in [panel.saved_ok, panel.save_overwrote] {
                assert!(
                    template.contains("{}"),
                    "{}: {template:?} has no placeholder",
                    lang.code()
                );
            }
            assert!(panel.saved("x.fac", false).contains("x.fac"));
            assert!(panel.saved("x.fac", true).contains("x.fac"));
        }
    }

    #[test]
    fn selecting_a_language_is_visible_through_t() {
        // Global state, so restore whatever was there: other tests in this
        // binary share the process.
        let before = current();
        set(Lang::En);
        assert_eq!(current(), Lang::En);
        assert_eq!(t().tray.quit, "Quit");
        set(Lang::Zh);
        assert_eq!(current(), Lang::Zh);
        assert_eq!(t().tray.quit, "退出");
        set(before);
    }

    #[test]
    fn tooltip_composition_covers_all_four_cases() {
        let text = &ZH.tray;
        assert_eq!(
            text.tooltip_playing(Some("音乐"), None, false),
            "FxTrumpet — 音乐"
        );
        assert_eq!(
            text.tooltip_playing(None, None, false),
            "FxTrumpet — 无预设"
        );
        assert_eq!(
            text.tooltip_playing(None, Some("boom"), false),
            "FxTrumpet — 无预设 (boom)"
        );
        assert_eq!(
            text.tooltip_playing(Some("音乐"), None, true),
            "FxTrumpet — 音乐 — 输出未经增强（默认设备不是虚拟声卡）"
        );
    }

    #[test]
    fn device_lines_fall_back_to_a_dash() {
        assert_eq!(ZH.panel.input_line(None), "输入  —");
        assert_eq!(EN.panel.output_line(Some("Speakers")), "Out  Speakers");
        assert_eq!(ZH.panel.latency(120), "延迟 120 ms");
        assert_eq!(EN.panel.underruns(3), "underruns 3");
    }

    /// The underrun counter is a frame count, which is why it needs this: the
    /// old footer printed `欠载 24302880`, eight digits of a unit nobody
    /// reasons in. Both languages group digits their own way, so the scale has
    /// to come from the table rather than be shared.
    #[test]
    fn large_counters_are_abbreviated_in_the_languages_own_scale() {
        assert_eq!(ZH.panel.underruns(0), "欠载 0");
        assert_eq!(ZH.panel.underruns(9_999), "欠载 9999");
        assert_eq!(ZH.panel.underruns(24_302_880), "欠载 2430.3 万");
        assert_eq!(ZH.panel.underruns(250_000_000), "欠载 2.50 亿");

        assert_eq!(EN.panel.underruns(0), "underruns 0");
        assert_eq!(EN.panel.underruns(9_999), "underruns 9999");
        assert_eq!(EN.panel.underruns(24_302_880), "underruns 24.3M");
        assert_eq!(EN.panel.underruns(2_500_000_000), "underruns 2.5B");
    }

    /// Abbreviating the footer is only acceptable because the exact figure is
    /// still reachable, so the tooltip has to carry it.
    #[test]
    fn the_detail_tooltip_keeps_the_exact_figure() {
        let detail = ZH.panel.stats_detail(12, 24_302_880, 2);
        assert!(detail.contains("24302880"), "{detail}");
        assert!(detail.contains("延迟 12 ms"), "{detail}");
        assert!(detail.contains("丢帧 2"), "{detail}");
    }

    /// The curve's axis and its readout both format a frequency; if they ever
    /// disagreed a band's label would not match the dot under the pointer.
    #[test]
    fn frequency_and_gain_formatting_is_shared() {
        for text in [&ZH.panel, &EN.panel] {
            assert_eq!(text.frequency(31.0), "31 Hz");
            assert_eq!(text.frequency(999.0), "999 Hz");
            assert_eq!(text.frequency(1000.0), "1.0 kHz");
            assert_eq!(text.frequency(16_000.0), "16.0 kHz");

            assert_eq!(text.gain(3.0), "+3.0");
            assert_eq!(text.gain(-1.3), "-1.3");
            assert_eq!(text.gain_db(3.0), "+3.0 dB");
        }
    }

    /// A band parked a hair below zero must not read as a cut.
    #[test]
    fn a_negligible_gain_prints_without_a_sign() {
        for snap in [0.0, -0.0, 0.04, -0.04] {
            assert_eq!(ZH.panel.gain(snap), "0.0", "{snap} should print unsigned");
        }
        // …but a tenth of a decibel in either direction is a real setting.
        assert_eq!(ZH.panel.gain(0.1), "+0.1");
        assert_eq!(ZH.panel.gain(-0.1), "-0.1");
    }
}
