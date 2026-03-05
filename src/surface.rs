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
use tracing::{debug, error, info, warn};
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

    // Stats
    frame_count: u64,
    loop_start: std::time::Instant,
}

impl App {
    /// Run the breezy-cosmic overlay on the XR glasses output.
    ///
    /// This connects to Wayland, creates the layer-shell surface,
    /// and enters the SCTK event loop. Returns when the surface is closed.
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

        // Create the wl_surface for our overlay
        let surface = compositor.create_surface(&qh);

        // For now, create the layer surface without targeting a specific output.
        // The SCTK OutputState needs a roundtrip to discover output info (names),
        // but we can't roundtrip without an App instance. Since we know the XR
        // glasses are a separate display, the compositor will place the overlay
        // on the default output. We'll add output targeting in Phase 2 once we
        // have the Wayland event model fully wired up.
        //
        // TODO Phase 2: Implement two-phase init with output discovery roundtrip
        // so we can pass Some(&wl_output) here to target the XR glasses directly.
        let layer = layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("breezy-cosmic"),
            None, // Will target specific output in Phase 2
        );

        // Configure: fullscreen overlay, no keyboard, don't push other surfaces
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(xr.width as u32, xr.height as u32);

        // Initial commit (no buffer attached) — required by protocol
        layer.commit();

        let xr_width = xr.width as u32;
        let xr_height = xr.height as u32;

        // Allocate SHM buffer pool (triple-buffered: 3x frame size)
        let pool_size = (xr_width * xr_height * 4 * 3) as usize;
        let pool = SlotPool::new(pool_size, &shm).context("Failed to create SHM buffer pool")?;

        info!(
            "Layer surface created ({}x{}), overlay mode, awaiting configure...",
            xr_width, xr_height
        );

        let mut app = App {
            registry_state,
            output_state,
            shm,
            layer,
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
            frame_count: 0,
            loop_start: std::time::Instant::now(),
        };

        // Event loop — SCTK drives everything from here
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

        // 1. Read current head pose from XRLinuxDriver shared memory
        let pose = self.pose_reader.try_read();

        // 2. Capture frame from primary monitor (via grim)
        let frame = match self.capture.capture_frame() {
            Ok(f) => f,
            Err(e) => {
                debug!("Capture failed: {}", e);
                self.request_next_frame(qh);
                return;
            }
        };

        // 3. Upload captured frame to GPU texture
        self.gpu.upload_frame(&frame);

        // 4. Compute view-projection matrix from head pose
        let mvp = if let Some(ref pose_data) = pose {
            self.renderer
                .compute_view_matrix(pose_data, &self.config.display, (width, height))
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
