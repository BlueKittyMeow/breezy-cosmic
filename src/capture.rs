//! Screen capture from the primary monitor.
//!
//! Uses a Python helper (`breezy_portal_capture.py`) that manages the
//! xdg-desktop-portal ScreenCast session and writes raw video frames to
//! shared memory via GStreamer + PipeWire.
//!
//! SHM layout at /dev/shm/breezy_capture:
//!   [0..4]   magic      u32 LE  0x42434150 ("BCAP")
//!   [4..8]   width      u32 LE
//!   [8..12]  height     u32 LE
//!   [12..16] stride     u32 LE  (bytes per row)
//!   [16..20] format     u32 LE  (0=BGRx, 1=RGBx, 2=BGRA, 3=RGBA)
//!   [20..24] frame_seq  u32 LE  (increments each new frame)
//!   [24..32] timestamp  u64 LE  (monotonic ns)
//!   [32..]   pixel data (height * stride bytes)

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use tracing::{debug, info, warn, error};

const SHM_PATH: &str = "/dev/shm/breezy_capture";
const HEADER_SIZE: usize = 32;
const MAGIC: u32 = 0x42434150; // 'BCAP'

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

/// Screen capture backend
pub struct ScreenCapture {
    target_output: String,
    width: u32,
    height: u32,
    source_type: String,
    helper_process: Option<Child>,
    shm: Option<ShmReader>,
    last_seq: u32,
}

/// Reader for the shared memory capture buffer
struct ShmReader {
    mmap: memmap2::Mmap,
    _file: std::fs::File,
}

impl ShmReader {
    fn open() -> Result<Self> {
        let file = std::fs::File::open(SHM_PATH)
            .context("Failed to open capture SHM")?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(Self { mmap, _file: file })
    }

    /// Re-map the file (in case it was resized)
    fn remap(&mut self) -> Result<()> {
        let file = std::fs::File::open(SHM_PATH)
            .context("Failed to re-open capture SHM")?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        self._file = file;
        self.mmap = mmap;
        Ok(())
    }

    fn magic(&self) -> u32 {
        if self.mmap.len() < HEADER_SIZE {
            return 0;
        }
        u32::from_le_bytes(self.mmap[0..4].try_into().unwrap())
    }

    fn width(&self) -> u32 {
        u32::from_le_bytes(self.mmap[4..8].try_into().unwrap())
    }

    fn height(&self) -> u32 {
        u32::from_le_bytes(self.mmap[8..12].try_into().unwrap())
    }

    fn stride(&self) -> u32 {
        u32::from_le_bytes(self.mmap[12..16].try_into().unwrap())
    }

    fn format_code(&self) -> u32 {
        u32::from_le_bytes(self.mmap[16..20].try_into().unwrap())
    }

    fn frame_seq(&self) -> u32 {
        u32::from_le_bytes(self.mmap[20..24].try_into().unwrap())
    }

    fn timestamp_ns(&self) -> u64 {
        u64::from_le_bytes(self.mmap[24..32].try_into().unwrap())
    }

    fn pixel_data(&self) -> &[u8] {
        let h = self.height() as usize;
        let s = self.stride() as usize;
        let end = HEADER_SIZE + h * s;
        if end <= self.mmap.len() {
            &self.mmap[HEADER_SIZE..end]
        } else {
            &[]
        }
    }
}

impl ScreenCapture {
    /// Create a new screen capture targeting the named output
    pub fn new(output_name: &str, width: u32, height: u32, source: &str) -> Self {
        Self {
            target_output: output_name.to_string(),
            width,
            height,
            source_type: source.to_string(),
            helper_process: None,
            shm: None,
            last_seq: 0,
        }
    }

    /// Initialize the capture session.
    ///
    /// Launches the Python portal capture helper which handles:
    /// 1. xdg-desktop-portal ScreenCast session
    /// 2. PipeWire stream via GStreamer
    /// 3. Writing raw frames to /dev/shm/breezy_capture
    pub fn init(&mut self) -> Result<()> {
        // Find the helper script
        let helper_path = Self::find_helper()?;
        info!("Launching capture helper: {}", helper_path.display());

        // Clean up any stale SHM file
        let _ = std::fs::remove_file(SHM_PATH);

        // Launch the helper
        let child = Command::new("/usr/bin/python3")
            .arg(&helper_path)
            .arg(&self.target_output)
            .arg(&self.source_type)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to launch capture helper")?;

        let pid = child.id();
        self.helper_process = Some(child);
        info!("Capture helper launched (PID {})", pid);

        // Wait for the helper to create the SHM file and start writing frames.
        // The portal may show a dialog on first use, so we allow up to 30 seconds.
        info!("Waiting for capture helper to initialize (portal dialog may appear)...");
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(120);

        loop {
            if start.elapsed() > timeout {
                // Check if helper is still alive
                if let Some(ref mut child) = self.helper_process {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            // Helper exited — read its stderr
                            let mut stderr_output = String::new();
                            if let Some(ref mut stderr) = child.stderr {
                                use std::io::Read;
                                let _ = stderr.read_to_string(&mut stderr_output);
                            }
                            anyhow::bail!(
                                "Capture helper exited with {}: {}",
                                status, stderr_output.trim()
                            );
                        }
                        _ => {}
                    }
                }
                anyhow::bail!(
                    "Capture helper did not produce frames within {}s. \
                     Check if the portal dialog needs attention.",
                    timeout.as_secs()
                );
            }

            // Try to open and read SHM
            if std::path::Path::new(SHM_PATH).exists() {
                if let Ok(reader) = ShmReader::open() {
                    if reader.magic() == MAGIC && reader.frame_seq() > 0 {
                        let w = reader.width();
                        let h = reader.height();
                        info!(
                            "Capture helper ready: {}x{} (seq={})",
                            w, h, reader.frame_seq()
                        );
                        self.shm = Some(reader);
                        return Ok(());
                    }
                }
            }

            // Check helper is still running
            if let Some(ref mut child) = self.helper_process {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let mut stderr_output = String::new();
                        if let Some(ref mut stderr) = child.stderr {
                            use std::io::Read;
                            let _ = stderr.read_to_string(&mut stderr_output);
                        }
                        anyhow::bail!(
                            "Capture helper exited early with {}: {}",
                            status, stderr_output.trim()
                        );
                    }
                    _ => {}
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// Capture a single frame from shared memory
    pub fn capture_frame(&mut self) -> Result<CapturedFrame> {
        let reader = self.shm.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Capture not initialized"))?;

        // Re-map to see latest data
        reader.remap().context("Failed to remap capture SHM")?;

        if reader.magic() != MAGIC {
            anyhow::bail!("Capture SHM has invalid magic");
        }

        let seq = reader.frame_seq();
        if seq == self.last_seq {
            // No new frame — return the same data anyway (caller expects a frame)
            // This is fine since the renderer will just re-draw the same texture
        }
        self.last_seq = seq;

        let width = reader.width();
        let height = reader.height();
        let stride = reader.stride();
        let timestamp_ns = reader.timestamp_ns();
        let fmt_code = reader.format_code();

        let pixel_data = reader.pixel_data();
        if pixel_data.is_empty() {
            anyhow::bail!("Capture SHM pixel data is empty");
        }

        // Convert BGRx → RGBA for the GPU pipeline
        let _format = match fmt_code {
            0 => PixelFormat::Xrgb8888,  // BGRx (B in low byte = xRGB in LE)
            1 => PixelFormat::Xbgr8888,  // RGBx
            2 => PixelFormat::Argb8888,  // BGRA
            3 => PixelFormat::Abgr8888,  // RGBA
            _ => PixelFormat::Xrgb8888,  // Default assumption
        };

        // BGRx from GStreamer: bytes are [B, G, R, x] per pixel
        // We need RGBA: [R, G, B, A]
        let rgba = if fmt_code == 0 {
            // BGRx → RGBA swizzle
            let mut out = Vec::with_capacity(pixel_data.len());
            for row in 0..height as usize {
                let row_start = row * stride as usize;
                let row_end = row_start + width as usize * 4;
                if row_end > pixel_data.len() {
                    break;
                }
                for pixel in pixel_data[row_start..row_end].chunks(4) {
                    out.push(pixel[2]); // R (from B,G,R,x)
                    out.push(pixel[1]); // G
                    out.push(pixel[0]); // B
                    out.push(255);      // A
                }
            }
            out
        } else {
            // Assume already RGBA-ish, just copy
            pixel_data.to_vec()
        };

        Ok(CapturedFrame {
            data: rgba,
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Abgr8888,
            timestamp_ns,
        })
    }

    /// Find the capture helper script
    fn find_helper() -> Result<PathBuf> {
        // Look next to the binary first
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));

        let candidates = [
            exe_dir.as_ref().map(|d| d.join("breezy_portal_capture.py")),
            exe_dir.as_ref().map(|d| d.join("../breezy_portal_capture.py")),
            Some(PathBuf::from("breezy_portal_capture.py")),
            Some(PathBuf::from(
                "/home/bluekitty/Documents/Git/breezy-cosmic/breezy_portal_capture.py",
            )),
        ];

        for candidate in candidates.iter().flatten() {
            if candidate.exists() {
                return Ok(candidate.clone());
            }
        }

        anyhow::bail!(
            "Could not find breezy_portal_capture.py. \
             Place it next to the breezy-cosmic binary or in the project root."
        )
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        // Kill the helper process on cleanup
        if let Some(ref mut child) = self.helper_process {
            info!("Stopping capture helper (PID {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        // Clean up SHM
        let _ = std::fs::remove_file(SHM_PATH);
    }
}
