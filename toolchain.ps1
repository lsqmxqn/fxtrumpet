# FxTrumpet — set up the MSVC + Windows SDK build environment.
#
#   . .\toolchain.ps1                       # auto-detect
#   cargo build --release --bin dspcheck
#
# Or with an explicit layout:
#   . .\toolchain.ps1 -MsvcRoot 'D:\Program Files\VisualStudio\VC\Tools\MSVC'
#
# ---------------------------------------------------------------------------
# Why this script exists
#
# On the development machine Visual Studio lives at D:\Program Files\VisualStudio
# and is NOT registered with the installer, so `vswhere.exe` returns nothing and
# there is no HKLM\...\VisualStudio\SxS\VS7 entry. Consequences:
#
#   * cargo cannot find link.exe, so even a hello-world Rust program fails to
#     build ("linker `link.exe` not found");
#   * the `cc` crate cannot find cl.exe, so the vendored DSP never compiles.
#
# There is also a second, sneakier trap: Git for Windows ships a coreutils
# `link.exe` in /usr/bin that is a hardlink utility, not a linker. If it comes
# first on PATH, rustc invokes it and the build dies with
# "link: extra operand ... Try 'link --help'". Prepending the MSVC bin
# directory — which this script does — resolves both problems.
#
# On a GitHub runner none of that applies: Visual Studio IS registered there and
# vswhere finds it. So the toolset root is *resolved* rather than hardcoded, in
# this order:
#
#   1. -MsvcRoot, if given
#   2. $env:FXTRUMPET_MSVC_ROOT
#   3. vswhere                                  (what CI relies on)
#   4. D:\Program Files\VisualStudio\VC\Tools\MSVC   (this machine's layout)
#
# The same resolution runs for the Windows SDK, which lives at the standard
# C:\Program Files (x86)\Windows Kits\10 both here and on a runner.
# ---------------------------------------------------------------------------

param(
    [string]$MsvcRoot = '',
    [string]$SdkRoot  = '',
    [string]$Arch     = 'x64',
    [string]$HostArch = 'Hostx64'
)

# Dot-sourcing a script runs it in the caller's scope, so any assignment to
# $ErrorActionPreference leaks into the session. That matters: with 'Stop',
# PowerShell 5.1 turns redirected *native* stderr into a terminating
# NativeCommandError — so the next `cargo build` would die on cargo's own
# progress output ("Compiling ..."). Save the caller's value first, restore it
# at the end of this script.
$fxtrumpet_previous_eap = $ErrorActionPreference
$ErrorActionPreference = 'Stop'

# This machine's layout, used when nothing better is found. Kept as a constant
# rather than a parameter default so that "no value was supplied" stays
# distinguishable from "the caller asked for exactly this path".
$defaultMsvcRoot = 'D:\Program Files\VisualStudio\VC\Tools\MSVC'
$defaultSdkRoot = 'C:\Program Files (x86)\Windows Kits\10'

function Resolve-MsvcRoot {
    param([string]$Explicit)

    if ($Explicit) { return $Explicit }
    if ($env:FXTRUMPET_MSVC_ROOT) { return $env:FXTRUMPET_MSVC_ROOT }

    # vswhere is how a *registered* Visual Studio is located. It exists on this
    # machine but reports nothing, because the installation is not registered —
    # which is the whole reason this script is needed locally. On a runner this
    # is the branch that does the work.
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        # A probe that is expected to "fail", so relax the preference: under
        # 'Stop', redirected native stderr becomes a terminating error.
        $previous = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $install = & $vswhere -latest -products * `
                -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
                -property installationPath 2>$null
        } finally {
            $ErrorActionPreference = $previous
        }

        $install = ($install | Select-Object -First 1)
        if ($install) {
            # vswhere ends its output with a newline even for a single property.
            $root = Join-Path $install.Trim() 'VC\Tools\MSVC'
            if (Test-Path $root) { return $root }
        }
    }

    return $defaultMsvcRoot
}

function Resolve-SdkRoot {
    param([string]$Explicit)

    if ($Explicit) { return $Explicit }
    if ($env:FXTRUMPET_SDK_ROOT) { return $env:FXTRUMPET_SDK_ROOT }
    return $defaultSdkRoot
}

function Get-NewestVersionDir {
    param(
        [Parameter(Mandatory)][string]$Root,
        # Empty is valid and means "the version directory itself is the probe"
        # (used for the Windows SDK, whose version dirs are always usable). A
        # Mandatory string parameter rejects '' unless AllowEmptyString is set.
        [Parameter(Mandatory)][AllowEmptyString()][string]$RelativeProbe,
        [Parameter(Mandatory)][string]$Label
    )

    if (-not (Test-Path $Root)) {
        throw "$Label root not found: $Root"
    }

    # Only consider versions that actually ship the piece we need. A partially
    # installed toolset (e.g. MSVC 14.50 here has headers but no lib\x64) would
    # otherwise be picked as "newest" and fail much later, during linking.
    $candidates = Get-ChildItem -Path $Root -Directory |
        Where-Object {
            if ([string]::IsNullOrEmpty($RelativeProbe)) { $true }
            else { Test-Path (Join-Path $_.FullName $RelativeProbe) }
        } |
        Sort-Object { [version]($_.Name -replace '[^0-9.]', '') } -Descending

    if (-not $candidates) {
        throw "$Label has no usable version under $Root (none contains '$RelativeProbe')"
    }

    return $candidates[0].FullName
}

$MsvcRoot = Resolve-MsvcRoot -Explicit $MsvcRoot
$SdkRoot = Resolve-SdkRoot -Explicit $SdkRoot

$toolset = Get-NewestVersionDir -Root $MsvcRoot `
    -RelativeProbe "lib\$Arch" -Label 'MSVC toolset'

$sdkVersion = Get-NewestVersionDir -Root (Join-Path $SdkRoot 'Include') `
    -RelativeProbe '' -Label 'Windows SDK include'
$sdkVersionName = Split-Path $sdkVersion -Leaf

if (-not (Test-Path (Join-Path $SdkRoot "Lib\$sdkVersionName\um\$Arch"))) {
    throw "Windows SDK $sdkVersionName has no Lib\...\um\$Arch — install the SDK's x64 libraries"
}

$vcBin = Join-Path $toolset "bin\$HostArch\$Arch"
if (-not (Test-Path (Join-Path $vcBin 'cl.exe'))) {
    throw "cl.exe not found in $vcBin"
}

$sdkBin = Join-Path $SdkRoot "bin\$sdkVersionName\$Arch"

# MSVC's bin directory MUST come first: see the Git-for-Windows link.exe note
# at the top of this file.
$env:PATH = (@($vcBin, $sdkBin) + ($env:PATH -split ';')) -join ';'

$env:INCLUDE = (@(
        (Join-Path $toolset 'include')
        (Join-Path $SdkRoot "Include\$sdkVersionName\ucrt")
        (Join-Path $SdkRoot "Include\$sdkVersionName\um")
        (Join-Path $SdkRoot "Include\$sdkVersionName\shared")
        (Join-Path $SdkRoot "Include\$sdkVersionName\winrt")
        (Join-Path $SdkRoot "Include\$sdkVersionName\cppwinrt")
    ) -join ';')

$env:LIB = (@(
        (Join-Path $toolset "lib\$Arch")
        (Join-Path $SdkRoot "Lib\$sdkVersionName\ucrt\$Arch")
        (Join-Path $SdkRoot "Lib\$sdkVersionName\um\$Arch")
    ) -join ';')

# Tell cc-rs which compiler to use instead of letting it probe vswhere, which
# cannot see this installation.
$env:CC = 'cl.exe'
$env:CXX = 'cl.exe'

# cl.exe prints its banner on stderr and exits non-zero when given no inputs.
# Under $ErrorActionPreference='Stop' PowerShell 5.1 turns redirected native
# stderr into a terminating NativeCommandError, which would abort this script
# on a probe that is expected to "fail". Relax the preference for the probe.
$clVersion = ''
$ErrorActionPreference = 'Continue'
try {
    $clVersion = (& cl.exe 2>&1 | Select-Object -First 1)
} finally {
    $ErrorActionPreference = 'Stop'
}

if (-not $clVersion) { throw 'cl.exe produced no version banner — the toolset is not usable' }

$linkPath = (Get-Command link.exe -ErrorAction SilentlyContinue).Source

Write-Host 'FxTrumpet toolchain ready'
Write-Host "  MSVC root    : $MsvcRoot"
Write-Host "  MSVC toolset : $toolset"
Write-Host "  Windows SDK  : $sdkVersionName  ($SdkRoot)"
Write-Host "  cl.exe       : $clVersion"
Write-Host "  link.exe     : $linkPath"

if ($linkPath -and $linkPath -notlike "$vcBin*") {
    Write-Warning "link.exe resolves to $linkPath, not MSVC's. Linking will fail — check PATH order."
}

Write-Host ''
Write-Host 'Next: cargo build --release --bin dspcheck'

# Hand the caller's preference back, so that a later `cargo build` is not turned
# into a terminating error by cargo's own stderr progress output.
$ErrorActionPreference = $fxtrumpet_previous_eap
