//! Putting the player's bus into the world: its file, coupled parts, paint scheme and place.

use super::*;

/// The vehicle file to spawn for `--bus`. The rear section of an articulated bus is never
/// driven on its own (OMSI does not even list it): asked for one, the bus it belongs to is
/// spawned, and a rear section that nothing couples is refused.
pub(crate) fn player_bus_path(root: &Path, bus: &str) -> Result<PathBuf> {
    let path = omsi_cfg::resolve_path(root, bus);
    let def = omsi_vehicle::Vehicle::load(&path)
        .with_context(|| format!("loading {}", path.display()))?;
    // a part with a front coupling that another vehicle couples behind itself is a rear
    // section, whether or not the mod gave it a [friendlyname] of its own
    let fronts = if def.coupling_front.is_some() {
        omsi_vehicle::vehicle::front_sections_of(&path)
    } else {
        Vec::new()
    };
    // (a rail vehicle couples at both ends by nature: a locomotive or a tram is driven as
    // it is)
    if (fronts.is_empty() && !def.is_rear_section()) || crate::rail_drive::is_rail(&def) {
        return Ok(path);
    }
    match fronts.first() {
        Some(front) => {
            log::warn!("{} is the rear section of {}: spawning the whole bus", path.display(), front.display());
            Ok(front.clone())
        }
        None => anyhow::bail!("{} is the rear section of an articulated bus and cannot be driven on its own, and no vehicle in its folder couples it ([couple_back]); choose the front section instead", path.display()),
    }
}

/// The next vehicle of a consist from one with definition `def` (turned round: `rev`),
/// towards the back of the train or its front, and whether that one is turned round - as
/// Omsi.exe builds a consist (0x70a174): towards the back a vehicle goes on with its
/// `[couple_back]`, or with its `[couple_front]` when it is itself turned round (towards the
/// front the other way about); the coupled one is turned round when the coupling says so,
/// against the one it hangs on; a coupling back to its own file that turns nothing round
/// is not followed. (Following `[couple_back]` whatever the way, the Berlin A3's unit - the
/// S car and its K car turned round, whose `[couple_back]` names the S car - went on S, K,
/// S, K, S, none of them turned.)
pub(crate) fn next_coupled(def: &omsi_vehicle::vehicle::Vehicle, rev: bool, toward_back: bool) -> Option<(PathBuf, bool)> {
    let (file, flag) = if toward_back != rev { def.couple_back.as_ref() } else { def.couple_front.as_ref() }?;
    let path = omsi_cfg::resolve_path(def.dir(), file);
    let same = path.file_name().map(|f| f.to_ascii_lowercase()) == def.path.file_name().map(|f| f.to_ascii_lowercase());
    if !flag && same {
        return None;
    }
    Some((path, flag ^ rev))
}

/// Load the `[couple_back]` chain behind `vehicle` (articulated rear sections, trailers),
/// each file resolved from the folder of the part before it in whichever content root
/// holds it.
pub(crate) fn load_coupled_parts(
    root: &Path,
    vehicle: &mut omsi_sim::VehicleInstance,
) -> Vec<Arc<omsi_sim::VehicleType>> {
    let mut parts = Vec::new();
    let mut lead = vehicle.ty.clone();
    let mut lead_rev = false;
    for _ in 0..8 {
        let Some((path, reversed)) = next_coupled(&lead.def, lead_rev, true) else {
            break;
        };
        match omsi_sim::VehicleType::load(root, &path) {
            Ok(t) => {
                let t = Arc::new(t);
                log::info!(
                    "coupled behind {}: {} ({} meshes{})",
                    lead.def
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    path.display(),
                    t.meshes.len(),
                    if reversed { ", reversed" } else { "" }
                );
                vehicle.attach_trailer_ex(t.clone(), reversed);
                parts.push(t.clone());
                lead = t;
                lead_rev = reversed;
            }
            Err(e) => {
                log::warn!(
                    "coupled part {} of {}: {e:#}",
                    path.display(),
                    lead.def.path.display()
                );
                break;
            }
        }
    }
    parts
}

/// Choose the player's fleet number and registration before the script VM runs `{init}`.
/// Manual plates win, free registrations come from the map, otherwise the selected number
/// is paired with the bus's registration rules.
fn player_identity(
    args: &Args,
    world: &World,
    vt: &omsi_sim::VehicleType,
) -> (Option<String>, Option<String>) {
    let available = vt.def.numbers_with_plates();
    let saved_number = args
        .situation_strvars
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("number"))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty());
    let wanted = args
        .number
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or(saved_number);

    let number = match wanted {
        Some(w) if available.is_empty() => Some(w.to_string()),
        Some(w) => match available.iter().find(|(n, _)| n.trim() == w) {
            Some((n, _)) => Some(n.clone()),
            None => {
                log::warn!("--number {w}: not in the bus's [number] list, the first one is taken");
                available.first().map(|(n, _)| n.clone())
            }
        },
        None => available.first().map(|(n, _)| n.clone()),
    };

    let saved_ident = args
        .situation_strvars
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("ident"))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty());
    let typed = args.plate.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let ident = if let Some(p) = typed {
        Some(p.to_string())
    } else if let Some(p) = saved_ident {
        Some(p.to_string())
    } else if vt.def.registration_free {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        world.free_registration(seed)
    } else {
        number
            .as_deref()
            .map(|n| vt.def.chosen_plate_of_number(n))
            .filter(|p| !p.trim().is_empty())
    };

    (number, ident)
}

pub(crate) fn spawn_player(
    args: &Args,
    world: &World,
    renderer: &Renderer,
    scene: &mut Scene,
) -> Result<Option<Player>> {
    let Some(bus) = &args.bus else {
        return Ok(None);
    };
    let path = player_bus_path(&args.root, bus)?;
    let vt = Arc::new(omsi_sim::VehicleType::load(&args.root, &path)?);
    log::info!(
        "vehicle {} {}: {} meshes, {} script blocks, {} variables",
        vt.def.manufacturer,
        vt.def.type_name,
        vt.meshes.len(),
        vt.program.blocks.len(),
        vt.program.var_names.len()
    );
    let doors = crate::player::door_keys(&vt);
    if !doors.is_empty() {
        let keys: Vec<String> = doors.iter().enumerate().map(|(i, g)| format!("Shift+{} = {}", i + 1, g.join(" + "))).collect();
        log::info!("door keys: {}", keys.join(", "));
    }
    let mut host = omsi_sim::VehicleHost::new(start_clock(args));
    // the maintenance condition of the options (AI vehicles never wear)
    host.wear_lifespan = crate::settings::Settings::load().wear_lifespan();
    host.hof = find_hof(args, world, &vt);
    host.font_lib = Some(world.fonts.clone());
    if !world.ticket_pack.trim().is_empty() {
        let p = omsi_cfg::resolve_path(&args.root, &world.ticket_pack);
        match omsi_content::tickets::TicketPack::load(&p) {
            Ok(t) => {
                log::info!("ticket pack {}: {} tickets", p.display(), t.tickets.len());
                host.tickets = Some(std::sync::Arc::new(t));
            }
            Err(e) => log::warn!("{e}"),
        }
    }
    if let Some(h) = &host.hof {
        log::info!(
            "depot file {}: {} termini, {} bus stops, {} IBIS trips",
            h.name,
            h.termini.len(),
            h.bus_stops.len(),
            h.info_trips.len()
        );
    }
    // number / ident are there before the scripts' first {init} instruction
    let (number, ident) = player_identity(args, world, &vt);
    host.initial_number = number;
    host.initial_ident = ident;
    // (the paint scheme's variables are there for the scripts' {init})
    host.paint_scheme = Some(paint_scheme(&vt, args.paint.as_deref()));
    let mut vehicle = omsi_sim::VehicleInstance::new(vt.clone(), host);
    // place at the entry point
    if let Some(ep) = world
        .global
        .entry_points
        .get(args.entry)
        .or(world.global.entry_points.first())
    {
        let found = world.entry_point_place(ep);
        match found {
            Some((pos, rot)) => {
                vehicle.position = pos;
                // the height the map's editor recorded for the entry point (global.cfg), when
                // it is at the same place but on another level: an entry point under a bridge
                // whose object came out on the deck put the bus on the bridge
                if let Some(rec) = recorded_entry_pos(ep, pos) {
                    let off = ((rec.x - pos.x).powi(2) + (rec.y - pos.y).powi(2)).sqrt();
                    if off < 3.0 && (rec.z - pos.z).abs() > 1.5 {
                        log::info!("entry point {}: the object stands at height {:.1}, the map recorded {:.1}: the recorded one", ep.index, pos.z, rec.z);
                        vehicle.position.z = rec.z;
                    }
                }
                let pos = vehicle.position;
                // on the road surface there, not at the marker's own height: a marker
                // placed on the terrain a little under (or over) the road left one axle in
                // the asphalt and the bus stood tilted from the start; a surface metres away
                // (a bridge over the place, a lower level) is not this one
                if let Some(g) = world.stand_height(pos.x, pos.y, pos.z) {
                    vehicle.position.z = g;
                } else if crate::scene::drive_probe(&world.terrains, &world.surfaces, pos.x, pos.y, pos.z + 1.5).below.is_none() {
                    // nothing under the place at all (the marker came out under the ground):
                    // on the ground above, not in the void under the map
                    if let Some(g) = world.walk_height(pos.x, pos.y) {
                        log::info!("entry point {}: nothing under its height {:.1}; put on the ground at {:.1}", ep.index, pos.z, g);
                        vehicle.position.z = g;
                    }
                }
                vehicle.heading = rot[0];
                log::info!(
                    "spawned at entry point {} \"{}\" ({:.1}, {:.1}, {:.1}) heading {:.0}",
                    ep.index,
                    ep.name,
                    pos.x,
                    pos.y,
                    pos.z,
                    rot[0]
                );
            }
            None => log::warn!(
                "entry point object {} not loaded; vehicle stays at origin",
                ep.object_id
            ),
        }
    }
    let scheme = paint_scheme(&vt, args.paint.as_deref());
    match scheme {
        Some(i) => log::info!(
            "paint scheme {} '{}' (asked for {:?})",
            i,
            vt.paint_schemes[i].name,
            args.paint
        ),
        None => log::info!("paint: the model's own textures"),
    }
    vehicle.apply_paint_vars(scheme);
    log::info!("gearbox: {}", if vehicle.ty.program.manual_gearbox() { "manual (gates)" } else { "automatic or none" });
    if let Some(sp) = &args.spawn {
        let v: Vec<f64> = sp
            .split(',')
            .filter_map(|x| x.trim().parse().ok())
            .collect();
        if v.len() >= 3 {
            // x,y,heading[,z]: a height given is the road's (the ground may lie below it)
            let z = match v.get(3) {
                Some(&road) => world.stand_height(v[0], v[1], road).map_or(road, |g| road.max(g)),
                None => world.ground_height(v[0], v[1]).unwrap_or(0.0),
            };
            vehicle.position = DVec3::new(v[0], v[1], z);
            vehicle.heading = v[2];
        }
    }
    // a parked bus or car standing where the player's bus is put (a depot's entry point):
    // it drives off instead of the player's bus standing inside it
    if let Some(bb) = vt.def.bounding_box {
        let n = world.clear_parked_under(renderer, scene, &omsi_sim::collision::Obb::from_box(bb, vehicle.position, vehicle.heading));
        if n > 0 {
            log::info!("spawn: {n} parked vehicle(s) cleared from the place of the bus");
        }
        world.clear_props_under(renderer, scene, &omsi_sim::collision::Obb::from_box(bb, vehicle.position, vehicle.heading));
    }
    let render = world.add_vehicle(renderer, scene, &vt, scheme);
    // coupled rear sections / trailers
    let trailer_renders: Vec<scene::VehicleRender> = load_coupled_parts(&args.root, &mut vehicle)
        .iter()
        .map(|t| {
            world.add_vehicle_part(
                renderer,
                scene,
                t,
                scheme.filter(|i| *i < t.paint_schemes.len()),
                &render,
            )
        })
        .collect();
    // fonts for [texttexture]
    vehicle.init_text_textures(&mut world.fonts.lock(), &|p| {
        omsi_texture::decode_file(p)
            .ok()
            .map(|i| (i.width, i.height, i.rgba))
    });
    for t in vehicle.trailers.iter_mut() {
        t.init_text_textures(&mut world.fonts.lock(), &|p| {
            omsi_texture::decode_file(p)
                .ok()
                .map(|i| (i.width, i.height, i.rgba))
        });
    }
    // number / ident were installed before {init}; do not rewrite them here.
    // ground following through the loaded tiles (road surfaces first, then terrain)
    let terrains = world.terrains.clone();
    let surfaces = world.surfaces.clone();
    vehicle.ground = Some(std::sync::Arc::new(move |x, y| {
        let tx = (x / omsi_map::tile_size()).floor() as i32;
        let ty = (y / omsi_map::tile_size()).floor() as i32;
        let lx = (x - tx as f64 * omsi_map::tile_size()) as f32;
        let ly = (y - ty as f64 * omsi_map::tile_size()) as f32;
        if let Some(s) = surfaces.read().get(&(tx, ty)) {
            if let Some(h) = s.sample(lx, ly) {
                return Some(h as f64);
            }
        }
        let t = terrains.read();
        let terrain = t.get(&(tx, ty))?;
        Some(terrain.sample(lx, ly) as f64)
    }));
    // what the wheels stand on: the faces of the roads and surface objects themselves,
    // and the terrain where it is not cut away under them (the streamer keeps both maps to
    // the loaded tiles, and the bus is one of its centres)
    let terrains = world.terrains.clone();
    let surfaces = world.surfaces.clone();
    vehicle.contact = Some(std::sync::Arc::new(scene::DriveGround {
        terrains,
        surfaces,
    }));
    let objects = crate::settings::Settings::load().collision_objects;
    vehicle.collision = objects.then(|| world.collision.lock().clone());
    vehicle.wheel_walls = objects;
    // a rail vehicle rides the track (its position comes from the rails, not the tyres)
    let rail_bound = crate::rail_drive::is_rail(&vt.def);
    if rail_bound {
        log::info!("rail: {} is bound to the rails", vt.def.path.display());
    }
    if args.physics != "simple" && !rail_bound {
        // (on what the wheels stand on near the height found above: the texel height of
        // `ground_height` undid that, and put the bus on a wall's top or a deck over it)
        if let Some(z) = world.stand_height(vehicle.position.x, vehicle.position.y, vehicle.position.z) {
            vehicle.position.z = z;
        }
        log::info!("spawn: the bus stands at height {:.2}", vehicle.position.z);
        vehicle.enable_rigid_body();
        let rb = vehicle.rigid.as_ref().unwrap();
        log::info!(
            "rigid body: mass {:.0} kg, inertia {:?}, cog {:.2} m, {} wheels, loads {:?}",
            rb.mass,
            rb.inertia,
            rb.cog.z,
            rb.wheels.len(),
            rb.wheels
                .iter()
                .map(|w| w.rest_load.round())
                .collect::<Vec<_>>()
        );
        for a in 0..vehicle.ty.def.axles.len() {
            let w = &rb.wheels[a * 2];
            let model = vehicle.ty.wheel_geometry(0).get(a).copied().flatten();
            log::info!(
                "axle {a}: tyre r {:.3} m, hub {:.3} m unloaded{}, spring {:.0} kN/m x {:.2}, rest load {:.1} kN, sag {:.3} m",
                w.radius,
                w.attach.z,
                model.map(|(z, r)| format!(" (model wheel centre {z:.3}, tyre {r:.3})")).unwrap_or_else(|| " (radius: no wheel mesh found)".into()),
                w.spring / 1000.0,
                w.spring_factor,
                w.rest_load / 1000.0,
                w.rest_compression()
            );
        }
    }
    log::info!(
        "collision world: {} obstacle boxes, {} collision meshes of {} parts",
        vehicle.collision.as_ref().map(|c| c.boxes.len()).unwrap_or(0),
        vehicle.collision.as_ref().map(|c| c.meshes.len()).unwrap_or(0),
        vehicle.collision.as_ref().map(|c| c.mesh_parts()).unwrap_or(0)
    );
    let bindings = omsi_content::KeyboardCfg::load(&crate::startup::keyboard_cfg(&args.root))
        .map(|k| k.with_game_defaults().vehicles)
        .unwrap_or_default();
    static NEXT_UID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut p = Player {
        uid: NEXT_UID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        vehicle,
        render,
        trailer_renders,
        axes: Default::default(),
        analog: Default::default(),
        cam_choice: (0, 0),
        bindings,
        sounds: None,
        pressed_mesh: None,
        press_info: (true, 0.0),
        auto_drag: None,
        pressed_trailer_mesh: None,
        occlude_controls: false,
        startup: None,
        startup_at: None,
        give_ticket: false,
        give_change: false,
        door_buttons: hashbrown::HashMap::new(),
        cam_before_special: None,
        held_keys: Default::default(),
        hand_coupled: 0,
        rail_bound,
        rail: None,
        head: Vec3::ZERO,
        head_vel: Vec3::ZERO,
        head_omega: Vec3::ZERO,
        head_idle: Default::default(),
        steer_look: 0.0,
        seat: Vec3::ZERO,
        mirror_offsets: crate::settings::mirror_offsets(&vt.def.path),
        mirror_shifts: crate::settings::mirror_shifts(&vt.def.path),
        mirror_fovs: crate::settings::mirror_fovs(&vt.def.path),
        mirrors_dirty: false,
        take_change: false,
        toggled_up: Default::default(),
        momentary_gears: crate::settings::Settings::load().momentary_gears,
        auto_shift: crate::settings::Settings::load().auto_shift,
        auto_shift_wait: 0.0,
        auto_shift_idle: 0.0,
        side_lights_by_l: false,
        driver: None,
        ibis_duty: None,
        ibis_typist: None,
        duty_typed: false,
        html_next_stop: None,
        ibis_background: false,
        arm: Default::default(),
        blinker_key_state: 0,
        blinker_cancel: crate::settings::Settings::load().blinker_cancel,
    };
    for _ in 0..3 {
        p.vehicle.update(1.0 / 30.0);
    }
    if let Some(sv) = &args.setvar {
        for kv in sv.split(',') {
            if let Some((k, v)) = kv.split_once('=') {
                if !p.vehicle.set_var(k.trim(), omsi_cfg::parse_f32(v)) {
                    log::warn!("variable {k} not found");
                }
            }
        }
        p.vehicle.update(1.0 / 30.0);
    }
    // a situation remembers how dirty the bus was; --dirt sets it for a test run
    if let Some(d) = args.dirt {
        p.vehicle.dirt = d.clamp(0.0, 1.0);
    }
    if args.is_resuming() {
        let (numeric, textual) = p
            .vehicle
            .restore_script_state(&args.situation_vars, &args.situation_strvars);
        log::info!(
            "situation: {numeric} of {} variables and {textual} of {} strings restored",
            args.situation_vars.len(),
            args.situation_strvars.len()
        );
    }
    if let Some(sv) = &args.setstr {
        for kv in sv.split(',') {
            if let Some((k, v)) = kv.split_once('=') {
                match p.vehicle.ty.program.str_var(k.trim()) {
                    Some(i) => p.vehicle.state.str_vars[i as usize] = v.to_string(),
                    None => log::warn!("string variable {k} not found"),
                }
            }
        }
    }
    // immediate triggers (those without a time)
    for (name, at) in parse_triggers(args) {
        if at <= 0.0 && !p.vehicle.trigger(&name) {
            log::warn!("trigger {name} not found");
        }
    }
    if omsi_cfg::env::var_os("OMSI_DEBUG_MESHES").is_some() {
        for (i, vm) in p.vehicle.ty.meshes.iter().enumerate() {
            let def = &p.vehicle.ty.model.meshes[vm.def_index];
            let (lo, hi) = vm.data.positions.iter().fold(
                (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
                |(lo, hi), v| (lo.min(*v), hi.max(*v)),
            );
            let (uv0, uv1) = vm.data.uvs.iter().fold(
                (glam::Vec2::splat(f32::MAX), glam::Vec2::splat(f32::MIN)),
                |(a, b), v| (a.min(*v), b.max(*v)),
            );
            let nrm = vm
                .data
                .normals
                .iter()
                .map(|n| n.length())
                .fold(f32::MAX, f32::min);
            log::info!("mesh {i:3} {:40} vp={} tris={:6} bounds {:?}..{:?} uv {:?}..{:?} min|n|={nrm:.2} mats={} anims={} visible={:?}", def.file, def.viewpoint, vm.data.indices.len() / 3, lo, hi, uv0, uv1, vm.materials.len(), def.animations.len(), def.visible);
            // per material slot: which part of its texture the mesh shows (display texts)
            if omsi_cfg::env::var("OMSI_DEBUG_MESHES")
                .map(|f| {
                    !f.is_empty()
                        && def
                        .file
                        .to_ascii_lowercase()
                        .contains(&f.to_ascii_lowercase())
                })
                .unwrap_or(false)
            {
                for &(first, count, slot) in &vm.data.ranges {
                    let idx = &vm.data.indices[first as usize..(first + count) as usize];
                    let (a, b) = idx
                        .iter()
                        .filter_map(|k| vm.data.uvs.get(*k as usize))
                        .fold(
                            (glam::Vec2::splat(f32::MAX), glam::Vec2::splat(f32::MIN)),
                            |(a, b), v| (a.min(*v), b.max(*v)),
                        );
                    let (p0, p1) = idx
                        .iter()
                        .filter_map(|k| vm.data.positions.get(*k as usize))
                        .fold(
                            (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
                            |(a, b), v| (a.min(*v), b.max(*v)),
                        );
                    log::info!(
                        "    slot {slot} {:?}: {} tris, uv {a:?}..{b:?}, at {p0:?}..{p1:?}",
                        vm.materials
                            .get(slot as usize)
                            .map(|m| m.texture.as_str())
                            .unwrap_or(""),
                        count / 3
                    );
                    if count <= 24 {
                        for t in idx.chunks_exact(3) {
                            log::info!(
                                "      tri uv {:?} pos {:?}",
                                t.iter()
                                    .map(|k| vm.data.uvs[*k as usize])
                                    .collect::<Vec<_>>(),
                                t.iter()
                                    .map(|k| vm.data.positions[*k as usize])
                                    .collect::<Vec<_>>()
                            );
                        }
                    }
                }
            }
        }
    }
    if omsi_cfg::env::var_os("OMSI_DEBUG_PROPS").is_some() {
        for (i, vm) in p.vehicle.ty.meshes.iter().enumerate() {
            let pr = &p.vehicle.mesh_props[i];
            let def = &p.vehicle.ty.model.meshes[vm.def_index];
            if pr.slot_alpha.iter().any(|a| *a < 1.0)
                || !pr.visible
                || def.materials.iter().any(|m| m.alphascale.is_some())
            {
                log::info!(
                    "mesh {} {}: visible={} slot_alpha={:?} o3d mats={:?} overrides={:?}",
                    i,
                    vm.def_index,
                    pr.visible,
                    pr.slot_alpha,
                    vm.materials
                        .iter()
                        .map(|m| m.texture.clone())
                        .collect::<Vec<_>>(),
                    def.materials
                        .iter()
                        .map(|m| (m.texture.clone(), m.index, m.alpha, m.alphascale.clone()))
                        .collect::<Vec<_>>()
                );
            }
        }
        for v in [
            "Rain_Window_Front_Wetness",
            "Rain_Window_Norm_Wetness",
            "Rain_Window_Wiped_Wetness",
            "PrecipRate",
            "cp_lenkrad_visible",
            "IBIS_mode",
            "IBIS_RouteIndex",
            "IBIS_TerminusIndex",
            "IBIS_TerminusCode",
            "IBIS_LinieKurs",
            "elec_busbar_main",
        ] {
            log::info!("{v} = {:?}", p.vehicle.var(v));
        }
        for v in [
            "IBIS_terminus_name",
            "IBIS_Complex_Line",
            "IBIS_busstop_name",
            "act_busstop",
            "IBIS",
            "Haltestelle",
        ] {
            log::info!("${v} = {:?}", p.vehicle.str_var(v));
        }
    }
    p.driver = crate::driver::DriverFigure::new(world, renderer, scene, &p.vehicle, 0);
    p.sync_transforms(
        renderer,
        scene,
        matches!(args.view.as_str(), "driver" | "pax"),
    );
    Ok(Some(p))
}

/// The paint scheme a vehicle wears: the one named (or numbered) by `--paint`; none named is
/// the model's own textures (the launcher's "Default paint"). Taking the first scheme then
/// put another livery on a bus whose default was chosen. The game and the launcher's
/// preview (`--export-glb`) both come here, so the preview shows what is driven.
pub(crate) fn paint_scheme(vt: &omsi_sim::VehicleType, paint: Option<&str>) -> Option<usize> {
    let asked = paint.map(str::trim).filter(|p| !p.is_empty());
    let found = asked.and_then(|p| {
        vt.paint_schemes
            .iter()
            .position(|s| s.name.eq_ignore_ascii_case(p))
            .or_else(|| p.parse::<usize>().ok().filter(|i| *i < vt.paint_schemes.len()))
    });
    if let (Some(p), None) = (asked, found) {
        log::warn!(
            "paint scheme '{p}' not found; available: {:?}",
            vt.paint_schemes.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
        );
    }
    found
}

/// Where the map's `global.cfg` recorded an entry point (x, height, y within its tile), in
/// world coordinates, taken in the tile of its object at `object` (Grundorf's records agree
/// with their objects that way to a few decimetres).
pub(crate) fn recorded_entry_pos(ep: &omsi_map::global::EntryPoint, object: DVec3) -> Option<DVec3> {
    let s = omsi_map::tile_size();
    let (tx, ty) = ((object.x / s).floor(), (object.y / s).floor());
    ep.pos.iter().all(|v| v.is_finite()).then(|| DVec3::new(tx * s + ep.pos[0], ty * s + ep.pos[1], ep.pos[2]))
}
