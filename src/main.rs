// Phase 1 prototype — layer-shell overlay with head-tracked rendering
#![allow(dead_code, unused_imports)]

mod capture;
mod config;
mod gpu;
mod output;
mod pose;
mod render;
mod surface;

use anyhow::{Context, Result};
use clap::Parser;
use glam::Mat4;
use std::path::PathBuf;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::gpu::GpuPipeline;
use crate::output::OutputManager;
use crate::pose::PoseReader;
use crate::render::Renderer;
use crate::surface::App;

#[derive(Parser, Debug)]
#[command(name = "breezy-cosmic", about = "Head-tracked virtual display for XR glasses on COSMIC")]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Override XR output match string (EDID)
    #[arg(long)]
    xr_match: Option<String>,

    /// List detected outputs and exit
    #[arg(long)]
    list_outputs: bool,

    /// Dry run: detect glasses and show pose data without rendering
    #[arg(long)]
    dry_run: bool,

    /// Render test: capture one frame, render with GPU, save as PPM
    #[arg(long)]
    render_test: bool,
}

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Load configuration
    let config = if let Some(path) = &args.config {
        Config::from_file(path).context("Failed to load config file")?
    } else {
        Config::load_or_default()
    };

    info!("breezy-cosmic v{}", env!("CARGO_PKG_VERSION"));
    info!("Configuration: {:?}", config);

    // Check for XRLinuxDriver shared memory
    let mut pose_reader = PoseReader::new();
    match pose_reader.check_available() {
        Ok(true) => {
            info!("XRLinuxDriver detected — pose data available");
            if let Err(e) = pose_reader.init() {
                warn!("Failed to initialize pose reader: {}", e);
            }
        }
        Ok(false) => {
            warn!("XRLinuxDriver shared memory not found at /dev/shm/breezy_desktop_imu");
            warn!("Make sure XRLinuxDriver is installed and your XR glasses are connected.");
            if !args.dry_run && !args.list_outputs {
                warn!("Continuing without pose tracking — display will be static.");
            }
        }
        Err(e) => {
            warn!("Error checking XRLinuxDriver: {}", e);
        }
    }

    // Dry run mode: just show pose data
    if args.dry_run {
        return dry_run(&mut pose_reader);
    }

    // Detect outputs using cosmic-randr
    let mut output_manager = OutputManager::new_standalone();

    if args.list_outputs {
        output_manager.list_outputs();
        return Ok(());
    }

    // Find XR and primary outputs
    let xr_match = args
        .xr_match
        .as_deref()
        .unwrap_or(&config.output.xr_match);

    let (primary, xr) = output_manager.find_outputs(xr_match, &config.output.primary_match)?;
    info!("Primary output: {} ({}x{})", primary.name, primary.width, primary.height);
    info!("XR output: {} ({}) {}x{}", xr.name, xr.description, xr.width, xr.height);

    // Initialize screen capture
    let mut capture = capture::ScreenCapture::new(
        &primary.name,
        primary.width as u32,
        primary.height as u32,
    );
    capture
        .init()
        .context("Failed to initialize screen capture")?;

    // Initialize GPU pipeline
    let xr_width = xr.width as u32;
    let xr_height = xr.height as u32;

    info!("Initializing GPU pipeline ({}x{})...", xr_width, xr_height);
    let mut gpu = pollster::block_on(GpuPipeline::new(xr_width, xr_height))
        .context("Failed to initialize GPU pipeline")?;

    // Initialize head-tracking renderer
    let mut renderer = Renderer::new();

    // Render test mode: capture one frame, render it, save to file
    if args.render_test {
        return render_test(&mut capture, &mut gpu, &mut renderer, &mut pose_reader, &config);
    }

    // ── Main path: SCTK layer-shell event loop ──
    info!("Starting layer-shell overlay...");
    App::run(&primary, &xr, capture, gpu, renderer, pose_reader, config)
}

/// Render test: capture one frame, process through GPU, save result as PPM
fn render_test(
    capture: &mut capture::ScreenCapture,
    gpu: &mut GpuPipeline,
    renderer: &mut Renderer,
    pose_reader: &mut PoseReader,
    config: &Config,
) -> Result<()> {
    info!("Render test mode — capturing one frame and rendering through GPU...");

    let frame = capture
        .capture_frame()
        .context("Failed to capture test frame")?;
    info!("Captured frame: {}x{}", frame.width, frame.height);

    gpu.upload_frame(&frame);

    let mvp = if let Some(pose) = pose_reader.try_read() {
        info!("Using live pose data for render test");
        renderer.compute_view_matrix(&pose, &config.display, (gpu.width, gpu.height))
    } else {
        info!("No pose data — rendering with identity matrix");
        Mat4::IDENTITY
    };

    let pixels = gpu
        .render_frame(&mvp)
        .context("Failed to render test frame")?;

    // Save as PPM
    let out_path = "/tmp/breezy-render-test.ppm";
    let mut ppm = Vec::new();
    ppm.extend_from_slice(format!("P6\n{} {}\n255\n", gpu.width, gpu.height).as_bytes());
    for pixel in pixels.chunks(4) {
        ppm.push(pixel[0]); // R
        ppm.push(pixel[1]); // G
        ppm.push(pixel[2]); // B
    }
    std::fs::write(out_path, &ppm)?;

    info!("Render test saved to {}", out_path);
    info!(
        "Output: {}x{} ({} bytes)",
        gpu.width,
        gpu.height,
        ppm.len()
    );

    Ok(())
}

fn dry_run(pose_reader: &mut PoseReader) -> Result<()> {
    info!("Dry run mode — displaying pose data (Ctrl+C to exit)");
    info!("(If no data appears, check that XRLinuxDriver is running and glasses are connected)");

    loop {
        match pose_reader.try_read() {
            Some(pose) => {
                let q = pose.orientation();
                let pos = pose.position_eus();
                println!(
                    "Quat: ({:.3}, {:.3}, {:.3}, {:.3})  Pos: ({:.3}, {:.3}, {:.3})  ts: {}ms  follow: {}",
                    q.x, q.y, q.z, q.w,
                    pos[0], pos[1], pos[2],
                    pose.timestamp_ms,
                    if pose.smooth_follow_enabled { "on" } else { "off" },
                );
            }
            None => {
                println!("Waiting for pose data...");
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
