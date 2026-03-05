# breezy-cosmic Technical Specification

**Version:** 0.1.0 (Phase 1 — Layer-Shell Prototype)
**Target:** Pop!_OS 24.04, COSMIC desktop, Viture Luma Pro XR glasses

---

## 1. Overview

breezy-cosmic is a standalone Wayland client that enables head-tracked virtual display rendering on XR glasses connected to a COSMIC desktop. It captures content from the user's primary monitor, reads 6DoF pose data from XRLinuxDriver via shared memory, and renders the captured content with real-time head-tracking transforms onto the XR glasses' display using a layer-shell overlay.

This is a Phase 1 prototype that requires no changes to cosmic-comp. Phase 2 would propose native virtual output support upstream.

## 2. Architecture

```
┌─────────────────────────────────────────────────┐
│                    breezy-cosmic                 │
│                                                  │
│  ┌──────────┐  ┌──────────┐  ┌───────────────┐  │
│  │  Pose    │  │ Capture  │  │   Renderer    │  │
│  │  Reader  │  │  Source   │  │  (wgpu/GL)    │  │
│  │          │  │          │  │               │  │
│  │ reads    │  │ captures │  │ composites    │  │
│  │ /dev/shm │  │ primary  │  │ with pose     │  │
│  │ IMU data │  │ monitor  │  │ transforms    │  │
│  └────┬─────┘  └────┬─────┘  └───────┬───────┘  │
│       │              │                │          │
│       └──────────────┼────────────────┘          │
│                      │                           │
│              ┌───────▼───────┐                   │
│              │  Layer Shell  │                    │
│              │   Surface     │                    │
│              │  (on XR out)  │                    │
│              └───────────────┘                    │
└─────────────────────────────────────────────────┘
         │                           │
         ▼                           ▼
  /dev/shm/breezy_desktop_imu   Wayland compositor
  (XRLinuxDriver)                (cosmic-comp)
```

## 3. Components

### 3.1 Output Detection (`src/output.rs`)

Connects to the Wayland compositor via wlr-output-management-unstable-v4 protocol. Enumerates all outputs and identifies:

- **Primary output**: The user's main monitor (largest resolution, or user-configured)
- **XR output**: The Viture Luma Pro display, identified by EDID strings containing "VITURE" or matching known product IDs

Output detection runs at startup and monitors for hotplug events (glasses connected/disconnected).

### 3.2 Pose Reader (`src/pose.rs`)

Memory-maps `/dev/shm/breezy_desktop_imu` (read-only) and polls for pose updates at display refresh rate.

**Shared memory layout** (from XRLinuxDriver):
```
Offset  Type        Field
0       u8          version
1       u8          enabled
2       [f32; 4]    look_ahead_cfg
18      [u32; 2]    display_res
26      f32         display_fov
30      f32         lens_distance_ratio
34      u8          sbs_enabled
35      u8          custom_banner
36      u8          smooth_follow
37      [f32; 16]   smooth_follow_origin (4 quaternions)
101     [f32; 3]    position (NWU coordinates)
113     u64         timestamp_ms
121+    [f32; 16]   orientations (4 quaternions: T0, T1, T2, T3)
last    u8          parity (XOR of all preceding bytes)
```

**Coordinate transform** (NWU → EUS):
```
x_eus = -y_nwu
y_eus =  z_nwu
z_eus = -x_nwu
```

**Quaternion usage**: The T0 quaternion (first 4 floats of orientations) represents current head orientation. Applied as a rotation matrix to the virtual display plane.

### 3.3 Screen Capture (`src/capture.rs`)

Uses `ext-image-copy-capture-v1` protocol (available in cosmic-comp via Smithay) to capture frames from the primary output.

**Capture flow:**
1. Create capture session for the primary output
2. Receive buffer constraints (supported formats, sizes)
3. Allocate SHM or dmabuf buffers matching constraints
4. Attach buffer and request frame capture
5. On frame ready: upload to GPU texture for rendering

**Target**: Capture at native resolution, ≤16ms per frame to maintain real-time feel.

**Fallback**: If ext-image-copy-capture is not available, fall back to wlr-screencopy-unstable-v1.

### 3.4 Layer Shell Surface (`src/surface.rs`)

Creates a `wlr-layer-shell-unstable-v1` surface on the XR output with:

- **Layer**: `Overlay` (topmost, above all windows)
- **Anchor**: All four edges (fills entire output)
- **Exclusive zone**: -1 (don't affect other surfaces)
- **Keyboard interactivity**: None (pass-through)
- **Size**: Full resolution of the XR output

The surface is the render target — wgpu or OpenGL renders head-tracked content here.

### 3.5 Renderer (`src/render.rs`)

GPU-accelerated rendering pipeline using wgpu (Vulkan backend on Linux):

1. **Upload captured frame** as GPU texture
2. **Read current pose** (quaternion + position)
3. **Compute view matrix** from pose:
   - Convert quaternion to rotation matrix
   - Apply display distance and FOV parameters from shared memory
   - Account for lens distortion ratio
4. **Render textured quad** with the view matrix applied
5. **Present** to the layer-shell surface

**Smooth follow mode**: When enabled, the virtual display doesn't rigidly follow head movement but gently drifts toward the gaze direction using SLERP interpolation between the smooth_follow_origin quaternion and the current pose.

### 3.6 Control & Config (`src/config.rs`)

**Runtime control via D-Bus** (optional Phase 1 stretch goal):
- `com.xronlinux.BreezyDesktop.Cosmic` service
- Methods: `Enable()`, `Disable()`, `SetDisplayDistance(f64)`, `SetSmoothFollow(bool)`

**Configuration file** (`~/.config/breezy-cosmic/config.toml`):
```toml
[display]
distance = 1.5           # Virtual display distance (meters)
scale = 1.0              # Content scale factor
smooth_follow = true     # Smooth follow mode
follow_threshold = 15.0  # Degrees before smooth follow kicks in

[capture]
target_fps = 60          # Capture framerate target
use_dmabuf = true        # Prefer dmabuf over SHM capture

[output]
xr_match = "VITURE"      # EDID string to match XR output
primary_match = ""        # EDID string for primary (empty = largest)
```

## 4. Wayland Protocols Used

| Protocol | Purpose | Required |
|----------|---------|----------|
| wl_compositor | Create surfaces | Yes |
| wl_shm | Shared memory buffers | Yes |
| zwlr_layer_shell_v1 | Layer-shell overlay on XR output | Yes |
| zwlr_output_manager_v1 | Enumerate and identify outputs | Yes |
| ext_image_copy_capture_v1 | Capture primary monitor content | Yes |
| linux_dmabuf_v1 | Zero-copy buffer sharing | Preferred |
| zwlr_screencopy_v1 | Fallback screen capture | Fallback |
| wp_viewporter | Surface viewport control | Optional |

## 5. Dependencies

```toml
[dependencies]
# Wayland client
smithay-client-toolkit = "0.20"
wayland-client = "0.31"
wayland-protocols = { version = "0.32", features = ["staging"] }
wayland-protocols-wlr = "0.3"

# Rendering
wgpu = "28"
raw-window-handle = "0.6"

# Math (quaternions, matrices)
glam = "0.29"

# Shared memory
memmap2 = "0.9"

# Event loop
calloop = "0.14"
calloop-wayland-source = "0.4"

# Config
toml = "0.8"
serde = { version = "1", features = ["derive"] }

# Logging
tracing = "0.1"
tracing-subscriber = "0.3"

# CLI
clap = { version = "4", features = ["derive"] }
```

## 6. Build & Run

```bash
# Build
cargo build --release

# Run (XRLinuxDriver must be running, glasses connected)
./target/release/breezy-cosmic

# Run with verbose logging
RUST_LOG=debug ./target/release/breezy-cosmic

# Run with custom config
./target/release/breezy-cosmic --config ~/.config/breezy-cosmic/config.toml
```

## 7. XRLinuxDriver Setup

XRLinuxDriver must be installed separately for Viture Luma Pro support:

```bash
# Clone and build
git clone https://github.com/wheaney/XRLinuxDriver.git
cd XRLinuxDriver
# Follow build instructions in README

# Verify glasses are detected
ls -la /dev/shm/breezy_desktop_imu
# Should exist when glasses are plugged in
```

## 8. Phase 2 Roadmap

After Phase 1 proves the concept:

1. **Upstream PR to cosmic-comp**: Add `backend_headless` feature to Smithay deps, create `src/backend/virtual.rs` for virtual output management, extend `BackendData` enum for hybrid operation
2. **Custom Wayland protocol**: `zcosmic-virtual-output-v1` for creating/destroying virtual outputs from client side
3. **Migration**: breezy-cosmic switches from layer-shell capture-render to using a real virtual output
4. **Multi-display**: Support multiple virtual displays at different positions in 3D space
5. **Side-by-side stereo**: SBS rendering for 3D depth perception

## 9. Known Limitations (Phase 1)

- **Extra latency**: Capture → render → display adds ~1 frame of latency vs native compositor integration
- **Single virtual display**: Layer-shell approach doesn't easily support multiple independent displays
- **No independent resolution**: The "virtual display" is a re-projection of the captured content, not its own render target
- **Resource usage**: Full-screen capture + re-render is more GPU-intensive than direct compositor integration
- **No window awareness**: Apps don't know they're on a virtual display; can't adjust layout
