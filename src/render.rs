//! GPU-accelerated rendering with head-tracking transforms.
//!
//! Takes captured frames from the primary monitor and renders them
//! as a textured quad with quaternion-based view transforms derived
//! from the XR glasses' IMU data.

use glam::{Mat4, Quat, Vec3};
use tracing::trace;

use crate::capture::CapturedFrame;
use crate::config::DisplayConfig;
use crate::pose::PoseData;

/// Manages the rendering pipeline for head-tracked display
pub struct Renderer {
    /// Current smooth-follow anchor orientation
    smooth_follow_anchor: Quat,
    /// Whether smooth follow anchor has been initialized
    anchor_initialized: bool,
    /// Last frame's orientation (for interpolation)
    last_orientation: Quat,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            smooth_follow_anchor: Quat::IDENTITY,
            anchor_initialized: false,
            last_orientation: Quat::IDENTITY,
        }
    }

    /// Compute the view-projection matrix for the current pose
    ///
    /// This transforms the virtual display quad based on head orientation:
    /// - The display is placed at a fixed distance in front of the user
    /// - Head rotation moves the viewport (the display appears to stay in place)
    /// - In smooth-follow mode, the display gently drifts toward gaze direction
    pub fn compute_view_matrix(
        &mut self,
        pose: &PoseData,
        config: &DisplayConfig,
        viewport_size: (u32, u32),
    ) -> Mat4 {
        let head_orientation = pose.orientation();
        let head_position = Vec3::from(pose.position_eus());

        // Display placement: centered at (distance) meters in front of user
        let display_distance = config.distance as f32;
        let display_center = Vec3::new(0.0, 0.0, -display_distance);

        // Compute effective orientation
        let effective_orientation = if config.smooth_follow {
            self.compute_smooth_follow(head_orientation, config)
        } else {
            head_orientation
        };

        self.last_orientation = effective_orientation;

        // Build the view matrix:
        // 1. The virtual display is a quad at display_center in world space
        // 2. The camera (user's head) is at origin, looking along -Z
        // 3. Head rotation rotates the camera, making the display appear to move
        //
        // For head-locked rendering (display moves with head), we'd use identity.
        // For world-locked (display stays in place), we use the inverse of head rotation.

        let camera_rotation = effective_orientation.inverse();

        // View matrix: rotate world by inverse of head orientation
        let view = Mat4::from_quat(camera_rotation)
            * Mat4::from_translation(-head_position);

        // Projection: simple perspective based on display FOV
        let fov_rad = if pose.display_fov > 0.0 {
            pose.display_fov.to_radians()
        } else {
            46.0f32.to_radians() // Default ~46° FOV for Viture Luma Pro
        };

        let aspect = viewport_size.0 as f32 / viewport_size.1 as f32;
        let projection = Mat4::perspective_rh(fov_rad, aspect, 0.01, 100.0);

        // Model matrix: scale quad to fill the virtual display area
        let display_scale = display_distance * (fov_rad / 2.0).tan();
        let model = Mat4::from_translation(display_center)
            * Mat4::from_scale(Vec3::new(
                display_scale * aspect * config.scale as f32,
                display_scale * config.scale as f32,
                1.0,
            ));

        projection * view * model
    }

    /// Smooth follow: the display gently drifts toward gaze direction
    /// instead of rigidly staying in world space
    fn compute_smooth_follow(
        &mut self,
        head_orientation: Quat,
        config: &DisplayConfig,
    ) -> Quat {
        if !self.anchor_initialized {
            self.smooth_follow_anchor = head_orientation;
            self.anchor_initialized = true;
            return head_orientation;
        }

        // Calculate angular difference between anchor and current head orientation
        let relative = self.smooth_follow_anchor.inverse() * head_orientation;
        let angle_deg = relative.to_axis_angle().1.to_degrees();

        if angle_deg > config.follow_threshold as f32 {
            // Head has moved beyond threshold — start moving the anchor
            // Use SLERP to smoothly interpolate the anchor toward current gaze
            let follow_speed = 0.05; // How fast the display follows (0 = locked, 1 = instant)
            let overshoot = (angle_deg - config.follow_threshold as f32) / angle_deg;
            let t = (follow_speed * overshoot).min(0.3);

            self.smooth_follow_anchor = self
                .smooth_follow_anchor
                .slerp(head_orientation, t)
                .normalize();

            trace!(
                "Smooth follow: angle={:.1}° threshold={:.1}° t={:.3}",
                angle_deg,
                config.follow_threshold,
                t
            );
        }

        self.smooth_follow_anchor
    }

    /// Reset smooth follow anchor to current head position
    pub fn reset_smooth_follow(&mut self) {
        self.anchor_initialized = false;
    }

    /// Generate vertices for a textured quad (the virtual display surface)
    ///
    /// Returns (vertices, indices) for a unit quad centered at origin.
    /// The view-projection matrix will position and orient it.
    pub fn quad_vertices() -> (Vec<QuadVertex>, Vec<u16>) {
        let vertices = vec![
            // Position          // UV
            QuadVertex { position: [-1.0, -1.0, 0.0], uv: [0.0, 1.0] }, // bottom-left
            QuadVertex { position: [ 1.0, -1.0, 0.0], uv: [1.0, 1.0] }, // bottom-right
            QuadVertex { position: [ 1.0,  1.0, 0.0], uv: [1.0, 0.0] }, // top-right
            QuadVertex { position: [-1.0,  1.0, 0.0], uv: [0.0, 0.0] }, // top-left
        ];

        let indices = vec![0, 1, 2, 0, 2, 3];

        (vertices, indices)
    }
}

/// Vertex format for the display quad
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct QuadVertex {
    pub position: [f32; 3],
    pub uv: [f32; 2],
}

/// WGSL shader for rendering the captured frame with a view-projection transform
pub const SHADER_SOURCE: &str = r#"
struct Uniforms {
    mvp: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(1) @binding(0) var frame_texture: texture_2d<f32>;
@group(1) @binding(1) var frame_sampler: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = uniforms.mvp * vec4<f32>(in.position, 1.0);
    out.tex_coord = in.uv;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(frame_texture, frame_sampler, in.tex_coord);
}
"#;

/// Uniform buffer layout matching the WGSL shader
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Uniforms {
    pub mvp: [[f32; 4]; 4],
}

impl Uniforms {
    pub fn from_mat4(mat: &Mat4) -> Self {
        Self {
            mvp: mat.to_cols_array_2d(),
        }
    }
}
