#!/usr/bin/env bash
# Run a command (normally the app in smoke mode) on a headless virtual Wayland display, so test
# windows never show up on (or take focus from) the real desktop.
#
#   scripts/headless.sh env YTM_SMOKE=home YTM_SMOKE_SHOT=/tmp/x.png target/release/ytm-native
#   scripts/headless.sh --stop
#
# The display is a `kwin_wayland --virtual` (1280x800) on its own socket and its own D-Bus
# session, started on first use and left running. The app itself stays on the normal session
# bus, because it needs KWallet for the login key. Size: YTM_HEADLESS_SIZE=<w>x<h>, at start.
set -euo pipefail

sock=ytm-headless
runtime=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}

if [[ ${1:-} == --stop ]]; then
    pkill -f "kwin_wayland --virtual --socket $sock" || true
    for _ in $(seq 1 50); do
        pgrep -f "kwin_wayland --virtual --socket $sock" > /dev/null || break
        sleep 0.1
    done
    exit 0
fi

if ! pgrep -f "kwin_wayland --virtual --socket $sock" > /dev/null; then
    size=${YTM_HEADLESS_SIZE:-1280x800}
    log="$runtime/$sock.log"
    # A KWin that was killed leaves its socket behind; it would look ready before the new one is.
    [[ -S $runtime/$sock ]] && mv -f "$runtime/$sock" "$runtime/$sock.stale"
    [[ -e $runtime/$sock.lock ]] && mv -f "$runtime/$sock.lock" "$runtime/$sock.lock.stale"
    env -u DISPLAY WAYLAND_DISPLAY= setsid dbus-run-session -- \
        kwin_wayland --virtual --socket "$sock" --width "${size%x*}" --height "${size#*x}" \
        --no-lockscreen --no-global-shortcuts --no-kactivities > "$log" 2>&1 &
    for _ in $(seq 1 100); do
        [[ -S $runtime/$sock ]] && break
        sleep 0.1
    done
    [[ -S $runtime/$sock ]] || { echo "headless KWin didn't start, see $log" >&2; exit 1; }
fi

[[ $# -gt 0 ]] || exit 0
exec env -u DISPLAY WAYLAND_DISPLAY="$sock" "$@"
