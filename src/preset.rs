//! `.fac` presets: parsing, the embedded library, and the user's drop-in folder.
//!
//! Two independent reasons to parse `.fac` ourselves:
//!
//! 1. **Applying** a preset goes through the engine (`Dsp::load_preset`), which
//!    is authoritative. Nothing here replaces that.
//! 2. **Showing** one does not. To list presets in the tray, or to draw the EQ
//!    curve before the engine has ever seen the file, we need the name and the
//!    band table without mutating engine state. That is what this module is for.
//!
//! ## Format
//!
//! Plain text, UTF-8, CRLF, positional. Concretely, for `Music.fac`:
//!
//! ```text
//!  0  CLASS1 : Effect Type
//!  1  9: Version
//!  2  音乐                        <- the display name, verbatim
//!  3  0: Double Params Flag
//!  4  1: Total number of elements
//!  5  50: Main 0                  <- Fidelity
//!  6  35: Main 1                  <- Surround
//!  7   0: Main 2                  <- reserved, unused
//!  8  35: Main 3                  <- Ambience
//!  9  20: Main 4                  <- DynamicBoost
//! 10  60: Main 5                  <- Bass
//! ...
//! 29  10: Number of EQ Bands
//! 30   1: On/Off Flag
//! 31  Band 1
//! 32     62.5: CF
//! 33     0: Boost/Cut
//! ...
//! ```
//!
//! Rather than trust the line numbers, the parser walks the file looking for
//! the labelled sections. Line indices are a display convention, not a
//! contract, and the bundled presets are not all identical.

use std::path::{Path, PathBuf};

use crate::ffi::{self, DfxEffectId};

/// The six `Main` slots in a `.fac` file, in file order.
pub const NUM_MAIN_SLOTS: usize = 6;

/// `.fac` `Main` slot -> engine effect id.
///
/// **These are not the same numbering.** The file's `Main` index and
/// `DfxDsp::Effect` disagree for two of the five live slots — upstream maps
/// them through `DFXG_VALS_*_INDEX` (DfxDspPreset.cpp:24-28), where
/// `SURROUND_INDEX` is 1 and `AMBIENCE_INDEX` is 3, while the `Effect` enum is
/// `{ Fidelity=0, Ambience=1, Surround=2, DynamicBoost=3, Bass=4 }`.
///
/// Slot 2 is reserved and has no effect; it is `None` here so callers cannot
/// silently apply it to the wrong thing.
pub const MAIN_SLOT_TO_EFFECT: [Option<DfxEffectId>; NUM_MAIN_SLOTS] = [
    Some(ffi::DFX_EFFECT_FIDELITY),      // Main 0
    Some(ffi::DFX_EFFECT_SURROUND),      // Main 1  (note: enum value 2)
    None,                                // Main 2  reserved
    Some(ffi::DFX_EFFECT_AMBIENCE),      // Main 3  (note: enum value 1)
    Some(ffi::DFX_EFFECT_DYNAMIC_BOOST), // Main 4
    Some(ffi::DFX_EFFECT_BASS),          // Main 5
];

/// Human-readable label for each `Main` slot, in file order.
pub const MAIN_SLOT_LABELS: [&str; NUM_MAIN_SLOTS] = [
    "Fidelity",
    "Surround",
    "(reserved)",
    "Ambience",
    "DynamicBoost",
    "Bass",
];

/// One parsed `.fac` file.
#[derive(Debug, Clone, PartialEq)]
pub struct FacPreset {
    /// Where it came from.
    pub path: PathBuf,
    /// Display name, taken from line 2 verbatim.
    pub name: String,
    /// Format version (line 1). The bundled files all say 9.
    pub version: u32,
    /// The six `Main` values, 0..=127 (MIDI scale).
    pub mains: [i32; NUM_MAIN_SLOTS],
    /// Whether the EQ section is switched on.
    pub eq_on: bool,
    /// `(centre frequency Hz, boost/cut dB)` per band, in file order.
    pub bands: Vec<(f64, f64)>,
    /// The "application dependent" integers. Index 0..=4 are the per-effect
    /// enable flags in the same order as [`MAIN_SLOT_LABELS`] minus the
    /// reserved slot (see DfxDspPreset.cpp:32-38).
    pub app_flags: Vec<i32>,
}

impl FacPreset {
    /// The value a given effect would take, on the 0..=10 slider scale that
    /// [`ffi::Dsp::set_effect`] expects.
    ///
    /// The loader writes `Main / 127` into the engine's normalised field, so
    /// `Main / 12.7` is the inverse that lands back on the same value.
    pub fn effect_slider_value(&self, effect: DfxEffectId) -> Option<f64> {
        let slot = MAIN_SLOT_TO_EFFECT
            .iter()
            .position(|candidate| *candidate == Some(effect))?;
        Some(f64::from(self.mains[slot]) / 12.7)
    }

    /// Whether the effect's own on/off flag is set.
    pub fn effect_enabled(&self, effect: DfxEffectId) -> bool {
        let Some(slot) = MAIN_SLOT_TO_EFFECT
            .iter()
            .position(|candidate| *candidate == Some(effect))
        else {
            return false;
        };
        // The flag array omits the reserved slot, so the indices after it shift
        // down by one.
        let flag_index = if slot > 2 { slot - 1 } else { slot };
        self.app_flags
            .get(flag_index)
            .is_some_and(|flag| *flag != 0)
    }

    /// Reads and parses a `.fac` file.
    pub fn from_file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)?;
        Self::from_bytes(path, &bytes).map_err(|message| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, message)
        })
    }

    /// Parses `.fac` content already in memory.
    ///
    /// Accepts CRLF or LF, and tolerates a UTF-8 BOM. The name is decoded as
    /// UTF-8 — that is what the bundled files use, and what upstream's
    /// `pstrCovertUTF8StringToWideCharString_WithAlloc` expects
    /// (Valsfile.cpp:318).
    pub fn from_bytes(path: &Path, bytes: &[u8]) -> Result<Self, String> {
        let text = String::from_utf8_lossy(strip_bom(bytes)).into_owned();
        let lines: Vec<&str> = text
            .split('\n')
            .map(|line| line.trim_end_matches('\r'))
            .collect();

        let header = lines.first().copied().unwrap_or_default();
        if !header.starts_with("CLASS1") {
            return Err(format!(
                "not a CLASS1 preset file (first line was {header:?})"
            ));
        }

        let version = lines
            .get(1)
            .and_then(|line| leading_int(line))
            .unwrap_or(0);

        // Line 2 holds the name as-is. Some community presets pad it.
        let name = lines.get(2).copied().unwrap_or_default().trim().to_owned();

        let mut mains = [0i32; NUM_MAIN_SLOTS];
        for (slot, value) in mains.iter_mut().enumerate() {
            let label = format!("Main {slot}");
            *value = find_labeled_int(&lines, &label).unwrap_or(0);
        }

        let app_flags = parse_indexed_block(&lines, "Number of Application Dependent Integers")
            .map(|values| values.into_iter().map(|(_, value)| value).collect())
            .unwrap_or_default();

        let bands = parse_eq_bands(&lines);
        let eq_on = find_labeled_int(&lines, "On/Off Flag").unwrap_or(1) != 0;

        Ok(Self {
            path: path.to_path_buf(),
            name,
            version,
            mains,
            eq_on,
            bands,
            app_flags,
        })
    }

    /// A short "F/S/A/D/B" summary of the effect levels, handy in logs.
    pub fn effect_summary(&self) -> String {
        MAIN_SLOT_TO_EFFECT
            .iter()
            .zip(MAIN_SLOT_LABELS.iter())
            .zip(self.mains.iter())
            .filter(|((effect, _), _)| effect.is_some())
            .map(|((_, label), value)| format!("{label}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Strips a UTF-8 BOM if present.
fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// Parses the leading integer of a `"50: Main 0"`-style line.
fn leading_int(line: &str) -> Option<u32> {
    let digits: String = line.trim_start().chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Finds the first line whose label part is `label`, and returns its leading int.
fn find_labeled_int(lines: &[&str], label: &str) -> Option<i32> {
    lines.iter().find_map(|line| {
        let (value, text) = line.split_once(':')?;
        if text.trim() != label {
            return None;
        }
        // The number sits *before* the colon; parsing the whole line would
        // always fail on the trailing label.
        value.trim().parse().ok()
    })
}

/// Reads a `"7: Number of Application Dependent Integers"` header and returns
/// the `(index, value)` pairs of the `"N: Integer[i]"` lines that follow it.
///
/// The entries are **not** adjacent to the header. Upstream emits the sibling
/// `"Number of Application Dependent Reals"` and `... Strings` counts in
/// between, so the scan walks forward and takes only lines that actually parse
/// as `Integer[i]`, stopping once the declared count is collected.
fn parse_indexed_block(lines: &[&str], header_label: &str) -> Option<Vec<(usize, i32)>> {
    let header_index = lines.iter().position(|line| {
        line.split_once(':')
            .is_some_and(|(_, text)| text.trim() == header_label)
    })?;

    let count = leading_int(lines[header_index])? as usize;
    if count == 0 {
        return Some(Vec::new());
    }

    let mut out = Vec::with_capacity(count);
    for line in lines.iter().skip(header_index + 1) {
        let Some((value, label)) = line.split_once(':') else {
            continue;
        };
        let Some(index) = label
            .trim()
            .strip_prefix("Integer[")
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(|digits| digits.parse::<usize>().ok())
        else {
            continue;
        };
        out.push((index, value.trim().parse().unwrap_or(0)));
        if out.len() == count {
            break;
        }
    }
    Some(out)
}

/// Scans the EQ section: `Band N` / `CF` / `Boost/Cut` triples.
///
/// Returns `(centre frequency, gain dB)` per band. An empty vector means the
/// file declared no bands; that is not an error, just a preset with no curve.
fn parse_eq_bands(lines: &[&str]) -> Vec<(f64, f64)> {
    let Some(header_index) = lines.iter().position(|line| {
        line.split_once(':')
            .is_some_and(|(_, text)| text.trim() == "Number of EQ Bands")
    }) else {
        return Vec::new();
    };

    // Skip the header and the On/Off Flag line, then read triples until the
    // section runs out or the declared band count is reached.
    let declared = leading_int(lines[header_index]).unwrap_or(0) as usize;
    let mut bands = Vec::with_capacity(declared);
    let mut cursor = header_index + 1;

    while cursor + 2 < lines.len() + 1 && bands.len() < declared.max(1) {
        let Some(line) = lines.get(cursor) else { break };
        if !line.trim().starts_with("Band") {
            // One non-band line is expected right after the header (the
            // On/Off flag); stop as soon as the section is clearly over.
            if bands.is_empty() {
                cursor += 1;
                continue;
            }
            break;
        }

        let freq = lines
            .get(cursor + 1)
            .and_then(|line| value_before_colon(line))
            .unwrap_or(0.0);
        let gain = lines
            .get(cursor + 2)
            .and_then(|line| value_before_colon(line))
            .unwrap_or(0.0);
        bands.push((freq, gain));
        cursor += 3;
    }

    bands
}

/// Reads the numeric value before the colon of a `"   62.5: CF"` line.
fn value_before_colon(line: &str) -> Option<f64> {
    let (value, _) = line.split_once(':')?;
    value.trim().parse().ok()
}

/// One entry in the preset chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetEntry {
    /// File on disk. For embedded presets this is the copy under `%APPDATA%`.
    pub path: PathBuf,
    /// Display name, from the file's line 2.
    pub name: String,
    /// Where it came from, for grouping in the menu.
    pub origin: PresetOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetOrigin {
    /// Shipped inside the executable and unpacked on first run.
    BuiltIn,
    /// Dropped into `%APPDATA%\FxTrumpet\presets` by the user.
    User,
}

/// The `.fac` files compiled into the binary.
///
/// Name and bytes only; the display label is read from the file itself at
/// runtime, so renaming a preset means editing the `.fac`, not this table.
const EMBEDDED: &[(&str, &[u8])] = &[
    ("Music.fac", include_bytes!("../assets/presets/Music.fac")),
    ("Movie.fac", include_bytes!("../assets/presets/Movie.fac")),
    ("Gaming.fac", include_bytes!("../assets/presets/Gaming.fac")),
    ("BassBoost.fac", include_bytes!("../assets/presets/BassBoost.fac")),
    ("General.fac", include_bytes!("../assets/presets/General.fac")),
    ("Classical.fac", include_bytes!("../assets/presets/Classical.fac")),
    ("Jazz.fac", include_bytes!("../assets/presets/Jazz.fac")),
    ("Pop.fac", include_bytes!("../assets/presets/Pop.fac")),
    ("Metal.fac", include_bytes!("../assets/presets/Metal.fac")),
    ("Trap.fac", include_bytes!("../assets/presets/Trap.fac")),
    ("RnB.fac", include_bytes!("../assets/presets/RnB.fac")),
    ("ClassicRock.fac", include_bytes!("../assets/presets/ClassicRock.fac")),
    ("ModernRock.fac", include_bytes!("../assets/presets/ModernRock.fac")),
    ("AlternativeRock.fac", include_bytes!("../assets/presets/AlternativeRock.fac")),
    ("Preset1.fac", include_bytes!("../assets/presets/Preset1.fac")),
    ("Preset2.fac", include_bytes!("../assets/presets/Preset2.fac")),
    ("Preset6.fac", include_bytes!("../assets/presets/Preset6.fac")),
];

/// How many presets ship in the binary.
pub fn embedded_count() -> usize {
    EMBEDDED.len()
}

/// Parses every preset compiled into the binary.
///
/// Exists so a caller can inspect the bundled library without unpacking it to
/// `%APPDATA%` first, which is what keeps `audiochk` genuinely read-only.
pub fn embedded_presets() -> Vec<FacPreset> {
    EMBEDDED
        .iter()
        .filter_map(|(name, bytes)| FacPreset::from_bytes(Path::new(name), bytes).ok())
        .collect()
}

/// Unpacks the embedded presets into `%APPDATA%\FxTrumpet\presets`.
///
/// Files that already exist are left alone, so a user can edit a bundled
/// preset and keep their edit across upgrades. Returns the directory.
pub fn unpack_embedded_presets() -> std::io::Result<PathBuf> {
    let dir = crate::config::presets_dir();
    std::fs::create_dir_all(&dir)?;

    for (filename, bytes) in EMBEDDED {
        let target = dir.join(filename);
        if target.exists() {
            continue;
        }
        std::fs::write(&target, bytes)?;
    }
    Ok(dir)
}

/// Lists every `.fac` in `dir`, sorted by display name.
///
/// Unreadable files are skipped rather than aborting the listing: one corrupt
/// preset in the user's folder must not hide the other twenty.
pub fn list_dir(dir: &Path, origin: PresetOrigin) -> Vec<PresetEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut out: Vec<PresetEntry> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("fac"))
        })
        .filter_map(|path| {
            let preset = FacPreset::from_file(&path).ok()?;
            Some(PresetEntry {
                path,
                name: preset.name,
                origin,
            })
        })
        .collect();

    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Every preset FxTrumpet can see, sorted by display name.
///
/// One folder serves both purposes: `%APPDATA%\FxTrumpet\presets` is where the
/// bundled files are unpacked *and* where the user drops their own, so adding a
/// preset is a single copy. Origin is decided by whether the file is one we
/// shipped, not by which directory it came from.
pub fn library() -> Vec<PresetEntry> {
    let mut out = list_dir(&crate::config::presets_dir(), PresetOrigin::BuiltIn);

    for entry in &mut out {
        let filename = entry
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let shipped = EMBEDDED
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(filename));
        if !shipped {
            entry.origin = PresetOrigin::User;
        }
    }

    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.origin.cmp_ignore_order(&b.origin))
    });
    out
}

/// Ordering for [`PresetOrigin`] that puts built-ins first.
trait OriginOrder {
    fn cmp_ignore_order(&self, other: &Self) -> std::cmp::Ordering;
}

impl OriginOrder for PresetOrigin {
    fn cmp_ignore_order(&self, other: &Self) -> std::cmp::Ordering {
        let rank = |origin: &PresetOrigin| match origin {
            PresetOrigin::BuiltIn => 0,
            PresetOrigin::User => 1,
        };
        rank(self).cmp(&rank(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MUSIC: &[u8] = include_bytes!("../assets/presets/Music.fac");

    #[test]
    fn parses_the_bundled_preset() {
        let preset = FacPreset::from_bytes(Path::new("Music.fac"), MUSIC).unwrap();
        assert_eq!(preset.version, 9);
        assert_eq!(preset.name, "音乐");
        assert_eq!(preset.mains, [50, 35, 0, 35, 20, 60]);
        assert_eq!(preset.bands.len(), 10);
        assert_eq!(preset.bands[0], (62.5, 0.0));
        assert_eq!(preset.bands[9], (16000.0, 2.0));
        assert!(preset.app_flags.len() >= 5);
    }

    #[test]
    fn main_slot_mapping_is_not_the_enum_ordinal() {
        // The trap this table exists to prevent.
        assert_eq!(MAIN_SLOT_TO_EFFECT[1], Some(ffi::DFX_EFFECT_SURROUND));
        assert_ne!(MAIN_SLOT_TO_EFFECT[1], Some(1));
        assert_eq!(MAIN_SLOT_TO_EFFECT[3], Some(ffi::DFX_EFFECT_AMBIENCE));
        assert_ne!(MAIN_SLOT_TO_EFFECT[3], Some(3));

        let preset = FacPreset::from_bytes(Path::new("Music.fac"), MUSIC).unwrap();
        // Main 0 = 50, and the loader puts it straight into the normalised
        // field, so 50 / 12.7 must land back on 50 via the setter.
        let fidelity = preset.effect_slider_value(ffi::DFX_EFFECT_FIDELITY).unwrap();
        assert!((fidelity - 3.937).abs() < 0.01, "got {fidelity}");
    }

    #[test]
    fn rejects_a_non_preset() {
        assert!(FacPreset::from_bytes(Path::new("x.fac"), b"hello\nworld\n").is_err());
    }

    #[test]
    fn tolerates_crlf_and_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(MUSIC);
        // The bundled files are already CRLF; this only asserts the BOM path.
        let preset = FacPreset::from_bytes(Path::new("Music.fac"), &bytes).unwrap();
        assert_eq!(preset.mains[0], 50);
    }

    #[test]
    fn app_flags_are_not_adjacent_to_their_header() {
        // Upstream writes `Number of Application Dependent Reals` and
        // `... Strings` between the Integers header and the Integer[] lines, so
        // a naive "header + N lines" read silently yields nothing.
        let preset = FacPreset::from_bytes(Path::new("Music.fac"), MUSIC).unwrap();
        assert_eq!(preset.app_flags, vec![1, 1, 1, 1, 1, 0, 2]);
    }

    #[test]
    fn effect_enable_flags_line_up_with_the_slot_map() {
        let preset = FacPreset::from_bytes(Path::new("Music.fac"), MUSIC).unwrap();
        // Music.fac switches every live effect on, and the reserved Main 2 slot
        // has no effect id to ask about.
        for effect in [
            ffi::DFX_EFFECT_FIDELITY,
            ffi::DFX_EFFECT_SURROUND,
            ffi::DFX_EFFECT_AMBIENCE,
            ffi::DFX_EFFECT_DYNAMIC_BOOST,
            ffi::DFX_EFFECT_BASS,
        ] {
            assert!(preset.effect_enabled(effect), "effect {effect} should be on");
        }
    }

    #[test]
    fn a_custom_name_survives_and_a_missing_main_defaults_to_zero() {
        let text = "CLASS1 : Effect Type\r\n9: Version\r\nMy Preset\r\n50: Main 0\r\n";
        let preset = FacPreset::from_bytes(Path::new("custom.fac"), text.as_bytes()).unwrap();
        assert_eq!(preset.name, "My Preset");
        assert_eq!(preset.mains[0], 50);
        assert_eq!(preset.mains[5], 0);
        assert!(preset.bands.is_empty());
    }
}
