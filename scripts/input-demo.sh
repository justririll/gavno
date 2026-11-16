#!/bin/sh
# Exercise a private nested compositor and leave screenshots and a startup log.
set -eu
out=${1:-input-demo}
mkdir -p "$out"
out=$(cd "$out" && pwd)
bin=${GAVNO_BIN:-${CARGO_TARGET_DIR:-target}/debug/gavno}
case "$bin" in /*) ;; *) bin=$(pwd)/$bin ;; esac
[ -x "$bin" ] || { echo "Build gavno or set GAVNO_BIN" >&2; exit 1; }
[ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ] || { echo "A Wayland or X display is required" >&2; exit 1; }
private=$(mktemp -d)
pid=
demo_client=${GAVNO_DEMO_CLIENT:-}
if [ -z "$demo_client" ]; then
    if command -v foot >/dev/null 2>&1; then
        demo_client='exec foot -T "GAVNO input demo"'
    elif command -v zenity >/dev/null 2>&1; then
        demo_client='exec zenity --info --no-wrap --text="GAVNO input demo"'
    fi
fi
cleanup() {
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || :; wait "$pid" 2>/dev/null || :; fi
    rm -rf "$private"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$private/config/gavno" "$private/runtime" "$private/home"
chmod 700 "$private/runtime"
cat > "$private/config/gavno/config.toml" <<'CONFIG'
mod_key = "super"
polkit_agent = ""
[modules]
overview = false
[keyboard]
layout = "us"
options = ""
[lock]
after_idle_secs = 0
[classic]
desktop_home = false
desktop_trash = false
CONFIG
# Preserve access to the host Wayland socket when replacing the runtime dir.
wayland=${WAYLAND_DISPLAY:-}
case "$wayland" in ''|/*) ;; *) wayland=${XDG_RUNTIME_DIR:?}/$wayland ;; esac
export GAVNO_SOCKET="$private/runtime/gavno/ipc.sock"
# env -i prevents host GAVNO_* settings from overriding the private config.
if [ -n "$wayland" ]; then
    env -i PATH="$PATH" HOME="$private/home" USER="${USER:-}" LANG="${LANG:-C.UTF-8}" \
        XDG_RUNTIME_DIR="$private/runtime" XDG_CONFIG_HOME="$private/config" \
        WAYLAND_DISPLAY="$wayland" GDK_BACKEND=wayland GAVNO_SOCKET="$GAVNO_SOCKET" GAVNO_DEBUG_INPUT=1 \
        "$bin" --winit -c "$demo_client" > "$out/compositor.log" 2>&1 &
else
    env -i PATH="$PATH" HOME="$private/home" USER="${USER:-}" LANG="${LANG:-C.UTF-8}" \
        XDG_RUNTIME_DIR="$private/runtime" XDG_CONFIG_HOME="$private/config" \
        DISPLAY="$DISPLAY" GDK_BACKEND=wayland XAUTHORITY="${XAUTHORITY:-$HOME/.Xauthority}" \
        GAVNO_SOCKET="$GAVNO_SOCKET" GAVNO_DEBUG_INPUT=1 \
        "$bin" --winit -c "$demo_client" > "$out/compositor.log" 2>&1 &
fi
pid=$!
msg() { "$bin" msg "$@"; }
tries=0
until msg state > /dev/null 2>&1; do
    if ! kill -0 "$pid" 2>/dev/null || [ "$tries" -ge 100 ]; then
        cat "$out/compositor.log" >&2; exit 1
    fi
    tries=$((tries + 1))
    sleep 0.1
done
if [ -n "$demo_client" ]; then
    tries=0
    until msg screen | python3 -c 'import json,sys; sys.exit(not json.load(sys.stdin)["result"]["windows"])'; do
        [ "$tries" -lt 100 ] || { echo "Demo client did not map a window" >&2; exit 1; }
        tries=$((tries + 1))
        sleep 0.1
    done
else
    echo "No foot/zenity found; set GAVNO_DEMO_CLIENT to demonstrate switching occupied workspaces" >&2
fi
shot() {
    # Gesture end commits the target; let its visible spring settle for the demo.
    sleep 0.5
    # Encode the path safely, including quotes and newlines in directory names.
    params=$(python3 -c 'import json,sys; print(json.dumps({"path":sys.argv[1]}))' "$out/$1.png")
    msg screenshot "$params"
    msg state
}
msg inject_gesture '{"kind":"swipe","fingers":3,"dx":-400,"dy":0}'
shot 01-workspace
msg inject_key '{"key":"super","state":"press"}'
msg inject_gesture '{"kind":"swipe","fingers":3,"dy":-340}'
msg inject_key '{"key":"super","state":"release"}'
shot 02-classic
# Let the mode spring settle before starting the reverse gesture.
sleep 1
msg inject_key '{"key":"super","state":"press"}'
msg inject_gesture '{"kind":"swipe","fingers":3,"dy":340}'
msg inject_key '{"key":"super","state":"release"}'
shot 03-gavno
sleep 1
msg inject_key '{"key":"super","state":"tap"}'
shot 04-launcher
printf 'Artifacts: %s\n' "$out"
