# Vendored-source patches

`vendor.ps1` mirrors upstream sources verbatim, then applies the fixes below.
Each one is applied by matching a unique anchor line and is verified — if the
anchor is missing the script throws rather than silently vendoring broken code.

Anything here is a fix to an upstream defect, not a local preference. Keep the
list short, and report them upstream where possible.

This file has two parts:

- **Applied patches** — defects we do patch, because they break FxTrumpet today.
- **Known defects, deliberately unpatched** — real upstream bugs we have chosen
  not to touch, with an explicit note on what would change that decision. Recording
  them keeps the knowledge from being re-derived, without growing the patch surface.

**Applied patch status**: patch 1 is verified — `dspcheck` exits 0 on 14
consecutive runs where it previously died with 139 (access violation) on roughly
4 out of every 5.

---

## 1. `preset_list_handle_` is never initialised

**File**: `dsp/DfxDspPrivate.cpp`, constructor
**Severity**: intermittent crash on shutdown (~80% of runs in testing)

`DfxDspPrivate` declares four pointer members:

```cpp
// u_DfxDsp.h:135-138
int *dfxp_handle_;
int *preset_list_handle_;
int *midi_to_rval_qnt_handle_;
int *rval_to_midi_qnt_handle_;
```

The constructor initialises three of them:

```cpp
dfxp_handle_ = NULL;
slout1_ = NULL;
midi_to_rval_qnt_handle_ = NULL;
rval_to_midi_qnt_handle_ = NULL;
```

`preset_list_handle_` is missing. It is never assigned anywhere in the project —
the preset-list feature it belonged to was disabled, but its teardown was left
behind:

```cpp
// DfxDspPrivate.cpp:124-129, the destructor
if (preset_list_handle_ != NULL)
{
    if (prelstFreeUp(&(preset_list_handle_)) != OKAY) { }
}
```

`data_ = new DfxDspPrivate();` (DfxDsp.cpp:26) does not zero-initialise, so that
member holds whatever the allocator left in the heap slot. When the garbage
happens to be non-zero — which it usually is — the destructor passes a wild
pointer to `prelstFreeUp()` and the process dies with an access violation.

Diagnosis, for reference: the crash is inside `drop(dsp)`, after the caller has
printed everything. `being_destroyed_` rules out a background thread, there are
no thread or timer creations anywhere in the engine, and `dfxp_handle_` is
`calloc`-allocated so `free()` on it is correct. That leaves an uninitialised
pointer, which the three references above pin down exactly.

**Fix** — one line in the constructor:

```cpp
rval_to_midi_qnt_handle_ = NULL;
preset_list_handle_ = NULL;   // added
```

Setting it to NULL is safe and loses nothing: since the member is never
assigned, there is never anything for `prelstFreeUp()` to release.

**Cross-check that the member really is write-only from the destructor's side.**
The only `prelstCreate()` call anywhere in the tree sits inside a block comment —
the disabled legacy `DfxDsp::initPresets` at `DfxDspPreset.cpp:499-548`:

```cpp
/*
int DfxDsp::initPresets(...)
{
    ...
    if (prelstCreate(&preset_handle_, NULL, ...) != OKAY)   // line 519
        return(NOT_OKAY);
    ...
}
*/
```

Two things follow. The preset-list feature is switched off rather than deleted,
which is why the teardown survived; and that dead code passes a member
(`preset_handle_`) that does not even exist on `DfxDspPrivate` — the class has
exactly four handles (`u_DfxDsp.h:135-138`), and `preset_list_handle_` is not
the same name. It was mechanically duplicated from the legacy `dfxg` code and
never compiled in this class. So there is no live path that could ever populate
the member, and NULL really is the correct value.

**Remove this patch when** upstream initialises the member, or when the
vestigial `prelstFreeUp` call is deleted outright.

---

# Known defects, deliberately unpatched

## 2. `.fac` read path leaks on every error path, and discards the reason

**Files**: `dsp/ptutil/VALS/Valsfile.cpp` (`valsRead`, lines 266–507);
`dsp/DfxDspPreset.cpp` (`loadPreset`, lines 58–97)
**Severity**: resource leak on failure; silent failure to the caller
**Status**: documented only — no patch applied, on purpose

### The leak

`valsRead()` allocates its handle at line 291 (`calloc`) and opens the file at
line 307. Everything after that has **ten** early `return(NOT_OKAY)` sites:

```
309  322  338  389  415  446  465  478  496  498
```

- **All ten leak `cast_handle`.** The local is never freed, and `*hpp_vals` is
  written only on the success path (line 504) — so the allocation is unreachable
  from anywhere once the function returns early.
- **Eight of them also leak the open `FILE *stream`** (322, 389, 415, 446, 465,
  478, 496, 498). Only line 338 closes it first — evidence the author knew the
  cleanup was needed but applied it inconsistently.

`loadPreset()` contributes one more:

```cpp
PT_HANDLE *new_vals_hdl;                                    // uninitialised
if (valsRead(..., &new_vals_hdl) != OKAY)
    return(NOT_OKAY);                                       // line 67
...
if (getStateInfoFromVals(new_vals_hdl, true) != OKAY)
    return(NOT_OKAY);                                       // line 71  ← leaks new_vals_hdl
...
if (valsFreeUp(&new_vals_hdl) != OKAY)                      // line 93, success path only
```

Line 71 is the one that matters: the handle *was* successfully allocated,
`getStateInfoFromVals()` does not free its argument (its body contains no
`valsFreeUp` and no raw `free()`), and the function returns before line 93.

### Why we are not patching it yet

The cost is currently bounded. FxTrumpet loads a preset once at startup, and a
failure simply falls back to a built-in default — a one-off leak of one small
struct is not a correctness problem. Rewriting ten return sites in vendored code
is a large diff to carry for a defect that is dormant, and this file exists to
keep the patch list short. **The real gap is diagnosability, not memory.**

### What would change that decision

- preset enumeration or live preset switching that calls `loadPreset` repeatedly
  (the M4 panel), where the leak would accumulate;
- loading user-supplied `.fac` files, where a corrupt or hostile file walks
  straight into these paths;
- upstream fixing it — then this note can simply be deleted.

### The fix to apply when we do take it

Collapse the ten returns into a single cleanup exit — `goto cleanup` with
`if (stream) fclose(stream); free(cast_handle);` — and fast-forward
`valsFreeUp(&new_vals_hdl)` to before the line-71 return.

### Related: failures lose their reason

Because `loadPreset()` reduces every failure to a bare `NOT_OKAY`, a caller
cannot distinguish "file missing" from "not a `.fac` file" from "truncated EQ
section". If we want readable errors, the cheapest route is to validate in our
own layer — path exists, first line is `CLASS1 : Effect Type`, version parses —
rather than to instrument upstream. Note `valsRead()` reads the preset name from
line 3 as **UTF-8** (line 318–321) and converts with
`pstrCovertUTF8StringToWideCharString_WithAlloc`, which is why a GBK console
shows Chinese preset names as mojibake while the engine holds them correctly.
