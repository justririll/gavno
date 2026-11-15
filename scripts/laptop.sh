#!/bin/sh
# Dev loop for the laptop target: edit on the PC, build and run natively there.
#   scripts/laptop.sh sync           copy sources (Cargo.lock included)
#   scripts/laptop.sh build          sync + cargo build (low priority)
#   scripts/laptop.sh run [cmd]      build + start GAVNO on tty7 (optionally run cmd inside)
#   scripts/laptop.sh stop           stop GAVNO (SIGTERM) and go back to tty1
#   scripts/laptop.sh log            follow the GAVNO journal
# GAVNO_ARGS adds GAVNO flags, e.g. GAVNO_ARGS=--greeter scripts/laptop.sh run
set -eu
HOST=${GAVNO_HOST:-root@192.168.31.235}
DIR=/home/kirill/Projects/ds
VT=${GAVNO_VT:-7}  # above NAutoVTs, so logind never puts a getty here
UNIT=gavno-dev
cd "$(dirname "$0")/.."

sync_src() {
    rsync -a --delete --exclude target --exclude .git --chown=kirill:users ./ "$HOST:$DIR/"
}

build() {
    sync_src
    ssh "$HOST" "cd $DIR && runuser -u kirill -- nice -n 19 ionice -c3 cargo build -j 8 --color never 2>&1 | grep -vE '^\s+(Compiling|Checking|Downloaded|Downloading)'"
}

case "${1:-}" in
sync) sync_src ;;
build) build ;;
run)
    shift
    # Never switch the screen away from a real login session (greetd) unless asked.
    if [ "${FORCE:-0}" != 1 ] && ssh "$HOST" "loginctl list-sessions --no-legend | awk '{print \$1}' | xargs -r -n1 loginctl show-session -p Service -p Active --value | paste - - | grep -q '^greetd	yes'"; then
        echo "a greetd login session is active on the laptop; not starting $UNIT (FORCE=1 to override)" >&2
        exit 1
    fi
    build
    ssh "$HOST" "systemctl stop $UNIT getty@tty$VT 2>/dev/null || true; systemctl reset-failed $UNIT 2>/dev/null || true
        systemd-run --unit=$UNIT \
            -p PAMName=login -p User=kirill -p TTYPath=/dev/tty$VT \
            -p StandardInput=tty-force -p StandardOutput=journal -p StandardError=journal \
            -p TTYReset=yes -p TTYVHangup=yes -p TTYVTDisallocate=yes \
            -p UtmpIdentifier=tty$VT -p UtmpMode=user \
            -p KillSignal=SIGTERM -p TimeoutStopSec=5 \
            -E XDG_SESSION_TYPE=wayland -E XDG_SESSION_CLASS=user -E XDG_SEAT=seat0 -E XDG_VTNR=$VT \
            -E RUST_LOG=\${RUST_LOG:-info} -E RUST_BACKTRACE=1 \
            $DIR/target/debug/gavno --tty ${GAVNO_ARGS:-} ${1:+-c '$*'}
        sleep 0.5; chvt $VT"
    ;;
stop) ssh "$HOST" "systemctl stop $UNIT; chvt 1" ;;
log) ssh "$HOST" "journalctl -u $UNIT -f -o cat" ;;
*) sed -n '2,8p' "$0"; exit 2 ;;
esac
