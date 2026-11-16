#!/bin/sh
# Check a live GAVNO session end to end and write a report.
#   scripts/selftest.sh          automatic checks plus a few yes/no questions
#   scripts/selftest.sh --auto   automatic checks only (no windows, no prompts)
# Run it as your user inside the GAVNO session. Nothing is changed: test
# windows and notifications are closed again, Classic is switched back.
set -u
AUTO=0
[ "${1:-}" = --auto ] && AUTO=1
REPORT="${XDG_STATE_HOME:-$HOME/.local/state}/gavno/selftest-$(date +%Y%m%d-%H%M%S).txt"
mkdir -p "$(dirname "$REPORT")"
PASS=0 FAIL=0 SKIP=0

log() { printf '%s\n' "$*" | tee -a "$REPORT"; }
pass() { PASS=$((PASS + 1)); log "PASS  $1${2:+: $2}"; }
fail() { FAIL=$((FAIL + 1)); log "FAIL  $1${2:+: $2}"; }
skip() { SKIP=$((SKIP + 1)); log "SKIP  $1${2:+: $2}"; }
have() { command -v "$1" >/dev/null 2>&1; }
# ask "question" -> 0 for yes; skipped (returns 2) in --auto mode
ask() {
    [ "$AUTO" = 1 ] && return 2
    printf '%s [y/n] ' "$1" > /dev/tty
    read -r answer < /dev/tty
    case "$answer" in y | Y | yes) return 0 ;; *) return 1 ;; esac
}
SOCK="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/gavno/ipc.sock"
ipc() {
    params=${2:-}
    [ -n "$params" ] || params='{}'
    python3 - "$SOCK" "$1" "$params" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(5)
s.connect(sys.argv[1])
f = s.makefile("rw")
f.write(json.dumps({"id": 1, "method": sys.argv[2], "params": json.loads(sys.argv[3])}) + "\n")
f.flush()
reply = json.loads(f.readline())
if "error" in reply:
    print(reply["error"], file=sys.stderr)
    sys.exit(1)
print(json.dumps(reply["result"]))
PY
}

log "GAVNO self-test $(date -Is) on $(uname -n), kernel $(uname -r)"
log "report: $REPORT"
log ""

# --- compositor -------------------------------------------------------------
GPID=$(pgrep -u "$(id -u)" -x gavno | head -1)
if [ -n "$GPID" ]; then
    pass compositor "pid $GPID, $(gavno --version 2>/dev/null | head -1)"
else
    fail compositor "no gavno process for this user"
fi
if [ "${WAYLAND_DISPLAY:-}" ]; then pass wayland "WAYLAND_DISPLAY=$WAYLAND_DISPLAY"; else fail wayland "WAYLAND_DISPLAY unset"; fi
if outputs=$(ipc outputs 2>&1); then
    pass ipc "$(printf '%s' "$outputs" | python3 -c 'import json,sys; print(", ".join("%s %dx%d@%.0f scale %s hdr-supported %s" % (o["name"], o["mode"]["w"], o["mode"]["h"], o["mode"]["refresh"]/1000, o["scale"], o["hdr"]["supported"]) for o in json.load(sys.stdin) if o.get("mode")))')"
else
    fail ipc "$outputs"
fi

# --- resources --------------------------------------------------------------
if [ -n "$GPID" ]; then
    mem=$(awk '/^Rss:|^Pss:|^Private_Dirty:/{printf "%s %d MB  ", $1, $2/1024}' /proc/"$GPID"/smaps_rollup)
    threads=$(ls /proc/"$GPID"/task | wc -l)
    t0=$(awk '{print $14+$15}' /proc/"$GPID"/stat); sleep 5; t1=$(awk '{print $14+$15}' /proc/"$GPID"/stat)
    hz=$(getconf CLK_TCK)
    log "INFO  gavno memory: $mem threads $threads, CPU over 5 s idle: $(( (t1 - t0) * 1000 / hz )) ms"
    for p in gavno-polkit-agent gavno-files foot xwayland-satellite; do
        pid=$(pgrep -u "$(id -u)" -f "^[^ ]*$p" | head -1)
        [ -n "$pid" ] && log "INFO  $p: $(awk '/^Pss:/{printf "Pss %d MB", $2/1024}' /proc/"$pid"/smaps_rollup)"
    done
fi

# --- X11 --------------------------------------------------------------------
if [ "${DISPLAY:-}" ] && have xprop; then
    if xprop -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1 || xset q >/dev/null 2>&1; then
        pass x11 "DISPLAY=$DISPLAY answers"
    else
        fail x11 "DISPLAY=$DISPLAY does not answer"
    fi
else
    fail x11 "DISPLAY unset or xprop missing"
fi

# --- notifications ----------------------------------------------------------
owner=$(busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus GetConnectionUnixProcessID s org.freedesktop.Notifications 2>/dev/null | awk '{print $2}')
if [ -n "$owner" ] && [ "$owner" = "$GPID" ]; then
    pass notifications "GAVNO owns org.freedesktop.Notifications"
elif [ -n "$owner" ]; then
    fail notifications "owned by pid $owner ($(cat /proc/"$owner"/comm 2>/dev/null)), not GAVNO"
else
    fail notifications "nobody owns org.freedesktop.Notifications"
fi
if have notify-send && [ "$AUTO" = 0 ]; then
    notify-send -a "GAVNO self-test" -t 4000 "Self-test" "If you can read this card, notifications work."
    if ask "Did a 'Self-test' notification card appear?"; then pass notification-card; else fail notification-card "not visible"; fi
fi

# --- polkit -----------------------------------------------------------------
if pgrep -u "$(id -u)" -x gavno-polkit-ag >/dev/null; then
    pass polkit-agent "gavno-polkit-agent running"
else
    fail polkit-agent "gavno-polkit-agent not running"
fi
if have pkexec && [ "$AUTO" = 0 ] && ask "Test a password prompt now (pkexec true; type your password)?"; then
    if pkexec true; then pass polkit-prompt; else fail polkit-prompt "pkexec failed or was cancelled"; fi
fi

# --- screen capture and sharing ---------------------------------------------
if have grim; then
    shot=$(mktemp --suffix=.png)
    if grim "$shot" 2>/dev/null && [ -s "$shot" ]; then pass screencopy "grim captured $(du -h "$shot" | cut -f1)"; else fail screencopy "grim failed"; fi
    rm -f "$shot"
else
    skip screencopy "grim not installed"
fi
if busctl --user status org.freedesktop.portal.Desktop >/dev/null 2>&1; then
    version=$(busctl --user get-property org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.ScreenCast version 2>/dev/null)
    if [ -n "$version" ]; then pass screencast-portal "ScreenCast portal $version"; else fail screencast-portal "portal has no ScreenCast interface (xdg-desktop-portal-wlr?)"; fi
else
    fail screencast-portal "xdg-desktop-portal not reachable"
fi

# --- media ------------------------------------------------------------------
players=$(busctl --user list --no-legend 2>/dev/null | awk '{print $1}' | grep '^org.mpris.MediaPlayer2\.' | tr '\n' ' ')
if [ -n "$players" ]; then log "INFO  MPRIS players: $players"; else log "INFO  no MPRIS player running (play something to test the bar capsule)"; fi

# --- terminal ---------------------------------------------------------------
if pgrep -u "$(id -u)" -f '^foot --server' >/dev/null; then pass foot-server "running"; else fail foot-server "foot --server not running (autostart)"; fi

# --- battery ----------------------------------------------------------------
bat=""
for supply in /sys/class/power_supply/*; do
    [ "$(cat "$supply"/type 2>/dev/null)" = Battery ] && { bat=$supply; break; }
done
if [ -n "$bat" ]; then
    log "INFO  battery $(cat "$bat"/capacity 2>/dev/null)% $(cat "$bat"/status 2>/dev/null); GAVNO warns at 15% and 5%"
else
    skip battery "no battery found"
fi

# --- Classic ----------------------------------------------------------------
if [ "$AUTO" = 0 ] && ask "Switch to Classic for a few seconds?"; then
    ipc shell_mode '{"mode":"classic"}' >/dev/null
    sleep 2
    if ask "Do you see the taskbar at the bottom and desktop icons?"; then pass classic; else fail classic "not shown"; fi
    ipc shell_mode '{"mode":"gavno"}' >/dev/null
fi

# --- settings app -----------------------------------------------------------
if have gavno-settings && [ "$AUTO" = 0 ] && ask "Open GAVNO Settings to check it?"; then
    gavno-settings >/dev/null 2>&1 &
    sp=$!
    if ask "Did Settings open with Displays, Keyboard and the other pages?"; then pass settings-app; else fail settings-app; fi
    kill "$sp" 2>/dev/null
fi

log ""
log "summary: $PASS passed, $FAIL failed, $SKIP skipped"
log "recent compositor warnings:"
journalctl --user -b --no-pager -q -p warning -t gavno 2>/dev/null | tail -10 | tee -a "$REPORT" >/dev/null
[ "$FAIL" = 0 ]
