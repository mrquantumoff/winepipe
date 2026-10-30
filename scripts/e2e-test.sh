#!/bin/sh
# End-to-end test: register WinePipe in a scratch prefix and run the test
# host (64- and 32-bit) under Wine.
#
# By default the tests run against a private, throwaway PipeWire daemon (its
# own socket, Dummy-Driver clock, no devices), so they never touch your
# desktop audio graph. That also lets the suite kill the server mid-stream to
# check the reset path. `--live` runs against your session's PipeWire
# instead. The driver is not autoconnected and outputs silence there, but the
# graph quantum is forced while each test runs.
#
# Set WINEPREFIX to reuse a prefix; the default is ~/.cache/winepipe-prefix.
set -eu
live=0
[ "${1:-}" = "--live" ] && live=1
root=$(cd "$(dirname "$0")/.." && pwd)
export WINEPREFIX=${WINEPREFIX:-$HOME/.cache/winepipe-prefix}
export WINEDEBUG=-all WINEDLLPATH="$root/build" WINEPIPE_AUTOCONNECT=0
WINE=${WINE:-wine}

if [ ! -d "$WINEPREFIX/drive_c/windows/system32" ]; then
    echo "creating Wine prefix $WINEPREFIX"
    mkdir -p "$WINEPREFIX"
    "$WINE" wineboot -i >/dev/null 2>&1
fi
"$root/scripts/winepipe-register" "$root/build" >/dev/null

pw_pid=
start_server() {
    export PIPEWIRE_REMOTE="winepipe-e2e-$$"
    PIPEWIRE_CORE=$PIPEWIRE_REMOTE pipewire >/dev/null 2>&1 &
    pw_pid=$!
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        pw-cli info 0 >/dev/null 2>&1 && return 0
        sleep 0.2
    done
    echo "could not start a private PipeWire daemon" >&2
    exit 1
}
stop_server() {
    [ -n "$pw_pid" ] && kill "$pw_pid" 2>/dev/null && wait "$pw_pid" 2>/dev/null || true
    pw_pid=
}
trap stop_server EXIT

if [ $live = 0 ]; then
    start_server
    echo "using private PipeWire daemon $PIPEWIRE_REMOTE"
else
    echo "using the session PipeWire daemon"
fi

failed=0
run() {
    exe=$1; shift
    printf '%-22s %-28s ' "$exe" "$*"
    out=$(timeout 120 "$WINE" "$root/build/bin/$exe" "$@" 2>&1) || true
    if echo "$out" | grep -q '^RESULT: OK'; then
        echo "$out" | awk '/^driver: cycles/ {printf "ok  (%s late=%s skipped=%s)\n", $3" cycles", $7, $9}'
    else
        echo "FAIL"
        echo "$out" | grep -vE 'MESA|pci id' | sed 's/^/    /'
        failed=1
    fi
}

run winepipe-test64.exe --buffer 256 --seconds 3
run winepipe-test64.exe --buffer 64 --seconds 3
run winepipe-test64.exe --buffer 32 --seconds 2 --output-ready
run winepipe-test64.exe --buffer 128 --seconds 1 --cycles 3
run winepipe-test64.exe --buffer 256 --seconds 2 --no-timeinfo
run winepipe-test32.exe --buffer 256 --seconds 3
run winepipe-test32.exe --buffer 64 --seconds 2 --cycles 2
WINEPIPE_SAMPLE_TYPE=1 run winepipe-test64.exe --buffer 128 --seconds 2
run winepipe-test64.exe --buffer 128 --seconds 2 --stall-ms 100

if [ $live = 0 ]; then
    # Kill the server mid-stream: the host must get exactly one
    # kAsioResetRequest and every driver call must still return.
    printf '%-22s %-28s ' winepipe-test64.exe "(server killed after 1.5 s)"
    out=$(mktemp)
    timeout 60 "$WINE" "$root/build/bin/winepipe-test64.exe" --buffer 128 --seconds 3 >"$out" 2>&1 &
    t=$!
    sleep 1.5
    kill -9 "$pw_pid"; wait "$pw_pid" 2>/dev/null || true; pw_pid=
    wait $t || true
    if grep -q '^events: resets 1 ' "$out" && grep -q '^RESULT:' "$out"; then
        echo "ok  (one reset request, clean shutdown)"
    else
        echo "FAIL"; grep -vE 'MESA|pci id' "$out" | sed 's/^/    /'; failed=1
    fi
    rm -f "$out"
fi

[ $failed = 0 ] && echo "all end-to-end tests passed" || { echo "end-to-end tests FAILED"; exit 1; }
