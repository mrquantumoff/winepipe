#!/bin/sh
# Build WinePipe and install it for the Wine prefix in $WINEPREFIX.
#
#   WINEPREFIX=~/.wine-daw [WINE=/path/to/bin/wine] scripts/install-to-prefix.sh [options]
#
# 1. Builds the driver (and the settings app) from this source tree.
# 2. Installs the driver into the library directory of the Wine that runs
#    the prefix ($WINE, default `wine` on PATH), so applications find it
#    without any environment variables. Uses sudo if that directory isn't
#    writable (e.g. /usr/lib/wine).
# 3. Installs winepipe-settings, winepipe-register and winepipe-config into
#    ~/.local (see --tools-prefix).
# 4. Registers the 64-bit and 32-bit driver in the prefix.
#
# Options:
#   --no-settings        don't build/install the settings app
#   --no-sudo            if Wine's library dir isn't writable, install the
#                        driver into ~/.local/lib/winepipe instead (then run
#                        applications with WINEDLLPATH set; printed at the end)
#   --tools-prefix DIR   where bin/ and share/ tools go (default ~/.local)
#   --check              run the test host in the prefix afterwards (2 s,
#                        silent, no autoconnect; briefly sets the graph quantum)
#   -h, --help           show this help
#
# Safe to run while applications in the prefix are open: files are replaced
# atomically. Restart them to load the new driver.
set -eu

die() { echo "install-to-prefix: $*" >&2; exit 1; }
say() { printf '==> %s\n' "$*"; }
# Run quietly; show the output only if it fails.
quiet() {
    log=$(mktemp)
    if ! "$@" >"$log" 2>&1; then
        tail -n 40 "$log" >&2
        rm -f "$log"
        die "failed: $*"
    fi
    rm -f "$log"
}

root=$(cd "$(dirname "$0")/.." && pwd)
settings=1
use_sudo=1
check=0
tools="$HOME/.local"

while [ $# -gt 0 ]; do
    case "$1" in
        --no-settings) settings=0 ;;
        --no-sudo) use_sudo=0 ;;
        --tools-prefix) tools=${2:?--tools-prefix needs a directory}; shift ;;
        --check) check=1 ;;
        -h|--help) sed -n '2,29s/^# \{0,1\}//p' "$0"; exit 0 ;;
        *) die "unknown option $1 (see --help)" ;;
    esac
    shift
done

[ -n "${WINEPREFIX:-}" ] || die "set WINEPREFIX to the prefix to install into"
prefix=$(cd "$WINEPREFIX" 2>/dev/null && pwd) || die "$WINEPREFIX does not exist"
[ -f "$prefix/system.reg" ] || die "$prefix is not an initialized Wine prefix (run: WINEPREFIX=$prefix wineboot -i)"

# --- locate the Wine installation ------------------------------------------
WINE=${WINE:-wine}
wine_path=$(command -v "$WINE" 2>/dev/null) || die "Wine binary '$WINE' not found (set WINE=/path/to/bin/wine)"
wine_real=$(readlink -f "$wine_path")
wine_root=$(dirname "$(dirname "$wine_real")")
libdir=
for d in "$wine_root/lib/wine" "$wine_root/lib64/wine" "$wine_root/lib/x86_64-linux-gnu/wine"; do
    if [ -f "$d/x86_64-unix/ntdll.so" ]; then libdir=$d; break; fi
done
[ -n "$libdir" ] || die "cannot find the library directory of $wine_real (no x86_64-unix/ntdll.so under $wine_root)"
[ -d "$libdir/i386-windows" ] || say "note: $libdir has no i386-windows; only the 64-bit driver will be usable"
say "prefix:   $prefix"
say "wine:     $wine_real ($("$wine_real" --version 2>/dev/null || echo unknown version))"
say "wine lib: $libdir"

# --- build ------------------------------------------------------------------
if command -v rustup >/dev/null 2>&1; then
    installed=$(rustup target list --installed)
    for t in x86_64-pc-windows-gnu i686-pc-windows-gnu; do
        echo "$installed" | grep -qx "$t" || { say "adding Rust target $t"; rustup target add "$t"; }
    done
fi
command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 || die "mingw-w64 is missing (x86_64-w64-mingw32-gcc)"
command -v winebuild >/dev/null 2>&1 || die "winebuild is missing (part of the wine package)"

say "building the driver"
quiet make -C "$root" driver
if [ $settings = 1 ]; then
    say "building the settings app (the first build takes a few minutes)"
    quiet make -C "$root" settings
fi

# --- install the driver -------------------------------------------------------
# install(1) writes a new file and renames it into place, so running
# applications keep the old copy mapped.
dest=$libdir
sudo_cmd=
if [ ! -w "$libdir/x86_64-unix" ] || [ ! -w "$libdir/x86_64-windows" ]; then
    if [ $use_sudo = 1 ] && command -v sudo >/dev/null 2>&1; then
        say "$libdir is not writable; using sudo"
        sudo_cmd=sudo
    else
        dest="$HOME/.local/lib/winepipe"
        say "$libdir is not writable; installing the driver into $dest"
    fi
fi
say "installing the driver into $dest"
$sudo_cmd install -Dm755 "$root/build/x86_64-unix/winepipe.so" "$dest/x86_64-unix/winepipe.so"
$sudo_cmd install -Dm644 "$root/build/x86_64-windows/winepipe.dll" "$dest/x86_64-windows/winepipe.dll"
$sudo_cmd install -Dm644 "$root/build/i386-windows/winepipe.dll" "$dest/i386-windows/winepipe.dll"

# --- tools ------------------------------------------------------------------
say "installing tools into $tools"
install -Dm755 "$root/scripts/winepipe-register" "$tools/bin/winepipe-register"
install -Dm755 "$root/scripts/winepipe-config" "$tools/bin/winepipe-config"
if [ $settings = 1 ]; then
    install -Dm755 "$root/build/bin/winepipe-settings" "$tools/bin/winepipe-settings"
    install -Dm644 "$root/data/io.github.mrquantumoff.WinePipe.desktop" \
        "$tools/share/applications/io.github.mrquantumoff.WinePipe.desktop"
fi

# --- register -----------------------------------------------------------------
say "registering in $prefix"
WINEPREFIX=$prefix WINE=$wine_real WINEDEBUG=-all "$root/scripts/winepipe-register" "$dest" 2>&1 |
    grep -vE 'MESA|pci id|^regsvr32:|^note: run applications' || true

if [ $check = 1 ]; then
    say "checking: 2 s test run in the prefix"
    dllpath=
    [ "$dest" = "$libdir" ] || dllpath=$dest
    out=$(env WINEPREFIX="$prefix" WINEDEBUG=-all WINEPIPE_AUTOCONNECT=0 WINEDLLPATH="$dllpath" \
        timeout 60 "$wine_real" "$root/build/bin/winepipe-test64.exe" --seconds 2 2>&1) || true
    echo "$out" | grep -E '^(driver:|callbacks|RESULT|FAIL)' | sed 's/^/    /'
    echo "$out" | grep -q '^RESULT: OK' || die "test run failed"
fi

say "done. Pick \"WinePipe\" as the ASIO device; restart applications that were already running."
case ":$PATH:" in *":$tools/bin:"*) ;; *) say "note: $tools/bin is not on your PATH" ;; esac
if [ "$dest" != "$libdir" ]; then
    say "run applications with: WINEDLLPATH=$dest"
fi
