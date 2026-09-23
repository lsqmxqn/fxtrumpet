// FxTrumpet build script.
//
// Compiles two static libraries out of the vendored FxSound sources and links
// them into the crate:
//
//   dfxdsp    the DSP engine        (vendor/dsp, 94 units)
//   dfxutil   the support layer     (vendor/audiopassthru, 33 units)
//
// They are built as separate libraries because upstream gives them different
// preprocessor definitions — see the constants below. Merging them into one
// `cc::Build` would apply the DSP's DSPSOFT_TARGET to files that upstream
// compiles without it.
//
// Requires MSVC. Run `. .\toolchain.ps1` (or `source toolchain.sh`) first — this
// machine's Visual Studio is not registered with the installer, so neither cargo
// nor cc-rs can locate cl.exe/link.exe on their own.

use std::path::{Path, PathBuf};
use std::process::Command;

// The executable's icon is drawn by the same code that draws the tray icon.
// `include!` rather than a copy: two rasterisers would drift, and the mismatch
// would only ever show up on someone else's desktop.
//
// `allow(dead_code)` because the build script uses `render_ico` while
// `render_rgba` and `DESIGN_SIZE` are there for the other consumer.
#[allow(dead_code)]
mod icon_raster {
    include!("src/ui/icon_raster.rs");
}

/// Reads a translation-unit list produced by `vendor.ps1` from upstream's
/// `.vcxproj`, resolving each entry against `root`.
///
/// The trees must NOT be globbed. Alongside the units the upstream projects
/// actually build they also contain superseded variants — `Lex32org.c` and
/// friends — which reference struct members that no longer exist in `c_Lex.h`
/// and fail with C2039.
fn read_source_list(list_path: &Path, root: &Path) -> Vec<PathBuf> {
    let contents = std::fs::read_to_string(list_path).unwrap_or_else(|err| {
        panic!(
            "cannot read {}: {}\n\
             Run: powershell -NoProfile -ExecutionPolicy Bypass -File vendor.ps1 \
             -Source ..\\fxsound-app -Dest .",
            list_path.display(),
            err
        )
    });

    let mut sources = Vec::new();
    for line in contents.lines() {
        // Set-Content writes CRLF; trim handles that and stray blank lines.
        let relative = line.trim();
        if relative.is_empty() {
            continue;
        }
        let path = root.join(relative);
        if !path.is_file() {
            panic!("source list references a missing file: {}", path.display());
        }
        sources.push(path);
    }

    if sources.is_empty() {
        panic!("source list {} is empty", list_path.display());
    }

    sources
}

/// Whether the Rust side is linking the CRT statically.
///
/// The C++ has to make the same choice: MSVC does not support a process that
/// links both CRTs, and the failure mode is not a link error but two copies of
/// the same global state. `.cargo/config.toml` turns it on via
/// `target-feature=+crt-static`; `cc` would normally notice that on its own by
/// reading `CARGO_CFG_TARGET_FEATURE`, but that variable is only populated from
/// rustflags under some cargo versions, so all three sources are checked and
/// the answer is passed to `cc` explicitly.
fn crt_is_static() -> bool {
    ["CARGO_CFG_TARGET_FEATURE", "CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .any(|value| value.contains("crt-static"))
}

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let vendor = manifest_dir.join("vendor");

    let dsp_root = vendor.join("dsp");
    let ap_root = vendor.join("audiopassthru");

    let dsp_list = vendor.join("sources-dsp.txt");
    let ap_list = vendor.join("sources-audiopassthru.txt");

    for required in [&dsp_root, &ap_root] {
        if !required.is_dir() {
            panic!(
                "vendored sources missing at {}.\n\
                 Run: powershell -NoProfile -ExecutionPolicy Bypass -File vendor.ps1 \
                 -Source ..\\fxsound-app -Dest .",
                required.display()
            );
        }
    }

    // ── DSP engine ─────────────────────────────────────────────────────
    let dsp_sources = read_source_list(&dsp_list, &dsp_root);
    let static_crt = crt_is_static();

    let mut dsp = cc::Build::new();
    dsp.cpp(true)
        .std("c++17")
        .warnings(false)
        .static_crt(static_crt)
        .include(&dsp_root)
        .include(dsp_root.join("include"))
        .include(dsp_root.join("ptutil").join("include"))
        // DfxDsp.vcxproj adds this to its include path, so a few shared headers
        // (pt_defs.h, codedefs.h, ...) resolve from here.
        .include(ap_root.join("include"))
        // Copied verbatim from the <PreprocessorDefinitions> of DfxDsp.vcxproj:
        // NDEBUG;_LIB;PT_NON_MFC;DSPSOFT_TARGET;PT_DSP_BUILD=PT_DSP_DFX
        //
        // DSPSOFT_TARGET is not optional. Without it, ptutil/include/boardrv1.h
        // hits `#error PC_TARGET or DSP_TARGET or DSPSOFT_TARGET not defined`
        // and nothing compiles at all.
        .define("NDEBUG", None)
        .define("_LIB", None)
        .define("WIN32", None)
        .define("PT_NON_MFC", None)
        .define("DSPSOFT_TARGET", None)
        .define("PT_DSP_BUILD", Some("PT_DSP_DFX"))
        .define("UNICODE", None)
        .define("_UNICODE", None)
        // Deliberately NOT defining WIN32_LEAN_AND_MEAN. It looks harmless, but
        // it strips objbase.h (and the rest of OLE) out of windows.h, which
        // breaks call sites like CoCreateGuid in the support layer with C3861.
        .define("_CRT_SECURE_NO_WARNINGS", None);

    for source in &dsp_sources {
        dsp.file(source);
    }
    dsp.file(manifest_dir.join("capi").join("dfxdsp_capi.cpp"));
    dsp.compile("dfxdsp");

    // ── support layer ──────────────────────────────────────────────────
    // FILE / MRY / MTH / pstr / ptime / reg / SLOUT / operatingSystem. The DSP
    // references their reg*, mth*, pstr*, file* helpers, so without this library
    // linking fails with LNK2019 on 14 distinct symbols.
    //
    // The device layer (AudioPassthru/, sndDevices/) is deliberately excluded:
    // FxTrumpet runs its own WASAPI loop, and sndDevices drags in its own web of
    // dependencies.
    let ap_sources = read_source_list(&ap_list, &ap_root);

    let mut util = cc::Build::new();
    util.cpp(true)
        .std("c++17")
        .warnings(false)
        .static_crt(static_crt)
        .include(ap_root.join("include"))
        // audiopassthru.vcxproj adds $(ProjectDir)..\dsp\include.
        .include(dsp_root.join("include"))
        // audiopassthru.vcxproj lists only NDEBUG;_LIB (+WIN32) in its
        // <PreprocessorDefinitions>, but it also sets
        // <CharacterSet>Unicode</CharacterSet>, which MSBuild turns into
        // UNICODE and _UNICODE automatically. Omitting them makes the generic
        // Win32 macros resolve to their ANSI variants, and the wide-string call
        // sites then fail with C2664 (wchar_t* -> LPCSTR).
        //
        // Nothing beyond this list: upstream sets neither PT_NON_MFC nor
        // WIN32_LEAN_AND_MEAN here. UseOfMfc is unset, so there is no MFC.
        .define("NDEBUG", None)
        .define("_LIB", None)
        .define("WIN32", None)
        .define("UNICODE", None)
        .define("_UNICODE", None)
        .define("_CRT_SECURE_NO_WARNINGS", None);

    for source in &ap_sources {
        util.file(source);
    }
    util.compile("dfxutil");

    // Win32 / COM / WASAPI symbols the engine and support layer reference.
    // shlwapi is needed for PathFileExistsW, used by FILE\FileGeneral.cpp.
    println!("cargo:rustc-link-lib=ole32");
    println!("cargo:rustc-link-lib=winmm");
    println!("cargo:rustc-link-lib=user32");
    println!("cargo:rustc-link-lib=advapi32");
    println!("cargo:rustc-link-lib=shell32");
    println!("cargo:rustc-link-lib=shlwapi");

    println!("cargo:rerun-if-changed=vendor/dsp");
    println!("cargo:rerun-if-changed=vendor/audiopassthru");
    println!("cargo:rerun-if-changed=vendor/sources-dsp.txt");
    println!("cargo:rerun-if-changed=vendor/sources-audiopassthru.txt");
    println!("cargo:rerun-if-changed=capi/dfxdsp_capi.cpp");
    println!("cargo:rerun-if-changed=capi/dfxdsp_capi.h");

    embed_windows_resources();
}

/// Bakes the icon and the version block into every binary this crate builds.
///
/// Without this an `.exe` shows up in Explorer with the generic application
/// icon and a Properties dialog that knows nothing about it — which reads as
/// "unfinished" no matter how good the audio is.
///
/// The pipeline is the plain Win32 one: draw the `.ico`, write an `.rc`, run
/// `rc.exe` over it, hand the resulting `.res` to the linker. `rc.exe` ships in
/// the Windows SDK, which `toolchain.ps1` already requires for the DSP, so this
/// adds no new prerequisite.
///
/// Missing `rc.exe` degrades to a warning instead of a failed build: an icon is
/// not worth refusing to compile over.
fn embed_windows_resources() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo always sets OUT_DIR"));

    let icon_path = out_dir.join("fxtrumpet.ico");
    if let Err(err) = std::fs::write(&icon_path, icon_raster::render_ico(&icon_raster::ICO_SIZES)) {
        println!("cargo:warning=could not write {}: {err}", icon_path.display());
        return;
    }

    let script_path = out_dir.join("fxtrumpet.rc");
    if let Err(err) = std::fs::write(&script_path, resource_script()) {
        println!("cargo:warning=could not write {}: {err}", script_path.display());
        return;
    }

    let Some(rc) = find_resource_compiler() else {
        println!(
            "cargo:warning=rc.exe was not found, so the binaries will have no icon or version \
             information. Set FXTRUMPET_RC to its full path, or build with the Windows SDK on PATH."
        );
        return;
    };

    let resource_path = out_dir.join("fxtrumpet.res");
    let output = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource_path)
        .arg(&script_path)
        // The `.rc` refers to the icon by a relative name, so the working
        // directory has to be the one both files were written to.
        .current_dir(&out_dir)
        .output();

    match output {
        Ok(output) if output.status.success() => {
            // `-bins`, not the all-targets form: the library target never links,
            // and passing a linker input to it is at best noise.
            println!("cargo:rustc-link-arg-bins={}", resource_path.display());
        }
        Ok(output) => println!(
            "cargo:warning=rc.exe failed ({}): {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) => println!("cargo:warning=could not run {}: {err}", rc.display()),
    }

    println!("cargo:rerun-if-changed=src/ui/icon_raster.rs");
    println!("cargo:rerun-if-env-changed=FXTRUMPET_RC");
}

/// The `FILEVERSION` / `PRODUCTVERSION` tuple for a semver string.
///
/// A PE version resource has four *numeric* fields and no way to express
/// pre-release ordering. So for `0.1.0-beta` the tuple can only carry the
/// numeric core, `0,1,0,0`, and the tag lives in the `FileVersion` **string**
/// alongside it. That is a limit of the format rather than an oversight:
/// Windows compares these fields as integers, so no value sorts below `0,1,0,0`
/// and the release this beta will become is indistinguishable from it by the
/// tuple alone. Anything that needs to tell the two apart has to read the
/// string — `GetFileVersionInfo` + the `StringFileInfo` block, not the fixed
/// part.
///
/// The tag and any build metadata are stripped before parsing instead of being
/// left to fail, because a component that fails to parse silently becomes 0:
/// the right answer here for the wrong reason, and the wrong one the moment a
/// version looks like `0.1.3-rc1` and the tag is not in position three.
fn numeric_version(version: &str) -> String {
    // In semver the `-` of a pre-release always precedes the `+` of build
    // metadata, so cutting at the first `-` already removes both.
    let core = version.split('-').next().unwrap_or(version);
    let core = core.split('+').next().unwrap_or(core);

    let mut parts: Vec<u16> = core
        .split('.')
        .map(|part| part.trim().parse::<u16>().unwrap_or(0))
        .collect();
    parts.resize(4, 0);

    format!("{},{},{},{}", parts[0], parts[1], parts[2], parts[3])
}

/// Writes the resource script.
///
/// Deliberately ASCII-only and free of `#include`/`LANGUAGE` constants: the
/// numeric forms need no headers, so `rc.exe` works even when the SDK's include
/// path is not set up in the environment cargo was launched from.
fn resource_script() -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
    let file_version = numeric_version(&version);

    let description = std::env::var("CARGO_PKG_DESCRIPTION")
        .unwrap_or_else(|_| "tray-resident audio enhancer".to_owned())
        .replace(['"', '\n', '\r'], " ");

    format!(
        "\
LANGUAGE 9, 1
1 ICON \"fxtrumpet.ico\"
1 VERSIONINFO
FILEVERSION {file_version}
PRODUCTVERSION {file_version}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK \"StringFileInfo\"
  BEGIN
    BLOCK \"040904B0\"
    BEGIN
      VALUE \"CompanyName\", \"FxTrumpet\"
      VALUE \"FileDescription\", \"{description}\"
      VALUE \"FileVersion\", \"{version}\"
      VALUE \"InternalName\", \"fxtrumpet\"
      VALUE \"OriginalFilename\", \"fxtrumpet.exe\"
      VALUE \"ProductName\", \"FxTrumpet\"
      VALUE \"ProductVersion\", \"{version}\"
    END
  END
  BLOCK \"VarFileInfo\"
  BEGIN
    VALUE \"Translation\", 0x409, 0x4B0
  END
END
"
    )
}

/// Locates `rc.exe`.
///
/// Checked in order of decreasing explicitness: an override, the Windows SDK's
/// versioned `bin` directories, then `PATH`.
fn find_resource_compiler() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("FXTRUMPET_RC") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }

    for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Some(root) = std::env::var_os(variable) else {
            continue;
        };
        let bin = Path::new(&root).join("Windows Kits").join("10").join("bin");
        let Ok(entries) = std::fs::read_dir(&bin) else {
            continue;
        };

        // Lexicographic sort on "10.0.<build>.0" orders by build number because
        // the leading components are fixed width in practice.
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path().join("x64").join("rc.exe"))
            .filter(|path| path.is_file())
            .collect();
        candidates.sort();
        if let Some(newest) = candidates.pop() {
            return Some(newest);
        }
    }

    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join("rc.exe"))
        .find(|candidate| candidate.is_file())
}
