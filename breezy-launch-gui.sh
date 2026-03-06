#!/usr/bin/env bash
#
# breezy-launch-gui — GUI wrapper for breezy-cosmic
# Uses zenity for a graphical menu, no terminal needed.
# Called from .desktop entry; delegates to breezy-launch.sh for actual work.
#

set -euo pipefail

BREEZY_DIR="$(cd "$(dirname "$0")" && pwd)"
LAUNCHER="$BREEZY_DIR/breezy-launch.sh"
PIDFILE="/tmp/breezy-cosmic.pid"

is_running() {
    [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null
}

notify() {
    notify-send -i breezy-cosmic "Breezy Cosmic" "$1" 2>/dev/null || true
}

# If called with an argument, run that action directly (for .desktop Actions)
if [ "${1:-}" = "desktop" ]; then
    "$LAUNCHER" desktop &
    notify "Starting desktop capture..."
    exit 0
elif [ "${1:-}" = "window" ]; then
    "$LAUNCHER" window &
    notify "Starting window capture — pick a window in the dialog..."
    exit 0
elif [ "${1:-}" = "repin" ]; then
    "$LAUNCHER" repin
    notify "Display re-pinned to current gaze"
    exit 0
elif [ "${1:-}" = "stop" ]; then
    "$LAUNCHER" stop
    notify "Stopped"
    exit 0
fi

# Interactive GUI menu
if is_running; then
    PID=$(cat "$PIDFILE")
    choice=$(zenity --list \
        --title="Breezy Cosmic" \
        --text="XR display is running (PID $PID)" \
        --column="Action" --column="Description" \
        --width=420 --height=320 \
        --window-icon=breezy-cosmic \
        "repin"    "Re-center display on current gaze" \
        "stop"     "Stop breezy-cosmic" \
        "restart"  "Restart with desktop capture" \
        "status"   "Show status in terminal" \
        2>/dev/null) || exit 0

    case "$choice" in
        repin)   "$LAUNCHER" repin; notify "Display re-pinned" ;;
        stop)    "$LAUNCHER" stop; notify "Stopped" ;;
        restart) "$LAUNCHER" stop; sleep 1; "$LAUNCHER" desktop & notify "Restarting..." ;;
        status)  cosmic-term & ;;
    esac
else
    choice=$(zenity --list \
        --title="Breezy Cosmic" \
        --text="Launch XR head-tracked display" \
        --column="Action" --column="Description" \
        --width=420 --height=280 \
        --window-icon=breezy-cosmic \
        "desktop"  "Capture full desktop" \
        "window"   "Capture a single window (HUD mode)" \
        2>/dev/null) || exit 0

    case "$choice" in
        desktop) "$LAUNCHER" desktop & notify "Starting desktop capture..." ;;
        window)  "$LAUNCHER" window & notify "Starting window capture..." ;;
    esac
fi
