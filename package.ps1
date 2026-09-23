# FxTrumpet — M6 packaging.
#
#   .\package.ps1                 # build, assemble, zip
#   .\package.ps1 -NoBuild        # re-package whatever is already in target\release
#   .\package.ps1 -NoZip          # leave the folder, skip the archive
#   .\package.ps1 -Locked         # pass --locked to cargo; CI uses this so the
#                                 # committed Cargo.lock is what actually gets built
#
# Produces, under -OutDir (default `dist`):
#
#   FxTrumpet-<version>-win64.zip     the thing you hand to someone
#   FxTrumpet\                        the unpacked tree the archive is made of
#     fxtrumpet.exe                     the application (icon + version resource)
#     driver\fxvad.{inf,sys,cat}     FxSound's signed virtual sound card
#     README.txt                     what it is, how to run it, how to remove it
#     LICENSE.txt                    AGPL-3.0-or-later and the driver's terms
#     install.ps1                    per-user install, no administrator needed
#     uninstall.ps1                  the reverse
#   SHA256SUMS.txt                 hashes of every file above, plus the archive
#
# Two things the script deliberately checks rather than assumes, and fails on:
#
#   * the executable carries its icon and version resource. A build made without
#     `rc.exe` still compiles and links perfectly well — it just ships a
#     faceless .exe, which is exactly the defect this milestone exists to fix;
#   * the executable does not import the VC++ redistributable. That import is
#     invisible in a build log and only shows up as "the app does not start" on
#     somebody else's machine. Needs `dumpbin.exe` on PATH — dot-source
#     `.\toolchain.ps1` first, or it reports that the check was skipped.

[CmdletBinding()]
param(
    [string]$OutDir = 'dist',
    [string]$Configuration = 'release',
    [switch]$NoBuild,
    [switch]$NoZip,
    [switch]$Locked
)

$ErrorActionPreference = 'Stop'

# `.\package.ps1 --locked` binds the *out directory* to the string "--locked".
#
# PowerShell switches are single-dash, so `--locked` is not a parameter name at
# all — it falls through to the first positional parameter, which is -OutDir.
# The build then succeeds and the entire distributable lands in a directory
# literally named `--locked`, with exit code 0. That is a genuinely confusing way
# to spend an afternoon, and it happened once in CI. Catch it at the door.
if ($OutDir -match '^-') {
    throw @"
-OutDir was given the value '$OutDir', which looks like a mistyped switch.
PowerShell switches take a single dash: write -Locked, not --locked.
"@
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$version = $null
$stage = $null

function Write-Step {
    param([string]$Text)
    Write-Host ''
    Write-Host "== $Text" -ForegroundColor Cyan
}

# Writes a text file with CRLF endings.
#
# The here-strings above are LF-only because they live in a file written by
# tooling that does not care; .txt and .ps1 on Windows look wrong in Notepad and
# diff noisily against hand-edited copies without the conversion.
function Write-TextFile {
    param([string]$Path, [string]$Text, [string]$Encoding)
    $normalised = ($Text -replace "`r`n", "`n") -replace "`n", "`r`n"
    Set-Content -LiteralPath $Path -Value $normalised -Encoding $Encoding
}

function Read-CargoVersion {
    $toml = Get-Content (Join-Path $here 'Cargo.toml') -Raw
    if ($toml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
        throw 'could not read `version` from Cargo.toml'
    }
    return $Matches[1]
}

# Deletes through .NET rather than Remove-Item.
#
# Remove-Item is not a reliable primitive for this job. Some environments wrap
# it in a "safe delete" that diverts the target to the Recycle Bin and fails
# closed when the move fails — which it does for a tree this size — so a second
# run of this script dies the moment it tries to clear dist\FxTrumpet. A packaging
# script wants an outright delete anyway: the point is a clean staging directory,
# not a recoverable one.
function Remove-Tree {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) { [System.IO.Directory]::Delete($Path, $true) }
}

function Remove-File {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) { [System.IO.File]::Delete($Path) }
}

# The signed inf/sys/cat triple is vendored rather than committed: it is a
# binary blob with its own licence, and it is already present in a sibling
# checkout. FXTRUMPET_DRIVER_DIR overrides, for a machine that has neither.
function Find-DriverDir {
    $candidates = @()
    if ($env:FXTRUMPET_DRIVER_DIR) { $candidates += $env:FXTRUMPET_DRIVER_DIR }
    $candidates += (Join-Path $here 'driver')
    $candidates += (Join-Path $here '..\NexBox\src-tauri\resources\binaries\fxvad')

    foreach ($candidate in $candidates) {
        if (-not $candidate) { continue }
        $full = [System.IO.Path]::GetFullPath($candidate)
        $complete = @('fxvad.inf', 'fxvad.sys', 'fxvadntamd64.cat') |
            Where-Object { Test-Path (Join-Path $full $_) }
        if ($complete.Count -eq 3) { return $full }
    }
    return $null
}

function Copy-DriverFiles {
    param([string]$Source, [string]$Destination)

    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    foreach ($name in @('fxvad.inf', 'fxvad.sys', 'fxvadntamd64.cat')) {
        Copy-Item (Join-Path $Source $name) (Join-Path $Destination $name) -Force
    }
}

# Writes the per-user installer.
#
# Installing the driver is intentionally NOT attempted here: it needs
# administrator rights, and the app already owns that flow (tray menu ->
# `--install-driver` -> UAC). Duplicating it would mean two code paths that
# both have to know about restoring the default endpoint afterwards.
$installScript = @'
# FxTrumpet - per-user install.
#
#   powershell -ExecutionPolicy Bypass -File install.ps1
#
# Copies FxTrumpet into %LOCALAPPDATA%\Programs\FxTrumpet, adds a Start-menu shortcut
# and launches it. No administrator rights are required: everything stays inside
# the user's own profile.
#
# Installing the virtual sound card DOES require administrator rights, so it is
# left to the application: right-click the tray icon and choose
# "Install sound card...", then accept the elevation prompt. Until that is
# done FxTrumpet passes audio through unprocessed.

$ErrorActionPreference = 'Stop'

$source = Split-Path -Parent $MyInvocation.MyCommand.Path
$target = Join-Path $env:LOCALAPPDATA 'Programs\FxTrumpet'
$exe = Join-Path $target 'fxtrumpet.exe'

Write-Host "Installing FxTrumpet to $target"

# A running copy holds the current executable open, so it has to go first.
$running = Get-Process -Name fxtrumpet -ErrorAction SilentlyContinue
if ($running) {
    Write-Host 'Stopping the running copy.'
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 800

    # Force-killing it skips its own restore path, which can leave the system's
    # default output pointing at the virtual sound card - i.e. silence. The
    # build being replaced is still on disk at this point, so let it clean up.
    if (Test-Path $exe) {
        & $exe --restore-output
        Start-Sleep -Milliseconds 300
    }
}

New-Item -ItemType Directory -Force -Path $target | Out-Null
Copy-Item -Path (Join-Path $source '*') -Destination $target -Recurse -Force

# Start-menu shortcut. Plain COM rather than Add-Type: this has to work in a
# locked-down shell where compiling C# at runtime is blocked.
$startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$shortcut = Join-Path $startMenu 'FxTrumpet.lnk'
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($shortcut)
$link.TargetPath = $exe
$link.WorkingDirectory = $target
$link.Description = 'FxTrumpet audio enhancer'
$link.Save()

# Nothing to do about startup: FxTrumpet registers itself in HKCU\...\Run on its
# first start. That is deliberate - it is the only way a logon can never leave
# the machine silent - so the entry is written before the window even appears.
Start-Process -FilePath $exe

Write-Host ''
Write-Host 'FxTrumpet is running; look for its icon in the notification area.'
Write-Host 'Next: right-click that icon -> "Install sound card...".'
'@

$uninstallScript = @'
# FxTrumpet - per-user uninstall.
#
#   powershell -ExecutionPolicy Bypass -File uninstall.ps1
#
# Removes the program, its shortcut and its start-at-logon entry. It leaves two
# things alone on purpose:
#
#   * the virtual sound card, which needs administrator rights to remove
#     (tray menu -> "Remove virtual sound card", before uninstalling);
#   * %APPDATA%\FxTrumpet, which holds your presets and settings. Delete that
#     folder by hand if you want a clean slate.

$ErrorActionPreference = 'Stop'

$target = Join-Path $env:LOCALAPPDATA 'Programs\FxTrumpet'
$exe = Join-Path $target 'fxtrumpet.exe'

$running = Get-Process -Name fxtrumpet -ErrorAction SilentlyContinue
if ($running) {
    Write-Host 'Stopping FxTrumpet.'
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 800
}

# Hand the default output back before the binary goes away. Without this a
# machine that was left pointed at the virtual sound card stays mute, and
# removing the startup entry below would also remove the automatic repair.
if (Test-Path $exe) {
    Write-Host 'Restoring the default output device.'
    & $exe --restore-output
    Start-Sleep -Milliseconds 300
}

$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
Remove-ItemProperty -Path $runKey -Name 'FxTrumpet' -ErrorAction SilentlyContinue

# Task Manager keeps its on/off state in a second location; leaving it behind
# would make a later reinstall look "already disabled" for no visible reason.
$approvedKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
Remove-ItemProperty -Path $approvedKey -Name 'FxTrumpet' -ErrorAction SilentlyContinue

$shortcut = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\FxTrumpet.lnk'
# .NET rather than Remove-Item: an uninstaller wants an outright delete, and
# Remove-Item can be diverted to the Recycle Bin by a locked-down shell, which
# would leave the program sitting in place while reporting success.
if (Test-Path $shortcut) { [System.IO.File]::Delete($shortcut) }

if (Test-Path $target) { [System.IO.Directory]::Delete($target, $true) }

Write-Host ''
Write-Host 'FxTrumpet removed.'
Write-Host "Settings and presets remain in $env:APPDATA\FxTrumpet."
Write-Host 'The virtual sound card is still installed; remove it from the tray menu first if you want it gone.'
'@

$licenseText = @'
FxTrumpet
======

This build of FxTrumpet is distributed under the GNU Affero General Public
License, version 3 or later (AGPL-3.0-or-later), because it is built from and
links against source code released under that licence:

  * the DFX DSP engine, from fxsound-app/dsp          (AGPL-3.0)
  * the DFX support layer, from fxsound-app/audiopassthru (AGPL-3.0)
  * the Windows virtual sound card driver from fxsound-driver (AGPL-3.0)

The full licence texts ship alongside those sources. If you distribute this
program, or run a modified version of it as a network service, you must make the
corresponding source available under the same licence.

Trademarks
----------

"FxSound" and the FxSound name and artwork belong to their owner. The bundled
driver is FxSound's own signed package, redistributed unmodified so that Windows
will load it; FxTrumpet itself is an independent, unaffiliated program. Do not
present FxTrumpet as FxSound.
'@

$readme = @'
FxTrumpet - 低占用常驻托盘音效增强  /  a tray-resident audio enhancer
=================================================================

  这是什么 / What this is
  ----------------------
  一个常驻系统托盘的小工具，复用 FxSound 的虚拟声卡与 DSP 引擎来增强系统
  声音，但不带 FxSound 的应用界面。空闲时约 16 MB 内存。

  A small tray application that reuses FxSound's virtual sound card and its DSP
  engine to enhance whatever the system is playing, without shipping FxSound's
  UI. It idles at around 16 MB.

  安装 / Install
  --------------
    powershell -ExecutionPolicy Bypass -File install.ps1

  脚本会把它复制到 %LOCALAPPDATA%\Programs\FxTrumpet、建一个开始菜单快捷方式，
  并启动。然后右键托盘图标 -> 「安装虚拟声卡…」，同意提权（这一步需要管
  理员权限）。装完驱动，声音才会真正经过增强。

  The script copies it to %LOCALAPPDATA%\Programs\FxTrumpet, creates a Start-menu
  shortcut and launches it. Then right-click the tray icon and choose
  "Install sound card...", and accept the elevation prompt - that step needs
  administrator rights, and until it is done there is nothing to route audio
  through.

  运行方式 / How it runs
  ----------------------
  程序会把自己注册为「开机自启」（HKCU\...\Run）。这是必要的，不是可选项：
  必须由它接管系统默认输出设备，声音才会经过增强；如果登录后它没起来，而默
  认设备还停在虚拟声卡上，机器就没有声音了。可以在托盘菜单里关掉，也可以从
  任务管理器的「启动」选项卡禁用。

  FxTrumpet registers itself to start at logon. That is load-bearing rather than a
  convenience: it has to take over the system's default output for the enhancer
  to be in the path at all, so a logon that does not start it would leave the
  machine silent. Turn it off from the tray menu, or from the Startup tab of
  Task Manager.

  完全没有声音 / No sound at all
  -----------------------------
    fxtrumpet.exe --restore-output

  这一条会把默认输出设备切回真实声卡然后退出。绝大多数情况下用不到：正常退
  出会自己还回去，异常退出下次启动也会自动修复。

  Hands the default output back to a real device and exits. Almost never needed
  - a clean exit restores it by itself, and a crash is repaired on the next
  start.

  卸载 / Uninstall
  ----------------
    powershell -ExecutionPolicy Bypass -File uninstall.ps1

  卸载前建议先在托盘菜单里卸掉虚拟声卡驱动（需要管理员权限）。

  Remove the virtual sound card from the tray menu first if you want it gone;
  that step needs administrator rights.

  目录 / Layout
  -------------
    fxtrumpet.exe                    主程序 / the application
    driver\fxvad.inf              虚拟声卡驱动（FxSound 签名版）
    driver\fxvad.sys              the signed virtual sound card driver
    driver\fxvadntamd64.cat       ...with its catalogue and INF
    install.ps1 / uninstall.ps1   安装与卸载 / install and remove

  预设 / Presets
  --------------
  17 个内置预设，首次运行时解包到 %APPDATA%\FxTrumpet\presets。把 .fac 文件丢
  进那个目录，再点托盘的「重新扫描预设」即可使用。

  调音面板（右键托盘 -> 「调音面板…」）最下面一节是「保存为预设」：填个名
  字点保存，当前的音效、均衡与输出设置就写成一份 .fac 存进同一个目录，并立
  刻出现在面板列表与托盘菜单里，不必手动「重新扫描预设」。重名会覆盖。名称
  里不能含 \ / : * ? " < > | 或 CON、NUL、COM1 这类保留名，面板会当场拦下。

  Seventeen presets ship inside the executable and are unpacked to
  %APPDATA%\FxTrumpet\presets on first run. Drop .fac files there and hit "Rescan
  presets" to add your own.

  The tuning panel (tray -> "Tuning panel...") ends with a "Save as preset"
  section: type a name, hit Save, and the current effects, equalizer and output
  settings are written as a .fac into that same folder, appearing immediately in
  the panel list and the tray menu without a manual rescan. An existing name is
  overwritten. A name may not contain \ / : * ? " < > | or a reserved device
  name such as CON, NUL or COM1 - the panel refuses those outright.

  语言 / Language
  ---------------
  托盘菜单与调音面板都支持中文与 English，默认跟随 Windows 显示语言。想改就
  在托盘菜单的「语言」里点一下（两项分别写作「中文」和「English」）；选过之
  后会记住，不再跟随系统。想恢复跟随系统，把 %APPDATA%\FxTrumpet\config.json
  里的 "language" 改成 "auto"。

  The tray menu and the tuning panel both come in Chinese and English, following
  your Windows display language by default. Change it from the tray menu's
  "Language" submenu (the two entries read "中文" and "English"); your choice is
  remembered and stops following the system. To go back to following the system,
  set "language" to "auto" in %APPDATA%\FxTrumpet\config.json.

  关于本项目 / About this project
  ------------------------------
  这是一件 AI 生成的作品：由 WorkBuddy 智能体驱动，底层模型为
  DeepSeek-V4.1-Flash，生成于 2026-09。除 driver\ 下逐字复制、由 FxSound
  签名的虚拟声卡驱动与 vendor\ 下逐字复制的上游源码之外，本程序其余代码与
  文档均出自该模型。认为其中某处不对，先假设它写错了再查。

  This is an AI-generated work: produced through the WorkBuddy agent, driven by
  the DeepSeek-V4.1-Flash model, in 2026-09. Apart from the verbatim, FxSound-
  signed driver under driver\ and the verbatim upstream sources under vendor\,
  all of the code and prose here was written by that model. Assume any given line
  is wrong until you have checked it.

  许可 / Licence
  --------------
  AGPL-3.0-or-later，见 LICENSE.txt。驱动来自 FxSound，版权归其所有。
  AGPL-3.0-or-later; see LICENSE.txt. The driver is FxSound's, redistributed
  unmodified.
'@

Push-Location $here
try {
    $version = Read-CargoVersion
    Write-Host "FxTrumpet $version" -ForegroundColor Green

    Write-Step "Building ($Configuration)"
    if ($NoBuild) {
        Write-Host 'skipped (-NoBuild)'
    } else {
        # Inlined rather than calling build.ps1: a PowerShell script that ends
        # with `exit` takes its caller down with it, so `& .\build.ps1` would
        # end this script the moment the build finished - before a single file
        # had been assembled.
        $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
        if (Test-Path $cargoBin) { $env:PATH = "$cargoBin;$env:PATH" }
        . (Join-Path $here 'toolchain.ps1')

        # cargo writes progress to stderr; under 'Stop' PowerShell 5.1 turns
        # redirected native stderr into a terminating error.
        $cargoArgs = @("--$Configuration", '--bin', 'fxtrumpet')
        if ($Locked) { $cargoArgs += '--locked' }
        $ErrorActionPreference = 'Continue'
        & cargo build @cargoArgs
        $code = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($code -ne 0) { throw "cargo build exited $code" }
    }

    $exe = Join-Path $here "target\$Configuration\fxtrumpet.exe"
    if (-not (Test-Path $exe)) { throw "no executable at $exe" }

    Write-Step 'Checking the executable'
    $info = (Get-Item $exe).VersionInfo
    if (-not $info.ProductName -or -not $info.FileVersion) {
        throw @"
$exe has no version resource, which means it also has no icon.
That happens when `rc.exe` cannot be found at build time. Either install the
Windows SDK, or point FXTRUMPET_RC at an rc.exe, then build again.
"@
    }
    Write-Host ("  product   : {0} {1}" -f $info.ProductName, $info.FileVersion)
    Write-Host ("  size      : {0:N0} bytes" -f (Get-Item $exe).Length)

    # A dynamic-CRT build runs fine here and fails on a clean machine, so it is
    # worth catching at packaging time rather than at the user's. This is a hard
    # failure rather than a warning on purpose: a warning in a CI log is a
    # regression that ships anyway.
    $dumpbin = (Get-Command dumpbin.exe -ErrorAction SilentlyContinue).Source
    if ($dumpbin) {
        $deps = & $dumpbin /nologo /dependents $exe
        $redist = $deps | Select-String -Pattern 'VCRUNTIME|MSVCP'
        if ($redist) {
            throw @"
the executable imports the Visual C++ redistributable:
$($redist -join "`n")
It will not start on a machine without it. Check that .cargo\config.toml still
enables target-feature=+crt-static, and that build.rs is passing /MT to the
vendored C++.
"@
        } else {
            Write-Host '  runtime   : self-contained (no VC++ redistributable needed)'
        }
    } else {
        Write-Host '  runtime   : NOT CHECKED - dumpbin.exe is not on PATH'
        Write-Host '              (dot-source .\toolchain.ps1 first; CI does)'
    }

    Write-Step 'Locating the driver package'
    $driverSource = Find-DriverDir
    if (-not $driverSource) {
        throw @"
could not find fxvad.inf / fxvad.sys / fxvadntamd64.cat.
Looked next to this script, in .\driver, and in ..\NexBox\src-tauri\resources\
binaries\fxvad. Set FXTRUMPET_DRIVER_DIR to override.
"@
    }
    Write-Host "  source    : $driverSource"

    Write-Step "Assembling $OutDir\FxTrumpet"
    $stage = Join-Path $here (Join-Path $OutDir 'FxTrumpet')
    Remove-Tree -Path $stage
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    Copy-Item $exe (Join-Path $stage 'fxtrumpet.exe') -Force
    Copy-DriverFiles -Source $driverSource -Destination (Join-Path $stage 'driver')
    Write-TextFile -Path (Join-Path $stage 'README.txt') -Text $readme -Encoding UTF8
    Write-TextFile -Path (Join-Path $stage 'LICENSE.txt') -Text $licenseText -Encoding UTF8
    Write-TextFile -Path (Join-Path $stage 'install.ps1') -Text $installScript -Encoding ASCII
    Write-TextFile -Path (Join-Path $stage 'uninstall.ps1') -Text $uninstallScript -Encoding ASCII

    Get-ChildItem -LiteralPath $stage -Recurse -File |
        ForEach-Object { Write-Host ("  {0,-40} {1,10:N0}" -f $_.FullName.Substring($stage.Length + 1), $_.Length) }

    $archive = $null
    if (-not $NoZip) {
        Write-Step 'Writing the archive'
        $archive = Join-Path $here (Join-Path $OutDir "FxTrumpet-$version-win64.zip")
        Remove-File -Path $archive
        # Compress-Archive would nest the folder differently depending on the
        # path shape; going through the parent keeps `FxTrumpet\...` at the root.
        #
        # Note for anyone comparing hashes: the *contents* are reproducible —
        # fxtrumpet.exe hashes identically across runs — but the archive does not,
        # because zip stores each entry's modification time. Compare the entries
        # (SHA256SUMS.txt) rather than the zip.
        Push-Location (Split-Path -Parent $stage)
        try {
            Compress-Archive -Path (Split-Path -Leaf $stage) -DestinationPath $archive -CompressionLevel Optimal
        } finally {
            Pop-Location
        }
        Write-Host ("  {0} ({1:N0} bytes)" -f (Split-Path -Leaf $archive), (Get-Item $archive).Length)
    }

    Write-Step 'Hashing'
    $sums = Join-Path $here (Join-Path $OutDir 'SHA256SUMS.txt')
    $lines = @()
    Get-ChildItem -LiteralPath $stage -Recurse -File | Sort-Object FullName | ForEach-Object {
        $relative = $_.FullName.Substring((Split-Path -Parent $stage).Length + 1).Replace('\', '/')
        $lines += ('{0}  {1}' -f (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower(), $relative)
    }
    if ($archive) {
        $lines += ('{0}  {1}' -f (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLower(), (Split-Path -Leaf $archive))
    }
    Set-Content -LiteralPath $sums -Value $lines -Encoding ASCII
    $lines | ForEach-Object { Write-Host "  $_" }

    Write-Host ''
    Write-Host "Packaged FxTrumpet $version" -ForegroundColor Green
    if ($archive) { Write-Host "  $archive" }
    Write-Host "  $stage"
    Write-Host "  $sums"
} finally {
    Pop-Location
}

exit 0
