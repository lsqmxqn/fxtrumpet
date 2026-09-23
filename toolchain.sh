#!/usr/bin/env bash
# FxTrumpet — Git Bash / MSYS equivalent of toolchain.ps1.
#
#   source toolchain.sh
#   cargo build --release --bin dspcheck
#
# See toolchain.ps1 for the full explanation. The short version: this machine's
# Visual Studio lives at D:\Program Files\VisualStudio, is not registered with
# the installer, and Git for Windows ships a coreutils `link.exe` that shadows
# MSVC's linker. Both are handled by prepending the MSVC bin directory to PATH
# and exporting INCLUDE/LIB explicitly.

FXTRUMPET_MSVC_ROOT="${FXTRUMPET_MSVC_ROOT:-/d/Program Files/VisualStudio/VC/Tools/MSVC}"
FXTRUMPET_SDK_ROOT="${FXTRUMPET_SDK_ROOT:-/c/Program Files (x86)/Windows Kits/10}"
FXTRUMPET_ARCH="${FXTRUMPET_ARCH:-x64}"
FXTRUMPET_HOST_ARCH="${FXTRUMPET_HOST_ARCH:-Hostx64}"

_fxtrumpet_fail() { echo "toolchain.sh: $*" >&2; return 1; }

# Pick the newest version that actually contains the piece we need, so a
# partially installed toolset (headers but no libraries) is never selected.
_fxtrumpet_newest() {
    local root="$1" probe="$2"
    [ -d "$root" ] || _fxtrumpet_fail "not found: $root" || return 1
    local best="" best_key=""
    local dir name key
    for dir in "$root"/*/; do
        [ -d "$dir" ] || continue
        if [ -n "$probe" ] && [ ! -e "$dir$probe" ]; then continue; fi
        name="$(basename "$dir")"
        key="$(printf '%s' "$name" | tr -cd '0-9.')"
        if [ -z "$best_key" ] || [ "$(printf '%s\n%s\n' "$best_key" "$key" | sort -V | tail -1)" = "$key" ]; then
            best="$dir"; best_key="$key"
        fi
    done
    [ -n "$best" ] || { _fxtrumpet_fail "no usable version under $root"; return 1; }
    printf '%s' "${best%/}"
}

FXTRUMPET_MSVC="$(_fxtrumpet_newest "$FXTRUMPET_MSVC_ROOT" "lib/$FXTRUMPET_ARCH")" || return 1
FXTRUMPET_SDK_INC="$(_fxtrumpet_newest "$FXTRUMPET_SDK_ROOT/Include" "")" || return 1
FXTRUMPET_SDK_VER="$(basename "$FXTRUMPET_SDK_INC")"

[ -d "$FXTRUMPET_SDK_ROOT/Lib/$FXTRUMPET_SDK_VER/um/$FXTRUMPET_ARCH" ] \
    || _fxtrumpet_fail "SDK $FXTRUMPET_SDK_VER has no Lib/.../um/$FXTRUMPET_ARCH" || return 1

FXTRUMPET_VC_BIN="$FXTRUMPET_MSVC/bin/$FXTRUMPET_HOST_ARCH/$FXTRUMPET_ARCH"
[ -x "$FXTRUMPET_VC_BIN/cl.exe" ] || _fxtrumpet_fail "cl.exe not found in $FXTRUMPET_VC_BIN" || return 1

# MSVC's bin MUST come first, otherwise coreutils' link.exe /usr/bin/link.exe wins.
export PATH="$FXTRUMPET_VC_BIN:$FXTRUMPET_SDK_ROOT/bin/$FXTRUMPET_SDK_VER/$FXTRUMPET_ARCH:$PATH"

# cl.exe and link.exe read Windows-style paths from these.
_fxtrumpet_win() { printf '%s' "$1" | sed 's|^/\([a-z]\)/|\U\1:\\|; s|/|\\|g'; }

export INCLUDE="$(_fxtrumpet_win "$FXTRUMPET_MSVC/include");$(_fxtrumpet_win "$FXTRUMPET_SDK_ROOT/Include/$FXTRUMPET_SDK_VER/ucrt");$(_fxtrumpet_win "$FXTRUMPET_SDK_ROOT/Include/$FXTRUMPET_SDK_VER/um");$(_fxtrumpet_win "$FXTRUMPET_SDK_ROOT/Include/$FXTRUMPET_SDK_VER/shared");$(_fxtrumpet_win "$FXTRUMPET_SDK_ROOT/Include/$FXTRUMPET_SDK_VER/winrt");$(_fxtrumpet_win "$FXTRUMPET_SDK_ROOT/Include/$FXTRUMPET_SDK_VER/cppwinrt")"

export LIB="$(_fxtrumpet_win "$FXTRUMPET_MSVC/lib/$FXTRUMPET_ARCH");$(_fxtrumpet_win "$FXTRUMPET_SDK_ROOT/Lib/$FXTRUMPET_SDK_VER/ucrt/$FXTRUMPET_ARCH");$(_fxtrumpet_win "$FXTRUMPET_SDK_ROOT/Lib/$FXTRUMPET_SDK_VER/um/$FXTRUMPET_ARCH")"

# cc-rs probes vswhere, which cannot see this installation; point it at cl.exe directly.
export CC=cl.exe
export CXX=cl.exe

_which_link="$(command -v link.exe)"
echo "FxTrumpet toolchain ready"
echo "  MSVC toolset : $FXTRUMPET_MSVC"
echo "  Windows SDK  : $FXTRUMPET_SDK_VER"
echo "  cl.exe       : $FXTRUMPET_VC_BIN/cl.exe"
echo "  link.exe     : $_which_link"
case "$_which_link" in
    "$FXTRUMPET_VC_BIN"/*) ;;
    *) echo "  WARNING: link.exe is not MSVC's — linking will fail." >&2 ;;
esac
echo
echo "Next: cargo build --release --bin dspcheck"

unset _which_link _fxtrumpet_fail
