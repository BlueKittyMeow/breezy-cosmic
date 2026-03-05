//! Wayland output detection and management.
//!
//! Uses wlr-output-management protocol to enumerate connected displays
//! and identify the XR glasses output vs the primary monitor.

use anyhow::{Context, Result};
use tracing::info;

/// Information about a detected Wayland output
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub name: String,
    pub description: String,
    pub make: String,
    pub model: String,
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
    pub scale: f64,
    pub x: i32,
    pub y: i32,
    pub enabled: bool,
}

impl OutputInfo {
    /// Check if this output matches the given EDID search string (case-insensitive)
    pub fn matches_edid(&self, pattern: &str) -> bool {
        if pattern.is_empty() {
            return false;
        }
        let pattern_lower = pattern.to_lowercase();
        self.name.to_lowercase().contains(&pattern_lower)
            || self.description.to_lowercase().contains(&pattern_lower)
            || self.make.to_lowercase().contains(&pattern_lower)
            || self.model.to_lowercase().contains(&pattern_lower)
    }

    /// Total pixel count (for auto-detecting "largest" output)
    pub fn pixel_count(&self) -> i64 {
        self.width as i64 * self.height as i64
    }
}

/// Manages output detection via Wayland protocols
pub struct OutputManager {
    outputs: Vec<OutputInfo>,
}

impl OutputManager {
    /// Create an OutputManager that uses cosmic-randr for output detection
    /// (standalone mode — no Wayland event queue needed)
    pub fn new_standalone() -> Self {
        Self {
            outputs: Vec::new(),
        }
    }

    /// Populate output list from compositor
    ///
    /// This queries the compositor for all connected outputs.
    /// In the SCTK-based implementation, outputs are discovered through
    /// the registry and wl_output events.
    pub fn detect_outputs(&mut self) -> Result<()> {
        // TODO: This will be populated by Wayland event handlers.
        // For initial development, we can also fall back to parsing
        // `cosmic-randr list` output as a bootstrap mechanism.
        info!("Detecting outputs...");
        self.detect_via_cosmic_randr()?;
        Ok(())
    }

    /// Fallback: detect outputs by parsing `cosmic-randr list`
    fn detect_via_cosmic_randr(&mut self) -> Result<()> {
        let output = std::process::Command::new("cosmic-randr")
            .arg("list")
            .output()
            .context("Failed to run cosmic-randr. Is COSMIC installed?")?;

        if !output.status.success() {
            anyhow::bail!(
                "cosmic-randr failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        self.outputs = parse_cosmic_randr_output(&stdout);

        info!("Detected {} output(s)", self.outputs.len());
        for out in &self.outputs {
            info!(
                "  {} — {} {} ({}x{}@{:.1}Hz) {}",
                out.name,
                out.make,
                out.model,
                out.width,
                out.height,
                out.refresh_mhz as f64 / 1000.0,
                if out.enabled { "enabled" } else { "disabled" }
            );
        }

        Ok(())
    }

    /// Print all detected outputs (for --list-outputs mode)
    pub fn list_outputs(&mut self) {
        if self.outputs.is_empty() {
            let _ = self.detect_outputs();
        }

        println!("Detected outputs:");
        println!("{:-<70}", "");
        for (i, out) in self.outputs.iter().enumerate() {
            println!(
                "  [{}] {} — {} {}",
                i, out.name, out.make, out.model
            );
            println!(
                "      Resolution: {}x{} @ {:.1}Hz",
                out.width,
                out.height,
                out.refresh_mhz as f64 / 1000.0
            );
            println!(
                "      Position: ({}, {})  Scale: {:.1}x  {}",
                out.x,
                out.y,
                out.scale,
                if out.enabled { "ENABLED" } else { "disabled" }
            );
            println!();
        }
    }

    /// Find the primary monitor and XR glasses output
    pub fn find_outputs(
        &mut self,
        xr_match: &str,
        primary_match: &str,
    ) -> Result<(OutputInfo, OutputInfo)> {
        if self.outputs.is_empty() {
            self.detect_outputs()?;
        }

        // Find XR output
        let xr = self
            .outputs
            .iter()
            .find(|o| o.enabled && o.matches_edid(xr_match))
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No XR output found matching '{}'. Use --list-outputs to see available displays.\n\
                     Make sure your XR glasses are connected and detected as a display.",
                    xr_match
                )
            })?;

        // Find primary output (either by match string or auto-detect largest)
        let primary = if !primary_match.is_empty() {
            self.outputs
                .iter()
                .find(|o| o.enabled && o.matches_edid(primary_match) && o.name != xr.name)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("No primary output found matching '{}'", primary_match)
                })?
        } else {
            // Auto-detect: pick the largest enabled output that isn't the XR glasses
            self.outputs
                .iter()
                .filter(|o| o.enabled && o.name != xr.name)
                .max_by_key(|o| o.pixel_count())
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No primary output found. You need at least two displays \
                         (your monitor + the XR glasses)."
                    )
                })?
        };

        Ok((primary, xr))
    }
}

/// Strip ANSI escape sequences from text
///
/// cosmic-randr outputs ANSI color codes (e.g. `\x1b[1meDP-1\x1b[0m`)
/// which break our text parsing. This removes all SGR sequences.
fn strip_ansi(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip ESC [ ... m sequences (SGR)
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // Consume until we hit a letter (the command byte)
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Parse the text output of `cosmic-randr list` into OutputInfo structs
fn parse_cosmic_randr_output(text: &str) -> Vec<OutputInfo> {
    // Strip ANSI color codes before parsing
    let clean = strip_ansi(text);

    let mut outputs = Vec::new();
    let mut current: Option<OutputInfo> = None;

    for line in clean.lines() {
        let trimmed = line.trim();

        // New output block starts with a non-indented line containing the output name
        if !line.starts_with(' ') && !trimmed.is_empty() {
            // Save previous output
            if let Some(out) = current.take() {
                outputs.push(out);
            }

            // Parse output name — after ANSI stripping, format is typically:
            //   "eDP-1 (enabled)" or "DP-1 (disabled)" or just "eDP-1:"
            let name_part = trimmed
                .split_whitespace()
                .next()
                .unwrap_or(trimmed)
                .trim_end_matches(':');

            // Check enabled/disabled from the header line
            let enabled = !trimmed.contains("disabled");

            current = Some(OutputInfo {
                name: name_part.to_string(),
                description: String::new(),
                make: String::new(),
                model: String::new(),
                width: 0,
                height: 0,
                refresh_mhz: 0,
                scale: 1.0,
                x: 0,
                y: 0,
                enabled,
            });
        } else if let Some(ref mut out) = current {
            // Parse indented properties
            if let Some(val) = trimmed.strip_prefix("Make: ") {
                out.make = val.to_string();
            } else if let Some(val) = trimmed.strip_prefix("Model: ") {
                out.model = val.to_string();
            } else if let Some(val) = trimmed.strip_prefix("Description: ") {
                out.description = val.to_string();
            } else if let Some(val) = trimmed.strip_prefix("Scale: ") {
                out.scale = val.parse().unwrap_or(1.0);
            } else if let Some(val) = trimmed.strip_prefix("Position: ") {
                // Format: "x, y" or "(x, y)"
                let cleaned = val.trim_matches(|c| c == '(' || c == ')');
                let parts: Vec<&str> = cleaned.split(',').collect();
                if parts.len() == 2 {
                    out.x = parts[0].trim().parse().unwrap_or(0);
                    out.y = parts[1].trim().parse().unwrap_or(0);
                }
            } else if trimmed.contains('x') && trimmed.contains("Hz") {
                // Resolution line like "1920x1080 @ 60.056 Hz (current) (preferred)"
                // Only use the "current" mode's resolution
                let is_current = trimmed.contains("current");

                // Parse resolution and refresh rate
                if let Some(res_part) = trimmed.split_whitespace().next() {
                    let dims: Vec<&str> = res_part.split('x').collect();
                    if dims.len() == 2 {
                        let w: i32 = dims[0].parse().unwrap_or(0);
                        let h: i32 = dims[1].parse().unwrap_or(0);
                        // Only update if this is the current mode, or if we haven't
                        // found any resolution yet (fallback to first listed mode)
                        if is_current || out.width == 0 {
                            out.width = w;
                            out.height = h;
                        }
                    }
                }
                // Extract refresh rate
                if is_current || out.refresh_mhz == 0 {
                    if let Some(hz_idx) = trimmed.find("Hz") {
                        let before_hz = &trimmed[..hz_idx];
                        if let Some(at_idx) = before_hz.rfind("@ ") {
                            let rate_str = before_hz[at_idx + 2..].trim();
                            if let Ok(rate) = rate_str.parse::<f64>() {
                                out.refresh_mhz = (rate * 1000.0) as i32;
                            }
                        }
                    }
                }
            } else if trimmed == "Disabled" || trimmed.contains("disabled") {
                out.enabled = false;
            }
        }
    }

    // Don't forget the last output
    if let Some(out) = current {
        outputs.push(out);
    }

    outputs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\x1b[1meDP-1\x1b[0m"), "eDP-1");
        assert_eq!(strip_ansi("\x1b[1;32m(enabled)\x1b[0m"), "(enabled)");
        assert_eq!(strip_ansi("no ansi here"), "no ansi here");
        assert_eq!(
            strip_ansi("\x1b[35m1920x1080\x1b[0m @ \x1b[36m 60.056 Hz\x1b[0m"),
            "1920x1080 @  60.056 Hz"
        );
    }

    #[test]
    fn test_parse_cosmic_randr_with_ansi() {
        // Real cosmic-randr output with ANSI codes (preserving indentation)
        let input = concat!(
            "\x1b[1meDP-1\x1b[0m \x1b[1;32m(enabled)\x1b[0m\x1b[1;33m\n",
            "\x1b[1;33m  Make: \x1b[0mAU Optronics\x1b[1;33m\n",
            "\x1b[1;33m  Model: \x1b[0m0xF992\x1b[1;33m\n",
            "\x1b[1;33m  Scale: \x1b[0m1\x1b[1;33m\n",
            "\x1b[1;33m  Position: \x1b[0m0, 0\x1b[1;33m\n",
            "\x1b[1;33m  Modes:\x1b[0m\n",
            "    \x1b[35m1920x1080\x1b[0m @ \x1b[36m 60.056 Hz\x1b[0m\x1b[1;35m (current)\x1b[0m\x1b[1;32m (preferred)\x1b[0m\n",
            "    \x1b[35m1920x1080\x1b[0m @ \x1b[36m 48.045 Hz\x1b[0m\n",
        );

        let outputs = parse_cosmic_randr_output(input);
        assert_eq!(outputs.len(), 1);

        let out = &outputs[0];
        assert_eq!(out.name, "eDP-1");
        assert_eq!(out.make, "AU Optronics");
        assert_eq!(out.model, "0xF992");
        assert_eq!(out.width, 1920);
        assert_eq!(out.height, 1080);
        assert!(out.refresh_mhz > 59000 && out.refresh_mhz < 61000);
        assert!(out.enabled);
        assert_eq!(out.scale, 1.0);
    }

    #[test]
    fn test_parse_multiple_outputs() {
        let input = "eDP-1 (enabled)\n\
                      \x20\x20Make: AU Optronics\n\
                      \x20\x20Model: 0xF992\n\
                      \x20\x20Scale: 1.25\n\
                      \x20\x20Position: 0, 0\n\
                      \x20\x20Modes:\n\
                      \x20\x20\x20\x201920x1080 @ 60.000 Hz (current) (preferred)\n\
                      DP-3 (enabled)\n\
                      \x20\x20Make: VITURE\n\
                      \x20\x20Model: Luma Pro\n\
                      \x20\x20Scale: 1\n\
                      \x20\x20Position: 1920, 0\n\
                      \x20\x20Modes:\n\
                      \x20\x20\x20\x201920x1080 @ 60.000 Hz (current) (preferred)\n";

        let outputs = parse_cosmic_randr_output(input);
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].name, "eDP-1");
        assert_eq!(outputs[0].scale, 1.25);
        assert_eq!(outputs[1].name, "DP-3");
        assert_eq!(outputs[1].make, "VITURE");
        assert_eq!(outputs[1].model, "Luma Pro");
    }
}
