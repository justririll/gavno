#!/bin/sh
# Install GAVNO as the greetd greeter and user session on the laptop.
#   scripts/install-greeter.sh                build on the laptop and install files
#   scripts/install-greeter.sh --enable       also enable greetd.service at boot
#   scripts/install-greeter.sh --test         also start greetd once on VT 8 (no boot change)
# Without --enable nothing is enabled or started. Options may be combined
# except that --test and --enable are mutually exclusive.
set -eu
HOST=${GAVNO_HOST:-root@192.168.31.235}
DIR=/home/kirill/Projects/ds
TEST_VT=8
cd "$(dirname "$0")/.."

ENABLE=0
TEST=0
for arg in "$@"; do
    case "$arg" in
    --enable) ENABLE=1 ;;
    --test) TEST=1 ;;
    *) sed -n '2,6p' "$0"; exit 2 ;;
    esac
done
if [ "$ENABLE" = 1 ] && [ "$TEST" = 1 ]; then
    echo "--enable and --test cannot be combined" >&2
    exit 2
fi

scripts/laptop.sh sync
# --quiet instead of a grep filter, so a failed build fails this script.
ssh "$HOST" "cd $DIR && runuser -u kirill -- nice -n 19 ionice -c3 cargo build --release --quiet -j 8 --color never" \
    || { echo "release build failed" >&2; exit 1; }

# The rest runs on the laptop as root, from the synced checkout.
ssh "$HOST" "DIR=$DIR ENABLE=$ENABLE TEST=$TEST TEST_VT=$TEST_VT sh -s" <<'REMOTE'
set -eu
cd "$DIR"
install -D -m 755 target/release/gavno /usr/local/bin/gavno
install -D -m 755 dist/gavno-session /usr/local/bin/gavno-session
# The webcam finger pointer was removed; drop what older installs left behind.
rm -f /usr/local/bin/gavno-cam
rm -rf /usr/local/share/gavno/models
# Local AI helper (design/ai.md): the session-bus service, the CPU-only model unit
# (inactive until gavno-ai starts it) and the polkit rule that lets wheel start it.
install -D -m 755 target/release/gavno-ai /usr/local/bin/gavno-ai
install -D -m 644 dist/dbus-1/services/org.gavno.AI1.service /usr/share/dbus-1/services/org.gavno.AI1.service
install -D -m 644 dist/systemd/llama-gavno.service /etc/systemd/system/llama-gavno.service
install -D -m 644 dist/polkit-1/rules.d/50-local-llm.rules /etc/polkit-1/rules.d/50-local-llm.rules
systemctl daemon-reload
install -D -m 644 dist/wayland-sessions/gavno.desktop /usr/share/wayland-sessions/gavno.desktop
# xdg-desktop-portal reads these when it starts, i.e. at the next login.
install -D -m 644 dist/xdg-desktop-portal/gavno-portals.conf /usr/share/xdg-desktop-portal/gavno-portals.conf
install -D -m 644 dist/xdg-desktop-portal/gavno.portal /usr/share/xdg-desktop-portal/portals/gavno.portal

# The greeter hands the panel to the session. With the user's output settings
# (mode, HDR) the session adopts the greeter's state at login without a modeset.
user_config=/home/kirill/.config/gavno/config.toml
greeter_home=$(getent passwd greetd | cut -d: -f6)
if [ -r "$user_config" ] && [ -n "$greeter_home" ]; then
    greeter_config=$greeter_home/.config/gavno/config.toml
    install -d -o greetd -g greetd "$greeter_home/.config" "$greeter_home/.config/gavno"
    awk '/^\[/ { keep = ($0 ~ /^\[outputs[].]/) } keep' "$user_config" > "$greeter_config.new"
    chown greetd:greetd "$greeter_config.new"
    chmod 644 "$greeter_config.new"
    mv "$greeter_config.new" "$greeter_config"
fi

if [ ! -e /etc/pam.d/gavno-lock ]; then
    install -D -m 644 dist/pam.d/gavno-lock /etc/pam.d/gavno-lock
fi

if [ "$TEST" = 0 ]; then
    mkdir -p /etc/greetd
    if [ -e /etc/greetd/config.toml ] && ! cmp -s dist/greetd/config.toml /etc/greetd/config.toml; then
        bak=/etc/greetd/config.toml.bak.$(date +%Y%m%d-%H%M%S)
        cp -p /etc/greetd/config.toml "$bak"
        echo "backed up old config to $bak"
    fi
    install -m 644 dist/greetd/config.toml /etc/greetd/config.toml
fi

# Without logind the greeter user needs direct access to the DRM and input devices.
if [ -d /run/systemd/seats ] && command -v loginctl >/dev/null 2>&1; then
    echo "logind is available: skipping video/input group setup for greetd"
else
    usermod -aG video,input greetd
    echo "logind not available: added greetd to video and input groups"
fi

if [ "$TEST" = 1 ]; then
    cat > /etc/greetd/config-test.toml <<CONF
[terminal]
vt = $TEST_VT

[default_session]
command = "/usr/local/bin/gavno --greeter"
user = "greetd"
CONF
    greetd_bin=$(command -v greetd)
    systemctl stop greetd-test 2>/dev/null || true
    systemctl reset-failed greetd-test 2>/dev/null || true
    systemd-run --unit=greetd-test "$greetd_bin" --config /etc/greetd/config-test.toml
    sleep 1
    chvt "$TEST_VT"
    echo "test greetd running on VT $TEST_VT; stop with: systemctl stop greetd-test; chvt 1"
elif [ "$ENABLE" = 1 ]; then
    systemctl enable greetd.service
    systemctl set-default graphical.target
    echo "greetd enabled; takes effect on next boot."
    echo "roll back with: systemctl disable greetd && systemctl set-default multi-user.target"
else
    if systemctl is-enabled --quiet greetd.service; then
        echo "installed; greetd is enabled and will use the new binary on next login screen"
    else
        echo "installed; greetd not enabled (re-run with --enable, or --test for a one-off run on VT $TEST_VT)"
    fi
fi
REMOTE
