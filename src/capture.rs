//! Screen capture from the primary monitor.
//!
//! Uses ext-image-copy-capture-v1 protocol (preferred) or falls back to
//! wlr-screencopy-unstable-v1 to capture frames from the primary output.
//!
//! Captured frames are uploaded to GPU textures for rendering by the
//! head-tracked renderer.

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

/// Represents a captured frame ready for GPU upload
pub struct CapturedFrame {
    /// Raw pixel data (RGBA8 or BGRA8 depending on compositor)
    pub data: Vec<u8>,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Stride in bytes
    pub stride: u32,
    /// Pixel format
    pub format: PixelFormat,
    /// Monotonic timestamp of capture
    pub timestamp_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PixelFormat {
    Argb8888,
    Xrgb8888,
    Abgr8888,
    Xbgr8888,
}

impl PixelFormat {
    /// Bytes per pixel
    pub fn bpp(&self) -> u32 {
        4 // All supported formats are 32-bit
    }

    /// Whether the format needs channel swizzling to become RGBA
    pub fn needs_swizzle(&self) -> bool {
        matches!(self, PixelFormat::Argb8888 | PixelFormat::Xrgb8888)
    }
}

/// Screen capture backend abstraction
pub struct ScreenCapture {
    method: CaptureMethod,
    target_output: String,
    width: u32,
    height: u32,
}

#[derive(Debug)]
enum CaptureMethod {
    /// ext-image-copy-capture-v1 (preferred, Smithay-native)
    ImageCopyCapture,
    /// wlr-screencopy-unstable-v1 (fallback)
    WlrScreencopy,
    /// Fallback: use grim CLI tool
    GrimFallback,
}

impl ScreenCapture {
    /// Create a new screen capture targeting the named output
    pub fn new(output_name: &str, width: u32, height: u32) -> Self {
        Self {
            method: CaptureMethod::GrimFallback, // Start with simplest fallback
            target_output: output_name.to_string(),
            width,
            height,
        }
    }

    /// Initialize the capture session
    ///
    /// Tries protocols in order of preference:
    /// 1. ext-image-copy-capture-v1
    /// 2. wlr-screencopy-v1
    /// 3. grim CLI fallback
    pub fn init(&mut self) -> Result<()> {
        // TODO: Implement Wayland protocol-based capture.
        //
        // For the initial prototype, we use grim as a capture backend.
        // This is simpler to implement and lets us validate the rendering
        // pipeline before investing in protocol-level capture.
        //
        // The protocol path will be:
        // 1. Bind ext_image_copy_capture_manager_v1 global
        // 2. Create capture session for target output
        // 3. Receive buffer constraints (formats, sizes)
        // 4. Allocate SHM or dmabuf buffers
        // 5. Per-frame: attach buffer → capture → read pixels

        if Self::check_grim_available() {
            self.method = CaptureMethod::GrimFallback;
            info!("Using grim for screen capture (protocol capture coming in next iteration)");
            Ok(())
        } else {
            anyhow::bail!(
                "No capture method available. Install grim: sudo apt install grim\n\
                 (Protocol-based capture will be added in the next version)"
            )
        }
    }

    /// Capture a single frame
    pub fn capture_frame(&self) -> Result<CapturedFrame> {
        match self.method {
            CaptureMethod::GrimFallback => self.capture_via_grim(),
            CaptureMethod::ImageCopyCapture => {
                // TODO: Implement protocol-based capture
                self.capture_via_grim()
            }
            CaptureMethod::WlrScreencopy => {
                // TODO: Implement protocol-based capture
                self.capture_via_grim()
            }
        }
    }

    /// Capture using the `grim` command-line tool
    fn capture_via_grim(&self) -> Result<CapturedFrame> {
        use std::process::Command;

        // grim can output raw pixel data to stdout with -t ppm,
        // or we can capture to a temp file as PNG and decode it.
        // For performance, we'll use raw PPM format to stdout.
        let output = Command::new("grim")
            .args([
                "-o",
                &self.target_output,
                "-t",
                "ppm",
                "-",
            ])
            .output()
            .context("Failed to run grim")?;

        if !output.status.success() {
            anyhow::bail!(
                "grim capture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Parse PPM format (P6 binary)
        parse_ppm(&output.stdout)
    }

    fn check_grim_available() -> bool {
        std::process::Command::new("grim")
            .arg("--help")
            .output()
            .is_ok()
    }
}

/// Parse PPM (P6 binary) image data into a CapturedFrame
fn parse_ppm(data: &[u8]) -> Result<CapturedFrame> {
    let data_str = std::str::from_utf8(data).unwrap_or("");

    // PPM P6 header: "P6\n<width> <height>\n<maxval>\n<binary data>"
    // Skip "P6\n"
    if !data_str.starts_with("P6") {
        anyhow::bail!("Not a valid PPM P6 file");
    }
    let mut pos = data_str.find('\n').unwrap_or(2) + 1;

    // Skip comments
    while pos < data.len() && data[pos] == b'#' {
        pos = data_str[pos..].find('\n').map(|p| pos + p + 1).unwrap_or(data.len());
    }

    // Read width and height
    let header_rest = &data_str[pos..];
    let parts: Vec<&str> = header_rest.split_whitespace().take(3).collect();
    if parts.len() < 3 {
        anyhow::bail!("Invalid PPM header");
    }

    let width: u32 = parts[0].parse().context("Invalid PPM width")?;
    let height: u32 = parts[1].parse().context("Invalid PPM height")?;
    let _maxval: u32 = parts[2].parse().context("Invalid PPM maxval")?;

    // Find start of binary data (after the third whitespace-delimited value + one byte)
    let mut ws_count = 0;
    let mut binary_start = pos;
    while binary_start < data.len() && ws_count < 3 {
        if data[binary_start].is_ascii_whitespace() {
            ws_count += 1;
            // Skip consecutive whitespace
            while binary_start + 1 < data.len() && data[binary_start + 1].is_ascii_whitespace() {
                binary_start += 1;
            }
        }
        binary_start += 1;
    }

    let rgb_data = &data[binary_start..];
    let expected_size = (width * height * 3) as usize;

    if rgb_data.len() < expected_size {
        anyhow::bail!(
            "PPM data too short: {} bytes (expected {})",
            rgb_data.len(),
            expected_size
        );
    }

    // Convert RGB to RGBA
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for pixel in rgb_data[..expected_size].chunks(3) {
        rgba.push(pixel[0]); // R
        rgba.push(pixel[1]); // G
        rgba.push(pixel[2]); // B
        rgba.push(255);      // A
    }

    debug!("Captured frame: {}x{} ({} bytes)", width, height, rgba.len());

    Ok(CapturedFrame {
        data: rgba,
        width,
        height,
        stride: width * 4,
        format: PixelFormat::Abgr8888,
        timestamp_ns: 0, // TODO: use monotonic clock
    })
}
