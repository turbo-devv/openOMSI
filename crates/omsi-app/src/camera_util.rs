//! Camera helpers: the default and follow cameras, picking rays, the orbit distance, and the mirrors.

use super::*;

/// Default camera from `[mapcam]`: tile x, tile y, x, z, y, yaw, pitch, distance.
pub(crate) fn default_camera(world: &World) -> Camera {
    let mc = &world.global.map_cam;
    if mc.len() >= 8 {
        let tx = mc[0];
        let ty = mc[1];
        let x = tx * omsi_map::tile_size() + mc[2];
        let z = mc[3];
        let y = ty * omsi_map::tile_size() + mc[4];
        let yaw = mc[5] as f32;
        let pitch = mc[6] as f32;
        let dist = mc[7] as f32;
        let mut cam = Camera {
            position: DVec3::new(x, y, z),
            yaw,
            pitch,
            roll: 0.0,
            fov_deg: 60.0,
            near: 0.1,
            far: 6000.0,
        };
        // the editor camera orbits the point at `dist`; step back along the view direction
        cam.position -= (cam.forward() * dist).as_dvec3();
        cam.position.z = cam.position.z.max(z + 5.0);
        cam
    } else {
        Camera {
            position: DVec3::new(150.0, 150.0, 60.0),
            yaw: 0.0,
            pitch: -20.0,
            roll: 0.0,
            fov_deg: 60.0,
            near: 0.1,
            far: 6000.0,
        }
    }
}

/// Can the ray from `o` along `dir` (vehicle frame) come near mesh `i` of the type, posed
/// by `xf`? `widen` is the extra angle (radians) of the rays spread around it. The hover
/// test cast a ray at every triangle of the whole bus each frame - 0.7 ms of the main
/// thread on a fast machine, three times that on a slow one - while the mouse was over
/// nothing; a sphere per mesh passes most of them by.
pub(crate) fn ray_may_hit(
    ty: &omsi_sim::VehicleType,
    i: usize,
    xf: &glam::Mat4,
    o: Vec3,
    dir: Vec3,
    widen: f32,
) -> bool {
    let Some(&(c, r)) = ty.mesh_bounds.get(i) else {
        return true;
    };
    if r <= 0.0 {
        return true;
    }
    let scale = xf
        .x_axis
        .truncate()
        .length()
        .max(xf.y_axis.truncate().length())
        .max(xf.z_axis.truncate().length());
    let c = xf.transform_point3(c);
    let r = r * scale + (c - o).length() * widen + 0.01;
    omsi_geometry::ray_near_sphere(o, dir.normalize_or_zero(), c, r)
}

/// How far the outside camera sits from the bus: at the start, and the nearest and
/// farthest the wheel or +/- take it. The old 18 m default with a 200 m maximum was a
/// helicopter view; a bus is 11 m long and reads best from ten.
pub(crate) const ORBIT_DEFAULT: f32 = 10.0;

pub(crate) const ORBIT_MIN: f32 = 3.5;

pub(crate) const ORBIT_MAX: f32 = 40.0;

/// `--look yaw,pitch`: how far the head is turned in an offscreen run.
pub(crate) fn look_of(args: &Args) -> (f32, f32) {
    args.look
        .as_deref()
        .map(|s| {
            let v: Vec<f32> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            (
                v.first().copied().unwrap_or(0.0),
                v.get(1).copied().unwrap_or(0.0),
            )
        })
        .unwrap_or((0.0, 0.0))
}

/// How far the offscreen outside camera sits from the vehicle: 18 m, or `OMSI_ORBIT_DIST`
/// metres for close-ups (a headlight, a door) together with `--look`.
pub(crate) fn offscreen_orbit() -> f32 {
    omsi_cfg::env::var("OMSI_ORBIT_DIST")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(18.0)
}

/// The angle one pixel subtends vertically, so that a pick tolerance can be stated in
/// pixels and mean the same at any window size or field of view.
pub(crate) fn pixel_angle(cam: &Camera, height: f32) -> f32 {
    2.0 * (cam.fov_deg.to_radians() * 0.5).tan() / height.max(1.0)
}

/// World-space ray through a window pixel: (camera position, unit direction).
pub(crate) fn cursor_ray(cam: &Camera, x: f32, y: f32, w: f32, h: f32) -> (DVec3, Vec3) {
    let (_, d) = cam.ray(
        x / w * 2.0 - 1.0,
        1.0 - y / h * 2.0,
        w / h.max(1.0),
        cam.position,
    );
    (cam.position, d)
}

/// The AI car to follow: an id, "auto" = the first overtaker, "moving" = the oldest car
/// driving when first asked, "turn" = the first car entering a turning lane, "steer" = the
/// car steering hardest, "bus" = the first timetable bus, "type:<name>" = the first vehicle
/// whose file name contains it.
pub(crate) fn follow_id(args: &Args, traffic: Option<&traffic::Traffic>) -> Option<u64> {
    match args.follow.as_deref()? {
        "auto" => traffic?.last_overtaker.map(|o| o.0),
        // the oldest car that is driving when first asked, kept while it exists
        "moving" => {
            thread_local!(static CHOSEN: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) });
            let t = traffic?;
            if let Some(id) = CHOSEN
                .with(|c| c.get())
                .filter(|id| t.car_pose(*id).is_some())
            {
                return Some(id);
            }
            let id = t
                .cars
                .iter()
                .filter(|c| c.state.speed > 3.0)
                .map(|c| c.id)
                .min()?;
            CHOSEN.with(|c| c.set(Some(id)));
            Some(id)
        }
        // the oldest timetable bus that is under way when first asked, kept while it exists
        "bus" => {
            thread_local!(static BUS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) });
            let t = traffic?;
            if let Some(id) = BUS.with(|c| c.get()).filter(|id| t.car_pose(*id).is_some()) {
                return Some(id);
            }
            let id = t.cars.iter().filter(|c| c.is_bus() && c.state.speed > 2.0).map(|c| c.id).min()?;
            BUS.with(|c| c.set(Some(id)));
            Some(id)
        }
        "turn" => traffic?.first_turner.map(|o| o.0),
        // the first car stopped by a red light, giving way at a junction without lights,
        // pulling out onto the other half of the road round an obstacle
        "red" => traffic?.first_red.map(|o| o.0),
        "yield" => traffic?.first_yield.map(|o| o.0),
        "pass" => traffic?.first_passer.map(|o| o.0),
        // whichever moving car steers hardest at this moment (wheel close-ups)
        "steer" => traffic?
            .cars
            .iter()
            .filter(|c| c.state.speed > 1.0 && !c.is_bus())
            .max_by(|a, b| a.body.steer.abs().total_cmp(&b.body.steer.abs()))
            .map(|c| c.id),
        v if v.starts_with("type:") => {
            let want = v[5..].to_ascii_lowercase();
            traffic?
                .cars
                .iter()
                .find(|c| {
                    c.vehicle
                        .ty
                        .def
                        .path
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains(&want)
                })
                .map(|c| c.id)
        }
        v => v.parse().ok(),
    }
}

/// Chase camera behind AI car `id`: from high above by default, or where
/// `OMSI_FOLLOW_CAM=right,forward,up,yaw,pitch` puts it in the car's frame (metres, and
/// degrees relative to the car's heading); a sixth value 1 reads the offset as east, north,
/// up and the yaw as a compass heading, so the view does not turn with the car.
pub(crate) fn follow_camera(traffic: Option<&traffic::Traffic>, id: u64) -> Option<Camera> {
    let (pos, heading) = traffic?.car_pose(id)?;
    let h = (heading as f32).to_radians();
    let back = DVec3::new(-(h.sin() as f64), -(h.cos() as f64), 0.0);
    // OMSI_FOLLOW_CAM=right,ahead,up,yaw,pitch: a camera in the car's own frame (metres,
    // degrees relative to its heading), e.g. beside the road with the car at the edge of
    // the picture; `yaw` may be a list a/b/c, one per snapshot (--snapshots); a sixth value
    // 1 reads the offset as east, north, up and the yaw as a compass heading, so the view
    // does not turn with the car
    if let Ok(v) = omsi_cfg::env::var("OMSI_FOLLOW_CAM") {
        thread_local!(static CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });
        let mut fields: Vec<String> = v.split(',').map(|x| x.trim().to_string()).collect();
        if let Some(yaws) = fields.get_mut(3).filter(|y| y.contains('/')) {
            let list: Vec<String> = yaws.split('/').map(|s| s.to_string()).collect();
            let k = CALLS.with(|c| c.replace(c.get() + 1));
            *yaws = list[k.min(list.len() - 1)].clone();
        }
        let f: Vec<f64> = fields.iter().filter_map(|x| x.parse().ok()).collect();
        if f.len() == 6 && f[5] != 0.0 {
            let position = pos + DVec3::new(f[0], f[1], f[2]);
            return Some(Camera {
                position,
                yaw: f[3] as f32,
                pitch: f[4] as f32,
                roll: 0.0,
                fov_deg: 60.0,
                near: 0.1,
                far: 6000.0,
            });
        }
        if f.len() >= 5 {
            let right = DVec3::new(h.cos() as f64, -(h.sin() as f64), 0.0);
            return Some(Camera {
                position: pos + right * f[0] - back * f[1] + DVec3::new(0.0, 0.0, f[2]),
                yaw: heading as f32 + f[3] as f32,
                pitch: f[4] as f32,
                roll: 0.0,
                fov_deg: 60.0,
                near: 0.1,
                far: 6000.0,
            });
        }
    }
    Some(Camera {
        position: pos + back * 6.0 + DVec3::new(0.0, 0.0, 45.0),
        yaw: heading as f32,
        pitch: -80.0,
        roll: 0.0,
        fov_deg: 60.0,
        near: 0.1,
        far: 6000.0,
    })
}

/// How much of the plain light a mirror loses at full night in enhanced graphics (see
/// `render_mirrors`): measured in the window at 23:30, the mirrors' street, the bus's own
/// flank and the sky then match the enhanced picture beside them (0.45 left them 2-3 times
/// as bright).
const MIRROR_NIGHT_DIM: f32 = 0.8;

/// The least radius a mirror's camera counts with in the visibility test (m). A mirror's
/// camera sits in the middle of its glass; with the file's radius 0 (`[add_camera_reflexion]`)
/// a mirror whose lower half shows at the edge of the picture - the SD200's right mirror
/// from the driver's seat - was never redrawn and stood frozen.
const MIRROR_MIN_RADIUS: f32 = 0.3;

/// Whether a mirror is in the picture of `view` (camera and aspect): its camera's place as
/// a sphere of the `[add_camera_reflexion_2]` radius (a point for `[add_camera_reflexion]`)
/// against the view's frustum, as Omsi.exe tests it (0x6f611f, 0x7f41ac) before it redraws
/// a mirror.
fn mirror_in_view(eye: DVec3, radius: f32, view: &(Camera, f32)) -> bool {
    let (cam, aspect) = view;
    let d = (eye - cam.position).as_vec3();
    let f = cam.forward();
    let (r, u) = (cam.right(), cam.up());
    let z = d.dot(f);
    if z < -radius {
        return false;
    }
    let tan_y = (cam.fov_deg.to_radians() * 0.5).tan();
    let tan_x = tan_y * aspect;
    let slack_x = radius * (1.0 + tan_x * tan_x).sqrt();
    let slack_y = radius * (1.0 + tan_y * tan_y).sqrt();
    d.dot(r).abs() <= z.max(0.0) * tan_x + slack_x && d.dot(u).abs() <= z.max(0.0) * tan_y + slack_y
}

/// A mirror's picture is drawn this wide over its height (Omsi.exe sub_6f6468 gives the
/// reflection cameras' projection 1.6 on their square textures), and the mirror meshes show
/// its middle: drawn square, a mirror showed a slice two thirds as wide as OMSI's - the
/// strip of the bus at its edge and hardly any of the lane beside it.
const MIRROR_ASPECT: f32 = 1.6;

/// A mirror's camera as Omsi.exe aims it, every frame (0x6f6468 -> 0x7ed0e8): the yaw and
/// pitch of an `[add_camera_reflexion]` are not where it looks but which way the mirror's
/// face is turned - (cos p sin y, cos p cos y, sin p) in the vehicle's frame (0x7edfd0); the
/// ray from the eye of the view being drawn to the mirror is reflected in that face, and the
/// camera looks along the reflection (yaw 90 - atan2(forward, right) and its elevation) from
/// the mirror - what a mirror shows whoever looks into it. `off` is the player's own turn of
/// the mirror (Ctrl+Alt+arrows), which turns the face. (The yaw was turned round instead,
/// which happened to fit a mirror facing straight back at the driver and no other: the left
/// mirrors, the kerb-side blind-spot mirrors and the door monitors of many buses looked into
/// the saloon or at the sky.)
pub(crate) fn mirror_view(v: &omsi_sim::VehicleInstance, c: &omsi_vehicle::Camera, eye: DVec3, off: [f32; 2]) -> omsi_vehicle::Camera {
    let rot = v.body_rotation();
    let at = v.position + rot.transform_point3(glam::Vec3::new(c.pos[0], c.pos[1], c.pos[2])).as_dvec3();
    let d = rot.inverse().transform_vector3((at - eye).as_vec3());
    let Some(d) = d.try_normalize() else { return c.clone() };
    let (y, p) = ((c.yaw + off[0]).to_radians(), (c.pitch + off[1]).to_radians());
    let m = glam::Vec3::new(p.cos() * y.sin(), p.cos() * y.cos(), p.sin());
    let r = d - m * (2.0 * d.dot(m));
    omsi_vehicle::Camera { yaw: r.x.atan2(r.y).to_degrees(), pitch: r.z.clamp(-1.0, 1.0).asin().to_degrees(), ..c.clone() }
}

/// A mirror camera as the player has set it: moved by a shift (bus frame, m) and with
/// `dfov` degrees added to its field of view (a mirror that gives none draws with 50).
pub(crate) fn adjusted(c: &omsi_vehicle::Camera, s: [f32; 3], dfov: f32) -> omsi_vehicle::Camera {
    let fov = if dfov.abs() > 0.01 { (if c.fov > 1.0 { c.fov } else { 50.0 } + dfov).clamp(8.0, 110.0) } else { c.fov };
    omsi_vehicle::Camera { pos: [c.pos[0] + s[0], c.pos[1] + s[1], c.pos[2] + s[2]], fov, ..c.clone() }
}

/// Where the driver's eye is (for a mirror drawn with no view to aim it by).
pub(crate) fn driver_eye(p: &Player) -> DVec3 {
    if let Some((t, c)) = p.trailer_driver_camera() {
        return t.camera_world_full(c).0;
    }
    let def = &p.vehicle.ty.def;
    let n = def.cameras_driver.len().max(1);
    match def.cameras_driver.get((def.camera_std + p.cam_choice.0) % n) {
        Some(c) => p.vehicle.camera_world(c).0 + p.vehicle.body_rotation().transform_vector3(p.head_offset() + p.seat).as_dvec3(),
        None => p.vehicle.position + DVec3::Z * 2.0,
    }
}

/// Draw the views of the vehicle's `[add_camera_reflexion]` cameras into its mirror textures.
/// `only`: render just one mirror, the `i % n`-th of those `view` sees (round robin, as
/// OMSI takes its turns among the mirrors in the picture; with no view, of all of them).
pub(crate) fn render_mirrors(
    renderer: &mut Renderer,
    scene: &mut Scene,
    world: &scene::World,
    p: &Player,
    lighting: &omsi_render::Lighting,
    only: Option<usize>,
    view: Option<(Camera, f32)>,
) -> usize {
    // (aimed from the eye of the view being drawn, as Omsi.exe aims them - from the
    // driver's without one)
    let eye = view.as_ref().map(|v| v.0.position).unwrap_or_else(|| driver_eye(p));
    let cams: Vec<omsi_vehicle::Camera> = p
        .vehicle
        .ty
        .def
        .cameras_reflexion
        .iter()
        .enumerate()
        .map(|(i, c)| mirror_view(&p.vehicle, &adjusted(c, p.mirror_shifts.get(i).copied().unwrap_or([0.0; 3]), p.mirror_fovs.get(i).copied().unwrap_or(0.0)), eye, p.mirror_offsets.get(i).copied().unwrap_or([0.0; 2])))
        .collect();
    if cams.is_empty() {
        return 0;
    }
    let textures = world.mirror_textures.lock().clone();
    // small images: skip objects that would be tiny anyway (the original's
    // performance_minObjSizeRefl)
    let mut lighting = lighting.clone();
    // a mirror's small picture: nothing smaller than a few of its pixels, and nothing much
    // beyond what a mirror shows (the far distance below)
    let px = crate::MIRROR_SIZE.load(std::sync::atomic::Ordering::Relaxed).max(64) as f32;
    lighting.min_obj_size = lighting.min_obj_size.max((12.0 / px).clamp(0.03, 0.09));
    lighting.shadows = false;
    // Enhanced graphics draw the mirrors with the plain shading (see Renderer::render_inner),
    // whose light is OMSI's: a night that stays a blue dusk. The window's enhanced night is
    // far darker, and the mirrors showed the street by daylight beside it. The plain light
    // is taken down with the night (the lamps keep theirs) to the enhanced picture's level.
    // (By the sun's darkness, Envir_Brightness's ramp from +6 to -6 degrees: `night` is
    // whole at sunset already, from +10 degrees on, and rain raises it by day, and the
    // mirrors were a fifth of the window's light through the whole dusk, #432.)
    if lighting.enhanced && omsi_cfg::env::var_os("OMSI_MIRROR_ENHANCED").is_none() {
        let alt = lighting.sun_dir.z.clamp(-1.0, 1.0).asin().to_degrees();
        let dark = 1.0 - ((alt + 6.0) / 12.0).clamp(0.0, 1.0);
        let k = 1.0 - MIRROR_NIGHT_DIM * dark;
        lighting.ambient *= k;
        lighting.secondary *= k;
        lighting.sun_color *= k;
        lighting.sky_color *= k;
        lighting.fog_color *= k;
        // (the plain sky dome is drawn from its day/twilight/night pictures by these
        // weights, not by the light: the mirrors showed a sky three times the window's)
        for w in lighting.sky_weights.iter_mut() {
            *w *= k;
        }
    }
    // the mirrors in the picture (all of them without a view); none in it, none redrawn
    let seen: Vec<usize> = (0..cams.len())
        .filter(|&i| view.as_ref().map(|v| mirror_in_view(p.vehicle.camera_world_full(&cams[i]).0, cams[i].extra.unwrap_or(0.0).max(MIRROR_MIN_RADIUS), v)).unwrap_or(true))
        .collect();
    if seen.is_empty() {
        return 0;
    }
    let pick = only.map(|k| seen[k % seen.len()]);
    for (i, c) in cams.iter().enumerate() {
        if !seen.contains(&i) || pick.is_some_and(|k| k != i) {
            continue;
        }
        let Some(Some(tex)) = textures.get(i) else {
            continue;
        };
        let (eye, yaw, pitch, roll) = p.vehicle.camera_world_full(c);
        let pitch = pitch.clamp(-89.0, 89.0);
        if omsi_cfg::env::var_os("OMSI_DEBUG_MIRRORS").is_some() {
            log::info!("mirror {i}: eye {:.2},{:.2},{:.2} yaw {yaw:.1} pitch {pitch:.1} roll {roll:.2} fov {:.0} ({} of {} in view)", eye.x, eye.y, eye.z, c.fov, seen.len(), cams.len());
        }
        let cam = Camera {
            position: eye,
            yaw,
            pitch,
            roll,
            fov_deg: if c.fov > 1.0 { c.fov } else { 50.0 },
            // (Omsi.exe's reflection pass, 0x6f68a8: near 0.1 m, far 100 km. Ours cut at
            // 0.3 m and 450 m: what is close to an inside mirror's camera - a handrail, the
            // driver's head - vanished, and the street behind ended at the next junction.
            // The far end is the objects' own reach here, as in the main view.)
            near: 0.1,
            far: if renderer.options.max_obj_dist > 0.0 { renderer.options.max_obj_dist.clamp(450.0, 6000.0) } else { 3000.0 },
        };
        renderer.render_to_texture(scene, *tex, &cam, &lighting, MIRROR_ASPECT);
    }
    seen.len()
}
