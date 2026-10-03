//! wgpu renderer.

pub mod atmosphere;
pub mod clouds;
mod puddles;
mod triple;
pub use triple::{panel_width, ScreenView, TripleScreen};

#[derive(Default)]
struct ViewCulling {
    drawn: Vec<u64>,
    sizes: hashbrown::HashMap<[u64; 4], f32>,
    scratch: hashbrown::HashMap<[u64; 4], f32>,
}

use anyhow::{anyhow, Context, Result};
use glam::{DVec3, Mat4, Vec3, Vec4};
use omsi_geometry::MeshData;
use std::collections::HashMap;
use std::sync::Arc;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CameraUniform {
    view_proj: [[f32; 4]; 4],
    cam_pos: [f32; 4],
    /// World-space render origin modulo 1000 m (the procedural patterns' period), added back
    /// for procedural patterns that must not jump when the floating origin moves to the next
    /// 100 m cell.
    world_origin: [f32; 4],
    sun_dir: [f32; 4],
    ambient: [f32; 4],
    fog: [f32; 4],
    sun_color: [f32; 4],
    sky_color: [f32; 4],
    light_grid: [f32; 4],
    sky: [f32; 4],
    cam_right: [f32; 4],
    cam_up: [f32; 4],
    clouds: [f32; 4],
    light_view_proj: [[f32; 4]; 4],
    light_view_proj_far: [[f32; 4]; 4],
    shadow: [f32; 4],
    /// x enhanced graphics (0/1), y seconds since start, z sun ndc x, w sun ndc y
    post: [f32; 4],
    /// The player's vehicle as a box the weather does not reach into: origin (render-origin
    /// relative) and sin(heading); cos(heading) and the half extents; the box centre offset
    /// and whether there is one.
    inside_a: [f32; 4],
    inside_b: [f32; 4],
    inside_c: [f32; 4],
    /// x procedural detail texturing (0/1), y soft shadow penumbra (0/1), z aerial
    /// perspective strength, w the close shadow cascade's half range
    flags: [f32; 4],
    /// The close shadow cascade (right half of the near map).
    light_view_proj_close: [[f32; 4]; 4],
    /// The player's vehicle's velocity (m/s, world) and 1: the airstream the rain on its
    /// glass meets (see `Lighting::glass_wind`).
    wind: [f32; 4],
}

/// The period the sky's cloud patterns repeat with (m): 5 x the cloud field (14 km), 8 x
/// its billow detail (8.75 km) and 28 x the high layer's (2.5 km) - the render origin is
/// taken modulo this for them, which keeps centimetres of precision in 32 bits.
const CLOUD_ORIGIN_PERIOD: f64 = 70000.0;

/// The enhanced path's post passes (post.wgsl `PostParams`).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PostUniform {
    a: [f32; 4],
    b: [f32; 4],
    c: [f32; 4],
}

/// The enhanced lighting (enhanced_common.wgsl `Enhanced`).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct EnhancedUniform {
    exposure: [f32; 4],
    sun: [f32; 4],
    sh: [[f32; 4]; 9],
    ground: [f32; 4],
    fog: [f32; 4],
    fog_color: [f32; 4],
    weather: [f32; 4],
    lights: [f32; 4],
    sun_disc: [f32; 4],
    debug: [f32; 4],
    /// xyz where the sky cube was drawn from, relative to the camera (the dome looks the
    /// clouds up through it with the parallax taken out)
    eye: [f32; 4],
    /// x how bright an LED panel's dots burn (`Lighting::led_glow`), y how much of the
    /// mip chain an LED panel is held at (`Lighting::led_mips`)
    led: [f32; 4],
}

/// High-range colour targets of the enhanced path for one size: the multisampled one the
/// scene is drawn into, the single-sampled one it resolves into, the glow's levels, the
/// tone-mapped picture FXAA reads, and the bind groups of the passes between them.
struct HdrTargets {
    msaa_view: Option<wgpu::TextureView>,
    view: wgpu::TextureView,
    /// The screen mask (`MASK_FORMAT`), multisampled and resolved like the picture.
    mask_msaa: Option<wgpu::TextureView>,
    mask: wgpu::TextureView,
    /// Glow levels at 1/2, 1/4, ... of the size, and the upsampled sums per level.
    down: Vec<wgpu::TextureView>,
    up: Vec<wgpu::TextureView>,
    ldr: wgpu::TextureView,
    /// down[i] reads the picture (i = 0) or down[i - 1]; up[i] reads up[i + 1] (or the
    /// last down level) and down[i].
    down_bg: Vec<wgpu::BindGroup>,
    up_bg: Vec<wgpu::BindGroup>,
    meter_bg: wgpu::BindGroup,
    /// Tone mapping with the adapted exposure in `adapt[k]`.
    tonemap_bg: [wgpu::BindGroup; 2],
    fxaa_bg: wgpu::BindGroup,
    /// Classic shading is presented directly, without the Enhanced tone curve.
    classic_bg: wgpu::BindGroup,
    /// Allocated only when wet roads need scene reflections in the main view.
    puddles: Option<puddles::Targets>,
}

/// The enhanced path's reflection probe: a cube map of the sky around the camera with a
/// GGX-blurred mip chain, drawn now and then.
struct Probe {
    view: wgpu::TextureView,
    /// Per mip level, per face: the view the level is drawn into.
    faces: Vec<Vec<wgpu::TextureView>>,
    /// Per mip level, the two passes' bind groups (faces 0-2 and 3-5).
    bind_groups: Vec<[wgpu::BindGroup; 2]>,
    sky_pipeline: wgpu::RenderPipeline,
    filter_pipeline: wgpu::RenderPipeline,
    /// Frames since it was drawn, and the sky table scale it was drawn with.
    age: u32,
    scale: f32,
    /// The enhanced sky itself at `SKY_CUBE_SIZE` a face (clouds and all), drawn a face a
    /// frame; the dome reads it (camera binding 17). The volumetric clouds cost 6-18 ms a
    /// frame drawn for every pixel of a 1440p sky; a face a frame is well under one.
    cube_view: wgpu::TextureView,
    cube_faces: Vec<wgpu::TextureView>,
    cube_bind_groups: Vec<wgpu::BindGroup>,
    cube_pipeline: wgpu::RenderPipeline,
    cube_next: u32,
    cube_filled: bool,
    /// Redraws so far (picks the round of the next).
    cube_round: u32,
    /// Window frames since the last face was redrawn (see SKY_CUBE_EVERY).
    cube_wait: u32,
    /// Where every face of the cube is drawn from (world). A face drawn from wherever the
    /// camera stood at the time left six faces from six places: flying up towards the
    /// clouds, the faces disagreed at their seams and the blended history dragged each one
    /// behind the camera, so the clouds shook and lagged. The cube keeps one eye; the dome
    /// takes the parallax out, and the eye moves on (the whole cube redrawn) once the
    /// camera has gone far enough for the correction to show.
    cube_eye: Option<DVec3>,
    cube_recapture: bool,
}

/// Face size of the enhanced sky cube (see `Probe::cube_view`): about as many texels per
/// degree as a 1600-pixel-wide picture has pixels at half its size (at 512 the clouds'
/// edges stood in blocks of three or four pixels).
const SKY_CUBE_SIZE: u32 = 1024;
/// Redraw rounds of a sky cube face: each starts the clouds' steps elsewhere, and the
/// rounds are averaged (all of them at once for a picture on its own or a new sky).
const SKY_CUBE_ROUNDS: u32 = 8;
/// The old picture's share when a face is redrawn in the window.
const SKY_CUBE_HISTORY: f64 = 0.8;
/// A face is redrawn every this many window frames: the clouds drift by a pixel of the
/// cube in seconds, and a face a frame (a 32-step march of 1024x1024 texels) cost 2.9 ms of
/// every enhanced frame on an M4.
const SKY_CUBE_EVERY: u32 = 4;

const PROBE_SIZE: u32 = 64;
const PROBE_MIPS: u32 = 6;
/// Glow levels of the enhanced post path.
const GLOW_LEVELS: usize = 6;
/// The illuminance a light's core gives (maplight colour 1, in the sky model's units:
/// 55 lux, so the street under a lamp gets its 15-25 lux).
const LAMP_E: f32 = 0.0055;
/// Illuminance of a bus saloon's lamps on the seats and the floor (300 lux).
const CABIN_E: f32 = 0.03;
/// A lit window's radiance at night.
const WINDOW_RADIANCE: f32 = 0.0022;
/// The metering (see `meter_tuning`).
const METER_GAIN: f32 = 0.4;
const METER_TARGET: f32 = -2.84;
const METER_DARKEN: f32 = 0.6;
const METER_BRIGHTEN: f32 = 0.8;
/// How far night vision takes the colour out of a dark scene (post.wgsl `night_vision`).
const NIGHT_VISION: f32 = 0.55;
/// The sun's angular radius as drawn (a little larger than the real 0.27°).
const SUN_RADIUS: f32 = 0.0065;

/// The textures of the ambient-occlusion pass for one target size.
struct AoTargets {
    size: (u32, u32),
    depth_view: wgpu::TextureView,
    ao_view: wgpu::TextureView,
    blur_view: wgpu::TextureView,
    ssao_bg: wgpu::BindGroup,
    blur_bg: wgpu::BindGroup,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SsaoUniform {
    inv_proj: [[f32; 4]; 4],
    params: [f32; 4],
    /// xy: the projection's off-centre shift (0 for the window's own symmetric frustum;
    /// a headset eye or a triple screen's side panel has one)
    shift: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuPointLight {
    pos: [f32; 4],
    color: [f32; 4],
    /// Spot direction and cosine of the outer cone (-2 = a point light).
    dir: [f32; 4],
    /// Cosine of the inner cone, the core radius, the beam's gain towards its cut-off, the
    /// radius (`pos.w` is 0 on a light only the enhanced path draws, which the vanilla
    /// shader then passes by).
    extra: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuCorona {
    pos: [f32; 3],
    size: f32,
    color: [f32; 4],
    dir: [f32; 4],
    /// xyz: the up axis; w: rotating mode
    up: [f32; 4],
    /// x: inner cone cosine, y: z offset, z: flags
    extra: [f32; 4],
}

/// A point light in world space (`[maplight]`, `[interiorlight]`, headlights).
#[derive(Debug, Clone, Copy)]
pub struct PointLight {
    pub position: DVec3,
    pub radius: f32,
    pub color: [f32; 3],
    pub intensity: f32,
    /// A spot light's direction (zero = a point light) and the cosines of its inner and
    /// outer cone (enhanced path).
    pub direction: Vec3,
    pub cone: [f32; 2],
    /// The radius within which the light is at full strength (`[maplight]`'s); 0 = an
    /// eighth of `radius` (enhanced path; vanilla always takes the eighth).
    pub core: f32,
    /// A headlight's beam (enhanced path): up to this many times stronger towards the
    /// horizon than along its axis, so that the road far ahead is lit as a low beam lights
    /// it rather than only the pool in front of the bumper (0 = an even cone).
    pub beam: f32,
    /// Which path draws the light.
    pub mode: LightMode,
}

impl Default for PointLight {
    fn default() -> Self {
        Self {
            position: DVec3::ZERO,
            radius: 0.0,
            color: [1.0; 3],
            intensity: 1.0,
            direction: Vec3::ZERO,
            cone: [1.0, 0.0],
            core: 0.0,
            beam: 0.0,
            mode: LightMode::Both,
        }
    }
}

/// Which renderer a light belongs to: a vehicle's headlight is three point lights along
/// its axis for the vanilla path and one real spot light for the enhanced one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LightMode {
    #[default]
    Both,
    Vanilla,
    Enhanced,
}

/// One particle of a `[smoke]` system (exhaust, boiling coolant, wheel spray, chimneys).
#[derive(Debug, Clone, Copy)]
pub struct SmokeParticle {
    pub position: DVec3,
    /// Half its width (m).
    pub size: f32,
    pub color: [f32; 3],
    pub alpha: f32,
}

/// The light map atlas: tiles a side and pixels a tile.
pub const LM_ATLAS_TILES: u32 = 5;
pub const LM_TILE_PX: u32 = 256;

/// A light corona sprite (`[light_enh]`, `[light_enh_2]`).
#[derive(Debug, Clone, Copy)]
pub struct Corona {
    pub position: DVec3,
    pub size: f32,
    pub color: [f32; 3],
    /// 0..2 (a `[light_enh_2]` fading variable of 2 is double brightness).
    pub brightness: f32,
    /// Facing direction (zero = omnidirectional) and cosine of the visibility half-cone
    /// (the outer cone: where it begins to be seen).
    pub direction: Vec3,
    pub cone_cos: f32,
    /// Cosine of the inner cone (full brightness); below `cone_cos` = no inner cone.
    pub inner_cos: f32,
    /// `[light_enh_2]` rotating: 0 a flat sprite facing `direction`, 1 turned to the viewer
    /// about `up`, 2 turned to the viewer about every axis (a billboard).
    pub rotating: u8,
    pub up: Vec3,
    /// How far the spot is moved from its place towards the viewer (m), so that a lamp
    /// inside its housing still shows; negative = the old default (half its size, at most
    /// half a metre).
    pub z_offset: f32,
    /// `[light_enh_2]` parameter bits: 1 star, 2 no fog, 4 only effects.
    pub flags: u8,
    /// Its picture: 0 the standard glow, else one registered with
    /// [`Renderer::set_corona_texture`] (a light's own `bitmap`, the fog cone's picture).
    pub texture: u16,
    /// A light's cone in the fog rather than its glow (OMSI's `light_cone.bmp` fan, see
    /// corona.wgsl): `size` is the fan's radius, `cone_cos` and `inner_cos` hold the outer
    /// and inner half angles (radians), `beam_width` the fog's visibility (m).
    pub beam: bool,
    pub beam_width: f32,
    /// The halo round a light in fog: a billboard of `size`, pulled
    /// towards the viewer, seen from in front of the light; the angles and the visibility
    /// travel as for a cone.
    pub halo: bool,
}

impl Default for Corona {
    fn default() -> Self {
        Corona {
            position: DVec3::ZERO,
            size: 0.1,
            color: [1.0; 3],
            brightness: 0.0,
            direction: Vec3::ZERO,
            cone_cos: -1.0,
            inner_cos: -2.0,
            rotating: 2,
            up: Vec3::Z,
            z_offset: -1.0,
            flags: 0,
            texture: 0,
            beam: false,
            beam_width: 0.0,
            halo: false,
        }
    }
}

const LIGHT_CELL: f32 = 25.0;
const LIGHT_GRID_SIDE: usize = 64;
/// (32: a depot or a bus interior with many lamps lost the farthest past 16 in a cell)
const LIGHT_CELL_CAP: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaterialUniform {
    color: [f32; 4],
    params: [f32; 4],
    extra: [f32; 4],
    params2: [f32; 4],
    /// rgb: emissive colour; w: an explicitly identified transparent glass layer
    emissive: [f32; 4],
    specular: [f32; 4],
    /// x: `[matl_bumpmap]` factor, y: has a bump map, z/w: noZwrite/noZcheck
    bump: [f32; 4],
    /// The PBR maps beside the diffuse texture (`Scene::pbr_maps`): x has a normal map,
    /// y an occlusion, z a roughness, w a metalness channel.
    pbr: [f32; 4],
    /// x: a screen (`MaterialExtra::screen`); y: 1 `[matl_texadress_border]`, 2
    /// `[matl_texadress_mirroronce]`; z the border colour's rgb packed as r * 65536 + g * 256 + b (bytes), w its alpha.
    flags: [f32; 4],
    /// rgb: the D3D material's ambient colour, which takes the ambient light (C); w: 1 for
    /// a texture that is a season's snow picture (no snow laid over it), 2 the map's water
    ambient: [f32; 4],
}

/// The maps of a PBR set found beside a diffuse texture (`foo_n.png` and the rest, see
/// `omsi_texture::pbr`): a tangent-space normal map, and occlusion / roughness / metalness
/// packed into the red, green and blue of one texture.
#[derive(Debug, Clone, Copy)]
pub struct PbrMaps {
    pub normal: Option<TextureId>,
    pub orm: Option<TextureId>,
    /// x normal, y occlusion, z roughness, w metalness (1 = present)
    pub flags: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlphaMode {
    Opaque,
    Test,
    Blend,
}

/// How a material's textures read outside [0, 1]: Omsi.exe sets the material's
/// `[matl_texadress_*]` mode as ADDRESSU/ADDRESSV of all eight sampler stages (0x7fff70).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TexAddressing {
    /// Repeating, Direct3D's default.
    #[default]
    Wrap,
    /// `[matl_texadress_mirror]`: every other repeat mirrored.
    Mirror,
    /// `[matl_texadress_clamp]`, and `[matl_texadress_border]` (whose colour the shader
    /// puts outside, `MaterialExtra::border`).
    Clamp,
    /// `[matl_texadress_mirroronce]`: mirrored once about 0, then clamped (the shader takes
    /// the coordinates' absolute value under the clamping sampler).
    MirrorOnce,
}

#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// World position (f64: maps span millions of metres).
    pub position: DVec3,
    /// Degrees, 0 = looking north (+y), clockwise positive (OMSI heading).
    pub yaw: f32,
    /// Degrees, positive = looking up.
    pub pitch: f32,
    /// Degrees about the view direction (0 = the horizon level; see [`Camera::up`]). A
    /// camera fixed to a vehicle - a mirror's - leans with its body.
    pub roll: f32,
    pub fov_deg: f32,
    pub near: f32,
    pub far: f32,
}

impl Camera {
    pub fn forward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.to_radians().sin_cos();
        let (sp, cp) = self.pitch.to_radians().sin_cos();
        Vec3::new(sy * cp, cy * cp, sp)
    }
    pub fn right(&self) -> Vec3 {
        let f = self.forward();
        let r0 = Vec3::new(f.y, -f.x, 0.0).normalize_or_zero();
        if self.roll == 0.0 {
            return r0;
        }
        f.cross(self.up()).normalize_or(r0)
    }
    /// The picture's up: world up for a level camera, turned about the view direction by
    /// `roll` (positive: the top leans to the right).
    pub fn up(&self) -> Vec3 {
        let f = self.forward();
        let r0 = Vec3::new(f.y, -f.x, 0.0).normalize_or_zero();
        if self.roll == 0.0 || r0 == Vec3::ZERO {
            return Vec3::Z;
        }
        let u0 = r0.cross(f);
        let (s, c) = self.roll.to_radians().sin_cos();
        (u0 * c + r0 * s).normalize_or(Vec3::Z)
    }
    /// View-projection relative to a render origin (the camera itself when `origin` is its
    /// position), so that GPU maths stays in small numbers.
    pub fn view_proj(&self, aspect: f32, origin: DVec3) -> Mat4 {
        let view = Mat4::look_to_rh((self.position - origin).as_vec3(), self.forward(), self.up());
        // Reversed Z (near and far swapped): the depth buffer then spends its float
        // precision where the scene is far away instead of where it is close, which is what
        // stops distant roads, kerbs and painted ground from flickering against each other
        // - with a plain 0..1 depth the resolution at a kilometre is a good quarter of a
        // metre, less than the gap between a road surface and the ground under it.
        let proj = Mat4::perspective_rh(self.fov_deg.to_radians(), aspect, self.far, self.near);
        proj * view
    }

    /// Ray through a normalized device coordinate, relative to `origin`.
    pub fn ray(&self, ndc_x: f32, ndc_y: f32, aspect: f32, origin: DVec3) -> (Vec3, Vec3) {
        let inv = self.view_proj(aspect, origin).inverse();
        // ndc z = 0 is the far plane with reversed Z: the longest baseline for the ray
        let p = inv.project_point3(Vec3::new(ndc_x, ndc_y, 0.0));
        let o = (self.position - origin).as_vec3();
        (o, (p - o).normalize_or_zero())
    }
}

#[derive(Clone, Debug)]
pub struct Lighting {
    /// Objects smaller on the screen than this are not drawn: the original's
    /// `performance_minObjSize`, in its own measure (the object's diameter over its distance,
    /// as a share of the vertical field of view; see `render_inner`), times the object's
    /// `[detail_factor]`. OMSI's presets say 0.013 (0.020 for the slowest machines). The
    /// renderer's `RenderOptions::min_obj_size` is the floor; a picture may ask for more
    /// (the mirrors take the original's `performance_minObjSizeRefl`).
    pub min_obj_size: f32,
    pub sun_dir: Vec3,
    pub sun_intensity: f32,
    /// Direct sun colour (envir light A).
    pub sun_color: Vec3,
    /// Light from above (envir light B).
    pub secondary: Vec3,
    /// Undirected light (envir light C).
    pub ambient: Vec3,
    pub fog_color: Vec3,
    pub fog_density: f32,
    pub sky_color: Vec3,
    /// 0 at day, 1 at night: strength of nightmaps and coronas.
    pub night: f32,
    /// The night maps (`[matl_nightmap]`, the tiles' light maps) switched as Omsi.exe
    /// switches them - on with the lamps, not faded in with the dusk (its stage is set when
    /// the object's `NightlightA` is over 0.5, 0x61197a/0x7fee02); None: by `night`.
    pub night_maps: Option<f32>,
    /// Sun azimuth (radians, clockwise from north) and day/twilight/night sky texture weights.
    pub sun_azimuth: f32,
    pub sky_weights: [f32; 3],
    /// Cloud layer: density 0..1 and the texture offset (wind drift), 0 = no clouds.
    pub cloud_density: f32,
    pub cloud_offset: [f32; 2],
    /// Sun shadow map (off in mirrors and at night).
    pub shadows: bool,
    /// How wet the roads are (0..1): rain darkens them and makes them mirror the sky.
    pub wetness: f32,
    /// Snow cover on the ground and the roads (0..1).
    pub snow: f32,
    /// Enhanced graphics: the physically based high-range renderer (enhanced.wgsl) with its
    /// computed sky, automatic exposure, glow and tone mapping (post.wgsl).
    pub enhanced: bool,
    /// Vanilla graphics - the picture as OMSI 2 draws it: none of the extras the
    /// rewrite's own vanilla renderer (Vanilla+) adds (snow laid on the surfaces, rain drops
    /// running down the panes). Shadows, ambient occlusion and the detail grain are switched off by the settings.
    pub classic: bool,
    /// The player's vehicle (origin, heading in degrees, `[boundingbox]` w l h cx cy cz):
    /// no rain sheen or snow cover is shaded inside it.
    pub inside: Option<(DVec3, f64, [f32; 6])>,
    /// Actual road height beneath the player's vehicle; independent of suspension motion.
    /// The local puddle capture is skipped when no road height is known.
    pub puddle_ground: Option<f64>,
    /// Upward normal of that actual road face (including road grade and camber).
    pub puddle_normal: Vec3,
    /// Coupled parts of the player's vehicle (same layout as `inside`). Their own
    /// origins keep shared AI meshes out of the local puddle capture.
    pub puddle_parts: Vec<(DVec3, f64, [f32; 6])>,
    /// Procedural (fractal) detail texturing of the ground and roads up close - the
    /// `detail_textures` setting; independent of `enhanced`.
    pub detail: bool,
    /// Enhanced path: how closed the cloud cover is (0..1; `sun_intensity` already says
    /// how much sun comes through), how hard it rains (0..1), the height the weather's fog
    /// lies on (the ground under the player; `None` = just under the camera), and
    /// envir.cfg's light colours relative to the stock ones (A sun, B sky, C ambient).
    pub overcast: f32,
    pub rain: f32,
    pub fog_base: Option<f64>,
    pub envir_tint: [Vec3; 3],
    /// How bright an LED panel's dots burn (`MaterialExtra::led`; the settings' 16 levels
    /// give 0 = off .. 3.75): the enhanced picture draws them this much above their own
    /// colour, bright enough for the glow to bloom a halo around the panel.
    pub led_glow: f32,
    /// How much of the mip chain an LED panel is held at - the `\S:n` mask's (`STFilter`)
    /// and the panel's own grid picture's: both are sampled at the level their screen
    /// footprint asks for, never coarser than this. 0 point-samples them (the sharpest
    /// dots, and the worst shimmer - a regular grid is the worst case for a point sample);
    /// 1.3 (the default) keeps a matrix's dots a couple of pixels across where the full
    /// chain has run them together, and what shimmer is left is a fraction of a
    /// full-resolution sample's; 4 is near the calm of the full chain.
    pub led_mips: f32,
    /// The player's vehicle's velocity (m/s, world): at speed the airstream drives the drops
    /// on its glass up the windscreen and back along the side windows.
    pub glass_wind: Vec3,
}

impl Lighting {
    /// Whether the sun shadow map is drawn with this light (not once the sun is about a
    /// degree below the horizon - Omsi.exe's cutoff, sun z -0.02 in sub_754c80 - nor with
    /// the sun dim, nor with OMSI_NO_SHADOWS).
    pub fn casts_sun_shadows(&self) -> bool {
        self.shadows
            && self.sun_dir.normalize_or_zero().z > -0.02
            && self.sun_intensity > 0.05
            && omsi_cfg::env::var_os("OMSI_NO_SHADOWS").is_none()
    }
}

impl Default for Lighting {
    fn default() -> Self {
        Self {
            min_obj_size: 0.0,
            sun_dir: Vec3::new(0.3, 0.2, 0.9).normalize(),
            sun_intensity: 0.9,
            sun_color: Vec3::ONE,
            secondary: Vec3::splat(0.15),
            ambient: Vec3::splat(0.25),
            fog_color: Vec3::new(0.70, 0.78, 0.90),
            fog_density: 0.0006,
            sky_color: Vec3::new(0.55, 0.70, 0.92),
            night: 0.0,
            night_maps: None,
            sun_azimuth: 0.0,
            sky_weights: [1.0, 0.0, 0.0],
            cloud_density: 0.0,
            cloud_offset: [0.0; 2],
            shadows: true,
            wetness: 0.0,
            snow: 0.0,
            enhanced: false,
            classic: false,
            inside: None,
            puddle_ground: None,
            puddle_normal: Vec3::Z,
            puddle_parts: Vec::new(),
            detail: true,
            overcast: 0.0,
            rain: 0.0,
            fog_base: None,
            envir_tint: [Vec3::ONE; 3],
            led_glow: 1.5,
            led_mips: 1.3,
            glass_wind: Vec3::ZERO,
        }
    }
}

pub type MeshId = usize;
pub type TextureId = usize;
pub type MaterialId = usize;

pub struct GpuMesh {
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    pub ranges: Vec<(u32, u32, u32)>,
    pub bounds_center: Vec3,
    pub bounds_radius: f32,
    /// Back faces are culled (a content mesh, see `MeshData::one_sided`).
    pub one_sided: bool,
    /// Source asset for the optional draw-cost audit.
    pub source: Option<String>,
}

/// An RGBA picture borrowed for an upload.
struct RgbaRef<'a> {
    width: u32,
    height: u32,
    rgba: &'a [u8],
}

pub struct GpuTexture {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: (u32, u32),
    /// Bytes the texture takes on the GPU, all levels (0 for a freed slot's placeholder).
    bytes: u64,
    /// Which texture this is (a slot is reused and a texture replaced): part of the
    /// key of a material bind group made this frame (see `Scene::bind_groups`).
    gen: u64,
}

static TEXTURE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_gen() -> u64 {
    TEXTURE_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl GpuTexture {
    /// A slot showing `view` (a picture the renderer draws: the one behind the rain on the
    /// glass), with `texture` a small stand-in the slot's bookkeeping asks about.
    fn showing(texture: wgpu::Texture, view: wgpu::TextureView, size: (u32, u32)) -> GpuTexture {
        GpuTexture { texture, view, size, bytes: 0, gen: next_gen() }
    }

    fn new(texture: wgpu::Texture, size: (u32, u32), bytes: u64) -> GpuTexture {
        let view = texture.create_view(&Default::default());
        GpuTexture {
            texture,
            view,
            size,
            bytes,
            gen: next_gen(),
        }
    }
}

/// What a material's bind group is made of: its textures (slot and generation), the
/// sampler and the uniform values.
#[derive(Clone, PartialEq, Eq, Hash)]
struct BindKey {
    textures: [(usize, u64); 7],
    address: TexAddressing,
    uniform: [u32; 40],
}

/// Bytes of a texture of `format` with `levels` mip levels.
fn texture_bytes(format: wgpu::TextureFormat, w: u32, h: u32, levels: u32) -> u64 {
    let (bw, bh) = format.block_dimensions();
    let block = format.block_copy_size(None).unwrap_or(4) as u64;
    (0..levels)
        .map(|l| ((w >> l).max(1).div_ceil(bw) * (h >> l).max(1).div_ceil(bh)) as u64 * block)
        .sum()
}

pub struct Material {
    pub texture: Option<TextureId>,
    pub alpha: AlphaMode,
    pub color: [f32; 4],
    pub unlit: bool,
    /// `[matl_noZwrite]`: a blended surface (glass, rain film, dirt) that must not write
    /// depth, or everything blended behind it is thrown away - which is what punched holes
    /// into the world seen through a window or a mirror.
    pub no_z_write: bool,
    /// See [`MaterialExtra::writes_depth`].
    pub writes_depth: bool,
    /// `[matl_noZcheck]`: a decal drawn over the surface it lies on - blended, without
    /// depth write, with the surfaces' depth bias (see the blended draw items).
    pub no_z_check: bool,
    /// `[matl_Zbias]`: a positive bias pulls a decal in front of the coplanar surface
    /// under it (drawn with the depth bias of the road surfaces).
    pub z_bias: i32,
    pub nightmap: Option<TextureId>,
    pub lightmap: Option<TextureId>,
    pub envmap: Option<(TextureId, f32)>,
    /// `[matl_envmap_mask]`: the reflection mask is this texture's alpha instead of the
    /// diffuse texture's.
    pub env_mask: Option<TextureId>,
    /// `[matl_bumpmap]` height map and factor.
    pub bump: Option<(TextureId, f32)>,
    pub emissive: [f32; 3],
    /// `[matl_transmap]` (texture, its alpha channel is used).
    pub transmap: Option<(TextureId, bool)>,
    /// Its textures' addressing (`[matl_texadress_*]`).
    address: TexAddressing,
    /// Keep the exact material parameters so a CTC texture swap can change only the diffuse
    /// map without losing map lighting, moisture, screen, or other renderer flags.
    uniform: MaterialUniform,
    buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl Material {
    /// Whether the material samples texture `id`.
    pub fn uses_texture(&self, id: TextureId) -> bool {
        self.texture == Some(id)
            || self.nightmap == Some(id)
            || self.lightmap == Some(id)
            || self.envmap.map(|e| e.0) == Some(id)
            || self.transmap.map(|t| t.0) == Some(id)
            || self.env_mask == Some(id)
            || self.bump.map(|b| b.0) == Some(id)
    }

    /// `[matl_transmap]` was given, its file there or not (the shader's
    /// `has_transmap_declared`).
    pub fn transmap_declared(&self) -> bool {
        (self.uniform.params2[3] + 0.5) as u32 & 2 != 0
    }
}

/// The material manager's settings beyond the maps of `add_material_all`: depth handling,
/// the reflection mask and the o3d material's specular term.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MaterialExtra {
    /// `[matl_envmap_mask]`
    pub env_mask: Option<TextureId>,
    /// `[matl_noZwrite]`
    pub no_z_write: bool,
    /// A slot `no_z_write` marks as a see-through layer (a pane, a dirt film, a sticker
    /// on a window - for the glass shading and the shadow map) that the model.cfg does not
    /// give `[matl_noZwrite]`: Omsi.exe draws it with its depth written (0x7fd6c4 sets
    /// ZWRITEENABLE from that flag alone), and so is it drawn here. Stacked panes of a
    /// door or a window then hide each other in model order as in the original, instead
    /// of all being blended over each other whichever is in front (#211).
    pub writes_depth: bool,
    /// `[matl_noZcheck]`
    pub no_z_check: bool,
    /// `[matl_Zbias]`
    pub z_bias: i32,
    /// The D3D material's ambient colour, its share of the ambient light (C); None: the
    /// diffuse colour's.
    pub ambient: Option<[f32; 3]>,
    /// Specular colour (rgb) and power (w) of the D3D material; black = no highlight.
    pub specular: [f32; 4],
    /// `[matl_bumpmap]`: a height map (in its alpha, `Image::bump_height_map`) whose slope
    /// shifts the `[matl_envmap]` lookup, times the factor.
    pub bump: Option<(TextureId, f32)>,
    /// A named transparent window layer. This is separate from envmap/transmap because
    /// stock and add-on buses often use a plain alpha-blended window texture.
    pub glass: bool,
    /// The night map is switched by something other than the time of day - a `[matl_item]`'s
    /// variable, a vehicle mesh's `[visible]`: it glows by day as well (warning lamps,
    /// dashboard displays), not only at night.
    pub night_switched: bool,
    /// A display's text (`[useTextTexture]`): in the enhanced picture it glows a little
    /// by itself, as a lit matrix does, instead of taking only the light that reaches it
    /// under the bus's front overhang, where it was hardly readable by day.
    pub display: bool,
    /// A screen the bus draws itself - a `[useTextTexture]` or `[useScriptTexture]` slot:
    /// the IBIS, the matrix displays, the dashboard's LCDs. The enhanced picture's glow
    /// and FXAA leave it alone (see `MASK_FORMAT`): FXAA took half the contrast out of
    /// their letters and they read as blurred.
    pub screen: bool,
    /// An LED matrix - a display whose lit dots are the `\S:n` script texture's
    /// (`[matl_transmap]`), the Krueger and K++ destination panels: the dots are the
    /// panel's own light, so the enhanced picture lets them burn in HDR and blooms them
    /// (the glow's source keeps them, where the other screens are left out of it -
    /// something no Direct3D 9 without shaders of its own could do). `MASK_FORMAT`'s g.
    /// (Only a panel whose `[matl_lightmap]` is white all over: a flipdot carries the same
    /// mask, but its light map is a picture of the lamps over it, and it does not glow.)
    pub led: bool,
    /// The film of water on a window (`[alphascale] Rain_Window_…`): drawn as drops that sit,
    /// gather and run down the glass instead of the texture sliding down as a whole.
    pub rain_film: bool,
    /// The map's water (`texture/water.tga`): Enhanced draws it as water - a smooth surface
    /// mirroring the sky more the flatter it is seen, rippled by small waves.
    pub water: bool,
    /// `[nomaplighting]`: the map's lamps (`[maplight]`) do not light it - a street lamp
    /// is not lit by its own light.
    pub no_map_lights: bool,
    /// A `[tree]`'s leaf cards: the vanilla picture leaves the map's lamps off them, as
    /// OMSI 2 shows a tree standing right under a street lamp dark; Vanilla+ and Enhanced
    /// still light them.
    pub tree: bool,
    /// 1 when the texture's `.cfg` sidecar carries `[moisture]`/`[puddles]`: the road of a
    /// junction or crossing object gets wet and collects puddles like a spline's.
    pub moisture: f32,
    /// `[matl_transmap]` was given, whether or not its file is there: Omsi.exe raises the
    /// material's transmap flag before it reads the name (0x7fbbf4), and with it the
    /// `[matl_envmap]` reflection goes by the texture's alpha instead of the factor.
    pub transmap_declared: bool,
    /// `[matl_texadress_border]`: its colour (RGBA, 0..1). Where the (scrolled) texture
    /// coordinates leave [0, 1] the diffuse texture reads this colour instead of its edge,
    /// as Direct3D's border addressing does: a roller blind's band that has scrolled away
    /// vanishes in a transparent border.
    pub border: Option<[f32; 4]>,
    /// An opaque, sphere-mapped part of a vehicle that is not its body (a handrail, a
    /// bumper, a wheel trim): the enhanced picture may make it metal by its `[matl_envmap]`
    /// factor alone, as the vanilla one shows the sphere map on it - chrome read as a
    /// faint clear coat there. A body needs a mask of its own for that (a Golf's bonnet).
    pub metal_ok: bool,
}

/// The textures a material's bind group samples.
#[derive(Clone, Copy)]
struct MaterialMaps {
    texture: Option<TextureId>,
    transmap: Option<(TextureId, bool)>,
    nightmap: Option<TextureId>,
    lightmap: Option<TextureId>,
    envmap: Option<(TextureId, f32)>,
    env_mask: Option<TextureId>,
    bump: Option<(TextureId, f32)>,
    pbr: Option<PbrMaps>,
}

#[derive(Clone, Copy, Default)]
struct InstanceBounds {
    centre: Vec3,
    radius: f32,
    scale: f32,
}

impl InstanceBounds {
    fn new(mesh: &GpuMesh, transform: Mat4) -> Self {
        let scale = transform_scale(transform);
        Self {
            centre: transform.transform_point3(mesh.bounds_center),
            radius: mesh.bounds_radius * scale,
            scale,
        }
    }
}

fn transform_scale(transform: Mat4) -> f32 {
    transform.x_axis.truncate().length_squared()
        .max(transform.y_axis.truncate().length_squared())
        .max(transform.z_axis.truncate().length_squared()).sqrt()
}

/// The ordered world passes used by OMSI for ground and scenery geometry.
///
/// Keep these phases separate in the main pass: a later phase must be able to sit over an
/// earlier blended surface, while depth testing still lets nearer geometry win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum RenderPhase {
    PreSurface = 0,
    Terrain = 1,
    Surface = 2,
    Spline = 3,
    OnSurface = 4,
    BeforeNormal = 5,
    #[default]
    Normal = 6,
    AfterNormal = 7,
    AfterVehicles = 8,
}

impl RenderPhase {
    const COUNT: usize = 9;
    const DRAW_ORDER: [Self; Self::COUNT] = [
        Self::PreSurface,
        Self::Terrain,
        Self::Surface,
        Self::Spline,
        Self::OnSurface,
        Self::BeforeNormal,
        Self::Normal,
        Self::AfterNormal,
        Self::AfterVehicles,
    ];
}

pub struct Instance {
    pub mesh: MeshId,
    /// Transform relative to `origin` (rotation/scale plus a small translation).
    pub transform: Mat4,
    /// World position of the instance's local frame.
    pub origin: DVec3,
    /// Material per mesh material slot.
    pub materials: Vec<MaterialId>,
    /// Dynamic parameters: per material slot alpha multiplier; per instance visibility and
    /// uv offset.
    pub slot_alpha: Vec<f32>,
    /// Per material slot `[matl_lightmap]` strength (script variable).
    pub slot_light: Vec<f32>,
    /// Per material slot: is the `[matl_item]` variant (night map) active (0/1).
    pub slot_night: Vec<f32>,
    pub visible: bool,
    /// `[texcoordtransX/Y]` offset per material slot: every band of the SD200's roller
    /// blind scrolls on its own, so one offset for the whole mesh is not enough.
    pub slot_uv: Vec<[f32; 2]>,
    /// Interior light brightness (0..1) added as warm ambient (`[interiorlight]`), for an
    /// instance without lamps of its own (a passenger standing in a lit bus).
    pub interior: f32,
    /// The `[interiorlight]` lamps that light this mesh (its `[illumination_interior]`): the
    /// first of its run of slots in `Scene::interior_lights` times `LAMP_CODE_STRIDE` plus
    /// how many; 0 = none, `interior` stands in.
    pub interior_lamps: u32,
    /// First entry of this instance in the per-draw storage buffers (set by `prepare`).
    base: u32,
    /// Transformed local bounds, refreshed with the GPU instance data. The floating
    /// render origin is applied per view, so changing it cannot leave stale spheres.
    bounds: InstanceBounds,
    /// Surface geometry classification (roads, markings, crossings), used for culling and
    /// weather/shading. `surface_bias` independently selects the rasterizer's depth bias.
    pub surface: bool,
    /// `[rendertype] presurface`: drawn before terrain, including blended materials whose
    /// transparent texels write depth to reveal excavations below the ground.
    pub presurface: bool,
    /// OMSI world-pass order. Most instances use `Normal`; road/surface assets are assigned
    /// their authored phase by the scene loader.
    pub render_phase: RenderPhase,
    /// Apply rasterizer depth bias. Metric-lifted OMSI roads and ordered scenery phases skip
    /// it, so their placement does not change with the camera angle.
    pub surface_bias: bool,
    /// OMSI sorts blended spline pieces by their placement origin (horizontal distance), not
    /// the containing map tile's shared origin.
    pub blend_sort_origin: Option<DVec3>,
    /// Screen-size range [min, max) in which this instance is drawn (`[LOD]` levels).
    pub lod: (f32, f32),
    /// A vehicle's flat shadow blob (`[isshadow]`, a surface). It is drawn always, as OMSI
    /// draws it: it stood in for the sun shadow map only while that was off, and with the
    /// map on (the usual case, Enhanced always) no bus had anything under it - the sun's
    /// shadow falls beside the bus at any but a noon sun, while the blob is the sky light
    /// the body keeps off the road, which no shadow map and no screen-space AO supplies.
    pub blob: bool,
    /// A painted ground layer (`[groundtex]` through its brush mask): a surface that is the
    /// ground itself, so it does not get the roads' pull towards the camera (see vs_main).
    pub ground_layer: bool,
    /// A surface object (a crossing, markings) over the road splines. Legacy instances get a
    /// small view-space pull; ordered OMSI surfaces keep the flag for shading but skip it.
    pub decal: bool,
    /// The whole object this mesh belongs to (see `set_object_culling`): the radius of a
    /// sphere about `origin` that holds all of it (0 = the mesh is judged on its own sphere),
    /// its `[detail_factor]` and whether it is kept at any distance (`[noDistanceCheck]`).
    pub object_radius: f32,
    pub detail: f32,
    pub any_distance: bool,
    /// Drawn only while the camera stands in this area of the ground (world x0, y0, x1, y1):
    /// a stand-in for far tiles, which OMSI has loaded only around its own tile (see
    /// `set_near_only`).
    pub near_only: Option<[f64; 4]>,
    /// Seen only in the mirrors and other views drawn into textures, not in the window's
    /// picture: the driver at the wheel while the player looks from the driver's seat (the
    /// figure would fill the view, but the mirrors show him as OMSI does).
    pub mirror_only: bool,
    /// The model marks the mesh `[shadow]`: one OMSI casts a shadow from (with the
    /// option `omsi_shadow_casters` only these do).
    pub omsi_caster: bool,
    /// It may cast a sun shadow: every ordinary instance, no surface - a surface lies on
    /// the ground, and a caster in one plane with what it falls on paints dark patches into
    /// it - except a spline standing clear of the ground (a bridge deck, an elevated
    /// railway), which is raised with `set_casts_shadow`.
    pub casts_shadow: bool,
    /// Part of a vehicle whose roof lies this high over its origin (model frame): what faces
    /// up under the roof (the floor, the seats) is out of the weather - no snow nor wet on
    /// it. (Only the vehicle the camera is in was spared, by its box; every other bus showed
    /// its saloon under snow through the windows.)
    pub roof: Option<f32>,
    /// Drawn with every slot in model order among the blended draws, as Omsi.exe draws a
    /// model: mesh after mesh, each material subset with its own states and depth write
    /// (0x7c32c4 -> 0x7fd6c4, DrawSubset), not its opaque parts first. Set on the models
    /// where it matters: a blended slot that writes depth before an opaque one (a body with
    /// `[matl_alpha] 2` listed before its interior hides the interior as in the original,
    /// instead of showing it through the paint's alpha).
    pub ordered: bool,
}

pub struct Scene {
    pub meshes: Vec<GpuMesh>,
    pub textures: Vec<GpuTexture>,
    /// Texture slot read by procedural rain films.
    glass_slot: Option<TextureId>,
    pub materials: Vec<Material>,
    pub instances: Vec<Instance>,
    /// World position everything is expressed relative to on the GPU (updated per frame).
    pub render_origin: DVec3,
    /// Point lights and coronas for the next frame (set by the app every frame).
    pub lights: Vec<PointLight>,
    /// The vehicles' `[interiorlight]` lamps, in slots each vehicle keeps
    /// (`Renderer::alloc_interior_lights`): they light only the meshes that name them.
    pub interior_lights: Vec<PointLight>,
    interior_free: Vec<(u32, u32)>,
    pub coronas: Vec<Corona>,
    /// Smoke particles for the next frame (set by the app every frame).
    pub smoke: Vec<SmokeParticle>,
    smoke_buf: Option<wgpu::Buffer>,
    smoke_count: u32,
    /// Runs of this frame's coronas by picture: (texture, first, count).
    /// (and whether the run belongs to the vehicle the camera is in, drawn after it)
    corona_runs: Vec<(u16, u32, u32, bool)>,
    model_buf: Option<GpuArray>,
    params_buf: Option<GpuArray>,
    light_buf: Option<GpuArray>,
    grid_buf: Option<GpuArray>,
    corona_buf: Option<wgpu::Buffer>,
    corona_count: u32,
    /// The frame's draw list (see `Batch`), shared by the shadow, prepass and main passes.
    draw_buf: Option<GpuArray>,
    camera_bind_group: Option<wgpu::BindGroup>,
    shadow_bind_group: Option<wgpu::BindGroup>,
    sky_bind_group: Option<wgpu::BindGroup>,
    /// HUD images drawn after the scene: (texture, rect in pixels x0,y0,x1,y1).
    pub overlays: Vec<(TextureId, [f32; 4])>,
    /// Overlay textures that hold premultiplied alpha (drawn by `omsi-ui`, e.g. the
    /// navigator) rather than straight alpha.
    pub premultiplied: std::collections::HashSet<TextureId>,
    /// Per overlay: the texture its bind group was made for, its rect buffer and the group
    /// (kept between frames; only the rect is rewritten).
    overlay_res: Vec<(TextureId, wgpu::Buffer, wgpu::BindGroup, [f32; 8])>,
    /// Structural change (render origin moved, buffers too small): everything is rebuilt.
    dirty: bool,
    /// How many instances (and per-draw entries) the buffers hold; instances added since
    /// are appended to the buffers instead of rebuilding them, as long as they fit.
    uploaded_instances: usize,
    uploaded_entries: u32,
    /// Instances whose transform or parameters changed since the last `prepare`: only
    /// their entries are rewritten. Rebuilding the whole per-draw buffer for 17 000 objects
    /// because one bus moved was the biggest single CPU cost of a frame.
    changed: Vec<usize>,
    changed_mark: Vec<bool>,
    cache_bounds: bool,
    bounds_meshes: Vec<bool>,
    bounds_dirty: bool,
    /// What the per-draw buffers hold, kept on the CPU: changed entries are written here
    /// and uploaded as a few merged ranges. Every `write_buffer` makes a new staging buffer
    /// on the GPU, and one per changed vehicle or person was a hundred of them a frame.
    cpu_models: Vec<[[f32; 4]; 4]>,
    cpu_params: Vec<[f32; 4]>,
    /// The light grid and lights as last uploaded, so that unchanged ones are not sent again.
    last_grid: Vec<u32>,
    last_lights: Vec<u8>,
    /// Material bind groups and uniform buffers made since the last `prepare`, by what they
    /// hold: materials made in one go with the same textures and values share them (a C2's
    /// 965 materials need about a tenth as many). Only for a frame, so that nothing keeps a
    /// freed or replaced texture alive.
    bind_groups: HashMap<BindKey, (wgpu::BindGroup, wgpu::Buffer)>,
    /// The PBR maps of a diffuse texture (register them before making its materials).
    pub pbr_maps: HashMap<TextureId, PbrMaps>,
    /// Textures that are a season's snow pictures (`WinterSnow` folders): a material drawn
    /// with one shows its snow as the map made it, as OMSI 2 shows snow, and gets no snow
    /// laid over it (register them before making their materials).
    pub snow_textures: std::collections::HashSet<TextureId>,
}

/// `MaterialUniform::ambient`'s w: 1 for a material whose texture is a season's snow
/// picture (`Scene::snow_textures`).
fn snow_texture_flag(scene: &Scene, texture: Option<TextureId>) -> f32 {
    if texture.is_some_and(|t| scene.snow_textures.contains(&t)) {
        1.0
    } else {
        0.0
    }
}

impl Scene {
    /// Bytes of one texture on the GPU (0 for a freed slot or an unknown id).
    pub fn texture_bytes_of(&self, id: TextureId) -> u64 {
        self.textures.get(id).map(|t| t.bytes).unwrap_or(0)
    }

    /// The size of one texture.
    pub fn texture_size_of(&self, id: TextureId) -> Option<(u32, u32)> {
        self.textures.get(id).map(|t| t.size)
    }

    /// The GPU format of one texture, for statistics.
    pub fn texture_format_of(&self, id: TextureId) -> String {
        self.textures
            .get(id)
            .map(|t| format!("{:?}", t.texture.format()))
            .unwrap_or_default()
    }

    /// Bytes on the GPU: (textures, mesh buffers, per-draw and light buffers). Freed slots
    /// share one small placeholder, which is not counted.
    pub fn gpu_bytes(&self) -> (u64, u64, u64) {
        let tex = self.textures.iter().map(|t| t.bytes).sum();
        let mut seen: std::collections::HashSet<*const ()> = std::collections::HashSet::new();
        let mut mesh = 0u64;
        for m in &self.meshes {
            if m.ranges.is_empty() {
                continue;
            }
            for b in [&m.vertex_buf, &m.index_buf] {
                if seen.insert(b as *const wgpu::Buffer as *const ()) {
                    mesh += b.size();
                }
            }
        }
        let other = [&self.model_buf, &self.params_buf, &self.light_buf, &self.grid_buf, &self.draw_buf]
            .iter()
            .filter_map(|b| b.as_ref())
            .map(|b| b.gpu_bytes())
            .sum::<u64>()
            + self.corona_buf.as_ref().map_or(0, |b| b.size());
        (tex, mesh, other)
    }
}

/// The enhanced path's post pipelines (post.wgsl).
struct PostPipelines {
    down_first: wgpu::RenderPipeline,
    down: wgpu::RenderPipeline,
    up: wgpu::RenderPipeline,
    meter: wgpu::RenderPipeline,
    adapt: wgpu::RenderPipeline,
    /// Straight into the target, or gamma-encoded for FXAA.
    tonemap: wgpu::RenderPipeline,
    tonemap_encoded: wgpu::RenderPipeline,
    fxaa: wgpu::RenderPipeline,
}

/// The pipelines of the main pass for one colour target format: the swap chain's, and
/// the high-range one of the enhanced path.
struct PassPipelines {
    /// Single-sampled films, drawn after resolving the scene and its reflections.
    rain_pipelines: Vec<wgpu::RenderPipeline>,
    /// Indexed by `pipe_code`: 4 depth/blend kinds x culled x depth-biased.
    pipelines: Vec<wgpu::RenderPipeline>,
    corona_pipeline: wgpu::RenderPipeline,
    /// Smoke particles (`[smoke]`): the corona sprite alpha-blended with the smoke texture.
    smoke_pipeline: wgpu::RenderPipeline,
    sky_pipeline: wgpu::RenderPipeline,
}

/// Texture memory (MB) the adapter is taken to have room for (0 = no adapter yet), see
/// `Renderer::new`.
pub static ADAPTER_TEXTURE_MB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The device runs on OpenGL (set in `Renderer::new`).
static GL_BACKEND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the device draws on OpenGL (known once a renderer is made).
pub fn gl_backend() -> bool {
    GL_BACKEND.load(std::sync::atomic::Ordering::Relaxed)
}

/// How the per-draw arrays (the model matrices, the instance parameters, the draw list) and
/// the point lights reach the scene shader on this device (set in `Renderer::new_on`).
/// Older OpenGL chips cannot read a storage buffer in a vertex shader - an Intel HD 2500, a
/// Mali on GLES ("Downlevel flags VERTEX_STORAGE are required but not supported", the
/// renderer was never made, #770, #316) - or have no storage buffers at all (OpenGL below
/// 4.3, GLES 3.0): for them the arrays are textures the vertex shader reads texel by texel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ArrayPath {
    /// Storage buffers everywhere (every Metal, Vulkan and DirectX 12 device).
    Storage,
    /// The vertex shader's arrays are textures; the lights stay storage buffers.
    VertexTextures,
    /// No storage buffers at all: the arrays are textures, and the point lights (the street
    /// lamps, headlights and interior lamps lit per pixel) are left out - the sixteen
    /// textures a fragment shader may read here are all taken.
    NoStorage,
}

static ARRAY_PATH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn array_path() -> ArrayPath {
    match ARRAY_PATH.load(std::sync::atomic::Ordering::Relaxed) {
        1 => ArrayPath::VertexTextures,
        2 => ArrayPath::NoStorage,
        _ => ArrayPath::Storage,
    }
}

/// Whether the camera group has the enhanced path's three textures (the reflection probe,
/// the sky table, the sky cube). The OpenGL backend numbers every texture of a pipeline
/// layout in one row of sixteen (wgpu-hal's `MAX_TEXTURE_SLOTS`), whatever stage reads it:
/// where the scene's arrays are textures as well (three more), the camera and material
/// groups came to 19 and making the scene pipelines panicked ("index out of bounds: the
/// len is 16 but the index is 16", Mali on Android, #1133). There the three are left out
/// and the enhanced path with them.
fn camera_enhanced_textures() -> bool {
    !(gl_backend() && array_path() != ArrayPath::Storage)
}

/// Texels per row of an array kept as a texture (within every device's 2048).
const ARRAY_TEX_WIDTH: u32 = 2048;

/// A read-only array of the scene shader: a storage buffer, or where the device cannot
/// read one (see `ArrayPath`) a texture of one value per texel - four floats
/// (`Rgba32Float`, 16 bytes) or one `u32` (`R32Uint`) - in rows of `ARRAY_TEX_WIDTH`.
enum GpuArray {
    Buffer(wgpu::Buffer),
    Texture { texture: wgpu::Texture, view: wgpu::TextureView, texel: u32 },
    /// Not bound on this device (the lights under `ArrayPath::NoStorage`): takes any write.
    Unused,
}

impl GpuArray {
    /// An array of `size` bytes of `texel`-byte values (16 or 4), a texture where the
    /// vertex shader reads it on a device without vertex storage (`vertex`), or without any.
    fn new(device: &wgpu::Device, label: &str, size: u64, texel: u32, vertex: bool) -> GpuArray {
        let path = array_path();
        if path == ArrayPath::Storage || (!vertex && path == ArrayPath::VertexTextures) {
            return GpuArray::Buffer(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(16).next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if !vertex {
            return GpuArray::Unused;
        }
        let texels = size.div_ceil(texel as u64).max(1);
        let max_rows = device.limits().max_texture_dimension_2d;
        let rows = (texels.div_ceil(ARRAY_TEX_WIDTH as u64) as u32).clamp(1, max_rows);
        if rows == max_rows {
            log::warn!("{label}: {texels} values do not fit a texture of {ARRAY_TEX_WIDTH} x {max_rows}; the rest are not drawn");
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: ARRAY_TEX_WIDTH, height: rows, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: if texel == 16 { wgpu::TextureFormat::Rgba32Float } else { wgpu::TextureFormat::R32Uint },
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        GpuArray::Texture { texture, view, texel }
    }

    /// Its room in bytes.
    fn size(&self) -> u64 {
        match self {
            GpuArray::Buffer(b) => b.size(),
            GpuArray::Texture { texture, texel, .. } => texture.width() as u64 * texture.height() as u64 * *texel as u64,
            GpuArray::Unused => u64::MAX,
        }
    }

    /// Bytes it takes on the GPU (for the memory summary).
    fn gpu_bytes(&self) -> u64 {
        match self {
            GpuArray::Unused => 0,
            a => a.size(),
        }
    }

    /// Write `data` at byte `offset` (both whole values); what runs past its end is dropped.
    fn write(&self, queue: &wgpu::Queue, offset: u64, data: &[u8]) {
        match self {
            GpuArray::Buffer(b) => queue.write_buffer(b, offset, data),
            GpuArray::Texture { texture, texel, .. } => {
                let t = *texel as usize;
                let total = texture.width() as u64 * texture.height() as u64;
                let mut data = data;
                for (x, y, width, rows) in array_tex_spans(offset / t as u64, (data.len() / t) as u64, total) {
                    let bytes = (width * rows) as usize * t;
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
                        &data[..bytes],
                        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width * t as u32), rows_per_image: None },
                        wgpu::Extent3d { width, height: rows, depth_or_array_layers: 1 },
                    );
                    data = &data[bytes..];
                }
            }
            GpuArray::Unused => {}
        }
    }

    fn binding(&self) -> wgpu::BindingResource<'_> {
        match self {
            GpuArray::Buffer(b) => b.as_entire_binding(),
            GpuArray::Texture { view, .. } => wgpu::BindingResource::TextureView(view),
            GpuArray::Unused => unreachable!("an unused array is not bound"),
        }
    }
}

/// The rectangles (x, y, width, rows) of an array texture `ARRAY_TEX_WIDTH` wide and
/// `total` texels big that `n` values from value `at` on fill, in order: the rest of a row,
/// whole rows, the start of the last row. What runs past the end is left out.
fn array_tex_spans(mut at: u64, n: u64, total: u64) -> Vec<(u32, u32, u32, u32)> {
    let w = ARRAY_TEX_WIDTH as u64;
    let mut left = n.min(total.saturating_sub(at));
    let mut out = Vec::new();
    while left > 0 {
        let (x, y) = (at % w, at / w);
        let (width, rows) = if x == 0 && left >= w { (w, left / w) } else { ((w - x).min(left), 1) };
        out.push((x as u32, y as u32, width as u32, rows as u32));
        at += width * rows;
        left -= width * rows;
    }
    out
}

/// The bind group layout entry of a scene array read in `stage` (see `GpuArray`):
/// `float4` for vec4 values, else u32.
fn array_layout_entry(binding: u32, stage: wgpu::ShaderStages, float4: bool) -> wgpu::BindGroupLayoutEntry {
    let texture = match array_path() {
        ArrayPath::Storage => false,
        ArrayPath::VertexTextures => stage.contains(wgpu::ShaderStages::VERTEX),
        ArrayPath::NoStorage => true,
    };
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: stage,
        ty: if texture {
            wgpu::BindingType::Texture {
                sample_type: if float4 { wgpu::TextureSampleType::Float { filterable: false } } else { wgpu::TextureSampleType::Uint },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            }
        } else {
            wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }
        },
        count: None,
    }
}

/// Wait until the GPU has done `submission` (None: everything submitted so far).
///
/// On OpenGL wgpu holds the one GL context for the whole of a wait, and every other thread
/// that wants it meanwhile (a worker making a bus's textures) gives up after
/// a second with a panic - "Could not lock adapter context. This is most-likely a deadlock."
/// (wgpu-hal's WGL lock; #843: a slow chip took longer than that for a frame). There the
/// wait is made of short ones, and the context is free between them.
pub fn wait_gpu(device: &wgpu::Device, submission: Option<wgpu::SubmissionIndex>) -> Result<(), wgpu::PollError> {
    if !gl_backend() {
        return device.poll(wgpu::PollType::Wait { submission_index: submission, timeout: None }).map(|_| ());
    }
    loop {
        match device.poll(wgpu::PollType::Wait { submission_index: submission.clone(), timeout: Some(GL_WAIT_SLICE) }) {
            Err(wgpu::PollError::Timeout) => std::thread::yield_now(),
            r => return r.map(|_| ()),
        }
    }
}

/// The longest a single wait for the GPU holds the GL context (see [`wait_gpu`]).
const GL_WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(20);

/// On OpenGL, the GPU work of worker threads (textures and meshes of a bus made while the
/// world loads) goes one thread at a time: a dozen of them queueing for the GL context left
/// the last one waiting past wgpu's one second (#843). Elsewhere the device takes them all.
fn gl_worker_turn() -> Option<std::sync::MutexGuard<'static, ()>> {
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    gl_backend().then(|| TURN.lock().unwrap_or_else(|e| e.into_inner()))
}

/// The card's own memory in MB where the system tells it: Windows, through DXGI, for
/// whichever backend draws (wgpu does not say).
fn dedicated_vram_mb(info: &wgpu::AdapterInfo) -> Option<u64> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
        let f: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0;
        while let Ok(a) = f.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = a.GetDesc1() else { continue };
            if d.VendorId == info.vendor && d.DeviceId == info.device {
                return Some(d.DedicatedVideoMemory as u64 >> 20);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = info;
        None
    }
}

pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter_name: String,
    camera_layout: wgpu::BindGroupLayout,
    material_layout: wgpu::BindGroupLayout,
    pass: PassPipelines,
    hdr_pass: Option<PassPipelines>,
    reflection_pass: Option<PassPipelines>,
    corona_bind_group: wgpu::BindGroup,
    /// The smoke texture (`Texture/rauch.tga`, see [`Renderer::set_smoke_texture`]).
    smoke_bind_group: wgpu::BindGroup,
    /// The coronas' pictures besides the standard glow (index = `Corona::texture`; entry 0
    /// unused).
    corona_textures: Vec<Option<wgpu::BindGroup>>,
    corona_layout: wgpu::BindGroupLayout,
    corona_sampler: wgpu::Sampler,
    sky_layout: wgpu::BindGroupLayout,
    sky_sampler: wgpu::Sampler,
    /// The enhanced clouds' noise (clouds.rs): the shape map, the detail volume and their
    /// repeating, mip-mapped sampler (sky bind group bindings 6-8).
    cloud_shape_view: wgpu::TextureView,
    cloud_detail_view: wgpu::TextureView,
    cloud_sampler: wgpu::Sampler,
    sky_mesh: (wgpu::Buffer, wgpu::Buffer, u32),
    overlay_pipeline: wgpu::RenderPipeline,
    overlay_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    camera_buf: wgpu::Buffer,
    white_texture: GpuTexture,
    black_texture: GpuTexture,
    /// A tangent-space normal pointing straight out (the PBR normal map's stand-in).
    flat_normal_texture: GpuTexture,
    format: wgpu::TextureFormat,
    depth: Option<(wgpu::Texture, wgpu::TextureView, u32, u32)>,
    /// Multisampled colour and depth targets per size (the window, each mirror).
    msaa_targets: HashMap<(u32, u32), (wgpu::TextureView, wgpu::TextureView)>,
    /// Screen-space ambient occlusion: depth prepass and AO textures of the window's size.
    ao: Option<AoTargets>,
    ao_sampler: wgpu::Sampler,
    ao_layout: wgpu::BindGroupLayout,
    ao_buf: wgpu::Buffer,
    /// Depth prepass pipelines (plain, alpha-tested; each two-sided and culled),
    /// single-sampled, camera projection.
    /// Plain opaque, alpha-tested, and the opaque portions of blended transmap materials;
    /// each has a two-sided and a culled variant.
    prepass_pipelines: [wgpu::RenderPipeline; 6],
    /// The same, multisampled: the enhanced main pass's own depth laid first (see
    /// `render_inner`), so that its costly shading runs once per visible surface.
    prepass_msaa_pipelines: Option<[wgpu::RenderPipeline; 6]>,
    /// Ambient occlusion and its blur; none on OpenGL (GLES), whose shading language cannot
    /// read a depth texture texel by texel - the pipelines failed there, AO off or not (#422).
    ssao_pipeline: Option<wgpu::RenderPipeline>,
    blur_pipeline: Option<wgpu::RenderPipeline>,
    shadow_view: wgpu::TextureView,
    shadow_view_far: wgpu::TextureView,
    shadow_sampler: wgpu::Sampler,
    shadow_layout: wgpu::BindGroupLayout,
    /// [near opaque, near alpha-tested, far opaque, far alpha-tested]
    shadow_pipelines: [wgpu::RenderPipeline; 6],
    /// The settings this renderer was built with.
    pub options: RenderOptions,
    /// Enhanced path: the HDR targets per size, the post pipelines and their resources.
    hdr_targets: HashMap<(u32, u32), HdrTargets>,
    puddles: Option<puddles::Pipelines>,
    post: PostPipelines,
    post_layout: wgpu::BindGroupLayout,
    post_buf: wgpu::Buffer,
    post_sampler: wgpu::Sampler,
    /// The metered mean log luminance (1x1), and the adapted value in two textures drawn
    /// into by turns (`adapt_front` is the current one), with their bind groups.
    meter_view: wgpu::TextureView,
    adapt_views: [wgpu::TextureView; 2],
    adapt_bg: [wgpu::BindGroup; 2],
    adapt_front: usize,
    /// OMSI_DEBUG_EXPOSURE: the adapted metering read back now and then, and logged.
    exposure_log: Option<ExposureLog>,
    /// Enhanced lighting: its uniform, the sky table, the sampler both use, the reflection
    /// probe, the sky last computed and the exposure as it follows it.
    enh_buf: wgpu::Buffer,
    sky_lut: wgpu::Texture,
    sky_lut_view: wgpu::TextureView,
    lin_sampler: wgpu::Sampler,
    probe: Option<Probe>,
    sky_state: Option<atmosphere::SkyState>,
    /// A sky being computed on a helper thread, for this input.
    sky_job: Option<(
        atmosphere::SkyInput,
        std::sync::mpsc::Receiver<atmosphere::SkyState>,
    )>,
    /// The pre-exposure (natural log) as it follows the sky's.
    exposure: Option<f32>,
    /// The projection's width over height while a picture is drawn into a texture (see
    /// `render_to_texture`), whatever the texture's own shape.
    texture_aspect: Option<f32>,
    last_frame: Option<std::time::Instant>,
    /// The next frame stands alone (an offscreen picture): the exposure is there at once.
    pub instant_exposure: bool,
    /// Overlay pipeline without multisampling, for drawing the HUD after the post pass.
    overlay_pipeline_1x: wgpu::RenderPipeline,
    xr_ui_pipeline: wgpu::RenderPipeline,
    started: std::time::Instant,
    /// `[matl_texadress_clamp]` (and border, mirror-once) and `[matl_texadress_mirror]`.
    clamp_sampler: wgpu::Sampler,
    mirror_sampler: wgpu::Sampler,
    /// The addressing of the next material's textures (`[matl_texadress_*]`).
    pub address_next: std::cell::Cell<TexAddressing>,
    /// The next material is lit at night by the tile light maps (`[LightMapMapping]`, the
    /// splines) instead of the map's lamps.
    pub light_map_next: std::cell::Cell<bool>,
    /// The tile light maps (`.map.LM.bmp`) of the 5x5 tiles around the camera, north up, and
    /// where that square lies (world x, y of its south-west corner, its side in metres).
    lm_atlas: wgpu::Texture,
    lm_atlas_view: wgpu::TextureView,
    lm_uniform: wgpu::Buffer,
    lm_place: std::cell::Cell<(f64, f64, f64)>,
    /// Mipmap generation on the GPU (a blit per level).
    mip_pipeline: wgpu::RenderPipeline,
    mip_layout: wgpu::BindGroupLayout,
    mip_sampler: wgpu::Sampler,
    /// Set by the device's error handler when something failed with multisampling on.
    gpu_error: Arc<std::sync::atomic::AtomicBool>,
    /// The headset's pictures: the heading (degrees) the `[matl_envmap]` sphere maps are
    /// laid out by instead of each eye's view (see `set_env_heading`).
    env_heading: std::cell::Cell<Option<f32>>,
    /// The card ran out of memory since the last `take_out_of_memory`.
    out_of_memory: Arc<std::sync::atomic::AtomicBool>,
    /// Why the graphics device was lost (a driver reset, the card removed), once it was.
    device_lost: Arc<std::sync::Mutex<Option<String>>>,
    /// Render scale: the pipeline that scales the 3D picture up to the window, its
    /// parameters, and the smaller targets per size (with their bind groups).
    upscale_pipeline: wgpu::RenderPipeline,
    copy_pipeline: wgpu::RenderPipeline,
    /// Triple screen: a panel's picture copied 1:1 to its place in the window.
    panel_pipeline: wgpu::RenderPipeline,
    upscale_layout: wgpu::BindGroupLayout,
    upscale_buf: wgpu::Buffer,
    scale_targets: HashMap<(u32, u32), (wgpu::TextureView, wgpu::BindGroup)>,
    triple_targets: Option<((u32, u32), Vec<(wgpu::TextureView, wgpu::BindGroup)>)>,
    triple_culling: [ViewCulling; 3],
    /// Full-resolution current scene before rain films, in its original colour format.
    glass_picture: Option<wgpu::TextureView>,
    /// When each size of the size-keyed targets (scale, MSAA, HDR) was last asked for.
    target_use: HashMap<(u32, u32), std::time::Instant>,
    /// The game's frame-rate governor on top of the render scale (1 = none; see
    /// `set_dynamic_scale`).
    dynamic_scale: std::cell::Cell<f32>,
    /// `OMSI_DEBUG_FLICKER`: which near instances in view were drawn last frame.
    flicker: std::cell::RefCell<HashMap<usize, bool>>,
    /// Instances the main view drew last frame (a bit each): they keep being drawn a little
    /// past the size and distance limits, so that an object right at a limit does not pop
    /// in and out as the camera sways.
    cull_drawn: std::cell::RefCell<Vec<u64>>,
    /// The screen size each object (origin and radius) was judged by in the main view's last
    /// frame: all its meshes and LOD levels take the same one, so exactly one level of an
    /// object is drawn and it does not flip between levels with the view's jitter.
    object_sizes: std::cell::RefCell<hashbrown::HashMap<[u64; 4], f32>>,
    /// Cleared and reused as the next main view's object-size history.
    object_sizes_scratch: std::cell::RefCell<hashbrown::HashMap<[u64; 4], f32>>,
    /// The far shadow cascade as last drawn: its light matrix, frames since, the render
    /// origin and the sun it was drawn for.
    shadow_far_cache: std::cell::Cell<(Mat4, u32, DVec3, Vec3)>,
    /// The same for the near cascade, drawn every other frame (see `render_inner`).
    shadow_near_cache: std::cell::Cell<(Mat4, u32, DVec3, Vec3)>,
    /// Shadow atlas matrices from the first OpenXR eye, reused by the second eye.
    xr_shadow_cache: std::cell::Cell<Option<(DVec3, Vec3, Mat4, Mat4, Mat4)>>,
    /// Depth-only pipeline that fills its viewport with the far depth: clears the close
    /// cascade's part of the atlas when the near part is kept from the frame before.
    shadow_clear_pipeline: wgpu::RenderPipeline,
    /// Sort the blended draws by origin distance alone, as before the camera-enclosing
    /// objects were drawn last (only for before/after pictures, `OMSI_BLEND_AB`).
    pub blend_by_origin: bool,
    /// Draw the models' `[isshadow]` shadow blobs (see [`RenderOptions::shadow_blobs`]).
    /// Settable while the game runs, so the graphics list can switch it off at once.
    pub shadow_blobs: bool,
    /// GPU time per pass (OMSI_GPU_TIMERS, when the device has timestamp queries): the
    /// mirrors and the window's picture apart, each timed on its own.
    gpu_timers: [Option<GpuTimers>; 2],
    /// Seconds spent per stage of `render_inner` since start (OMSI_PROFILE).
    pub stats: std::cell::RefCell<std::collections::BTreeMap<&'static str, f64>>,
    /// What the window's pictures drew since start, summed (OMSI_PROFILE): instances that
    /// passed the culling and draws per pass.
    pub counts: std::cell::RefCell<std::collections::BTreeMap<&'static str, f64>>,
    profiling: bool,
    draw_audit_at: std::time::Instant,
    /// Reuse a small set of encoding workers instead of creating OS threads for each
    /// main/mirror picture. Keep these separate from simulation's worker queue.
    encoding_pool: Option<rayon::ThreadPool>,
    _device_poller: Option<DevicePoller>,
    /// Vertex data of changed meshes (skinned people, the driver) waiting for the next
    /// picture: (mesh, bytes). Written with one staging buffer and a copy each at the start
    /// of the frame - a `write_buffer` per mesh made wgpu create a staging buffer for every
    /// one of them, forty a frame with a crowd at a stop.
    pending_meshes: std::cell::RefCell<Vec<(MeshId, Vec<u8>)>>,
    /// What a freed mesh and a freed material hold (see `free_mesh`), made once.
    freed: std::cell::OnceCell<Freed>,
}

/// The placeholders freed scene slots share: an empty vertex and index buffer and a plain
/// material bind group. Making new ones for every freed slot (two buffers per mesh, a
/// buffer and a bind group per material) was most of a tile unload's time.
struct Freed {
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    buf: wgpu::Buffer,
}

pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Samples per pixel of the scene passes when nothing else is asked for (the shadow maps
/// stay single-sampled). Four samples take the staircase off every edge; the multisampled
/// colour target resolves into the window or the mirror texture at the end of the main pass.
pub const MSAA: u32 = 4;
/// Sun shadow map resolution when nothing else is asked for.
pub const SHADOW_SIZE: u32 = 2048;

/// What the renderer is built with: the user's graphics settings.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Samples per pixel: 1 (off), 2, 4 or 8.
    pub msaa: u32,
    /// Anisotropic filtering: 1 (off) .. 16.
    pub anisotropy: u16,
    /// Sun shadow map size per cascade (1024, 2048, 4096).
    pub shadow_size: u32,
    /// Screen-space ambient occlusion.
    pub ssao: bool,
    /// The 3D picture of the window drawn at this fraction of its size and scaled up
    /// (0.5..1); 0 = automatic: full size up to `AUTO_SCALE_PIXELS`, smaller above.
    pub render_scale: f32,
    /// Uncompressed texture files (BMP, TGA, JPG) are compressed to BC1/BC3 on loading
    /// where the device takes block formats and the result is close to the picture
    /// (DXT files always go up as blocks there).
    pub compress_textures: bool,
    /// FXAA over the enhanced path's tone-mapped picture.
    pub fxaa: bool,
    /// The original's `performance_minObjSize` (see `Lighting::min_obj_size`, which may
    /// raise it for one picture, as the mirrors do).
    pub min_obj_size: f32,
    /// The original's `performance_maxObjDist` (m): objects farther away are not drawn
    /// unless they say `[noDistanceCheck]`. 0 = no limit.
    pub max_obj_dist: f32,
    /// Only the meshes the models mark `[shadow]` cast sun shadows, as in OMSI 2 (else every
    /// solid mesh does).
    pub omsi_shadow_casters: bool,
    /// Draw the models' `[isshadow]` shadow meshes - OMSI's flat blob under a vehicle,
    /// standing in for the sky light the body keeps off the road (see [`Instance::blob`]).
    /// Off, the sun shadow map is all the shading under a vehicle, and the blob (which
    /// OMSI draws whatever the depth) cannot be seen at all.
    pub shadow_blobs: bool,
    /// The materials' reflection maps (`[matl_envmap]`: the shine of paint, chrome and
    /// glass). Off, nothing mirrors the sky photo - some players find it too strong.
    pub reflections: bool,
    /// Enhanced graphics are not asked for: a phone or OpenGL then leaves their pipelines
    /// out (they would never be drawn, and compiling the ray-marched clouds' sky killed
    /// Mali and Adreno drivers before the first frame, #364, #333, #316, #371). Asked for,
    /// they are built on every device and graphics API; a computer always builds them.
    pub no_enhanced: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            msaa: MSAA,
            anisotropy: 8,
            shadow_size: SHADOW_SIZE,
            ssao: true,
            render_scale: 0.0,
            compress_textures: true,
            fxaa: true,
            min_obj_size: 0.013,
            max_obj_dist: 0.0,
            omsi_shadow_casters: false,
            shadow_blobs: true,
            reflections: true,
            no_enhanced: false,
        }
    }
}

/// Automatic render scale: a window of up to this many pixels is drawn at full size (the
/// default 1600x900 window and a 2560x1080 screen are); a bigger one - a Retina window has
/// four times the pixels of its size in points - gets a 3D picture of about this many
/// pixels, scaled up. The HUD is always drawn at full size. Elsewhere (a desktop card on a
/// 1440p or 4K screen) the picture is drawn at full size up to 4K: scaled down to 2.8
/// million pixels, a 4K screen showed a picture of 58 % its size, and the enhanced
/// graphics looked like textures of low quality; the frame-rate governor still steps down
/// on a card that cannot keep up.
pub const AUTO_SCALE_PIXELS: f32 = if cfg!(target_os = "macos") || cfg!(target_os = "android") { 2_800_000.0 } else { 8_400_000.0 };

/// Interior lamps one mesh may be lit by (OMSI: four; a model may list more in its
/// `[illumination_interior]`), and the step of the lamp code sent to the shaders (`first *
/// stride + count`, exact in the f32 it travels in for a quarter of a million lamp slots).
pub const MAX_LAMPS_PER_MESH: u32 = 63;
pub const LAMP_CODE_STRIDE: u32 = 64;

/// The enhanced pass's second target: r is 1 where the bus's own screens are
/// (`MaterialExtra::screen`), 0 elsewhere - the glow takes no light from them and FXAA
/// passes them through; g is 1 on an LED panel's own dots (`MaterialExtra::led`), which the
/// glow's source keeps and multiplies up (see `post.wgsl`). b carries the puddle's
/// reflected-light weight; a is blend coverage. Sharing this attachment avoids another
/// geometry pass or a full normal/material buffer just for water.
const MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The colour targets of a pipeline drawing into `format`: Enhanced and classic puddle
/// shading draw into `HDR_FORMAT` with the screen mask beside it,
/// written by the scene's own shader only (`mask`), coverage-blended where the colour is.
fn color_targets(format: wgpu::TextureFormat, blend: Option<wgpu::BlendState>, write: wgpu::ColorWrites, mask: bool) -> Vec<Option<wgpu::ColorTargetState>> {
    let mut v = vec![Some(wgpu::ColorTargetState { format, blend, write_mask: write })];
    if format == HDR_FORMAT {
        v.push(Some(wgpu::ColorTargetState {
            format: MASK_FORMAT,
            blend: blend.map(|_| wgpu::BlendState {
                color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::SrcAlpha, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
                alpha: wgpu::BlendComponent::REPLACE,
            }),
            write_mask: if mask { wgpu::ColorWrites::ALL } else { wgpu::ColorWrites::empty() },
        }));
    }
    v
}
/// Half size of the area around the camera covered by the near shadow cascade (m).
pub const SHADOW_RANGE: f32 = 140.0;
/// Half size of the far cascade (m): coarser, but reaches the whole visible street.
pub const SHADOW_RANGE_FAR: f32 = 700.0;
/// Half size of the close cascade around the camera (m). The near cascade's texel is
/// 280 m / 2048 = 14 cm, and a moving bus's shadow edge stepped from texel to texel: the
/// shadow trembled while driving. The close map (the right half of the near map's atlas)
/// has 3 cm texels for the bus and everything next to it.
pub const SHADOW_RANGE_CLOSE: f32 = 32.0;
/// The close cascade's map is drawn no bigger than this (texels a side), whatever the shadow
/// setting: 3 cm texels. At the 4096 setting it had been 1.6 cm, and drawing it was a third
/// of the shadow pass's 4.6 ms.
const SHADOW_CLOSE_MAX: u32 = 2048;

impl Renderer {
    /// Create a renderer with the default options. `surface` is used to pick a compatible
    /// adapter and format.
    pub async fn new(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        format: Option<wgpu::TextureFormat>,
    ) -> Result<Renderer> {
        Self::new_with(instance, surface, format, RenderOptions::default()).await
    }

    /// Create a renderer with the given graphics settings.
    pub async fn new_with(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        format: Option<wgpu::TextureFormat>,
        options: RenderOptions,
    ) -> Result<Renderer> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: surface, force_fallback_adapter: false, ..Default::default() })
            .await
            .map_err(|e| anyhow!("no graphics adapter that can draw the game was found (Metal, Vulkan, DirectX 12 or OpenGL 3.3 or later); updating the graphics driver often helps: {e}"))?;
        Self::new_on(adapter, surface, format, options).await
    }

    /// The adapters of `instance` that can show `surface`, the ones worth trying first first:
    /// a graphics card of its own, then the processor's graphics, then anything else (a
    /// software renderer last).
    pub fn adapters_for(instance: &wgpu::Instance, surface: &wgpu::Surface<'_>) -> Vec<wgpu::Adapter> {
        let mut v: Vec<wgpu::Adapter> = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
            .into_iter()
            .filter(|a| a.is_surface_supported(surface))
            .collect();
        let rank = |a: &wgpu::Adapter| match a.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 0,
            wgpu::DeviceType::IntegratedGpu => 1,
            wgpu::DeviceType::VirtualGpu | wgpu::DeviceType::Other => 2,
            wgpu::DeviceType::Cpu => 3,
        };
        v.sort_by_key(rank);
        v
    }

    /// Create a renderer on this adapter.
    pub async fn new_on(
        adapter: wgpu::Adapter,
        surface: Option<&wgpu::Surface<'_>>,
        format: Option<wgpu::TextureFormat>,
        options: RenderOptions,
    ) -> Result<Renderer> {
        let info = adapter.get_info();
        // test hooks for an adapter that cannot be opened: an error, or wgpu going down
        match omsi_cfg::env::var("OMSI_FAKE_GPU_ERROR").as_deref() {
            Ok("open") => return Err(anyhow!("test: {} refused (OMSI_FAKE_GPU_ERROR=open)", info.name)),
            Ok("open-panic") => panic!("test: {} went down while being opened (OMSI_FAKE_GPU_ERROR=open-panic)", info.name),
            _ => {}
        }
        // What the textures may take on this adapter (wgpu does not tell a card's memory):
        // a discrete card is taken for one of 2-4 GB, whose rest the pictures (the render
        // targets, the shadow maps) and the driver need; an integrated one shares the
        // system's memory, Apple's generously
        let vram = dedicated_vram_mb(&info);
        let guess_mb: u64 = match info.device_type {
            // (a card of 2 or 3 GB, where Windows says: half of it - 1600 MB of a GTX 1050's
            // 2 GB left too little for the rest, and its Vulkan device was lost at the start;
            // a card of 2 GB a third of it - with half, 4x MSAA, SSAO and the shadows its
            // DirectX 12 device still ran out of memory on Grundorf within seconds, #114)
            wgpu::DeviceType::DiscreteGpu => vram.filter(|v| *v >= 512).map_or(1600, |v| if v <= 2560 { v * 35 / 100 } else { (v / 2).min(1600) }),
            wgpu::DeviceType::IntegratedGpu if info.backend == wgpu::Backend::Metal => 3000,
            wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::VirtualGpu => 1000,
            _ => 800,
        };
        ADAPTER_TEXTURE_MB.store(guess_mb, std::sync::atomic::Ordering::Relaxed);
        log::info!("graphics adapter: {} ({:?}, {:?}{}), texture memory taken for it: {guess_mb} MB", info.name, info.device_type, info.backend, vram.map(|v| format!(", {v} MB of its own")).unwrap_or_default());
        // The legacy Intel Windows Vulkan branch has repeatedly crashed inside igvk64.dll
        // while compiling the larger multisampled/SSAO pipeline set. This is a driver access
        // violation, so wgpu cannot turn it into a recoverable error. Start those adapters
        // with the conservative feature set instead. OMSI_INTEL_FULL_GPU=1 is useful for
        // retesting after a driver update without rebuilding the game.
        let intel_vulkan_safe = cfg!(windows)
            && info.backend == wgpu::Backend::Vulkan
            && info.vendor == 0x8086
            && omsi_cfg::env::var_os("OMSI_INTEL_FULL_GPU").is_none();
        let options = if intel_vulkan_safe {
            log::warn!(
                "Intel Vulkan adapter detected ({}): using the stable driver profile (1x MSAA, 1x anisotropy, SSAO and runtime texture compression off); set OMSI_INTEL_FULL_GPU=1 after updating the Intel driver to retry the requested settings",
                info.name
            );
            RenderOptions {
                msaa: 1,
                anisotropy: 1,
                ssao: false,
                compress_textures: false,
                ..options
            }
        } else {
            options
        };
        // A small or shared graphics chip (the processor's graphics outside a Mac, a phone,
        // a card of up to 2.5 GB, anything on OpenGL) gets a lighter picture whatever the
        // settings ask: no SSAO and no multisampling, smaller shadow maps; a card of up to
        // 4 GB no SSAO and at most 2x. (The settings' "High" on such a machine ran out of
        // memory or at a dozen frames a second.) OMSI_FULL_GPU=1 asks for the settings as
        // they are.
        GL_BACKEND.store(info.backend == wgpu::Backend::Gl, std::sync::atomic::Ordering::Relaxed);
        let full = omsi_cfg::env::var_os("OMSI_FULL_GPU").is_some();
        let weak = !full
            && (info.backend == wgpu::Backend::Gl
                // (a phone's chip, whatever type its driver reports: some say "other")
                || cfg!(target_os = "android")
                || (info.device_type == wgpu::DeviceType::IntegratedGpu && info.backend != wgpu::Backend::Metal)
                || vram.is_some_and(|v| v <= 2560));
        let modest = !full && !weak && vram.is_some_and(|v| v <= 4200);
        let options = if weak {
            log::warn!("{}: a small or shared graphics chip - no SSAO, no MSAA, shadow maps of at most 1024 (OMSI_FULL_GPU=1 keeps the settings)", info.name);
            RenderOptions { msaa: 1, ssao: false, shadow_size: options.shadow_size.min(1024), ..options }
        } else if modest {
            log::info!("{}: {} MB of its own - no SSAO, at most 2x MSAA and 2048 shadow maps (OMSI_FULL_GPU=1 keeps the settings)", info.name, vram.unwrap_or(0));
            RenderOptions { msaa: options.msaa.min(2), ssao: false, shadow_size: options.shadow_size.min(2048), ..options }
        } else {
            options
        };
        let shadow_size = options
            .shadow_size
            .clamp(512, if intel_vulkan_safe { 2048 } else { 8192 });
        let mut limits = wgpu::Limits::default().using_resolution(adapter.limits());
        if intel_vulkan_safe {
            // Do not request every large limit the Intel driver advertises. In particular,
            // asking for its maximum storage-buffer and buffer sizes makes 31.0.101.2141
            // crash in vkCreateDevice instead of returning a VkResult. The WebGPU defaults
            // are ample for the renderer and stay on the driver's well-tested path.
            limits = wgpu::Limits::default().using_resolution(adapter.limits());
        } else {
            limits.max_storage_buffer_binding_size =
                adapter.limits().max_storage_buffer_binding_size;
            limits.max_buffer_size = adapter.limits().max_buffer_size;
        }
        // an older or smaller graphics chip (an OpenGL one, a GT 530) does not reach the
        // WebGPU defaults: asked for them anyway, the device was never opened
        if !limits.check_limits(&adapter.limits()) {
            log::warn!("{}: below the standard limits; using what it has", info.name);
            limits = adapter.limits();
        }
        // OMSI_GPU_LIMITS=default|downlevel: the WebGPU defaults (or the downlevel ones) and
        // nothing more, whatever this machine could do - to find what a stricter driver refuses
        match omsi_cfg::env::var("OMSI_GPU_LIMITS").as_deref() {
            Ok("default") => limits = wgpu::Limits::default(),
            Ok("downlevel") => limits = wgpu::Limits::downlevel_defaults(),
            _ => {}
        }
        // the shadow atlas is two maps wide: no wider than the card draws
        let shadow_size = shadow_size.min(limits.max_texture_dimension_2d / 2).max(256);
        let format = format
            .or_else(|| {
                surface.map(|s| {
                    let formats = s.get_capabilities(&adapter).formats;
                    // (an Android driver lists the plain RGBA8 first: the picture, written in
                    // linear light for an sRGB target, came out dark and flat)
                    if cfg!(target_os = "android") {
                        if let Some(f) = formats.iter().find(|f| f.is_srgb()) {
                            return *f;
                        }
                    }
                    formats[0]
                })
            })
            .unwrap_or(wgpu::TextureFormat::Rgba8UnormSrgb);
        // Every target the scene is drawn into with multisampling (the swap chain or mirror
        // format, the HDR target of the enhanced path and its screen mask, the depth buffer)
        // must take the sample count, and the colour targets must resolve. A device judges
        // that by the WebGPU table, which promises only 1x and 4x, unless it was asked for
        // the adapter's own table: the launcher's "2x MSAA" (which Apple GPUs do support)
        // was a fatal validation error before the first frame because the adapter's table
        // said yes and the device's said no. So the adapter's table is asked for when the
        // wanted count needs it (2x, 8x), and the count is checked against the table the
        // device will use.
        let wanted = match options.msaa {
            1 | 2 | 4 | 8 => options.msaa,
            _ => MSAA,
        };
        let targets = [format, wgpu::TextureFormat::Rgba16Float, DEPTH_FORMAT, MASK_FORMAT];
        let takes = |flags: wgpu::TextureFormatFeatureFlags, f: wgpu::TextureFormat, n: u32| {
            flags.sample_count_supported(n)
                && (n == 1
                    || f.is_depth_stencil_format()
                    || flags.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_RESOLVE))
        };
        let adapter_table_needed = !targets.iter().all(|&f| {
            takes(
                f.guaranteed_format_features(wgpu::Features::empty()).flags,
                f,
                wanted,
            )
        });
        let mut required_features = if adapter_table_needed && info.backend != wgpu::Backend::Noop {
            adapter.features() & wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
        } else {
            wgpu::Features::empty()
        };
        if omsi_cfg::env::var_os("OMSI_GPU_TIMERS").is_some() {
            required_features |= adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
        }
        // DXT textures stay compressed on the GPU where it takes them (Apple silicon does);
        // OMSI_NO_BC=1 uploads everything as RGBA (the old way, for comparisons)
        if omsi_cfg::env::var_os("OMSI_NO_BC").is_none() {
            required_features |= adapter.features() & wgpu::Features::TEXTURE_COMPRESSION_BC;
        }
        if intel_vulkan_safe {
            // Keep vkCreateDevice entirely free of optional extensions. Compressed source
            // textures are decoded to RGBA by upload_texture on this device.
            required_features = wgpu::Features::empty();
        }
        // the per-draw arrays as storage buffers where the device reads them in a vertex
        // shader (three there, two lights arrays in a fragment shader), else as textures;
        // OMSI_GPU_ARRAYS=textures|nostorage takes those paths on any device
        let downlevel = adapter.get_downlevel_capabilities().flags;
        let storage = limits.max_storage_buffers_per_shader_stage;
        let path = match omsi_cfg::env::var("OMSI_GPU_ARRAYS").as_deref() {
            Ok("textures") => ArrayPath::VertexTextures,
            Ok("nostorage") => ArrayPath::NoStorage,
            _ if !downlevel.contains(wgpu::DownlevelFlags::FRAGMENT_STORAGE) || storage < 2 => ArrayPath::NoStorage,
            _ if !downlevel.contains(wgpu::DownlevelFlags::VERTEX_STORAGE) || storage < 3 => ArrayPath::VertexTextures,
            _ => ArrayPath::Storage,
        };
        ARRAY_PATH.store(path as u8, std::sync::atomic::Ordering::Relaxed);
        match path {
            ArrayPath::Storage => {}
            ArrayPath::VertexTextures => log::warn!("{}: no storage buffers in vertex shaders; the scene's arrays are read from textures", info.name),
            ArrayPath::NoStorage => log::warn!("{}: no storage buffers; the scene's arrays are read from textures and the lamps light no pixels of their own", info.name),
        }
        log::info!("opening graphics device: {} ({:?}, vendor {:#06x}, device {:#06x}), features {:?}, max buffer {} MB, max storage binding {} MB", info.name, info.backend, info.vendor, info.device, required_features, limits.max_buffer_size / 1_000_000, limits.max_storage_buffer_binding_size as u64 / 1_000_000);
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("omsi"),
                required_features,
                required_limits: limits,
                // (a card of up to 4 GB gets the allocator's small blocks: the large ones
                // left hundreds of MB reserved and unused, and 2 GB cards lost the device to
                // "Out of memory" in the first frames with the textures well under budget,
                // #332, #295)
                memory_hints: if weak || modest || vram.is_some_and(|v| v <= 4200) { wgpu::MemoryHints::MemoryUsage } else { wgpu::MemoryHints::Performance },
                ..Default::default()
            })
            .await
            .context("request_device")?;
        log::info!("graphics device opened; compiling renderer pipelines");
        // the same choice wgpu-core makes when it validates a texture or a pipeline
        let adapter_table = device
            .features()
            .contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES)
            || !adapter
                .get_downlevel_capabilities()
                .flags
                .contains(wgpu::DownlevelFlags::WEBGPU_TEXTURE_FORMAT_SUPPORT);
        let flags_of = |f: wgpu::TextureFormat| {
            if adapter_table {
                adapter.get_texture_format_features(f).flags
            } else {
                f.guaranteed_format_features(device.features()).flags
            }
        };
        let supported = |n: u32| targets.iter().all(|&f| takes(flags_of(f), f, n));
        let msaa = [wanted, 8, 4, 2, 1]
            .into_iter()
            .filter(|&n| n <= wanted)
            .find(|&n| supported(n))
            .unwrap_or(1);
        if msaa != wanted {
            log::warn!("{}x MSAA is not supported by {} (the device takes {:?} samples for {:?}, {:?} for the HDR target, {:?} for depth); using {}x", wanted, info.name, flags_of(format).supported_sample_counts(), format, flags_of(targets[1]).supported_sample_counts(), flags_of(DEPTH_FORMAT).supported_sample_counts(), msaa);
        }
        let options = RenderOptions {
            msaa,
            shadow_size,
            // (16x was held at 8x once, for lane-line dashes far down a road seen to alias
            // into two streaks running apart like an arrow; Spandau's Heerstrasse at 8x and
            // 16x showed none of it - 16x kept the far dashes narrow where 8x smeared them
            // sideways - so 16x is the player's choice again, 8x the default)
            anisotropy: options.anisotropy.clamp(1, 16),
            ..options
        };
        let bc = device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_BC);
        let compress =
            bc && options.compress_textures && omsi_cfg::env::var_os("OMSI_NO_TEXCOMPRESS").is_none();
        omsi_texture::set_gpu_options(omsi_texture::GpuOptions { bc, compress });
        log::info!("renderer: {} ({:?}), {:?}, {}x MSAA{}, anisotropy {}, shadow map {}, SSAO {}, render scale {}, textures {}", info.name, info.backend, format, options.msaa, if adapter_table { " (adapter format table)" } else { "" }, options.anisotropy, options.shadow_size, options.ssao, if options.render_scale > 0.0 { format!("{:.2}", options.render_scale.clamp(0.5, 1.0)) } else { "auto".to_string() }, match (bc, compress) { (false, _) => "RGBA (no BC on this device)", (true, false) => "DXT as blocks, others RGBA", (true, true) => "DXT as blocks, others compressed where close" });
        // Anything that still fails to validate with multisampling (a driver whose table
        // promises more than it takes) is caught here, and the renderer is built again
        // without it instead of the default handler's abort.
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let renderer = Self::build(
            device.clone(),
            queue.clone(),
            format!("{} ({:?})", info.name, info.backend),
            format,
            options,
        );
        match scope.pop().await {
            None => Ok(renderer),
            Some(e) if options.msaa > 1 => {
                log::error!(
                    "{}x MSAA failed on {}: {}; drawing without multisampling",
                    options.msaa,
                    info.name,
                    gpu_error_text(&e)
                );
                drop(renderer);
                Ok(Self::build(
                    device,
                    queue,
                    format!("{} ({:?})", info.name, info.backend),
                    format,
                    RenderOptions { msaa: 1, ..options },
                ))
            }
            Some(e) => Err(anyhow!("renderer pipelines: {}", gpu_error_text(&e))),
        }
    }

    /// The pipelines, samplers and fixed textures of a renderer whose options are settled.
    fn build(
        device: wgpu::Device,
        queue: wgpu::Queue,
        adapter_name: String,
        format: wgpu::TextureFormat,
        options: RenderOptions,
    ) -> Renderer {
        let (msaa, shadow_size) = (options.msaa, options.shadow_size);
        // A GPU error while multisampling is on is logged and switches multisampling off
        // at the next frame (`render_inner`). Otherwise it is logged and the game goes on:
        // wgpu's own handler ends the process, and one call a driver refused (a limit of
        // that card, a bigger map than the last) closed the game a few seconds into the
        // drive - a wrong picture for a frame is better than no game. The first errors and
        // then every thousandth reach the log.
        let gpu_error = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let out_of_memory = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let device_lost: Arc<std::sync::Mutex<Option<String>>> = Default::default();
        {
            let lost = device_lost.clone();
            device.set_device_lost_callback(move |reason, message| {
                // (dropping the device at the end also calls this: that is no loss)
                if matches!(reason, wgpu::DeviceLostReason::Destroyed) {
                    return;
                }
                log::error!("the graphics device was lost ({reason:?}): {message}");
                *lost.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{reason:?}: {message}"));
            });
        }
        {
            let flag = gpu_error.clone();
            let oom = out_of_memory.clone();
            let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
            device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                if matches!(e, wgpu::Error::OutOfMemory { .. }) {
                    oom.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                let n = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if msaa > 1 && !flag.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    log::error!("GPU error with {msaa}x MSAA (drawing without it from now on): {}", gpu_error_text(&e));
                } else if n < 20 || n % 1000 == 0 {
                    log::error!("GPU error #{} (the game goes on): {}", n + 1, gpu_error_text(&e));
                }
            }));
        }
        if omsi_cfg::env::var("OMSI_FAKE_GPU_ERROR").as_deref() == Ok("build") && msaa > 1 {
            // test hook for the fallback in `new_with`: a sample count no device takes
            let _ = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("invalid"),
                size: wgpu::Extent3d {
                    width: 4,
                    height: 4,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 3,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
        }
        // One module for both paths: the enhanced fragment shader shares the vertex shader,
        // which the depth prepass relies on to the last bit (see `VsOut::clip`).
        log::info!("renderer: compiling the scene shaders");
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("omsi"),
            source: wgpu::ShaderSource::Wgsl(
                scene_shader_source(GL_BACKEND.load(std::sync::atomic::Ordering::Relaxed)).into(),
            ),
        });
        let shadow_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow camera"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                array_layout_entry(1, wgpu::ShaderStages::VERTEX, true),
                array_layout_entry(2, wgpu::ShaderStages::VERTEX, true),
                array_layout_entry(10, wgpu::ShaderStages::VERTEX, false),
            ],
        });
        let mut camera_entries = vec![
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                array_layout_entry(1, wgpu::ShaderStages::VERTEX, true),
                array_layout_entry(2, wgpu::ShaderStages::VERTEX, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 9,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                array_layout_entry(10, wgpu::ShaderStages::VERTEX, false),
                // enhanced: its lighting and a clamped linear sampler (its textures: below)
                wgpu::BindGroupLayoutEntry {
                    binding: 11,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 13,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // the tile light maps around the camera, and where they lie
                wgpu::BindGroupLayoutEntry {
                    binding: 18,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 19,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
        ];
        // the enhanced path's textures (see `camera_enhanced_textures`)
        if camera_enhanced_textures() {
            for (binding, view_dimension) in [(12, wgpu::TextureViewDimension::Cube), (14, wgpu::TextureViewDimension::D2), (17, wgpu::TextureViewDimension::Cube)] {
                camera_entries.push(wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension, multisampled: false },
                    count: None,
                });
            }
        }
        // the point lights and their grid (see `ArrayPath::NoStorage`)
        if array_path() != ArrayPath::NoStorage {
            camera_entries.push(array_layout_entry(3, wgpu::ShaderStages::FRAGMENT, true));
            camera_entries.push(array_layout_entry(4, wgpu::ShaderStages::FRAGMENT, false));
        }
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera"),
            entries: &camera_entries,
        });
        let lm_atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("light map atlas"),
            size: wgpu::Extent3d { width: LM_ATLAS_TILES * LM_TILE_PX, height: LM_ATLAS_TILES * LM_TILE_PX, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let lm_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("light map atlas place"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let material_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("material"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // the PBR set beside the diffuse texture: normal map, occlusion/roughness/metal
                wgpu::BindGroupLayoutEntry {
                    binding: 9,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 10,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // Tile masks/light maps clamp independently of repeating ground textures.
                wgpu::BindGroupLayoutEntry {
                    binding: 11,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // coronas: camera group + a corona texture group
        let corona_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("corona"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("omsi"),
            bind_group_layouts: &[Some(&camera_layout), Some(&material_layout)],
            immediate_size: 0,
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
        };
        let make = |format: wgpu::TextureFormat,
                    fs: &str,
                    blend: Option<wgpu::BlendState>,
                    depth_write: bool,
                    cull: bool,
                    bias: i32,
                    alpha_to_coverage: bool,
                    samples: u32| {
            let use_alpha_to_coverage = alpha_to_coverage && samples > 1;
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("omsi"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[vertex_layout.clone()],
                    compilation_options: Default::default(),
                },
                primitive: one_sided_primitive(cull),
                // Reversed Z: the near plane is 1 and the far plane 0, so nearer means
                // greater. The depth bias keeps its meaning (negative = towards the viewer)
                // only if its sign is turned around with the axis.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(depth_write),
                    depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                    stencil: Default::default(),
                    bias: wgpu::DepthBiasState {
                        constant: -bias,
                        slope_scale: if bias != 0 {
                            -bias.signum() as f32 * 2.0
                        } else {
                            0.0
                        },
                        clamp: 0.0,
                    },
                }),
                multisample: wgpu::MultisampleState {
                    count: samples,
                    mask: !0,
                    alpha_to_coverage_enabled: use_alpha_to_coverage,
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    targets: &color_targets(
                        format,
                        blend,
                        if fs == "fs_surface_depth" { wgpu::ColorWrites::empty() } else { wgpu::ColorWrites::ALL },
                        fs != "fs_surface_depth",
                    ),
                    // only the alpha-tested pipelines keep their `discard` (see ALPHA_TEST
                    // in shader.wgsl): early depth testing for everything else
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants: &[
                            ("ALPHA_TEST", if alpha_to_coverage { 1.0 } else { 0.0 }),
                            (
                                "ALPHA_TO_COVERAGE",
                                if use_alpha_to_coverage { 1.0 } else { 0.0 },
                            ),
                        ],
                        ..Default::default()
                    },
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let bias: i32 = omsi_cfg::env::var("OMSI_SURFACE_BIAS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(-24);
        // one pipeline per `pipe_code`: the kind decides blending and the depth write
        let scene_pipelines = |f: wgpu::TextureFormat, fs: &str, samples: u32| -> Vec<wgpu::RenderPipeline> {
            let mut out = Vec::with_capacity(PIPE_KINDS as usize * 4);
            for kind in 0..PIPE_KINDS {
                let blend = (kind == PIPE_BLEND || kind == PIPE_BLEND_NO_WRITE)
                    .then_some(wgpu::BlendState::ALPHA_BLENDING);
                let depth_write = kind != PIPE_BLEND_NO_WRITE;
                for cull in [false, true] {
                    for surface in [false, true] {
                        out.push(make(
                            f,
                            if kind == PIPE_SURFACE_DEPTH { "fs_surface_depth" } else { fs },
                            blend,
                            depth_write,
                            cull,
                            if surface { bias } else { 0 },
                            kind == PIPE_ALPHA_TEST,
                            samples,
                        ));
                    }
                }
            }
            out
        };
        let hdr_format = wgpu::TextureFormat::Rgba16Float;
        // sun shadow map: depth only, from the light's orthographic camera
        let shadow_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shadow map"),
            size: wgpu::Extent3d {
                // near cascade on the left, close cascade on the right
                width: shadow_size * 2,
                height: shadow_size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let shadow_view = shadow_tex.create_view(&Default::default());
        let shadow_tex_far = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shadow map far"),
            size: wgpu::Extent3d {
                width: shadow_size,
                height: shadow_size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let shadow_view_far = shadow_tex_far.create_view(&Default::default());
        let shadow_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        let shadow_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("shadow"),
                bind_group_layouts: &[Some(&shadow_layout), Some(&material_layout)],
                immediate_size: 0,
            });
        let make_shadow = |kind: u8, cascade: u8| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("shadow"),
                layout: Some(&shadow_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(match cascade {
                        0 => "vs_shadow",
                        1 => "vs_shadow_far",
                        _ => "vs_shadow_close",
                    }),
                    buffers: &[vertex_layout.clone()],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    front_face: wgpu::FrontFace::Cw,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: wgpu::DepthBiasState {
                        constant: 4,
                        slope_scale: 3.0,
                        clamp: 0.0,
                    },
                }),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(if kind == PIPE_ALPHA_TEST { "fs_shadow_test" } else { "fs_shadow" }),
                    targets: &[],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let shadow_clear_pipeline = {
            log::info!("renderer: compiling the shadow clear shader");
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("shadow clear"),
                source: wgpu::ShaderSource::Wgsl(
                    "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
                        let x = f32(i32(i & 1u) * 4 - 1);
                        let y = f32(i32(i >> 1u) * 4 - 1);
                        return vec4<f32>(x, y, 1.0, 1.0);
                    }"
                    .into(),
                ),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("shadow clear"),
                bind_group_layouts: &[],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("shadow clear"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                fragment: None,
                multiview_mask: None,
                cache: None,
            })
        };
        let shadow_pipelines = [
            make_shadow(PIPE_OPAQUE, 0),
            make_shadow(PIPE_ALPHA_TEST, 0),
            make_shadow(PIPE_OPAQUE, 1),
            make_shadow(PIPE_ALPHA_TEST, 1),
            make_shadow(PIPE_OPAQUE, 2),
            make_shadow(PIPE_ALPHA_TEST, 2),
        ];
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: options.anisotropy,
            ..Default::default()
        });
        // [matl_texadress_clamp]: a number plate is a small quad whose texture must not
        // repeat beyond its edge - repeated, the plate text tiled the whole rear of the bus
        let clamp_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: options.anisotropy,
            ..Default::default()
        });
        let mirror_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::MirrorRepeat,
            address_mode_v: wgpu::AddressMode::MirrorRepeat,
            address_mode_w: wgpu::AddressMode::MirrorRepeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: options.anisotropy,
            ..Default::default()
        });
        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera"),
            size: std::mem::size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let white = omsi_texture::Image::solid([255, 255, 255, 255]);
        let white_texture = upload_texture(&device, &queue, &white, false);
        let black_texture = upload_texture(
            &device,
            &queue,
            &omsi_texture::Image::solid([0, 0, 0, 255]),
            false,
        );
        let flat_normal_texture = upload_texture(&device, &queue, &omsi_texture::Image::solid([128, 128, 255, 255]), false);
        // corona sprite: soft radial falloff
        let cs = 64u32;
        let mut corona_img = omsi_texture::Image {
            width: cs,
            height: cs,
            rgba: vec![0; (cs * cs * 4) as usize],
            has_alpha: false,
        };
        for y in 0..cs {
            for x in 0..cs {
                let dx = (x as f32 + 0.5) / cs as f32 * 2.0 - 1.0;
                let dy = (y as f32 + 0.5) / cs as f32 * 2.0 - 1.0;
                let r = (dx * dx + dy * dy).sqrt();
                let v = ((1.0 - r).max(0.0)).powf(1.6) * 255.0;
                let o = ((y * cs + x) * 4) as usize;
                corona_img.rgba[o..o + 4].copy_from_slice(&[v as u8, v as u8, v as u8, 255]);
            }
        }
        let corona_texture = upload_texture(&device, &queue, &corona_img, false);
        let corona_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let corona_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("corona"),
            layout: &corona_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&corona_texture.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&corona_sampler),
                },
            ],
        });
        // a soft grey puff until the app hands over the game's own `Texture/rauch.tga`
        let mut puff = omsi_texture::Image { width: cs, height: cs, rgba: vec![0; (cs * cs * 4) as usize], has_alpha: true };
        for y in 0..cs {
            for x in 0..cs {
                let dx = (x as f32 + 0.5) / cs as f32 * 2.0 - 1.0;
                let dy = (y as f32 + 0.5) / cs as f32 * 2.0 - 1.0;
                let a = (1.0 - (dx * dx + dy * dy).sqrt()).max(0.0).powf(1.2) * 255.0;
                let o = ((y * cs + x) * 4) as usize;
                puff.rgba[o..o + 4].copy_from_slice(&[255, 255, 255, a as u8]);
            }
        }
        let puff_texture = upload_texture(&device, &queue, &puff, false);
        let smoke_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("smoke"),
            layout: &corona_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&puff_texture.view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&corona_sampler) },
            ],
        });
        drop(puff_texture);
        log::info!("renderer: compiling the coronas shaders");
        let corona_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("corona"),
            source: wgpu::ShaderSource::Wgsl(corona_shader_source().into()),
        });
        let corona_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("corona"),
            bind_group_layouts: &[Some(&camera_layout), Some(&corona_layout)],
            immediate_size: 0,
        });
        let corona_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuCorona>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4],
        };
        let additive = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::REPLACE,
        };
        // Omsi's lamp sprites: SRCBLEND ONE, DESTBLEND INVSRCCOLOR (src + dst * (1 - src)),
        // which keeps a coloured sprite's hue over a lit background instead of washing it to white
        let screen = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrc,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::REPLACE,
        };
        let alpha_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::REPLACE,
        };
        let corona_pipeline_for = |f: wgpu::TextureFormat, fs: &str, blend: wgpu::BlendState| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("corona"),
                layout: Some(&corona_pl),
                vertex: wgpu::VertexState {
                    module: &corona_shader,
                    entry_point: Some("vs_main"),
                    buffers: &[corona_vertex.clone()],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    front_face: wgpu::FrontFace::Ccw,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: msaa,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                fragment: Some(wgpu::FragmentState {
                    module: &corona_shader,
                    entry_point: Some(fs),
                    targets: &color_targets(f, Some(blend), wgpu::ColorWrites::COLOR, false),
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        drop(corona_texture);
        // sky dome
        let sky_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sky"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let (cloud_shape_view, cloud_detail_view, cloud_sampler) = cloud_noise_textures(&device, &queue);
        log::info!("renderer: compiling the sky and clouds shaders");
        let sky_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sky"),
            source: wgpu::ShaderSource::Wgsl(sky_shader_source().into()),
        });
        let sky_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sky"),
            bind_group_layouts: &[Some(&camera_layout), Some(&sky_layout)],
            immediate_size: 0,
        });
        let sky_vertex = wgpu::VertexBufferLayout {
            array_stride: 12,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3],
        };
        let sky_pipeline_for = |f: wgpu::TextureFormat, fs: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("sky"),
                layout: Some(&sky_pl),
                vertex: wgpu::VertexState {
                    module: &sky_shader,
                    entry_point: Some("vs_main"),
                    buffers: &[sky_vertex.clone()],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    front_face: wgpu::FrontFace::Ccw,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: msaa,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                fragment: Some(wgpu::FragmentState {
                    module: &sky_shader,
                    entry_point: Some(fs),
                    targets: &color_targets(f, None, wgpu::ColorWrites::COLOR, false),
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let pass = PassPipelines {
            pipelines: scene_pipelines(format, "fs_main", msaa),
            rain_pipelines: scene_pipelines(format, "fs_main", 1),
            corona_pipeline: corona_pipeline_for(format, "fs_main", screen),
            smoke_pipeline: corona_pipeline_for(format, "fs_smoke", alpha_blend),
            sky_pipeline: sky_pipeline_for(format, "fs_main"),
        };
        // the enhanced path: its own lighting in all three
        let leave_out_enhanced = options.no_enhanced && (cfg!(target_os = "android") || adapter_name.to_ascii_lowercase().contains("opengl") || GL_BACKEND.load(std::sync::atomic::Ordering::Relaxed)) || !camera_enhanced_textures();
        let hdr_pass = (!leave_out_enhanced).then(|| PassPipelines {
            pipelines: scene_pipelines(hdr_format, "fs_enhanced", msaa),
            rain_pipelines: scene_pipelines(hdr_format, "fs_enhanced", 1),
            corona_pipeline: corona_pipeline_for(hdr_format, "fs_enhanced", additive),
            smoke_pipeline: corona_pipeline_for(hdr_format, "fs_smoke_enhanced", alpha_blend),
            sky_pipeline: sky_pipeline_for(hdr_format, "fs_enhanced"),
        });
        let reflection_pass = (!leave_out_enhanced && !GL_BACKEND.load(std::sync::atomic::Ordering::Relaxed)).then(|| PassPipelines {
            pipelines: scene_pipelines(hdr_format, "fs_vanilla_reflections", msaa),
            rain_pipelines: scene_pipelines(hdr_format, "fs_vanilla_reflections", 1),
            corona_pipeline: corona_pipeline_for(hdr_format, "fs_main", screen),
            smoke_pipeline: corona_pipeline_for(hdr_format, "fs_smoke", alpha_blend),
            sky_pipeline: sky_pipeline_for(hdr_format, "fs_main"),
        });
        let sky_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        // dome: latitude rings from a little below the horizon to the zenith
        let (mut sv, mut si): (Vec<[f32; 3]>, Vec<u32>) = (Vec::new(), Vec::new());
        let (rings, segs) = (12u32, 32u32);
        for r in 0..=rings {
            let elev = -0.15 + (std::f32::consts::FRAC_PI_2 + 0.15) * r as f32 / rings as f32;
            for sgm in 0..=segs {
                let az = sgm as f32 / segs as f32 * std::f32::consts::TAU;
                sv.push([elev.cos() * az.sin(), elev.cos() * az.cos(), elev.sin()]);
            }
        }
        for r in 0..rings {
            for sgm in 0..segs {
                let a = r * (segs + 1) + sgm;
                let b = a + segs + 1;
                si.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
            }
        }
        let sky_vb = buffer_init(&device, &queue, Some("sky vb"), bytemuck::cast_slice(&sv), wgpu::BufferUsages::VERTEX);
        let sky_ib = buffer_init(&device, &queue, Some("sky ib"), bytemuck::cast_slice(&si), wgpu::BufferUsages::INDEX);
        let sky_mesh = (sky_vb, sky_ib, si.len() as u32);
        // HUD overlay quads
        let overlay_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        log::info!("renderer: compiling the overlays shaders");
        let overlay_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay"),
            source: wgpu::ShaderSource::Wgsl(include_str!("overlay.wgsl").into()),
        });
        let overlay_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("overlay"),
            bind_group_layouts: &[Some(&overlay_layout)],
            immediate_size: 0,
        });
        let premul = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::OVER,
        };
        let overlay_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("overlay"),
            layout: Some(&overlay_pl),
            vertex: wgpu::VertexState {
                module: &overlay_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                front_face: wgpu::FrontFace::Ccw,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: msaa,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            fragment: Some(wgpu::FragmentState {
                module: &overlay_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(premul),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        // ambient occlusion: a depth prepass with the camera projection, then the AO and a blur
        log::info!("renderer: compiling the SSAO shaders");
        let ssao_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ssao"),
            source: wgpu::ShaderSource::Wgsl(include_str!("ssao.wgsl").into()),
        });
        let ao_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ssao"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let ao_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ssao params"),
            size: std::mem::size_of::<SsaoUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ao_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let ao_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ssao"),
            bind_group_layouts: &[Some(&ao_layout)],
            immediate_size: 0,
        });
        let make_ao = |entry: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&ao_pl),
                vertex: wgpu::VertexState {
                    module: &ssao_shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &ssao_shader,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rg16Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let gl = GL_BACKEND.load(std::sync::atomic::Ordering::Relaxed);
        let ssao_pipeline = (!gl).then(|| make_ao("fs_ssao"));
        let blur_pipeline = (!gl).then(|| make_ao("fs_blur"));
        let prepass_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("prepass"),
            bind_group_layouts: &[Some(&camera_layout), Some(&material_layout)],
            immediate_size: 0,
        });
        // the prepass culls exactly as the main pass does: a back face that wrote depth
        // here would hide what the main pass then draws behind it
        let make_prepass_samples = |kind: u8, cull: bool, samples: u32| {
            let fragment = match kind {
                0 => "fs_shadow",
                1 => "fs_shadow_test",
                2 => "fs_transmap_depth",
                _ => unreachable!(),
            };
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("depth prepass"),
                layout: Some(&prepass_pl),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[vertex_layout.clone()],
                    compilation_options: Default::default(),
                },
                primitive: one_sided_primitive(cull),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState { count: samples, ..Default::default() },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fragment),
                    targets: &[],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let make_prepass = |kind: u8, cull: bool| make_prepass_samples(kind, cull, 1);
        let prepass_pipelines = [
            make_prepass(0, false),
            make_prepass(0, true),
            make_prepass(1, false),
            make_prepass(1, true),
            make_prepass(2, false),
            make_prepass(2, true),
        ];
        // (not on Apple's GPUs: their tile renderer drops hidden opaque fragments by itself,
        // and the extra pass only cost what it saved)
        let prepass_msaa_pipelines = (msaa > 1 && !cfg!(target_vendor = "apple")).then(|| {
            [(0, false), (0, true), (1, false), (1, true), (2, false), (2, true)]
                .map(|(kind, cull)| make_prepass_samples(kind, cull, msaa))
        });
        // --- mipmaps on the GPU: the CPU box filter took up to a second per bus spawn
        log::info!("renderer: compiling the mip maps shaders");
        let mip_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mip"),
            source: wgpu::ShaderSource::Wgsl(include_str!("mip.wgsl").into()),
        });
        let mip_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mip"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let mip_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mip"),
            bind_group_layouts: &[Some(&mip_layout)],
            immediate_size: 0,
        });
        let mip_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mip"),
            layout: Some(&mip_pl),
            vertex: wgpu::VertexState {
                module: &mip_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &mip_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let mip_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        // --- enhanced graphics: the post passes (glow, metering, adaptation, tone curve, FXAA)
        log::info!("renderer: compiling the post passes shaders");
        let post_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("post"),
            source: wgpu::ShaderSource::Wgsl(include_str!("post.wgsl").into()),
        });
        let float_tex = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let post_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                float_tex(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                float_tex(3),
                float_tex(4),
            ],
        });
        let post_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("post params"),
            size: std::mem::size_of::<PostUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let post_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let post_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("post"),
            bind_group_layouts: &[Some(&post_layout)],
            immediate_size: 0,
        });
        let post_pipeline = |entry: &str, target: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&post_pl),
                vertex: wgpu::VertexState {
                    module: &post_shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &post_shader,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let post = PostPipelines {
            down_first: post_pipeline("fs_down_first", hdr_format),
            down: post_pipeline("fs_down", hdr_format),
            up: post_pipeline("fs_up", hdr_format),
            meter: post_pipeline("fs_meter", hdr_format),
            adapt: post_pipeline("fs_adapt", hdr_format),
            tonemap: post_pipeline("fs_tonemap", format),
            tonemap_encoded: post_pipeline("fs_tonemap_encoded", wgpu::TextureFormat::Rgba8Unorm),
            fxaa: post_pipeline("fs_fxaa", format),
        };
        let one = wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        };
        let tiny = |label: &str| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: one,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: hdr_format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let meter_view = tiny("exposure meter");
        let adapt_views = [tiny("exposure a"), tiny("exposure b")];
        // adapt_bg[k] reads the meter and adapt[k] (and draws into the other one)
        let adapt_bg = [0usize, 1].map(|k| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("exposure"),
                layout: &post_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: post_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&meter_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&post_sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&adapt_views[k]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::TextureView(&white_texture.view),
                    },
                ],
            })
        });
        // --- enhanced lighting: the uniform, the sky table, the reflection probe
        let enh_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("enhanced lighting"),
            size: std::mem::size_of::<EnhancedUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sky_lut = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sky table"),
            size: wgpu::Extent3d {
                width: atmosphere::SKY_LUT_W,
                height: atmosphere::SKY_LUT_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: hdr_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let sky_lut_view = sky_lut.create_view(&Default::default());
        let lin_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let uniform_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let probe_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("probe"),
            entries: &[
                uniform_entry(0),
                uniform_entry(11),
                wgpu::BindGroupLayoutEntry {
                    binding: 13,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                float_tex(14),
                uniform_entry(15),
                wgpu::BindGroupLayoutEntry {
                    binding: 16,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::Cube,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let probe = {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("reflection probe"),
                size: wgpu::Extent3d {
                    width: PROBE_SIZE,
                    height: PROBE_SIZE,
                    depth_or_array_layers: 6,
                },
                mip_level_count: PROBE_MIPS,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: hdr_format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let cube = |base: u32, count: u32| {
                tex.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("probe cube"),
                    dimension: Some(wgpu::TextureViewDimension::Cube),
                    base_mip_level: base,
                    mip_level_count: Some(count),
                    base_array_layer: 0,
                    array_layer_count: Some(6),
                    ..Default::default()
                })
            };
            let view = cube(0, PROBE_MIPS);
            let faces: Vec<Vec<wgpu::TextureView>> = (0..PROBE_MIPS)
                .map(|m| {
                    (0..6)
                        .map(|f| {
                            tex.create_view(&wgpu::TextureViewDescriptor {
                                label: Some("probe face"),
                                dimension: Some(wgpu::TextureViewDimension::D2),
                                base_mip_level: m,
                                mip_level_count: Some(1),
                                base_array_layer: f,
                                array_layer_count: Some(1),
                                ..Default::default()
                            })
                        })
                        .collect()
                })
                .collect();
            // level 0 is drawn from the sky and reads nothing: a black cube stands in
            let dummy = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("probe placeholder"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 6,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: hdr_format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let dummy_view = dummy.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::Cube),
                ..Default::default()
            });
            let cube_tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("sky cube"),
                size: wgpu::Extent3d {
                    width: SKY_CUBE_SIZE,
                    height: SKY_CUBE_SIZE,
                    depth_or_array_layers: 6,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: hdr_format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let cube_view = cube_tex.create_view(&wgpu::TextureViewDescriptor {
                label: Some("sky cube"),
                dimension: Some(wgpu::TextureViewDimension::Cube),
                ..Default::default()
            });
            let bind_groups = (0..PROBE_MIPS)
                .map(|m| {
                    // level 0 is the sky cube (clouds and all) looked up, the others the
                    // level above blurred
                    let src = if m == 0 {
                        cube_view.clone()
                    } else {
                        cube(0, m)
                    };
                    [0u32, 3].map(|first| {
                        let rough = m as f32 / (PROBE_MIPS - 1) as f32;
                        let buf = buffer_init(&device, &queue, Some("probe pass"), bytemuck::cast_slice(&[
                                first as f32,
                                rough,
                                PROBE_SIZE as f32,
                                m as f32,
                            ]), wgpu::BufferUsages::UNIFORM);
                        device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("probe"),
                            layout: &probe_layout,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: camera_buf.as_entire_binding(),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 11,
                                    resource: enh_buf.as_entire_binding(),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 13,
                                    resource: wgpu::BindingResource::Sampler(&lin_sampler),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 14,
                                    resource: wgpu::BindingResource::TextureView(&sky_lut_view),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 15,
                                    resource: buf.as_entire_binding(),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 16,
                                    resource: wgpu::BindingResource::TextureView(&src),
                                },
                            ],
                        })
                    })
                })
                .collect();
            let probe_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("probe"),
                bind_group_layouts: &[Some(&probe_layout), Some(&sky_layout)],
                immediate_size: 0,
            });
            let target = Some(wgpu::ColorTargetState {
                format: hdr_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            });
            let probe_pipeline = |entry: &str| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&probe_pl),
                    vertex: wgpu::VertexState {
                        module: &sky_shader,
                        entry_point: Some("vs_probe"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        cull_mode: None,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: Default::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &sky_shader,
                        entry_point: Some(entry),
                        targets: &[target.clone(), target.clone(), target.clone()],
                        compilation_options: Default::default(),
                    }),
                    multiview_mask: None,
                    cache: None,
                })
            };
            let cube_faces: Vec<wgpu::TextureView> = (0..6)
                .map(|f| {
                    cube_tex.create_view(&wgpu::TextureViewDescriptor {
                        label: Some("sky cube face"),
                        dimension: Some(wgpu::TextureViewDimension::D2),
                        base_mip_level: 0,
                        mip_level_count: Some(1),
                        base_array_layer: f,
                        array_layer_count: Some(1),
                        ..Default::default()
                    })
                })
                .collect();
            // per face and redraw round: the round picks where the clouds' steps start
            let cube_bind_groups: Vec<wgpu::BindGroup> = (0..6 * SKY_CUBE_ROUNDS)
                .map(|k| {
                    let (f, round) = (k / SKY_CUBE_ROUNDS, k % SKY_CUBE_ROUNDS);
                    let buf = buffer_init(&device, &queue, Some("sky cube face"), bytemuck::cast_slice(&[f as f32, round as f32, SKY_CUBE_SIZE as f32, 0.0]), wgpu::BufferUsages::UNIFORM);
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("sky cube"),
                        layout: &probe_layout,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: camera_buf.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 11, resource: enh_buf.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 13, resource: wgpu::BindingResource::Sampler(&lin_sampler) },
                            wgpu::BindGroupEntry { binding: 14, resource: wgpu::BindingResource::TextureView(&sky_lut_view) },
                            wgpu::BindGroupEntry { binding: 15, resource: buf.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 16, resource: wgpu::BindingResource::TextureView(&dummy_view) },
                        ],
                    })
                })
                .collect();
            let cube_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("sky cube"),
                layout: Some(&probe_pl),
                vertex: wgpu::VertexState {
                    module: &sky_shader,
                    entry_point: Some("vs_probe"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &sky_shader,
                    entry_point: Some("fs_sky_cube"),
                    // a redraw is blended into what the face holds (the blend constant is
                    // the old picture's share): the clouds' grain averages out
                    targets: &[Some(wgpu::ColorTargetState {
                        format: hdr_format,
                        blend: Some(wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::OneMinusConstant,
                                dst_factor: wgpu::BlendFactor::Constant,
                                operation: wgpu::BlendOperation::Add,
                            },
                            alpha: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::OneMinusConstant,
                                dst_factor: wgpu::BlendFactor::Constant,
                                operation: wgpu::BlendOperation::Add,
                            },
                        }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            });
            Probe {
                view,
                faces,
                bind_groups,
                sky_pipeline: probe_pipeline("fs_probe_sky"),
                filter_pipeline: probe_pipeline("fs_probe_filter"),
                age: u32::MAX,
                scale: 1.0,
                cube_view,
                cube_faces,
                cube_bind_groups,
                cube_pipeline,
                cube_next: 0,
                cube_filled: false,
                cube_round: 0,
                cube_wait: 0,
                cube_eye: None,
                cube_recapture: false,
            }
        };
        let overlay_pipeline_1x = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("overlay 1x"),
            layout: Some(&overlay_pl),
            vertex: wgpu::VertexState {
                module: &overlay_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                front_face: wgpu::FrontFace::Ccw,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &overlay_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(premul),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        log::info!("renderer: compiling the VR interface shaders");
        let xr_ui_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("OpenXR spatial UI"),
            source: wgpu::ShaderSource::Wgsl(include_str!("xr_ui.wgsl").into()),
        });
        let xr_ui_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("OpenXR spatial UI"),
            layout: Some(&overlay_pl),
            vertex: wgpu::VertexState {
                module: &xr_ui_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                front_face: wgpu::FrontFace::Ccw,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &xr_ui_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(premul),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        // --- render scale: the smaller 3D picture scaled up to the window
        log::info!("renderer: compiling the upscaler shaders");
        let upscale_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("upscale"),
            source: wgpu::ShaderSource::Wgsl(include_str!("upscale.wgsl").into()),
        });
        let upscale_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("upscale"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let upscale_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("upscale"),
            bind_group_layouts: &[Some(&upscale_layout)],
            immediate_size: 0,
        });
        let upscale_pipeline_for = |entry| device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("upscale"),
            layout: Some(&upscale_pl),
            vertex: wgpu::VertexState {
                module: &upscale_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &upscale_shader,
                entry_point: Some(entry),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let upscale_pipeline = upscale_pipeline_for("fs_main");
        let copy_pipeline = upscale_pipeline_for("fs_copy");
        let panel_pipeline = upscale_pipeline_for("fs_panel");
        let upscale_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("upscale params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let gpu_timers = [GpuTimers::new(&device), GpuTimers::new(&device)];
        // GLES cannot reliably read the depth buffer (the same limitation as SSAO).
        let puddles = (!gl && !leave_out_enhanced).then(|| {
            puddles::Pipelines::new(&device, &shader, &camera_layout, &material_layout)
        });
        Renderer {
            _device_poller: DevicePoller::start(&device),
            upscale_pipeline,
            copy_pipeline,
            panel_pipeline,
            upscale_layout,
            upscale_buf,
            scale_targets: HashMap::new(),
            triple_targets: None,
            triple_culling: Default::default(),
            glass_picture: None,
            target_use: HashMap::new(),
            dynamic_scale: std::cell::Cell::new(1.0),
            flicker: std::cell::RefCell::new(HashMap::new()),
            cull_drawn: std::cell::RefCell::new(Vec::new()),
            object_sizes: Default::default(),
            object_sizes_scratch: Default::default(),
            shadow_far_cache: std::cell::Cell::new((Mat4::IDENTITY, 0, DVec3::ZERO, Vec3::ZERO)),
            shadow_near_cache: std::cell::Cell::new((Mat4::IDENTITY, 0, DVec3::ZERO, Vec3::ZERO)),
            xr_shadow_cache: std::cell::Cell::new(None),
            shadow_clear_pipeline,
            mip_pipeline,
            mip_layout,
            mip_sampler,
            clamp_sampler,
            mirror_sampler,
            address_next: std::cell::Cell::new(TexAddressing::Wrap),
            light_map_next: std::cell::Cell::new(false),
            lm_atlas_view: lm_atlas.create_view(&wgpu::TextureViewDescriptor::default()),
            lm_atlas,
            lm_uniform,
            lm_place: std::cell::Cell::new((0.0, 0.0, 0.0)),
            hdr_targets: HashMap::new(),
            puddles,
            post,
            post_layout,
            post_buf,
            post_sampler,
            meter_view,
            adapt_views,
            adapt_bg,
            adapt_front: 0,
            exposure_log: ExposureLog::new(&device),
            enh_buf,
            sky_lut,
            sky_lut_view,
            lin_sampler,
            probe: Some(probe),
            sky_state: None,
            sky_job: None,
            exposure: None,
            texture_aspect: None,
            last_frame: None,
            instant_exposure: false,
            overlay_pipeline_1x,
            xr_ui_pipeline,
            started: std::time::Instant::now(),
            ao: None,
            ao_sampler,
            ao_layout,
            ao_buf,
            prepass_pipelines,
            prepass_msaa_pipelines,
            ssao_pipeline,
            blur_pipeline,
            device,
            queue,
            adapter_name,
            camera_layout,
            material_layout,
            pass,
            hdr_pass,
            reflection_pass,
            corona_bind_group,
            smoke_bind_group,
            corona_textures: Vec::new(),
            corona_layout,
            corona_sampler,
            sky_layout,
            sky_sampler,
            cloud_shape_view,
            cloud_detail_view,
            cloud_sampler,
            sky_mesh,
            overlay_pipeline,
            overlay_layout,
            sampler,
            camera_buf,
            white_texture,
            black_texture,
            flat_normal_texture,
            format,
            depth: None,
            msaa_targets: HashMap::new(),
            shadow_view,
            shadow_view_far,
            shadow_sampler,
            shadow_layout,
            shadow_pipelines,
            shadow_blobs: options.shadow_blobs,
            options,
            gpu_error,
            env_heading: Default::default(),
            out_of_memory,
            device_lost,
            blend_by_origin: false,
            gpu_timers,
            stats: Default::default(),
            counts: Default::default(),
            profiling: omsi_cfg::env::var_os("OMSI_PROFILE").is_some(),
            draw_audit_at: std::time::Instant::now(),
            encoding_pool: if omsi_cfg::env::var_os("OMSI_NO_RENDER_POOL").is_some() {
                None
            } else {
                let workers = std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(2).clamp(2, 8);
                rayon::ThreadPoolBuilder::new().num_threads(workers)
                    .thread_name(|i| format!("omsi-render-{i}"))
                    .build().ok()
            },
            pending_meshes: Default::default(),
            freed: std::cell::OnceCell::new(),
        }
    }

    fn main_pass(&self, enhanced: bool, reflections: bool) -> &PassPipelines {
        if enhanced { self.hdr_pass.as_ref().unwrap() }
        else if reflections { self.reflection_pass.as_ref().unwrap() }
        else { &self.pass }
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// The fraction of a window of this size the 3D picture is drawn at (`render_scale`).
    pub fn scene_scale(&self, width: u32, height: u32) -> f32 {
        let s = scene_scale_for(self.options.render_scale, width, height);
        // (a fixed render scale is the player's choice: the governor works on automatic)
        if self.options.render_scale > 0.0 {
            s
        } else {
            (s * self.dynamic_scale.get()).clamp(0.5, 1.0)
        }
    }

    /// Draw the 3D picture at this fraction of what the render scale says (0.6..1): the
    /// game lowers it while a slow graphics chip cannot keep the frame rate up, and raises
    /// it again when there is room. Steps of a twentieth, so that the few sizes it takes
    /// keep their render targets.
    pub fn set_dynamic_scale(&self, s: f32) {
        // (1, 0.85, 0.7 or 0.55 - the last for a phone's chip that is still too slow: see
        // the game's governor)
        let s = s.clamp(0.55, 1.0);
        let level = [1.0f32, 0.85, 0.7, 0.55].into_iter().min_by(|a, b| (a - s).abs().total_cmp(&(b - s).abs())).unwrap_or(1.0);
        self.dynamic_scale.set(level);
    }

    pub fn dynamic_scale(&self) -> f32 {
        self.dynamic_scale.get()
    }

    /// The size the 3D scene is drawn at for a window of this size.
    pub fn scene_size(&self, width: u32, height: u32) -> (u32, u32) {
        let s = self.scene_scale(width, height);
        if s >= 0.999 {
            return (width, height);
        }
        (
            ((width as f32 * s).round() as u32).max(1),
            ((height as f32 * s).round() as u32).max(1),
        )
    }

    /// The smaller colour target of the render scale for this size, with its bind group.
    fn scale_target(&mut self, w: u32, h: u32) -> (wgpu::TextureView, wgpu::BindGroup) {
        self.target_use.insert((w, h), std::time::Instant::now());
        if let Some(t) = self.scale_targets.get(&(w, h)) {
            return t.clone();
        }
        self.evict_targets();
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scaled scene"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        let bg = self.picture_group(&view);
        self.scale_targets
            .insert((w, h), (view.clone(), bg.clone()));
        (view, bg)
    }

    fn picture_group(&self, view: &wgpu::TextureView) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene picture"),
            layout: &self.upscale_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.upscale_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.post_sampler) },
            ],
        })
    }

    pub fn new_scene(&self) -> Scene {
        Scene {
            meshes: Vec::new(),
            textures: Vec::new(),
            glass_slot: None,
            materials: Vec::new(),
            instances: Vec::new(),
            render_origin: DVec3::ZERO,
            lights: Vec::new(),
            interior_lights: Vec::new(),
            interior_free: Vec::new(),
            coronas: Vec::new(),
            model_buf: None,
            params_buf: None,
            light_buf: None,
            grid_buf: None,
            corona_buf: None,
            corona_count: 0,
            smoke: Vec::new(),
            smoke_buf: None,
            smoke_count: 0,
            corona_runs: Vec::new(),
            draw_buf: None,
            camera_bind_group: None,
            shadow_bind_group: None,
            sky_bind_group: None,
            overlays: Vec::new(),
            premultiplied: Default::default(),
            overlay_res: Vec::new(),
            dirty: true,
            changed: Vec::new(),
            changed_mark: Vec::new(),
            cache_bounds: omsi_cfg::env::var_os("OMSI_NO_BOUNDS_CACHE").is_none(),
            bounds_meshes: Vec::new(),
            bounds_dirty: false,
            uploaded_instances: 0,
            uploaded_entries: 0,
            cpu_models: Vec::new(),
            cpu_params: Vec::new(),
            last_grid: Vec::new(),
            last_lights: Vec::new(),
            bind_groups: HashMap::new(),
            pbr_maps: HashMap::new(),
            snow_textures: Default::default(),
        }
    }

    /// Draw instance `instance` with mesh `mesh` from now on (a vehicle's skinned mesh gets
    /// a copy of its own).
    pub fn set_instance_mesh(&self, scene: &mut Scene, instance: usize, mesh: MeshId) {
        if scene.instances[instance].mesh != mesh {
            scene.instances[instance].mesh = mesh;
            scene.dirty = true;
        }
    }

    /// Overwrite the vertices of a mesh (skinned humans); the vertex count must not grow.
    pub fn update_mesh(
        &self,
        scene: &mut Scene,
        id: MeshId,
        positions: &[Vec3],
        normals: &[Vec3],
        uvs: &[glam::Vec2],
    ) {
        let verts: Vec<Vertex> = positions
            .iter()
            .zip(normals)
            .zip(uvs)
            .map(|((p, n), uv)| Vertex {
                pos: p.to_array(),
                normal: n.to_array(),
                uv: uv.to_array(),
            })
            .collect();
        let bytes: &[u8] = bytemuck::cast_slice(&verts);
        let m = &mut scene.meshes[id];
        if (m.vertex_buf.size() as usize) < bytes.len() {
            return;
        }
        {
            // (a newer pose of the same mesh replaces one still waiting)
            let mut pending = self.pending_meshes.borrow_mut();
            match pending.iter_mut().find(|(mid, _)| *mid == id) {
                Some(e) => e.1 = bytes.to_vec(),
                None => pending.push((id, bytes.to_vec())),
            }
        }
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for p in positions {
            lo = lo.min(*p);
            hi = hi.max(*p);
        }
        if !positions.is_empty() {
            m.bounds_center = (lo + hi) * 0.5;
            m.bounds_radius = (hi - m.bounds_center).length();
            Self::mesh_bounds_changed(scene, id);
        }
    }

    /// Copy the vertex data `update_mesh` collected into the meshes. Written through the
    /// queue, mesh by mesh (the writes land before the frame's commands run): one staging
    /// buffer for the whole frame outgrew the device's buffer limit on big maps, its
    /// creation failed validation and mapping it panicked (Windows, Vulkan).
    fn flush_pending_meshes(&self, scene: &Scene, _encoder: &mut wgpu::CommandEncoder) {
        let pending = std::mem::take(&mut *self.pending_meshes.borrow_mut());
        for (id, b) in &pending {
            let Some(m) = scene.meshes.get(*id) else { continue };
            let len = (b.len() as u64) / 4 * 4;
            if len == 0 || m.vertex_buf.size() < len {
                continue;
            }
            self.queue.write_buffer(&m.vertex_buf, 0, &b[..len as usize]);
        }
    }

    pub fn add_mesh(&self, scene: &mut Scene, data: &MeshData) -> MeshId {
        scene.meshes.push(make_mesh(&self.device, &self.queue, data));
        scene.meshes.len() - 1
    }

    /// Put a mesh made on another thread ([`prepare_mesh`]) into the scene.
    pub fn add_prepared_mesh(&self, scene: &mut Scene, mesh: PreparedMesh) -> MeshId {
        scene.meshes.push(mesh.0);
        scene.meshes.len() - 1
    }

    /// Put a texture made on another thread ([`prepare_texture`]) into the scene.
    pub fn add_prepared_texture(&self, scene: &mut Scene, texture: PreparedTexture) -> TextureId {
        scene.textures.push(texture.0);
        scene.textures.len() - 1
    }

    pub fn add_texture(
        &self,
        scene: &mut Scene,
        img: &omsi_texture::Image,
        mipmaps: bool,
    ) -> TextureId {
        let fitted = fit_image(img, self.device.limits().max_texture_dimension_2d);
        let img = fitted.as_ref().unwrap_or(img);
        let t = if mipmaps && img.width > 1 && img.height > 1 {
            self.upload_texture_gpu_mips(img)
        } else {
            upload_texture(&self.device, &self.queue, img, false)
        };
        scene.textures.push(t);
        scene.textures.len() - 1
    }

    pub fn add_blank_texture(&self, scene: &mut Scene, width: u32, height: u32) -> TextureId {
        let (width, height) = fit_size(width.max(1), height.max(1), self.device.limits().max_texture_dimension_2d);
        // A newly allocated GPU texture has undefined contents. Script displays may be
        // sampled before their first `STUnlock`, so initialise them as transparent rather
        // than briefly showing arbitrary solid pixels on new or AI vehicles.
        let image = omsi_texture::Image {
            width,
            height,
            rgba: vec![0; (width * height * 4) as usize],
            has_alpha: true,
        };
        let texture = upload_texture(&self.device, &self.queue, &image, false);
        scene.textures.push(texture);
        scene.textures.len() - 1
    }

    /// The PBR set found beside diffuse texture `diffuse` (`omsi_texture::pbr`): its maps up
    /// (as data, not colours) and known to the materials made with that texture from now on.
    pub fn add_pbr_maps(&self, scene: &mut Scene, diffuse: TextureId, set: &omsi_texture::pbr::PbrImages) {
        // stored so that the sRGB textures give back the bytes as they are: a normal map's
        // 128 read as 0.22 through the sRGB curve bent every normal
        let lut: Vec<u8> = (0..256)
            .map(|v| {
                let l = v as f32 / 255.0;
                let s = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
                (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8
            })
            .collect();
        let mut up = |img: &omsi_texture::Image| {
            let data = omsi_texture::Image { width: img.width, height: img.height, rgba: img.rgba.iter().map(|b| lut[*b as usize]).collect(), has_alpha: false };
            self.add_texture(scene, &data, true)
        };
        let normal = set.normal.as_ref().map(&mut up);
        let orm = set.orm.as_ref().map(&mut up);
        scene.pbr_maps.insert(diffuse, PbrMaps { normal, orm, flags: set.flags });
    }

    /// The device takes BC1-3 (DXT) textures.
    pub fn supports_bc(&self) -> bool {
        self.device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
    }

    /// Upload a texture prepared by `omsi_texture::gpu` (blocks with their levels, or RGBA
    /// whose chain is made here).
    pub fn add_texture_data(
        &self,
        scene: &mut Scene,
        data: &omsi_texture::TextureData,
    ) -> TextureId {
        let t = self.upload_texture_data(data);
        scene.textures.push(t);
        scene.textures.len() - 1
    }

    fn upload_texture_data(&self, data: &omsi_texture::TextureData) -> GpuTexture {
        if let Some(small) = fit_texture(data, self.device.limits().max_texture_dimension_2d) {
            return self.upload_texture_data(&small);
        }
        if let Some(t) = prepare_texture(&self.device, &self.queue, data) {
            return t.0;
        }
        use omsi_texture::PixelFormat;
        let (w, h) = (data.width.max(1), data.height.max(1));
        let blocks_ok =
            !data.format.is_compressed() || (self.supports_bc() && w % 4 == 0 && h % 4 == 0);
        if !blocks_ok || data.levels.is_empty() {
            // a device without block formats (the loader was told otherwise): decode
            let rgba = match (data.format, data.levels.first()) {
                (PixelFormat::Rgba8, Some(l)) => l.clone(),
                (f, Some(l)) => omsi_texture::bc::decode(
                    l,
                    w,
                    h,
                    match f {
                        PixelFormat::Bc1 => omsi_texture::bc::Bc::Bc1 { punch: true },
                        PixelFormat::Bc2 => omsi_texture::bc::Bc::Bc2,
                        _ => omsi_texture::bc::Bc::Bc3,
                    },
                ),
                _ => vec![255; (w * h * 4) as usize],
            };
            return self.upload_texture_gpu_mips(&omsi_texture::Image {
                width: w,
                height: h,
                rgba,
                has_alpha: data.has_alpha,
            });
        }
        // RGBA gets its chain on the GPU (a picture with levels of its own was made by
        // `prepare_texture` above); the picture is borrowed, not copied
        self.upload_rgba_gpu_mips(w, h, &data.levels[0])
    }

    /// Bytes of all textures of the scene on the GPU (render targets included).
    /// Bytes the meshes' vertex and index buffers take on the GPU (the freed ones' shared
    /// placeholder not counted).
    pub fn mesh_bytes(&self, scene: &Scene) -> u64 {
        let freed = self.freed.get().map(|f| (f.vertex_buf.clone(), f.index_buf.clone()));
        scene
            .meshes
            .iter()
            .filter(|m| freed.as_ref().is_none_or(|(v, _)| m.vertex_buf != *v))
            .map(|m| m.vertex_buf.size() + m.index_buf.size())
            .sum()
    }

    pub fn texture_bytes(&self, scene: &Scene) -> u64 {
        scene.textures.iter().map(|t| t.bytes).sum()
    }

    /// Bytes of one texture (0 for a freed slot).
    pub fn texture_size_bytes(&self, scene: &Scene, id: TextureId) -> u64 {
        scene.textures.get(id).map(|t| t.bytes).unwrap_or(0)
    }

    /// Upload a texture and build its mip chain on the GPU: level 0 is written, every
    /// further level is the one above drawn at half size.
    fn upload_texture_gpu_mips(&self, img: &omsi_texture::Image) -> GpuTexture {
        self.upload_rgba_gpu_mips(img.width, img.height, &img.rgba)
    }

    fn upload_rgba_gpu_mips(&self, width: u32, height: u32, rgba: &[u8]) -> GpuTexture {
        let img = RgbaRef {
            width,
            height,
            rgba,
        };
        let mip_count = (32 - img.width.max(img.height).leading_zeros()).max(1);
        let size = wgpu::Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size,
            mip_level_count: mip_count,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            img.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(img.width * 4),
                rows_per_image: Some(img.height),
            },
            size,
        );
        self.generate_mip_chain(&texture, mip_count);
        let view = texture.create_view(&Default::default());
        GpuTexture {
            texture,
            view,
            size: (img.width, img.height),
            bytes: texture_bytes(
                wgpu::TextureFormat::Rgba8UnormSrgb,
                img.width,
                img.height,
                mip_count,
            ),
            gen: next_gen(),
        }
    }

    /// Build all lower levels of `texture` by rendering each level from the last. The
    /// texture has already received its level-zero pixels and was created with render-attach
    /// usage by [`upload_rgba_gpu_mips`].
    fn generate_mip_chain(&self, texture: &wgpu::Texture, mip_count: u32) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mips"),
            });
        for level in 1..mip_count {
            let src = texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: level - 1,
                mip_level_count: Some(1),
                ..Default::default()
            });
            let dst = texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            });
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("mip"),
                layout: &self.mip_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&src),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.mip_sampler),
                    },
                ],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mip"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.mip_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
    }

    /// `[matl_noZwrite]` for a material that is already in the scene.
    pub fn set_no_z_write(&self, scene: &mut Scene, id: MaterialId, on: bool) {
        if let Some(m) = scene.materials.get_mut(id) {
            m.no_z_write = on;
        }
    }

    /// The view of a texture of the scene (to draw into it with another pipeline).
    pub fn texture_view(&self, scene: &Scene, id: TextureId) -> Option<wgpu::TextureView> {
        scene.textures.get(id).map(|t| t.view.clone())
    }

    pub fn upload_speed_mb_s(&self) -> f64 {
        let n = 1024u32;
        let size = wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("upload check"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let data = vec![0u8; (n * n * 4) as usize];
        let rounds = 4;
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                &data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(n * 4), rows_per_image: Some(n) },
                size,
            );
        }
        let secs = t.elapsed().as_secs_f64().max(1e-6);
        self.queue.submit([]);
        texture.destroy();
        (data.len() * rounds) as f64 / 1e6 / secs
    }

    /// A texture the scene can be rendered into (`render_to_texture`), e.g. a rear-view mirror.
    pub fn add_render_texture(&self, scene: &mut Scene, width: u32, height: u32) -> TextureId {
        let (width, height) = fit_size(width.max(1), height.max(1), self.device.limits().max_texture_dimension_2d);
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("render target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        // (its depth buffer is the pass's own, see `msaa_targets`)
        let bytes = texture_bytes(self.format, width, height, 1);
        scene.textures.push(GpuTexture {
            texture,
            view,
            size: (width, height),
            bytes,
            gen: next_gen(),
        });
        scene.textures.len() - 1
    }

    /// Render the scene from `camera` into a texture made by `add_render_texture`.
    /// `aspect`: the projection's width over height (OMSI draws its mirrors 1.6 wide into
    /// square textures, and their meshes show the middle of that).
    pub fn render_to_texture(
        &mut self,
        scene: &mut Scene,
        id: TextureId,
        camera: &Camera,
        lighting: &Lighting,
        aspect: f32,
    ) {
        let Some(t) = scene.textures.get(id) else {
            return;
        };
        let view = t.view.clone();
        let (w, h) = t.size;
        self.texture_aspect = Some(aspect);
        self.render_inner(scene, &view, w, h, camera, lighting, false, Some(id), None, false);
        self.texture_aspect = None;
    }

    /// Replace the pixels of a texture (same size as when created, no mipmaps regenerated).
    pub fn update_texture(&self, scene: &Scene, id: TextureId, img: &omsi_texture::Image) {
        let Some(gt) = scene.textures.get(id) else { return };
        // (a picture larger than the chip takes went up halved, see `fit_image`; one larger
        // than its texture is not written - a device error each frame)
        let fitted = fit_image(img, self.device.limits().max_texture_dimension_2d);
        let img = fitted.as_ref().unwrap_or(img);
        if img.width > gt.size.0 || img.height > gt.size.1 || img.rgba.len() < (img.width * img.height * 4) as usize {
            return;
        }
        let t = &gt.texture;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: t,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &img.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(img.width * 4),
                rows_per_image: Some(img.height),
            },
            wgpu::Extent3d {
                width: img.width,
                height: img.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Replace the top pixels of a dynamic texture and rebuild its mip chain. Returns true
    /// when the texture had to be recreated (and its materials therefore need rebinding).
    /// This is used for OMSI script textures after `STFilter`.
    pub fn update_texture_mips(
        &self,
        scene: &mut Scene,
        id: TextureId,
        img: &omsi_texture::Image,
    ) -> bool {
        let Some(old) = scene.textures.get(id) else {
            return false;
        };
        let fitted = fit_image(img, self.device.limits().max_texture_dimension_2d);
        let img = fitted.as_ref().unwrap_or(img);
        let levels = (32 - img.width.max(img.height).leading_zeros()).max(1);
        if old.size != (img.width, img.height) || old.texture.mip_level_count() != levels {
            scene.textures[id] = self.upload_texture_gpu_mips(img);
            return true;
        }
        self.update_texture(scene, id, img);
        if levels > 1 {
            self.generate_mip_chain(&scene.textures[id].texture, levels);
        }
        false
    }

    pub fn add_material(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
    ) -> MaterialId {
        self.add_material_ex(scene, texture, alpha, color, unlit, None)
    }

    /// Material with an optional transparency map `(texture, use_alpha_channel)`.
    pub fn add_material_ex(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
        transmap: Option<(TextureId, bool)>,
    ) -> MaterialId {
        self.add_material_full(
            scene, texture, alpha, color, unlit, transmap, None, None, None, None, [0.0; 3],
        )
    }

    /// Material with transparency map and `[matl_nightmap]` self-illumination texture.
    pub fn add_material_night(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
        transmap: Option<(TextureId, bool)>,
        nightmap: Option<TextureId>,
    ) -> MaterialId {
        self.add_material_full(
            scene, texture, alpha, color, unlit, transmap, None, nightmap, None, None, [0.0; 3],
        )
    }

    /// Material with a `[matl_lightmap]` texture whose strength comes from the per-slot
    /// instance value (illuminated displays).
    pub fn add_material_lit(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
        transmap: Option<(TextureId, bool)>,
        nightmap: Option<TextureId>,
        lightmap: Option<TextureId>,
    ) -> MaterialId {
        self.add_material_full(
            scene, texture, alpha, color, unlit, transmap, None, nightmap, lightmap, None, [0.0; 3],
        )
    }

    /// Full vehicle material: also a `[matl_envmap]` sphere map with its strength factor
    /// (masked by the diffuse alpha channel like the original).
    pub fn add_material_env(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
        transmap: Option<(TextureId, bool)>,
        nightmap: Option<TextureId>,
        lightmap: Option<TextureId>,
        envmap: Option<(TextureId, f32)>,
    ) -> MaterialId {
        self.add_material_full(
            scene, texture, alpha, color, unlit, transmap, None, nightmap, lightmap, envmap,
            [0.0; 3],
        )
    }

    /// Like `add_material_env` with an emissive colour (`[matl_allcolor]`).
    pub fn add_material_all(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
        transmap: Option<(TextureId, bool)>,
        nightmap: Option<TextureId>,
        lightmap: Option<TextureId>,
        envmap: Option<(TextureId, f32)>,
        emissive: [f32; 3],
    ) -> MaterialId {
        self.add_material_full(
            scene, texture, alpha, color, unlit, transmap, None, nightmap, lightmap, envmap,
            emissive,
        )
    }

    /// Like `add_material_all` with the rest of the material manager's settings
    /// (reflection mask, depth handling, specular term).
    #[allow(clippy::too_many_arguments)]
    pub fn add_material_extra(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
        transmap: Option<(TextureId, bool)>,
        nightmap: Option<TextureId>,
        lightmap: Option<TextureId>,
        envmap: Option<(TextureId, f32)>,
        emissive: [f32; 3],
        extra: MaterialExtra,
    ) -> MaterialId {
        self.add_material_inner(
            scene, texture, alpha, color, unlit, transmap, None, nightmap, lightmap, envmap,
            emissive, extra.moisture, extra,
        )
    }

    /// Swap the material of one slot of an instance (material variants).
    pub fn set_material(
        &self,
        scene: &mut Scene,
        instance: usize,
        slot: usize,
        material: MaterialId,
    ) {
        if let Some(m) = scene.instances[instance].materials.get_mut(slot) {
            *m = material;
        }
    }

    /// Make a copy of a material with a different diffuse texture. Used by scenery CTC and
    /// `[texchanges]` selectors: the slot's alpha, lighting, reflection, and depth settings
    /// stay as they were, while the replacement texture may bring its own PBR maps.
    pub fn add_material_retextured(
        &self,
        scene: &mut Scene,
        base: MaterialId,
        texture: Option<TextureId>,
    ) -> Option<MaterialId> {
        let (
            alpha,
            color,
            unlit,
            no_z_write,
            writes_depth,
            no_z_check,
            z_bias,
            nightmap,
            lightmap,
            envmap,
            env_mask,
            bump,
            emissive,
            transmap,
            address,
            mut uniform,
        ) = {
            let src = scene.materials.get(base)?;
            (
                src.alpha,
                src.color,
                src.unlit,
                src.no_z_write,
                src.writes_depth,
                src.no_z_check,
                src.z_bias,
                src.nightmap,
                src.lightmap,
                src.envmap,
                src.env_mask,
                src.bump,
                src.emissive,
                src.transmap,
                src.address,
                src.uniform,
            )
        };
        uniform.pbr = texture
            .and_then(|id| scene.pbr_maps.get(&id))
            .map(|maps| maps.flags)
            .unwrap_or([0.0; 4]);
        if uniform.ambient[3] < 1.5 {
            uniform.ambient[3] = snow_texture_flag(scene, texture);
        }
        let slot = |t: Option<TextureId>| {
            t.and_then(|t| scene.textures.get(t).map(|g| (t, g.gen)))
                .unwrap_or((usize::MAX, 0))
        };
        let key = BindKey {
            textures: [
                slot(texture),
                slot(transmap.map(|t| t.0)),
                slot(nightmap),
                slot(lightmap),
                slot(envmap.map(|e| e.0)),
                slot(env_mask),
                slot(bump.map(|b| b.0)),
            ],
            address,
            uniform: bytemuck::cast(uniform),
        };
        let (bind_group, buf) = match scene.bind_groups.get(&key) {
            Some((bg, b)) => (bg.clone(), b.clone()),
            None => {
                let buf = buffer_init(
                    &self.device,
                    &self.queue,
                    None,
                    bytemuck::bytes_of(&uniform),
                    wgpu::BufferUsages::UNIFORM,
                );
                let bind_group = self.material_bind_group(
                    &scene.textures,
                    MaterialMaps {
                        texture,
                        transmap,
                        nightmap,
                        lightmap,
                        envmap,
                        env_mask,
                        bump,
                        pbr: texture.and_then(|id| scene.pbr_maps.get(&id)).copied(),
                    },
                    address,
                    &buf,
                );
                scene
                    .bind_groups
                    .insert(key, (bind_group.clone(), buf.clone()));
                (bind_group, buf)
            }
        };
        scene.materials.push(Material {
            texture,
            alpha,
            color,
            unlit,
            no_z_write,
            writes_depth,
            no_z_check,
            z_bias,
            nightmap,
            lightmap,
            envmap,
            env_mask,
            bump,
            emissive,
            transmap,
            address,
            uniform,
            buf,
            bind_group,
        });
        Some(scene.materials.len() - 1)
    }

    /// Terrain material: uv is tile space, the ground texture repeats `repeats` times per
    /// tile, its detail texture `detail` times, and the optional mask (alpha 0 = cut) is
    /// sampled in tile space.
    /// `nightmap`: the tile's `.map.LM.bmp` (street lamp light pools), added at night.
    /// `moisture`: 1 when this layer's `<texture>.cfg` sidecar carries `[moisture]` or
    /// `[puddles]` (the map's base ground layer wets in the rain just like a painted one).
    #[allow(clippy::too_many_arguments)]
    pub fn add_terrain_material(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        mask: Option<TextureId>,
        detail: Option<(TextureId, f32)>,
        repeats: f32,
        nightmap: Option<TextureId>,
        moisture: f32,
    ) -> MaterialId {
        self.add_material_wet(
            scene,
            texture,
            if mask.is_some() {
                AlphaMode::Test
            } else {
                AlphaMode::Opaque
            },
            [1.0; 4],
            false,
            mask.map(|m| (m, true)),
            Some((detail.map(|d| d.1).unwrap_or(0.0), repeats)),
            nightmap,
            detail.map(|d| d.0),
            None,
            [0.0; 3],
            moisture,
        )
    }

    /// A painted ground layer: the tile mesh drawn again with one of the map's other
    /// `[groundtex]` textures, blended in wherever the layer's painting mask (the alpha
    /// DDS the editor's brush writes to `texture/map/tile_x_y.map.<n>.dds`) says so.
    #[allow(clippy::too_many_arguments)]
    pub fn add_terrain_layer_material(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        mask: TextureId,
        detail: Option<(TextureId, f32)>,
        repeats: f32,
        nightmap: Option<TextureId>,
        moisture: f32,
    ) -> MaterialId {
        // The brush mask includes fully transparent road cutouts. Those fragments must
        // never write biased terrain depth over the splines drawn in the next phase:
        // they contribute no colour, but would reject the road and expose the sky.
        // The C++ handler likewise draws painted terrain with depth writes disabled.
        self.add_material_inner(
            scene,
            texture,
            AlphaMode::Blend,
            [1.0; 4],
            false,
            Some((mask, true)),
            Some((detail.map(|d| d.1).unwrap_or(0.0), repeats)),
            nightmap,
            detail.map(|d| d.0),
            None,
            [0.0; 3],
            moisture,
            MaterialExtra {
                no_z_write: true,
                ..MaterialExtra::default()
            },
        )
    }

    fn add_material_full(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
        transmap: Option<(TextureId, bool)>,
        terrain: Option<(f32, f32)>,
        nightmap: Option<TextureId>,
        lightmap: Option<TextureId>,
        envmap: Option<(TextureId, f32)>,
        emissive: [f32; 3],
    ) -> MaterialId {
        self.add_material_wet(
            scene, texture, alpha, color, unlit, transmap, terrain, nightmap, lightmap, envmap,
            emissive, 0.0,
        )
    }

    /// `moisture`: 1 when the `<texture>.cfg` sidecar marks this surface as one that
    /// darkens and starts to mirror the sky while the rain is on it.
    #[allow(clippy::too_many_arguments)]
    pub fn add_material_wet(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
        transmap: Option<(TextureId, bool)>,
        terrain: Option<(f32, f32)>,
        nightmap: Option<TextureId>,
        lightmap: Option<TextureId>,
        envmap: Option<(TextureId, f32)>,
        emissive: [f32; 3],
        moisture: f32,
    ) -> MaterialId {
        self.add_material_inner(
            scene,
            texture,
            alpha,
            color,
            unlit,
            transmap,
            terrain,
            nightmap,
            lightmap,
            envmap,
            emissive,
            moisture,
            MaterialExtra::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn add_material_inner(
        &self,
        scene: &mut Scene,
        texture: Option<TextureId>,
        alpha: AlphaMode,
        color: [f32; 4],
        unlit: bool,
        transmap: Option<(TextureId, bool)>,
        terrain: Option<(f32, f32)>,
        nightmap: Option<TextureId>,
        lightmap: Option<TextureId>,
        envmap: Option<(TextureId, f32)>,
        emissive: [f32; 3],
        moisture: f32,
        extra: MaterialExtra,
    ) -> MaterialId {
        // (the reflection maps switched off: as a material without one)
        let envmap = envmap.filter(|_| self.options.reflections);
        // the mask and the bump map only ever change the reflection
        let env_mask = extra.env_mask.filter(|_| envmap.is_some());
        let bump = extra.bump.filter(|_| envmap.is_some());
        // a rain film's reflection slot holds the picture behind the glass: its drops show
        // the street through themselves, bent and upside down, as real drops do
        let envmap = if extra.rain_film { Some((self.glass_slot(scene), 0.0)) } else { envmap };
        let address = self.address_next.replace(TexAddressing::Wrap);
        let lm_mapped = self.light_map_next.replace(false);
        let mode = match alpha {
            AlphaMode::Opaque => 0.0,
            AlphaMode::Test => 1.0,
            AlphaMode::Blend => 2.0,
        };
        // a mirror's glass shows a picture this renderer drew (`add_render_texture`): the
        // enhanced shader must not brighten it as it does a display (see enhanced.wgsl)
        let mirror = unlit
            && texture
                .and_then(|t| scene.textures.get(t))
                .is_some_and(|t| t.texture.usage().contains(wgpu::TextureUsages::RENDER_ATTACHMENT));
        let uniform = MaterialUniform {
            color,
            params: [
                mode,
                // 1 unlit (0.9 a mirror's own picture); 0.25 lit by everything but the map's
                // lamps; 0.15 a tree, not lit by the map's lamps in the vanilla picture
                if mirror { 0.9 } else if unlit { 1.0 } else if lm_mapped { 0.35 } else if extra.no_map_lights { 0.25 } else if extra.tree { 0.15 } else { 0.0 },
                // a [matl_transmap] whose file is missing still takes the alpha stage: Omsi.exe
                // sets a NULL texture there, which D3D9 samples as alpha 1, so the slot is as
                // opaque as its transmap says - not as see-through as the diffuse texture's
                // alpha, a reflection mask on car bodies (traffic half transparent)
                if transmap.is_some() || extra.transmap_declared { 1.0 } else { 0.0 },
                if transmap.map(|t| t.1).unwrap_or(false) {
                    1.0
                } else {
                    0.0
                },
            ],
            extra: [
                if terrain.is_some() { 1.0 } else { 0.0 },
                terrain.map(|t| t.0).unwrap_or(1.0),
                terrain.map(|t| t.1).unwrap_or(1.0),
                match (nightmap.is_some(), extra.night_switched) {
                    (false, _) => 0.0,
                    (true, false) => 1.0,
                    (true, true) => 2.0,
                },
            ],
            params2: [
                if lightmap.is_some() { 1.0 } else { 0.0 },
                envmap.map(|e| e.1).unwrap_or(0.0),
                moisture,
                // bit 1: a [matl_envmap_mask]; bit 2: a [matl_transmap]; bit 4: a vehicle's
                // part that may be metal (see the shaders)
                (if env_mask.is_some() { 1.0 } else { 0.0 })
                    + if extra.transmap_declared || transmap.is_some() { 2.0 } else { 0.0 }
                    + if extra.metal_ok { 4.0 } else { 0.0 },
            ],
            emissive: [emissive[0], emissive[1], emissive[2], if extra.rain_film { 2.0 } else if extra.glass { 1.0 } else if extra.led { -2.0 } else if extra.display { -1.0 } else { 0.0 }],
            specular: extra.specular,
            bump: [
                bump.map(|b| b.1).unwrap_or(0.0),
                if bump.is_some() { 1.0 } else { 0.0 },
                if extra.no_z_write { 1.0 } else { 0.0 },
                if extra.no_z_check { 1.0 } else { 0.0 },
            ],
            pbr: texture.and_then(|t| scene.pbr_maps.get(&t)).map(|m| m.flags).unwrap_or([0.0; 4]),
            flags: {
                let b = extra.border.unwrap_or([0.0; 4]).map(|c| (c.clamp(0.0, 1.0) * 255.0).round());
                [
                    if extra.screen { 1.0 } else { 0.0 },
                    if extra.border.is_some() {
                        1.0
                    } else if address == TexAddressing::MirrorOnce {
                        2.0
                    } else {
                        0.0
                    },
                    b[0] * 65536.0 + b[1] * 256.0 + b[2],
                    b[3] / 255.0,
                ]
            },
            ambient: {
                let a = extra.ambient.unwrap_or([color[0], color[1], color[2]]);
                [a[0], a[1], a[2], if extra.water { 2.0 } else { snow_texture_flag(scene, texture) }]
            },
        };
        let slot = |t: Option<TextureId>| {
            t.and_then(|t| scene.textures.get(t).map(|g| (t, g.gen)))
                .unwrap_or((usize::MAX, 0))
        };
        let key = BindKey {
            textures: [
                slot(texture),
                slot(transmap.map(|t| t.0)),
                slot(nightmap),
                slot(lightmap),
                slot(envmap.map(|e| e.0)),
                slot(env_mask),
                slot(bump.map(|b| b.0)),
            ],
            address,
            uniform: bytemuck::cast(uniform),
        };
        let (bind_group, buf) = match scene.bind_groups.get(&key) {
            Some((bg, b)) => (bg.clone(), b.clone()),
            None => {
                let buf = buffer_init(&self.device, &self.queue, None, bytemuck::bytes_of(&uniform), wgpu::BufferUsages::UNIFORM);
                let bind_group = self.material_bind_group(
                    &scene.textures,
                    MaterialMaps {
                        texture,
                        transmap,
                        nightmap,
                        lightmap,
                        envmap,
                        env_mask,
                        bump,
                        pbr: texture.and_then(|t| scene.pbr_maps.get(&t)).copied(),
                    },
                    address,
                    &buf,
                );
                scene
                    .bind_groups
                    .insert(key, (bind_group.clone(), buf.clone()));
                (bind_group, buf)
            }
        };
        scene.materials.push(Material {
            texture,
            alpha,
            color,
            unlit,
            no_z_write: extra.no_z_write,
            writes_depth: extra.writes_depth && extra.no_z_write,
            no_z_check: extra.no_z_check,
            z_bias: extra.z_bias,
            nightmap,
            lightmap,
            envmap,
            env_mask,
            bump,
            emissive,
            transmap,
            address,
            uniform,
            buf,
            bind_group,
        });
        scene.materials.len() - 1
    }

    /// The scene's slot for the picture behind the glass (black until a frame is drawn).
    fn glass_slot(&self, scene: &mut Scene) -> TextureId {
        if let Some(id) = scene.glass_slot {
            return id;
        }
        scene.textures.push(GpuTexture::showing(self.black_texture.texture.clone(), self.black_texture.view.clone(), (1, 1)));
        let id = scene.textures.len() - 1;
        scene.glass_slot = Some(id);
        id
    }

    /// Refraction reads a copy made before drawing films, avoiding rain/wiper feedback.
    fn prepare_glass_behind(&mut self, scene: &mut Scene, width: u32, height: u32, format: wgpu::TextureFormat) {
        let Some(id) = scene.glass_slot else { return };
        if self.glass_picture.as_ref().is_none_or(|v| v.texture().width() != width || v.texture().height() != height || v.texture().format() != format) {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("picture behind glass"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1, sample_count: 1,
                dimension: wgpu::TextureDimension::D2, format,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            self.glass_picture = Some(tex.create_view(&Default::default()));
        }
        let view = self.glass_picture.as_ref().unwrap();
        if scene.textures[id].view != *view {
            scene.textures[id] = GpuTexture::showing(view.texture().clone(), view.clone(), (width, height));
            self.rebind_textures(scene, &[id]);
        }
    }

    /// The bind group of a material: its textures (or the plain white/black ones), its
    /// sampler and its uniform buffer.
    fn material_bind_group(
        &self,
        textures: &[GpuTexture],
        maps: MaterialMaps,
        address: TexAddressing,
        buf: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        let view = |t: Option<TextureId>, or: &'_ GpuTexture| -> wgpu::TextureView {
            t.and_then(|t| textures.get(t))
                .map(|t| t.view.clone())
                .unwrap_or_else(|| or.view.clone())
        };
        let env_view = view(maps.envmap.map(|e| e.0), &self.black_texture);
        let night_view = view(maps.nightmap, &self.black_texture);
        let light_view = view(maps.lightmap, &self.black_texture);
        let diffuse_view = view(maps.texture, &self.white_texture);
        let trans_view = view(maps.transmap.map(|t| t.0), &self.white_texture);
        // a mask or bump map without its own texture reads white (full mask, flat)
        let mask_view = view(maps.env_mask, &self.white_texture);
        let bump_view = view(maps.bump.map(|b| b.0), &self.white_texture);
        // (without a PBR set: a flat normal, and full occlusion/roughness/metal channels the
        // shader leaves unread by its flags)
        let normal_view = view(maps.pbr.and_then(|p| p.normal), &self.flat_normal_texture);
        let orm_view = view(maps.pbr.and_then(|p| p.orm), &self.white_texture);
        let sampler = match address {
            TexAddressing::Wrap => &self.sampler,
            TexAddressing::Mirror => &self.mirror_sampler,
            TexAddressing::Clamp | TexAddressing::MirrorOnce => &self.clamp_sampler,
        };
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.material_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&diffuse_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&trans_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&night_view),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&light_view),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&env_view),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::TextureView(&mask_view),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: wgpu::BindingResource::TextureView(&bump_view),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: wgpu::BindingResource::TextureView(&normal_view),
                },
                wgpu::BindGroupEntry {
                    binding: 10,
                    resource: wgpu::BindingResource::TextureView(&orm_view),
                },
                wgpu::BindGroupEntry {
                    binding: 11,
                    resource: wgpu::BindingResource::Sampler(&self.clamp_sampler),
                },
            ],
        })
    }

    /// Put `data` into texture slot `id` in place of what is there (a picture uploaded as
    /// RGBA to spare a frame, compressed since), and point the materials using it at it.
    pub fn replace_texture(
        &self,
        scene: &mut Scene,
        id: TextureId,
        data: &omsi_texture::TextureData,
    ) {
        if id >= scene.textures.len() {
            return;
        }
        scene.textures[id] = self.upload_texture_data(data);
    }

    /// Mip levels and size of a texture: (width, height, levels).
    pub fn texture_levels(&self, scene: &Scene, id: TextureId) -> Option<(u32, u32, u32)> {
        let t = scene.textures.get(id)?;
        (t.bytes > 0).then(|| (t.size.0, t.size.1, t.texture.mip_level_count()))
    }

    /// Let go of the `n` finest mip levels of texture `id` (a texture far away while the
    /// textures are over their memory budget): the coarser levels are copied into a smaller
    /// texture on the GPU, which takes the slot. False when the texture cannot shrink so far
    /// (a block format needs sides that are multiples of four). The materials using it
    /// need [`Renderer::rebind_textures`] afterwards.
    pub fn drop_top_levels(&self, scene: &mut Scene, id: TextureId, n: u32) -> bool {
        let Some(t) = scene.textures.get(id) else {
            return false;
        };
        let (w, h) = t.size;
        let levels = t.texture.mip_level_count();
        let format = t.texture.format();
        if n == 0 || n >= levels || !t.texture.usage().contains(wgpu::TextureUsages::COPY_SRC) {
            return false;
        }
        let (nw, nh) = ((w >> n).max(1), (h >> n).max(1));
        let (bw, bh) = format.block_dimensions();
        if nw % bw != 0 || nh % bh != 0 {
            return false;
        }
        let new_levels = levels - n;
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: nw,
                height: nh,
                depth_or_array_layers: 1,
            },
            mip_level_count: new_levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("drop levels"),
            });
        for l in 0..new_levels {
            let (lw, lh) = ((nw >> l).max(1), (nh >> l).max(1));
            let size = wgpu::Extent3d {
                width: lw.div_ceil(bw) * bw,
                height: lh.div_ceil(bh) * bh,
                depth_or_array_layers: 1,
            };
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &t.texture,
                    mip_level: l + n,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: l,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                size,
            );
        }
        self.queue.submit([encoder.finish()]);
        scene.textures[id] =
            GpuTexture::new(texture, (nw, nh), texture_bytes(format, nw, nh, new_levels));
        true
    }

    /// Rebuild the bind groups of the materials that sample any of `ids` (after
    /// [`Renderer::replace_texture`]). Returns how many were rebuilt.
    pub fn rebind_textures(&self, scene: &mut Scene, ids: &[TextureId]) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let textures = &scene.textures;
        let pbr_maps = &scene.pbr_maps;
        let set: std::collections::HashSet<TextureId> = ids.iter().copied().collect();
        let mut n = 0;
        for m in scene.materials.iter_mut() {
            let uses = [
                m.texture,
                m.nightmap,
                m.lightmap,
                m.envmap.map(|e| e.0),
                m.transmap.map(|t| t.0),
                m.env_mask,
                m.bump.map(|b| b.0),
            ]
            .iter()
            .flatten()
            .any(|t| set.contains(t));
            if !uses {
                continue;
            }
            m.bind_group = self.material_bind_group(
                textures,
                MaterialMaps {
                    texture: m.texture,
                    transmap: m.transmap,
                    nightmap: m.nightmap,
                    lightmap: m.lightmap,
                    envmap: m.envmap,
                    env_mask: m.env_mask,
                    bump: m.bump,
                    pbr: m.texture.and_then(|t| pbr_maps.get(&t)).copied(),
                },
                m.address,
                &m.buf,
            );
            n += 1;
        }
        n
    }

    /// Sky gradient textures (day, twilight, night); without them the sky is the clear colour.
    pub fn set_sky_textures(&self, scene: &mut Scene, textures: [TextureId; 3]) {
        self.set_sky_textures_clouds(scene, textures, None)
    }

    /// Sky gradients plus an optional tiling cloud texture (`Texture\clouds.tga`).
    pub fn set_sky_textures_clouds(
        &self,
        scene: &mut Scene,
        textures: [TextureId; 3],
        clouds: Option<TextureId>,
    ) {
        let views: Vec<&wgpu::TextureView> =
            textures.iter().map(|t| &scene.textures[*t].view).collect();
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sky"),
            layout: &self.sky_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(views[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sky_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(match clouds {
                        Some(c) => &scene.textures[c].view,
                        None => &self.black_texture.view,
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&self.cloud_shape_view),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::TextureView(&self.cloud_detail_view),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: wgpu::BindingResource::Sampler(&self.cloud_sampler),
                },
            ],
        });
        scene.sky_bind_group = Some(bg);
    }

    pub fn add_instance(
        &self,
        scene: &mut Scene,
        mesh: MeshId,
        origin: DVec3,
        transform: Mat4,
        materials: Vec<MaterialId>,
    ) -> usize {
        let slots = scene.meshes[mesh]
            .ranges
            .iter()
            .map(|r| r.2)
            .max()
            .map(|m| m as usize + 1)
            .unwrap_or(1);
        scene.instances.push(Instance {
            mesh,
            transform,
            origin,
            materials,
            slot_alpha: vec![1.0; slots],
            slot_light: vec![1.0; slots],
            slot_night: vec![1.0; slots],
            visible: true,
            slot_uv: vec![[0.0; 2]; slots],
            interior: 0.0,
            interior_lamps: 0,
            base: 0,
            bounds: InstanceBounds::default(),
            surface: false,
            presurface: false,
            render_phase: RenderPhase::Normal,
            surface_bias: false,
            blend_sort_origin: None,
            lod: (0.0, f32::MAX),
            blob: false,
            ground_layer: false,
            decal: false,
            object_radius: 0.0,
            detail: 1.0,
            any_distance: false,
            near_only: None,
            mirror_only: false,
            omsi_caster: false,
            ordered: false,
            casts_shadow: true,
            roof: None,
        });
        scene.instances.len() - 1
    }

    pub fn add_surface_instance(
        &self,
        scene: &mut Scene,
        mesh: MeshId,
        origin: DVec3,
        transform: Mat4,
        materials: Vec<MaterialId>,
    ) -> usize {
        let slots = scene.meshes[mesh]
            .ranges
            .iter()
            .map(|r| r.2)
            .max()
            .map(|m| m as usize + 1)
            .unwrap_or(1);
        scene.instances.push(Instance {
            mesh,
            transform,
            origin,
            materials,
            slot_alpha: vec![1.0; slots],
            slot_light: vec![1.0; slots],
            slot_night: vec![1.0; slots],
            visible: true,
            slot_uv: vec![[0.0; 2]; slots],
            interior: 0.0,
            interior_lamps: 0,
            base: 0,
            bounds: InstanceBounds::default(),
            surface: true,
            presurface: false,
            render_phase: RenderPhase::Normal,
            surface_bias: true,
            blend_sort_origin: None,
            lod: (0.0, f32::MAX),
            blob: false,
            ground_layer: false,
            decal: false,
            object_radius: 0.0,
            detail: 1.0,
            any_distance: false,
            near_only: None,
            mirror_only: false,
            omsi_caster: false,
            ordered: false,
            casts_shadow: false,
            roof: None,
        });
        scene.instances.len() - 1
    }

    /// A vehicle's shadow blob: a surface instance (see `Instance::blob`).
    pub fn add_shadow_blob_instance(
        &self,
        scene: &mut Scene,
        mesh: MeshId,
        origin: DVec3,
        transform: Mat4,
        materials: Vec<MaterialId>,
    ) -> usize {
        let i = self.add_surface_instance(scene, mesh, origin, transform, materials);
        scene.instances[i].blob = true;
        i
    }

    /// Note that instance `i` needs its per-draw entries rewritten.
    fn mark_changed(scene: &mut Scene, i: usize) {
        if scene.dirty || i >= scene.uploaded_instances {
            return;
        }
        if scene.changed_mark.len() < scene.instances.len() {
            scene.changed_mark.resize(scene.instances.len(), false);
        }
        if !scene.changed_mark[i] {
            scene.changed_mark[i] = true;
            scene.changed.push(i);
        }
    }

    /// `n` consecutive slots for a vehicle's `[interiorlight]` lamps; give them back with
    /// [`Renderer::free_interior_lights`].
    pub fn alloc_interior_lights(&self, scene: &mut Scene, n: u32) -> u32 {
        if let Some(k) = scene.interior_free.iter().position(|f| f.1 >= n) {
            let (first, len) = scene.interior_free[k];
            if len == n {
                scene.interior_free.remove(k);
            } else {
                scene.interior_free[k] = (first + n, len - n);
            }
            return first;
        }
        let first = scene.interior_lights.len() as u32;
        scene
            .interior_lights
            .extend((0..n).map(|_| PointLight { intensity: 0.0, ..Default::default() }));
        first
    }

    pub fn free_interior_lights(&self, scene: &mut Scene, first: u32, n: u32) {
        for l in scene
            .interior_lights
            .iter_mut()
            .skip(first as usize)
            .take(n as usize)
        {
            l.intensity = 0.0;
        }
        scene.interior_free.push((first, n));
    }

    pub fn set_interior_light(&self, scene: &mut Scene, slot: u32, light: PointLight) {
        if let Some(l) = scene.interior_lights.get_mut(slot as usize) {
            *l = light;
        }
    }

    /// Which lamps light an instance: `count` (up to `MAX_LAMPS_PER_MESH`) slots from `first` (see
    /// `Instance::interior_lamps`).
    pub fn set_interior_lamps(&self, scene: &mut Scene, instance: usize, first: u32, count: u32) {
        let code = if count == 0 { 0 } else { first * LAMP_CODE_STRIDE + count.min(MAX_LAMPS_PER_MESH) };
        let i = &mut scene.instances[instance];
        if i.interior_lamps != code {
            i.interior_lamps = code;
            Self::mark_changed(scene, instance);
        }
    }

    /// Interior light brightness of an instance (warm ambient inside vehicles).
    /// Whether the model marks the instance's mesh `[shadow]` (see [`Instance::omsi_caster`]).
    pub fn set_omsi_caster(&self, scene: &mut Scene, instance: usize, on: bool) {
        if let Some(i) = scene.instances.get_mut(instance) {
            i.omsi_caster = on;
        }
    }

    /// Draw an instance with all its slots in model order (see [`Instance::ordered`]).
    pub fn set_ordered(&self, scene: &mut Scene, instance: usize, on: bool) {
        if let Some(i) = scene.instances.get_mut(instance) {
            i.ordered = on;
        }
    }

    /// Let an instance cast a sun shadow or not (see [`Instance::casts_shadow`]).
    pub fn set_casts_shadow(&self, scene: &mut Scene, instance: usize, on: bool) {
        if let Some(i) = scene.instances.get_mut(instance) {
            i.casts_shadow = on;
        }
    }

    /// The vehicle roof height of an instance (see [`Instance::roof`]).
    pub fn set_roof(&self, scene: &mut Scene, instance: usize, roof: Option<f32>) {
        if let Some(i) = scene.instances.get_mut(instance) {
            if i.roof != roof {
                i.roof = roof;
                Self::mark_changed(scene, instance);
            }
        }
    }

    /// Show an instance only in the mirrors (see [`Instance::mirror_only`]).
    pub fn set_mirror_only(&self, scene: &mut Scene, instance: usize, on: bool) {
        if let Some(i) = scene.instances.get_mut(instance) {
            i.mirror_only = on;
        }
    }

    pub fn set_interior(&self, scene: &mut Scene, instance: usize, interior: f32) {
        let i = &mut scene.instances[instance];
        if (i.interior - interior).abs() > 1e-4 {
            i.interior = interior;
            Self::mark_changed(scene, instance);
        }
    }

    /// Per-slot lightmap strengths (`[matl_lightmap]` variables).
    pub fn set_slot_light(&self, scene: &mut Scene, instance: usize, light: &[f32]) {
        let i = &mut scene.instances[instance];
        let mut changed = false;
        for (k, l) in i.slot_light.iter_mut().enumerate() {
            let v = light.get(k).copied().unwrap_or(1.0);
            changed |= *l != v;
            *l = v;
        }
        if changed {
            Self::mark_changed(scene, instance);
        }
    }

    /// Per-slot `[matl_item]` activity (night maps of `[matl_change]` variants).
    pub fn set_slot_night(&self, scene: &mut Scene, instance: usize, night: &[f32]) {
        let i = &mut scene.instances[instance];
        let mut changed = false;
        for (k, l) in i.slot_night.iter_mut().enumerate() {
            let v = night.get(k).copied().unwrap_or(1.0);
            changed |= *l != v;
            *l = v;
        }
        if changed {
            Self::mark_changed(scene, instance);
        }
    }

    /// Restrict an instance to a screen-size range (LOD levels).
    pub fn set_lod_range(&self, scene: &mut Scene, instance: usize, min: f32, max: f32) {
        scene.instances[instance].lod = (min, max);
    }

    /// Judge this mesh as part of a whole object, the way the original does: the object is
    /// drawn or left out as one - by its screen size against `performance_minObjSize` times
    /// its `[detail_factor]`, and beyond `performance_maxObjDist` unless it says
    /// `[noDistanceCheck]` - and its `[LOD]` level is chosen by the object's size, not by
    /// each mesh's own. `radius` (m, in the instance's own scale) holds the whole object
    /// about the instance's origin. Judged mesh by mesh, the small parts of a bus (mirrors,
    /// wipers, lamps) went missing at a distance while its body was still there, and the
    /// meshes of one LOD level switched at different distances.
    pub fn set_object_culling(
        &self,
        scene: &mut Scene,
        instance: usize,
        radius: f32,
        detail: f32,
        any_distance: bool,
    ) {
        let i = &mut scene.instances[instance];
        i.object_radius = radius.max(0.0);
        i.detail = if detail > 0.0 { detail } else { 1.0 };
        i.any_distance = any_distance;
    }

    /// Draw an instance only while the camera stands in `area` (world x0, y0, x1, y1 on the
    /// ground; None: wherever it is loaded).
    pub fn set_near_only(&self, scene: &mut Scene, instance: usize, area: Option<[f64; 4]>) {
        scene.instances[instance].near_only = area;
    }

    /// Change an instance transform (re-uploaded on the next `prepare`).
    pub fn set_transform(
        &self,
        scene: &mut Scene,
        instance: usize,
        origin: DVec3,
        transform: Mat4,
    ) {
        let i = &mut scene.instances[instance];
        if i.transform != transform || i.origin != origin {
            i.transform = transform;
            i.origin = origin;
            Self::mark_changed(scene, instance);
        }
    }

    /// Choose the render origin. Everything is re-uploaded when it moves.
    pub fn set_render_origin(&self, scene: &mut Scene, origin: DVec3) {
        if scene.render_origin != origin {
            scene.render_origin = origin;
            scene.dirty = true;
        }
    }

    /// Bounding sphere of an instance relative to the render origin: the mesh's sphere
    /// through the instance transform (the model matrix is that transform moved by the
    /// origin, so no matrix product is needed), the radius scaled by the largest axis.
    fn bounding_sphere(scene: &Scene, i: &Instance) -> (Vec3, f32) {
        let b = if scene.cache_bounds { i.bounds } else { InstanceBounds::new(&scene.meshes[i.mesh], i.transform) };
        (b.centre + (i.origin - scene.render_origin).as_vec3(), b.radius)
    }

    fn instance_scale(scene: &Scene, i: &Instance) -> f32 {
        if scene.cache_bounds { i.bounds.scale } else { transform_scale(i.transform) }
    }

    fn mesh_bounds_changed(scene: &mut Scene, mesh: MeshId) {
        scene.bounds_meshes.resize(scene.meshes.len(), false);
        scene.bounds_meshes[mesh] = true;
        scene.bounds_dirty = true;
    }

    fn prepare_bounds(scene: &mut Scene) {
        if !scene.cache_bounds { return; }
        if scene.dirty {
            for i in &mut scene.instances {
                i.bounds = InstanceBounds::new(&scene.meshes[i.mesh], i.transform);
            }
        } else {
            for i in &mut scene.instances[scene.uploaded_instances..] {
                i.bounds = InstanceBounds::new(&scene.meshes[i.mesh], i.transform);
            }
            for &idx in &scene.changed {
                if let Some(i) = scene.instances.get_mut(idx) {
                    i.bounds = InstanceBounds::new(&scene.meshes[i.mesh], i.transform);
                }
            }
            if scene.bounds_dirty {
                // Skinning can alter a shared mesh without changing any model matrix.
                // Scan once per pose update, rather than once per shadow/mirror view.
                for i in &mut scene.instances {
                    if scene.bounds_meshes.get(i.mesh).copied().unwrap_or(false) {
                        i.bounds = InstanceBounds::new(&scene.meshes[i.mesh], i.transform);
                    }
                }
            }
        }
        if scene.bounds_dirty {
            scene.bounds_meshes.fill(false);
            scene.bounds_dirty = false;
        }
    }

    /// A dynamic alpha value is never allowed to fade an opaque body panel; only
    /// blended materials follow the script value. An alpha-tested slot is cut out by its
    /// texture or transmap alone: the Thüringer Wald buses put `[alphascale]
    /// Envir_Brightness` on their transmapped body and roof (`[matl_alpha] 1`), which is 0
    /// at night, and scaled by it the whole roof went at dusk - with alpha to coverage
    /// under MSAA the colour pass drew none of its samples - while in OMSI it stays.
    /// Except a blended slot with a declared `[matl_transmap]`: Omsi.exe's transmap stage
    /// (0x7ffeb7) replaces the diffuse alpha that `[alphascale]` scaled (0x7feb8f).
    pub fn clamp_slot_alpha(alpha: f32, material_alpha: AlphaMode, transmap_declared: bool) -> f32 {
        match material_alpha {
            AlphaMode::Opaque | AlphaMode::Test => 1.0,
            AlphaMode::Blend if transmap_declared => 1.0,
            AlphaMode::Blend => alpha,
        }
    }

    /// Change the dynamic parameters of an instance. `slot_alpha` entries beyond the mesh's
    /// slot count are ignored; missing ones keep 1.0.
    pub fn set_params(
        &self,
        scene: &mut Scene,
        instance: usize,
        slot_alpha: &[f32],
        visible: bool,
        slot_uv: &[[f32; 2]],
    ) {
        let i = &mut scene.instances[instance];
        let mut changed = i.visible != visible;
        for (k, a) in i.slot_alpha.iter_mut().enumerate() {
            let requested = slot_alpha.get(k).copied().unwrap_or(1.0);
            let v = i
                .materials
                .get(k)
                .and_then(|id| scene.materials.get(*id))
                .map_or(requested, |m| {
                    Self::clamp_slot_alpha(requested, m.alpha, m.transmap_declared())
                });
            changed |= *a != v;
            *a = v;
        }
        for (k, u) in i.slot_uv.iter_mut().enumerate() {
            let v = slot_uv.get(k).copied().unwrap_or([0.0; 2]);
            changed |= *u != v;
            *u = v;
        }
        i.visible = visible;
        if changed {
            Self::mark_changed(scene, instance);
        }
    }

    /// The ambient-occlusion textures for a target of this size (rebuilt on resize).
    fn ensure_ao(&mut self, w: u32, h: u32) -> bool {
        if self.ao.as_ref().map(|a| a.size == (w, h)).unwrap_or(false) {
            return false;
        }
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("prepass depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
                | if self.puddles.is_some() { wgpu::TextureUsages::COPY_SRC } else { wgpu::TextureUsages::empty() },
            view_formats: &[],
        });
        // the AO itself at half size: four times fewer pixels, and the blur hides the rest
        let half = wgpu::Extent3d {
            width: w.div_ceil(2),
            height: h.div_ceil(2),
            depth_or_array_layers: 1,
        };
        let mk = |label: &str| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: half,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // r the occlusion, g the view depth (m) it was worked out at: the main
                // pass upsamples it by depth, not bilinearly (see `ao_at` in shader.wgsl)
                format: wgpu::TextureFormat::Rg16Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let ao = mk("ssao");
        let blur = mk("ssao blur");
        let depth_view = depth.create_view(&Default::default());
        let ao_view = ao.create_view(&Default::default());
        let blur_view = blur.create_view(&Default::default());
        let bg = |label: &str, tex: &wgpu::TextureView| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.ao_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.ao_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&depth_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(tex),
                    },
                ],
            })
        };
        // the SSAO pass reads no AO texture, but the layout wants one: bind the blur target
        let ssao_bg = bg("ssao", &blur_view);
        let blur_bg = bg("ssao blur", &ao_view);
        self.ao = Some(AoTargets {
            size: (w, h),
            depth_view,
            ao_view,
            blur_view,
            ssao_bg,
            blur_bg,
        });
        true
    }

    /// Before a new size's targets are made: let go of the sizes nobody asked for lately
    /// (dragging the window's edge made a full set per frame, and the old caches, cleared
    /// only past a dozen entries, could hold gigabytes meanwhile), and past a handful of
    /// sizes the least recently used (the window, the scaled picture and the mirrors are
    /// asked for every frame they are drawn).
    fn evict_targets(&mut self) {
        const STALE: std::time::Duration = std::time::Duration::from_millis(250);
        const KEEP: usize = 10;
        let now = std::time::Instant::now();
        let mut uses: Vec<((u32, u32), std::time::Instant)> = self.target_use.iter().map(|(k, t)| (*k, *t)).collect();
        uses.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
        let keep: std::collections::HashSet<(u32, u32)> = uses.iter().filter(|(_, t)| now.duration_since(*t) < STALE).take(KEEP).map(|(k, _)| *k).collect();
        self.target_use.retain(|k, _| keep.contains(k));
        self.scale_targets.retain(|k, _| keep.contains(k));
        self.msaa_targets.retain(|k, _| keep.contains(k));
        self.hdr_targets.retain(|k, _| keep.contains(k));
    }

    /// The multisampled colour and depth attachments for a target of this size.
    fn msaa_targets(&mut self, w: u32, h: u32) -> (wgpu::TextureView, wgpu::TextureView) {
        self.target_use.insert((w, h), std::time::Instant::now());
        if let Some(t) = self.msaa_targets.get(&(w, h)) {
            return t.clone();
        }
        self.evict_targets();
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let color = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("msaa colour"),
            size,
            mip_level_count: 1,
            sample_count: self.options.msaa,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("msaa depth"),
            size,
            mip_level_count: 1,
            sample_count: self.options.msaa,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let t = (
            color.create_view(&Default::default()),
            depth.create_view(&Default::default()),
        );
        self.msaa_targets.insert((w, h), t.clone());
        t
    }

    /// The HDR colour targets of the enhanced path for this size, with the glow's levels,
    /// the tone-mapped picture FXAA reads and the post passes' bind groups.
    fn hdr_targets(&mut self, w: u32, h: u32) -> bool {
        self.target_use.insert((w, h), std::time::Instant::now());
        if self.hdr_targets.contains_key(&(w, h)) {
            return false;
        }
        self.evict_targets();
        let fmt = wgpu::TextureFormat::Rgba16Float;
        let target = |label: &str, tw: u32, th: u32, format: wgpu::TextureFormat, samples: u32| {
            let usage = if samples > 1 {
                wgpu::TextureUsages::RENDER_ATTACHMENT
            } else {
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC
            };
            self.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: tw.max(1),
                        height: th.max(1),
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: samples,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let msaa_view =
            (self.options.msaa > 1).then(|| target("hdr msaa", w, h, fmt, self.options.msaa));
        let view = target("hdr", w, h, fmt, 1);
        let mask_msaa = (self.options.msaa > 1).then(|| target("screen mask msaa", w, h, MASK_FORMAT, self.options.msaa));
        let mask = target("screen mask", w, h, MASK_FORMAT, 1);
        // the glow: halving until the smallest level is a few dozen pixels across
        let levels = GLOW_LEVELS
            .min((w.min(h).max(16) as f32).log2() as usize - 3)
            .max(1);
        let down: Vec<wgpu::TextureView> = (1..=levels)
            .map(|k| target("glow down", w >> k, h >> k, fmt, 1))
            .collect();
        let up: Vec<wgpu::TextureView> = (1..=levels)
            .map(|k| target("glow up", w >> k, h >> k, fmt, 1))
            .collect();
        let ldr = target("tone mapped", w, h, wgpu::TextureFormat::Rgba8Unorm, 1);
        let bg = |src: &wgpu::TextureView, base: &wgpu::TextureView, adapt: &wgpu::TextureView| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post"),
                layout: &self.post_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.post_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(src),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.post_sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(base),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::TextureView(adapt),
                    },
                ],
            })
        };
        let none = &self.white_texture.view;
        // (the first level of the glow reads the screen mask as its `t_base`: no light from
        // the screens)
        let down_bg: Vec<wgpu::BindGroup> = (0..levels)
            .map(|i| bg(if i == 0 { &view } else { &down[i - 1] }, if i == 0 { &mask } else { none }, none))
            .collect();
        let up_bg: Vec<wgpu::BindGroup> = (0..levels)
            .map(|i| {
                bg(
                    if i + 1 == levels {
                        &down[i]
                    } else {
                        &up[i + 1]
                    },
                    &down[i],
                    none,
                )
            })
            .collect();
        let meter_bg = bg(&down[levels - 1], none, none);
        let tonemap_bg = [
            bg(&view, &up[0], &self.adapt_views[0]),
            bg(&view, &up[0], &self.adapt_views[1]),
        ];
        // (FXAA reads the screen mask as its `t_base` and leaves the screens as they are)
        let fxaa_bg = bg(&ldr, &mask, none);
        let classic_bg = self.picture_group(&view);
        self.hdr_targets.insert(
            (w, h),
            HdrTargets {
                msaa_view,
                view,
                mask_msaa,
                mask,
                down,
                up,
                ldr,
                down_bg,
                up_bg,
                meter_bg,
                tonemap_bg,
                fxaa_bg,
                classic_bg,
                puddles: None,
            },
        );
        true
    }

    /// Take in a newly computed sky: its table goes to the GPU, the probe is redrawn.
    fn install_sky(&mut self, st: atmosphere::SkyState) {
        let mut bytes: Vec<u8> = Vec::with_capacity(st.lut.len() * 8);
        for texel in &st.lut {
            for v in texel {
                bytes.extend_from_slice(&atmosphere::f16_bits(*v).to_le_bytes());
            }
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.sky_lut,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atmosphere::SKY_LUT_W * 8),
                rows_per_image: Some(atmosphere::SKY_LUT_H),
            },
            wgpu::Extent3d {
                width: atmosphere::SKY_LUT_W,
                height: atmosphere::SKY_LUT_H,
                depth_or_array_layers: 1,
            },
        );
        if omsi_cfg::env::var_os("OMSI_DEBUG_SKY").is_some() {
            log::info!("sky: sun {:?} (altitude {:.1}°) sky {:?} ground {:?} exposure {:.3} table scale {:.4} haze {:.2} overcast {:.2} rain {:.2} sun visibility {:.2}", st.sun, st.input.sun_dir.z.asin().to_degrees(), st.sky_horizontal, st.ground, st.exposure, st.lut_scale, st.input.haze, st.input.overcast, st.input.rain, st.input.sun_visibility);
        }
        if let Some(p) = self.probe.as_mut() {
            p.age = u32::MAX;
        }
        self.sky_state = Some(st);
    }

    /// The enhanced path's light for this frame: the sky (recomputed when the sun or the
    /// weather has moved on), the exposure following it, the sky table and the uniform.
    /// Returns whether the reflection probe is to be drawn this frame.
    fn prepare_enhanced(&mut self, lighting: &Lighting, cam_rel: Vec3, ro: DVec3, dt: f32) -> bool {
        let s = lighting.sun_dir.normalize_or_zero();
        // haze: the weather's visibility below a few kilometres thickens the aerosol
        let visibility = 2.3 / lighting.fog_density.max(1e-6);
        let haze = (8000.0 / visibility).clamp(1.0, 6.0) + 2.0 * lighting.rain;
        // rain and snow fall from a closed deck: whatever the cloud type says, the sun is
        // gone and the sky is the grey dome (a low sun scattered orange in the snowfall)
        let wet_cover = (lighting.rain * 1.5).clamp(0.0, 1.0);
        let sun_visibility = lighting.sun_intensity.clamp(0.0, 1.0) * (1.0 - wet_cover);
        let input = atmosphere::SkyInput {
            sun_dir: s,
            sun_visibility,
            overcast: lighting.overcast.clamp(0.0, 1.0).max(wet_cover),
            haze,
            rain: lighting.rain.clamp(0.0, 1.0),
            ground_albedo: 0.2 + 0.45 * lighting.snow.clamp(0.0, 1.0),
            tint: lighting.envir_tint,
        };
        // A new sky takes a few milliseconds: it is computed on a helper thread and taken in
        // when it is ready. A picture on its own, and the first frame, wait for it.
        if let Some((_, rx)) = &self.sky_job {
            match rx.try_recv() {
                Ok(st) => {
                    self.sky_job = None;
                    self.install_sky(st);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.sky_job = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let pending = self
            .sky_job
            .as_ref()
            .map(|j| j.0)
            .or(self.sky_state.as_ref().map(|st| st.input));
        if pending
            .map(|i| sky_input_differs(&i, &input))
            .unwrap_or(true)
        {
            if self.instant_exposure || self.sky_state.is_none() {
                self.sky_job = None;
                self.install_sky(atmosphere::SkyState::compute(&input));
            } else {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(atmosphere::SkyState::compute(&input));
                });
                self.sky_job = Some((input, rx));
            }
        }
        let input_overcast = input.overcast;
        let st = self.sky_state.as_ref().expect("sky state");
        // the exposure follows the light over a second or two (in log space); an offscreen
        // picture and the first frame take it at once
        let target = st.exposure.max(1e-6).ln();
        let log_exposure = match self.exposure {
            Some(e) if !self.instant_exposure && dt > 0.0 => {
                e + (target - e) * (1.0 - (-dt / 1.5).exp())
            }
            _ => target,
        };
        self.exposure = Some(log_exposure);
        let pre = log_exposure.exp();
        // the fog's in-scattered light: a white sphere's average in this light
        let axes = [Vec3::X, -Vec3::X, Vec3::Y, -Vec3::Y, Vec3::Z, -Vec3::Z];
        let avg = axes
            .iter()
            .map(|a| atmosphere::sh_irradiance(&st.sh, *a))
            .fold(Vec3::ZERO, |a, b| a + b)
            / 6.0;
        let fog_rgb = avg * 0.9 / std::f32::consts::PI;
        // the weather's own fog (vanilla's density, which the culling uses as well); a
        // clear day's air is the sky model's
        let weather_fog = if lighting.fog_density > 1e-4 {
            lighting.fog_density
        } else {
            0.0
        };
        // (the air near the ground: the Rayleigh part and half the aerosols of the sky
        // model's, whose layer is thinner where a street is than its average)
        let clear_air = 1.3e-5 + 2.2e-5 * haze;
        // the fog lies on the ground under the player's vehicle, or just under the camera
        let base = match lighting.fog_base.or(lighting.inside.map(|v| v.0.z)) {
            Some(z) => (z - ro.z) as f32,
            None => cam_rel.z - 2.0,
        };
        let mut sh = [[0.0f32; 4]; 9];
        for (k, c) in st.sh.iter().enumerate() {
            sh[k] = c.extend(0.0).to_array();
        }
        // the probe is drawn every half second or so, and at once for a new sky
        let instant = self.instant_exposure;
        let (redraw, probe_scale) = match self.probe.as_mut() {
            Some(p) => {
                let redraw = instant || p.age >= 30;
                if redraw {
                    p.age = 0;
                    p.scale = st.lut_scale;
                } else {
                    p.age += 1;
                }
                (redraw, p.scale)
            }
            None => (false, 1.0),
        };
        // the sky cube's eye (see Probe::cube_eye): the correction for a camera that moved
        // away from it is exact for the base of the clouds, and wrong by up to the layer's
        // depth over their distance; a climb shows far sooner than driving along
        let cam_w = ro + cam_rel.as_dvec3();
        let eye_off = match self.probe.as_mut() {
            Some(p) => {
                let to_clouds = (1400.0 - cam_rel.z as f64).max(120.0);
                let far = match p.cube_eye {
                    Some(e) if p.cube_filled => {
                        let m = cam_w - e;
                        // (a third of what it was: flying the free camera fast, the clouds
                        // drifted with the old cube for 400 m and then jumped back into place)
                        (m.truncate().length() * 0.1 + m.z.abs()) / to_clouds > 0.01
                    }
                    _ => true,
                };
                if far {
                    p.cube_eye = Some(cam_w);
                    p.cube_recapture = p.cube_filled;
                }
                (p.cube_eye.unwrap_or(cam_w) - cam_w).as_vec3()
            }
            None => Vec3::ZERO,
        };
        let u = EnhancedUniform {
            // (self-lit surfaces at their own brightness after the metering, see ExposureLog)
            exposure: [pre, 2f32.powf(-self.exposure_log.as_ref().map(|l| l.ev).unwrap_or(0.0)).clamp(0.7, 1.6), pre * WINDOW_RADIANCE, 1.6],
            sun: st.sun.extend(SUN_RADIUS).to_array(),
            sh,
            ground: st.ground.extend(st.lut_scale).to_array(),
            fog: [weather_fog, 1.0 / 300.0, base, clear_air],
            fog_color: fog_rgb.extend(probe_scale).to_array(),
            weather: [
                lighting.wetness.clamp(0.0, 1.0),
                lighting.snow.clamp(0.0, 1.0),
                lighting.rain.clamp(0.0, 1.0),
                input_overcast,
            ],
            lights: [PROBE_MIPS as f32, LAMP_E, CABIN_E, sun_visibility],
            sun_disc: st.sun_disc.extend(dt).to_array(),
            debug: [
                debug_view(),
                omsi_cfg::env::var("OMSI_PUDDLE_F0")
                    .ok().and_then(|v| v.parse::<f32>().ok())
                    .filter(|v| v.is_finite()).unwrap_or(0.08).clamp(0.02, 0.2),
                omsi_cfg::env::var("OMSI_ENV_PHOTO")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1.0),
                0.0,
            ],
            eye: eye_off.extend(0.0).to_array(),
            // x how bright an LED panel's dots burn (see `MaterialExtra::led`; the settings'
            // 16 levels give 0 = off .. 3.75), y whether the LED panels' `\S:n` masks keep
            // their mip chain (0: at full resolution, the dots stay visible when small)
            led: [lighting.led_glow, lighting.led_mips, 0.0, 0.0],
        };
        self.queue
            .write_buffer(&self.enh_buf, 0, bytemuck::bytes_of(&u));
        redraw
    }

    #[allow(dead_code)]
    fn ensure_depth(&mut self, w: u32, h: u32) {
        if let Some((_, _, dw, dh)) = &self.depth {
            if *dw == w && *dh == h {
                return;
            }
        }
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        self.depth = Some((tex, view, w, h));
    }

    /// Upload instance transforms (call after building or changing the scene).
    /// The per-draw entries of one instance: one model matrix and two parameter vectors
    /// per material slot.
    fn instance_entries(
        i: &Instance,
        ro: DVec3,
        mats: &mut Vec<[[f32; 4]; 4]>,
        params: &mut Vec<[f32; 4]>,
    ) {
        let m =
            (Mat4::from_translation((i.origin - ro).as_vec3()) * i.transform).to_cols_array_2d();
        let vis = if i.visible { 1.0 } else { 0.0 };
        for (k, a) in i.slot_alpha.iter().enumerate() {
            mats.push(m);
            let uv = i.slot_uv.get(k).copied().unwrap_or([0.0; 2]);
            params.push([*a, vis, uv[0], uv[1]]);
            params.push([
                i.slot_light.get(k).copied().unwrap_or(1.0),
                i.slot_night.get(k).copied().unwrap_or(1.0),
                // (a lamp code is 1 or more, a plain brightness below 1)
                if i.interior_lamps != 0 {
                    i.interior_lamps as f32
                } else {
                    i.interior.min(0.99)
                },
                // the surface flag: 1 ground, 2 a vehicle's shadow blob (no snow on it),
                // 1.25 a legacy pulled decal, 0.9 an OMSI-ordered surface (weather
                // classification without view-space pull), 0.75 a painted ground layer;
                // below -500 a vehicle part, -(5000 + the roof height relative to origin)
                if let Some(roof) = i.roof.filter(|_| !i.blob && !i.surface) {
                    let z = (i.origin - ro).z as f32 + i.transform.transform_point3(Vec3::new(0.0, 0.0, roof)).z;
                    -(5000.0 + z.clamp(-4000.0, 4000.0))
                } else {
                    surface_instance_code(
                        i.blob,
                        i.ground_layer,
                        i.decal,
                        i.surface,
                        i.surface_bias,
                    )
                },
            ]);
        }
    }

    pub fn prepare(&self, scene: &mut Scene) {
        Self::prepare_bounds(scene);
        scene.bind_groups.clear();
        if !scene.dirty
            && scene.instances.len() > scene.uploaded_instances
            && scene.model_buf.is_some()
        {
            // new instances: append their entries if the buffers have room, else rebuild
            let ro = scene.render_origin;
            let mut mats: Vec<[[f32; 4]; 4]> = Vec::new();
            let mut params: Vec<[f32; 4]> = Vec::new();
            let first = scene.uploaded_instances;
            let mut base = scene.uploaded_entries;
            for i in scene.instances[first..].iter_mut() {
                i.base = base;
                Self::instance_entries(i, ro, &mut mats, &mut params);
                base = scene.uploaded_entries + mats.len() as u32;
            }
            let (buf, pbuf) = (
                scene.model_buf.as_ref().unwrap(),
                scene.params_buf.as_ref().unwrap(),
            );
            let mo = scene.uploaded_entries as u64 * 64;
            let po = scene.uploaded_entries as u64 * 32;
            let mb: &[u8] = bytemuck::cast_slice(&mats);
            let pb: &[u8] = bytemuck::cast_slice(&params);
            if mo + mb.len() as u64 <= buf.size()
                && po + pb.len() as u64 <= pbuf.size()
                && scene.cpu_models.len() == scene.uploaded_entries as usize
            {
                if !mb.is_empty() {
                    buf.write(&self.queue, mo, mb);
                    pbuf.write(&self.queue, po, pb);
                }
                scene.cpu_models.extend_from_slice(&mats);
                scene.cpu_params.extend_from_slice(&params);
                scene.uploaded_instances = scene.instances.len();
                scene.uploaded_entries = base;
            } else {
                scene.dirty = true;
            }
        }
        if !scene.dirty {
            // only what moved or changed since the last frame
            if !scene.changed.is_empty() {
                if omsi_cfg::env::var_os("OMSI_DEBUG_DRAWS").is_some() {
                    log::info!(
                        "prepare: {} changed instances of {}",
                        scene.changed.len(),
                        scene.instances.len()
                    );
                }
                if let (Some(buf), Some(pbuf)) = (&scene.model_buf, &scene.params_buf) {
                    let ro = scene.render_origin;
                    // The changed entries go into the CPU copy; the ranges they span are
                    // uploaded merged - the meshes of one vehicle or person sit next to each
                    // other, and neighbouring vehicles are sent together with whatever lies
                    // between them when that is not much.
                    const MERGE_GAP: u32 = 4096;
                    scene.changed.sort_unstable_by_key(|&i| {
                        scene.instances.get(i).map(|x| x.base).unwrap_or(u32::MAX)
                    });
                    let mut mats: Vec<[[f32; 4]; 4]> = Vec::new();
                    let mut params: Vec<[f32; 4]> = Vec::new();
                    let mut ranges: Vec<(u32, u32)> = Vec::new();
                    for &idx in &scene.changed {
                        let Some(i) = scene.instances.get(idx) else {
                            continue;
                        };
                        mats.clear();
                        params.clear();
                        Self::instance_entries(i, ro, &mut mats, &mut params);
                        let (b, n) = (i.base as usize, mats.len());
                        if b + n > scene.cpu_models.len() || (b + n) * 2 > scene.cpu_params.len() {
                            continue;
                        }
                        scene.cpu_models[b..b + n].copy_from_slice(&mats);
                        scene.cpu_params[b * 2..(b + n) * 2].copy_from_slice(&params);
                        let (start, end) = (b as u32, (b + n) as u32);
                        match ranges.last_mut() {
                            Some(r) if start <= r.1 + MERGE_GAP => r.1 = r.1.max(end),
                            _ => ranges.push((start, end)),
                        }
                    }
                    for (start, end) in ranges {
                        let mb: &[u8] =
                            bytemuck::cast_slice(&scene.cpu_models[start as usize..end as usize]);
                        let pb: &[u8] = bytemuck::cast_slice(
                            &scene.cpu_params[start as usize * 2..end as usize * 2],
                        );
                        let (mo, po) = (start as u64 * 64, start as u64 * 32);
                        if mo + mb.len() as u64 <= buf.size() && po + pb.len() as u64 <= pbuf.size()
                        {
                            buf.write(&self.queue, mo, mb);
                            pbuf.write(&self.queue, po, pb);
                        }
                    }
                }
                for &idx in &scene.changed {
                    if let Some(m) = scene.changed_mark.get_mut(idx) {
                        *m = false;
                    }
                }
                scene.changed.clear();
            }
            return;
        }
        scene.changed.clear();
        scene.changed_mark.clear();
        // one entry per (instance, material slot) so every draw call has its own parameters
        let mut mats: Vec<[[f32; 4]; 4]> = Vec::new();
        let mut params: Vec<[f32; 4]> = Vec::new();
        let ro = scene.render_origin;
        for i in scene.instances.iter_mut() {
            i.base = mats.len() as u32;
            Self::instance_entries(i, ro, &mut mats, &mut params);
        }
        if mats.is_empty() {
            mats.push(Mat4::IDENTITY.to_cols_array_2d());
            params.push([1.0, 1.0, 0.0, 0.0]);
            params.push([1.0, 1.0, 0.0, 0.0]);
        }
        scene.uploaded_instances = scene.instances.len();
        scene.uploaded_entries = mats.len() as u32;
        scene.cpu_models = mats;
        scene.cpu_params = params;
        let bytes: &[u8] = bytemuck::cast_slice(&scene.cpu_models);
        let pbytes: &[u8] = bytemuck::cast_slice(&scene.cpu_params);
        if let (Some(buf), Some(pbuf), Some(_)) = (
            &scene.model_buf,
            &scene.params_buf,
            &scene.camera_bind_group,
        ) {
            if buf.size() >= bytes.len() as u64 && pbuf.size() >= pbytes.len() as u64 {
                buf.write(&self.queue, 0, bytes);
                pbuf.write(&self.queue, 0, pbytes);
                scene.dirty = false;
                return;
            }
        }
        // a third more room than needed, so that the cars and people spawned over the
        // next minutes are appended instead of forcing a rebuild each time
        let cap = |n: usize| (((n as f64 * 1.35) as u64 + 65536).max(256)).div_ceil(256) * 256;
        let model_buf = GpuArray::new(&self.device, "models", cap(bytes.len()), 16, true);
        let params_buf = GpuArray::new(&self.device, "params", cap(pbytes.len()), 16, true);
        model_buf.write(&self.queue, 0, bytes);
        params_buf.write(&self.queue, 0, pbytes);
        scene.model_buf = Some(model_buf);
        scene.params_buf = Some(params_buf);
        self.rebuild_camera_bind_group(scene);
        scene.dirty = false;
    }

    fn rebuild_camera_bind_group(&self, scene: &mut Scene) {
        let (Some(model_buf), Some(params_buf), Some(light_buf), Some(grid_buf), Some(draw_buf)) = (
            &scene.model_buf,
            &scene.params_buf,
            &scene.light_buf,
            &scene.grid_buf,
            &scene.draw_buf,
        ) else {
            return;
        };
        let mut entries = vec![
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.camera_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: model_buf.binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&self.shadow_view),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::Sampler(&self.shadow_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::TextureView(&self.shadow_view_far),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: wgpu::BindingResource::TextureView(
                        self.ao
                            .as_ref()
                            .map(|a| &a.blur_view)
                            .unwrap_or(&self.white_texture.view),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: wgpu::BindingResource::Sampler(&self.ao_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 10,
                    resource: draw_buf.binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 11,
                    resource: self.enh_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 13,
                    resource: wgpu::BindingResource::Sampler(&self.lin_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 18,
                    resource: wgpu::BindingResource::TextureView(&self.lm_atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 19,
                    resource: self.lm_uniform.as_entire_binding(),
                },
        ];
        if camera_enhanced_textures() {
            let probe = self.probe.as_ref().expect("reflection probe");
            entries.push(wgpu::BindGroupEntry { binding: 12, resource: wgpu::BindingResource::TextureView(&probe.view) });
            entries.push(wgpu::BindGroupEntry { binding: 14, resource: wgpu::BindingResource::TextureView(&self.sky_lut_view) });
            entries.push(wgpu::BindGroupEntry { binding: 17, resource: wgpu::BindingResource::TextureView(&probe.cube_view) });
        }
        // (the point lights where the device has storage buffers for them)
        if array_path() != ArrayPath::NoStorage {
            entries.push(wgpu::BindGroupEntry { binding: 3, resource: light_buf.binding() });
            entries.push(wgpu::BindGroupEntry { binding: 4, resource: grid_buf.binding() });
        }
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("camera"),
            layout: &self.camera_layout,
            entries: &entries,
        });
        scene.camera_bind_group = Some(bg);
        let sbg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shadow camera"),
            layout: &self.shadow_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.camera_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: model_buf.binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 10,
                    resource: draw_buf.binding(),
                },
            ],
        });
        scene.shadow_bind_group = Some(sbg);
    }

    /// Upload this frame's point lights into the light grid around the camera. Returns the
    /// grid origin (render-origin relative). `enhanced`: the frame is drawn by the enhanced
    /// path, which takes a headlight as one spot light where the vanilla path takes three
    /// points (`LightMode`). Its mirrors (the vanilla shader) and its picture share the
    /// enhanced list, so that it is not sent twice a frame: the vanilla shader passes a spot
    /// by, whose radius it reads as 0, and the mirrors show no headlight pools. The three
    /// stand-in points stay out of it, or a street full of cars would fill the grid cells'
    /// sixteen places before the street lamps got theirs.
    fn prepare_lights(&self, scene: &mut Scene, cam_rel: Vec3, enhanced: bool) -> [f32; 4] {
        let ro = scene.render_origin;
        let side = LIGHT_GRID_SIDE;
        let half = side as f32 * LIGHT_CELL * 0.5;
        // snapped to whole cells, so that the grid stays the same while nothing moves
        let origin = [
            ((cam_rel.x - half) / LIGHT_CELL).floor() * LIGHT_CELL,
            ((cam_rel.y - half) / LIGHT_CELL).floor() * LIGHT_CELL,
        ];
        let mut gpu_lights: Vec<GpuPointLight> =
            Vec::with_capacity(scene.interior_lights.len() + scene.lights.len().max(1));
        // the vehicles' interior lamps first, at the indices their meshes name (they are in
        // no grid cell: only those meshes look them up)
        for l in &scene.interior_lights {
            gpu_lights.push(gpu_light(l, (l.position - ro).as_vec3()));
        }
        let mut grid = vec![u32::MAX; side * side * LIGHT_CELL_CAP];
        for l in &scene.lights {
            if !drawn_by(l, enhanced) {
                continue;
            }
            let p = (l.position - ro).as_vec3();
            let x0 = ((p.x - l.radius - origin[0]) / LIGHT_CELL).floor();
            let x1 = ((p.x + l.radius - origin[0]) / LIGHT_CELL).floor();
            let y0 = ((p.y - l.radius - origin[1]) / LIGHT_CELL).floor();
            let y1 = ((p.y + l.radius - origin[1]) / LIGHT_CELL).floor();
            if x1 < 0.0 || y1 < 0.0 || x0 >= side as f32 || y0 >= side as f32 {
                continue;
            }
            let idx = gpu_lights.len() as u32;
            gpu_lights.push(gpu_light(l, p));
            for y in (y0.max(0.0) as usize)..=(y1.min(side as f32 - 1.0) as usize) {
                for x in (x0.max(0.0) as usize)..=(x1.min(side as f32 - 1.0) as usize) {
                    let base = (y * side + x) * LIGHT_CELL_CAP;
                    if let Some(slot) = grid[base..base + LIGHT_CELL_CAP]
                        .iter()
                        .position(|v| *v == u32::MAX)
                    {
                        grid[base + slot] = idx;
                    } else if omsi_cfg::env::var_os("OMSI_DEBUG_LIGHT_GRID").is_some() {
                        log::info!("light grid: cell ({x}, {y}) full, light at ({:.1}, {:.1}) radius {:.0} {} left out", p.x, p.y, l.radius, if l.direction.length_squared() > 0.5 { "spot" } else { "point" });
                    }
                }
            }
        }
        if gpu_lights.is_empty() {
            gpu_lights.push(GpuPointLight {
                pos: [0.0; 4],
                color: [0.0; 4],
                dir: [0.0; 4],
                extra: [0.0; 4],
            });
        }
        let lbytes: &[u8] = bytemuck::cast_slice(&gpu_lights);
        let gbytes: &[u8] = bytemuck::cast_slice(&grid);
        let mut rebuilt = false;
        match &scene.light_buf {
            Some(b) if b.size() >= lbytes.len() as u64 => {
                if scene.last_lights != lbytes {
                    b.write(&self.queue, 0, lbytes);
                }
            }
            _ => {
                let cap = (lbytes.len() * 2).max(64 * std::mem::size_of::<GpuPointLight>());
                let b = GpuArray::new(&self.device, "lights", cap as u64, 16, false);
                b.write(&self.queue, 0, lbytes);
                scene.light_buf = Some(b);
                rebuilt = true;
            }
        }
        match &scene.grid_buf {
            Some(b) => {
                if scene.last_grid != grid {
                    b.write(&self.queue, 0, gbytes);
                }
            }
            None => {
                let b = GpuArray::new(&self.device, "light grid", gbytes.len() as u64, 4, false);
                b.write(&self.queue, 0, gbytes);
                scene.grid_buf = Some(b);
                rebuilt = true;
            }
        }
        scene.last_lights.clear();
        scene.last_lights.extend_from_slice(lbytes);
        scene.last_grid = grid;
        if rebuilt {
            self.rebuild_camera_bind_group(scene);
        }
        [origin[0], origin[1], LIGHT_CELL, side as f32]
    }

    /// Take in the pass times of the last timed frame once its readback has arrived.
    fn collect_gpu_timers(&mut self) {
        let period = self.queue.get_timestamp_period() as f64;
        for t in self.gpu_timers.iter_mut().flatten() {
            t.collect(period);
            // The last frame's stamps are read in a command buffer of their own, submitted
            // after that frame's: resolved in the frame's own buffer, the end stamp of its
            // last pass was often not written yet (Metal) and read as a stale number.
            if t.unresolved {
                t.unresolved = false;
                let n = t.pending.len() as u32 * 2;
                let mut enc = self
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("pass timers"),
                    });
                enc.resolve_query_set(&t.set, 0..n, &t.resolve, 0);
                enc.copy_buffer_to_buffer(&t.resolve, 0, &t.read, 0, n as u64 * 8);
                self.queue.submit([enc.finish()]);
                let ready = t.ready.clone();
                t.read.map_async(wgpu::MapMode::Read, .., move |r| {
                    ready.store(r.is_ok(), std::sync::atomic::Ordering::Relaxed)
                });
                t.waiting = true;
            }
        }
    }

    /// Average GPU time per pass so far: (pass, milliseconds, frames measured).
    /// The mirrors' frames (drawn without the overlays) are timed on their own.
    pub fn gpu_pass_times(&self) -> Vec<(String, f64, u32)> {
        let mut out = Vec::new();
        for (k, t) in self.gpu_timers.iter().enumerate() {
            let Some(t) = t else { continue };
            for (label, v) in &t.totals {
                let name = if k == 0 {
                    format!("mirrors: {label}")
                } else {
                    label.to_string()
                };
                out.push((name, v.0 / v.1.max(1) as f64 * 1000.0, v.1));
            }
        }
        out
    }
}

impl GpuTimers {
    /// Take in the pass times of the last timed frame once its readback has arrived.
    fn collect(&mut self, period: f64) {
        let t = self;
        if !t.waiting || !t.ready.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let n = t.pending.len() * 2;
        {
            let view = t.read.slice(0..n as u64 * 8).get_mapped_range();
            let stamps: &[u64] = bytemuck::cast_slice(&view[..n * 8]);
            // The passes in the order the GPU finished them, each counted from where the one
            // before it ended (the untimed passes in between go to the next timed one). A
            // tile-based GPU takes a pass's first stamp when its vertex stage starts, well
            // before the pass in front of it has finished its fragments: measured from their
            // own first stamps the post passes of the enhanced path overlapped and added up
            // to 25 ms of a 10 ms frame.
            if omsi_cfg::env::var_os("OMSI_GPU_TIMERS_RAW").is_some() {
                log::info!(
                    "gpu stamps: {:?}",
                    t.pending
                        .iter()
                        .enumerate()
                        .map(|(k, label)| (*label, stamps[k * 2], stamps[k * 2 + 1]))
                        .collect::<Vec<_>>()
                );
            }
            let mut order: Vec<(u64, u64, &'static str)> = t
                .pending
                .iter()
                .enumerate()
                .map(|(k, label)| (stamps[k * 2], stamps[k * 2 + 1], *label))
                .filter(|(a, b, _)| *b >= *a && *b > 0)
                .collect();
            order.sort_by_key(|(_, b, _)| *b);
            let mut prev: Option<u64> = None;
            for (a, b, label) in &order {
                let from = prev.unwrap_or(*a);
                let e = t.totals.entry(label).or_default();
                e.0 += b.saturating_sub(from) as f64 * period * 1e-9;
                e.1 += 1;
                prev = Some(*b);
            }
            if let (Some(first), Some(last)) = (order.iter().map(|o| o.0).min(), order.last()) {
                let e = t.totals.entry("(all passes)").or_default();
                e.0 += last.1.saturating_sub(first) as f64 * period * 1e-9;
                e.1 += 1;
            }
        }
        t.read.unmap();
        t.waiting = false;
    }
}

impl Renderer {
    /// Upload the frame's draw list, growing its buffer (and the bind groups that hold it)
    /// when it no longer fits.
    fn upload_draw_list(&self, scene: &mut Scene, list: &[u32]) {
        let bytes: &[u8] = bytemuck::cast_slice(if list.is_empty() { &[0u32] } else { list });
        let fits = scene
            .draw_buf
            .as_ref()
            .map(|b| b.size() >= bytes.len() as u64)
            .unwrap_or(false);
        if !fits {
            let cap = (bytes.len() as u64 * 3 / 2).max(1 << 16).div_ceil(4) * 4;
            scene.draw_buf = Some(GpuArray::new(&self.device, "draw list", cap, 4, true));
            self.rebuild_camera_bind_group(scene);
        } else if scene.camera_bind_group.is_none() || scene.shadow_bind_group.is_none() {
            self.rebuild_camera_bind_group(scene);
        }
        if let Some(b) = &scene.draw_buf {
            b.write(&self.queue, 0, bytes);
        }
    }

    /// Upload this frame's smoke particles, farthest first (they are blended over each other).
    fn prepare_smoke(&self, scene: &mut Scene, eye: DVec3) {
        let ro = scene.render_origin;
        let mut order: Vec<(f64, usize)> = scene
            .smoke
            .iter()
            .enumerate()
            .filter(|(_, p)| p.alpha > 0.002 && p.size > 0.0)
            .map(|(i, p)| (-(p.position - eye).length_squared(), i))
            .collect();
        order.sort_by(|a, b| a.0.total_cmp(&b.0));
        let data: Vec<GpuCorona> = order
            .iter()
            .map(|&(_, i)| {
                let p = &scene.smoke[i];
                GpuCorona {
                    pos: (p.position - ro).as_vec3().to_array(),
                    size: p.size,
                    color: [p.color[0], p.color[1], p.color[2], p.alpha.clamp(0.0, 1.0)],
                    dir: [0.0, 0.0, 0.0, -1.0],
                    up: [0.0, 0.0, 1.0, 2.0],
                    extra: [-2.0, 0.0, 0.0, 1.0],
                }
            })
            .collect();
        scene.smoke_count = data.len() as u32;
        if data.is_empty() {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(&data);
        match &scene.smoke_buf {
            Some(b) if b.size() as usize >= bytes.len() => self.queue.write_buffer(b, 0, bytes),
            _ => {
                let b = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("smoke"),
                    size: (bytes.len() * 2) as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.queue.write_buffer(&b, 0, bytes);
                scene.smoke_buf = Some(b);
            }
        }
    }

    /// Put a tile's light map into its place of the atlas (`slot`: column from the west, row
    /// from the north, 0..5); a picture of another size is scaled to 256 px.
    pub fn set_light_map_tile(&self, slot: (u32, u32), img: Option<&omsi_texture::Image>) {
        let n = LM_TILE_PX as usize;
        let mut px = vec![0u8; n * n * 4];
        if let Some(img) = img.filter(|i| i.width > 0 && i.height > 0) {
            for y in 0..n {
                for x in 0..n {
                    let sx = x * img.width as usize / n;
                    let sy = y * img.height as usize / n;
                    let o = (sy * img.width as usize + sx) * 4;
                    px[(y * n + x) * 4..(y * n + x) * 4 + 4].copy_from_slice(&img.rgba[o..o + 4]);
                }
            }
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.lm_atlas, mip_level: 0, origin: wgpu::Origin3d { x: slot.0 * LM_TILE_PX, y: slot.1 * LM_TILE_PX, z: 0 }, aspect: wgpu::TextureAspect::All },
            &px,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(LM_TILE_PX * 4), rows_per_image: None },
            wgpu::Extent3d { width: LM_TILE_PX, height: LM_TILE_PX, depth_or_array_layers: 1 },
        );
    }

    /// Where the light map atlas lies: its south-west corner (world x, y) and its side (m).
    pub fn set_light_map_place(&self, x: f64, y: f64, side: f64) {
        self.lm_place.set((x, y, side));
    }

    /// Register picture `id` for coronas (`Corona::texture`): a light's own bitmap, whose
    /// brightness is the glow's shape.
    pub fn set_corona_texture(&mut self, id: u16, img: &omsi_texture::Image) {
        let t = upload_texture(&self.device, &self.queue, img, true);
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("corona picture"),
            layout: &self.corona_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&t.view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.corona_sampler) },
            ],
        });
        let i = id as usize;
        if self.corona_textures.len() <= i {
            self.corona_textures.resize_with(i + 1, || None);
        }
        self.corona_textures[i] = Some(bg);
    }

    /// The picture of a smoke particle: the game's `Texture/rauch.tga` (OMSI's default
    /// for every `[smoke]`), alpha as its mask.
    pub fn set_smoke_texture(&mut self, img: &omsi_texture::Image) {
        let t = upload_texture(&self.device, &self.queue, img, true);
        self.smoke_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("smoke"),
            layout: &self.corona_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&t.view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.corona_sampler) },
            ],
        });
    }

    /// Upload this frame's coronas.
    /// `cab`: the box of the vehicle the camera is in, whose own flares Omsi.exe draws
    /// after the vehicle (0x6f0a2c -> 0x6f07bc), the others before it (0x6f0400/0x6f0418).
    fn prepare_coronas(&self, scene: &mut Scene, night: f32, cab: Option<&(DVec3, f64, [f32; 6])>) {
        let ro = scene.render_origin;
        // (its lamps sit on the skin of its box: half a metre round it is the vehicle's)
        let cab = cab.map(|&(o, h, mut bb)| {
            for v in &mut bb[..3] {
                *v += 1.0;
            }
            (o, h, bb)
        });
        let late = |c: &Corona| cab.as_ref().is_some_and(|b| point_in_vehicle_box(c.position, b));
        let mut order: Vec<(bool, &Corona)> = scene.coronas.iter().filter(|c| c.brightness > 0.001).map(|c| (late(c), c)).collect();
        order.sort_by_key(|(l, c)| (*l, c.texture));
        let mut runs: Vec<(u16, u32, u32, bool)> = Vec::new();
        for (k, &(l, c)) in order.iter().enumerate() {
            match runs.last_mut() {
                Some(r) if r.0 == c.texture && r.3 == l => r.2 += 1,
                _ => runs.push((c.texture, k as u32, 1, l)),
            }
        }
        let order: Vec<&Corona> = order.into_iter().map(|(_, c)| c).collect();
        if omsi_cfg::env::var_os("OMSI_DEBUG_CONES").is_some() {
            log::info!("coronas: {} in {} runs {:?}, {} beams", order.len(), runs.len(), runs, order.iter().filter(|c| c.beam).count());
        }
        scene.corona_runs = runs;
        let data: Vec<GpuCorona> = order
            .into_iter()
            .map(|c| {
                let p = (c.position - ro).as_vec3();
                let b = if c.cone_cos < -1.5 || c.beam || c.halo {
                    c.brightness
                } else {
                    // the original: ((1 - ambient)^2 + 0.8) 0.6 times the light's
                    // brightness (the shader takes the viewing angle and clamps it to 1)
                    c.brightness * (night * night + 0.8) * 0.6
                };
                GpuCorona {
                    pos: p.to_array(),
                    size: c.size,
                    color: [c.color[0], c.color[1], c.color[2], b],
                    dir: [c.direction.x, c.direction.y, c.direction.z, c.cone_cos],
                    up: [c.up.x, c.up.y, c.up.z, c.rotating as f32],
                    extra: [c.inner_cos, if c.beam || c.halo { c.beam_width } else { c.z_offset }, c.flags as f32, if c.beam { 1.0 } else if c.halo { 2.0 } else { 0.0 }],
                }
            })
            .collect();
        scene.corona_count = data.len() as u32;
        if data.is_empty() {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(&data);
        match &scene.corona_buf {
            Some(b) if b.size() as usize >= bytes.len() => self.queue.write_buffer(b, 0, bytes),
            _ => {
                let b = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("coronas"),
                    size: (bytes.len() * 2) as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.queue.write_buffer(&b, 0, bytes);
                scene.corona_buf = Some(b);
            }
        }
    }

    /// One step lighter on the graphics card, for a picture the card cannot keep up with at
    /// the smallest render scale: SSAO off. What was switched off, or None when it is off.
    pub fn lighten(&mut self) -> Option<&'static str> {
        if self.options.ssao {
            self.options.ssao = false;
            return Some("ambient occlusion (SSAO) off");
        }
        None
    }

    /// Rebuild the pipelines and targets without multisampling after a GPU error with it.
    /// The scene's materials stay valid (the device hands out the same bind group layout
    /// for identical entries); its per-draw buffers and camera bind group are made anew,
    /// because the camera buffer belongs to the renderer.
    fn fall_back_to_single_sample(&mut self, scene: &mut Scene) {
        log::error!(
            "{}x MSAA failed on {}; drawing without multisampling from now on",
            self.options.msaa,
            self.adapter_name
        );
        let options = RenderOptions {
            msaa: 1,
            ..self.options
        };
        *self = Self::build(
            self.device.clone(),
            self.queue.clone(),
            self.adapter_name.clone(),
            self.format,
            options,
        );
        scene.dirty = true;
        scene.model_buf = None;
        scene.params_buf = None;
        scene.camera_bind_group = None;
        scene.shadow_bind_group = None;
    }

    /// Render the scene into `target` (which must have the renderer's format).
    pub fn render(
        &mut self,
        scene: &mut Scene,
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
        camera: &Camera,
        lighting: &Lighting,
    ) {
        // (the triple screen's panels are not kept once it is turned off)
        if self.triple_targets.take().is_some() {
            self.triple_culling = Default::default();
        }
        self.render_inner(scene, target, width, height, camera, lighting, true, None, None, false);
    }

    /// Three independently culled and shaded physical panels, composited into a
    /// spanning window. HUD and menu remain in window pixels and are drawn once.
    pub fn render_triple(
        &mut self,
        scene: &mut Scene,
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
        camera: &Camera,
        lighting: &Lighting,
        rig: &TripleScreen,
    ) {
        if width < 3 || height == 0 || self.device_lost().is_some() {
            return;
        }
        let views = rig.views(camera, width, height);
        // all three panels have the same size: they share the size-keyed targets
        let pw = panel_width(width);
        if self.triple_targets.as_ref().map(|t| t.0) != Some((width, height)) {
            let targets = views
                .iter()
                .map(|v| {
                    let w = pw;
                    let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("triple screen panel"),
                        size: wgpu::Extent3d {
                            width: w,
                            height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: self.format,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    });
                    let view = texture.create_view(&Default::default());
                    // (z: where the panel starts in the window, for `fs_panel`)
                    let params = [w as f32, height as f32, v.viewport[0] as f32, 0.0];
                    let buf = buffer_init(
                        &self.device,
                        &self.queue,
                        Some("triple screen composite"),
                        bytemuck::cast_slice(&params),
                        wgpu::BufferUsages::UNIFORM,
                    );
                    let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("triple screen composite"),
                        layout: &self.upscale_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: buf.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(&view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&self.sky_sampler),
                            },
                        ],
                    });
                    (view, group)
                })
                .collect();
            self.triple_targets = Some(((width, height), targets));
        }
        // the HUD is drawn once over the whole window, after the panels: they draw none
        // (its rect buffers are kept aside, not rebuilt each frame)
        let overlays = std::mem::take(&mut scene.overlays);
        let overlay_res = std::mem::take(&mut scene.overlay_res);
        let env_heading = self.env_heading.replace(Some(camera.yaw));
        // Centre meters exposure and updates shared lighting once. All panels
        // then use that same exposure and sun shadow atlas.
        for (turn, i) in [1, 0, 2].into_iter().enumerate() {
            let view = self.triple_targets.as_ref().unwrap().1[i].0.clone();
            self.swap_triple_culling(i);
            self.render_inner(
                scene,
                &view,
                pw,
                height,
                &views[i].camera,
                lighting,
                true,
                None,
                Some(views[i].projection),
                turn != 0,
            );
            self.swap_triple_culling(i);
        }
        self.env_heading.set(env_heading);
        scene.overlays = overlays;
        scene.overlay_res = overlay_res;
        self.prepare_overlays(scene, width, height);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("triple screen composite"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("triple screen composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.panel_pipeline);
            for (i, v) in views.iter().enumerate() {
                // (the right panel loses its last pixel or two at the window's edge)
                let [x, y, w, h] = v.viewport;
                pass.set_scissor_rect(x, y, w.min(width - x), h);
                pass.set_bind_group(0, &self.triple_targets.as_ref().unwrap().1[i].1, &[]);
                pass.draw(0..3, 0..1);
            }
            pass.set_scissor_rect(0, 0, width, height);
            pass.set_pipeline(&self.overlay_pipeline_1x);
            for (_, _, group, _) in &scene.overlay_res {
                pass.set_bind_group(0, group, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
    }

    fn swap_triple_culling(&mut self, panel: usize) {
        // Visibility and LOD hysteresis belong to one projection. A side panel
        // must not replace the previous centre view's history each frame.
        let history = &mut self.triple_culling[panel];
        std::mem::swap(self.cull_drawn.get_mut(), &mut history.drawn);
        std::mem::swap(self.object_sizes.get_mut(), &mut history.sizes);
        std::mem::swap(self.object_sizes_scratch.get_mut(), &mut history.scratch);
    }

    /// Render one OpenXR view using the headset's asymmetric projection matrix.
    /// The matrix uses the same reversed depth range as the desktop camera. The
    /// second eye reuses the first eye's shadow atlas when both share an origin.
    pub fn render_xr_eye(
        &mut self,
        scene: &mut Scene,
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
        camera: &Camera,
        lighting: &Lighting,
        projection: Mat4,
        second_eye: bool,
    ) {
        self.render_inner(scene, target, width, height, camera, lighting, false, None, Some(projection), second_eye);
    }

    /// Composite the shared game menu and a world-positioned pointer into each eye.
    /// Eye-specific positions make the menu fuse into one virtual panel.
    pub fn render_xr_ui(
        &self,
        scene: &Scene,
        eyes: &[wgpu::TextureView; 2],
        desktop_size: (u32, u32),
        eye_size: (u32, u32),
        menu_range: std::ops::Range<usize>,
        menu_transforms: [Mat4; 2],
        cursor_overlay: Option<usize>,
        tooltip_overlay: Option<usize>,
        cursor_transforms: [Option<Mat4>; 2],
        navigator: Option<(TextureId, [Mat4; 2])>,
    ) {
        let Some(menu) = scene.overlays.get(menu_range) else { return };
        if menu.is_empty() && cursor_transforms.iter().all(Option::is_none) && navigator.is_none() {
            return;
        }
        let (w, h) = (desktop_size.0.max(1) as f32, desktop_size.1.max(1) as f32);
        let (eye_w, eye_h) = (eye_size.0.max(1) as f32, eye_size.1.max(1) as f32);
        let prepare = |id: &TextureId, quad: [Vec4; 4]| {
            let texture = scene.textures.get(*id)?;
            let mut uniform = [0.0f32; 20];
            for (index, corner) in quad.iter().enumerate() {
                uniform[index * 4..index * 4 + 4].copy_from_slice(&corner.to_array());
            }
            uniform[16] = scene.premultiplied.contains(id) as u8 as f32;
            let buffer = buffer_init(&self.device, &self.queue, Some("OpenXR menu rectangle"), bytemuck::cast_slice(&uniform), wgpu::BufferUsages::UNIFORM);
            let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("OpenXR menu rectangle"),
                layout: &self.overlay_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&texture.view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sky_sampler) },
                ],
            });
            Some((buffer, group))
        };
        let mut prepared = [Vec::new(), Vec::new()];
        for eye in 0..2 {
            if let Some((id, transforms)) = navigator.as_ref() {
                let quad = [Vec4::new(-1.0, 1.0, 0.0, 1.0), Vec4::new(1.0, 1.0, 0.0, 1.0),
                            Vec4::new(1.0, -1.0, 0.0, 1.0), Vec4::new(-1.0, -1.0, 0.0, 1.0)]
                    .map(|p| transforms[eye] * p);
                if let Some(item) = prepare(id, quad) { prepared[eye].push(item); }
            }
            for (index, (id, rect)) in menu.iter().enumerate() {
                // The first rectangle dims the view. Every other rectangle is
                // projected from the same menu plane in world space.
                let transform = if index == 0 && rect[0] <= 0.0 && rect[1] <= 0.0 && rect[2] >= w && rect[3] >= h {
                    Mat4::IDENTITY
                } else {
                    menu_transforms[eye]
                };
                let x0 = rect[0] / w * 2.0 - 1.0;
                let y0 = 1.0 - rect[1] / h * 2.0;
                let x1 = rect[2] / w * 2.0 - 1.0;
                let y1 = 1.0 - rect[3] / h * 2.0;
                let quad = [
                    transform * Vec4::new(x0, y0, 0.0, 1.0),
                    transform * Vec4::new(x1, y0, 0.0, 1.0),
                    transform * Vec4::new(x1, y1, 0.0, 1.0),
                    transform * Vec4::new(x0, y1, 0.0, 1.0),
                ];
                if let Some(item) = prepare(id, quad) { prepared[eye].push(item); }
            }
            if let (Some(index), Some(transform)) = (cursor_overlay, cursor_transforms[eye]) {
                if let Some((id, rect)) = scene.overlays.get(index) {
                    let half_x = (rect[2] - rect[0]) / eye_w;
                    let half_y = (rect[3] - rect[1]) / eye_h;
                    let quad = [
                        transform * Vec4::new(-half_x, half_y, 0.0, 1.0),
                        transform * Vec4::new(half_x, half_y, 0.0, 1.0),
                        transform * Vec4::new(half_x, -half_y, 0.0, 1.0),
                        transform * Vec4::new(-half_x, -half_y, 0.0, 1.0),
                    ];
                    if let Some(item) = prepare(id, quad) { prepared[eye].push(item); }
                }
                if let Some((id, rect)) = tooltip_overlay.and_then(|i| scene.overlays.get(i)) {
                    let pointer_width = scene.overlays.get(index).map(|(_, r)| r[2] - r[0]).unwrap_or(24.0);
                    let left = (pointer_width * 0.5 + 8.0) * 2.0 / eye_w;
                    let right = left + (rect[2] - rect[0]) * 2.0 / eye_w;
                    let bottom = -(rect[3] - rect[1]) * 2.0 / eye_h;
                    let quad = [
                        transform * Vec4::new(left, 0.0, 0.0, 1.0),
                        transform * Vec4::new(right, 0.0, 0.0, 1.0),
                        transform * Vec4::new(right, bottom, 0.0, 1.0),
                        transform * Vec4::new(left, bottom, 0.0, 1.0),
                    ];
                    if let Some(item) = prepare(id, quad) { prepared[eye].push(item); }
                }
            }
        }
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("OpenXR menu") });
        for (eye, target) in eyes.iter().enumerate() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("OpenXR menu"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.xr_ui_pipeline);
            for (_, group) in &prepared[eye] {
                pass.set_bind_group(0, group, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
    }

    fn prepare_overlays(&self, scene: &mut Scene, full_w: u32, full_h: u32) {
        let overlays = &scene.overlays;
        // the HUD's rect buffers and bind groups live on between frames: making them
        // anew for every overlay of every frame was a steady stream of GPU allocations
        scene.overlay_res.truncate(overlays.len());
        for (k, (tex, r)) in overlays.iter().copied().enumerate() {
            let r = snap_rect(r);
            let ndc = [
                r[0] / full_w as f32 * 2.0 - 1.0,
                1.0 - r[1] / full_h as f32 * 2.0,
                r[2] / full_w as f32 * 2.0 - 1.0,
                1.0 - r[3] / full_h as f32 * 2.0,
                scene.premultiplied.contains(&tex) as u8 as f32,
                0.0,
                0.0,
                0.0,
            ];
            if let Some((_, buf, _, last)) = scene.overlay_res.get_mut(k).filter(|o| o.0 == tex) {
                if *last != ndc {
                    self.queue.write_buffer(buf, 0, bytemuck::cast_slice(&ndc));
                    *last = ndc;
                }
                continue;
            }
            let buf = buffer_init(
                &self.device,
                &self.queue,
                Some("overlay rect"),
                bytemuck::cast_slice(&ndc),
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            );
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("overlay"),
                layout: &self.overlay_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&scene.textures[tex].view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sky_sampler),
                    },
                ],
            });
            if k < scene.overlay_res.len() {
                scene.overlay_res[k] = (tex, buf, bg, ndc);
            } else {
                scene.overlay_res.push((tex, buf, bg, ndc));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_inner(
        &mut self,
        scene: &mut Scene,
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
        camera: &Camera,
        lighting: &Lighting,
        with_overlays: bool,
        exclude_texture: Option<TextureId>,
        projection: Option<Mat4>,
        second_eye: bool,
    ) {
        // test hook for a lost device (a driver reset): its resources are taken away and
        // the session has to end in order
        if omsi_cfg::env::var("OMSI_FAKE_GPU_ERROR").as_deref() == Ok("lost")
            && with_overlays
            && self.started.elapsed().as_secs_f32() > 3.0
            && self.device_lost().is_none()
        {
            log::error!("the graphics device was lost (test): OMSI_FAKE_GPU_ERROR=lost");
            self.device.destroy();
            *self.device_lost.lock().unwrap_or_else(|e| e.into_inner()) = Some("test".into());
        }
        // the device is gone: nothing can be drawn, and the readbacks (the exposure meter)
        // would find their buffers taken away - "Error in Buffer::get_mapped_range:
        // Validation Error" ended the game instead of the session ending in order
        if self.device_lost.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            return;
        }
        if omsi_cfg::env::var("OMSI_FAKE_GPU_ERROR").as_deref() == Ok("frame")
            && self.options.msaa > 1
            && self.started.elapsed().as_secs_f32() > 3.0
        {
            // test hook for the fallback below
            let _ = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("invalid"),
                size: wgpu::Extent3d {
                    width: 4,
                    height: 4,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 3,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
        }
        if self.gpu_error.load(std::sync::atomic::Ordering::Relaxed) {
            self.fall_back_to_single_sample(scene);
        }
        // OMSI_GPU_TIMERS: GPU time per pass, one frame at a time
        self.collect_gpu_timers();
        let tset: Option<wgpu::QuerySet> = self.gpu_timers[with_overlays as usize]
            .as_ref()
            .filter(|t| !t.waiting && !t.unresolved)
            .map(|t| t.set.clone());
        let mut timed: Vec<&'static str> = Vec::new();
        // OMSI_PROFILE: time per stage, the mirrors apart from the window's picture
        let mut stage_t = std::time::Instant::now();
        let mut stage = |r: &Renderer, window: &'static str, mirror: &'static str| {
            if r.profiling {
                let now = std::time::Instant::now();
                *r.stats
                    .borrow_mut()
                    .entry(if with_overlays { window } else { mirror })
                    .or_default() += (now - stage_t).as_secs_f64();
                stage_t = now;
            }
        };
        // render origin: the camera position rounded to 100 m, kept while the camera stays near it
        if (camera.position - scene.render_origin).abs().max_element() > 200.0 {
            self.set_render_origin(scene, (camera.position / 100.0).floor() * 100.0);
        }
        let ro = scene.render_origin;
        // The window's 3D picture may be drawn smaller and scaled up to it (render scale):
        // from here on `width` and `height` are the size of the picture, `full_*` the
        // window's (the HUD is drawn at that size). Mirrors keep their own size.
        let (full_w, full_h) = (width, height);
        let (width, height) = if with_overlays {
            self.scene_size(full_w, full_h)
        } else {
            (full_w, full_h)
        };
        // FXAA on the plain graphics too, where there is no multisampling to smooth the
        // edges (the Enhanced path has it in its post passes): the picture is drawn into a
        // texture of the window's size and smoothed on its way to the window, the HUD after
        let vanilla_fxaa = with_overlays
            && (width, height) == (full_w, full_h)
            && self.options.fxaa
            && self.options.msaa <= 1
            && !(lighting.enhanced && self.hdr_pass.is_some() && omsi_cfg::env::var_os("OMSI_NO_ENHANCED").is_none())
            && omsi_cfg::env::var_os("OMSI_NO_FXAA").is_none();
        // Rain films are drawn after copying the clean current-frame scene. Classic
        // graphics need a sampleable scene target too, even without puddles or scaling.
        let enhanced_view = lighting.enhanced && self.hdr_pass.is_some() && omsi_cfg::env::var_os("OMSI_NO_ENHANCED").is_none();
        let glass_on = with_overlays
            && scene.glass_slot.is_some()
            && (lighting.rain > 0.001 || lighting.wetness > 0.02)
            && omsi_cfg::env::var_os("OMSI_NO_GLASS_PICTURE").is_none();
        let scaled = (width, height) != (full_w, full_h) || vanilla_fxaa || (glass_on && !enhanced_view);
        let scene_target: Option<(wgpu::TextureView, wgpu::BindGroup)> = if scaled {
            Some(self.scale_target(width, height))
        } else {
            None
        };
        let scene_view: &wgpu::TextureView = scene_target.as_ref().map(|t| &t.0).unwrap_or(target);
        let aspect = self.texture_aspect.unwrap_or(width as f32 / height.max(1) as f32);
        let cam_rel = (camera.position - ro).as_vec3();
        // The mirrors are drawn with the plain shading even in Enhanced: without the depth
        // prepass (sized for the window) every layer of a mirror's picture ran the enhanced
        // shader, 12.7 ms of GPU time for one 256-pixel mirror against 5.8 ms for the whole
        // window; plainly shaded it is 0.6 ms, and a mirror's small picture shows no
        // difference worth that. `OMSI_MIRROR_ENHANCED=1` draws them enhanced again.
        // The headset's eyes are the real picture as much as the window is (#784: VR showed
        // the plain graphics with Enhanced on); the first eye is the one that moves the
        // exposure, the sky cube and the frame clock on, as the window does without VR.
        let xr_view = projection.is_some();
        let lead_view = (with_overlays || xr_view) && !second_eye;
        let enhanced_frame = lighting.enhanced && self.hdr_pass.is_some() && omsi_cfg::env::var_os("OMSI_NO_ENHANCED").is_none() && (with_overlays || xr_view || omsi_cfg::env::var_os("OMSI_MIRROR_ENHANCED").is_some());
        // the mirrors are drawn by the same path as the window (their picture graded with
        // the window's exposure, see the post passes)
        let enhanced = enhanced_frame;
        // Rain reflections belong to all graphics modes. Classic shading uses the same
        // scene/mask targets, then presents their linear colour without Enhanced grading.
        let puddles_wanted = with_overlays
            && self.puddles.is_some()
            && self.options.reflections
            && lighting.wetness * (1.0 - lighting.snow.clamp(0.0, 1.0)) > 0.05
            && scene.materials.iter().any(|m| m.uniform.params2[2] > 0.0)
            && debug_view() == 0.0
            && omsi_cfg::env::var_os("OMSI_NO_PUDDLE_REFLECTIONS").is_none();
        let reflection_frame = !enhanced && puddles_wanted && self.reflection_pass.is_some();
        let masked_frame = enhanced || reflection_frame;
        let grid = self.prepare_lights(scene, cam_rel, enhanced_frame);
        self.prepare_coronas(scene, lighting.night, lighting.inside.as_ref().filter(|v| point_in_vehicle_box(camera.position, v)));
        self.prepare_smoke(scene, camera.position);
        // ambient occlusion only for the real picture, not for the mirrors
        let ao_on = with_overlays && self.options.ssao && self.ssao_pipeline.is_some() && omsi_cfg::env::var_os("OMSI_NO_AO").is_none();
        // the enhanced path's shading is costly: the depth prepass keeps it to the visible
        // surface (without multisampling, see `share_depth`)
        let prepass_on = ao_on || puddles_wanted || glass_on || (enhanced && (with_overlays || xr_view));
        if prepass_on && self.ensure_ao(width, height) {
            // a new AO texture: the camera bind group must point at it
            scene.dirty = true;
            scene.model_buf = None;
            self.hdr_targets.clear();
        }
        if masked_frame {
            self.hdr_targets(width, height);
        }
        if glass_on {
            self.prepare_glass_behind(scene, width, height, if masked_frame { HDR_FORMAT } else { self.format });
        }
        let dt = {
            let now = std::time::Instant::now();
            let dt = self
                .last_frame
                .map(|t| (now - t).as_secs_f32())
                .unwrap_or(0.0);
            if lead_view {
                self.last_frame = Some(now);
            }
            dt
        };
        stage(self, "setup", "mirror.setup");
        self.prepare(scene);
        stage(self, "prepare", "mirror.prepare");
        // overlay uniforms/bind groups (rects in pixels → NDC)
        let overlays: Vec<(TextureId, [f32; 4])> = if with_overlays {
            scene.overlays.clone()
        } else {
            Vec::new()
        };
        if with_overlays {
            self.prepare_overlays(scene, full_w, full_h);
        }
        // sun shadow map: an orthographic box around the camera, looking along the sun
        let sun = lighting.sun_dir.normalize_or_zero();
        let shadows = (with_overlays || projection.is_some()) && lighting.casts_sun_shadows();
        let shared_xr_shadows = if second_eye && shadows {
            self.xr_shadow_cache
                .get()
                .filter(|(origin, previous_sun, _, _, _)| *origin == scene.render_origin && *previous_sun == sun)
        } else {
            None
        };
        let draw_shadows = shadows && shared_xr_shadows.is_none();
        let light_matrix = |range: f32| {
            // Snap the centre to whole texels so the map does not shimmer while driving.
            let texel = range * 2.0 / self.options.shadow_size as f32;
            let up = if sun.z.abs() > 0.95 { Vec3::Y } else { Vec3::Z };
            let raw = cam_rel;
            let view0 = Mat4::look_at_rh(sun * 900.0, Vec3::ZERO, up);
            let ls = view0.transform_point3(raw);
            let snapped = Vec3::new(
                (ls.x / texel).round() * texel,
                (ls.y / texel).round() * texel,
                ls.z,
            );
            let center = view0.inverse().transform_point3(snapped);
            let view = Mat4::look_at_rh(center + sun * 900.0, center, up);
            let proj = Mat4::orthographic_rh(-range, range, -range, range, 1.0, 2200.0);
            proj * view
        };
        // The near cascade (140 m, 4096 texels at the top setting: the costliest shadow
        // pass, a third of it the trees' leaf cards) is drawn every other frame and kept
        // for the next, with the light matrix it was drawn with; the close one - the bus
        // and everything within 30 m - every frame. Redrawn at once when the camera has
        // jumped, the sun has moved or the render origin has (its matrix is relative to it).
        let near_wanted = light_matrix(SHADOW_RANGE);
        let (near_m, near_age, near_origin, near_sun) = self.shadow_near_cache.get();
        let near_jumped = (near_m.project_point3(cam_rel) - near_wanted.project_point3(cam_rel)).length() > 0.03;
        let redraw_near = draw_shadows
            && (near_age >= 1
                || near_jumped
                || near_m == Mat4::IDENTITY
                || near_origin != scene.render_origin
                || near_sun.dot(sun) < 0.99999
                || omsi_cfg::env::var_os("OMSI_SHADOW_NEAR_EVERY_FRAME").is_some());
        let light_view_proj = if let Some((_, _, near, _, _)) = shared_xr_shadows {
            near
        } else if !shadows {
            near_wanted
        } else if redraw_near {
            self.shadow_near_cache.set((near_wanted, 0, scene.render_origin, sun));
            near_wanted
        } else {
            self.shadow_near_cache.set((near_m, near_age + 1, near_origin, near_sun));
            near_m
        };
        let light_view_proj_close = shared_xr_shadows
            .map(|(_, _, _, _, close)| close)
            .unwrap_or_else(|| light_matrix(SHADOW_RANGE_CLOSE));
        // The far cascade (700 m, metre-sized texels) is drawn every 4th frame, or at once
        // when the camera has left the middle of the one drawn, the sun has moved or the
        // render origin has jumped (its matrix is relative to that). Drawn every frame it
        // was 0.6 ms of GPU time for a picture that hardly changes; a car in it is a few
        // texels, and a tile streamed in waits three frames at most for its shadow.
        let far_wanted = light_matrix(SHADOW_RANGE_FAR);
        let (far_m, far_age, far_origin, far_sun) = self.shadow_far_cache.get();
        let far_moved = (far_m.project_point3(cam_rel) - far_wanted.project_point3(cam_rel)).length() > 0.12;
        let redraw_far = draw_shadows
            && (far_age >= 3
                || far_moved
                || far_m == Mat4::IDENTITY
                || far_origin != scene.render_origin
                || far_sun.dot(sun) < 0.99999
                || omsi_cfg::env::var_os("OMSI_SHADOW_FAR_EVERY_FRAME").is_some());
        if redraw_far && omsi_cfg::env::var_os("OMSI_DEBUG_SHADOW_FAR").is_some() {
            log::info!("far shadow redrawn: age {far_age} moved {far_moved} origin {} sun {:.6}", far_origin != scene.render_origin, far_sun.dot(sun));
        }
        // The desktop view and the first XR eye maintain this cache; mirrors and
        // the second XR eye leave it alone.
        let light_view_proj_far = if let Some((_, _, _, far, _)) = shared_xr_shadows {
            far
        } else if !shadows {
            far_wanted
        } else if redraw_far {
            self.shadow_far_cache.set((far_wanted, 0, scene.render_origin, sun));
            far_wanted
        } else {
            self.shadow_far_cache.set((far_m, far_age + 1, far_origin, far_sun));
            far_m
        };
        if projection.is_some() && !second_eye && shadows {
            self.xr_shadow_cache.set(Some((
                scene.render_origin,
                sun,
                light_view_proj,
                light_view_proj_far,
                light_view_proj_close,
            )));
        }
        // where the sun stands on the screen (camera uniform post.zw; no shader reads it
        // since the light shafts were removed)
        let vp_mat = projection
            .map(|p| p * Mat4::look_to_rh((camera.position - ro).as_vec3(), camera.forward(), camera.up()))
            .unwrap_or_else(|| camera.view_proj(aspect, ro));
        let sun_clip = vp_mat * (cam_rel + sun * 5000.0).extend(1.0);
        let sun_ndc = if sun_clip.w > 0.0 {
            Vec3::new(sun_clip.x / sun_clip.w, sun_clip.y / sun_clip.w, 1.0)
        } else {
            Vec3::new(9.0, 9.0, 0.0)
        };
        // where the tile light maps lie, relative to the render origin
        {
            let (lx, ly, side) = self.lm_place.get();
            let v: [f32; 4] = [(lx - ro.x) as f32, (ly - ro.y) as f32, side as f32, if side > 0.0 { 1.0 } else { 0.0 }];
            self.queue.write_buffer(&self.lm_uniform, 0, bytemuck::cast_slice(&v));
        }
        let cu = CameraUniform {
            post: [
                if enhanced { 1.0 } else { 0.0 },
                self.started.elapsed().as_secs_f32(),
                sun_ndc.x,
                // (the sun's height on the screen is read by no shader any more: the close
                // shadow map's share of its half of the atlas)
                self.options.shadow_size.min(SHADOW_CLOSE_MAX) as f32 / self.options.shadow_size.max(1) as f32,
            ],
            view_proj: vp_mat.to_cols_array_2d(),
            cam_pos: cam_rel.extend(1.0).to_array(),
            // (modulo the shaders' PATTERN_PERIOD, 1000 m: the patterns repeat with it, and
            // the whole map coordinate has no precision left for them in 32 bits)
            // (zw: the origin modulo CLOUD_ORIGIN_PERIOD for the sky's clouds, which are
            // drawn over ground points: taken relative to the floating origin, the whole
            // cloud field jumped by the origin's step each time it moved on)
            world_origin: [
                ro.x.rem_euclid(1000.0) as f32,
                ro.y.rem_euclid(1000.0) as f32,
                ro.x.rem_euclid(CLOUD_ORIGIN_PERIOD) as f32,
                ro.y.rem_euclid(CLOUD_ORIGIN_PERIOD) as f32,
            ],
            sun_dir: lighting
                .sun_dir
                .normalize()
                .extend(lighting.sun_intensity)
                .to_array(),
            ambient: lighting
                .ambient
                .extend(lighting.snow.clamp(0.0, 1.0))
                .to_array(),
            fog: lighting.fog_color.extend(lighting.fog_density).to_array(),
            sun_color: lighting.sun_color.extend(lighting.night_maps.unwrap_or(lighting.night)).to_array(),
            sky_color: lighting
                .secondary
                .extend(if lighting.classic && !enhanced { 1.0 } else { 0.0 })
                .to_array(),
            light_grid: grid,
            sky: [
                lighting.sun_azimuth,
                lighting.sky_weights[0],
                lighting.sky_weights[1],
                lighting.sky_weights[2],
            ],
            clouds: [
                lighting.cloud_density,
                lighting.cloud_offset[0],
                lighting.cloud_offset[1],
                if ao_on { 1.0 } else { 0.0 },
            ],
            // (w: the heading the sphere maps are laid out by in the headset, see
            // `set_env_heading`; flagged by cam_up.w)
            cam_right: camera.right().extend(self.env_heading.get().map(|h| h.to_radians()).unwrap_or(0.0)).to_array(),
            cam_up: camera
                .right()
                .cross(camera.forward())
                .normalize_or_zero()
                .extend(if self.env_heading.get().is_some() { 1.0 } else { 0.0 })
                .to_array(),
            light_view_proj: light_view_proj.to_cols_array_2d(),
            light_view_proj_far: light_view_proj_far.to_cols_array_2d(),
            shadow: [
                if shadows { 1.0 } else { 0.0 },
                1.0 / self.options.shadow_size as f32,
                SHADOW_RANGE,
                lighting.wetness.clamp(0.0, 1.0),
            ],
            inside_a: match lighting.inside {
                Some((o, h, _)) => {
                    let r = (o - ro).as_vec3();
                    [r.x, r.y, r.z, (h as f32).to_radians().sin()]
                }
                None => [0.0; 4],
            },
            inside_b: match lighting.inside {
                Some((_, h, bb)) => [
                    (h as f32).to_radians().cos(),
                    bb[0] * 0.5,
                    bb[1] * 0.5,
                    bb[2] * 0.5,
                ],
                None => [1.0, 0.0, 0.0, 0.0],
            },
            inside_c: match lighting.inside {
                Some((_, _, bb)) => [bb[3], bb[4], bb[5], 1.0],
                None => [0.0; 4],
            },
            flags: [
                if lighting.detail { 1.0 } else { 0.0 },
                if enhanced { 1.0 } else { 0.0 },
                // (below zero: the rain films have the clean current picture to look through,
                // see `rain_behind`; above zero is an old branch never taken)
                if glass_on { -1.0 } else { 0.0 },
                if shadows { SHADOW_RANGE_CLOSE } else { 0.0 },
            ],
            light_view_proj_close: light_view_proj_close.to_cols_array_2d(),
            wind: [lighting.glass_wind.x, lighting.glass_wind.y, lighting.glass_wind.z, 1.0],
        };
        self.queue
            .write_buffer(&self.camera_buf, 0, bytemuck::bytes_of(&cu));
        // (a mirror takes the window's light - its own call would move the exposure on -
        // unless it comes before the window's first frame)
        let probe_redraw = enhanced
            && (lead_view || self.sky_state.is_none())
            && self.prepare_enhanced(lighting, cam_rel, ro, dt);
        // --- what every pass draws, as batches over one draw list (see `Batch`): the shadow
        // casters of each cascade, the depth prepass and the main pass. The list is built
        // and uploaded before any pass is encoded.
        stage(self, "setup", "mirror.setup");
        let debug_draws = omsi_cfg::env::var_os("OMSI_DEBUG_DRAWS").is_some();
        let debug_cull = omsi_cfg::env::var_os("OMSI_DEBUG_CULL").is_some();
        let mut list: Vec<u32> = Vec::new();
        let mut items: Vec<DrawItem> = Vec::new();
        // near, far, close
        let mut shadow_batches: [Vec<Batch>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let kind_of = |alpha: AlphaMode| -> u8 {
            match alpha {
                AlphaMode::Opaque => PIPE_OPAQUE,
                AlphaMode::Test => PIPE_ALPHA_TEST,
                AlphaMode::Blend => PIPE_BLEND,
            }
        };
        // an instance's screen size as the camera pass measures it for the LOD choice
        let lod_fov = camera.fov_deg.to_radians().max(1e-3);
        let lod_size = |inst: &Instance| -> f32 {
            let scale = Self::instance_scale(scene, inst);
            let radius = if inst.object_radius > 0.0 { inst.object_radius } else { scene.meshes[inst.mesh].bounds_radius } * scale;
            let d = ((inst.origin - scene.render_origin).as_vec3() - cam_rel).length();
            if d <= radius { f32::MAX } else { 2.0 * radius / (d.max(0.01) * lod_fov) }
        };
        let active = [draw_shadows && redraw_near, draw_shadows && redraw_far, draw_shadows];
        let boxes = [(SHADOW_RANGE, light_view_proj, 0.4f32), (SHADOW_RANGE_FAR, light_view_proj_far, 6.0), (SHADOW_RANGE_CLOSE, light_view_proj_close, 0.1)];
        let dbg_shadow = omsi_cfg::env::var_os("OMSI_DEBUG_SHADOW").is_some();
        let dbg_r: f32 = omsi_cfg::env::var("OMSI_DEBUG_SHADOW")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(3.0);
        let casters = |span: std::ops::Range<usize>| -> [Vec<DrawItem>; 3] {
            let mut out: [Vec<DrawItem>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            let mut ranges: Vec<(u8, u32, u32, usize)> = Vec::new();
            for inst in &scene.instances[span] {
                if !inst.visible || !inst.casts_shadow || (self.options.omsi_shadow_casters && !inst.omsi_caster) {
                    continue;
                }
                let m = &scene.meshes[inst.mesh];
                if m.ranges.is_empty() {
                    continue;
                }
                let (c, r) = Self::bounding_sphere(scene, inst);
                if inst.lod.0 > 0.0 || inst.lod.1 < f32::MAX {
                    let size = lod_size(inst);
                    if size < inst.lod.0 || (inst.lod.1 < f32::MAX && size >= inst.lod.1) {
                        if dbg_shadow && r >= dbg_r {
                            log::info!("shadow: mesh r={r:.1} at {:?} is another LOD than the one shown", inst.origin);
                        }
                        continue;
                    }
                }
                ranges.clear();
                for (ri, (_, _, slot)) in m.ranges.iter().enumerate() {
                    let mat_id = inst.materials.get(*slot as usize).copied().unwrap_or(0);
                    let mat = &scene.materials[mat_id];
                    let mut kind = kind_of(mat.alpha);
                    let cut_body = kind == PIPE_BLEND && mat.transmap.is_some() && !mat.no_z_write;
                    if (kind == PIPE_BLEND && !cut_body) || mat.no_z_check {
                        if dbg_shadow && r >= dbg_r {
                            log::info!("shadow: mesh r={r:.1} at {:?} slot {slot} is blended, never a caster", inst.origin);
                        }
                        continue;
                    }
                    if cut_body {
                        kind = PIPE_ALPHA_TEST;
                    }
                    ranges.push((kind, ri as u32, *slot, mat_id));
                }
                for (cascade, &(range, lvp, min_radius)) in boxes.iter().enumerate() {
                    if !active[cascade] {
                        continue;
                    }
                    let dbg = dbg_shadow && cascade == 0 && r >= dbg_r;
                    if m.bounds_radius > 0.0 && m.bounds_radius < min_radius {
                        if dbg_shadow && cascade == 0 && m.bounds_radius >= dbg_r {
                            log::info!("shadow: mesh r={:.1} skipped (ranges {})", m.bounds_radius, m.ranges.len());
                        }
                        continue;
                    }
                    let lc = lvp.project_point3(c);
                    let rr = r / range;
                    if lc.x.abs() > 1.0 + rr || lc.y.abs() > 1.0 + rr {
                        if dbg {
                            log::info!("shadow: mesh r={r:.1} outside the light box at ({:.2}, {:.2})", lc.x, lc.y);
                        }
                        continue;
                    }
                    if dbg {
                        log::info!("shadow: caster r={r:.1} at {:?} slots {:?}", inst.origin, ranges.iter().map(|x| x.0).collect::<Vec<_>>());
                    }
                    for &(kind, ri, slot, mat_id) in &ranges {
                        out[cascade].push(DrawItem {
                            pipe: kind,
                            mesh: inst.mesh as u32,
                            range: ri,
                            material: depth_only_material(kind, mat_id),
                            entry: inst.base + slot,
                        });
                    }
                }
            }
            out
        };
        if active.iter().any(|a| *a) {
            let n = scene.instances.len();
            let parts = (n / 8192).clamp(1, self.encoding_pool.as_ref().map_or(3, |p| p.current_num_threads()) + 1);
            let chunk = n.div_ceil(parts);
            let mut found: [Vec<DrawItem>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            for part in run_parts(self.encoding_pool.as_ref(), parts, |p| casters(p * chunk..((p + 1) * chunk).min(n))) {
                for (a, b) in found.iter_mut().zip(part) {
                    a.extend(b);
                }
            }
            for cascade in 0..3 {
                if !active[cascade] {
                    continue;
                }
                if debug_draws {
                    log::info!("shadow cascade {cascade}: {} draws", found[cascade].len());
                }
                batch_items(scene, &mut found[cascade], true, &mut list, &mut shadow_batches[cascade]);
            }
        }
        if shadows && debug_draws {
            log::info!(
                "shadow passes: {} batches",
                shadow_batches.iter().map(|b| b.len()).sum::<usize>()
            );
        }
        stage(self, "shadow items", "mirror.shadow items");
        // Frustum culling by bounding sphere in view space. OpenXR projections
        // are asymmetric; the desktop field of view must not clip an eye's
        // wider side as the head turns.
        let view = Mat4::look_to_rh(cam_rel, camera.forward(), camera.up());
        let (tan_x, tan_y) = if let Some(p) = projection {
            (
                ((p.z_axis.x - 1.0) / p.x_axis.x)
                    .abs()
                    .max(((p.z_axis.x + 1.0) / p.x_axis.x).abs()),
                ((p.z_axis.y - 1.0) / p.y_axis.y)
                    .abs()
                    .max(((p.z_axis.y + 1.0) / p.y_axis.y).abs()),
            )
        } else {
            let tan_y = (camera.fov_deg.to_radians() * 0.5).tan();
            (tan_y * aspect, tan_y)
        };
        let cos_y = 1.0 / (1.0 + tan_y * tan_y).sqrt();
        let cos_x = 1.0 / (1.0 + tan_x * tan_x).sqrt();
        // nothing behind the fog is drawn: where the fog has swallowed 99 % of a thing
        // there is nothing left to see of it (a storm's 700 m of sight cuts the draws
        // of a city map by two thirds)
        //
        // The enhanced picture's fog is another one: none at all below the weather's
        // 1e-4 (the clear air there is the sky model's, kilometres deep), and above it a
        // layer that thins out with height (`layer_depth` in enhanced_common.wgsl, 300 m
        // scale height over the fog's base). Culled by the vanilla density, whatever stood
        // in thin fog went missing in plain sight - most of all seen from above, with the
        // camera zoomed out.
        let fog_far = if enhanced_frame {
            if lighting.fog_density > 1e-4 {
                let base = lighting
                    .fog_base
                    .or(lighting.inside.map(|v| v.0.z))
                    .unwrap_or(camera.position.z - 2.0);
                let kh = ((camera.position.z - base).max(0.0) / 300.0) as f32;
                // a point on the ground seen from the camera's height: the thinnest fog a
                // line of sight down to the scenery passes through
                let thin = if kh < 1e-3 { 1.0 } else { (1.0 - (-kh).exp()) / kh };
                (4.6 / (lighting.fog_density * thin)).min(camera.far)
            } else {
                camera.far
            }
        } else if lighting.fog_density > 1e-7 {
            (4.6 / lighting.fog_density).min(camera.far)
        } else {
            camera.far
        };
        if let Some(p) = omsi_cfg::env::var("OMSI_DEBUG_CULL").ok().and_then(|v| {
            let f: Vec<f64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            (f.len() == 3).then(|| (DVec3::new(f[0], f[1], 0.0), f[2]))
        }) {
            for (i, inst) in scene.instances.iter().enumerate() {
                if (inst.origin.truncate() - p.0.truncate()).length() > p.1 || !with_overlays {
                    continue;
                }
                let m = &scene.meshes[inst.mesh];
                let (c, _) = Self::bounding_sphere(scene, inst);
                let v = view.transform_point3(c);
                log::info!("cull {i}: mesh {} r {:.2} centre {:?} origin {:?} view {:?} visible {} lod {:?} tan ({tan_x:.2}, {tan_y:.2}) fog {fog_far:.0}", inst.mesh, m.bounds_radius, m.bounds_center, inst.origin, v, inst.visible, inst.lod);
            }
        }
        let fov_y = camera.fov_deg.to_radians().max(1e-3);
        let max_obj_dist = self.options.max_obj_dist;
        let min_obj_size = lighting.min_obj_size.max(self.options.min_obj_size);
        // (instance, distance along the view direction, the camera is inside its bounds)
        // One thread: a few nanoseconds an instance. Spread over the worker pool the
        // hand-over cost more than the work (5 ms a frame for 8 500 instances while the
        // traffic's scripts kept the workers busy).
        // (the main view's last picture, for the hysteresis; the headset's eyes are main
        // views too: without their previous draw list small meshes and LODs blinked at
        // their thresholds while the head turned)
        let main_view = with_overlays || xr_view;
        let mut drawn_before = if main_view {
            std::mem::take(&mut *self.cull_drawn.borrow_mut())
        } else {
            Vec::new()
        };
        let was_drawn = |i: usize| drawn_before.get(i / 64).is_some_and(|w| w & (1u64 << (i % 64)) != 0);
        let (mut sizes_before, mut sizes_now) = if main_view {
            let sizes_before = std::mem::take(&mut *self.object_sizes.borrow_mut());
            let mut sizes_now = std::mem::take(&mut *self.object_sizes_scratch.borrow_mut());
            sizes_now.clear();
            (sizes_before, sizes_now)
        } else {
            Default::default()
        };
        let cull_one = |i: usize, sizes: &mut Vec<([u64; 4], f32)>| -> Option<(usize, f32, bool)> {
            let inst = &scene.instances[i];
            let m = &scene.meshes[inst.mesh];
            if m.ranges.is_empty() || !inst.visible || (inst.mirror_only && main_view) {
                return None;
            }
            if let Some([x0, y0, x1, y1]) = inst.near_only {
                let c = camera.position;
                if c.x < x0 || c.x > x1 || c.y < y0 || c.y > y1 {
                    return None;
                }
            }
            // OMSI's `[isshadow]` shadow blobs, switched off (see `RenderOptions::shadow_blobs`)
            if inst.blob && !self.shadow_blobs {
                return None;
            }
            let (c, r) = Self::bounding_sphere(scene, inst);
            let v = view.transform_point3(c);
            let z = -v.z; // distance along the view direction
            // camera inside the sphere: drawn whatever the frustum says, but the
            // object's LOD still chooses (all levels of a building stood in at once)
            let inside = v.length() <= r;
            if !inside && z + r < camera.near {
                return None;
            }
            // (the enhanced sky is not the fog's colour below the horizon: ground
            // left out for the fog let it show through, road-shaped holes in the
            // terrain seen from above, so the ground is always drawn there)
            if !inside && z - r > fog_far && !(enhanced_frame && (inst.surface || r > 100.0)) {
                return None;
            }
            if !inside && (v.x.abs() > z * tan_x + r / cos_x || v.y.abs() > z * tan_y + r / cos_y) {
                return None;
            }
            // The screen size: the diameter over the distance to the camera (not the
            // depth along the view: that is largest in the middle of the picture, so
            // a pole looked at straight on shrank below the limit and vanished while
            // it stayed at the edge), as a share of the vertical field of view -
            // of the whole object when the mesh belongs to one (see
            // `set_object_culling`), else of the mesh alone. Surfaces and terrain
            // are never dropped for it.
            let size = if inst.object_radius > 0.0 {
                let scale = Self::instance_scale(scene, inst);
                let radius = inst.object_radius * scale;
                let ov = view.transform_point3((inst.origin - scene.render_origin).as_vec3());
                let (od, oz) = (ov.length(), -ov.z);
                if od <= radius {
                    f32::MAX
                } else {
                    // performance_maxObjDist, by the distance and by the depth
                    let reach = if was_drawn(i) { max_obj_dist * 1.05 } else { max_obj_dist };
                    if !inst.any_distance
                        && max_obj_dist > 0.0
                        && (od > radius + reach || oz - radius > reach)
                    {
                        return None;
                    }
                    // one size for the whole object, held while it moves less than
                    // 6 % (the view's jitter); every mesh and level of it computes
                    // the same key and the same size, so they decide alike
                    let key = [
                        inst.origin.x.to_bits(),
                        inst.origin.y.to_bits(),
                        inst.origin.z.to_bits(),
                        radius.to_bits() as u64,
                    ];
                    let fresh = 2.0 * radius / (od.max(0.01) * fov_y);
                    let size = match sizes_before.get(&key) {
                        Some(&last) if fresh > last * 0.94 && fresh < last * 1.06 => last,
                        _ => fresh,
                    };
                    if main_view {
                        sizes.push((key, size));
                    }
                    size
                }
            } else if m.bounds_radius > 0.0 {
                2.0 * r / (v.length().max(0.01) * fov_y)
            } else {
                f32::MAX
            };
            // `OMSI_DEBUG_CULL`: what near and in the picture is left out, and why
            let near_dbg = debug_cull && with_overlays && (inst.origin - camera.position).length() < 150.0;
            // (an object's size is held already; a lone mesh's is not)
            let keep = if was_drawn(i) && inst.object_radius <= 0.0 { 0.85 } else { 1.0 };
            if !inst.surface && size < min_obj_size * inst.detail * keep {
                if near_dbg {
                    log::info!("cull: instance {i} mesh {} at {:.0} m: size {size:.4} < {:.4} (radius {:.1}, detail {})", inst.mesh, (inst.origin - camera.position).length(), min_obj_size * inst.detail, inst.object_radius, inst.detail);
                }
                return None;
            }
            // (the top level runs to f32::MAX, and a camera inside the object's sphere
            // measures it as f32::MAX: `>=` left out both levels of every object one
            // stood next to - the parked cars, lamps and houses that vanished close by)
            if (inst.lod.0 > 0.0 || inst.lod.1 < f32::MAX)
                && (size < inst.lod.0 || (inst.lod.1 < f32::MAX && size >= inst.lod.1))
            {
                if near_dbg && size < inst.lod.0 {
                    log::info!("cull: instance {i} mesh {} at {:.0} m: lod {:.4}..{:.4}, size {size:.4}", inst.mesh, (inst.origin - camera.position).length(), inst.lod.0, inst.lod.1);
                }
                return None;
            }
            Some((i, z, inside))
        };
        let n = scene.instances.len();
        let parts = (n / 8192).clamp(1, self.encoding_pool.as_ref().map_or(3, |p| p.current_num_threads()) + 1);
        let chunk = n.div_ceil(parts);
        let (mut visible, mut found): (Vec<(usize, f32, bool)>, Vec<([u64; 4], f32)>) = (Vec::new(), Vec::new());
        for (v, sizes) in run_parts(self.encoding_pool.as_ref(), parts, |p| {
            let mut sizes = Vec::new();
            let v: Vec<_> = (p * chunk..((p + 1) * chunk).min(n)).filter_map(|i| cull_one(i, &mut sizes)).collect();
            (v, sizes)
        }) {
            visible.extend(v);
            found.extend(sizes);
        }
        if main_view {
            sizes_now.extend(found);
            *self.object_sizes.borrow_mut() = sizes_now;
            sizes_before.clear();
            *self.object_sizes_scratch.borrow_mut() = sizes_before;
        }
        if main_view {
            drawn_before.resize(scene.instances.len().div_ceil(64), 0);
            drawn_before.fill(0);
            for &(i, _, _) in &visible {
                drawn_before[i / 64] |= 1u64 << (i % 64);
            }
            *self.cull_drawn.borrow_mut() = drawn_before;
        }
        // OMSI_DEBUG_FLICKER: a near instance in view in two frames running that is drawn in
        // one and not in the other - the objects blinking in and out as the view moves
        if with_overlays && omsi_cfg::env::var_os("OMSI_DEBUG_FLICKER").is_some() {
            let drawn: std::collections::HashSet<usize> = visible.iter().map(|v| v.0).collect();
            let mut prev = self.flicker.borrow_mut();
            let mut now: HashMap<usize, bool> = HashMap::new();
            for (i, inst) in scene.instances.iter().enumerate() {
                let m = &scene.meshes[inst.mesh];
                if m.ranges.is_empty() || !inst.visible || inst.surface {
                    continue;
                }
                let (c, r) = Self::bounding_sphere(scene, inst);
                let v = view.transform_point3(c);
                let z = -v.z;
                if v.length() > 150.0 || z + r < camera.near || v.x.abs() > z * tan_x + r / cos_x || v.y.abs() > z * tan_y + r / cos_y {
                    continue;
                }
                let d = drawn.contains(&i);
                if let Some(&was) = prev.get(&i) {
                    if was != d {
                        let od = (inst.origin - camera.position).length();
                        log::info!("flicker: instance {i} mesh {} {} at {od:.1} m (view z {z:.1}, r {r:.2}, object r {:.2}, detail {}, lod {:.3}..{:.3})", inst.mesh, if d { "appears" } else { "vanishes" }, inst.object_radius, inst.detail, inst.lod.0, inst.lod.1);
                    }
                }
                now.insert(i, d);
            }
            *prev = now;
        }
        stage(self, "cull", "mirror.cull");
        let only_surfaces = omsi_cfg::env::var_os("OMSI_ONLY_SURFACES").is_some();
        let visible: Vec<(usize, f32, bool)> = if only_surfaces {
            visible
                .into_iter()
                .filter(|(i, _, _)| scene.instances[*i].surface)
                .collect()
        } else {
            visible
        };
        if debug_draws {
            let surf = scene.instances.iter().filter(|i| i.surface).count();
            let vis_surf = visible
                .iter()
                .filter(|(i, _, _)| scene.instances[*i].surface)
                .count();
            log::info!(
                "draw: {} instances ({} surface), {} visible ({} surface)",
                scene.instances.len(),
                surf,
                visible.len(),
                vis_surf
            );
        }
        // the depth prepass: opaque and alpha-tested, single-sampled
        let mut prepass_batches: Vec<Batch> = Vec::new();
        let prepass_job = || -> (Vec<u32>, Vec<Batch>) {
            let mut items: Vec<DrawItem> = Vec::new();
            let mut list: Vec<u32> = Vec::new();
            let mut batches: Vec<Batch> = Vec::new();
            for &(i, _, _) in &visible {
                let inst = &scene.instances[i];
                let cull = culls_back_faces(scene, inst);
                for (ri, (_, _, slot)) in scene.meshes[inst.mesh].ranges.iter().enumerate() {
                    let mat_id = inst.materials.get(*slot as usize).copied().unwrap_or(0);
                    let mat = &scene.materials[mat_id];
                    let kind = kind_of(mat.alpha);
                    // Ground blends compose before they may occlude later scenery.
                    // The transmap body prepass is for ordinary meshes, not these
                    // authored surface layers (including their terrain brush masks).
                    if kind == PIPE_BLEND && world_surface_phase(effective_render_phase(inst)) {
                        continue;
                    }
                    if let Some(pre_kind) = depth_prepass_kind(kind, mat, inst.presurface) {
                        // plain/alpha-tested materials use their ordinary depth pass;
                        // blended transmaps use the opaque-pixels-only pass.
                        items.push(DrawItem {
                            pipe: pre_kind * 2 + cull as u8,
                            mesh: inst.mesh as u32,
                            range: ri as u32,
                            material: depth_only_material(pre_kind, mat_id),
                            entry: inst.base + *slot,
                        });
                    }
                }
            }
            batch_items(scene, &mut items, true, &mut list, &mut batches);
            (list, batches)
        };
        // The main pass follows the authored world phases. Each phase keeps its
        // opaque/cutout draws followed by its blended draws, far to near. Transparent
        // ground layers never write depth while composing: an alpha-zero junction
        // texel otherwise blocks a later opaque grass spline and exposes the sky
        // wherever that spline's prepass already rejected the terrain underneath.
        let mut main_batches: Vec<Batch> = Vec::new();
        // the blended (and, drawn in model order, all) slots of the vehicle the camera is in
        let mut cab_batches: Vec<Batch> = Vec::new();
        let mut cab_items: Vec<DrawItem> = Vec::new();
        let mut main_draws = [0usize; 2];
        // Keep mesh/material order here: an excavation's floor is drawn before its
        // invisible cover writes depth. Sorting its blended cover after the terrain
        // leaves the terrain's colour in place even though the cover writes depth.
        let has_presurface = visible.iter().any(|&(i, _, _)| scene.instances[i].presurface);
        let mut prepass_found: Option<(Vec<u32>, Vec<Batch>)> = None;
        let pool = self.encoding_pool.as_ref();
        in_scope(pool, |scope| {
            if prepass_on {
                let job = &prepass_job;
                let slot = &mut prepass_found;
                scope.spawn(move |_| *slot = Some(job()));
            }
            let mut by_phase: [Vec<(usize, f32, bool)>; RenderPhase::COUNT] =
                std::array::from_fn(|_| Vec::new());
            for &entry in &visible {
                let phase = effective_render_phase(&scene.instances[entry.0]);
                by_phase[phase as usize].push(entry);
            }
            // OMSI draws each authored world phase as its own opaque/cutout pass followed
            // by that phase's blended draws. Keeping the phase boundary here lets later
            // surface markings compose over spline blends without changing the depth test.
            for phase in RenderPhase::DRAW_ORDER {
                // Finish surface composition before committing the fully covered ground
                // pixels to depth. Doing this in the global prepass (or while blending
                // each spline) would reject authored road overlaps and on-surface rails.
                // Later scenery must still be occluded by the solid part of the road:
                // otherwise cutout bushes below a bridge repaint its asphalt.
                if phase == RenderPhase::BeforeNormal {
                    items.clear();
                    for &(i, _, _) in by_phase[..RenderPhase::BeforeNormal as usize].iter().flatten() {
                        let inst = &scene.instances[i];
                        for (ri, (_, _, slot)) in scene.meshes[inst.mesh].ranges.iter().enumerate() {
                            let mat_id = inst.materials.get(*slot as usize).copied().unwrap_or(0);
                            let mat = &scene.materials[mat_id];
                            if !surface_depth_coverage(effective_render_phase(inst), mat.alpha, mat.transmap.is_some(), mat.no_z_check)
                                || exclude_texture.is_some_and(|t| mat.uses_texture(t))
                            {
                                continue;
                            }
                            items.push(DrawItem {
                                pipe: pipe_code(PIPE_SURFACE_DEPTH, culls_back_faces(scene, inst), instance_depth_bias(inst, mat)),
                                mesh: inst.mesh as u32,
                                range: ri as u32,
                                material: mat_id as u32,
                                entry: inst.base + *slot,
                            });
                        }
                    }
                    batch_items(scene, &mut items, true, &mut list, &mut main_batches);
                }
                let visible = &by_phase[phase as usize];
                items.clear();
                let mut blended: Vec<usize> = Vec::new();
                for &(i, _, _) in visible {
                    let inst = &scene.instances[i];
                    if inst.ordered {
                        blended.push(i);
                        continue;
                    }
                    let mut has_blend = false;
                    let cull = culls_back_faces(scene, inst);
                    for (ri, (_, _, slot)) in scene.meshes[inst.mesh].ranges.iter().enumerate() {
                        let mat_id = inst.materials.get(*slot as usize).copied().unwrap_or(0);
                        let mat = &scene.materials[mat_id];
                        let kind = kind_of(mat.alpha);
                        if kind == PIPE_BLEND || mat.no_z_check {
                            has_blend = true;
                            continue;
                        }
                        // a render target cannot be sampled while being drawn into (mirror glass, or a reflection map of it)
                        if exclude_texture.is_some_and(|t| mat.uses_texture(t)) {
                            continue;
                        }
                        items.push(DrawItem {
                            pipe: pipe_code(
                                kind,
                                cull,
                                instance_depth_bias(inst, mat),
                            ),
                            mesh: inst.mesh as u32,
                            range: ri as u32,
                            material: mat_id as u32,
                            entry: inst.base + *slot,
                        });
                    }
                    if has_blend {
                        blended.push(i);
                    }

                }
                main_draws[0] += items.len();
                batch_items(scene, &mut items, true, &mut list, &mut main_batches);
                // Blended draws: objects far to near by the distance of their nearest blended
                // mesh (see `near_by_origin` below - not the single local origin all of an
                // object's meshes share), and within an object in creation order - the
                // model.cfg mesh order, which is what the original relies on (windows are
                // listed last).
                //
                // An object the camera is inside of (the bus seen from the driver's
                // seat) comes after everything outside it, and the player's own vehicle
                // last of all. By its origin alone the bus - whose origin is 4.6 m
                // behind the driver's eye on the NL202 - sorted as farther away than a
                // car right beside the driver's window, so the car was drawn after the
                // bus's window layers (rain film, dirt, door glass), which write depth:
                // its blended body failed the depth test and only the opaque wheels
                // were left, dark behind the tinted glass, exactly while the car was
                // half out of the picture.
                let mut holders: Vec<DVec3> = Vec::new();
                for &(i, _, inside) in visible {
                    let inst = &scene.instances[i];
                    if inside && !inst.surface && !holders.contains(&inst.origin) {
                        holders.push(inst.origin);
                    }
                }
                let player = lighting
                    .inside
                    .filter(|v| point_in_vehicle_box(camera.position, v))
                    .map(|v| v.0);
                // An object's *nearest* blended mesh to the camera, not the single point its
                // meshes all share (`inst.origin`): a long vehicle's own origin can sit well
                // behind (or ahead of) its nearest window, so ranking the whole object by that
                // one point against a much smaller nearby object - a car passing level with the
                // middle of a stopped bus - picked the wrong order even outside the "camera is
                // inside" case above (the bus's origin, metres behind the window nearest the
                // car, sorted as farther away than the car itself, so the car was drawn last and
                // painted over the window instead of being hidden behind the body between the
                // windows). Every blended mesh of the object is a candidate; the closest one's
                // distance, less its own bounding radius, stands for the whole object.
                //
                // Scope, checked systematically while chasing a report of a car showing through
                // a stopped bus's body from outside (never reproduced, before or after this
                // commit): this order only ever decides how mutually-*blended* draws composite
                // where they overlap on screen (a car's own window glass in front of a bus's
                // window + interior, say) - it cannot be why an opaque wall would fail to hide
                // something behind it. Every pipeline the main pass uses, opaque or blended,
                // keeps depth *testing* on (`GreaterEqual`, see the pipeline table above); only
                // depth *writing* differs. Opaque batches are always recorded before blended ones
                // in the same pass (`main_draws[0]` first), so by the time any blended draw runs,
                // the depth buffer already holds every opaque surface in front of it, blend order
                // or not. Dumping the EN92's and the O530 Facelift's per-material alpha mode
                // (`OMSI_ONLY_MESH`) found every body panel `AlphaMode::Opaque`, as OMSI requires
                // (diffuse alpha is a reflection mask, not transparency, unless `[matl_alpha]` 1
                // or 2 says otherwise); an A/B render (this commit vs its parent, same seed, a
                // parked car centred behind a stopped EN92's midsection) came back pixel-identical
                // at the car/bus silhouette - the only measured difference was in the bus's own
                // overlapping window/dirt/interior layers, which is exactly this sort's stated
                // job. If the reported artefact is real, its cause is still open and elsewhere.
                let near_by_origin = if self.blend_by_origin {
                    HashMap::new()
                } else {
                    nearest_by_origin(blended.iter().filter_map(|&i| {
                        let inst = &scene.instances[i];
                        if inst.surface {
                            return None;
                        }
                        let (c, r) = Self::bounding_sphere(scene, inst);
                        Some((inst.origin, (c - cam_rel).length() - r))
                    }))
                };
                let mut keyed: Vec<(u8, f32, usize)> = blended
                    .iter()
                    .map(|&i| {
                        let inst = &scene.instances[i];
                        // Surfaces are ground and go by distance alone: a tile's painted ground
                        // shares its origin with the terrain the camera is always inside of, and
                        // ranked with it, it was drawn after everything blended near it - over the
                        // shadow blobs of the buses standing on it. A blob belongs to the ground
                        // under its vehicle too, drawn before the vehicle's glass.
                        let rank = if self.blend_by_origin || inst.surface {
                            0
                        } else if player == Some(inst.origin) {
                            2
                        } else if holders.contains(&inst.origin) {
                            1
                        } else {
                            0
                        };
                        let dist = if let Some(sort_origin) = inst.blend_sort_origin {
                            // Spline surfaces use the C++ handler's placement-origin distance
                            // in the horizontal plane (Rust's world axes are x/y horizontal,
                            // z vertical).
                        horizontal_sort_distance(sort_origin, ro, cam_rel)
                        } else if self.blend_by_origin || inst.surface {
                            ((inst.origin - ro).as_vec3() - cam_rel).length()
                        } else {
                            near_by_origin
                                .get(&origin_key(inst.origin))
                                .copied()
                                .unwrap_or(0.0)
                        };
                        (rank, dist, i)
                    })
                    .collect();
                // (a total order even where a distance is NaN - an instance at a NaN position:
                // partial_cmp's "equal" for it broke the sort's order, and since Rust 1.81 the
                // sort panics on that, which ended the game)
                keyed.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));
                items.clear();
                for (rank, _, i) in keyed {
                    let inst = &scene.instances[i];
                    let cull = culls_back_faces(scene, inst);
                    for (ri, (_, _, slot)) in scene.meshes[inst.mesh].ranges.iter().enumerate() {
                        let mat_id = inst.materials.get(*slot as usize).copied().unwrap_or(0);
                        let mat = &scene.materials[mat_id];
                        if (mat.alpha != AlphaMode::Blend && !mat.no_z_check && !inst.ordered)
                            || exclude_texture.is_some_and(|t| mat.uses_texture(t))
                        {
                            continue;
                        }
                        // A layer its script has faded out (`[alphascale]` at 0: the rain film
                        // on a dry day, the dirt on a clean bus) shows nothing, as in the
                        // original, whose blend takes it out whole; drawn anyway it ran the full
                        // shading over the whole windscreen for nothing - a bus's cab view had
                        // three or four such screen-sized layers.
                        if mat.alpha == AlphaMode::Blend && inst.slot_alpha.get(*slot as usize).is_some_and(|a| *a < 1.0 / 512.0) {
                            continue;
                        }
                        // Ground blends use the C++ handler's no-write composition;
                        // their opaque coverage is committed after OnSurface. Other
                        // materials retain [matl_noZwrite] (glass, rain, dirt) semantics.
                        // [matl_noZcheck] marks a decal that must win over the surface it lies
                        // on (a bus's shadow blob, the digits on a counter): the original draws
                        // it without a depth test right after that surface, in model order. With
                        // everything opaque drawn first here, no test at all would put it over
                        // the whole bus (the steering wheel in front of the counter, the body over
                        // the shadow), so it is drawn with the surfaces' depth bias instead: on
                        // top of its base, behind whatever really stands in front of it.
                        let kind = if mat.alpha != AlphaMode::Blend && !mat.no_z_check {
                            // (a model drawn in order: its opaque and cut-out slots too)
                            kind_of(mat.alpha)
                        } else if (mat.no_z_write && !mat.writes_depth) || mat.no_z_check || (world_surface_phase(inst.render_phase) && !inst.presurface) {
                            PIPE_BLEND_NO_WRITE
                        } else {
                            PIPE_BLEND
                        };
                        let item = DrawItem {
                            pipe: pipe_code(
                                kind,
                                cull,
                                instance_depth_bias(inst, mat),
                            ),
                            mesh: inst.mesh as u32,
                            range: ri as u32,
                            material: mat_id as u32,
                            entry: inst.base + *slot,
                        };
                        // the vehicle the camera is in is drawn after everything else
                        if rank == 2 {
                            cab_items.push(item);
                        } else {
                            items.push(item);
                        }
                    }
                }
                main_draws[1] += items.len();
                batch_items(scene, &mut items, false, &mut list, &mut main_batches);
            }
            // Omsi.exe draws the vehicle the camera sits in last of all, with the view mask
            // of its inside (0x6f1520 -> 0x6f0430), after every phase of the map, the other
            // vehicles, the particles and the lamps' flares (0x6f0400/0x6f0418): its glass,
            // whose depth is written unless the model says `[matl_noZwrite]`, then lies over
            // all of that. Its items wait here and are drawn after the coronas and the smoke
            // (see `cab_batches` in the main pass).
            main_draws[1] += cab_items.len();
            batch_items(scene, &mut cab_items, false, &mut list, &mut cab_batches);
        });
        if let Some((pre_list, mut pre_batches)) = prepass_found {
            let offset = list.len() as u32;
            for b in &mut pre_batches {
                b.instances = b.instances.start + offset..b.instances.end + offset;
            }
            list.extend(pre_list);
            prepass_batches = pre_batches;
        }
        // OMSI_SKIP_PIPE=3,1: leave pipeline kinds out of the main pass (0 opaque, 1 alpha
        // tested, 2 blended, 3 blended without depth writes, 4 surface depth) - with
        // OMSI_GPU_TIMERS_RAW, what each kind costs the GPU
        if let Ok(skip) = omsi_cfg::env::var("OMSI_SKIP_PIPE") {
            let skip: Vec<u8> = skip.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            main_batches.retain(|b| !skip.contains(&(b.pipe / 4)));
            cab_batches.retain(|b| !skip.contains(&(b.pipe / 4)));
        }
        if debug_draws {
            log::info!("  main pass: {} opaque/alpha-tested and {} blended draws in {} batches; prepass {} batches; draw list {} entries", main_draws[0], main_draws[1], main_batches.len(), prepass_batches.len(), list.len());
        }
        if self.profiling && !with_overlays {
            let mut c = self.counts.borrow_mut();
            *c.entry("mirror pictures").or_default() += 1.0;
            *c.entry("mirror visible instances").or_default() += visible.len() as f64;
        }
        if self.profiling && with_overlays {
            let mut c = self.counts.borrow_mut();
            *c.entry("scene instances").or_default() += scene.instances.len() as f64;
            *c.entry("visible instances").or_default() += visible.len() as f64;
            *c.entry("main draws").or_default() += (main_draws[0] + main_draws[1]) as f64;
            *c.entry("opaque draws").or_default() += main_draws[0] as f64;
            *c.entry("blended draws").or_default() += main_draws[1] as f64;
            *c.entry("main batches").or_default() += main_batches.len() as f64;
            *c.entry("prepass batches").or_default() += prepass_batches.len() as f64;
            *c.entry("shadow batches").or_default() +=
                (shadow_batches[0].len() + shadow_batches[1].len()) as f64;
            // triangles each pass draws (thousands), the geometry the GPU goes through
            let tris = |bs: &[Batch]| bs.iter().map(|b| b.count as f64 / 3.0 * b.instances.len() as f64).sum::<f64>() / 1000.0;
            *c.entry("ktris main").or_default() += tris(&main_batches);
            *c.entry("ktris prepass").or_default() += tris(&prepass_batches);
            *c.entry("ktris shadow near").or_default() += tris(&shadow_batches[0]);
            *c.entry("ktris shadow far").or_default() += tris(&shadow_batches[1]);
            *c.entry("ktris shadow close").or_default() += tris(&shadow_batches[2]);
        }
        if self.profiling && with_overlays && self.draw_audit_at.elapsed().as_secs() >= 10 {
            self.draw_audit_at = std::time::Instant::now();
            // (batches, draws, triangles) per asset: what the CPU encodes and what the GPU
            // goes through
            let mut assets: HashMap<&str, (usize, usize, u64)> = HashMap::new();
            for b in &main_batches {
                let source = scene.meshes[b.mesh as usize].source.as_deref().unwrap_or("procedural / vehicle");
                let cost = assets.entry(source).or_default();
                cost.0 += 1;
                cost.1 += b.instances.len();
                cost.2 += b.count as u64 / 3 * b.instances.len() as u64;
            }
            let mut assets: Vec<_> = assets.into_iter().collect();
            assets.sort_unstable_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
            for (source, (batches, draws, tris)) in assets.iter().take(12) {
                log::info!("draw audit: {batches} batches, {draws} draws, {tris} triangles: {source}");
            }
            assets.sort_unstable_by(|a, b| b.1.2.cmp(&a.1.2).then(a.0.cmp(b.0)));
            for (source, (batches, draws, tris)) in assets.iter().take(12) {
                log::info!("triangle audit: {tris} triangles in {draws} draws ({batches} batches): {source}");
            }
        }
        stage(self, "items", "mirror.items");
        let mut rain_batches = Vec::new();
        if glass_on {
            let (rain, main): (Vec<_>, Vec<_>) = main_batches.into_iter().partition(|b| scene.materials[b.material as usize].uniform.emissive[3] > 1.5);
            rain_batches = rain;
            main_batches = main;
        }
        self.upload_draw_list(scene, &list);
        stage(self, "upload", "mirror.upload");
        // OMSI_NO_BUNDLES=1 records the main pass directly, for comparison. (Splitting the
        // pass in two to finish the halves side by side was tried as well: the second half
        // has to load the first one's targets back into the GPU's tile memory, which cost
        // more GPU time than it saved on the CPU.)
        let main_bundles = if omsi_cfg::env::var_os("OMSI_NO_BUNDLES").is_none() {
            let pp = self.main_pass(enhanced, reflection_frame);
            let format = if masked_frame { HDR_FORMAT } else { self.format };
            record_bundles(
                &self.device,
                self.encoding_pool.as_ref(),
                scene,
                &main_batches,
                pp,
                scene.camera_bind_group.as_ref().expect("camera bind group"),
                format,
                self.options.msaa,
            )
        } else {
            Vec::new()
        };
        stage(self, "bundles", "mirror.bundles");
        // three command buffers, finished side by side (see below): the shadow maps, the
        // depth prepass with the ambient occlusion, and the picture itself
        let mut shadow_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("shadow maps"),
                });
        self.flush_pending_meshes(scene, &mut shadow_encoder);
        let mut prepass_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("depth prepass"),
                });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("picture"),
            });
        for cascade in [0usize, 1] {
            if !draw_shadows || (cascade == 1 && !redraw_far) {
                continue;
            }
            let view = if cascade == 0 {
                &self.shadow_view
            } else {
                &self.shadow_view_far
            };
            let keep_near = cascade == 0 && !redraw_near;
            let mut pass = shadow_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("shadow"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view,
                    depth_ops: Some(wgpu::Operations {
                        load: if keep_near { wgpu::LoadOp::Load } else { wgpu::LoadOp::Clear(1.0) },
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: pass_timer(
                    tset.as_ref(),
                    &mut timed,
                    if cascade == 0 {
                        "shadow near"
                    } else {
                        "shadow far"
                    },
                ),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, scene.shadow_bind_group.as_ref().unwrap(), &[]);
            if cascade == 0 {
                // the atlas: near cascade on the left half, close cascade on the right
                let sz = self.options.shadow_size as f32;
                pass.set_viewport(0.0, 0.0, sz, sz, 0.0, 1.0);
                encode_batches(&mut pass, scene, &shadow_batches[0], |pipe| {
                    &self.shadow_pipelines[pipe as usize]
                });
                let csz = self.options.shadow_size.min(SHADOW_CLOSE_MAX) as f32;
                pass.set_viewport(sz, 0.0, csz, csz, 0.0, 1.0);
                if keep_near {
                    // (the near half is last frame's: only the close part is cleared)
                    pass.set_pipeline(&self.shadow_clear_pipeline);
                    pass.draw(0..3, 0..1);
                    pass.set_bind_group(0, scene.shadow_bind_group.as_ref().unwrap(), &[]);
                }
                encode_batches(&mut pass, scene, &shadow_batches[2], |pipe| {
                    &self.shadow_pipelines[4 + pipe as usize]
                });
            } else {
                encode_batches(&mut pass, scene, &shadow_batches[cascade], |pipe| {
                    &self.shadow_pipelines[cascade * 2 + pipe as usize]
                });
            }
        }
        // --- depth prepass + ambient occlusion (single-sampled, camera projection)
        if prepass_on {
            let proj = projection.unwrap_or_else(|| {
                Mat4::perspective_rh(camera.fov_deg.to_radians(), aspect, camera.far, camera.near)
            });
            let u = SsaoUniform {
                inv_proj: proj.inverse().to_cols_array_2d(),
                params: [
                    1.0,
                    1.4,
                    width.div_ceil(2) as f32,
                    height.div_ceil(2) as f32,
                ],
                shift: [proj.z_axis.x, proj.z_axis.y, 0.0, 0.0],
            };
            self.queue
                .write_buffer(&self.ao_buf, 0, bytemuck::bytes_of(&u));
            let ao = self.ao.as_ref().unwrap();
            {
                let mut pass = prepass_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("depth prepass"),
                    color_attachments: &[],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &ao.depth_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(0.0),
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: pass_timer(tset.as_ref(), &mut timed, "depth prepass"),
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_bind_group(0, scene.camera_bind_group.as_ref().unwrap(), &[]);
                encode_batches(&mut pass, scene, &prepass_batches, |pipe| {
                    &self.prepass_pipelines[pipe as usize]
                });
            }
            for (pipe, bg, target, pass_label) in [
                (&self.ssao_pipeline, &ao.ssao_bg, &ao.ao_view, "ssao"),
                (&self.blur_pipeline, &ao.blur_bg, &ao.blur_view, "ssao blur"),
            ] {
                let Some(pipe) = pipe.as_ref().filter(|_| ao_on) else {
                    break;
                };
                let mut pass = prepass_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("ssao"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: pass_timer(tset.as_ref(), &mut timed, pass_label),
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(pipe);
                pass.set_bind_group(0, bg, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        // --- the enhanced sky cube: a face a frame (all six the first time and for a new
        // sky), drawn in the window's frame only
        if enhanced && (lead_view || probe_redraw) {
            if let (Some(probe), Some(sky_bg)) = (self.probe.as_mut(), scene.sky_bind_group.as_ref()) {
                // (face, round, the old picture's share): a whole new cube is every round
                // of every face averaged; afterwards one face a frame, blended in
                let full = !probe.cube_filled || self.instant_exposure;
                probe.cube_wait += 1;
                let recapture = std::mem::take(&mut probe.cube_recapture);
                let draws: Vec<(u32, u32, f64)> = if full {
                    (0..6)
                        .flat_map(|f| (0..SKY_CUBE_ROUNDS).map(move |r| (f, r, r as f64 / (r as f64 + 1.0))))
                        .collect()
                } else if recapture {
                    // the eye moved on: every face from the new one, nothing kept from the
                    // old place
                    let round = (probe.cube_round / 6) % SKY_CUBE_ROUNDS;
                    (0..6).map(|f| (f, round, 0.0)).collect()
                } else if (probe.cube_wait >= SKY_CUBE_EVERY && !redraw_near) || probe.cube_wait >= SKY_CUBE_EVERY * 2 || !lead_view {
                    // (on a frame that keeps the near shadow map: the two costliest
                    // occasional passes never fall on the same frame)
                    vec![(probe.cube_next, (probe.cube_round / 6) % SKY_CUBE_ROUNDS, SKY_CUBE_HISTORY)]
                } else {
                    Vec::new()
                };
                let single = draws.len() == 1;
                if !draws.is_empty() {
                    probe.cube_wait = 0;
                    probe.cube_next = (probe.cube_next + 1) % 6;
                    probe.cube_round = probe.cube_round.wrapping_add(1);
                }
                probe.cube_filled = true;
                for (f, round, history) in draws {
                    let mut pass = prepass_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("sky cube"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &probe.cube_faces[f as usize],
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: if history > 0.0 { wgpu::LoadOp::Load } else { wgpu::LoadOp::Clear(wgpu::Color::BLACK) },
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: if single { pass_timer(tset.as_ref(), &mut timed, "sky cube") } else { None },
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(&probe.cube_pipeline);
                    pass.set_blend_constant(wgpu::Color { r: history, g: history, b: history, a: history });
                    pass.set_bind_group(0, &probe.cube_bind_groups[(f * SKY_CUBE_ROUNDS + round) as usize], &[]);
                    pass.set_bind_group(1, sky_bg, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
        }
        // --- the reflection probe of the enhanced path: the sky into the six faces, then
        // each blurrier level from the sharper ones (three faces a pass)
        if probe_redraw {
            if let (Some(probe), Some(sky_bg)) =
                (self.probe.as_ref(), scene.sky_bind_group.as_ref())
            {
                for (m, faces) in probe.faces.iter().enumerate() {
                    for (half, bg) in probe.bind_groups[m].iter().enumerate() {
                        let attachments: Vec<Option<wgpu::RenderPassColorAttachment>> = faces
                            [half * 3..half * 3 + 3]
                            .iter()
                            .map(|v| {
                                Some(wgpu::RenderPassColorAttachment {
                                    view: v,
                                    depth_slice: None,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                        store: wgpu::StoreOp::Store,
                                    },
                                })
                            })
                            .collect();
                        let mut pass =
                            prepass_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: Some("reflection probe"),
                                color_attachments: &attachments,
                                depth_stencil_attachment: None,
                                timestamp_writes: if m == 0 && half == 0 {
                                    pass_timer(tset.as_ref(), &mut timed, "probe")
                                } else {
                                    None
                                },
                                occlusion_query_set: None,
                                multiview_mask: None,
                            });
                        pass.set_pipeline(if m == 0 {
                            &probe.sky_pipeline
                        } else {
                            &probe.filter_pipeline
                        });
                        pass.set_bind_group(0, bg, &[]);
                        pass.set_bind_group(1, sky_bg, &[]);
                        pass.draw(0..3, 0..1);
                    }
                }
            }
        }
        // Without multisampling the main pass tests against the depth the prepass left
        // (when there was one): the costly shading - lighting, the shadow filter - is then
        // done once per pixel for the surface that is seen, not for every tree and wall
        // hidden behind it.
        let single = self.options.msaa <= 1;
        // A presurface must colour its below-ground faces before its invisible cover
        // seals them. Reusing prepass depth would reject those faces (or let terrain
        // reject them first). The prepass still supplies AO; colour rebuilds its depth.
        // A depth-writing window in the colour pass must not replace the road receiver.
        let share_depth = prepass_on && single && self.ao.is_some() && !has_presurface && !puddles_wanted;
        let targets = if share_depth {
            None
        } else {
            Some(self.msaa_targets(width, height))
        };
        // With multisampling the prepass above (single-sampled, for the ambient occlusion)
        // cannot be the main pass's depth: the enhanced picture lays its depth again into
        // the multisampled buffer first. Without it every wall, tree and car hidden behind
        // the one in front ran the whole enhanced shading (11 ms of a 1080p frame in
        // central Spandau with 4x MSAA).
        let msaa_prepass = enhanced
            && !has_presurface
            && (with_overlays || xr_view)
            && !single
            && prepass_on
            && omsi_cfg::env::var_os("OMSI_NO_MSAA_PREPASS").is_none();
        let parts = if !cfg!(any(target_os = "macos", target_os = "ios")) && main_bundles.len() >= 2 && omsi_cfg::env::var_os("OMSI_NO_MAIN_SPLIT").is_none() {
            main_bundles.len().min(2)
        } else {
            1
        };
        let mut lead = (parts > 1).then(|| self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("main part") }));
        if msaa_prepass {
            if let (Some(pipes), Some(t)) = (self.prepass_msaa_pipelines.as_ref(), targets.as_ref()) {
                let mut pass = lead.as_mut().unwrap_or(&mut encoder).begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("msaa depth prepass"),
                    color_attachments: &[],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &t.1,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(0.0),
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: pass_timer(tset.as_ref(), &mut timed, "msaa prepass"),
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_bind_group(0, scene.camera_bind_group.as_ref().unwrap(), &[]);
                // Alpha-tested colour draws use alpha-to-coverage, but the depth-only
                // prepass uses a binary 0.5 cutoff. Letting those meshes write depth here
                // can hide the opaque geometry behind samples the colour pass leaves
                // uncovered (the sky then shows through buildings/terrain behind foliage).
                // The main alpha-tested pass writes matching depth as it draws the colour.
                encode_batches_filtered(
                    &mut pass,
                    scene,
                    &prepass_batches,
                    |batch| batch.pipe / 2 != PIPE_ALPHA_TEST,
                    |pipe| &pipes[pipe as usize],
                );
            }
        }
        let msaa_prepass = msaa_prepass && self.prepass_msaa_pipelines.is_some() && targets.is_some();
        let mut main_parts: Vec<wgpu::CommandEncoder> = Vec::new();
        {
            // The enhanced sky dome was meant to cover everything, so this used to clear to
            // black on that assumption - but the dome is a hemisphere, not a full sphere, and
            // wherever the ground does not quite reach (a streamed tile not loaded yet, a gap
            // right at the horizon) that showed as a stark black void, where vanilla's plain
            // sky colour clear made the very same gap invisible. Using that same colour here
            // (unscaled - multiplying it by the enhanced exposure blew a night sky's dim clear
            // colour out to white instead) keeps a real gap from ever reading as a rendering
            // bug of its own.
            let sky = if lighting.classic && !enhanced { lighting.sky_color.map(|v| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }) } else { lighting.sky_color };
            let msaa_color = targets.as_ref().map(|t| &t.0);
            let depth_view: &wgpu::TextureView = match &targets {
                Some(t) => &t.1,
                None => &self.ao.as_ref().unwrap().depth_view,
            };
            // the enhanced path draws into a high-range picture the post pass then grades
            let hdr = if masked_frame {
                self.hdr_targets.get(&(width, height))
            } else {
                None
            };
            let (draw_view, resolve_view): (&wgpu::TextureView, Option<&wgpu::TextureView>) =
                match hdr {
                    Some(h) => match &h.msaa_view {
                        Some(m) => (m, Some(&h.view)),
                        None => (&h.view, None),
                    },
                    None => {
                        if single {
                            (scene_view, None)
                        } else {
                            (msaa_color.expect("multisampled target"), Some(scene_view))
                        }
                    }
                };
            let pp = self.main_pass(enhanced, reflection_frame);
            // the enhanced pass's screen mask beside the picture (see `MASK_FORMAT`)
            let mask_attachment = hdr.map(|h| wgpu::RenderPassColorAttachment {
                view: h.mask_msaa.as_ref().unwrap_or(&h.mask),
                depth_slice: None,
                resolve_target: h.mask_msaa.as_ref().map(|_| &h.mask),
                ops: wgpu::Operations {
                    load: if parts > 1 { wgpu::LoadOp::Load } else { wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT) },
                    store: if h.mask_msaa.is_some() { wgpu::StoreOp::Discard } else { wgpu::StoreOp::Store },
                },
            });
            let per_part = main_bundles.len().div_ceil(parts.max(1));
            let sky_clear = wgpu::LoadOp::Clear(wgpu::Color { r: sky.x as f64, g: sky.y as f64, b: sky.z as f64, a: 1.0 });
            let depth_first = if share_depth || msaa_prepass { wgpu::LoadOp::Load } else { wgpu::LoadOp::Clear(0.0) };
            for g in 0..parts.saturating_sub(1) {
                let first = g == 0;
                let mut part = lead.take().unwrap_or_else(|| self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("main part") }));
                {
                    let part_colors = [
                        Some(wgpu::RenderPassColorAttachment {
                            view: draw_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations { load: if first { sky_clear } else { wgpu::LoadOp::Load }, store: wgpu::StoreOp::Store },
                        }),
                        hdr.map(|h| wgpu::RenderPassColorAttachment {
                            view: h.mask_msaa.as_ref().unwrap_or(&h.mask),
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations { load: if first { wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT) } else { wgpu::LoadOp::Load }, store: wgpu::StoreOp::Store },
                        }),
                    ];
                    let mut pass = part.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("main part"),
                        color_attachments: if part_colors[1].is_some() { &part_colors[..] } else { &part_colors[..1] },
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: depth_view,
                            depth_ops: Some(wgpu::Operations { load: if first { depth_first } else { wgpu::LoadOp::Load }, store: wgpu::StoreOp::Store }),
                            stencil_ops: None,
                        }),
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_bind_group(0, scene.camera_bind_group.as_ref().unwrap(), &[]);
                    if first {
                        if let Some(sky) = &scene.sky_bind_group {
                            pass.set_pipeline(&pp.sky_pipeline);
                            pass.set_bind_group(1, sky, &[]);
                            pass.set_vertex_buffer(0, self.sky_mesh.0.slice(..));
                            pass.set_index_buffer(self.sky_mesh.1.slice(..), wgpu::IndexFormat::Uint32);
                            pass.draw_indexed(0..self.sky_mesh.2, 0, 0..1);
                        }
                    }
                    pass.execute_bundles(main_bundles[g * per_part..((g + 1) * per_part).min(main_bundles.len())].iter());
                }
                main_parts.push(part);
            }
            let tail = (parts - 1) * per_part;
            let main_attachment = Some(wgpu::RenderPassColorAttachment {
                    view: draw_view,
                    depth_slice: None,
                    resolve_target: resolve_view,
                    ops: wgpu::Operations {
                        load: if parts > 1 { wgpu::LoadOp::Load } else { sky_clear },
                        store: if resolve_view.is_none() {
                            wgpu::StoreOp::Store
                        } else {
                            wgpu::StoreOp::Discard
                        },
                    },
                });
            let colors = [main_attachment, mask_attachment];
            let colors = if colors[1].is_some() { &colors[..] } else { &colors[..1] };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                // drawn with MSAA samples and resolved into the real target at the end
                // (without multisampling straight into the target)
                color_attachments: colors,
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: if parts > 1 { wgpu::LoadOp::Load } else { depth_first },
                        // (nothing reads the picture's depth after the pass - the ambient
                        // occlusion reads the prepass's own texture - so where the depth is
                        // not carried over, a tile-based GPU need not flush a full-size
                        // depth buffer back)
                        store: if share_depth || msaa_prepass || ao_on {
                            wgpu::StoreOp::Store
                        } else {
                            wgpu::StoreOp::Discard
                        },
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: pass_timer(
                    tset.as_ref(),
                    &mut timed,
                    if with_overlays { "main" } else { "mirror" },
                ),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, scene.camera_bind_group.as_ref().unwrap(), &[]);
            if let Some(sky) = scene.sky_bind_group.as_ref().filter(|_| parts == 1) {
                pass.set_pipeline(&pp.sky_pipeline);
                pass.set_bind_group(1, sky, &[]);
                pass.set_vertex_buffer(0, self.sky_mesh.0.slice(..));
                pass.set_index_buffer(self.sky_mesh.1.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.sky_mesh.2, 0, 0..1);
            }
            if main_bundles.is_empty() {
                pass.set_bind_group(0, scene.camera_bind_group.as_ref().unwrap(), &[]);
                encode_batches(&mut pass, scene, &main_batches, |pipe| {
                    main_pipeline(pp, pipe)
                });
            } else {
                // the batches, recorded as bundles on several threads (see `record_bundles`)
                pass.execute_bundles(main_bundles[tail..].iter());
                // a bundle leaves the pass without bind groups
                pass.set_bind_group(0, scene.camera_bind_group.as_ref().unwrap(), &[]);
            }
            // smoke, blended over the scene
            if scene.smoke_count > 0 && omsi_cfg::env::var_os("OMSI_NO_SMOKE").is_none() {
                if let Some(sb) = &scene.smoke_buf {
                    pass.set_pipeline(&pp.smoke_pipeline);
                    pass.set_bind_group(1, &self.smoke_bind_group, &[]);
                    pass.set_vertex_buffer(0, sb.slice(..));
                    pass.draw(0..6, 0..scene.smoke_count);
                }
            }
            // light coronas, additive: the world's, then the vehicle the camera is in -
            // drawn over them as Omsi.exe draws it last (see `cab_items`) - then its own
            let coronas_on = scene.corona_count > 0 && omsi_cfg::env::var_os("OMSI_NO_CORONAS").is_none();
            for late in [false, true] {
                if late && !cab_batches.is_empty() {
                    pass.set_bind_group(0, scene.camera_bind_group.as_ref().unwrap(), &[]);
                    encode_batches(&mut pass, scene, &cab_batches, |pipe| main_pipeline(pp, pipe));
                }
                if let Some(cb) = scene.corona_buf.as_ref().filter(|_| coronas_on) {
                    pass.set_pipeline(&pp.corona_pipeline);
                    pass.set_bind_group(0, scene.camera_bind_group.as_ref().unwrap(), &[]);
                    pass.set_vertex_buffer(0, cb.slice(..));
                    // in runs by picture (the standard glow, the lights' own bitmaps, the cone)
                    for &(tex, first, count, _) in scene.corona_runs.iter().filter(|r| r.3 == late) {
                        let bg = self.corona_textures.get(tex as usize).and_then(|b| b.as_ref()).unwrap_or(&self.corona_bind_group);
                        pass.set_bind_group(1, bg, &[]);
                        pass.draw(0..6, first..first + count);
                    }
                }
            }
            // HUD overlays (on the vanilla path at full size; the enhanced path draws them
            // after grading, a scaled picture after scaling it up)
            if !overlays.is_empty() && !masked_frame && !scaled {
                pass.set_pipeline(&self.overlay_pipeline);
                for (k, _) in overlays.iter().enumerate() {
                    if let Some((_, _, bg, _)) = scene.overlay_res.get(k) {
                        pass.set_bind_group(0, bg, &[]);
                        pass.draw(0..6, 0..1);
                    }
                }
            }
        }
        // Weather alone is insufficient: leave the allocation and reflection passes out when
        // the visible batches contain no moisture-tagged surface (a showroom, bare terrain).
        let puddles_on = puddles_wanted
            && main_batches.iter().any(|b| scene.materials[b.material as usize].uniform.params2[2] > 0.0)
            && self.prepare_puddle_reflections(width, height, camera, aspect, projection, &cu, lighting, ro);
        if puddles_on {
            // (the vehicle the camera is in is drawn into the puddles' picture as well)
            let all: Vec<Batch>;
            let batches = if cab_batches.is_empty() {
                &main_batches
            } else {
                all = main_batches.iter().chain(&cab_batches).cloned().collect();
                &all
            };
            self.encode_puddle_reflections(&mut encoder, width, height, scene, batches, &list, lighting, camera, tset.as_ref(), &mut timed);
        }
        if glass_on {
            let hdr = masked_frame.then(|| &self.hdr_targets[&(width, height)]);
            let view = hdr.map_or(scene_view, |h| h.puddles.as_ref().filter(|_| puddles_on).map_or(&h.view, |p| &p.view));
            let behind = self.glass_picture.as_ref().unwrap();
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo { texture: view.texture(), mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyTextureInfo { texture: behind.texture(), mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
            let colours = [
                Some(wgpu::RenderPassColorAttachment {
                    view, depth_slice: None, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                }),
                hdr.map(|h| wgpu::RenderPassColorAttachment {
                    view: &h.mask, depth_slice: None, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                }),
            ];
            let pipes = &self.main_pass(enhanced, reflection_frame).rain_pipelines;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rain on current scene"),
                color_attachments: &colours[..if hdr.is_some() { 2 } else { 1 }],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.ao.as_ref().unwrap().depth_view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None, occlusion_query_set: None, multiview_mask: None,
            });
            pass.set_bind_group(0, scene.camera_bind_group.as_ref().unwrap(), &[]);
            encode_batches(&mut pass, scene, &rain_batches, |pipe| &pipes[pipe as usize]);
        }
        if reflection_frame {
            let h = &self.hdr_targets[&(width, height)];
            let bg = h.puddles.as_ref().filter(|_| puddles_on).map_or(&h.classic_bg, |p| &p.classic_bg);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("classic reflections present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: scene_view, depth_slice: None, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None, timestamp_writes: None,
                occlusion_query_set: None, multiview_mask: None,
            });
            pass.set_pipeline(&self.copy_pipeline);
            pass.set_bind_group(0, bg, &[]);
            pass.draw(0..3, 0..1);
            if !overlays.is_empty() && !scaled {
                pass.set_pipeline(&self.overlay_pipeline_1x);
                for (k, _) in overlays.iter().enumerate() {
                    if let Some((_, _, bg, _)) = scene.overlay_res.get(k) {
                        pass.set_bind_group(0, bg, &[]);
                        pass.draw(0..6, 0..1);
                    }
                }
            }
        }
        if enhanced {
            // --- the post passes: glow, metering and adaptation, tone curve, FXAA
            let secs = |tau: f32| {
                if dt > 0.0 {
                    1.0 - (-dt / tau).exp()
                } else {
                    1.0
                }
            };
            let m = meter_tuning();
            let pu = PostUniform {
                // the metering may take a little off a bright picture and add a little to a
                // dark one: a night stays a night, snow stays white
                a: [
                    0.035,
                    m[2],
                    m[3],
                    if self.instant_exposure || dt <= 0.0 {
                        1.0
                    } else {
                        0.0
                    },
                ],
                // darker: the eye takes a few seconds; brighter: under one
                b: [secs(2.5), secs(0.6), m[1], m[4]],
                // (w: an LED panel's dots count for this much in the glow's source. The mix
                // the glow lands with is a few per cent - a lamp a hundred times brighter
                // than white spreads, a white wall does not - so the dots are multiplied up
                // there instead of being drawn burning: their halo shows, they don't bleach.
                // `Led glow`, 0 = not at all.)
                c: [m[0], m[5], self.exposure.map(f32::exp).unwrap_or(1.0), lighting.led_glow * 10.0],
            };
            self.queue
                .write_buffer(&self.post_buf, 0, bytemuck::bytes_of(&pu));
            // (a mirror's small picture goes without FXAA)
            let fxaa = with_overlays && self.options.fxaa && omsi_cfg::env::var_os("OMSI_NO_FXAA").is_none();
            if let Some(h) = self.hdr_targets.get(&(width, height)) {
                let puddles = h.puddles.as_ref().filter(|_| puddles_on);
                let levels = h.down.len();
                for i in 0..levels {
                    post_pass(
                        &mut encoder,
                        &h.down[i],
                        None,
                        if i == 0 {
                            &self.post.down_first
                        } else {
                            &self.post.down
                        },
                        if i == 0 { puddles.map(|p| &p.down_bg).unwrap_or(&h.down_bg[i]) } else { &h.down_bg[i] },
                    );
                }
                // the exposure: meter the smallest level, move the adapted value towards it
                // (the window's picture only: a mirror is graded with the window's exposure,
                // as the eye that looks into it is adapted to the street)
                if lead_view {
                    post_pass(
                        &mut encoder,
                        &self.meter_view,
                        None,
                        &self.post.meter,
                        &h.meter_bg,
                    );
                    let front = self.adapt_front;
                    post_pass(
                        &mut encoder,
                        &self.adapt_views[1 - front],
                        None,
                        &self.post.adapt,
                        &self.adapt_bg[front],
                    );
                    self.adapt_front = 1 - front;
                }
                // (a device lost since this frame began took the meter's buffer as well)
                let lost = self.device_lost().is_some();
                if let Some(log) = self.exposure_log.as_mut().filter(|_| with_overlays && !lost) {
                    let pre = self.exposure.unwrap_or(0.0) / std::f32::consts::LN_2;
                    log.sample(&mut encoder, &self.adapt_views[self.adapt_front], pre, m);
                }
                // (timed on its last pass: the glow chain with the metering, see GpuTimers)
                for i in (0..levels).rev() {
                    let timer = if i == 0 {
                        pass_timer(tset.as_ref(), &mut timed, "glow+meter")
                    } else {
                        None
                    };
                    post_pass(&mut encoder, &h.up[i], timer, &self.post.up, &h.up_bg[i]);
                }
                let final_view = if fxaa { &h.ldr } else { scene_view };
                let tonemap_bg = &puddles.map(|p| &p.tonemap_bg).unwrap_or(&h.tonemap_bg)[self.adapt_front];
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tone map"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: final_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: pass_timer(tset.as_ref(), &mut timed, "tone map"),
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(if fxaa {
                    &self.post.tonemap_encoded
                } else {
                    &self.post.tonemap
                });
                pass.set_bind_group(0, tonemap_bg, &[]);
                pass.draw(0..3, 0..1);
                if fxaa {
                    drop(pass);
                    pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("fxaa"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: scene_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: pass_timer(tset.as_ref(), &mut timed, "fxaa"),
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(&self.post.fxaa);
                    pass.set_bind_group(0, &h.fxaa_bg, &[]);
                    pass.draw(0..3, 0..1);
                }
                if !overlays.is_empty() && !scaled {
                    pass.set_pipeline(&self.overlay_pipeline_1x);
                    for (k, _) in overlays.iter().enumerate() {
                        if let Some((_, _, bg, _)) = scene.overlay_res.get(k) {
                            pass.set_bind_group(0, bg, &[]);
                            pass.draw(0..6, 0..1);
                        }
                    }
                }
            }
        }
        if let Some((_, bg)) = &scene_target {
            // --- the smaller picture scaled up to the window, the HUD on top at full size;
            // the smaller the picture, the more it is sharpened
            let sharpen = (1.0 - width as f32 / full_w as f32) * 2.0;
            self.queue.write_buffer(
                &self.upscale_buf,
                0,
                bytemuck::cast_slice(&[width as f32, height as f32, sharpen.clamp(0.0, 0.8), if vanilla_fxaa { 1.0 } else { 0.0 }]),
            );
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("upscale"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: pass_timer(tset.as_ref(), &mut timed, "upscale"),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.upscale_pipeline);
            pass.set_bind_group(0, bg, &[]);
            pass.draw(0..3, 0..1);
            if !overlays.is_empty() {
                pass.set_pipeline(&self.overlay_pipeline_1x);
                for (k, _) in overlays.iter().enumerate() {
                    if let Some((_, _, bg, _)) = scene.overlay_res.get(k) {
                        pass.set_bind_group(0, bg, &[]);
                        pass.draw(0..6, 0..1);
                    }
                }
            }
        }
        stage(self, "encode", "mirror.encode");
        // Turning the recorded passes into Metal commands is the costliest CPU step of a
        // frame (wgpu checks every draw): the shadow maps and the prepass are finished on
        // helper threads while this one finishes the picture.
        let big =
            shadow_batches.iter().map(|b| b.len()).sum::<usize>() + prepass_batches.len() > 64 || !main_parts.is_empty();
        let profiling = self.profiling;
        let finish = |encoder: wgpu::CommandEncoder| {
            let start = profiling.then(std::time::Instant::now);
            let commands = encoder.finish();
            (commands, start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0))
        };
        let (shadow_commands, prepass_commands, part_commands, commands, finish_times) = if big && self.encoding_pool.is_some() {
            self.encoding_pool.as_ref().unwrap().in_place_scope_fifo(|scope| {
                let (shadow_tx, shadow_rx) = std::sync::mpsc::sync_channel(1);
                let (prepass_tx, prepass_rx) = std::sync::mpsc::sync_channel(1);
                let parts = main_parts.len();
                let (part_tx, part_rx) = std::sync::mpsc::sync_channel(parts.max(1));
                let finish = &finish;
                scope.spawn_fifo(move |_| { let _ = shadow_tx.send(finish(shadow_encoder)); });
                scope.spawn_fifo(move |_| { let _ = prepass_tx.send(finish(prepass_encoder)); });
                for (k, e) in main_parts.into_iter().enumerate() {
                    let tx = part_tx.clone();
                    scope.spawn_fifo(move |_| { let _ = tx.send((k, finish(e).0)); });
                }
                let (commands, main_secs) = finish(encoder);
                let wait = std::time::Instant::now();
                let (shadow_commands, shadow_secs) = shadow_rx.recv().expect("command encoding worker");
                let shadow_wait = wait.elapsed().as_secs_f64();
                let wait = std::time::Instant::now();
                let (prepass_commands, prepass_secs) = prepass_rx.recv().expect("command encoding worker");
                let mut part_commands: Vec<Option<wgpu::CommandBuffer>> = (0..parts).map(|_| None).collect();
                for _ in 0..parts {
                    let (k, c) = part_rx.recv().expect("command encoding worker");
                    part_commands[k] = Some(c);
                }
                (
                    shadow_commands,
                    prepass_commands,
                    part_commands.into_iter().map(|c| c.expect("main pass part")).collect::<Vec<_>>(),
                    commands,
                    [shadow_secs, prepass_secs, main_secs, shadow_wait, wait.elapsed().as_secs_f64()],
                )
            })
        } else if big {
            std::thread::scope(|scope| {
                let finish = &finish;
                let shadow = scope.spawn(move || finish(shadow_encoder));
                let prepass = scope.spawn(move || finish(prepass_encoder));
                let parts: Vec<_> = main_parts.into_iter().map(|e| scope.spawn(move || finish(e).0)).collect();
                let (commands, main_secs) = finish(encoder);
                let wait = std::time::Instant::now();
                let (shadow_commands, shadow_secs) = shadow.join().expect("command encoding thread");
                let shadow_wait = wait.elapsed().as_secs_f64();
                let wait = std::time::Instant::now();
                let (prepass_commands, prepass_secs) = prepass.join().expect("command encoding thread");
                (
                    shadow_commands,
                    prepass_commands,
                    parts.into_iter().map(|h| h.join().expect("command encoding thread")).collect::<Vec<_>>(),
                    commands,
                    [shadow_secs, prepass_secs, main_secs, shadow_wait, wait.elapsed().as_secs_f64()],
                )
            })
        } else {
            let (shadow, shadow_secs) = finish(shadow_encoder);
            let (prepass, prepass_secs) = finish(prepass_encoder);
            let parts: Vec<_> = main_parts.into_iter().map(|e| finish(e).0).collect();
            let (main, main_secs) = finish(encoder);
            (shadow, prepass, parts, main, [shadow_secs, prepass_secs, main_secs, 0.0, 0.0])
        };
        if self.profiling {
            // These overlap across helper threads; do not add them to the stage totals.
            let keys = if with_overlays {
                ["finish.shadow", "finish.prepass", "finish.main", "finish.wait shadow", "finish.wait prepass"]
            } else {
                ["mirror.finish.shadow", "mirror.finish.prepass", "mirror.finish.main", "mirror.finish.wait shadow", "mirror.finish.wait prepass"]
            };
            for (key, secs) in keys.into_iter().zip(finish_times) {
                *self.stats.borrow_mut().entry(key).or_default() += secs;
            }
        }
        stage(self, "finish", "mirror.finish");
        self.queue
            .submit([shadow_commands, prepass_commands].into_iter().chain(part_commands).chain([commands]));
        stage(self, "submit", "mirror.submit");
        if let (Some(t), false) = (
            self.gpu_timers[with_overlays as usize].as_mut(),
            timed.is_empty(),
        ) {
            t.pending = timed;
            t.unresolved = true;
        }
    }

    /// Render off-screen and return RGBA8 pixels.
    pub fn render_to_image(
        &mut self,
        scene: &mut Scene,
        width: u32,
        height: u32,
        camera: &Camera,
        lighting: &Lighting,
    ) -> Result<Vec<u8>> {
        self.render_image(scene, width, height, camera, lighting, None)
    }

    /// Capture all three physical screen projections, including the shared HUD.
    pub fn render_triple_to_image(
        &mut self,
        scene: &mut Scene,
        width: u32,
        height: u32,
        camera: &Camera,
        lighting: &Lighting,
        rig: &TripleScreen,
    ) -> Result<Vec<u8>> {
        self.render_image(scene, width, height, camera, lighting, Some(rig))
    }

    fn render_image(
        &mut self,
        scene: &mut Scene,
        width: u32,
        height: u32,
        camera: &Camera,
        lighting: &Lighting,
        rig: Option<&TripleScreen>,
    ) -> Result<Vec<u8>> {
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        // a picture on its own: the enhanced exposure is where the light puts it at once
        self.instant_exposure = true;
        if let Some(rig) = rig {
            self.render_triple(scene, &view, width, height, camera, lighting, rig);
        } else {
            self.render(scene, &view, width, height, camera, lighting);
        }
        self.instant_exposure = false;
        let mut out = self.read_texture(&tex, wgpu::TextureAspect::All)?;
        // BGRA surfaces → swap
        if matches!(
            self.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        ) {
            for px in out.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        Ok(out)
    }

    fn read_texture(&self, texture: &wgpu::Texture, aspect: wgpu::TextureAspect) -> Result<Vec<u8>> {
        let (width, height) = (texture.width(), texture.height());
        let row_bytes = width * texture.format().block_copy_size(Some(aspect)).context("unsupported readback format")?;
        let bpr = row_bytes.div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (bpr * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let idx = self.queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        wait_gpu(&self.device, Some(idx)).map_err(|e| anyhow!("poll: {e:?}"))?;
        rx.recv()
            .context("map")?
            .map_err(|e| anyhow!("map: {e:?}"))?;
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((row_bytes * height) as usize);
        for row in 0..height {
            let start = (row * bpr) as usize;
            out.extend_from_slice(&data[start..start + row_bytes as usize]);
        }
        drop(data);
        buf.unmap();
        Ok(out)
    }
}

/// A buffer holding `contents` - what `create_buffer_init` makes, but written through the
/// queue instead of mapped at its creation. When the device refuses the memory (an
/// integrated chip that shares a small heap: "Out of Memory"), the buffer is invalid, and
/// mapping it ended the game - "Error in Buffer::get_mapped_range: Validation Error"
/// (#107, #109) - where writing to it is an error that is logged and the game goes on from.
fn buffer_init(device: &wgpu::Device, queue: &wgpu::Queue, label: Option<&str>, contents: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
    let align = wgpu::COPY_BUFFER_ALIGNMENT as usize;
    let size = contents.len().next_multiple_of(align).max(align);
    let buf = device.create_buffer(&wgpu::BufferDescriptor { label, size: size as u64, usage: usage | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
    if !contents.is_empty() {
        if contents.len() == size {
            queue.write_buffer(&buf, 0, contents);
        } else {
            let mut padded = contents.to_vec();
            padded.resize(size, 0);
            queue.write_buffer(&buf, 0, &padded);
        }
    }
    buf
}

/// The GPU buffers of a mesh.
fn make_mesh(device: &wgpu::Device, queue: &wgpu::Queue, data: &MeshData) -> GpuMesh {
    let verts: Vec<Vertex> = data
        .positions
        .iter()
        .zip(&data.normals)
        .zip(&data.uvs)
        .map(|((p, n), uv)| Vertex {
            pos: p.to_array(),
            normal: n.to_array(),
            uv: uv.to_array(),
        })
        .collect();
    let vertex_buf = buffer_init(device, queue, None, bytemuck::cast_slice(&verts), wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST);
    let index_buf = buffer_init(device, queue, None, bytemuck::cast_slice(&data.indices), wgpu::BufferUsages::INDEX);
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for p in &data.positions {
        lo = lo.min(*p);
        hi = hi.max(*p);
    }
    if data.positions.is_empty() {
        lo = Vec3::ZERO;
        hi = Vec3::ZERO;
    }
    let center = (lo + hi) * 0.5;
    GpuMesh {
        vertex_buf,
        index_buf,
        ranges: data.ranges.clone(),
        bounds_center: center,
        bounds_radius: (hi - center).length(),
        one_sided: data.one_sided,
        source: None,
    }
}

/// A mesh on the GPU, made on a worker thread; [`Renderer::add_prepared_mesh`] puts it into
/// a scene.
pub struct PreparedMesh(GpuMesh);

/// Make a mesh's GPU buffers on any thread (the device takes calls from all of them).
pub fn prepare_mesh(device: &wgpu::Device, queue: &wgpu::Queue, data: &MeshData) -> PreparedMesh {
    let _turn = gl_worker_turn();
    PreparedMesh(make_mesh(device, queue, data))
}

/// A texture on the GPU, made on a worker thread; [`Renderer::add_prepared_texture`] puts it
/// into a scene.
pub struct PreparedTexture(GpuTexture);

impl PreparedTexture {
    pub fn bytes(&self) -> u64 {
        self.0.bytes
    }
}

/// Make a texture that carries its levels (blocks, or RGBA that wants no GPU-made chain) on
/// any thread; None for anything else (an RGBA picture whose chain the GPU makes, a block
/// format the device does not take).
pub fn prepare_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    data: &omsi_texture::TextureData,
) -> Option<PreparedTexture> {
    if let Some(small) = fit_texture(data, device.limits().max_texture_dimension_2d) {
        return prepare_texture(device, queue, &small);
    }
    let _turn = gl_worker_turn();
    use omsi_texture::PixelFormat;
    let format = match data.format {
        PixelFormat::Rgba8 => wgpu::TextureFormat::Rgba8UnormSrgb,
        PixelFormat::Bc1 => wgpu::TextureFormat::Bc1RgbaUnormSrgb,
        PixelFormat::Bc2 => wgpu::TextureFormat::Bc2RgbaUnormSrgb,
        PixelFormat::Bc3 => wgpu::TextureFormat::Bc3RgbaUnormSrgb,
    };
    let (w, h) = (data.width.max(1), data.height.max(1));
    if data.levels.is_empty()
        || (data.format == PixelFormat::Rgba8
            && data.levels.len() == 1
            && data.gpu_mips
            && w > 1
            && h > 1)
    {
        return None;
    }
    if data.format.is_compressed()
        && (!device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
            || w % 4 != 0
            || h % 4 != 0)
    {
        return None;
    }
    let levels = data.levels.len() as u32;
    let size = wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size,
        mip_level_count: levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let (bw, bh) = format.block_dimensions();
    let block = format.block_copy_size(None).unwrap_or(4);
    for (l, bytes) in data.levels.iter().enumerate() {
        let (lw, lh) = ((w >> l).max(1), (h >> l).max(1));
        // compressed levels are written whole blocks at a time, also below 4x4
        let phys = wgpu::Extent3d {
            width: lw.div_ceil(bw) * bw,
            height: lh.div_ceil(bh) * bh,
            depth_or_array_layers: 1,
        };
        let need = ((phys.width / bw) * (phys.height / bh) * block) as usize;
        if bytes.len() < need {
            log::warn!(
                "texture level {l} of {w}x{h} {:?} has {} bytes, not {need}",
                data.format,
                bytes.len()
            );
            break;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: l as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &bytes[..need],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((phys.width / bw) * block),
                rows_per_image: Some(phys.height / bh),
            },
            phys,
        );
    }
    Some(PreparedTexture(GpuTexture::new(
        texture,
        (w, h),
        texture_bytes(format, w, h, levels),
    )))
}

fn upload_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    img: &omsi_texture::Image,
    mipmaps: bool,
) -> GpuTexture {
    let mip_count = if mipmaps {
        (32 - img.width.max(img.height).leading_zeros()).max(1)
    } else {
        1
    };
    let size = wgpu::Extent3d {
        width: img.width,
        height: img.height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size,
        mip_level_count: mip_count,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // CPU box-filter mip chain
    let mut level: Vec<u8> = img.rgba.clone();
    let (mut w, mut h) = (img.width, img.height);
    for mip in 0..mip_count {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: mip,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &level,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        if mip + 1 == mip_count {
            break;
        }
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let mut next = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            for x in 0..nw {
                for c in 0..4 {
                    let mut sum = 0u32;
                    let mut n = 0;
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let sx = (x * 2 + dx).min(w - 1);
                            let sy = (y * 2 + dy).min(h - 1);
                            sum += level[((sy * w + sx) * 4 + c) as usize] as u32;
                            n += 1;
                        }
                    }
                    next[((y * nw + x) * 4 + c) as usize] = (sum / n) as u8;
                }
            }
        }
        level = next;
        w = nw;
        h = nh;
    }
    let view = texture.create_view(&Default::default());
    GpuTexture {
        texture,
        view,
        size: (img.width, img.height),
        bytes: texture_bytes(
            wgpu::TextureFormat::Rgba8UnormSrgb,
            img.width,
            img.height,
            mip_count,
        ),
        gen: next_gen(),
    }
}

/// Does this path draw the light? (See `Renderer::prepare_lights`.)
fn drawn_by(l: &PointLight, enhanced: bool) -> bool {
    l.radius > 0.0
        && l.intensity > 0.0
        && l.mode
            != if enhanced {
                LightMode::Vanilla
            } else {
                LightMode::Enhanced
            }
}

/// A light as the shaders read it, at `p` relative to the render origin.
fn gpu_light(l: &PointLight, p: Vec3) -> GpuPointLight {
    let spot = l.direction.length_squared() > 1e-6;
    let dir = if spot {
        l.direction.normalize().extend(l.cone[1]).to_array()
    } else if l.mode == LightMode::Vanilla {
        // (a vehicle's headlight stand-in: lights a light-mapped road too, see
        // shader.wgsl point_lights)
        [1.0, 0.0, 0.0, -2.0]
    } else {
        [0.0, 0.0, 0.0, -2.0]
    };
    let vanilla_radius = if l.mode == LightMode::Enhanced {
        0.0
    } else {
        l.radius
    };
    GpuPointLight {
        pos: [p.x, p.y, p.z, vanilla_radius],
        color: [l.color[0], l.color[1], l.color[2], l.intensity],
        dir,
        extra: [l.cone[0], l.core, l.beam, l.radius],
    }
}

/// `OMSI_DEBUG_ENHANCED=n`: the enhanced main pass shows one of its terms alone (1 sun
/// shadow, 2 AO, 3 normal, 4 air transmittance, 5 ambient light, 6 reflection, 7 albedo,
/// 8 direct sun, 9 in-scattered air, 11 distance/depth, 12 alpha mode/terrain/surface,
/// 13 cab/AO/specular occlusion, 14 direct + ambient, 15 lamps and headlights, 16 what
/// glows by itself, 17 roughness/F0/metalness, 10 alpha/glass/envmap); `OMSI_ENV_PHOTO=0`
/// leaves the `[matl_envmap]` photo's structure out of the reflections. A blended surface
/// shows its values as if it were opaque: an invisible layer round the player's bus hides
/// the bus in these views.
fn debug_view() -> f32 {
    static VIEW: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *VIEW.get_or_init(|| {
        omsi_cfg::env::var("OMSI_DEBUG_ENHANCED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    })
}

/// The metering: the share of the metered difference that is corrected, its target (log2 of
/// the picture's mean luminance), how far it may darken and brighten (EV), the exposure
/// bias (EV) and the night vision strength. `OMSI_METER=gain,target,dark,bright,bias,night`
/// overrides them for tuning.
fn meter_tuning() -> [f32; 6] {
    static METER: std::sync::OnceLock<[f32; 6]> = std::sync::OnceLock::new();
    *METER.get_or_init(|| {
        let mut m = [
            METER_GAIN,
            METER_TARGET,
            METER_DARKEN,
            METER_BRIGHTEN,
            0.0,
            NIGHT_VISION,
        ];
        if let Ok(v) = omsi_cfg::env::var("OMSI_METER") {
            for (k, x) in v.split(',').take(6).enumerate() {
                if let Ok(x) = x.trim().parse() {
                    m[k] = x;
                }
            }
        }
        m
    })
}

/// Has the sky moved on far enough from `a` to be computed again? The sun by a tenth of a
/// degree (half a minute of the day), the weather by a per cent.
fn sky_input_differs(a: &atmosphere::SkyInput, b: &atmosphere::SkyInput) -> bool {
    let near = |x: f32, y: f32, tol: f32| (x - y).abs() <= tol;
    a.sun_dir.dot(b.sun_dir) < 0.999_998
        || !near(a.sun_visibility, b.sun_visibility, 0.01)
        || !near(a.overcast, b.overcast, 0.01)
        || !near(a.haze, b.haze, 0.02)
        || !near(a.rain, b.rain, 0.01)
        || !near(a.ground_albedo, b.ground_albedo, 0.01)
        || a.tint
            .iter()
            .zip(&b.tint)
            .any(|(x, y)| (*x - *y).abs().max_element() > 0.01)
}

/// The scene shader: the vanilla path and the enhanced fragment shader in one module.
///
/// On OpenGL a texture has one sampler (GLSL's combined sampler2D), so there the tile
/// masks are read through `s_diffuse` at a UV clamped half a texel inside the tile, which
/// is what `s_tile`'s clamp to edge gives; reading `t_trans`/`t_night` through both
/// samplers fails the whole module ("Conflicting samplers").
fn scene_shader_source(gl: bool) -> String {
    arrays_as_textures(&scene_shader_text(gl), array_path())
}

/// The scene module with its arrays read as `path` has them (see `ArrayPath`): each
/// storage array the device cannot read becomes a texture and its `name[i]` a function
/// that loads texel `i`; without storage at all the point lights are none.
fn arrays_as_textures(src: &str, path: ArrayPath) -> String {
    if path == ArrayPath::Storage {
        return src.to_string();
    }
    let w = ARRAY_TEX_WIDTH;
    let mut out = src.to_string();
    let mut swap = |decl: &str, with: String, name: &str, call: &str| {
        assert!(out.contains(decl), "scene shader: {decl} not found");
        out = out.replace(decl, &with);
        out = indexing_as_calls(&out, name, call);
    };
    let load = |name: &str, ty: &str, pick: &str| {
        format!(
            "var {name}_tex: texture_2d<{ty}>;\nfn {name}_at(i: u32) -> {} {{ return textureLoad({name}_tex, vec2<u32>(i % {w}u, i / {w}u), 0){pick}; }}",
            if pick.is_empty() { format!("vec4<{ty}>") } else { ty.to_string() }
        )
    };
    swap("var<storage, read> models: array<vec4<f32>>;", load("models", "f32", ""), "models", "models_at");
    swap("var<storage, read> inst_params: array<vec4<f32>>;", load("inst_params", "f32", ""), "inst_params", "inst_params_at");
    swap("var<storage, read> draw_list: array<u32>;", load("draw_list", "u32", ".x"), "draw_list", "draw_list_at");
    if path == ArrayPath::NoStorage {
        swap(
            "@group(0) @binding(3) var<storage, read> lights: array<PointLight>;",
            "fn lights_at(i: u32) -> PointLight { return PointLight(vec4<f32>(0.0), vec4<f32>(0.0), vec4<f32>(0.0), vec4<f32>(0.0)); }".to_string(),
            "lights",
            "lights_at",
        );
        swap("@group(0) @binding(4) var<storage, read> grid: array<u32>;", "fn grid_at(i: u32) -> u32 { return 0xffffffffu; }".to_string(), "grid", "grid_at");
    }
    out
}

/// `name[expr]` (the whole word `name`) turned into `call(expr)`.
fn indexing_as_calls(src: &str, name: &str, call: &str) -> String {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let pat = format!("{name}[");
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(at) = rest.find(&pat) {
        let before = rest[..at].chars().next_back();
        out.push_str(&rest[..at]);
        if before.is_some_and(word) {
            out.push_str(&pat);
            rest = &rest[at + pat.len()..];
            continue;
        }
        // the matching bracket
        let inner = &rest[at + pat.len()..];
        let mut depth = 1;
        let end = inner
            .char_indices()
            .find(|&(_, c)| {
                depth += match c {
                    '[' => 1,
                    ']' => -1,
                    _ => 0,
                };
                depth == 0
            })
            .map(|(i, _)| i)
            .expect("unbalanced brackets in the scene shader");
        out.push_str(call);
        out.push('(');
        out.push_str(&indexing_as_calls(&inner[..end], name, call));
        out.push(')');
        rest = &inner[end + 1..];
    }
    out.push_str(rest);
    out
}

fn scene_shader_text(gl: bool) -> String {
    let src = [
        include_str!("colour.wgsl"),
        include_str!("shader.wgsl"),
        include_str!("enhanced_common.wgsl"),
        include_str!("puddle_common.wgsl"),
        include_str!("enhanced.wgsl"),
    ]
    .join("\n");
    if !gl {
        return src;
    }
    let clamped = |t: &str| {
        format!(
            "textureSample({t}, s_diffuse, clamp(uv, 0.5 / vec2<f32>(textureDimensions({t})), \
             vec2<f32>(1.0) - 0.5 / vec2<f32>(textureDimensions({t}))))"
        )
    };
    let out = src
        .replace("textureSample(t_trans, s_tile, uv)", &clamped("t_trans"))
        .replace("textureSample(t_night, s_tile, uv)", &clamped("t_night"));
    debug_assert!(!out.contains("s_tile, uv)"));
    out
}

/// The enhanced clouds' noise textures (clouds.rs), made once: the shape map (2-D RGBA8)
/// and the detail volume (3-D R8), both with their mip chains, and a repeating sampler.
fn cloud_noise_textures(device: &wgpu::Device, queue: &wgpu::Queue) -> (wgpu::TextureView, wgpu::TextureView, wgpu::Sampler) {
    let t0 = std::time::Instant::now();
    let (shape, detail) = std::thread::scope(|s| {
        let a = s.spawn(clouds::shape_map);
        let b = s.spawn(clouds::detail_volume);
        (a.join().expect("cloud shape"), b.join().expect("cloud detail"))
    });
    let make = |label: &str, size: u32, dim: wgpu::TextureDimension, format: wgpu::TextureFormat, bpp: u32, levels: &[Vec<u8>]| {
        let depth = if dim == wgpu::TextureDimension::D3 { size } else { 1 };
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: depth },
            mip_level_count: levels.len() as u32,
            sample_count: 1,
            dimension: dim,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (m, data) in levels.iter().enumerate() {
            let e = (size >> m).max(1);
            let d = if dim == wgpu::TextureDimension::D3 { e } else { 1 };
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: m as u32, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(e * bpp), rows_per_image: Some(e) },
                wgpu::Extent3d { width: e, height: e, depth_or_array_layers: d },
            );
        }
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    };
    let shape_view = make("cloud shape", clouds::SHAPE_SIZE, wgpu::TextureDimension::D2, wgpu::TextureFormat::Rgba8Unorm, 4, &shape);
    let detail_view = make("cloud detail", clouds::DETAIL_SIZE, wgpu::TextureDimension::D3, wgpu::TextureFormat::R8Unorm, 1, &detail);
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("cloud noise"),
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        address_mode_w: wgpu::AddressMode::Repeat,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..Default::default()
    });
    log::info!("cloud noise made in {:.2} s", t0.elapsed().as_secs_f32());
    (shape_view, detail_view, sampler)
}

/// The sky dome (both paths) and the enhanced reflection probe.
fn sky_shader_source() -> String {
    [
        include_str!("colour.wgsl"),
        include_str!("sky.wgsl"),
        include_str!("enhanced_common.wgsl"),
        include_str!("sky_enhanced.wgsl"),
    ]
    .join("\n")
}

/// The light coronas (both paths).
fn corona_shader_source() -> String {
    [
        include_str!("corona.wgsl"),
        include_str!("enhanced_common.wgsl"),
    ]
    .join("\n")
}

/// One single-sampled full-screen pass.
fn post_pass(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    timer: Option<wgpu::RenderPassTimestampWrites<'_>>,
    pipeline: &wgpu::RenderPipeline,
    bg: &wgpu::BindGroup,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("post"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: timer,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bg, &[]);
    pass.draw(0..3, 0..1);
}

/// `OMSI_DEBUG_EXPOSURE=1`: the enhanced path's exposure as it adapts - the light model's
/// pre-exposure, the metered picture and the correction the tone mapping applies, logged
/// about four times a second (the metered value lives on the GPU and is read back).
struct ExposureLog {
    buf: wgpu::Buffer,
    ready: Arc<std::sync::atomic::AtomicBool>,
    waiting: bool,
    frame: u64,
    started: std::time::Instant,
    /// the pre-exposure (log2) and the metering settings of the frame being read back
    pending: (f32, [f32; 6]),
    /// The tone mapping's metering correction last read back (EV). Self-lit surfaces (a
    /// display's text, a script texture) are drawn this much brighter or darker in advance,
    /// so that they come out at their own brightness whatever the metering does to the
    /// rest of the picture: a destination display was darkened with a sunlit street and
    /// hardly readable by day.
    ev: f32,
    /// `OMSI_DEBUG_EXPOSURE`: log what is read.
    log: bool,
}

impl ExposureLog {
    fn new(device: &wgpu::Device) -> Option<ExposureLog> {
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("exposure readback"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Some(ExposureLog {
            buf,
            ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            waiting: false,
            frame: 0,
            started: std::time::Instant::now(),
            pending: (0.0, [0.0; 6]),
            ev: 0.0,
            log: omsi_cfg::env::var_os("OMSI_DEBUG_EXPOSURE").is_some(),
        })
    }

    fn sample(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        adapted: &wgpu::TextureView,
        pre_log2: f32,
        meter: [f32; 6],
    ) {
        self.frame += 1;
        if self.waiting && self.ready.swap(false, std::sync::atomic::Ordering::Relaxed) {
            {
                let view = self.buf.slice(0..8).get_mapped_range();
                let bits = u16::from_le_bytes([view[0], view[1]]);
                let metered = half_to_f32(bits);
                let (pre, m) = self.pending;
                let ev = ((m[1] - metered) * m[0]).clamp(-m[2], m[3]) + m[4];
                if ev.is_finite() {
                    self.ev = ev;
                }
                if self.log { log::info!("exposure t={:.2}s: light model {:+.2} EV, metered picture log2 {:+.2}, correction {:+.2} EV, total {:+.2} EV", self.started.elapsed().as_secs_f32(), pre, metered, ev, pre + ev); }
            }
            self.buf.unmap();
            self.waiting = false;
        }
        if self.waiting || self.frame % 8 != 0 {
            return;
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: adapted.texture(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        self.pending = (pre_log2, meter);
        let ready = self.ready.clone();
        // mapped once the frame's commands are submitted (see `render_inner`)
        self.waiting = true;
        encoder.map_buffer_on_submit(&self.buf, wgpu::MapMode::Read, .., move |r| {
            ready.store(r.is_ok(), std::sync::atomic::Ordering::Relaxed)
        });
    }
}

fn half_to_f32(b: u16) -> f32 {
    let sign = if b & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((b >> 10) & 0x1f) as i32;
    let m = (b & 0x3ff) as f32;
    match e {
        0 => sign * m / 1024.0 * 2f32.powi(-14),
        31 => sign * f32::INFINITY,
        _ => sign * (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

/// GPU time per render pass from timestamp queries (OMSI_GPU_TIMERS). One frame is timed
/// at a time: its readback has to arrive before the next one is measured. The passes
/// partition the frame: each counts from the end of the one the GPU finished before it, so
/// untimed passes are in the next timed one's figure and the figures add up to
/// "(all passes)".
struct GpuTimers {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    read: wgpu::Buffer,
    /// The passes of the frame being read back, in query order.
    pending: Vec<&'static str>,
    /// The frame's passes are timed, their stamps not yet resolved (see `collect_gpu_timers`).
    unresolved: bool,
    waiting: bool,
    ready: Arc<std::sync::atomic::AtomicBool>,
    /// pass → (seconds, frames)
    totals: std::collections::BTreeMap<&'static str, (f64, u32)>,
}

const GPU_TIMER_PASSES: u32 = 16;

impl GpuTimers {
    fn new(device: &wgpu::Device) -> Option<GpuTimers> {
        if omsi_cfg::env::var_os("OMSI_GPU_TIMERS").is_none()
            || !device.features().contains(wgpu::Features::TIMESTAMP_QUERY)
        {
            return None;
        }
        let set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("pass timers"),
            ty: wgpu::QueryType::Timestamp,
            count: GPU_TIMER_PASSES * 2,
        });
        let size = (GPU_TIMER_PASSES as u64 * 16).div_ceil(wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT)
            * wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT;
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pass timers"),
            size,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let read = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pass timers read"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Some(GpuTimers {
            set,
            resolve,
            read,
            pending: Vec::new(),
            unresolved: false,
            waiting: false,
            ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            totals: Default::default(),
        })
    }
}

/// The timestamp writes of the next timed pass (none when this frame is not timed).
fn pass_timer<'a>(
    set: Option<&'a wgpu::QuerySet>,
    timed: &mut Vec<&'static str>,
    label: &'static str,
) -> Option<wgpu::RenderPassTimestampWrites<'a>> {
    let set = set?;
    if timed.len() as u32 >= GPU_TIMER_PASSES {
        return None;
    }
    let i = timed.len() as u32 * 2;
    timed.push(label);
    Some(wgpu::RenderPassTimestampWrites {
        query_set: set,
        beginning_of_pass_write_index: Some(i),
        end_of_pass_write_index: Some(i + 1),
    })
}

fn origin_key(origin: DVec3) -> [u64; 3] {
    // DVec3 equality treats -0 and +0 alike. Keep that property in the hash key.
    origin.to_array().map(|v| if v == 0.0 { 0 } else { v.to_bits() })
}

/// For each distinct origin, the object's nearest blended mesh distance, rather than
/// the single point all its meshes share (a long vehicle's origin can be far from its
/// window nearest the camera). Full coordinates and the existing float equality apply.
fn nearest_by_origin(items: impl IntoIterator<Item = (DVec3, f32)>) -> HashMap<[u64; 3], f32> {
    let mut out: HashMap<[u64; 3], f32> = HashMap::new();
    for (origin, d) in items {
        // NaN origins never compared equal in the old lookup either.
        if origin.is_nan() {
            continue;
        }
        out.entry(origin_key(origin)).and_modify(|best| *best = best.min(d)).or_insert(d);
    }
    out
}

/// One draw of a mesh range with a material for one per-draw entry, before batching.
/// `pipe` picks the pipeline within a pass (and orders the batches).
#[derive(Clone, Copy)]
struct DrawItem {
    pipe: u8,
    mesh: u32,
    range: u32,
    material: u32,
    entry: u32,
}

/// Draws of the same mesh range with the same material and pipeline, made as one instanced
/// draw: `instances` indexes the frame's draw list, which holds each instance's per-draw
/// entry (the vertex shader looks it up). Thousands of single draws were the biggest CPU
/// cost of a frame - wgpu validates and records every one - and trees, lamps, fences,
/// people and the AI cars' shared meshes collapse into a few hundred batches.
#[derive(Clone)]
struct Batch {
    pipe: u8,
    mesh: u32,
    first: u32,
    count: u32,
    material: u32,
    instances: std::ops::Range<u32>,
}

/// Turn draw items into batches, appending their entries to `list`. `sort`: the order does
/// not matter (depth-tested opaque and alpha-tested draws), so equal draws are gathered;
/// otherwise only neighbours are merged (the blended pass keeps its far-to-near order).
fn batch_items(
    scene: &Scene,
    items: &mut [DrawItem],
    sort: bool,
    list: &mut Vec<u32>,
    out: &mut Vec<Batch>,
) {
    if sort {
        items.sort_unstable_by_key(|d| (d.pipe, d.material, d.mesh, d.range));
    }
    let mut k = 0;
    while k < items.len() {
        let d = items[k];
        let start = list.len() as u32;
        while k < items.len()
            && (
                items[k].pipe,
                items[k].mesh,
                items[k].range,
                items[k].material,
            ) == (d.pipe, d.mesh, d.range, d.material)
        {
            list.push(items[k].entry);
            k += 1;
        }
        let (first, count, _) = scene.meshes[d.mesh as usize].ranges[d.range as usize];
        out.push(Batch {
            pipe: d.pipe,
            mesh: d.mesh,
            first,
            count,
            material: d.material,
            instances: start..list.len() as u32,
        });
    }
}

/// The material a depth-only draw (shadow map, depth prepass) is batched with: an opaque
/// surface writes its depth whatever its texture, so all of them share the first material
/// and batch across materials; an alpha-tested one needs its own texture for the cut-out.
fn depth_only_material(kind: u8, material: MaterialId) -> u32 {
    if kind == 0 {
        0
    } else {
        material as u32
    }
}

/// Record batches into a pass or a bundle, setting pipeline, buffers and material only
/// when they change.
fn encode_batches<'a, E: wgpu::util::RenderEncoder<'a>>(
    pass: &mut E,
    scene: &'a Scene,
    batches: &[Batch],
    pipeline: impl Fn(u8) -> &'a wgpu::RenderPipeline,
) {
    encode_batches_filtered(pass, scene, batches, |_| true, pipeline);
}

/// Record the batches accepted by `include`, setting pipeline, buffers and material only
/// when they change.
fn encode_batches_filtered<'a, E: wgpu::util::RenderEncoder<'a>>(
    pass: &mut E,
    scene: &'a Scene,
    batches: &[Batch],
    include: impl Fn(&Batch) -> bool,
    pipeline: impl Fn(u8) -> &'a wgpu::RenderPipeline,
) {
    let (mut pipe, mut mesh, mut material) = (u8::MAX, u32::MAX, u32::MAX);
    for b in batches {
        if !include(b) {
            continue;
        }
        if b.pipe != pipe {
            pass.set_pipeline(pipeline(b.pipe));
            pipe = b.pipe;
        }
        if b.mesh != mesh {
            let m = &scene.meshes[b.mesh as usize];
            pass.set_vertex_buffer(0, m.vertex_buf.slice(..));
            pass.set_index_buffer(m.index_buf.slice(..), wgpu::IndexFormat::Uint32);
            mesh = b.mesh;
        }
        if b.material != material {
            pass.set_bind_group(
                1,
                Some(&scene.materials[b.material as usize].bind_group),
                &[],
            );
            material = b.material;
        }
        pass.draw_indexed(b.first..b.first + b.count, 0, b.instances.clone());
    }
}

/// Main pass pipeline kinds (`pipe_code`): opaque, alpha-tested, blended, blended
/// without depth write, and the depth-only coverage of composed ground surfaces.
const PIPE_OPAQUE: u8 = 0;
const PIPE_ALPHA_TEST: u8 = 1;
const PIPE_BLEND: u8 = 2;
const PIPE_BLEND_NO_WRITE: u8 = 3;
const PIPE_SURFACE_DEPTH: u8 = 4;
const PIPE_KINDS: u8 = 5;

fn effective_render_phase(instance: &Instance) -> RenderPhase {
    if instance.presurface {
        RenderPhase::PreSurface
    } else {
        instance.render_phase
    }
}

fn world_surface_phase(phase: RenderPhase) -> bool {
    matches!(phase, RenderPhase::PreSurface | RenderPhase::Surface | RenderPhase::Spline | RenderPhase::OnSurface)
}

/// Only diffuse-alpha ground coverage is committed after the surface phases. Painted
/// terrain masks, ordinary glass, and explicit no-Z-check materials keep their semantics.
fn surface_depth_coverage(phase: RenderPhase, alpha: AlphaMode, transmap: bool, no_z_check: bool) -> bool {
    world_surface_phase(phase) && alpha == AlphaMode::Blend && !transmap && !no_z_check
}
/// Which depth-prepass variant a material contributes to. A blended material normally has
/// no prepass because its fragments are see-through; a transmap is the useful exception:
/// fully opaque texels are usually the vehicle body while lower-alpha texels are its windows.
/// Presurfaces also contribute fully transparent blended texels, which seal the ground.
/// Materials explicitly marked no-Z-write/no-Z-check remain excluded, just as they are from
/// the ordinary prepass.
fn depth_prepass_kind(kind: u8, material: &Material, presurface: bool) -> Option<u8> {
    if material.no_z_check {
        return None;
    }
    if kind < PIPE_BLEND {
        Some(kind)
    } else if presurface && !material.no_z_write {
        // Alpha blending preserves colour, but even fully transparent cover texels
        // occlude later ground. AO must see the same cover as the colour pass.
        Some(PIPE_OPAQUE)
    } else if kind == PIPE_BLEND && material.transmap.is_some() && !material.no_z_write {
        Some(2)
    } else {
        None
    }
}

fn instance_depth_bias(instance: &Instance, material: &Material) -> bool {
    instance.surface_bias || material.z_bias > 0 || (material.no_z_check && !instance.surface)
}

fn surface_instance_code(
    blob: bool,
    ground_layer: bool,
    decal: bool,
    surface: bool,
    surface_bias: bool,
) -> f32 {
    if blob {
        2.0
    } else if ground_layer {
        0.75
    } else if decal {
        if surface_bias { 1.25 } else { 0.9 }
    } else if surface {
        if surface_bias { 1.0 } else { 0.9 }
    } else {
        0.0
    }
}

/// OMSI's spline blend sort is horizontal in the x/z ground plane; Rust's vertical axis is z.
fn horizontal_sort_distance(origin: DVec3, render_origin: DVec3, camera_relative: Vec3) -> f32 {
    let p = (origin - render_origin).as_vec3() - camera_relative;
    glam::Vec2::new(p.x, p.y).length()
}

/// A main-pass draw's pipeline: the kind, whether back faces are culled, and whether the
/// surface depth bias applies (roads, painted ground, `[matl_Zbias]` decals). The opaque
/// and alpha-tested draws are batched in this order (kinds 0 and 1 first).
fn pipe_code(kind: u8, cull: bool, surface: bool) -> u8 {
    debug_assert!(kind < PIPE_KINDS);
    kind * 4 + (cull as u8) * 2 + surface as u8
}

/// The pipeline of a main-pass batch (`DrawItem::pipe`).
fn main_pipeline(pp: &PassPipelines, pipe: u8) -> &wgpu::RenderPipeline {
    &pp.pipelines[pipe as usize]
}

/// Rasterizer state of a scene pipeline. The content meshes keep Direct3D's winding: the
/// visible side of a triangle is the one it shows clockwise on the screen (the side its
/// normals face). Turning y and z round for the right-handed world (`mesh_from_o3d`)
/// mirrors the mesh, but the right-handed camera mirrors the picture back, so the visible
/// side still arrives clockwise here (`o3d_front_faces_arrive_clockwise`). D3D culls the
/// other side by default, and models rely on it: the SD202's front flap click spot is an
/// inside-out shell over the right headlight, invisible from outside, and windows are
/// modelled twice, once per side, with different glass.
fn one_sided_primitive(cull: bool) -> wgpu::PrimitiveState {
    wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        cull_mode: cull.then_some(wgpu::Face::Back),
        front_face: wgpu::FrontFace::Cw,
        ..Default::default()
    }
}

/// Are the back faces of this instance culled? Content meshes only, and not when the
/// instance is mirrored (a negative determinant turns the winding round).
/// `OMSI_NO_CULL=1` draws every mesh from both sides, for comparison.
fn culls_back_faces(scene: &Scene, inst: &Instance) -> bool {
    static NO_CULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    scene.meshes[inst.mesh].one_sided
        && !*NO_CULL.get_or_init(|| omsi_cfg::env::var_os("OMSI_NO_CULL").is_some())
        && glam::Mat3::from_mat4(inst.transform).determinant() > 0.0
}

struct DevicePoller {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DevicePoller {
    fn start(device: &wgpu::Device) -> Option<Self> {
        // Not on OpenGL: there every poll takes the one GL context, and whenever the thread
        // drawing held it for more than a second (a big shader linked while the world
        // loads, a slow chip's frame) this thread gave up with wgpu-hal's panic "Could not
        // lock adapter context" (#898, after #843). It is not needed there: every submit
        // of the frame runs the same upkeep (wgpu-core's `maintain` after `queue.submit`).
        if cfg!(target_arch = "wasm32") || gl_backend() || omsi_cfg::env::var_os("OMSI_NO_POLL_THREAD").is_some() {
            return None;
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (device, flag) = (device.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("omsi-gpu-poll".into())
            .spawn(move || {
                let pause = std::time::Duration::from_millis(1);
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = device.poll(wgpu::PollType::Poll);
                    std::thread::sleep(pause);
                }
            })
            .ok()?;
        Some(Self { stop, thread: Some(thread) })
    }
}

impl Drop for DevicePoller {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn in_scope<'s, R>(pool: Option<&rayon::ThreadPool>, op: impl FnOnce(&rayon::Scope<'s>) -> R) -> R {
    match pool {
        Some(p) => p.in_place_scope(op),
        None => rayon::in_place_scope(op),
    }
}

fn run_parts<T: Send>(pool: Option<&rayon::ThreadPool>, parts: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let Some(pool) = pool else {
        return std::thread::scope(|s| {
            let f = &f;
            let helpers: Vec<_> = (1..parts.max(1)).map(|k| s.spawn(move || f(k))).collect();
            let mut out = vec![f(0)];
            out.extend(helpers.into_iter().map(|h| h.join().expect("render part")));
            out
        });
    };
    let mut out: Vec<Option<T>> = (0..parts.max(1)).map(|_| None).collect();
    pool.in_place_scope(|s| {
        let f = &f;
        let (first, rest) = out.split_first_mut().expect("one part at least");
        for (k, slot) in rest.iter_mut().enumerate() {
            s.spawn(move |_| *slot = Some(f(k + 1)));
        }
        *first = Some(f(0));
    });
    out.into_iter().map(|o| o.expect("render part")).collect()
}

/// The main pass's batches as render bundles, recorded on up to four threads. Checking
/// every draw is the costliest CPU step of a frame; a bundle is checked when it is made,
/// and the pass that runs it only replays it. The bundles keep the batches' order (the
/// blended ones are far to near).
fn record_bundles(
    device: &wgpu::Device,
    pool: Option<&rayon::ThreadPool>,
    scene: &Scene,
    batches: &[Batch],
    pp: &PassPipelines,
    camera: &wgpu::BindGroup,
    format: wgpu::TextureFormat,
    samples: u32,
) -> Vec<wgpu::RenderBundle> {
    let color_formats = [Some(format), Some(MASK_FORMAT)];
    let record = |chunk: &[Batch]| -> wgpu::RenderBundle {
        let mut bundle =
            device.create_render_bundle_encoder(&wgpu::RenderBundleEncoderDescriptor {
                label: Some("main pass part"),
                color_formats: &color_formats[..if format == HDR_FORMAT { 2 } else { 1 }],
                depth_stencil: Some(wgpu::RenderBundleDepthStencil {
                    format: DEPTH_FORMAT,
                    depth_read_only: false,
                    stencil_read_only: true,
                }),
                sample_count: samples,
                multiview: None,
            });
        bundle.set_bind_group(0, camera, &[]);
        encode_batches(&mut bundle, scene, chunk, |pipe| main_pipeline(pp, pipe));
        bundle.finish(&wgpu::RenderBundleDescriptor {
            label: Some("main pass part"),
        })
    };
    // a bundle wgpu refuses (a buffer it names could not be made: the card ran out of
    // memory) panics in `finish`: that part of the picture is left out for the frame and
    // the game goes on - it ended the game on Windows right after an "Out of memory"
    let record = |chunk: &[Batch]| -> Option<wgpu::RenderBundle> {
        CATCHING.with(|c| c.set(true));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| record(chunk)));
        CATCHING.with(|c| c.set(false));
        match r {
            Ok(b) => Some(b),
            Err(_) => {
                static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    log::error!("a part of the picture could not be recorded (the graphics card is out of memory?); left out");
                }
                None
            }
        }
    };
    let max_parts = pool.map(|p| (p.current_num_threads() + 1).clamp(4, 8)).unwrap_or(4);
    let parts = (batches.len() / 250).clamp(1, max_parts);
    if parts == 1 {
        return record(batches).into_iter().collect();
    }
    let chunks: Vec<&[Batch]> = batches.chunks(batches.len().div_ceil(parts)).collect();
    run_parts(pool, chunks.len(), |k| record(chunks[k])).into_iter().flatten().collect()
}

/// The render scale for a picture of this size: the requested one (0.5..1), or with 0
/// (automatic) full size up to `AUTO_SCALE_PIXELS` and that many pixels above it.
/// A requested one keeps to the same budget on a Mac or a phone: the Low preset's fixed
/// 0.75 of a Retina window drew more pixels than the budget allows (3.24 of 2.8 million
/// on a 3200x1800 window), and the automatic scale would never have drawn that many.
fn scene_scale_for(requested: f32, width: u32, height: u32) -> f32 {
    let pixels = width as f32 * height as f32;
    if requested > 0.0 {
        let requested = requested.clamp(0.5, 1.0);
        if (cfg!(target_os = "macos") || cfg!(target_os = "android")) && pixels > AUTO_SCALE_PIXELS {
            return requested.min((AUTO_SCALE_PIXELS / pixels).sqrt().clamp(0.5, 1.0));
        }
        return requested;
    }
    if pixels <= AUTO_SCALE_PIXELS {
        1.0
    } else {
        (AUTO_SCALE_PIXELS / pixels).sqrt().clamp(0.5, 1.0)
    }
}

/// A GPU error with what the validation layer said, not just its kind.
impl Renderer {
    /// Why the graphics device was lost, if it was: nothing can be drawn any more.
    /// Lay the sphere maps of `[matl_envmap]` out by this heading instead of the view
    /// (None: by the view, as Omsi.exe does). A sphere map turns with the view it is drawn
    /// for: in the headset every turn of the player's head turned every reflection of the
    /// bus with it, the two eyes each their own way - the reflections swam about.
    pub fn set_env_heading(&self, heading: Option<f32>) {
        self.env_heading.set(heading);
    }

    /// Whether the card ran out of memory since the last call (the game then keeps fewer
    /// textures, before the driver gives up the device).
    pub fn take_out_of_memory(&self) -> bool {
        self.out_of_memory.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn device_lost(&self) -> Option<String> {
        self.device_lost.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn gpu_error_text(e: &wgpu::Error) -> String {
    match e {
        wgpu::Error::Validation { description, .. } | wgpu::Error::Internal { description, .. } => {
            description.trim().replace('\n', " ")
        }
        other => other.to_string(),
    }
}

/// Is `p` inside a vehicle's `[boundingbox]` (origin, heading in degrees, w l h cx cy cz)?
/// The same box the shader keeps the weather out of, without its margins.
fn point_in_vehicle_box(p: DVec3, (origin, heading, bb): &(DVec3, f64, [f32; 6])) -> bool {
    let d = (p - *origin).as_vec3();
    let (sh, ch) = (*heading as f32).to_radians().sin_cos();
    let x = d.x * ch - d.y * sh - bb[3];
    let y = d.x * sh + d.y * ch - bb[4];
    let z = d.z - bb[5];
    x.abs() < bb[0] * 0.5 && y.abs() < bb[1] * 0.5 && z.abs() < bb[2] * 0.5
}

/// Helper for windowed rendering.
pub struct SurfaceState<'w> {
    /// (let go of in `drop`, unless the device was lost - see there)
    pub surface: std::mem::ManuallyDrop<wgpu::Surface<'w>>,
    pub config: wgpu::SurfaceConfiguration,
    /// The renderer's `device_lost`.
    lost: Arc<std::sync::Mutex<Option<String>>>,
}

impl Drop for SurfaceState<'_> {
    fn drop(&mut self) {
        // After a lost device the frame it was drawing never finishes, and its swapchain
        // image with it: letting the surface go (or configuring it again) then tears the
        // swapchain down under that image - "Trying to destroy a SwapchainAcquireSemaphore
        // that is still in use by a SurfaceTexture" (Vulkan) ended the game instead of the
        // session ending in order. The window goes with the process anyway. So on a panic:
        // the frame being drawn is let go of while unwinding, and the same message then
        // took the place of the panic that ended the game in its report (#112: a sort).
        if !std::thread::panicking() && self.lost.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
            // SAFETY: dropped only here, once
            unsafe { std::mem::ManuallyDrop::drop(&mut self.surface) };
        }
    }
}

impl<'w> SurfaceState<'w> {
    pub fn new(
        instance: &wgpu::Instance,
        window: Arc<winit_window::Window>,
        renderer: &Renderer,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        Self::new_with(instance, window, renderer, width, height, true)
    }

    /// `vsync` false lets the frames go out as fast as they are drawn.
    pub fn new_with(
        instance: &wgpu::Instance,
        window: Arc<winit_window::Window>,
        renderer: &Renderer,
        width: u32,
        height: u32,
        vsync: bool,
    ) -> Result<Self> {
        let surface = instance.create_surface(window).context("create_surface")?;
        let mut config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: renderer.format(),
            width: width.max(1),
            height: height.max(1),
            present_mode: if vsync {
                wgpu::PresentMode::AutoVsync
            } else {
                wgpu::PresentMode::AutoNoVsync
            },
            // (two frames in flight keep the graphics chip busy while the next frame is
            // recorded: with one, the whole loop serialized behind the vsync'd drawable -
            // on Apple silicon a frame cost GPU + CPU instead of the larger of the two,
            // 50 ms where 24 of them were GPU. The price is one more frame of input
            // delay, 33 ms at 60 Hz.)
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        config.width = width.max(1);
        config.height = height.max(1);
        surface.configure(&renderer.device, &config);
        Ok(SurfaceState { surface: std::mem::ManuallyDrop::new(surface), config, lost: renderer.device_lost.clone() })
    }

    pub fn resize(&mut self, renderer: &Renderer, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        // (not after a lost device: see `drop`)
        if renderer.device_lost().is_some() {
            return;
        }
        self.surface.configure(&renderer.device, &self.config);
    }

    pub fn set_vsync(&mut self, renderer: &Renderer, enabled: bool) {
        let mode = if enabled { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync };
        if self.config.present_mode == mode || renderer.device_lost().is_some() {
            return;
        }
        self.config.present_mode = mode;
        // (two frames in flight: see `new_with`)
        self.config.desired_maximum_frame_latency = 2;
        self.surface.configure(&renderer.device, &self.config);
    }
}

/// Re-export so the app does not need to depend on winit's window type path.
pub mod winit_window {
    pub use winit::window::Window;
}

/// Letting go of scene resources again (tile streaming). Ids stay stable: a freed mesh,
/// texture or material keeps its slot with a tiny placeholder, and a resource added later
/// can take the slot over (`recycle_*`), so a long drive across a big map does not grow the
/// scene without bound. The contract of `recycle_*`: call it right after the `add_*` that
/// produced `new`, before anything refers to `new`.
impl Renderer {
    /// The shared placeholders of freed slots.
    fn freed(&self, scene: &mut Scene) -> &Freed {
        if self.freed.get().is_none() {
            // a plain material, built as any other and taken out of the scene again (without
            // using up an addressing request meant for the next real material)
            let address = self.address_next.replace(TexAddressing::Wrap);
            let plain = self.add_material(scene, None, AlphaMode::Opaque, [1.0; 4], false);
            self.address_next.set(address);
            let m = scene.materials.swap_remove(plain);
            let (bind_group, buf) = (m.bind_group, m.buf);
            let vertex_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("freed mesh"),
                size: std::mem::size_of::<Vertex>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let index_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("freed mesh"),
                size: 4,
                usage: wgpu::BufferUsages::INDEX,
                mapped_at_creation: false,
            });
            let _ = self.freed.set(Freed {
                vertex_buf,
                index_buf,
                bind_group,
                buf,
            });
        }
        self.freed.get().unwrap()
    }

    /// Release a mesh's buffers; drawing it afterwards draws nothing.
    pub fn free_mesh(&self, scene: &mut Scene, id: MeshId) {
        if id >= scene.meshes.len() {
            return;
        }
        let (vertex_buf, index_buf) = {
            let f = self.freed(scene);
            (f.vertex_buf.clone(), f.index_buf.clone())
        };
        let m = &mut scene.meshes[id];
        m.vertex_buf = vertex_buf;
        m.index_buf = index_buf;
        m.ranges.clear();
        m.bounds_center = Vec3::ZERO;
        m.bounds_radius = 0.0;
        Self::mesh_bounds_changed(scene, id);
    }

    /// Release a texture (a material still using it keeps it alive until it is freed too).
    pub fn free_texture(&self, scene: &mut Scene, id: TextureId) {
        scene.snow_textures.remove(&id);
        // (its PBR maps go with it: the slot is taken by another texture next)
        if let Some(m) = scene.pbr_maps.remove(&id) {
            for t in [m.normal, m.orm].into_iter().flatten() {
                self.free_texture(scene, t);
            }
        }
        if let Some(t) = scene.textures.get_mut(id) {
            *t = GpuTexture {
                texture: self.white_texture.texture.clone(),
                view: self.white_texture.view.clone(),
                size: (1, 1),
                bytes: 0,
                gen: next_gen(),
            };
        }
    }

    /// Release a material's bind group (and with it the textures it held).
    pub fn free_material(&self, scene: &mut Scene, id: MaterialId) {
        if id >= scene.materials.len() {
            return;
        }
        let (bind_group, buf) = {
            let f = self.freed(scene);
            (f.bind_group.clone(), f.buf.clone())
        };
        scene.materials[id] = Material {
            texture: None,
            alpha: AlphaMode::Opaque,
            color: [1.0; 4],
            unlit: false,
            no_z_write: false,
            writes_depth: false,
            no_z_check: false,
            z_bias: 0,
            nightmap: None,
            lightmap: None,
            envmap: None,
            env_mask: None,
            bump: None,
            emissive: [0.0; 3],
            transmap: None,
            address: TexAddressing::Wrap,
            uniform: <MaterialUniform as bytemuck::Zeroable>::zeroed(),
            buf,
            bind_group,
        };
    }

    /// Hide an instance for good (its tile was unloaded); `recycle_instance` reuses it.
    pub fn remove_instance(&self, scene: &mut Scene, instance: usize) {
        if instance >= scene.instances.len() {
            return;
        }
        self.set_params(scene, instance, &[], false, &[]);
        scene.instances[instance].lod = (0.0, f32::MAX);
        scene.instances[instance].interior_lamps = 0;
    }

    /// Cut the scene's arrays down to these lengths (their tails are freed slots nothing
    /// refers to any more); the per-draw buffers are rebuilt when instances went.
    pub fn truncate(
        &self,
        scene: &mut Scene,
        meshes: usize,
        textures: usize,
        materials: usize,
        instances: usize,
    ) {
        fn cut<T>(v: &mut Vec<T>, n: usize) {
            if n < v.len() {
                v.truncate(n);
                if v.capacity() > v.len() * 2 + 64 {
                    v.shrink_to_fit();
                }
            }
        }
        cut(&mut scene.meshes, meshes);
        cut(&mut scene.textures, textures);
        cut(&mut scene.materials, materials);
        // hidden (freed) instances may still name a cut mesh or material: slot 0 instead,
        // they are not drawn and whoever takes them over sets their own
        let (nm, nt) = (scene.meshes.len(), scene.materials.len());
        for inst in scene.instances.iter_mut() {
            if inst.mesh >= nm {
                debug_assert!(!inst.visible, "a drawn instance lost its mesh");
                inst.mesh = 0;
                inst.visible = false;
            }
            for m in inst.materials.iter_mut() {
                if *m >= nt {
                    *m = 0;
                }
            }
        }
        if instances < scene.instances.len() {
            cut(&mut scene.instances, instances);
            scene.changed.retain(|i| *i < instances);
            scene.changed_mark.truncate(instances);
            scene.uploaded_instances = scene.uploaded_instances.min(instances);
            scene.dirty = true;
        }
    }

    pub fn recycle_mesh(&self, scene: &mut Scene, new: MeshId, into: MeshId) -> MeshId {
        if new + 1 != scene.meshes.len() || into >= new {
            return new;
        }
        let m = scene.meshes.pop().unwrap();
        scene.meshes[into] = m;
        Self::mesh_bounds_changed(scene, into);
        into
    }

    pub fn recycle_texture(&self, scene: &mut Scene, new: TextureId, into: TextureId) -> TextureId {
        if new + 1 != scene.textures.len() || into >= new {
            return new;
        }
        let t = scene.textures.pop().unwrap();
        scene.textures[into] = t;
        into
    }

    pub fn recycle_material(
        &self,
        scene: &mut Scene,
        new: MaterialId,
        into: MaterialId,
    ) -> MaterialId {
        if new + 1 != scene.materials.len() || into >= new {
            return new;
        }
        let m = scene.materials.pop().unwrap();
        scene.materials[into] = m;
        into
    }

    /// Move the instance just added into the removed instance `into`, which must have as many
    /// material slots (its per-draw entries are rewritten in place). Returns the id the
    /// instance has now.
    pub fn recycle_instance(&self, scene: &mut Scene, new: usize, into: usize) -> usize {
        if new + 1 != scene.instances.len() || into >= new || new < scene.uploaded_instances {
            return new;
        }
        if scene.instances[into].slot_alpha.len() != scene.instances[new].slot_alpha.len() {
            return new;
        }
        let mut inst = scene.instances.pop().unwrap();
        inst.base = scene.instances[into].base;
        scene.instances[into] = inst;
        Self::mark_changed(scene, into);
        into
    }

    /// Material slots of an instance (what `recycle_instance` must match).
    pub fn instance_slots(&self, scene: &Scene, instance: usize) -> usize {
        scene
            .instances
            .get(instance)
            .map(|i| i.slot_alpha.len())
            .unwrap_or(0)
    }
}

/// An overlay's rectangle (physical pixels) moved onto whole pixels, its size kept. The
/// overlays are pictures drawn texel for pixel - a text, a plate - and the linear filter
/// blended every pixel of one placed between pixels with its neighbour: the interface's texts
/// were soft at every size whose layout fell between them (most but 100 %, and the timetable's
/// rows at that too). A line thinner than a pixel stays one pixel wide or high.
fn snap_rect(r: [f32; 4]) -> [f32; 4] {
    // (half up the same way left of the window as right of it: `round` goes away from zero,
    // and a rectangle across the left edge came out a pixel wider)
    let snap = |v: f32| (v + 0.5).floor();
    let (x0, y0) = (snap(r[0]), snap(r[1]));
    let x1 = if r[2] > r[0] { snap(r[2]).max(x0 + 1.0) } else { snap(r[2]) };
    let y1 = if r[3] > r[1] { snap(r[3]).max(y0 + 1.0) } else { snap(r[3]) };
    [x0, y0, x1, y1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Overlays drawn texel for pixel: onto whole pixels, their size kept.
    #[test]
    fn overlays_land_on_whole_pixels() {
        // (a 120 x 26 text a quarter and a half pixel off: moved, the same size)
        assert_eq!(snap_rect([25.25, 40.5, 145.25, 66.5]), [25.0, 41.0, 145.0, 67.0]);
        assert_eq!(snap_rect([10.0, 20.0, 30.0, 40.0]), [10.0, 20.0, 30.0, 40.0]);
        // (left of the window as well: the same size)
        assert_eq!(snap_rect([-0.5, -2.5, 19.5, 7.5]), [0.0, -2.0, 20.0, 8.0]);
        // (a separator 0.6 px high stays a line)
        let line = snap_rect([16.0, 100.3, 300.0, 100.9]);
        assert_eq!(line[3] - line[1], 1.0);
        // (an empty rectangle stays empty)
        assert_eq!(snap_rect([5.2, 5.2, 5.2, 5.2]), [5.0, 5.0, 5.0, 5.0]);
    }

    #[test]
    #[ignore = "requires a graphics adapter; renders terrain and foliage lighting"]
    fn enhanced_masked_and_uncut_ground_share_lighting() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let mut renderer = pollster::block_on(Renderer::new_with(
            &instance,
            None,
            Some(wgpu::TextureFormat::Rgba8UnormSrgb),
            RenderOptions {
                msaa: 1,
                ssao: false,
                shadow_size: 1024,
                fxaa: false,
                render_scale: 1.0,
                ..Default::default()
            },
        ))
        .expect("test renderer");
        let mut scene = renderer.new_scene();
        let mut texture = |rgba: [u8; 4]| {
            renderer.add_texture(
                &mut scene,
                &omsi_texture::Image {
                    width: 1,
                    height: 1,
                    rgba: rgba.to_vec(),
                    has_alpha: true,
                },
                false,
            )
        };
        let grey = texture([100, 100, 100, 255]);
        let opaque = texture([255; 4]);
        let transparent = texture([100, 100, 100, 0]);
        let masked = renderer.add_terrain_material(
            &mut scene,
            Some(grey),
            Some(opaque),
            None,
            1.0,
            None,
            0.0,
        );
        let uncut =
            renderer.add_terrain_material(&mut scene, Some(grey), None, None, 1.0, None, 0.0);
        let cut = renderer.add_terrain_material(
            &mut scene,
            Some(grey),
            Some(transparent),
            None,
            1.0,
            None,
            0.0,
        );
        let foliage =
            renderer.add_material(&mut scene, Some(grey), AlphaMode::Test, [1.0; 4], false);
        let cut_foliage = renderer.add_material(
            &mut scene,
            Some(transparent),
            AlphaMode::Test,
            [1.0; 4],
            false,
        );
        let backdrop = renderer.add_material(
            &mut scene,
            None,
            AlphaMode::Opaque,
            [1.0, 0.0, 0.0, 1.0],
            true,
        );
        let mut quad = |left: f32, right: f32, z: f32, material| {
            let mesh = renderer.add_mesh(
                &mut scene,
                &MeshData {
                    positions: vec![
                        Vec3::new(left, -5.0, z),
                        Vec3::new(right, -5.0, z),
                        Vec3::new(right, 5.0, z),
                        Vec3::new(left, 5.0, z),
                    ],
                    normals: vec![Vec3::Z; 4],
                    uvs: vec![glam::Vec2::splat(0.5); 4],
                    indices: vec![0, 1, 2, 0, 2, 3],
                    ranges: vec![(0, 6, 0)],
                    one_sided: false,
                },
            );
            renderer.add_instance(
                &mut scene,
                mesh,
                DVec3::ZERO,
                Mat4::IDENTITY,
                vec![material],
            )
        };
        // Symmetric samples in the same image share exposure, view and lamp distances.
        quad(-6.0, 6.0, -1.0, backdrop);
        let ground = quad(-5.0, -0.5, 0.0, masked);
        let mapped = quad(0.5, 5.0, 0.0, uncut);
        scene.instances[ground].render_phase = RenderPhase::Terrain;
        scene.instances[mapped].render_phase = RenderPhase::Spline;
        scene.instances[mapped].surface = true;
        let camera = Camera {
            position: DVec3::new(0.0, -0.105, 6.0),
            yaw: 0.0,
            pitch: -89.0,
            roll: 0.0,
            fov_deg: 90.0,
            near: 0.1,
            far: 100.0,
        };
        let day = Lighting {
            enhanced: true,
            sun_dir: Vec3::Z,
            sun_intensity: 1.0,
            shadows: false,
            detail: false,
            fog_density: 0.0,
            ..Default::default()
        };
        let night = Lighting {
            sun_dir: -Vec3::Z,
            sun_intensity: 0.0,
            night: 1.0,
            ..day.clone()
        };
        let pixel = |rgba: &[u8], x: usize| -> [u8; 3] {
            rgba[(32 * 64 + x) * 4..(32 * 64 + x) * 4 + 3]
                .try_into()
                .unwrap()
        };
        for (name, lighting) in [("sun", &day), ("lamp", &night)] {
            scene.lights = if name == "lamp" {
                vec![PointLight {
                    position: DVec3::new(0.0, 0.0, 4.0),
                    radius: 20.0,
                    core: 10.0,
                    intensity: 1.0,
                    ..Default::default()
                }]
            } else {
                Vec::new()
            };
            let rgba = renderer
                .render_to_image(&mut scene, 64, 64, &camera, lighting)
                .unwrap();
            let (a, b) = (pixel(&rgba, 16), pixel(&rgba, 47));
            assert!(
                a.iter().all(|v| *v > 10 && *v < 245),
                "lit, unclipped {name}: {a:?}"
            );
            assert!(
                a.iter().zip(b).all(|(a, b)| a.abs_diff(b) <= 2),
                "masked terrain and uncut mapped ground differ under {name}: {a:?} / {b:?}"
            );
        }
        // A road cut still reveals the geometry beneath it.
        renderer.set_material(&mut scene, ground, 0, cut);
        let rgba = renderer
            .render_to_image(&mut scene, 64, 64, &camera, &night)
            .unwrap();
        let a = pixel(&rgba, 16);
        assert!(
            a[0] > a[1] + 30 && a[0] > a[2] + 30,
            "road cut must reveal red: {a:?}"
        );

        // Real foliage still scatters light arriving from behind its normal; ground does not.
        renderer.set_material(&mut scene, ground, 0, masked);
        renderer.set_material(&mut scene, mapped, 0, foliage);
        scene.lights[0].position.z = -4.0;
        let rgba = renderer
            .render_to_image(&mut scene, 64, 64, &camera, &night)
            .unwrap();
        let (a, b) = (pixel(&rgba, 16), pixel(&rgba, 47));
        assert!(
            b[1] > a[1] + 8,
            "foliage retains backlighting: ground {a:?}, foliage {b:?}"
        );
        renderer.set_material(&mut scene, mapped, 0, cut_foliage);
        let rgba = renderer
            .render_to_image(&mut scene, 64, 64, &camera, &night)
            .unwrap();
        let b = pixel(&rgba, 47);
        assert!(
            b[0] > b[1] + 30 && b[0] > b[2] + 30,
            "foliage cutout must reveal red: {b:?}"
        );
    }

    #[test]
    #[ignore = "requires a graphics adapter; run with --ignored on a GPU host"]
    fn cached_bounds_follow_transforms_skinning_and_recycled_resources() {
        let adapter = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let renderer = pollster::block_on(Renderer::new_with(
            &adapter, None, Some(wgpu::TextureFormat::Rgba8UnormSrgb),
            RenderOptions { msaa: 1, shadow_size: 1024, ..Default::default() },
        )).expect("test renderer");
        let mut scene = renderer.new_scene();
        scene.cache_bounds = true;
        let material = renderer.add_material(&mut scene, None, AlphaMode::Opaque, [1.0; 4], true);
        let data = MeshData {
            positions: vec![Vec3::ZERO, Vec3::X, Vec3::Y],
            normals: vec![Vec3::Z; 3], uvs: vec![glam::Vec2::ZERO; 3],
            indices: vec![0, 1, 2], ranges: vec![(0, 3, 0)], ..Default::default()
        };
        let mesh = renderer.add_mesh(&mut scene, &data);
        let origin = DVec3::new(1_000_000.000_001, 2_000_000.0, 12.0);
        let i = renderer.add_instance(&mut scene, mesh, origin, Mat4::IDENTITY, vec![material]);
        let check = |scene: &Scene, i: usize| {
            let inst = &scene.instances[i];
            let expected = InstanceBounds::new(&scene.meshes[inst.mesh], inst.transform);
            assert_eq!(Renderer::bounding_sphere(scene, inst),
                (expected.centre + (inst.origin - scene.render_origin).as_vec3(), expected.radius));
            assert_eq!(Renderer::instance_scale(scene, inst), expected.scale);
        };
        renderer.prepare(&mut scene);
        check(&scene, i);
        let transform = Mat4::from_scale_rotation_translation(
            Vec3::new(-2.0, 3.0, 4.0), glam::Quat::from_rotation_z(0.7), Vec3::new(10.0, 20.0, 30.0));
        renderer.set_transform(&mut scene, i, origin, transform);
        renderer.prepare(&mut scene);
        check(&scene, i);
        let posed: Vec<_> = data.positions.iter().map(|p| *p * 5.0 + Vec3::Z).collect();
        renderer.update_mesh(&mut scene, mesh, &posed, &data.normals, &data.uvs);
        renderer.prepare(&mut scene);
        check(&scene, i);
        renderer.set_render_origin(&mut scene, origin - DVec3::new(0.25, 0.5, 0.75));
        renderer.prepare(&mut scene);
        check(&scene, i);
        renderer.free_mesh(&mut scene, mesh);
        renderer.prepare(&mut scene);
        check(&scene, i);
        let new = renderer.add_mesh(&mut scene, &data);
        assert_eq!(renderer.recycle_mesh(&mut scene, new, mesh), mesh);
        renderer.prepare(&mut scene);
        check(&scene, i);
        let replacement = renderer.add_instance(&mut scene, mesh, origin, Mat4::IDENTITY, vec![material]);
        assert_eq!(renderer.recycle_instance(&mut scene, replacement, i), i);
        renderer.prepare(&mut scene);
        check(&scene, i);
        let other = renderer.add_mesh(&mut scene, &data);
        renderer.set_instance_mesh(&mut scene, i, other);
        renderer.prepare(&mut scene);
        check(&scene, i);
    }

    #[test]
    #[ignore = "requires a graphics adapter; run with --ignored on a GPU host"]
    fn presurface_reveals_excavation_before_terrain_is_drawn() {
        fn quad(renderer: &Renderer, scene: &mut Scene, y: f32, half: f32) -> MeshId {
            renderer.add_mesh(
                scene,
                &MeshData {
                    positions: vec![
                        Vec3::new(-half, y, -half),
                        Vec3::new(half, y, -half),
                        Vec3::new(half, y, half),
                        Vec3::new(-half, y, half),
                    ],
                    normals: vec![-Vec3::Y; 4],
                    uvs: vec![glam::Vec2::ZERO; 4],
                    ranges: vec![(0, 6, 0)],
                    indices: vec![0, 1, 2, 0, 2, 3],
                    one_sided: false,
                },
            )
        }
        let camera = Camera {
            position: DVec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            roll: 0.0,
            fov_deg: 90.0,
            near: 0.1,
            far: 100.0,
        };
        for (msaa, ssao, enhanced) in [
            (1, false, false),
            (1, true, false),
            (1, true, true),
            (4, true, true),
        ] {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let mut renderer = pollster::block_on(Renderer::new_with(
                &instance,
                None,
                Some(wgpu::TextureFormat::Rgba8UnormSrgb),
                RenderOptions {
                    msaa,
                    ssao,
                    shadow_size: 1024,
                    fxaa: false,
                    render_scale: 1.0,
                    ..Default::default()
                },
            ))
            .expect("test renderer");
            let mut scene = renderer.new_scene();
            let green = renderer.add_material(
                &mut scene,
                None,
                AlphaMode::Opaque,
                [0.0, 1.0, 0.0, 1.0],
                true,
            );
            let blue = renderer.add_material(
                &mut scene,
                None,
                AlphaMode::Opaque,
                [0.0, 0.0, 1.0, 1.0],
                true,
            );
            let red = renderer.add_material(
                &mut scene,
                None,
                AlphaMode::Opaque,
                [1.0, 0.0, 0.0, 1.0],
                true,
            );
            let texture = renderer.add_texture(
                &mut scene,
                &omsi_texture::Image {
                    width: 1,
                    height: 1,
                    rgba: vec![255, 255, 255, 0],
                    has_alpha: true,
                },
                false,
            );
            let transparent =
                renderer.add_material(&mut scene, Some(texture), AlphaMode::Blend, [1.0; 4], true);
            let cutout =
                renderer.add_material(&mut scene, Some(texture), AlphaMode::Test, [1.0; 4], true);
            // Put terrain first in the scene to catch reliance on insertion order. The
            // excavation floor is behind it, with a transparent cover in front of both.
            let terrain = quad(&renderer, &mut scene, 6.0, 10.0);
            renderer.add_instance(
                &mut scene,
                terrain,
                DVec3::ZERO,
                Mat4::IDENTITY,
                vec![green],
            );
            let floor = quad(&renderer, &mut scene, 8.0, 10.0);
            let floor = renderer.add_surface_instance(
                &mut scene,
                floor,
                DVec3::ZERO,
                Mat4::IDENTITY,
                vec![blue],
            );
            scene.instances[floor].presurface = true;
            let cover_mesh = quad(&renderer, &mut scene, 4.0, 1.5);
            let cover = renderer.add_surface_instance(
                &mut scene,
                cover_mesh,
                DVec3::ZERO,
                Mat4::IDENTITY,
                vec![transparent],
            );
            scene.instances[cover].presurface = true;
            let foreground_mesh = quad(&renderer, &mut scene, 2.0, 0.25);
            let foreground = renderer.add_instance(
                &mut scene,
                foreground_mesh,
                DVec3::ZERO,
                Mat4::IDENTITY,
                vec![red],
            );
            scene.instances[foreground].visible = false;
            let lighting = Lighting {
                enhanced,
                shadows: false,
                fog_density: 0.0,
                ..Default::default()
            };
            let pixel = |rgba: &[u8], x: usize| -> [u8; 3] {
                rgba[(32 * 64 + x) * 4..(32 * 64 + x) * 4 + 3]
                    .try_into()
                    .unwrap()
            };
            let rgba = renderer
                .render_to_image(&mut scene, 64, 64, &camera, &lighting)
                .unwrap();
            let centre = pixel(&rgba, 32);
            assert!(
                centre[2] > centre[1] + 40,
                "floor must show through cover: {centre:?}; {msaa}/{ssao}/{enhanced}"
            );
            let outside = pixel(&rgba, 4);
            assert!(
                outside[1] > outside[2] + 40,
                "terrain outside cover: {outside:?}"
            );
            scene.instances[foreground].visible = true;
            Renderer::mark_changed(&mut scene, foreground);
            let rgba = renderer
                .render_to_image(&mut scene, 64, 64, &camera, &lighting)
                .unwrap();
            let centre = pixel(&rgba, 32);
            assert!(
                centre[0] > centre[2] + 40,
                "foreground stays visible: {centre:?}"
            );
            scene.instances[foreground].visible = false;
            Renderer::mark_changed(&mut scene, foreground);
            // Ordinary blended surfaces must keep showing the terrain, as must covers
            // whose material explicitly disables depth writes or uses alpha testing.
            for (presurface, alpha, no_z_write) in [
                (false, AlphaMode::Blend, false),
                (true, AlphaMode::Blend, true),
                (true, AlphaMode::Test, false),
            ] {
                scene.instances[cover].presurface = presurface;
                scene.materials[transparent].no_z_write = no_z_write;
                renderer.set_material(
                    &mut scene,
                    cover,
                    0,
                    if alpha == AlphaMode::Test {
                        cutout
                    } else {
                        transparent
                    },
                );
                let rgba = renderer
                    .render_to_image(&mut scene, 64, 64, &camera, &lighting)
                    .unwrap();
                let centre = pixel(&rgba, 32);
                assert!(
                    centre[1] > centre[2] + 40,
                    "terrain should show: {centre:?}; {presurface}/{alpha:?}/{no_z_write}"
                );
            }
        }
    }

    /// Two blended panes of one model, the near one listed first, both marked see-through
    /// (`no_z_write`) for the shading: written into the depth buffer as Omsi.exe writes it
    /// (`writes_depth`, no `[matl_noZwrite]` in the model), the far pane drawn after it is
    /// hidden behind it; left out of it (`[matl_noZwrite]`), it is blended over it (#211).
    #[test]
    #[ignore = "requires a graphics adapter; run with --ignored on a GPU host"]
    fn stacked_panes_hide_each_other_in_model_order_where_they_write_depth() {
        let camera = Camera {
            position: DVec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            roll: 0.0,
            fov_deg: 90.0,
            near: 0.1,
            far: 100.0,
        };
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let mut renderer = pollster::block_on(Renderer::new_with(
            &instance,
            None,
            Some(wgpu::TextureFormat::Rgba8UnormSrgb),
            RenderOptions { msaa: 1, ssao: false, shadow_size: 1024, fxaa: false, render_scale: 1.0, ..Default::default() },
        ))
        .expect("test renderer");
        let mut scene = renderer.new_scene();
        let quad = |y: f32, half: f32| -> [Vec3; 4] {
            [Vec3::new(-half, y, -half), Vec3::new(half, y, -half), Vec3::new(half, y, half), Vec3::new(-half, y, half)]
        };
        let wall = renderer.add_mesh(
            &mut scene,
            &MeshData {
                positions: quad(8.0, 20.0).to_vec(),
                normals: vec![-Vec3::Y; 4],
                uvs: vec![glam::Vec2::ZERO; 4],
                ranges: vec![(0, 6, 0)],
                indices: vec![0, 1, 2, 0, 2, 3],
                one_sided: false,
            },
        );
        let green = renderer.add_material(&mut scene, None, AlphaMode::Opaque, [0.0, 1.0, 0.0, 1.0], true);
        renderer.add_instance(&mut scene, wall, DVec3::ZERO, Mat4::IDENTITY, vec![green]);
        // the near pane (slot 0) and the far one (slot 1), half transparent
        let panes = renderer.add_mesh(
            &mut scene,
            &MeshData {
                positions: [quad(2.0, 4.0), quad(4.0, 8.0)].concat(),
                normals: vec![-Vec3::Y; 8],
                uvs: vec![glam::Vec2::ZERO; 8],
                ranges: vec![(0, 6, 0), (6, 6, 1)],
                indices: vec![0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7],
                one_sided: false,
            },
        );
        let half = renderer.add_texture(
            &mut scene,
            &omsi_texture::Image { width: 1, height: 1, rgba: vec![255, 255, 255, 128], has_alpha: true },
            false,
        );
        let near = renderer.add_material(&mut scene, Some(half), AlphaMode::Blend, [1.0, 0.0, 0.0, 1.0], true);
        let far = renderer.add_material(&mut scene, Some(half), AlphaMode::Blend, [0.0, 0.0, 1.0, 1.0], true);
        renderer.add_instance(&mut scene, panes, DVec3::ZERO, Mat4::IDENTITY, vec![near, far]);
        let lighting = Lighting { shadows: false, fog_density: 0.0, ..Default::default() };
        for writes_depth in [true, false] {
            for m in [near, far] {
                scene.materials[m].no_z_write = true;
                scene.materials[m].writes_depth = writes_depth;
            }
            let rgba = renderer.render_to_image(&mut scene, 64, 64, &camera, &lighting).unwrap();
            let c = &rgba[(32 * 64 + 32) * 4..(32 * 64 + 32) * 4 + 3];
            if writes_depth {
                assert!(c[2] < 20 && c[0] > 60 && c[1] > 60, "far pane hidden behind the near one: {c:?}");
            } else {
                assert!(c[2] > 40 && c[0] > 60, "far pane blended over the near one: {c:?}");
            }
        }
    }

    /// A lamp's flare behind a window that writes its depth: seen from inside the vehicle
    /// it shows through the glass, which Omsi.exe draws after the flares (0x6f0430 after
    /// 0x6f0400/0x6f0418); seen from outside the vehicle, drawn before the flares, the
    /// window hides it as in the original.
    #[test]
    #[ignore = "requires a graphics adapter; run with --ignored on a GPU host"]
    fn flares_show_through_the_glass_of_the_vehicle_the_camera_is_in() {
        let camera = Camera { position: DVec3::ZERO, yaw: 0.0, pitch: 0.0, roll: 0.0, fov_deg: 90.0, near: 0.1, far: 100.0 };
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let mut renderer = pollster::block_on(Renderer::new_with(
            &instance,
            None,
            Some(wgpu::TextureFormat::Rgba8UnormSrgb),
            RenderOptions { msaa: 1, ssao: false, shadow_size: 1024, fxaa: false, render_scale: 1.0, ..Default::default() },
        ))
        .expect("test renderer");
        let mut scene = renderer.new_scene();
        let quad = |y: f32, half: f32| MeshData {
            positions: vec![Vec3::new(-half, y, -half), Vec3::new(half, y, -half), Vec3::new(half, y, half), Vec3::new(-half, y, half)],
            normals: vec![-Vec3::Y; 4],
            uvs: vec![glam::Vec2::ZERO; 4],
            ranges: vec![(0, 6, 0)],
            indices: vec![0, 1, 2, 0, 2, 3],
            one_sided: false,
        };
        let wall = renderer.add_mesh(&mut scene, &quad(30.0, 60.0));
        let black = renderer.add_material(&mut scene, None, AlphaMode::Opaque, [0.0, 0.0, 0.0, 1.0], true);
        renderer.add_instance(&mut scene, wall, DVec3::new(0.0, 0.0, 0.0), Mat4::IDENTITY, vec![black]);
        let pane = renderer.add_mesh(&mut scene, &quad(2.0, 4.0));
        let half = renderer.add_texture(
            &mut scene,
            &omsi_texture::Image { width: 1, height: 1, rgba: vec![255, 255, 255, 64], has_alpha: true },
            false,
        );
        let glass = renderer.add_material(&mut scene, Some(half), AlphaMode::Blend, [0.2, 0.2, 0.2, 1.0], true);
        scene.materials[glass].no_z_write = true;
        scene.materials[glass].writes_depth = true;
        renderer.add_instance(&mut scene, pane, DVec3::ZERO, Mat4::IDENTITY, vec![glass]);
        scene.coronas = vec![Corona { position: DVec3::new(0.0, 10.0, 0.0), size: 3.0, color: [1.0; 3], brightness: 1.0, ..Default::default() }];
        for inside in [true, false] {
            let lighting = Lighting {
                shadows: false,
                fog_density: 0.0,
                night: 1.0,
                inside: inside.then_some((DVec3::ZERO, 0.0, [6.0, 6.0, 6.0, 0.0, 0.0, 0.0])),
                ..Default::default()
            };
            let rgba = renderer.render_to_image(&mut scene, 64, 64, &camera, &lighting).unwrap();
            let c = &rgba[(32 * 64 + 32) * 4..(32 * 64 + 32) * 4 + 3];
            if inside {
                assert!(c[0] > 120, "the flare shows through the windscreen: {c:?}");
            } else {
                assert!(c[0] < 80, "the flare behind a bus's window seen from outside: {c:?}");
            }
        }
    }

    #[test]
    fn noop_backend_initializes_renderer() {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::NOOP;
        descriptor.backend_options.noop = wgpu::NoopBackendOptions { enable: true };
        let instance = wgpu::Instance::new(descriptor);
        let res = pollster::block_on(Renderer::new_with(
            &instance,
            None,
            Some(wgpu::TextureFormat::Rgba8UnormSrgb),
            RenderOptions {
                msaa: 2,
                shadow_size: 1024,
                ..Default::default()
            },
        ));
        assert!(res.is_ok(), "renderer should initialize on noop backend: {:?}", res.err());
    }

    #[test]
    fn declared_transmap_ignores_slot_alpha() {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::NOOP;
        descriptor.backend_options.noop = wgpu::NoopBackendOptions { enable: true };
        let instance = wgpu::Instance::new(descriptor);
        let renderer = pollster::block_on(Renderer::new_with(
            &instance,
            None,
            Some(wgpu::TextureFormat::Rgba8UnormSrgb),
            RenderOptions {
                msaa: 1,
                shadow_size: 1024,
                ..Default::default()
            },
        ))
        .expect("noop renderer");
        let mut scene = renderer.new_scene();
        let blended =
            |scene: &mut Scene, transmap: Option<(TextureId, bool)>, extra: MaterialExtra| {
                renderer.add_material_extra(
                    scene,
                    None,
                    AlphaMode::Blend,
                    [1.0; 4],
                    true,
                    transmap,
                    None,
                    None,
                    None,
                    [0.0; 3],
                    extra,
                )
            };
        // declared, its file missing: no transmap texture bound
        let declared = blended(
            &mut scene,
            None,
            MaterialExtra {
                transmap_declared: true,
                ..Default::default()
            },
        );
        let map = renderer.add_blank_texture(&mut scene, 1, 1);
        let bound = blended(&mut scene, Some((map, true)), MaterialExtra::default());
        // another bit of the same flags, no transmap
        let metal = blended(
            &mut scene,
            None,
            MaterialExtra {
                metal_ok: true,
                ..Default::default()
            },
        );
        let plain = blended(&mut scene, None, MaterialExtra::default());
        assert_eq!(scene.materials[metal].uniform.params2[3], 4.0);
        let flags: Vec<bool> = [declared, bound, metal, plain]
            .iter()
            .map(|&m| scene.materials[m].transmap_declared())
            .collect();
        assert_eq!(flags, [true, true, false, false]);
        let corner = [Vec3::ZERO, Vec3::X, Vec3::Y];
        let data = MeshData {
            positions: corner.repeat(4),
            normals: vec![Vec3::Z; 12],
            uvs: vec![glam::Vec2::ZERO; 12],
            indices: (0..12).collect(),
            ranges: (0..4).map(|s| (s * 3, 3, s)).collect(),
            ..Default::default()
        };
        let mesh = renderer.add_mesh(&mut scene, &data);
        let i = renderer.add_instance(
            &mut scene,
            mesh,
            DVec3::ZERO,
            Mat4::IDENTITY,
            vec![declared, bound, metal, plain],
        );
        renderer.set_params(&mut scene, i, &[0.0, 0.0, 0.35, 0.35], true, &[]);
        assert_eq!(scene.instances[i].slot_alpha, vec![1.0, 1.0, 0.35, 0.35]);
    }

    #[test]
    fn omsi_render_phases_are_monotonic_and_complete() {
        assert_eq!(
            RenderPhase::DRAW_ORDER.map(|phase| phase as usize),
            [0, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(RenderPhase::default(), RenderPhase::Normal);
    }

    #[test]
    fn metric_lifted_surfaces_keep_shading_class_without_view_space_pull() {
        assert_eq!(surface_instance_code(false, false, false, true, false), 0.9);
        assert_eq!(surface_instance_code(false, false, false, true, true), 1.0);
        assert_eq!(surface_instance_code(false, false, true, false, false), 0.9);
        assert_eq!(surface_instance_code(false, true, false, true, false), 0.75);
    }

    #[test]
    fn spline_blend_sort_ignores_height_and_camera_pitch() {
        let origin = DVec3::new(120.0, 45.0, 0.0);
        let render_origin = DVec3::new(100.0, 40.0, 0.0);
        let level_camera = Vec3::new(3.0, 1.0, 4.0);
        let high_camera = Vec3::new(3.0, 1.0, 80.0);
        assert_eq!(
            horizontal_sort_distance(origin, render_origin, level_camera),
            horizontal_sort_distance(origin + DVec3::Z * 60.0, render_origin, high_camera)
        );
    }

    #[test]
    fn render_scale_auto_keeps_ordinary_windows_sharp() {
        // the default window and a 2560x1080 screen are drawn at full size
        assert_eq!(scene_scale_for(0.0, 1600, 900), 1.0);
        assert_eq!(scene_scale_for(0.0, 2560, 1080), 1.0);
        // Mac/Android cap this at 2.8 million pixels; desktops keep it at full size.
        let s = scene_scale_for(0.0, 3200, 1800);
        if cfg!(target_os = "macos") || cfg!(target_os = "android") {
            assert!((s - 0.697).abs() < 0.01, "{s}");
            assert!((3200.0 * s * 1800.0 * s - AUTO_SCALE_PIXELS).abs() < 1.0);
        } else {
            assert_eq!(s, 1.0);
        }
        // never below half size
        assert_eq!(scene_scale_for(0.0, 16384, 16384), 0.5);
        // what is asked for, within 0.5..1 - on a Mac or a phone a scale over the pixel
        // budget is capped like the automatic one
        let s = scene_scale_for(0.75, 3200, 1800);
        if cfg!(target_os = "macos") || cfg!(target_os = "android") {
            assert!((s - 0.697).abs() < 0.01, "{s}");
            assert!((3200.0 * s * 1800.0 * s - AUTO_SCALE_PIXELS).abs() < 1.0);
        } else {
            assert_eq!(s, 0.75);
        }
        // a small window under the budget keeps what it asked for
        assert_eq!(scene_scale_for(0.3, 1600, 900), 0.5);
        assert_eq!(scene_scale_for(1.4, 1600, 900), 1.0);
    }

    #[test]
    fn surface_depth_coverage_excludes_glass_and_terrain_masks() {
        assert!(surface_depth_coverage(RenderPhase::Spline, AlphaMode::Blend, false, false));
        for phase in RenderPhase::DRAW_ORDER {
            if !world_surface_phase(phase) {
                assert!(!surface_depth_coverage(phase, AlphaMode::Blend, false, false));
            } else {
                assert!(surface_depth_coverage(phase, AlphaMode::Blend, false, false));
            }
        }
        assert!(!surface_depth_coverage(RenderPhase::Spline, AlphaMode::Blend, true, false));
        assert!(!surface_depth_coverage(RenderPhase::Spline, AlphaMode::Blend, false, true));
        assert!(!surface_depth_coverage(RenderPhase::Spline, AlphaMode::Opaque, false, false));
        assert!(!surface_depth_coverage(RenderPhase::Spline, AlphaMode::Test, false, false));
        let order = RenderPhase::DRAW_ORDER;
        assert!(order.iter().position(|p| *p == RenderPhase::OnSurface).unwrap()
            < order.iter().position(|p| *p == RenderPhase::BeforeNormal).unwrap());
    }

    #[test]
    fn pipeline_codes_cover_the_table() {
        // every kind x culling x depth bias has its own pipeline, and the opaque and
        // alpha-tested codes sort before the blended ones
        let mut seen = std::collections::HashSet::new();
        for kind in 0..PIPE_KINDS {
            for cull in [false, true] {
                for surface in [false, true] {
                    let code = pipe_code(kind, cull, surface);
                    assert!((code as usize) < PIPE_KINDS as usize * 4);
                    assert!(seen.insert(code));
                    assert_eq!(
                        code < pipe_code(PIPE_BLEND, false, false),
                        kind < PIPE_BLEND
                    );
                }
            }
        }
        assert_eq!(seen.len(), PIPE_KINDS as usize * 4);
        assert_eq!(pipe_code(PIPE_OPAQUE, false, false), 0);
        assert!(
            pipe_code(PIPE_ALPHA_TEST, true, true) < pipe_code(PIPE_BLEND_NO_WRITE, false, false)
        );
    }

    #[test]
    fn nearest_by_origin_picks_the_closest_mesh_of_each_object() {
        // A long bus (one origin) has a window 3 m from the camera and another 14 m away;
        // ranking the whole object by its shared origin (here 8 m) instead of its nearest
        // blended mesh is exactly what let a car beside the near window sort as farther
        // away than the bus and disappear behind it.
        let bus = DVec3::new(0.0, 0.0, 0.0);
        let car = DVec3::new(1.0, 0.0, 0.0);
        let by_origin = nearest_by_origin([(bus, 14.0), (bus, 3.0), (car, 8.0)]);
        let bus_dist = by_origin[&origin_key(bus)];
        let car_dist = by_origin[&origin_key(car)];
        assert_eq!(
            bus_dist, 3.0,
            "the object's distance is its nearest mesh, not the first or an average"
        );
        assert_eq!(car_dist, 8.0);
        // the car (8 m) is nearer than the bus's near window (3 m) is far: with the old
        // origin-only distance (bus origin 8.5 m, say) the two could tie or invert
        assert!(car_dist > bus_dist);
        assert_eq!(
            by_origin.len(),
            2,
            "one entry per distinct origin, not per mesh"
        );
    }

    #[test]
    fn origin_lookup_keeps_float_equality_and_large_coordinates() {
        let a = DVec3::new(-0.0, 1_000_000.000_001, 2.0);
        let b = DVec3::new(0.0, a.y, 2.0);
        let c = DVec3::new(0.0, a.y + 0.000_001, 2.0);
        let distances = nearest_by_origin([(a, 8.0), (b, 3.0), (c, 5.0), (DVec3::NAN, 1.0)]);
        assert_eq!(distances.len(), 2);
        assert_eq!(distances[&origin_key(a)], 3.0);
        assert_eq!(distances[&origin_key(c)], 5.0);
    }

    /// Every shader module parses and validates as the device will see it, translates to
    /// Metal and Vulkan SPIR-V, and its uniform structs are laid out as the Rust side writes them.
    #[test]
    fn shaders_validate_and_match_the_uniforms() {
        use wgpu::naga;
        let modules = [
            ("scene", scene_shader_source(false)),
            ("sky", sky_shader_source()),
            ("corona", corona_shader_source()),
            ("post", include_str!("post.wgsl").to_string()),
            ("ssao", include_str!("ssao.wgsl").to_string()),
            ("puddles", puddles::shader_source()),
            ("upscale", include_str!("upscale.wgsl").to_string()),
            ("mip", include_str!("mip.wgsl").to_string()),
            ("xr_ui", include_str!("xr_ui.wgsl").to_string()),
        ];
        let sizes: &[(&str, usize)] = &[
            ("Enhanced", std::mem::size_of::<EnhancedUniform>()),
            ("PostParams", std::mem::size_of::<PostUniform>()),
            ("PuddleParams", std::mem::size_of::<puddles::Uniform>()),
            ("VehicleReflection", std::mem::size_of::<puddles::VehicleUniform>()),
            ("PointLight", std::mem::size_of::<GpuPointLight>()),
            ("Camera", std::mem::size_of::<CameraUniform>()),
            ("MaterialParams", std::mem::size_of::<MaterialUniform>()),
        ];
        let mut checked = std::collections::HashSet::new();
        for (name, src) in &modules {
            let module = naga::front::wgsl::parse_str(src)
                .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(src)));
            let info = naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            // the backends take no override: resolved to their defaults as wgpu does
            let (module, info) = naga::back::pipeline_constants::process_overrides(
                &module,
                &info,
                None,
                &Default::default(),
            )
            .unwrap_or_else(|e| panic!("{name}: overrides: {e:?}"));
            let (module, info) = (module.into_owned(), info.into_owned());
            #[cfg(target_os = "macos")]
            let options = naga::back::msl::Options {
                lang_version: (2, 4),
                ..Default::default()
            };
            #[cfg(target_os = "macos")]
            naga::back::msl::write_string(
                &module,
                &info,
                &options,
                &naga::back::msl::PipelineOptions::default(),
            )
            .unwrap_or_else(|e| panic!("{name}: Metal: {e:?}"));
            for entry in &module.entry_points {
                let pipeline = naga::back::spv::PipelineOptions {
                    shader_stage: entry.stage,
                    entry_point: entry.name.clone(),
                };
                naga::back::spv::write_vec(&module, &info, &Default::default(), Some(&pipeline))
                    .unwrap_or_else(|e| panic!("{name}/{}: Vulkan: {e:?}", entry.name));
            }
            let mut layouter = naga::proc::Layouter::default();
            layouter.update(module.to_ctx()).expect("layout");
            for (ty_name, rust) in sizes {
                if *ty_name == "Camera" && *name == "corona" {
                    // Corona owns just the prefix through `cam_up`.  It must still include
                    // every member in that prefix: a missing `world_origin`, for example,
                    // makes a subsequently-added lighting use read the wrong vec4.
                    if let Some((h, _)) = module
                        .types
                        .iter()
                        .find(|(_, t)| t.name.as_deref() == Some(*ty_name))
                    {
                        assert_eq!(
                            layouter[h].size as usize,
                            std::mem::offset_of!(CameraUniform, clouds),
                            "corona: Camera prefix"
                        );
                    }
                    continue;
                }
                if let Some((h, _)) = module
                    .types
                    .iter()
                    .find(|(_, t)| t.name.as_deref() == Some(*ty_name))
                {
                    assert_eq!(layouter[h].size as usize, *rust, "{name}: {ty_name}");
                    checked.insert(*ty_name);
                }
            }
        }
        assert_eq!(checked.len(), sizes.len(), "structs checked: {checked:?}");
    }

    /// The scene module as the OpenGL backend gets it translates to GLSL ES 3.10 and desktop
    /// GLSL 4.30 for every entry point, with no `invariant gl_FragCoord` (rejected by AMD's
    /// desktop GL and by GLES) and no texture read through two samplers (#617, #610).
    #[test]
    fn the_scene_shader_translates_to_glsl() {
        use wgpu::naga;
        use wgpu::naga::back::glsl;
        let src = scene_shader_source(true);
        let module = naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
        let info = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
            .validate(&module)
            .expect("validate");
        let (module, info) =
            naga::back::pipeline_constants::process_overrides(&module, &info, None, &Default::default()).expect("overrides");
        for version in [glsl::Version::Embedded { version: 310, is_webgl: false }, glsl::Version::Desktop(430)] {
            let options = glsl::Options { version, ..Default::default() };
            for entry in &module.entry_points {
                let pipeline = glsl::PipelineOptions {
                    shader_stage: entry.stage,
                    entry_point: entry.name.clone(),
                    multiview: None,
                };
                let mut out = String::new();
                glsl::Writer::new(&mut out, &module, &info, &options, &pipeline, Default::default())
                    .and_then(|mut w| w.write())
                    .unwrap_or_else(|e| panic!("{version:?} {}: {e:?}", entry.name));
                assert!(!out.contains("invariant gl_FragCoord"), "{version:?} {}", entry.name);
            }
        }
    }

    /// Where the device cannot read storage buffers in a vertex shader, or none at all, the
    /// scene module reads its arrays from textures: it validates and translates to the GLSL
    /// of such chips (GLES 3.1 / GL 4.3 with storage for the lights, GLES 3.0 / GL 3.3
    /// without), and nothing of it is a storage buffer where it must not be (#770, #316).
    #[test]
    fn the_scene_shader_reads_its_arrays_from_textures_without_vertex_storage() {
        use wgpu::naga;
        use wgpu::naga::back::glsl;
        for (path, versions) in [
            (ArrayPath::VertexTextures, [glsl::Version::Embedded { version: 310, is_webgl: false }, glsl::Version::Desktop(430)]),
            (ArrayPath::NoStorage, [glsl::Version::Embedded { version: 300, is_webgl: false }, glsl::Version::Desktop(330)]),
        ] {
            let src = arrays_as_textures(&scene_shader_text(true), path);
            assert!(!src.contains("models[") && !src.contains("inst_params[") && !src.contains("draw_list["));
            assert_eq!(src.contains("var<storage"), path == ArrayPath::VertexTextures, "{path:?}");
            let module = naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{path:?}: {}", e.emit_to_string(&src)));
            let info = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
                .validate(&module)
                .unwrap_or_else(|e| panic!("{path:?}: {e:?}"));
            let (module, info) =
                naga::back::pipeline_constants::process_overrides(&module, &info, None, &Default::default()).expect("overrides");
            for version in versions {
                let options = glsl::Options { version, ..Default::default() };
                for entry in &module.entry_points {
                    let pipeline = glsl::PipelineOptions { shader_stage: entry.stage, entry_point: entry.name.clone(), multiview: None };
                    let mut out = String::new();
                    glsl::Writer::new(&mut out, &module, &info, &options, &pipeline, Default::default())
                        .and_then(|mut w| w.write())
                        .unwrap_or_else(|e| panic!("{path:?} {version:?} {}: {e:?}", entry.name));
                    let vertex = entry.stage == naga::ShaderStage::Vertex;
                    if vertex || path == ArrayPath::NoStorage {
                        assert!(!out.contains(" buffer "), "{path:?} {version:?} {}: a storage block", entry.name);
                    }
                }
            }
        }
        // the rewrite leaves names that only end alike alone, and nests
        assert_eq!(indexing_as_calls("a = my_models[1]; b = models[models[i + 1u] .x];", "models", "f"), "a = my_models[1]; b = f(f(i + 1u) .x);");
    }

    /// An array texture is written in the rest of a row, whole rows and the start of the
    /// last row, and never past its end.
    #[test]
    fn array_texture_writes_cover_the_range_once() {
        let w = ARRAY_TEX_WIDTH;
        assert_eq!(array_tex_spans(0, 10, 4 * w as u64), vec![(0, 0, 10, 1)]);
        assert_eq!(array_tex_spans(w as u64 - 2, 5, 4 * w as u64), vec![(w - 2, 0, 2, 1), (0, 1, 3, 1)]);
        assert_eq!(array_tex_spans(5, 3 * w as u64, 4 * w as u64), vec![(5, 0, w - 5, 1), (0, 1, w, 2), (0, 3, 5, 1)]);
        assert_eq!(array_tex_spans(0, 2 * w as u64, 4 * w as u64), vec![(0, 0, w, 2)]);
        // past the end: cut
        assert_eq!(array_tex_spans(2 * w as u64 - 1, 10, 2 * w as u64), vec![(w - 1, 1, 1, 1)]);
        assert!(array_tex_spans(3 * w as u64, 10, 2 * w as u64).is_empty());
        for (at, n) in [(0u64, 1u64), (7, 9000), (2047, 2049), (4096, 4096)] {
            let covered: u64 = array_tex_spans(at, n, 64 * w as u64).iter().map(|s| s.2 as u64 * s.3 as u64).sum();
            assert_eq!(covered, n);
        }
    }

    #[test]
    fn the_sky_is_recomputed_only_when_it_has_moved_on() {
        let a = atmosphere::SkyInput::default();
        assert!(!sky_input_differs(&a, &a));
        // the sun a hundredth of a degree on, a tint that drifts by a thousandth: the same sky
        let turn = |deg: f32| {
            glam::Quat::from_axis_angle(a.sun_dir.any_orthonormal_vector(), deg.to_radians())
                * a.sun_dir
        };
        assert!(!sky_input_differs(
            &a,
            &atmosphere::SkyInput {
                sun_dir: turn(0.01),
                tint: [Vec3::splat(1.001); 3],
                ..a
            }
        ));
        // a quarter of a degree, some rain, another envir.cfg: a new sky
        assert!(sky_input_differs(
            &a,
            &atmosphere::SkyInput {
                sun_dir: turn(0.25),
                ..a
            }
        ));
        assert!(sky_input_differs(
            &a,
            &atmosphere::SkyInput { rain: 0.3, ..a }
        ));
        assert!(sky_input_differs(
            &a,
            &atmosphere::SkyInput {
                tint: [Vec3::new(1.2, 1.0, 0.9), Vec3::ONE, Vec3::ONE],
                ..a
            }
        ));
    }

    #[test]
    fn each_path_gets_its_own_headlights() {
        let lamp = PointLight {
            radius: 30.0,
            color: [1.0, 0.78, 0.46],
            core: 5.0,
            ..Default::default()
        };
        let stand_in = PointLight {
            radius: 18.0,
            intensity: 0.8,
            mode: LightMode::Vanilla,
            ..Default::default()
        };
        let spot = PointLight {
            radius: 60.0,
            intensity: 20.0,
            direction: Vec3::new(0.0, 2.0, -0.6),
            cone: [0.97, 0.82],
            core: 1.0,
            beam: 24.0,
            mode: LightMode::Enhanced,
            ..Default::default()
        };
        assert!(drawn_by(&lamp, false) && drawn_by(&lamp, true));
        assert!(drawn_by(&stand_in, false) && !drawn_by(&stand_in, true));
        assert!(!drawn_by(&spot, false) && drawn_by(&spot, true));
        assert!(!drawn_by(
            &PointLight {
                intensity: 0.0,
                ..lamp
            },
            true
        ));
        // a map light reads as before in the vanilla shader (position, radius, colour)
        let g = gpu_light(&lamp, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(g.pos, [1.0, 2.0, 3.0, 30.0]);
        assert_eq!(g.color, [1.0, 0.78, 0.46, 1.0]);
        assert_eq!(g.dir[3], -2.0);
        assert_eq!(g.extra[1..], [5.0, 0.0, 30.0]);
        // a spot: no radius for the vanilla shader, the cone and the range for the enhanced
        let g = gpu_light(&spot, Vec3::ZERO);
        assert_eq!(g.pos[3], 0.0);
        assert_eq!(g.extra[3], 60.0);
        assert!(
            (Vec3::from_slice(&g.dir[..3]).length() - 1.0).abs() < 1e-5
                && g.dir[3] == 0.82
                && g.extra[0] == 0.97
        );
        // a headlight's beam gain rides along; a plain lamp has none
        assert_eq!(g.extra[2], 24.0);
        assert_eq!(gpu_light(&lamp, Vec3::ZERO).extra[2], 0.0);
    }

    #[test]
    fn metering_defaults_are_gentle() {
        // the light model's exposure leads: the metering corrects well under a stop either
        // way, so snow stays white and a night stays dark
        let m = meter_tuning();
        assert!(m[0] > 0.0 && m[0] < 0.6, "{m:?}");
        assert!(m[2] <= 0.75 && m[3] <= 1.0, "{m:?}");
        // a snow field metered 1.5 EV over the target is darkened by at most the cap
        let ev = ((m[1] - (m[1] + 1.5)) * m[0]).clamp(-m[2], m[3]);
        assert!(ev < 0.0 && ev >= -0.75, "{ev}");
    }

    #[test]
    fn half_floats_read_back() {
        for v in [0.0f32, 1.0, -2.5, 0.5, 1e-3, -3.46] {
            assert!(
                (half_to_f32(atmosphere::f16_bits(v)) - v).abs() <= v.abs() * 1e-3 + 1e-6,
                "{v}"
            );
        }
    }

    #[test]
    fn vehicle_box_contains_the_driver() {
        // an NL202: 2.5 x 12 x 3 m, the box centre 0.4 m ahead of the origin and 1.5 m up,
        // heading east; the driver sits 4.6 m ahead of the origin, 1.8 m up
        let bus = (
            DVec3::new(100.0, 200.0, 30.0),
            90.0,
            [2.5, 12.0, 3.0, 0.0, 0.4, 1.5],
        );
        assert!(point_in_vehicle_box(DVec3::new(104.6, 200.7, 31.8), &bus));
        // beside the bus, above it, behind it
        assert!(!point_in_vehicle_box(DVec3::new(104.6, 197.0, 31.8), &bus));
        assert!(!point_in_vehicle_box(DVec3::new(104.6, 200.0, 34.0), &bus));
        assert!(!point_in_vehicle_box(DVec3::new(93.0, 200.0, 31.0), &bus));
    }

    #[test]
    fn opaque_and_transmapped_materials_ignore_dynamic_alpha() {
        assert_eq!(
            Renderer::clamp_slot_alpha(0.0, AlphaMode::Opaque, false),
            1.0
        );
        assert_eq!(
            Renderer::clamp_slot_alpha(0.35, AlphaMode::Opaque, false),
            1.0
        );
        assert_eq!(Renderer::clamp_slot_alpha(0.0, AlphaMode::Test, false), 1.0);
        assert_eq!(
            Renderer::clamp_slot_alpha(0.35, AlphaMode::Test, false),
            1.0
        );
        assert_eq!(
            Renderer::clamp_slot_alpha(0.85, AlphaMode::Blend, false),
            0.85
        );
        assert_eq!(Renderer::clamp_slot_alpha(0.0, AlphaMode::Blend, true), 1.0);
        assert_eq!(
            Renderer::clamp_slot_alpha(0.0, AlphaMode::Opaque, true),
            1.0
        );
        assert_eq!(Renderer::clamp_slot_alpha(0.0, AlphaMode::Test, true), 1.0);
    }
}

/// A texture bigger than the graphics chip takes (`max` texels a side - 16384 on most, 2048
/// or 4096 on older ones) made to fit: its smaller levels when it has them, else the picture
/// halved until it fits. None when it fits as it is. A too big texture used to be a device
/// error, and the material that used it one too.
pub fn fit_texture(data: &omsi_texture::TextureData, max: u32) -> Option<omsi_texture::TextureData> {
    use omsi_texture::PixelFormat;
    let (w, h) = (data.width.max(1), data.height.max(1));
    if w <= max && h <= max {
        return None;
    }
    let mut k = 0u32;
    while (w >> k).max(1) > max || (h >> k).max(1) > max {
        k += 1;
    }
    if (k as usize) < data.levels.len() {
        return Some(omsi_texture::TextureData { width: (w >> k).max(1), height: (h >> k).max(1), levels: data.levels[k as usize..].to_vec(), ..data.clone() });
    }
    // one level only: decode it and halve it
    let mut rgba = match (data.format, data.levels.first()) {
        (PixelFormat::Rgba8, Some(l)) => l.clone(),
        (f, Some(l)) => omsi_texture::bc::decode(l, w, h, match f {
            PixelFormat::Bc1 => omsi_texture::bc::Bc::Bc1 { punch: true },
            PixelFormat::Bc2 => omsi_texture::bc::Bc::Bc2,
            _ => omsi_texture::bc::Bc::Bc3,
        }),
        _ => return None,
    };
    let (mut cw, mut ch) = (w, h);
    for _ in 0..k {
        let (nw, nh) = ((cw / 2).max(1), (ch / 2).max(1));
        let mut next = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            for x in 0..nw {
                for c in 0..4 {
                    let at = |xx: u32, yy: u32| rgba[((yy.min(ch - 1) * cw + xx.min(cw - 1)) * 4 + c) as usize] as u32;
                    let v = at(2 * x, 2 * y) + at(2 * x + 1, 2 * y) + at(2 * x, 2 * y + 1) + at(2 * x + 1, 2 * y + 1);
                    next[((y * nw + x) * 4 + c) as usize] = (v / 4) as u8;
                }
            }
        }
        rgba = next;
        (cw, ch) = (nw, nh);
    }
    Some(omsi_texture::TextureData { width: cw, height: ch, format: PixelFormat::Rgba8, levels: vec![rgba], has_alpha: data.has_alpha, gpu_mips: true })
}

/// A size halved (both sides, as `fit_texture` halves a picture) until neither passes `max`.
fn fit_size(w: u32, h: u32, max: u32) -> (u32, u32) {
    let (mut w, mut h) = (w, h);
    while w > max || h > max {
        (w, h) = ((w / 2).max(1), (h / 2).max(1));
    }
    (w, h)
}

/// An RGBA picture of the game's own (a script's or a sign's text, an HTML display) larger
/// than the graphics chip takes, halved until it fits (None when it fits as it is). Made at
/// its full size, the texture was a device error, and so was every frame's write into it.
fn fit_image(img: &omsi_texture::Image, max: u32) -> Option<omsi_texture::Image> {
    if img.width <= max && img.height <= max {
        return None;
    }
    let data = omsi_texture::TextureData {
        width: img.width,
        height: img.height,
        format: omsi_texture::PixelFormat::Rgba8,
        levels: vec![img.rgba.clone()],
        has_alpha: img.has_alpha,
        gpu_mips: true,
    };
    let small = fit_texture(&data, max)?;
    Some(omsi_texture::Image { width: small.width, height: small.height, rgba: small.levels.into_iter().next().unwrap_or_default(), has_alpha: img.has_alpha })
}

#[cfg(test)]
mod fit_tests {
    #[test]
    fn a_big_picture_is_halved_until_it_fits() {
        let data = omsi_texture::TextureData { width: 8, height: 4, format: omsi_texture::PixelFormat::Rgba8, levels: vec![vec![200; 8 * 4 * 4]], has_alpha: false, gpu_mips: true };
        let small = super::fit_texture(&data, 2).unwrap();
        assert_eq!((small.width, small.height), (2, 1));
        assert_eq!(small.levels[0].len(), 2 * 4);
        assert!(super::fit_texture(&data, 8).is_none());
        // a picture of the game's own, and a size, the same way
        let img = omsi_texture::Image { width: 5000, height: 300, rgba: vec![9; 5000 * 300 * 4], has_alpha: true };
        let small = super::fit_image(&img, 2048).unwrap();
        assert_eq!((small.width, small.height), (1250, 75));
        assert_eq!(small.rgba.len(), 1250 * 75 * 4);
        assert_eq!(super::fit_size(5000, 300, 2048), (1250, 75));
        assert_eq!(super::fit_size(2048, 16, 2048), (2048, 16));
        assert!(super::fit_image(&small, 2048).is_none());
    }
}

thread_local! {
    /// A panic on this thread now is caught and handled (see [`catching`]).
    static CATCHING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a panic on this thread is being caught by the renderer (the game's panic hook
/// does not report it as the end of the game).
pub fn catching() -> bool {
    CATCHING.with(|c| c.get())
}

/// Run `f`; a panic in it (a driver wgpu cannot use, taking it down in a way it does not
/// turn into an error) is caught and gives None, and is not reported as the end of the game.
pub fn catch<R>(f: impl FnOnce() -> R) -> Option<R> {
    let was = CATCHING.with(|c| c.replace(true));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    CATCHING.with(|c| c.set(was));
    r.ok()
}
