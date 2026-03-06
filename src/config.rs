use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub output: OutputConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// Virtual display distance from the user in meters
    #[serde(default = "default_distance")]
    pub distance: f64,

    /// Content scale factor
    #[serde(default = "default_scale")]
    pub scale: f64,

    /// Enable smooth follow mode (display gently tracks gaze)
    #[serde(default = "default_true")]
    pub smooth_follow: bool,

    /// Degrees of head rotation before smooth follow kicks in
    #[serde(default = "default_follow_threshold")]
    pub follow_threshold: f64,

    /// Yaw offset in degrees to pin the display (negative = left, positive = right)
    #[serde(default)]
    pub pin_yaw: f64,

    /// Pitch offset in degrees to pin the display (negative = down, positive = up)
    #[serde(default)]
    pub pin_pitch: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    /// Target capture framerate
    #[serde(default = "default_fps")]
    pub target_fps: u32,

    /// Prefer dmabuf over SHM for zero-copy capture
    #[serde(default = "default_true")]
    pub use_dmabuf: bool,

    /// Capture source: "monitor" (whole screen) or "window" (pick a window)
    #[serde(default = "default_source")]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    /// EDID string to match for XR glasses output
    #[serde(default = "default_xr_match")]
    pub xr_match: String,

    /// EDID string to match for primary monitor (empty = auto-detect largest)
    #[serde(default)]
    pub primary_match: String,
}

// Default value functions
fn default_distance() -> f64 { 1.5 }
fn default_scale() -> f64 { 1.0 }
fn default_true() -> bool { true }
fn default_follow_threshold() -> f64 { 15.0 }
fn default_fps() -> u32 { 60 }
fn default_xr_match() -> String { "VITURE".to_string() }
fn default_source() -> String { "monitor".to_string() }

impl Default for Config {
    fn default() -> Self {
        Self {
            display: DisplayConfig::default(),
            capture: CaptureConfig::default(),
            output: OutputConfig::default(),
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            distance: default_distance(),
            scale: default_scale(),
            smooth_follow: true,
            follow_threshold: default_follow_threshold(),
            pin_yaw: 0.0,
            pin_pitch: 0.0,
        }
    }
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            target_fps: default_fps(),
            use_dmabuf: true,
            source: default_source(),
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            xr_match: default_xr_match(),
            primary_match: String::new(),
        }
    }
}

impl Config {
    /// Load config from a specific file path
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Load from default location or return defaults
    pub fn load_or_default() -> Self {
        let config_path = default_config_path();
        if config_path.exists() {
            match Self::from_file(&config_path) {
                Ok(config) => {
                    info!("Loaded config from {}", config_path.display());
                    return config;
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load config from {}: {}. Using defaults.",
                        config_path.display(),
                        e
                    );
                }
            }
        }
        Self::default()
    }

    /// Save config to the default location
    pub fn save_default(&self) -> Result<()> {
        let path = default_config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        info!("Config saved to {}", path.display());
        Ok(())
    }
}

fn default_config_path() -> PathBuf {
    let config_dir = dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"));
    config_dir.join("breezy-cosmic").join("config.toml")
}
