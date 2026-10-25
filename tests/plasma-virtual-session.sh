#!/bin/bash
# Starts a headless KDE Plasma 6 Wayland session for testing ScreenMirror:
# a private session bus, kwin_wayland on the virtual backend with two outputs,
# and plasmashell (panel + system tray). Run as a normal user; Ctrl-C stops it.
#
#   tests/plasma-virtual-session.sh            # 2 x 1920x1080
#   WIDTH=1280 HEIGHT=800 tests/plasma-virtual-session.sh
#
# Then, in another shell, source the printed environment and run screenmirror.
set -eu

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-sm}"
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
export XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=KDE KDE_FULL_SESSION=true KDE_SESSION_VERSION=6
# As set by startplasma-wayland; without it KSycoca indexes no applications and
# KWin cannot find the .desktop file that authorizes screen capture.
export XDG_MENU_PREFIX=plasma-
export QT_QPA_PLATFORM=wayland LIBGL_ALWAYS_SOFTWARE=1
LOG_DIR="${LOG_DIR:-/tmp/screenmirror-session}"
mkdir -p "$LOG_DIR"
rm -f "$XDG_RUNTIME_DIR/bus" "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY.lock"

pids=()
trap 'kill "${pids[@]}" 2>/dev/null; wait' EXIT INT TERM

dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --nofork --nopidfile >"$LOG_DIR/dbus.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/bus" ] && break; sleep 0.1; done

kwin_wayland --virtual --output-count 2 --width "${WIDTH:-1920}" --height "${HEIGHT:-1080}" \
    --socket "$WAYLAND_DISPLAY" >"$LOG_DIR/kwin.log" 2>&1 &
pids+=($!)
for _ in $(seq 100); do [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break; sleep 0.1; done

kded6 >"$LOG_DIR/kded.log" 2>&1 &
pids+=($!)
plasmashell --no-respawn >"$LOG_DIR/plasmashell.log" 2>&1 &
pids+=($!)

cat <<EOF
Plasma Wayland test session running (logs in $LOG_DIR). Environment for clients:
export XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR WAYLAND_DISPLAY=$WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS QT_QPA_PLATFORM=wayland XDG_CURRENT_DESKTOP=KDE XDG_MENU_PREFIX=plasma-
EOF
wait -n
