//! Layer-shell surface management via SCTK.
//!
//! Creates a full-screen overlay surface on the XR glasses output
//! using the wlr-layer-shell protocol. This drives the main event loop:
//! on each frame callback, we capture → GPU render → write SHM pixels → present.

use anyhow::{Context, Result};
use glam::Mat4;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, error, info, warn};

/// Global flag set by SIGUSR1 to trigger a re-pin (re-center) of the display
static REPIN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Install the SIGUSR1 handler for re-pinning
pub fn install_repin_handler() {
    unsafe {
        libc::signal(libc::SIGUSR1, repin_signal_handler as libc::sighandler_t);
    }
}

extern "C" fn repin_signal_handler(_sig: libc::c_int) {
    REPIN_REQUESTED.store(true, Ordering::SeqCst);
}
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::capture::ScreenCapture;
use crate::config::Config;
use crate::gpu::GpuPipeline;
use crate::output::OutputInfo;
use crate::pose::PoseReader;
use crate::render::Renderer;

/// Application state for the SCTK event-driven layer shell client.
///
/// This struct is the Wayland dispatch target — all SCTK events route here.
/// The frame callback in `CompositorHandler::frame()` drives the
/// capture → GPU render → SHM write → present cycle.
pub struct App {
    // SCTK plumbing
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,

    // Layer surface (the overlay on XR glasses)
    layer: LayerSurface,
    pool: SlotPool,
    configured: bool,
    width: u32,
    height: u32,
    exit: bool,

    // breezy-cosmic components
    capture: ScreenCapture,
    gpu: GpuPipeline,
    renderer: Renderer,
    pose_reader: PoseReader,
    config: Config,

    // Source frame dimensions (may differ from viewport for window capture)
    source_size: Option<(u32, u32)>,

    // Stats
    frame_count: u64,
    loop_start: std::time::Instant,
}

impl App {
    /// Run the breezy-cosmic overlay on the XR glasses output.
    ///
    /// This connects to Wayland, creates the layer-shell surface,
    /// and enters the SCTK event loop. Returns when the surface is closed.
    ///
    /// Output targeting strategy:
    ///   1. Create an initial layer surface (untargeted) so App can be constructed
    ///   2. Roundtrip the event queue to populate SCTK OutputState
    ///   3. Find the XR glasses wl_output by matching connector name
    ///   4. Recreate the layer surface targeting the correct output
    pub fn run(
        _primary: &OutputInfo,
        xr: &OutputInfo,
        capture: ScreenCapture,
        gpu: GpuPipeline,
        renderer: Renderer,
        pose_reader: PoseReader,
        config: Config,
    ) -> Result<()> {
        // Connect to Wayland compositor
        let conn = Connection::connect_to_env().context("Failed to connect to Wayland")?;

        // The event queue and all SCTK globals are typed to App
        let (globals, mut event_queue) =
            registry_queue_init::<App>(&conn).context("Failed to initialize Wayland registry")?;
        let qh = event_queue.handle();

        // Bind SCTK globals
        let compositor =
            CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
        let layer_shell =
            LayerShell::bind(&globals, &qh).context("wlr-layer-shell not available")?;
        let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;
        let output_state = OutputState::new(&globals, &qh);
        let registry_state = RegistryState::new(&globals);

        let xr_width = xr.width as u32;
        let xr_height = xr.height as u32;

        // Create an initial untargeted layer surface so we can construct App
        // and roundtrip the event queue (needed to populate OutputState).
        let initial_surface = compositor.create_surface(&qh);
        let initial_layer = layer_shell.create_layer_surface(
            &qh,
            initial_surface,
            Layer::Overlay,
            Some("breezy-cosmic"),
            None, // untargeted — will be replaced after output discovery
        );
        configure_layer(&initial_layer, xr_width, xr_height);
        initial_layer.commit();

        // Allocate SHM buffer pool (triple-buffered: 3x frame size)
        let pool_size = (xr_width * xr_height * 4 * 3) as usize;
        let pool = SlotPool::new(pool_size, &shm).context("Failed to create SHM buffer pool")?;

        let mut app = App {
            registry_state,
            output_state,
            shm,
            layer: initial_layer,
            pool,
            configured: false,
            width: xr_width,
            height: xr_height,
            exit: false,
            capture,
            gpu,
            renderer,
            pose_reader,
            config,
            source_size: None,
            frame_count: 0,
            loop_start: std::time::Instant::now(),
        };

        // ── Output discovery phase ──────────────────────────────────
        //
        // Roundtrip the event queue to populate SCTK OutputState with
        // all connected wl_output objects and their metadata (name, modes, etc.)
        info!("Discovering Wayland outputs...");
        event_queue.roundtrip(&mut app).context("Roundtrip 1 failed")?;
        event_queue.roundtrip(&mut app).context("Roundtrip 2 failed")?;
        event_queue.roundtrip(&mut app).context("Roundtrip 3 failed")?;

        // Find the XR glasses output by connector name (e.g. "HDMI-A-1")
        let xr_wl_output = find_output_by_name(&app.output_state, &xr.name);

        if let Some(ref target) = xr_wl_output {
            // Found the XR output — recreate layer surface targeting it
            info!("Found XR output '{}', creating targeted overlay", xr.name);

            let targeted_surface = compositor.create_surface(&qh);
            let targeted_layer = layer_shell.create_layer_surface(
                &qh,
                targeted_surface,
                Layer::Overlay,
                Some("breezy-cosmic"),
                Some(target),
            );
            configure_layer(&targeted_layer, xr_width, xr_height);
            targeted_layer.commit();

            // Replace the untargeted surface with the targeted one.
            // Dropping the old LayerSurface sends the protocol destroy message.
            app.layer = targeted_layer;
            app.configured = false;
        } else {
            // Couldn't find XR output by name — fall back to untargeted.
            // Log all discovered outputs to help debugging.
            warn!(
                "XR output '{}' not found in Wayland outputs, falling back to compositor default",
                xr.name
            );
            for output in app.output_state.outputs() {
                if let Some(info) = app.output_state.info(&output) {
                    warn!(
                        "  Available output: {:?} ({} {})",
                        info.name, info.make, info.model
                    );
                }
            }
        }

        info!(
            "Layer surface created ({}x{}), overlay mode, awaiting configure...",
            xr_width, xr_height
        );

        // ── Event loop — SCTK drives everything from here ──────────
        info!("Entering event loop (Ctrl+C to exit)");
        loop {
            event_queue
                .blocking_dispatch(&mut app)
                .context("Wayland dispatch failed")?;

            if app.exit {
                info!("Layer surface closed by compositor");
                break;
            }
        }

        Ok(())
    }

    /// The core render+present function, called on each frame callback.
    ///
    /// Pipeline: capture primary monitor → upload to GPU → render with head tracking
    /// → read back pixels → write to SHM buffer → present to Wayland compositor
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let width = self.width;
        let height = self.height;
        let stride = width as i32 * 4;

        // 0. Check for re-pin signal (SIGUSR1)
        if REPIN_REQUESTED.swap(false, Ordering::SeqCst) {
            info!("Re-pin requested — resetting orientation reference");
            self.renderer.reset_smooth_follow();
        }

        // 1. Read current head pose from XRLinuxDriver shared memory
        let pose = self.pose_reader.try_read();

        if self.frame_count % 120 == 0 {
            if let Some(ref p) = pose {
                let q = p.orientation();
                info!(
                    "Pose: q=({:.3}, {:.3}, {:.3}, {:.3}) fov={:.1}° ts={}",
                    q.x, q.y, q.z, q.w, p.display_fov, p.timestamp_ms
                );
            } else {
                warn!("Pose: NONE (try_read returned None)");
            }
        }

        // 2. Capture frame from primary monitor, or use test pattern if unavailable
        match self.capture.capture_frame() {
            Ok(frame) => {
                self.source_size = Some((frame.width, frame.height));
                self.gpu.upload_frame(&frame);
            }
            Err(e) => {
                if self.frame_count == 0 {
                    warn!("Capture unavailable ({}), using test pattern", e);
                    let test = generate_test_pattern(width, height);
                    self.source_size = Some((width, height));
                    self.gpu.upload_frame(&test);
                }
            }
        }

        // 4. Compute view-projection matrix from head pose
        let mvp = if let Some(ref pose_data) = pose {
            self.renderer.compute_view_matrix(
                pose_data,
                &self.config.display,
                (width, height),
                self.source_size,
            )
        } else {
            // No pose data — render flat/centered (identity)
            Mat4::IDENTITY
        };

        // 5. GPU render (offscreen) → read back RGBA pixels
        let pixels = match self.gpu.render_frame(&mvp) {
            Ok(p) => p,
            Err(e) => {
                debug!("GPU render failed: {}", e);
                self.request_next_frame(qh);
                return;
            }
        };

        // 6. Allocate SHM buffer and write pixels for Wayland presentation
        let (buffer, canvas) = match self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(bc) => bc,
            Err(e) => {
                error!("Failed to create SHM buffer: {}", e);
                self.request_next_frame(qh);
                return;
            }
        };

        // Convert RGBA (GPU output) → ARGB8888 (Wayland SHM format, little-endian)
        // GPU output bytes: [R, G, B, A]
        // ARGB8888 LE bytes: [B, G, R, A]
        let pixel_count = (width * height) as usize;
        if pixels.len() >= pixel_count * 4 && canvas.len() >= pixel_count * 4 {
            for i in 0..pixel_count {
                let src = i * 4;
                let dst = i * 4;
                canvas[dst] = pixels[src + 2];     // B
                canvas[dst + 1] = pixels[src + 1]; // G
                canvas[dst + 2] = pixels[src];     // R
                canvas[dst + 3] = pixels[src + 3]; // A
            }
        }

        // 7. Present: damage → request next frame → attach buffer → commit
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);

        self.request_next_frame(qh);

        if let Err(e) = buffer.attach_to(self.layer.wl_surface()) {
            error!("Buffer attach failed: {}", e);
            return;
        }
        self.layer.commit();

        // Stats
        self.frame_count += 1;
        if self.frame_count % 60 == 0 {
            let elapsed = self.loop_start.elapsed().as_secs_f64();
            let fps = self.frame_count as f64 / elapsed;
            info!(
                "Frame {} | {:.1} FPS | pose: {}",
                self.frame_count,
                fps,
                if pose.is_some() { "active" } else { "none" },
            );
        }
    }

    /// Request the compositor to send the next frame callback
    fn request_next_frame(&self, qh: &QueueHandle<Self>) {
        self.layer
            .wl_surface()
            .frame(qh, self.layer.wl_surface().clone());
    }
}

// ── Helper functions ────────────────────────────────────────────────

use crate::capture::{CapturedFrame, PixelFormat};

/// Generate a test pattern for head-tracking verification.
///
/// Renders a grid of colored tiles with a central crosshair,
/// so head movement is immediately visible as the pattern shifts.
fn generate_test_pattern(width: u32, height: u32) -> CapturedFrame {
    let mut data = vec![0u8; (width * height * 4) as usize];
    let tile_size = 64u32;

    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) * 4) as usize;
            let tx = x / tile_size;
            let ty = y / tile_size;

            // Checkerboard base pattern
            let checker = (tx + ty) % 2 == 0;

            // Color based on quadrant
            let cx = x as f32 / width as f32;
            let cy = y as f32 / height as f32;

            let (r, g, b) = if checker {
                // Gradient tiles: position-based color
                ((cx * 200.0) as u8 + 40, (cy * 200.0) as u8 + 40, 120)
            } else {
                // Dark tiles
                (30, 30, 40)
            };

            // Draw crosshair at center (2px wide)
            let is_crosshair = (x as i32 - width as i32 / 2).unsigned_abs() < 2
                || (y as i32 - height as i32 / 2).unsigned_abs() < 2;

            // Draw border around edges (4px)
            let is_border = x < 4 || x >= width - 4 || y < 4 || y >= height - 4;

            if is_crosshair {
                data[offset] = 255; // R
                data[offset + 1] = 255; // G
                data[offset + 2] = 0;   // B
                data[offset + 3] = 255; // A
            } else if is_border {
                data[offset] = 255;
                data[offset + 1] = 0;
                data[offset + 2] = 0;
                data[offset + 3] = 255;
            } else {
                data[offset] = r;
                data[offset + 1] = g;
                data[offset + 2] = b;
                data[offset + 3] = 255;
            }
        }
    }

    CapturedFrame {
        data,
        width,
        height,
        stride: width * 4,
        format: PixelFormat::Abgr8888,
        timestamp_ns: 0,
    }
}

/// Configure a layer surface as a fullscreen XR overlay
fn configure_layer(layer: &LayerSurface, width: u32, height: u32) {
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(width, height);
}

/// Find a wl_output by connector name (e.g. "HDMI-A-1") in SCTK OutputState
fn find_output_by_name(
    output_state: &OutputState,
    target_name: &str,
) -> Option<wl_output::WlOutput> {
    for output in output_state.outputs() {
        if let Some(info) = output_state.info(&output) {
            if info.name.as_deref() == Some(target_name) {
                return Some(output);
            }
        }
    }
    None
}

// ── SCTK trait implementations ──────────────────────────────────────

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {}

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {}

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Frame callback → drive one render cycle
        self.draw(qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {}

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }

        info!("Layer surface configured: {}x{}", self.width, self.height);

        if !self.configured {
            self.configured = true;
            // First configure event — begin rendering
            self.draw(qh);
        }
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {}

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {}

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

// ── SCTK delegate macros (generate Dispatch impls) ──────────────────

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}
