# FxTrumpet — vendor the FxSound DSP engine and the support layer it depends on.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File vendor.ps1 `
#       -Source ..\fxsound-app -Dest .
#
# Two upstream projects are involved:
#
#   dsp/DfxDsp.vcxproj             the DSP engine itself            -> 94 units
#   audiopassthru/audiopassthru.vcxproj
#       ├─ support layer (FILE/MRY/MTH/pstr/ptime/reg/SLOUT/operatingSystem)
#       │                          utility functions the DSP links    -> 33 units
#       └─ device layer (AudioPassthru/, sndDevices/)
#                                  WASAPI capture/playback, device enumeration.
#                                  NOT vendored — FxTrumpet implements its own
#                                  audio loop, so this layer is dead weight and
#                                  drags in the whole sndDevices dependency web.
#
# Both source lists are derived from the corresponding .vcxproj rather than by
# globbing the directory, because the trees contain superseded variants that no
# longer compile (Lex32org.c references struct members absent from c_Lex.h).
#
# The sync is copy-only by default: existing files are overwritten in place, and
# nothing is deleted. This is safe because both build.rs and this script work
# from explicit translation-unit lists, so a file left behind from an earlier run
# is never compiled.
#
# Pass -Prune to also delete files that no longer exist upstream. That is only
# needed for tidiness, and it needs a working Recycle Bin / trash integration —
# some sandboxed environments fail closed on delete, which would abort the run.

param(
    [Parameter(Mandatory = $true)][string]$Source,
    [Parameter(Mandatory = $true)][string]$Dest,
    [switch]$Prune
)

$ErrorActionPreference = 'Stop'

function Sync-Tree {
    param(
        [Parameter(Mandatory)][string]$SourceRoot,
        [Parameter(Mandatory)][string]$DestRoot,
        [string[]]$ExcludePaths = @(),
        [string[]]$ExcludeExts = @()
    )

    if (-not (Test-Path $SourceRoot)) { throw "not found: $SourceRoot" }
    if (-not (Test-Path $DestRoot)) { New-Item -ItemType Directory -Path $DestRoot -Force | Out-Null }

    $copied = 0
    Get-ChildItem -Path $SourceRoot -Recurse -File | ForEach-Object {
        $rel = $_.FullName.Substring($SourceRoot.Length).TrimStart('\')

        $skip = $false
        foreach ($excluded in $ExcludePaths) {
            if ($rel -eq $excluded -or $rel -like "$excluded\*") { $skip = $true; break }
        }
        if ($skip) { return }
        if ($ExcludeExts -contains $_.Extension.ToLower()) { return }

        $target = Join-Path $DestRoot $rel
        $targetDir = Split-Path $target -Parent
        if (-not (Test-Path $targetDir)) { New-Item -ItemType Directory -Path $targetDir -Force | Out-Null }
        Copy-Item $_.FullName $target -Force
        $script:syncedCopied++
        $copied++
    }

    $pruned = 0
    if ($Prune) {
        Get-ChildItem -Path $DestRoot -Recurse -File | ForEach-Object {
            $rel = $_.FullName.Substring($DestRoot.Length).TrimStart('\')
            if (-not (Test-Path (Join-Path $SourceRoot $rel))) {
                Remove-Item $_.FullName -Force
                $script:syncedPruned++
                $pruned++
            }
        }
    }

    return @{ Copied = $copied; Pruned = $pruned }
}

# Derives the translation-unit list from an MSBuild project, as forward-slashed
# paths relative to that project's directory.
function Get-ProjectSources {
    param(
        [Parameter(Mandatory)][string]$Vcxproj,
        [string[]]$ExcludePaths = @()
    )

    if (-not (Test-Path $Vcxproj)) { throw "not found: $Vcxproj" }

    $all = Select-String -Path $Vcxproj -Pattern 'ClCompile Include="([^"]+)"' -AllMatches |
        ForEach-Object { $_.Matches } |
        ForEach-Object { $_.Groups[1].Value -replace '\\', '/' } |
        Sort-Object -Unique

    if (-not $all) { throw "no ClCompile entries found in $Vcxproj" }

    $kept = @($all | Where-Object {
            $path = $_
            -not ($ExcludePaths | Where-Object { $path -eq $_ -or $path -like "$_/*" })
        })

    if (-not $kept) { throw "all sources filtered out of $Vcxproj" }
    return $kept
}

# PS 5.1's -Encoding UTF8 emits a BOM, which would corrupt the first path once
# build.rs splits the file into lines. ASCII is safe: every path is ASCII.
function Write-SourceList {
    param([string]$Path, [string[]]$Sources, [string]$RelativeTo)
    Set-Content -Path $Path -Value $Sources -Encoding ASCII
    $missing = @($Sources | Where-Object { -not (Test-Path (Join-Path $RelativeTo $_)) })
    if ($missing.Count -gt 0) {
        throw "$Path lists files that were not copied: $($missing -join ', ')"
    }
}

$syncedCopied = 0
$syncedPruned = 0

$dstDsp = Join-Path $Dest 'vendor\dsp'
$dstAp = Join-Path $Dest 'vendor\audiopassthru'

# ── DSP engine ─────────────────────────────────────────────────────────
Sync-Tree -SourceRoot (Join-Path $Source 'dsp') -DestRoot $dstDsp `
    -ExcludePaths @('Win32Main', 'Resources') `
    -ExcludeExts @('.filters', '.txt', '.ico', '.rc', '.sln') | Out-Null

$dspSources = Get-ProjectSources -Vcxproj (Join-Path $dstDsp 'DfxDsp.vcxproj')
if ($dspSources -match 'Win32Main') { throw 'DfxDsp.vcxproj unexpectedly lists Win32Main sources' }
Write-SourceList -Path (Join-Path $Dest 'vendor\sources-dsp.txt') `
    -Sources $dspSources -RelativeTo $dstDsp

# ── support layer ──────────────────────────────────────────────────────
Sync-Tree -SourceRoot (Join-Path $Source 'audiopassthru') -DestRoot $dstAp `
    -ExcludePaths @('src\AudioPassthru', 'src\sndDevices') `
    -ExcludeExts @('.filters', '.sln') | Out-Null

$apSources = Get-ProjectSources -Vcxproj (Join-Path $dstAp 'audiopassthru.vcxproj') `
    -ExcludePaths @('src/AudioPassthru', 'src/sndDevices')
Write-SourceList -Path (Join-Path $Dest 'vendor\sources-audiopassthru.txt') `
    -Sources $apSources -RelativeTo $dstAp

# ── patches ────────────────────────────────────────────────────────────
# Upstream fixes are applied after the copy and before the build lists are
# derived, so a failed patch aborts the run instead of quietly vendoring broken
# sources. Each patch matches a unique anchor line and asserts the match count.
# See patches/README.md for the full diagnosis behind each one.

function Apply-AnchorPatch {
    param(
        [Parameter(Mandatory)][string]$File,
        [Parameter(Mandatory)][string]$Anchor,        # existing line, without leading tab
        [Parameter(Mandatory)][string]$InsertAfter,   # new line to add, without leading tab
        [Parameter(Mandatory)][string]$Description
    )

    if (-not (Test-Path $File)) { throw "patch target not found: $File" }

    $text = Get-Content -Path $File -Raw

    # Idempotent: re-running vendor.ps1 must not insert the line twice.
    if ($text.Contains("`t$InsertAfter")) {
        Write-Host "  already applied: $Description"
        return
    }

    $anchorText = "`t$Anchor"
    $matches = [regex]::Matches($text, [regex]::Escape($anchorText)).Count
    if ($matches -ne 1) {
        throw "anchor matched $matches times (expected exactly 1) in $File while applying: $Description"
    }

    $text = $text.Replace($anchorText, "`t$Anchor`r`n`t$InsertAfter")
    Set-Content -Path $File -Value $text -NoNewline -Encoding ASCII
    Write-Host "  applied: $Description"
}

Apply-AnchorPatch `
    -File (Join-Path $dstDsp 'DfxDspPrivate.cpp') `
    -Anchor 'rval_to_midi_qnt_handle_ = NULL;' `
    -InsertAfter 'preset_list_handle_ = NULL;' `
    -Description 'init preset_list_handle_ (destructor frees an uninitialised pointer)'

# ── licence ────────────────────────────────────────────────────────────
$licence = Join-Path $Source 'LICENSE'
if (Test-Path $licence) { Copy-Item $licence (Join-Path $Dest 'vendor\LICENSE.fxsound-app') -Force }

# Migration: an earlier layout copied only audiopassthru/include to
# vendor\audiopassthru_include. The whole audiopassthru project is mirrored now,
# and build.rs no longer references that path, so the directory is inert. It is
# only removed under -Prune.
$legacy = Join-Path $Dest 'vendor\audiopassthru_include'
if ($Prune -and (Test-Path $legacy)) {
    Get-ChildItem -Path $legacy -Recurse -File | ForEach-Object {
        Remove-Item $_.FullName -Force
        $syncedPruned++
    }
    Write-Host 'removed obsolete vendor\audiopassthru_include contents'
}

# ── report ─────────────────────────────────────────────────────────────
$dspHeaders = (Get-ChildItem -Path $dstDsp -Recurse -File -Include *.h).Count

Write-Host "vendored $syncedCopied files, pruned $syncedPruned stale"
Write-Host "  via DfxDsp.vcxproj        : $($dspSources.Count) units ($dspHeaders headers) -> vendor\sources-dsp.txt"
Write-Host "  via audiopassthru.vcxproj : $($apSources.Count) units            -> vendor\sources-audiopassthru.txt"

$leaked = Get-ChildItem -Path $dstDsp -Recurse -File -Filter 'Win32Main*' -ErrorAction SilentlyContinue
if ($leaked) { throw "Win32Main leaked into vendor tree: $($leaked.FullName)" }

$leakedDevice = Get-ChildItem -Path $dstAp -Recurse -File -Include 'sndDevices*.cpp', 'AudioPassthru*.cpp' -ErrorAction SilentlyContinue
if ($leakedDevice) { throw "device layer leaked into vendor tree: $($leakedDevice.FullName)" }
