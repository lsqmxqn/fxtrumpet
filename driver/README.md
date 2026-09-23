# driver/ — the virtual sound card (FxSound's signed package)

Three files, 343 KB in total, redistributed **byte-for-byte unmodified**:

| File | Size | What it is |
|---|---|---|
| `fxvad.inf` | 5,334 | installation instructions for the virtual audio device |
| `fxvad.sys` | 326,656 | the kernel-mode driver |
| `fxvadntamd64.cat` | 10,590 | the signature catalogue (x64) |

## Where they come from

Built and signed by FxSound, published in
[`fxsound2/fxsound-driver`](https://github.com/fxsound2/fxsound-driver) (the repository
holds the **source**; the binaries here were taken from FxSound's signed distribution).

They are not buildable from that source without the Windows Driver Kit **and** a code
signing certificate that Windows will trust for kernel-mode code. That is exactly why
FxTrumpet reuses FxSound's package instead of producing its own.

## Why they are committed rather than fetched

`package.ps1` needs all three to assemble a release, and CI has no way to reach the
sibling `NexBox` checkout the script used to fall back to. Committing them makes the
repository self-sufficient: `git clone` → `package.ps1` produces a complete, installable
zip with no extra inputs.

`Find-DriverDir` still accepts an override — `FXTRUMPET_DRIVER_DIR`, or a `driver/` folder
next to `package.ps1` — so an updated driver can be dropped in without editing anything.

## Do not reformat these files

`fxvadntamd64.cat` **signs the hashes of `fxvad.inf` and `fxvad.sys`**. Change one byte of
either and Windows rejects the driver, which surfaces to the user as "the driver would not
install" with no useful reason.

`fxvad.inf` is the trap: it is plain ASCII with CRLF line endings, so a careless
`text=auto` in `.gitattributes` would quietly normalise it to LF on the way into the
repository and back out again. All three extensions are therefore pinned to `binary` in
`.gitattributes`. If you ever replace these files, confirm nothing was normalised:

```powershell
git check-attr binary -- driver/fxvad.inf    # expect: driver/fxvad.inf: binary: set
git hash-object driver/fxvad.inf             # blob hash of the raw bytes
git hash-object ..\wherever\you\got\fxvad.inf # must print the same hash
```

## Licence and trademarks

The driver is licensed **AGPL-3.0** by FxSound LLC — see the upstream repository's
`LICENSE` (the same text FxSound ships alongside it). FxTrumpet's own root `LICENSE`
(AGPL-3.0) covers the rest of this repository; the driver keeps its own terms even though
they happen to agree.

**"FxSound" and the FxSound name and artwork belong to their owner.** FxTrumpet is an
independent, unaffiliated project. Do not present it as FxSound. The packaging scripts
ship this driver unmodified precisely so that it remains recognisably FxSound's, with its
signature intact — see the trademark section of `dist/FxTrumpet/LICENSE.txt`.
