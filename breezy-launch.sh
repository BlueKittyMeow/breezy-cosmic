#!/usr/bin/env bash
#
# breezy-launch — launcher for breezy-cosmic XR head-tracked display
#
# Usage:
#   breezy-launch                    Interactive menu
#   breezy-launch desktop            Full desktop capture
#   breezy-launch window             Window capture (pick a window)
#   breezy-launch repin              Re-center display on current gaze
#   breezy-launch stop               Stop breezy-cosmic
#   breezy-launch status             Show running status
#

set -euo pipefail

BREEZY_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$BREEZY_DIR/target/release/breezy-cosmic"
CONFIG="$HOME/.config/breezy-cosmic/config.toml"
PIDFILE="/tmp/breezy-cosmic.pid"
LOGFILE="/tmp/breezy-cosmic.log"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

header() {
    echo -e "${CYAN}${BOLD}"
    echo "  ╔══════════════════════════════════════╗"
    echo "  ║      🥽  breezy-cosmic launcher      ║"
    echo "  ║   Head-tracked XR display for COSMIC ║"
    echo "  ╚══════════════════════════════════════╝"
    echo -e "${NC}"
}

is_running() {
    if [ -f "$PIDFILE" ]; then
        local pid
        pid=$(cat "$PIDFILE")
        if kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        rm -f "$PIDFILE"
    fi
    return 1
}

get_pid() {
    if [ -f "$PIDFILE" ]; then
        cat "$PIDFILE"
    fi
}

set_config_source() {
    local source="$1"
    if [ -f "$CONFIG" ]; then
        sed -i "s/^source = .*/source = \"$source\"/" "$CONFIG"
        echo -e "${GREEN}Capture source set to: $source${NC}"
    else
        echo -e "${RED}Config not found at $CONFIG${NC}"
        return 1
    fi
}

set_config_pin() {
    local yaw="$1"
    local pitch="$2"
    if [ -f "$CONFIG" ]; then
        sed -i "s/^pin_yaw = .*/pin_yaw = $yaw/" "$CONFIG"
        sed -i "s/^pin_pitch = .*/pin_pitch = $pitch/" "$CONFIG"
        echo -e "${GREEN}Pin offset set to: yaw=${yaw}° pitch=${pitch}°${NC}"
    fi
}

start_breezy() {
    local source="${1:-monitor}"

    if is_running; then
        echo -e "${YELLOW}breezy-cosmic is already running (PID $(get_pid))${NC}"
        echo -e "Use '${BOLD}breezy-launch stop${NC}' first, or '${BOLD}breezy-launch repin${NC}' to re-center."
        return 1
    fi

    # Check binary exists
    if [ ! -x "$BINARY" ]; then
        echo -e "${RED}Binary not found at $BINARY${NC}"
        echo "Run: cd $BREEZY_DIR && cargo build --release"
        return 1
    fi

    # Set capture source
    set_config_source "$source"

    echo -e "${CYAN}Starting breezy-cosmic (source: $source)...${NC}"
    echo -e "${YELLOW}A portal dialog may appear — select what to share.${NC}"

    # Export Wayland environment
    export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=/run/user/$(id -u)/bus}"
    export RUST_LOG="${RUST_LOG:-info}"

    nohup "$BINARY" > "$LOGFILE" 2>&1 &
    local pid=$!
    echo "$pid" > "$PIDFILE"

    echo -e "${GREEN}Started! PID: $pid${NC}"
    echo -e "Log: $LOGFILE"
    echo ""
    echo -e "  ${BOLD}Re-center:${NC}  breezy-launch repin"
    echo -e "  ${BOLD}Stop:${NC}       breezy-launch stop"
    echo -e "  ${BOLD}Status:${NC}     breezy-launch status"
}

stop_breezy() {
    if ! is_running; then
        echo -e "${YELLOW}breezy-cosmic is not running${NC}"
        return 0
    fi

    local pid
    pid=$(get_pid)
    echo -e "${CYAN}Stopping breezy-cosmic (PID $pid)...${NC}"
    kill "$pid" 2>/dev/null || true

    # Also kill any capture helpers
    pkill -f "breezy_portal_capture" 2>/dev/null || true
    rm -f "$PIDFILE" /dev/shm/breezy_capture

    sleep 1
    if kill -0 "$pid" 2>/dev/null; then
        kill -9 "$pid" 2>/dev/null || true
    fi

    echo -e "${GREEN}Stopped.${NC}"
}

repin() {
    if ! is_running; then
        echo -e "${RED}breezy-cosmic is not running${NC}"
        return 1
    fi

    local pid
    pid=$(get_pid)
    echo -e "${CYAN}Re-centering display (sending SIGUSR1 to PID $pid)...${NC}"
    kill -USR1 "$pid"
    echo -e "${GREEN}Display re-centered on current gaze direction.${NC}"
}

show_status() {
    if is_running; then
        local pid
        pid=$(get_pid)
        echo -e "${GREEN}breezy-cosmic is RUNNING (PID $pid)${NC}"

        # Show config info
        if [ -f "$CONFIG" ]; then
            local source pin_yaw pin_pitch
            source=$(grep "^source" "$CONFIG" | sed 's/.*= *"\(.*\)"/\1/')
            pin_yaw=$(grep "^pin_yaw" "$CONFIG" | sed 's/.*= *//')
            pin_pitch=$(grep "^pin_pitch" "$CONFIG" | sed 's/.*= *//')
            echo -e "  Source:    ${BOLD}$source${NC}"
            echo -e "  Pin:       yaw=${pin_yaw}° pitch=${pin_pitch}°"
        fi

        # Show recent log
        echo ""
        echo -e "${BOLD}Recent log:${NC}"
        tail -5 "$LOGFILE" 2>/dev/null | sed 's/^/  /'
    else
        echo -e "${YELLOW}breezy-cosmic is NOT running${NC}"
    fi
}

interactive_menu() {
    header

    if is_running; then
        echo -e "  Status: ${GREEN}RUNNING${NC} (PID $(get_pid))"
    else
        echo -e "  Status: ${YELLOW}stopped${NC}"
    fi
    echo ""

    echo -e "  ${BOLD}1)${NC} Launch — full desktop"
    echo -e "  ${BOLD}2)${NC} Launch — pick a window (HUD mode)"
    echo -e "  ${BOLD}3)${NC} Re-center display (re-pin)"
    echo -e "  ${BOLD}4)${NC} Set pin offset (yaw/pitch)"
    echo -e "  ${BOLD}5)${NC} Stop"
    echo -e "  ${BOLD}6)${NC} Show status & log"
    echo -e "  ${BOLD}q)${NC} Quit"
    echo ""
    read -rp "  Choose [1-6, q]: " choice

    case "$choice" in
        1) start_breezy "monitor" ;;
        2) start_breezy "window" ;;
        3) repin ;;
        4)
            read -rp "  Yaw offset (degrees, negative=left): " yaw
            read -rp "  Pitch offset (degrees, negative=down): " pitch
            set_config_pin "${yaw:-0.0}" "${pitch:-0.0}"
            if is_running; then
                echo -e "${YELLOW}Restart breezy-cosmic for pin offset to take effect.${NC}"
            fi
            ;;
        5) stop_breezy ;;
        6) show_status ;;
        q|Q) exit 0 ;;
        *) echo -e "${RED}Invalid choice${NC}" ;;
    esac
}

# ── Main ──────────────────────────────────────────────────────────────
case "${1:-}" in
    desktop|monitor) start_breezy "monitor" ;;
    window)          start_breezy "window" ;;
    repin|recenter)  repin ;;
    stop|kill)       stop_breezy ;;
    status)          show_status ;;
    "")              interactive_menu ;;
    *)
        echo "Usage: breezy-launch [desktop|window|repin|stop|status]"
        exit 1
        ;;
esac
