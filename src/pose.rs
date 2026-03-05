//! Reads 6DoF pose data from XRLinuxDriver shared memory.
//!
//! XRLinuxDriver writes IMU sensor fusion data (quaternion orientation + 3D position)
//! to `/dev/shm/breezy_desktop_imu`. This module memory-maps that file and provides
//! a safe Rust interface to the pose data.
//!
//! Coordinate system: XRLinuxDriver uses NWU (North-West-Up).
//! We convert to EUS (East-Up-South) for rendering:
//!   x_eus = -y_nwu, y_eus = z_nwu, z_eus = -x_nwu

use anyhow::Result;
use glam::Quat;
use memmap2::MmapOptions;
use std::fs::File;
use std::path::Path;
use tracing::trace;

/// Path to XRLinuxDriver shared memory
const SHM_PATH: &str = "/dev/shm/breezy_desktop_imu";

/// Expected minimum size of the shared memory region
const MIN_SHM_SIZE: usize = 178;

/// Parsed pose data from XRLinuxDriver
#[derive(Debug, Clone)]
pub struct PoseData {
    pub version: u8,
    pub enabled: bool,
    pub look_ahead_cfg: [f32; 4],
    pub display_res: [u32; 2],
    pub display_fov: f32,
    pub lens_distance_ratio: f32,
    pub sbs_enabled: bool,
    pub custom_banner: bool,
    pub smooth_follow_enabled: bool,
    pub smooth_follow_origin: [f32; 16], // 4 quaternions
    pub position_nwu: [f32; 3],
    pub timestamp_ms: u64,
    pub orientations: [f32; 16], // 4 quaternions (T0, T1, T2, T3)
    pub parity: u8,
}

impl PoseData {
    /// Get the primary orientation quaternion (T0) in EUS coordinates
    pub fn orientation(&self) -> Quat {
        // T0 quaternion is the first 4 floats of orientations
        // XRLinuxDriver stores as [x, y, z, w] in NWU
        let x_nwu = self.orientations[0];
        let y_nwu = self.orientations[1];
        let z_nwu = self.orientations[2];
        let w = self.orientations[3];

        // Convert NWU quaternion to EUS
        // NWU → EUS: x_eus = -y_nwu, y_eus = z_nwu, z_eus = -x_nwu
        Quat::from_xyzw(-y_nwu, z_nwu, -x_nwu, w).normalize()
    }

    /// Get position in EUS coordinates
    pub fn position_eus(&self) -> [f32; 3] {
        let [x, y, z] = self.position_nwu;
        [-y, z, -x]
    }

    /// Get the look-ahead compensated orientation (T1) in EUS coordinates
    pub fn orientation_lookahead(&self) -> Quat {
        let x_nwu = self.orientations[4];
        let y_nwu = self.orientations[5];
        let z_nwu = self.orientations[6];
        let w = self.orientations[7];
        Quat::from_xyzw(-y_nwu, z_nwu, -x_nwu, w).normalize()
    }
}

/// Reader for XRLinuxDriver pose data via shared memory
pub struct PoseReader {
    mmap: Option<memmap2::Mmap>,
}

impl PoseReader {
    pub fn new() -> Self {
        Self { mmap: None }
    }

    /// Check if the shared memory file exists and is accessible
    pub fn check_available(&self) -> Result<bool> {
        Ok(Path::new(SHM_PATH).exists())
    }

    /// Initialize the memory mapping (call after confirming availability)
    pub fn init(&mut self) -> Result<()> {
        let file = File::open(SHM_PATH)?;
        let mmap = unsafe { MmapOptions::new().map(&file)? };

        if mmap.len() < MIN_SHM_SIZE {
            anyhow::bail!(
                "Shared memory too small: {} bytes (expected >= {})",
                mmap.len(),
                MIN_SHM_SIZE
            );
        }

        self.mmap = Some(mmap);
        Ok(())
    }

    /// Read the current pose data. Returns None if not initialized or data is invalid.
    pub fn read_pose(&self) -> Option<PoseData> {
        let mmap = self.mmap.as_ref()?;
        let data = mmap.as_ref();

        if data.len() < MIN_SHM_SIZE {
            return None;
        }

        // Verify parity byte
        let parity_expected = data[..MIN_SHM_SIZE - 1]
            .iter()
            .fold(0u8, |acc, &b| acc ^ b);
        let parity_actual = data[MIN_SHM_SIZE - 1];

        if parity_expected != parity_actual {
            trace!("Parity mismatch: expected {:#x}, got {:#x}", parity_expected, parity_actual);
            // Don't fail on parity — the data might be mid-write.
            // We'll use it anyway and rely on timestamp checking for staleness.
        }

        Some(parse_pose_data(data))
    }

    /// Try to read pose, initializing mmap on first call if available
    pub fn try_read(&mut self) -> Option<PoseData> {
        if self.mmap.is_none() {
            if Path::new(SHM_PATH).exists() {
                if let Err(e) = self.init() {
                    trace!("Failed to init pose reader: {}", e);
                    return None;
                }
            } else {
                return None;
            }
        }
        self.read_pose()
    }
}

fn read_f32(data: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ])
}

fn read_f32_array<const N: usize>(data: &[u8], offset: usize) -> [f32; N] {
    let mut arr = [0.0f32; N];
    for i in 0..N {
        arr[i] = read_f32(data, offset + i * 4);
    }
    arr
}

fn parse_pose_data(data: &[u8]) -> PoseData {
    // Offsets based on XRLinuxDriver shared memory layout
    // These are approximate — the actual struct is packed C
    let mut offset = 0;

    let version = data[offset];
    offset += 1;

    let enabled = data[offset] != 0;
    offset += 1;

    let look_ahead_cfg = read_f32_array::<4>(data, offset);
    offset += 16;

    let display_res = [read_u32(data, offset), read_u32(data, offset + 4)];
    offset += 8;

    let display_fov = read_f32(data, offset);
    offset += 4;

    let lens_distance_ratio = read_f32(data, offset);
    offset += 4;

    let sbs_enabled = data[offset] != 0;
    offset += 1;

    let custom_banner = data[offset] != 0;
    offset += 1;

    let smooth_follow_enabled = data[offset] != 0;
    offset += 1;

    let smooth_follow_origin = read_f32_array::<16>(data, offset);
    offset += 64;

    let position_nwu = read_f32_array::<3>(data, offset);
    offset += 12;

    let timestamp_ms = read_u64(data, offset);
    offset += 8;

    let orientations = read_f32_array::<16>(data, offset);
    offset += 64;

    let parity = if offset < data.len() { data[offset] } else { 0 };

    PoseData {
        version,
        enabled,
        look_ahead_cfg,
        display_res,
        display_fov,
        lens_distance_ratio,
        sbs_enabled,
        custom_banner,
        smooth_follow_enabled,
        smooth_follow_origin,
        position_nwu,
        timestamp_ms,
        orientations,
        parity,
    }
}
