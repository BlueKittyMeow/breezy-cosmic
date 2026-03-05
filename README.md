# breezy-cosmic

Head-tracked virtual display for XR glasses on COSMIC desktop.

Captures your primary monitor's content and re-renders it with real-time head-tracking on your XR glasses, so the virtual display appears fixed in space as you move your head.

## Status

**Phase 1 — Layer-Shell Prototype** (in development)

This is a standalone Wayland client that works with unmodified cosmic-comp. It creates a layer-shell overlay on the XR glasses' display and renders captured content with pose-based transforms.

## Hardware

- **Tested with:** Viture Luma Pro XR glasses
- **Requires:** Pop!_OS 24.04 with COSMIC desktop
- **Also needs:** [XRLinuxDriver](https://github.com/wheaney/XRLinuxDriver) for IMU pose data

## Prerequisites

### 1. Install XRLinuxDriver

XRLinuxDriver provides head-tracking data for supported XR glasses.

```bash
# Install from the official release
curl -Lo xrlinuxdriver.tar.gz \
  https://github.com/wheaney/XRLinuxDriver/releases/latest/download/xrlinuxdriver-x86_64.tar.gz
tar xzf xrlinuxdriver.tar.gz
cd xrlinuxdriver
./install.sh

# Verify it's running (with glasses plugged in)
ls -la /dev/shm/breezy_desktop_imu
# Should show the shared memory file
```

If the shared memory file doesn't appear, check that:
- Your glasses are connected via USB-C (they use DisplayPort alt-mode)
- The XRLinuxDriver service is running: `systemctl --user status xrdriver`
- Your user has access to the USB device (may need udev rules)

### 2. Install build dependencies

```bash
# Rust toolchain (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Wayland and rendering libraries
sudo apt install libwayland-dev libxkbcommon-dev libvulkan-dev \
  pkg-config cmake

# Screen capture tool (used as interim capture backend)
sudo apt install grim
```

### 3. Verify your glasses are detected as a display

```bash
cosmic-randr list
# You should see your XR glasses listed as an output
# (look for "VITURE" in the make/model)
```

## Build

```bash
cargo build --release
```

## Run

```bash
# Basic usage (auto-detects glasses and primary monitor)
./target/release/breezy-cosmic

# List all detected outputs
./target/release/breezy-cosmic --list-outputs

# Dry run: show pose data without rendering
./target/release/breezy-cosmic --dry-run

# Custom XR output match string
./target/release/breezy-cosmic --xr-match "VITURE"

# With debug logging
RUST_LOG=debug ./target/release/breezy-cosmic
```

## Configuration

Config file: `~/.config/breezy-cosmic/config.toml`

```toml
[display]
distance = 1.5           # Virtual display distance (meters)
scale = 1.0              # Content scale factor
smooth_follow = true     # Display gently follows gaze
follow_threshold = 15.0  # Degrees before follow kicks in

[capture]
target_fps = 60
use_dmabuf = true

[output]
xr_match = "VITURE"      # EDID string to match XR glasses
primary_match = ""        # Empty = auto-detect largest monitor
```

## Architecture

See [SPEC.md](SPEC.md) for the full technical specification.

```
XRLinuxDriver (IMU) ──→ /dev/shm ──→ breezy-cosmic ──→ layer-shell surface
                                          ↑                    (on XR output)
primary monitor ──→ screen capture ───────┘
```

## Roadmap

- **Phase 1** (current): Layer-shell prototype — standalone client, no compositor changes
- **Phase 2**: Propose virtual output support upstream to cosmic-comp
- **Phase 3**: Native compositor integration with real virtual outputs
- **Phase 4**: Multi-display support, side-by-side stereo 3D

## Credits

Built on top of the [breezy-desktop](https://github.com/wheaney/breezy-desktop) ecosystem by wheaney.

## License

GPL-3.0-only (matching breezy-desktop)
