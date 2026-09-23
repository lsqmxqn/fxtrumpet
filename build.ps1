# FxTrumpet — one-command build.
#
#   .\build.ps1                            # build the dspcheck smoke test
#   .\build.ps1 --release --bin fxtrumpet     # build something else
#   .\build.ps1 --release --bin dspcheck -v
#
# Equivalent to:
#
#   . .\toolchain.ps1
#   cargo build --release --bin dspcheck
#
# but with the two papercuts already handled:
#
#   * cargo lives in %USERPROFILE%\.cargo\bin, which is not always on PATH in a
#     fresh non-interactive shell;
#   * cargo writes progress to stderr, and PowerShell 5.1 with
#     $ErrorActionPreference='Stop' turns redirected native stderr into a
#     terminating NativeCommandError. toolchain.ps1 now restores the caller's
#     preference for exactly this reason, and we set it here too so the script
#     is safe however it is launched.
#
# Arguments are passed straight through to cargo, prefixed with `build`.

param(
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs
)

$ErrorActionPreference = 'Stop'

$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path $cargoBin) { $env:PATH = "$cargoBin;$env:PATH" }

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $here 'toolchain.ps1')

if (-not $CargoArgs -or $CargoArgs.Count -eq 0) {
    $CargoArgs = @('--release', '--bin', 'dspcheck')
}

$ErrorActionPreference = 'Continue'
Push-Location $here
try {
    & cargo build @CargoArgs
    $code = $LASTEXITCODE
} finally {
    Pop-Location
}

if ($code -ne 0) {
    Write-Host "cargo exited $code" -ForegroundColor Red
}
exit $code
