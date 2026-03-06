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
    /// Reference quaternion captured at startup (in raw Madgwick space)
    reference_q: Option<Quat>,
    /// Current smooth-follow anchor orientation (in renderer space)
    smooth_follow_anchor: Quat,
    /// Whether smooth follow anchor has been initialized
    anchor_initialized: bool,
    /// Last frame's orientation (for interpolation)
    last_orientation: Quat,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            reference_q: None,
            smooth_follow_anchor: Quat::IDENTITY,
            anchor_initialized: false,
            last_orientation: Quat::IDENTITY,
        }
    }

    /// Convert a raw Madgwick quaternion to a relative orientation in renderer space.
    ///
    /// Strategy: use RELATIVE orientation to avoid absolute NED↔OpenGL headaches.
    /// 1. Capture first valid quaternion as "reference" (= looking straight ahead)
    /// 2. Compute delta = ref.inverse() * current (change in sensor frame)
    /// 3. Remap delta's rotation axis from sensor frame to renderer frame
    fn sensor_to_renderer(&mut self, raw_q: Quat) -> Quat {
        // Capture reference on first call
        let ref_q = *self.reference_q.get_or_insert(raw_q);

        // Delta in sensor frame: how has orientation changed since reference?
        let delta = ref_q.inverse() * raw_q;

        // Empirically determined axis mapping from user testing:
        //
        // Sensor physical axes (VITURE Luma Ultra IMU chip orientation):
        //   Sensor X = forward (nose direction) — tilt axis
        //   Sensor Y = up (top of head)         — yaw axis
        //   Sensor Z = right (ear direction)    — pitch/nod axis
        //
        // Renderer (OpenGL/wgpu):
        //   Renderer X = right  — pitch axis (vertical movement)
        //   Renderer Y = up     — yaw axis (horizontal movement)
        //   Renderer Z = back   — roll axis (in-place rotation)
        //
        // Mapping with sign corrections (from empirical testing):
        //   renderer_x = -sensor_z  (nod down → display moves up)
        //   renderer_y = -sensor_y  (turn right → display slides left)
        //   renderer_z =  sensor_x  (tilt left → display rotates clockwise)
        Quat::from_xyzw(-delta.z, -delta.y, delta.x, delta.w).normalize()
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
        source_size: Option<(u32, u32)>,
    ) -> Mat4 {
        let raw_q = pose.orientation();
        let head_orientation = self.sensor_to_renderer(raw_q);

        // Debug: log Euler angles in renderer space
        let (yaw, pitch, roll) = {
            let (y, x, z) = head_orientation.to_euler(glam::EulerRot::YXZ);
            (y.to_degrees(), x.to_degrees(), z.to_degrees())
        };
        trace!(
            "Head: yaw={:.1}° pitch={:.1}° roll={:.1}° | raw=({:.3},{:.3},{:.3},{:.3})",
            yaw, pitch, roll,
            raw_q.x, raw_q.y, raw_q.z, raw_q.w,
        );

        // Display placement: offset by pin_yaw/pin_pitch, then placed at distance
        let display_distance = config.distance as f32;
        let pin_yaw_rad = (config.pin_yaw as f32).to_radians();
        let pin_pitch_rad = (config.pin_pitch as f32).to_radians();

        // Pin rotation: rotate the display's world position by the pin angles
        // so it sits at a fixed direction in the user's environment
        let pin_rotation = Quat::from_euler(
            glam::EulerRot::YXZ,
            pin_yaw_rad,    // yaw: left/right
            pin_pitch_rad,  // pitch: up/down
            0.0,            // no roll on the pin
        );
        let display_center = pin_rotation * Vec3::new(0.0, 0.0, -display_distance);

        // Dead zone + non-linear damping: ignore micro-movements, respond to
        // intentional head turns. This prevents involuntary micro-movements
        // (breathing, muscle tremor, etc.) from causing visible display wobble.
        let angle_from_last = self
            .last_orientation
            .inverse()
            .mul_quat(head_orientation)
            .to_axis_angle()
            .1
            .to_degrees();

        let dead_zone_deg = 0.4; // Ignore movements smaller than this
        let filtered = if angle_from_last < dead_zone_deg {
            // Below dead zone: don't move at all
            self.last_orientation
        } else {
            // Scale responsiveness by movement size:
            // small movements (just past dead zone) → heavy smoothing
            // large movements (intentional turns) → snappy response
            let t = ((angle_from_last - dead_zone_deg) / 5.0).clamp(0.05, 0.8);
            self.last_orientation.slerp(head_orientation, t).normalize()
        };

        // Compute effective orientation
        let effective_orientation = if config.smooth_follow {
            self.compute_smooth_follow(filtered, config)
        } else {
            filtered
        };

        self.last_orientation = effective_orientation;

        // View matrix: the head_orientation is a relative rotation from "looking ahead"
        // We want the display to appear pinned in space, so when the head turns right,
        // the display slides left. This means the camera rotation = head rotation.
        // (No inverse needed — the relative quaternion already represents the view change)
        let camera_rotation = effective_orientation;

        // View matrix: rotate world by head orientation
        let view = Mat4::from_quat(camera_rotation);

        // Projection: perspective based on display FOV
        let fov_rad = if pose.display_fov > 0.0 {
            pose.display_fov.to_radians()
        } else {
            46.0f32.to_radians()
        };

        let viewport_aspect = viewport_size.0 as f32 / viewport_size.1 as f32;
        let projection = Mat4::perspective_rh(fov_rad, viewport_aspect, 0.01, 100.0);

        // Use source aspect ratio for the quad shape so content isn't stretched.
        // Falls back to viewport aspect if no source dimensions provided.
        let source_aspect = match source_size {
            Some((sw, sh)) if sw > 0 && sh > 0 => sw as f32 / sh as f32,
            _ => viewport_aspect,
        };

        // Model matrix: place quad at pin offset, oriented to face origin
        let display_scale = display_distance * (fov_rad / 2.0).tan();
        let model = Mat4::from_translation(display_center)
            * Mat4::from_quat(pin_rotation)  // orient quad to face the viewer
            * Mat4::from_scale(Vec3::new(
                display_scale * source_aspect * config.scale as f32,
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
            let follow_speed = 0.05;
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

    /// Reset smooth follow anchor AND reference orientation
    pub fn reset_smooth_follow(&mut self) {
        self.anchor_initialized = false;
        self.reference_q = None;
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
