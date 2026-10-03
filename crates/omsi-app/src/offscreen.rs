//! The offscreen run: a scripted session rendered to pictures (`--offscreen`).

use super::*;

pub(crate) fn run_offscreen(
    args: &Args,
    out: &PathBuf,
    mut lan_off: Option<omsi_net::LanSession>,
    mut remotes_off: lan::LanGame,
) -> Result<()> {
    let (w, h) = args
        .size
        .split_once('x')
        .map(|(a, b)| {
            (
                a.parse::<u32>().unwrap_or(1600),
                b.parse::<u32>().unwrap_or(900),
            )
        })
        .unwrap_or((1600, 900));
    let view_aspect = w as f32 / h.max(1) as f32;
    let settings = settings::Settings::load();
    let instance = graphics_instance();
    let mut renderer = pollster::block_on(Renderer::new_with(
        &instance,
        None,
        Some(wgpu::TextureFormat::Rgba8UnormSrgb),
        settings.render_options(),
    ))?;
    log::info!("adapter: {}", renderer.adapter_name);
    crate::lights::load_smoke_texture(&mut renderer, &args.root);
    crate::lights::set_corona_root(&args.root);
    let mut scene = renderer.new_scene();
    let (world, mut camera) = lan::answering_while(&mut lan_off, args.bus.as_deref(), || load_world(args, &renderer, &mut scene))?;
    let lan_seed = lan_off.as_ref().map(lan::population_seed);
    // (a player who joins another's game draws the host's traffic in it, whatever their own
    // count says: without it the host's cars had nowhere to go - "passengers, but no
    // traffic" on a server)
    // (and without traffic it still runs the light programs and switches the lamps)
    let mut traffic = {
        let mut t = traffic::Traffic::new(&args.root, &world, args.traffic)?;
        t.lights_only = !(args.traffic > 0 || args.schedule || crate::rail_drive::args_rail(args) || args.lan_join.is_some());
        if let Some(seed) = lan_seed {
            t.set_lan_seed(seed);
        }
        if args.traffic > 0 {
            t.precache_random(&world, &renderer, &mut scene);
        }
        Some(t)
    };
    let mut schedule = if args.schedule {
        Some(schedule::Schedule::new(
            &args.root,
            &world,
            &start_clock(args),
        ))
    } else {
        None
    };
    if let Some(s) = schedule.as_mut() {
        lan::answering_while(&mut lan_off, args.bus.as_deref(), || {
            s.precache(
                &world,
                &renderer,
                &mut scene,
                traffic.as_mut(),
                parse_time(&args.time),
            )
        });
        if let (Some(t), true) = (traffic.as_mut(), omsi_cfg::env::var_os("OMSI_CHECK_TRIPS").is_some()) {
            s.check_routes(&world, t);
        }
    }
    let mut player = spawn_player(args, &world, &renderer, &mut scene)?;
    let spawn_z = player.as_ref().map(|p| p.vehicle.position.z).unwrap_or(0.0);
    if let Some(p) = player.as_mut() {
        p.vehicle.host.auto_clutch = if settings.auto_clutch { 1.0 } else { 0.0 };
        // OMSI_PAX_CAM=n: `--view pax` from the bus's n-th passenger camera
        if let Some(k) = omsi_cfg::env::var("OMSI_PAX_CAM").ok().and_then(|v| v.parse().ok()) {
            p.cam_choice.1 = k;
        }
    }
    let center = player
        .as_ref()
        .map(|p| p.vehicle.position)
        .unwrap_or(camera.position);
    let mut duty: Option<schedule::PlayerDuty> = None;
    // why the duty asked for cannot be driven (the picture's HUD says it too)
    let mut duty_error: Option<String> = None;
    if let (Some(sch), Some(line), Some(p)) = (schedule.as_mut(), &args.line, player.as_mut()) {
        match sch.player_duty(
            &world,
            line,
            args.tour.as_deref().unwrap_or(""),
            parse_time(&args.time),
            args.trip.as_deref(),
            args.whole_tour,
        ) {
            Ok(mut d) => {
                if let Some(k) = args.duty_trip {
                    d.start_at(k, args.duty_first_stop);
                }
                if args.is_resuming() {
                    d.resume(&mut p.vehicle, parse_time(&args.time), args.situation_next_stop);
                    // (as in the window: see `App`)
                    p.duty_typed = args.autostart;
                } else {
                    d.update(&mut p.vehicle, parse_time(&args.time));
                }
                let mut fonts = world.fonts.lock();
                if let Err(e) = crate::schedule_paper::update_vehicle(
                    &mut p.vehicle,
                    &d,
                    &mut fonts,
                ) {
                    log::warn!("driver timetable paper: {e:#}");
                }
                log::info!(
                    "duty: line {} tour {} trip {} next stop {} ({}) delay {:.0} s, stops {:?}",
                    d.line,
                    d.tour,
                    d.trips[d.trip_index].name,
                    d.next_stop,
                    p.vehicle
                        .host
                        .tt_stops
                        .get(d.next_stop)
                        .map(|s| s.0.clone())
                        .unwrap_or_default(),
                    p.vehicle.host.tt_delay,
                    p.vehicle
                        .host
                        .tt_stops
                        .iter()
                        .map(|s| format!(
                            "{} {:02}:{:02}",
                            s.0,
                            (s.2 / 3600.0) as i32,
                            ((s.2 % 3600.0) / 60.0) as i32
                        ))
                        .collect::<Vec<_>>()
                );
                duty = Some(d);
            }
            Err(e) => {
                log::warn!("no player duty: {e}");
                duty_error = Some(e);
            }
        }
    }
    if let Some(p) = player.as_mut() {
        let active = if duty.is_some() { 1.0 } else { 0.0 };
        p.vehicle.host.schedule_active = active;
        p.vehicle.set_var("schedule_active", active);
    }
    let mut career = args
        .driver
        .as_deref()
        .map(|d| career::Career::load(&args.root, d))
        .unwrap_or_default();
    let mut humans_off = if args.passengers || args.lan_join.is_some() {
        let mut h = humans::Humans::new(&args.root);
        if let Some(seed) = lan_seed {
            h.set_lan_seed(seed);
        }
        h.exact_fare = settings.exact_fare;
        h.boarding = settings.boarding.clone();
        h.voices = match settings.pax_voices.as_str() { "off" => 2, "tickets" => 1, _ => 0 };
        if let Some(p) = player.as_mut() {
            h.set_cabin(&mut p.vehicle);
            h.ticket_key = ticket_key_name(&args.root, &p.bindings);
            h.tickets = p.vehicle.host.tickets.clone();
            if !world.global.money_system.trim().is_empty() {
                h.money = Some(money::Money::new(&args.root, &world.global.money_system));
            }
        }
        // (with the passengers setting, as in the window)
        h.density = world
            .global
            .passenger_density((parse_time(&args.time) / 3600.0) as f32)
            * settings.pax_density;
        h.time_of_day = parse_time(&args.time);
        h.stop_targets = schedule.as_ref().map(|s| s.stop_targets());
        h.stop_names = schedule.as_ref().map(|s| s.stop_names());
        h.populate(&world, &renderer, &mut scene, center);
        if let Some(p) = player.as_ref() {
            if args.riders > 0 {
                h.seed_riders(args.riders, &p.vehicle, &world, &renderer, &mut scene);
            }
        }
        Some(h)
    } else {
        None
    };
    let mut player_ref: Option<Player> = None;
    let envir = omsi_content::Envir::load(&args.root.join("envir.cfg")).ok();
    let weather = load_weather(args);
    setup_sky(args, &renderer, &mut scene, envir.as_ref(), Some(&weather));
    if let Some(p) = player.as_mut() {
        apply_weather(&mut p.vehicle, &weather, initial_wetness(&weather));
    }
    // the workshop's waiting time moves the clock on, so the sky has to follow it
    let mut service_seconds = 0.0f64;
    let daylight0 = omsi_sim::Daylight::compute(&start_clock(args), envir.as_ref());
    if let Some(p) = player.as_mut() {
        p.vehicle.set_var("Envir_Brightness", daylight0.envir_brightness(world.light_map_light_at(p.vehicle.position)));
        let mut clock = p.vehicle.host.clock.clone();
        let was = clock.time;
        let at_station = at_petrol_station(&world, &p.vehicle);
        for line in run_services(
            args,
            &mut p.vehicle,
            &mut clock,
            world.global.repair_time_min,
            at_station,
        ) {
            log::info!("{line}");
        }
        service_seconds = clock.time - was;
        p.vehicle.host.clock = clock;
    }
    // one simulation loop for everything: traffic, the player's vehicle (test profile and
    // timed triggers) and the passengers, so that they see each other every frame
    let dt = 1.0 / 30.0;
    let drive_frames = args.drive.map(|s| (s / dt) as usize).unwrap_or(0);
    let mut wheel_worst: Option<(DVec3, f64)> = None;
    let total_frames = drive_frames
        .max(if humans_off.is_some() { 30 } else { 0 })
        .max(if traffic.is_some() { 1 } else { 0 });
    // a dedicated server runs until it is told to stop (SIGTERM, Ctrl+C)
    let server = args.server.is_some();
    let total_frames = if server { usize::MAX } else { total_frames };
    if server {
        quit::install(|_| {});
        log::info!("server: running; Ctrl+C or SIGTERM stops it");
    }
    let timed: Vec<(String, f32)> = parse_triggers(args)
        .into_iter()
        .filter(|(_, t)| *t > 0.0)
        .collect();
    let mut snapshot_times: Vec<f32> = args
        .snapshots
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter_map(|v| v.trim().parse::<f32>().ok())
        .collect();
    snapshot_times.sort_by(|a, b| a.total_cmp(b));
    let drive_start = player
        .as_ref()
        .map(|p| p.vehicle.position)
        .unwrap_or(DVec3::ZERO);
    // test harness for crashes, kerbs and reversing: OMSI_DRIVE_PROFILE="t throttle brake
    // [steer]/ …" holds piecewise constant pedals from each t on, OMSI_DRIVE_V0 gives the
    // bus a speed (km/h) on the first frame, OMSI_DEBUG_PHYSICS=secs logs the pose
    let drive_profile: Vec<[f32; 4]> = omsi_cfg::env::var("OMSI_DRIVE_PROFILE")
        .unwrap_or_default()
        .split(['/', ';'])
        .filter_map(|s| {
            let v: Vec<f32> = s
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            (v.len() >= 3).then(|| [v[0], v[1], v[2], v.get(3).copied().unwrap_or(0.0)])
        })
        .collect();
    let drive_v0: Option<f32> = omsi_cfg::env::var("OMSI_DRIVE_V0")
        .ok()
        .and_then(|v| v.parse().ok());
    let physics_log: f32 = omsi_cfg::env::var("OMSI_DEBUG_PHYSICS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let mut last_reasons: Vec<String> = Vec::new();
    if args.autostart && !args.is_resuming() {
        if let Some(p) = player.as_mut() {
            log::info!("{}", p.start_up());
            if let Some(d) = duty.as_ref() {
                // typed into the IBIS when the start-up has the electrics on
                let (trip, stop) = d.trip_for_ibis();
                p.set_duty_destination(trip, stop);
            }
        }
    }
    if let Some(t) = traffic.as_mut() {
        t.day_time = parse_time(&args.time);
        let daylight = omsi_sim::Daylight::compute(
            &start_clock(args),
            omsi_content::Envir::load(&args.root.join("envir.cfg"))
                .ok()
                .as_ref(),
        );
        t.night = daylight.brightness < 0.75;
        t.daylight = Some(daylight);
        t.populate(&world, &renderer, &mut scene, center);
    }
    // OMSI_GROUND_SAMPLE=<csv>: what the wheels stand on every metre along the street lanes
    // within 400 m of the start (lane, s, x, y, lane z, ground z, the id of an object whose
    // collision mesh stands in the way there) - two builds compared on
    // the same map show where a change of the ground rules adds or removes a bump
    if let (Ok(path), Some(t)) = (omsi_cfg::env::var("OMSI_GROUND_SAMPLE"), traffic.as_ref()) {
        use std::io::Write;
        let Ok(mut f) = std::fs::File::create(&path) else { return Err(anyhow::anyhow!("OMSI_GROUND_SAMPLE: cannot write {path}")) };
        let collision = world.collision.lock().clone();
        for (li, l) in t.net.lanes.iter().enumerate() {
            if l.kind != omsi_sim::traffic::LaneKind::Street { continue; }
            let len = l.length();
            let mut s = 0.0f32;
            while s < len {
                let (p, _) = l.at(s);
                if (p.truncate() - center.truncate()).length() < 400.0 {
                    let g = crate::scene::drive_probe(&world.terrains, &world.surfaces, p.x, p.y, p.z + 0.5);
                    // and a wall there: a 2 m box from 0.3 m to 3 m over the ground, against
                    // the objects' collision meshes (the id of the first one it touches)
                    let base = g.below.unwrap_or(p.z);
                    let mut probe = omsi_sim::collision::Obb::from_box([2.0, 2.0, 2.7, 0.0, 0.0, 0.0], DVec3::new(p.x, p.y, base), 0.0);
                    probe.z0 = base + 0.3;
                    probe.z1 = base + 3.0;
                    let wall = collision.meshes.iter().find(|m| m.parts_near(&probe, None).next().is_some()).map(|m| m.id);
                    // and beside the lane, where a bus's wheels run (1.1 m) and a lane over
                    // (2.5 m): a ground wider than the road shows there
                    let (q, _) = l.at((s + 0.5).min(len));
                    let dir = (q - p).truncate().normalize_or_zero();
                    let side: Vec<String> = [-2.5, -1.1, 1.1, 2.5]
                        .iter()
                        .map(|&d| {
                            let w = p.truncate() + glam::DVec2::new(dir.y, -dir.x) * d;
                            crate::scene::drive_probe(&world.terrains, &world.surfaces, w.x, w.y, p.z + 0.5).below.map(|z| format!("{z:.4}")).unwrap_or_default()
                        })
                        .collect();
                    let _ = writeln!(f, "{li},{s:.1},{:.2},{:.2},{:.3},{},{},{}", p.x, p.y, p.z, g.below.map(|z| format!("{z:.4}")).unwrap_or_default(), wall.map(|w| w.to_string()).unwrap_or_default(), side.join(","));
                }
                s += 1.0;
            }
        }
    }
    // LAN in an offscreen run too, so that one game's view of another can be rendered
    // (`OMSI_LAN_AUDIO=1`: with the other buses' sounds, heard at the camera - for the logs)
    let lan_audio = (lan_off.is_some() && omsi_cfg::env::var_os("OMSI_LAN_AUDIO").is_some())
        .then(omsi_audio::AudioEngine::new);
    if let (Some(l), Some(p)) = (lan_off.as_mut(), player.as_mut()) {
        lan::settle_spawn(
            l,
            &mut remotes_off,
            p,
            args,
            &world,
            traffic.as_ref().map(|t| &t.net),
            lan::WELCOME_WAIT,
        );
    }
    let run_clock = start_clock(args);
    // a dedicated server's administration and clock (see `admin`)
    let mut srv_admin = crate::admin::ServerAdmin::default();
    let mut srv_clock = 0.0f64;
    // the METAR sync of a dedicated server: the report is downloaded in the background (at
    // once, then every ten minutes) and its values are told to the players
    let srv_metar: Option<String> = if server { crate::server::SERVER_METAR.get().cloned().flatten() } else { None };
    let mut srv_metar_due = std::time::Instant::now();
    let mut srv_metar_rx: Option<std::sync::mpsc::Receiver<Option<omsi_content::weather::Weather>>> = None;
    // (the weather's name on the status page)
    let mut srv_weather_name = if weather.path.to_string_lossy().starts_with("metar:") {
        weather.name.clone()
    } else {
        weather.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    };
    if let (true, Some((pw, speed))) = (server, crate::server::SERVER_ADMIN.get()) {
        srv_admin.password = pw.clone();
        if let Some(l) = lan_off.as_mut() {
            l.clock_speed = *speed;
        }
    }
    let mut ground_gap = crate::ground_gap::GroundGap::from_env();
    if let Some(t) = traffic.as_ref() {
        crate::ground_gap::check_lanes(&world, t);
    }
    for i in 0..total_frames {
        let t_s = i as f32 * dt;
        if server {
            srv_clock += dt as f64 * lan_off.as_ref().map(|l| l.clock_speed).unwrap_or(1.0);
            // a server on the real time (server.cfg): its clock reads this machine's
            if i % 30 == 0 && crate::real_time::server_real() {
                if let Some(n) = crate::real_time::now() {
                    let have = (parse_time(&args.time) + srv_clock + srv_admin.shift).rem_euclid(86400.0);
                    let off = (n.secs - have + 43_200.0).rem_euclid(86_400.0) - 43_200.0;
                    if off.abs() > 0.5 {
                        srv_admin.shift += off;
                    }
                }
            }
            if let (Some(icao), Some(l)) = (srv_metar.as_ref(), lan_off.as_mut()) {
                if srv_metar_rx.is_some() {
                    let got = srv_metar_rx.as_ref().map(|rx| rx.try_recv());
                    match got {
                        Some(Ok(report)) => {
                            srv_metar_rx = None;
                            // (a failed download is tried again in a minute)
                            let wait = if report.is_some() { 600 } else { 60 };
                            srv_metar_due = std::time::Instant::now() + std::time::Duration::from_secs(wait);
                            if let Some(w) = report {
                                if let Some(wire) = crate::weather_setup::report_wire(&w) {
                                    if wire != l.weather() {
                                        log::info!("server: weather now the METAR report of {icao}: {wire}");
                                        l.set_weather(&wire);
                                    }
                                    srv_weather_name = w.name.clone();
                                }
                            }
                        }
                        Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                            srv_metar_rx = None;
                            srv_metar_due = std::time::Instant::now() + std::time::Duration::from_secs(60);
                        }
                        _ => {}
                    }
                } else if std::time::Instant::now() >= srv_metar_due {
                    let (tx, rx) = std::sync::mpsc::channel();
                    srv_metar_rx = Some(rx);
                    let icao = icao.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(crate::weather_setup::try_metar(&icao));
                    });
                }
            }
            if let Some(l) = lan_off.as_mut() {
                let positions = |id: u32| remotes_off.remotes.get(&id).map(|r| (r.vehicle().position, r.vehicle().heading));
                srv_admin.prune(l);
                for (from, text) in l.take_commands() {
                    crate::admin::server_command(l, from, &text, &mut srv_admin, &positions);
                }
                // a tool on this machine (POST /admin, checked by the gateway): an admin of its own
                for text in crate::lan::take_local_admin() {
                    srv_admin.admins.insert(crate::admin::LOCAL_ADMIN);
                    crate::admin::server_command(l, crate::admin::LOCAL_ADMIN, &format!("admin {text}"), &mut srv_admin, &positions);
                }
                // an admin set the time of day: the shift that makes the clock read it
                if let Some(want) = srv_admin.set_clock.take() {
                    let now = (parse_time(&args.time) + srv_clock + srv_admin.shift).rem_euclid(86400.0);
                    srv_admin.shift += (want - now + 43_200.0).rem_euclid(86_400.0) - 43_200.0;
                }
                // an admin's traffic order: the density asked for, or every AI car off the road
                match srv_admin.traffic.take() {
                    Some(crate::admin::TrafficOrder::Density(n)) => {
                        if let Some(t) = traffic.as_mut() {
                            t.target = n;
                            log::info!("server: traffic density now {n}");
                        }
                    }
                    Some(crate::admin::TrafficOrder::Clear) => {
                        if let Some(t) = traffic.as_mut() {
                            let ids: Vec<u64> = t.cars.iter().filter(|c| !c.is_bus()).map(|c| c.id).collect();
                            for id in &ids {
                                t.remove_car(&world, &renderer, &mut scene, *id);
                            }
                            log::info!("server: {} AI vehicles taken off the road", ids.len());
                        }
                    }
                    None => {}
                }
                if let Some(want) = srv_admin.set_weather.take() {
                    // only an installed weather (the name came over the network)
                    let found = omsi_cfg::read_dir_merged("Weather")
                        .into_iter()
                        .filter_map(|p| p.file_name().map(|n| format!("Weather/{}", n.to_string_lossy())))
                        .find(|f| f.eq_ignore_ascii_case(&want));
                    match found {
                        Some(f) => {
                            log::info!("server: weather now {f}");
                            l.set_weather(&f);
                        }
                        None => log::info!("server: weather {want} is not installed"),
                    }
                }
                // (the weather follows the METAR report: no next weather for the admins)
                if std::mem::take(&mut srv_admin.next_weather) && srv_metar.is_none() {
                    let mut files: Vec<String> = omsi_cfg::read_dir_merged("Weather")
                        .into_iter()
                        .filter(|p| p.extension().map(|e| e.eq_ignore_ascii_case("owt")).unwrap_or(false))
                        .filter_map(|p| p.file_name().map(|n| format!("Weather/{}", n.to_string_lossy())))
                        .collect();
                    files.sort();
                    files.dedup();
                    if !files.is_empty() {
                        let cur = l.weather().replace('\\', "/").to_ascii_lowercase();
                        let k = files.iter().position(|f| f.to_ascii_lowercase() == cur).map(|k| (k + 1) % files.len()).unwrap_or(0);
                        log::info!("server: weather now {}", files[k]);
                        l.set_weather(&files[k]);
                    }
                }
            }
            if quit::requested().is_some() {
                log::info!("server: stopping");
                break;
            }
            if i % 30 == 0 {
                if let Some(l) = lan_off.as_ref() {
                    crate::server::tick_status(l, parse_time(&args.time) + srv_clock + srv_admin.shift, srv_weather_name.as_str());
                    // the shared world, counted for GET /status
                    let (cars, buses, dormant, parked) = traffic.as_ref().map(|t| t.counts()).unwrap_or_default();
                    let (walking, waiting, aboard) = humans_off.as_ref().map(|h| h.counts()).unwrap_or_default();
                    let target = traffic.as_ref().map(|t| t.target).unwrap_or(0);
                    crate::lan::update_server_world(omsi_net::ws::WorldCounts { cars, buses, dormant, parked, walking, waiting, aboard, traffic: target });
                }
            }
            if lan_off.is_none() {
                std::thread::sleep(std::time::Duration::from_secs_f32(dt));
            }
        }
        if let Some(t) = traffic.as_mut() {
            // the camera the snapshots are taken with is the player's eye for the traffic
            let view_cam = match follow_id(args, Some(&*t))
                .and_then(|id| follow_camera(Some(&*t), id))
            {
                Some(c) => c,
                None => match player.as_ref() {
                    Some(p)
                    if args.cam.is_none() && args.view != "free" && args.follow.is_none() =>
                        {
                            p.camera(&args.view, &camera)
                        }
                    _ => Camera {
                        position: camera.position,
                        yaw: camera.yaw,
                        pitch: camera.pitch,
                        roll: camera.roll,
                        fov_deg: camera.fov_deg,
                        near: camera.near,
                        far: camera.far,
                    },
                },
            };
            traffic_inputs(
                t,
                Some(&view_cam),
                w as f64 / h.max(1) as f64,
                triple_extent(&settings, &view_cam, w, h),
                weather.fog.0 as f64,
                &run_clock,
                humans_off.as_ref(),
                player.as_ref(),
                &renderer.options,
            );
            if i % 60 == 0 {
                // (following a car, the population goes with the camera)
                let pc = if args.follow.is_some() { view_cam.position } else { center };
                t.populate(&world, &renderer, &mut scene, pc);
                // (what the window does every frame: cars that parked become parked objects,
                // released vehicles go back to the world)
                t.sync(&world, &renderer, &mut scene);
                // `OMSI_POPULATION_SHOTS=1` (with OMSI_DEBUG_POPULATION): a picture from the
                // viewer whenever a car was put inside its frustum (behind something), with
                // where on the picture it stands - to see that it really is hidden
                let framed = std::mem::take(&mut t.framed_spawns);
                if !framed.is_empty() && omsi_cfg::env::var_os("OMSI_POPULATION_SHOTS").is_some() {
                    t.sync(&world, &renderer, &mut scene);
                    if let Some(p) = player.as_mut() {
                        p.sync_transforms(
                            &renderer,
                            &mut scene,
                            matches!(args.view.as_str(), "driver" | "pax"),
                        );
                        p.sync_driver(&renderer, &mut scene, 1.0 / 30.0, settings.driver, args.view == "driver");
                    }
                    let daylight = omsi_sim::Daylight::compute(&run_clock, envir.as_ref());
                    let lighting = weather_lighting(
                        &daylight,
                        &weather,
                        cloud_drift_at(&weather, run_clock.time),
                        0.0,
                        settings.shadows,
                    );
                    let pixels =
                        renderer.render_to_image(&mut scene, w, h, &view_cam, &lighting)?;
                    let path = out.with_file_name(format!(
                        "{}_spawn_{t_s:.0}.png",
                        out.file_stem().and_then(|s| s.to_str()).unwrap_or("snap")
                    ));
                    image::save_buffer(&path, &pixels, w, h, image::ColorType::Rgba8)?;
                    let f = view_cam.forward();
                    let r = view_cam.right();
                    let u = r.cross(f);
                    let tan_y = (view_cam.fov_deg * 0.5).to_radians().tan();
                    let tan_x = tan_y * w as f32 / h as f32;
                    for (id, pos) in framed {
                        let rel =
                            (pos - view_cam.position).as_vec3() + glam::Vec3::new(0.0, 0.0, 0.8);
                        let z = rel.dot(f);
                        let px = w as f32 * 0.5 * (1.0 + rel.dot(r) / (z * tan_x));
                        let py = h as f32 * 0.5 * (1.0 - rel.dot(u) / (z * tan_y));
                        log::info!("population shot {}: car {id} appeared at pixel ({px:.0}, {py:.0}), {z:.0} m ahead", path.display());
                    }
                }
            }
            if let Some(s) = schedule.as_mut() {
                // at start, pick up trips that left within the last 20 minutes (a few per
                // frame until they are all out, as the window does)
                if i % 60 == 0 || s.pending() > 0 {
                    // no timetable vehicle is put into the player's bus or a LAN player's
                    t.keep_clear = player
                        .as_ref()
                        .map(|p| traffic::vehicle_bodies(&p.vehicle))
                        .unwrap_or_default();
                    t.keep_clear.extend(
                        remotes_off
                            .remotes
                            .values()
                            .flat_map(|r| traffic::vehicle_bodies(r.vehicle())),
                    );
                    s.tick(
                        &world,
                        t,
                        &renderer,
                        &mut scene,
                        t.day_time,
                        if i == 0 { 20.0 * 60.0 } else { 2.5 },
                    );
                }
            }
            t.others = lan_outlines(&remotes_off);
            t.others.extend(own_outlines(player.as_ref(), &[]));
            t.player_priority = player.as_ref().and_then(|p| p.vehicle.var("TrafficPriority")).is_some_and(|v| v > 0.5);
            t.player_blinker = player.as_ref().map(|p| lan::indicator(&p.vehicle)).unwrap_or(0);
            t.tick(dt, player.as_ref().map(|p| player_outline(p)));
            world.set_switches(&t.switch_requests());
            world.set_signals(&t.signal_aspects(&world.signal_routes, None));
            if let Some(p) = player.as_mut() {
                p.vehicle.dynamic_boxes = t.boxes(p.vehicle.position, 80.0);
            }
        }
        if let Some(player) = player.as_mut() {
            player.tick_startup(dt);
            if let Some(d) = duty.as_mut() {
                if let Some(stop) = player.html_next_stop.take() {
                    if d.skip_to(stop) {
                        let (trip, k) = d.trip_for_ibis();
                        player.ibis_to_stop(trip, k);
                    }
                }
                if let Some((arrival, departure)) =
                    d.update(&mut player.vehicle, parse_time(&args.time) + t_s as f64)
                {
                    career.stop_served(arrival, departure);
                }
                if d.take_trip_change() && player.duty_typed {
                    let (trip, stop) = d.trip_for_ibis();
                    player.set_duty_destination(trip, stop);
                }
                let mut fonts = world.fonts.lock();
                if let Err(e) = crate::schedule_paper::update_vehicle(
                    &mut player.vehicle,
                    d,
                    &mut fonts,
                ) {
                    log::warn!("driver timetable paper: {e:#}");
                }
            }
            career.tick(
                dt,
                &player.vehicle,
                humans_off.as_ref().map(|h| h.riding()).unwrap_or(0),
            );
            let crash = std::mem::take(&mut player.vehicle.last_crash);
            if crash > 0.0 {
                career.crashed(crash, player.vehicle.physics.velocity_kmh() / 3.6);
            }
            if i < drive_frames {
                for (name, at) in &timed {
                    if *at > t_s - dt && *at <= t_s {
                        // (a gate of a manual gearbox comes with the automatic clutch, as
                        // from the keys)
                        player.clutch_for_gate(name);
                        // (the game's door actions, `door_<n>` / `doors_all`, as a button
                        // pressed and let go)
                        if crate::player::door_action(name).is_some() {
                            player.action(name, true);
                            player.action(name, false);
                        } else {
                            player.vehicle.trigger(name);
                        }
                    }
                }
                player.axes.clutch = (player.axes.clutch - 0.7 * dt).max(0.0);
                // --drive test profile: full throttle, steering from --setvar drive_steer, brake after drive_brake_at
                let steer = args
                    .setvar
                    .as_ref()
                    .and_then(|s| {
                        s.split(',').find_map(|kv| {
                            kv.strip_prefix("drive_steer=")
                                .and_then(|v| v.parse::<f32>().ok())
                        })
                    })
                    .unwrap_or(0.0);
                let brake_at = args
                    .setvar
                    .as_ref()
                    .and_then(|s| {
                        s.split(',').find_map(|kv| {
                            kv.strip_prefix("drive_brake_at=")
                                .and_then(|v| v.parse::<f32>().ok())
                        })
                    })
                    .unwrap_or(f32::MAX);
                let throttle_from = args
                    .setvar
                    .as_ref()
                    .and_then(|s| {
                        s.split(',').find_map(|kv| {
                            kv.strip_prefix("drive_throttle_from=")
                                .and_then(|v| v.parse::<f32>().ok())
                        })
                    })
                    .unwrap_or(0.0);
                // `drive_brake_until=S`: the foot on the brake until S, as a driver holds it
                // to select a gear (the Citaro's ZF/Voith D button wants the brake pressed)
                let brake_until = args
                    .setvar
                    .as_ref()
                    .and_then(|s| {
                        s.split(',').find_map(|kv| {
                            kv.strip_prefix("drive_brake_until=")
                                .and_then(|v| v.parse::<f32>().ok())
                        })
                    })
                    .unwrap_or(0.0);
                let braking = t_s >= brake_at || t_s < brake_until;
                let throttle = if braking || t_s < throttle_from {
                    0.0
                } else {
                    1.0
                };
                let mut controls = omsi_sim::Controls {
                    throttle,
                    brake: if braking { 1.0 } else { 0.0 },
                    steering: steer,
                    ..Default::default()
                };
                if let Some(step) = drive_profile.iter().rev().find(|s| s[0] <= t_s) {
                    controls = omsi_sim::Controls {
                        throttle: step[1],
                        brake: step[2],
                        steering: step[3],
                        ..Default::default()
                    };
                }
                // OMSI_AUTOPILOT=<km/h>: the player's bus follows the road network's lanes at
                // that speed (a steering wheel on a pure-pursuit point 12 m ahead, a throttle
                // and brake on the speed) - to drive it round a map's roundabouts and bends
                // and see where it falls through or leaves the road; each lane taken is the
                // straightest on
                if let (Some(kmh), Some(net)) = (omsi_cfg::env::var("OMSI_AUTOPILOT").ok().and_then(|v| v.parse::<f32>().ok()), traffic.as_ref().map(|t| &t.net)) {
                    let v = &player.vehicle;
                    let h = v.heading.to_radians();
                    let fwd = DVec3::new(h.sin(), h.cos(), 0.0);
                    let probe = v.position + fwd * 3.0;
                    if let Some((mut lane, mut s, _)) = net.nearest_lane(probe, omsi_sim::traffic::LaneKind::Street) {
                        // (the lane that runs our way)
                        let lh = net.lanes[lane].at(s).1 as f64;
                        let dh = (lh - v.heading + 540.0).rem_euclid(360.0) - 180.0;
                        if dh.abs() > 100.0 {
                            if let Some((l2, s2, _)) = (0..net.lanes.len()).filter(|&k| net.lanes[k].kind == omsi_sim::traffic::LaneKind::Street).filter_map(|k| net.lanes[k].nearest_point(probe).map(|(s, d)| (k, s, d))).filter(|(k, s, d)| *d < 6.0 && ((net.lanes[*k].at(*s).1 as f64 - v.heading + 540.0).rem_euclid(360.0) - 180.0).abs() < 80.0).min_by(|a, b| a.2.total_cmp(&b.2)) {
                                lane = l2;
                                s = s2;
                            }
                        }
                        let mut ahead = 12.0f32;
                        loop {
                            let len = net.lanes[lane].length();
                            if s + ahead <= len || net.lanes[lane].next.is_empty() {
                                s = (s + ahead).min(len);
                                break;
                            }
                            ahead -= len - s;
                            let here = net.lanes[lane].at(len).1;
                            lane = *net.lanes[lane].next.iter().min_by(|a, b| {
                                let da = (net.lanes[**a].at(0.0).1 - here + 540.0).rem_euclid(360.0) - 180.0;
                                let db = (net.lanes[**b].at(0.0).1 - here + 540.0).rem_euclid(360.0) - 180.0;
                                da.abs().total_cmp(&db.abs())
                            }).unwrap();
                            s = 0.0;
                        }
                        let target = net.lanes[lane].at(s).0;
                        let d = (target - v.position).truncate();
                        let want = d.x.atan2(d.y).to_degrees();
                        let alpha = ((want - v.heading + 540.0).rem_euclid(360.0) - 180.0) as f32;
                        let speed = v.physics.velocity_kmh();
                        controls.steering = (alpha / 30.0).clamp(-1.0, 1.0);
                        controls.throttle = ((kmh - speed) / 10.0).clamp(0.0, 1.0);
                        controls.brake = ((speed - kmh - 3.0) / 10.0).clamp(0.0, 1.0);
                    }
                }
                player.tick_auto_shift(dt, controls.throttle, controls.brake);
                player.auto_clutch_bite(controls.throttle);
                controls.clutch = controls.clutch.max(player.axes.clutch);
                player.vehicle.set_controls(controls);
                if i == 0 {
                    if let Some(v0) = drive_v0 {
                        player.vehicle.set_speed(v0 / 3.6);
                    }
                }
                player.vehicle.update(dt);
                // OMSI_DEBUG_VARS with OMSI_DEBUG_VARS_EVERY=<s>: the variables through the drive
                if let (Ok(list), Some(every)) = (omsi_cfg::env::var("OMSI_DEBUG_VARS"), omsi_cfg::env::var("OMSI_DEBUG_VARS_EVERY").ok().and_then(|v| v.parse::<f32>().ok())) {
                    if (t_s / every).floor() != ((t_s - dt) / every).floor() {
                        let vals: Vec<String> = list.split(',').map(str::trim).map(|v| format!("{v}={:.2}", player.vehicle.var(v).unwrap_or(f32::NAN))).collect();
                        log::info!("t={t_s:.1}: {}", vals.join(" "));
                    }
                }
                // OMSI_JOINT_ANGLE=degrees: the rear section held at that angle to the front
                // one (the joint and its bellows seen bent, without driving a curve)
                if let Some(a) = omsi_cfg::env::var("OMSI_JOINT_ANGLE").ok().and_then(|v| v.trim().parse::<f64>().ok()) {
                    let v = &mut player.vehicle;
                    let (pos, rot, heading) = (v.position, v.body_rotation(), v.heading);
                    if let Some(t) = v.trailers.first_mut() {
                        let c = t.coupling_point(pos, rot);
                        let h = (heading + a).to_radians();
                        t.place_pivot(c - DVec3::new(h.sin(), h.cos(), 0.0) * t.pivot_length() as f64);
                    }
                }
                crate::rail_drive::frame(player, traffic.as_ref().map(|t| &t.net), &world, dt);
                // (the autopilot's log: where the bus is against the ground under it, twice a
                // second, and at once when the ground is not under it any more)
                if omsi_cfg::env::var_os("OMSI_AUTOPILOT").is_some() {
                    let at = player.vehicle.position;
                    let under = crate::scene::drive_probe(&world.terrains, &world.surfaces, at.x, at.y, at.z + 1.5).below;
                    let lost = under.is_none_or(|g| at.z < g - 0.6);
                    if i % 15 == 0 || lost {
                        log::info!("autopilot t={t_s:.1} at ({:.1}, {:.1}, {:.2}) heading {:.0} {:.0} km/h, ground under {:?}{}", at.x, at.y, at.z, player.vehicle.heading, player.vehicle.physics.velocity_kmh(), under.map(|g| (g * 100.0).round() / 100.0), if lost { " FELL" } else { "" });
                    }
                }
                // the driver's hands follow the wheel frame by frame (as in the window), so
                // that the snapshots show them where the hand-over-hand has got to
                if settings.driver && !snapshot_times.is_empty() {
                    player.sync_driver(&renderer, &mut scene, dt, true, false);
                }
                // OMSI_SUSP_TRACE=<csv>: every frame, the body's height and vertical speed and
                // each wheel's travel, load and the ground under it (bumps and hops)
                if let Ok(path) = omsi_cfg::env::var("OMSI_SUSP_TRACE") {
                    use std::io::Write;
                    static TRACE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);
                    let mut f = TRACE.lock().unwrap_or_else(|e| e.into_inner());
                    if f.is_none() {
                        *f = std::fs::File::create(&path).ok();
                        if let Some(f) = f.as_mut() {
                            let _ = writeln!(f, "t,x,y,z,vz,kmh,wheel,compression,rate,load,ground_z,on_ground");
                        }
                    }
                    if let (Some(f), Some(rb)) = (f.as_mut(), player.vehicle.rigid.as_ref()) {
                        for (k, w) in rb.wheels.iter().enumerate() {
                            let _ = writeln!(
                                f,
                                "{:.4},{:.2},{:.2},{:.4},{:.3},{:.1},{k},{:.4},{:.3},{:.0},{:.4},{}",
                                t_s, rb.position.x, rb.position.y, rb.position.z, rb.velocity.z,
                                player.vehicle.physics.velocity_kmh(), w.compression, w.compression_rate, w.load, w.ground_z, w.on_ground as u8
                            );
                        }
                    }
                }
                // OMSI_WHEEL_TRACE: the deepest a drawn tyre goes into the road (or floats
                // over it), once a second while driving
                if omsi_cfg::env::var_os("OMSI_WHEEL_TRACE").is_some() {
                    let lows = tyre_lows(&player.vehicle, &world);
                    if let Some((p, d)) = lows.iter().min_by(|a, b| a.1.total_cmp(&b.1)) {
                        wheel_worst = match wheel_worst {
                            Some((_, w)) if w <= *d => wheel_worst,
                            _ => Some((*p, *d)),
                        };
                    }
                    if (t_s % 1.0) < dt {
                        if let Some((p, d)) = wheel_worst.take() {
                            log::info!("wheel trace t={t_s:.0}: deepest tyre {d:+.3} m at ({:.1}, {:.1}, {:.2}), {:.0} km/h", p.x, p.y, p.z, player.vehicle.physics.velocity_kmh());
                        }
                    }
                }
                // what the window's HUD would say about a bus that does not move
                // (once per reason: the numbers in a line change all the time)
                if i % 30 == 0 {
                    let why = standing_reasons(&player.vehicle, &|a| crate::diagnostics::rebound_key(&player.bindings, a));
                    let key = |l: &String| l.split('(').next().unwrap_or_default().to_string();
                    for line in why
                        .iter()
                        .filter(|l| !last_reasons.iter().any(|o| key(o) == key(l)))
                    {
                        log::info!("HUD at {t_s:.1} s (not moving): {line}");
                    }
                    last_reasons = why;
                }
                lay_down_poles(&world, &renderer, &mut scene, &mut player.vehicle);
                // the worst the bus did on the way (a bus on end, flying, through the ground)
                {
                    let v = &player.vehicle;
                    let g = world.ground_height(v.position.x, v.position.y).map(|g| v.position.z - g).unwrap_or(0.0);
                    let mut e = DRIVE_EXTREMES.lock();
                    if v.pitch.abs() > e.0.abs() {
                        e.0 = v.pitch;
                        e.4 = (v.position, t_s);
                    }
                    if v.bank.abs() > e.1.abs() {
                        e.1 = v.bank;
                    }
                    e.2 = e.2.max(g);
                    e.3 = e.3.min(g);
                }
                if physics_log > 0.0
                    && (t_s / physics_log).floor() != ((t_s + dt) / physics_log).floor()
                {
                    log_physics(&player.vehicle, t_s + dt);
                }
                // `OMSI_TRACE_VARS=a,b,$c`: the listed variables every half second of the run
                // (a leading `$` reads a string variable) - how a start-up sequence unfolds
                if i % 15 == 0 {
                    if let Ok(list) = omsi_cfg::env::var("OMSI_TRACE_VARS") {
                        let vals: Vec<String> = list
                            .split(',')
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(|v| match v.strip_prefix('$') {
                                Some(s) => format!("{v}={:?}", player.vehicle.str_var(s)),
                                None => format!(
                                    "{v}={}",
                                    player
                                        .vehicle
                                        .var(v)
                                        .map(|x| format!("{x:.3}"))
                                        .unwrap_or_else(|| "-".into())
                                ),
                            })
                            .collect();
                        log::info!("t={t_s:.1} {}", vals.join(" "));
                    }
                }
            }
        }
        if let Some(g) = ground_gap.as_mut() {
            g.frame(&world, t_s, player.as_ref().map(|p| &p.vehicle), traffic.as_ref());
        }
        let size = (w, h);
        if let Some(h) = humans_off.as_mut() {
            // keep density and time_of_day up to date every tick, as app_events.rs does
            // (stop_target = enter_mean * density; without this it stays at the startup
            // value and the new formula returns 0 for the whole session when the map has
            // a low hourly density at the start time)
            h.density = world
                .global
                .passenger_density((run_clock.time / 3600.0) as f32)
                * settings.pax_density;
            h.time_of_day = run_clock.time;
            // populate stops near every LAN player every 2 seconds, as app_events.rs
            // does every 2 s near the local player.  At startup `center` is ZERO (no
            // player bus on a headless server), so stops on the actual map – which can
            // be thousands of metres away – fall outside the 600 m filter in
            // `populate_with` and are never seeded without this loop.
            if i % 60 == 0 {
                let player_centers: Vec<glam::DVec3> = remotes_off
                    .remotes
                    .values()
                    .map(|r| r.vehicle().position)
                    .chain(player.as_ref().map(|p| p.vehicle.position))
                    .collect();
                for c in &player_centers {
                    h.populate(&world, &renderer, &mut scene, *c);
                }
                // also update which stops the LAN players are near
                h.lan_centers = player_centers;
            }
            // what the passengers must not be seen appearing in front of
            let followed = traffic
                .as_ref()
                .and_then(|t| follow_id(args, Some(t)).and_then(|id| follow_camera(Some(t), id)));
            let eye_cam = match player.as_ref() {
                _ if followed.is_some() => followed.unwrap(),
                Some(p) if args.cam.is_none() && args.view != "free" && args.follow.is_none() => {
                    p.camera(&args.view, &camera)
                }
                _ => Camera {
                    position: camera.position,
                    yaw: camera.yaw,
                    pitch: camera.pitch,
                    roll: camera.roll,
                    fov_deg: camera.fov_deg,
                    near: camera.near,
                    far: camera.far,
                },
            };
            h.eye = Some(humans::Eye::of(&eye_cam, view_aspect).widened(triple_extent(&settings, &eye_cam, size.0, size.1)));
            h.set_remote_buses(remotes_off.remotes.iter().map(|(id, r)| (*id, r.vehicle())));
            let took = h.tick(
                dt,
                &world,
                player.as_ref().map(|p| &p.vehicle),
                traffic.as_ref(),
                &renderer,
                &mut scene,
            );
            // validators used: the bus's `ev_Stamper` sound
            for bus in h.take_stamped() {
                match bus {
                    None => {
                        if let Some(p) = player.as_mut() {
                            p.vehicle.host.fired_triggers.push("ev_Stamper".into());
                        }
                    }
                    Some(id) => {
                        if let Some(c) = traffic.as_mut().and_then(|t| t.cars.iter_mut().find(|c| c.id == id)) {
                            c.vehicle.host.fired_triggers.push("ev_Stamper".into());
                        }
                    }
                }
            }
            if let Some(t) = traffic.as_mut() {
                let (alighting, waiting) = h.stop_wishes();
                t.set_stop_wishes(alighting, waiting);
                for (id, stop, secs) in h.take_holds() {
                    t.hold_boarding(id, stop, secs);
                }
                for (id, entry, exit) in h.take_ai_requests() {
                    t.set_pax_requests(id, &entry, &exit);
                }
            }
            if let Some(p) = player.as_mut() {
                if took {
                    p.vehicle.set_var("GivenTicket", -1.0);
                }
                p.vehicle.host.humans_on_path_link = h.path_link_counts();
                p.vehicle.host.humans_on_seat = h.seat_counts();
            }
            if h.tracing() {
                // OMSI_TRACE_PAX wants every frame as a window would draw it
                h.sync(&renderer, &mut scene, eye_cam.position);
            }
            if let Some(m) = h.take_message() {
                log::info!("HUD: {m}");
            }
            if let Some(p) = player.as_mut() {
                h.write_pax_vars(&mut p.vehicle);
            }
            career.tickets = (h.tickets_sold as i32, h.ticket_cash as f64);
            career.boarded = h.boarded as i32;
            career.served = h.served as i32;
            career.stepped_in = h.stepped_in as i32;
            career.content = h.content as i32;
            career.ticket_requests = h.ticket_requests as i32;
            career.ticket_points = h.ticket_points as i32;
            if let Some(p) = player.as_ref() {
                let hurt = h.run_over(&p.vehicle);
                if hurt > 0 {
                    career.crashes[1] += hurt as i32;
                    log::warn!("{hurt} pedestrian(s) knocked down");
                }
            }
            if std::mem::take(&mut h.stop_request) {
                if let Some(p) = player.as_mut() {
                    p.vehicle.trigger("int_haltewunsch");
                }
            }
        }
        if let Some(l) = lan_off.as_mut() {
            let listener = if args.cam.is_some() {
                Some(camera.position)
            } else {
                player.as_ref().map(|p| p.vehicle.position)
            };
            if let (Some(a), Some(at)) = (lan_audio.as_ref(), listener) {
                a.set_listener(omsi_audio::Listener {
                    position: at.as_vec3(),
                    ..Default::default()
                });
            }
            // a host tells the others its clock (a client here keeps the one it started with;
            // a server's runs at its speed, moved by its admins)
            let mut now = start_clock(args);
            if server {
                now.advance((srv_clock + srv_admin.shift) as f32);
            } else {
                now.advance(t_s + service_seconds as f32);
            }
            let clock = (l.role == omsi_net::Role::Host).then_some(&now);
            let frame = lan::Frame {
                audio: lan_audio.as_ref(),
                listener,
                muffled: false,
                riders: humans_off.as_ref().map(|h| h.riding()).unwrap_or(0),
                clock,
                tour: duty.as_ref().map(|d| format!("{}/{}", d.line, d.tour)),
                walker: None,
                inside_of: None,
            };
            let updates = lan::tick(
                l,
                &mut remotes_off,
                dt,
                args,
                player.as_mut(),
                Some(&world),
                Some(&renderer),
                Some(&mut scene),
                traffic.as_mut(),
                humans_off.as_mut(),
                None,
                &frame,
            );
            for u in updates {
                if let (lan::WorldUpdate::Tours(tours), Some(s)) = (u, schedule.as_mut()) {
                    s.set_lan_tours(tours);
                }
            }
            // the other games run in real time
            std::thread::sleep(std::time::Duration::from_secs_f32(dt));
        }
        // mid-run snapshots (relative to the first overtake with --follow auto)
        let auto_base = match args.follow.as_deref() {
            Some("auto") => traffic.as_ref().and_then(|t| t.last_overtaker).map(|o| o.1),
            Some("turn") => traffic.as_ref().and_then(|t| t.first_turner).map(|o| o.1),
            Some("red") => traffic.as_ref().and_then(|t| t.first_red).map(|o| o.1),
            Some("yield") => traffic.as_ref().and_then(|t| t.first_yield).map(|o| o.1),
            Some("pass") => traffic.as_ref().and_then(|t| t.first_passer).map(|o| o.1),
            _ => Some(0.0),
        };
        if let (Some(&ts), Some(base)) = (snapshot_times.first(), auto_base) {
            if t_s + dt > ts + base {
                snapshot_times.remove(0);
                if let Some(t) = traffic.as_mut() {
                    t.sync(&world, &renderer, &mut scene);
                }
                let mut cam = Camera {
                    position: camera.position,
                    yaw: camera.yaw,
                    pitch: camera.pitch,
                    roll: camera.roll,
                    fov_deg: camera.fov_deg,
                    near: camera.near,
                    far: camera.far,
                };
                if let Some(p) = player.as_mut() {
                    p.sync_transforms(
                        &renderer,
                        &mut scene,
                        matches!(args.view.as_str(), "driver" | "pax"),
                    );
                    p.sync_driver(&renderer, &mut scene, 1.0 / 30.0, settings.driver, args.view == "driver");
                    if args.cam.is_none() && args.view != "free" && args.follow.is_none() {
                        // the head turned as --look says, like the final image
                        cam = p.camera_look(&args.view, &camera, look_of(args), offscreen_orbit());
                        if args.view == "outside" {
                            cam = p.camera_clipped(cam, &world, offscreen_orbit(), 0.0);
                        }
                    }
                    vehicle_camera(p, &mut cam);
                }
                if let Some(id) = follow_id(args, traffic.as_ref()) {
                    if let Some(c) = follow_camera(traffic.as_ref(), id) {
                        cam = c;
                    }
                }
                if let Some(h) = humans_off.as_mut() {
                    h.sync(&renderer, &mut scene, cam.position);
                }
                // the time of day of this moment, and its lights: street lamps by night and
                // the vehicles' own (indicators, brake and tail lights) as they are now -
                // without them a snapshot showed no vehicle light at all
                let snap_clock = {
                    let mut c = start_clock(args);
                    c.time += t_s as f64 + service_seconds;
                    c
                };
                let daylight = omsi_sim::Daylight::compute(&snap_clock, envir.as_ref());
                world.set_lamps(&renderer, &mut scene, daylight.lamps_on);
                world.update_night_modes(&renderer, &mut scene, &snap_clock, daylight.brightness);
                {
                    let mut vehicles: Vec<&omsi_sim::VehicleInstance> = Vec::new();
                    if let Some(p) = player.as_ref() {
                        vehicles.push(&p.vehicle);
                    }
                    if let Some(t) = traffic.as_ref() {
                        vehicles.extend(t.cars.iter().map(|c| &c.vehicle));
                    }
                    vehicles.extend(remotes_off.remotes.values().map(|r| r.vehicle()));
                    lights::set_cone_strength(weather.fog.0, precip_of(&weather).1, daylight.night);
                    lights::upload_corona_textures(&mut renderer);
                    world.update_light_map_atlas(&renderer, cam.position);
                    lights::collect(&world, &mut scene, &daylight, cam.position, &vehicles);
                }
                let rate = precip_of(&weather).1;
                let mut lighting = weather_lighting(
                    &daylight,
                    &weather,
                    cloud_drift_at(&weather, snap_clock.time),
                    if rate > 0.0 {
                        (0.4 + rate).min(1.0)
                    } else {
                        0.0
                    },
                    settings.shadows,
                );
                lighting.inside = player.as_ref().and_then(|p| {
                    p.vehicle
                        .ty
                        .def
                        .bounding_box
                        .map(|bb| (p.vehicle.position, p.vehicle.heading, bb))
                });
                let puddle_surface = lighting.inside.and_then(|(o, _, _)| world.puddle_surface(o));
                lighting.puddle_ground = puddle_surface.map(|(h, _)| h);
                lighting.puddle_normal = puddle_surface.map_or(glam::Vec3::Z, |(_, n)| n);
                lighting.puddle_parts = player.as_ref().into_iter().flat_map(|p| &p.vehicle.trailers)
                    .filter_map(|t| t.ty.def.bounding_box.map(|bb| (t.position, t.heading, bb))).take(3).collect();
                lighting.detail = settings.detail_textures;
                world.finish_texture_upgrades(&renderer, &mut scene);
                let pixels = renderer.render_to_image(&mut scene, w, h, &cam, &lighting)?;
                let path = out.with_file_name(format!(
                    "{}_{ts:.1}.png",
                    out.file_stem().and_then(|s| s.to_str()).unwrap_or("snap")
                ));
                image::save_buffer(&path, &pixels, w, h, image::ColorType::Rgba8)?;
                if omsi_cfg::env::var_os("OMSI_BLEND_AB").is_some() {
                    // the same moment with the blended draws in the old order (by origin
                    // distance only), for a before/after picture of the draw order
                    renderer.blend_by_origin = true;
                    let old = renderer.render_to_image(&mut scene, w, h, &cam, &lighting)?;
                    renderer.blend_by_origin = false;
                    image::save_buffer(
                        path.with_extension("old.png"),
                        &old,
                        w,
                        h,
                        image::ColorType::Rgba8,
                    )?;
                }
                log::info!(
                    "snapshot at {ts:.1} s -> {} (camera ({:.1}, {:.1}, {:.1}) yaw {:.0})",
                    path.display(),
                    cam.position.x,
                    cam.position.y,
                    cam.position.z,
                    cam.yaw
                );
            }
        }
    }
    if let Some(id) = follow_id(args, traffic.as_ref()) {
        match follow_camera(traffic.as_ref(), id) {
            Some(c) => {
                log::info!(
                    "following car {id} at ({:.1}, {:.1}) heading {:.0}",
                    c.position.x,
                    c.position.y,
                    c.yaw
                );
                camera = c;
            }
            None => log::warn!("--follow: car {id} not found"),
        }
    }
    if let Some(g) = ground_gap.take() {
        g.report();
    }
    if let Some(t) = traffic.as_mut() {
        t.sync(&world, &renderer, &mut scene);
        let buses = t
            .cars
            .iter()
            .filter(|c| c.vehicle.ty.def.passenger_cabin.is_some())
            .count();
        // a car that is stopped where nothing is holding it, or one sitting inside another,
        // is a traffic bug: report both so they can be counted rather than guessed at
        let stuck = t.cars.iter().filter(|c| c.stopped > 60.0).count();
        let mut overlapping = 0;
        for (i, a) in t.cars.iter().enumerate() {
            for b in t.cars.iter().skip(i + 1) {
                if (a.vehicle.position - b.vehicle.position).length() < 2.5 {
                    overlapping += 1;
                }
            }
        }
        if let Some(p) = player_ref.as_ref() {
            let near = t
                .cars
                .iter()
                .map(|c| (c.vehicle.position - p.vehicle.position).length())
                .fold(f64::MAX, f64::min);
            if near < 40.0 {
                log::info!("nearest AI vehicle to the player: {near:.1} m");
            }
        }
        if omsi_cfg::env::var_os("OMSI_DEBUG_STUCK").is_some() {
            for l in t.stuck_report() {
                log::info!("stuck: {l}");
            }
        }
        if stuck > 0 || overlapping > 0 {
            log::info!(
                "traffic health: {stuck} stuck for over a minute, {overlapping} pairs overlapping"
            );
            if omsi_cfg::env::var_os("OMSI_DEBUG_STUCK").is_some() {
                for c in t.cars.iter().filter(|c| c.stopped > 30.0) {
                    log::info!("  waiting {:.0} s: car {} ({}) lane {} at ({:.1}, {:.1}) lead {:?} why {:?} {:.1} junction {}", c.stopped, c.id, c.vehicle.ty.def.type_name, c.state.lane, c.vehicle.position.x, c.vehicle.position.y, c.lead_car, c.why.0, c.why.1, c.junction_why);
                }
            }
            for c in t.cars.iter().filter(|c| c.stopped > 60.0).take(4) {
                log::info!(
                    "  stuck {:.0} s at ({:.0}, {:.0}) on lane {} of {} ({}): {}",
                    c.stopped,
                    c.vehicle.position.x,
                    c.vehicle.position.y,
                    c.state.lane,
                    t.net.lanes.len(),
                    c.vehicle.ty.def.type_name,
                    c.holding.as_deref().unwrap_or("-")
                );
            }
        }
        log::info!("traffic: {} vehicles ({buses} of them buses), {} waiting at red lights, mean speed {:.1} km/h", t.cars.len(), t.held_at_red, t.cars.iter().map(|c| c.state.speed).sum::<f32>() / t.cars.len().max(1) as f32 * 3.6);
        for c in t.cars.iter().filter(|c| c.is_bus()) {
            log::info!("scheduled {} at ({:.1}, {:.1}, {:.1}) heading {:.0} speed {:.1} km/h, {} stops left, at_station={} dwell={:.1} delay={:+.0} s", c.vehicle.ty.def.type_name, c.vehicle.position.x, c.vehicle.position.y, c.vehicle.position.z, c.vehicle.heading, c.state.speed * 3.6, c.bus.as_ref().map(|b| b.stops.len()).unwrap_or(0), c.at_station(), c.standing_for(t.day_time), c.bus.as_ref().map(|b| b.delay).unwrap_or(0.0));
            if omsi_cfg::env::var_os("OMSI_DEBUG_PROPS").is_some() {
                for v in [
                    "Matrix_Nr",
                    "Matrix_TerminusL1",
                    "Matrix_TerminusL2",
                    "SetLineTo",
                ] {
                    log::info!("  ${v} = {:?}", c.vehicle.str_var(v));
                }
                for v in [
                    "AI_target_index",
                    "IBIS_TerminusIndex",
                    "Matrix_RefreshCursor",
                    "elec_busbar_main",
                    "Font_7x6",
                ] {
                    log::info!("  {v} = {:?}", c.vehicle.var(v));
                }
                log::info!(
                    "  hof: {:?}, fonts: {}",
                    c.vehicle.host.hof.as_ref().map(|h| h.name.clone()),
                    c.vehicle.host.fonts.entries.len()
                );
            }
            let st = &c.state;
            let lane = &t.net.lanes[st.lane];
            log::info!("  lane {} (key {:?} len {:.1}) s={:.1} route_index {} of {} planned_next {:?} next stop {:?} light {:?}", st.lane, lane.key, lane.length(), st.s, st.route_index, st.route.len(), st.planned_next, c.next_stop(), st.planned_next.and_then(|n| t.net.lanes[n].traffic_light));
        }
    }
    if server {
        if let Some(l) = lan_off.take() {
            l.leave();
        }
        return Ok(());
    }
    let drive_spawn_z = spawn_z;
    if let Some(mut player) = player.take() {
        if let Some(secs) = args.drive {
            let start = drive_start;
            player.sync_transforms(
                &renderer,
                &mut scene,
                matches!(args.view.as_str(), "driver" | "pax"),
            );
            player.sync_driver(&renderer, &mut scene, 1.0 / 30.0, settings.driver, args.view == "driver");
            for (t, f) in std::mem::take(&mut player.vehicle.host.fired_file_triggers) {
                log::info!("announcement: {t} -> {f}");
            }
            // OMSI_DEBUG_REST: where the bus came to rest against the ground under it (a
            // bus sunk into the road, or hanging over it, after spawning)
            if omsi_cfg::env::var_os("OMSI_DEBUG_REST").is_some() {
                let p = player.vehicle.position;
                log::info!(
                    "rest: entry {} bus at ({:.1}, {:.1}, {:.2}) heading {:.0}; road/ground there {:?}, walk {:?}, spawned at z {:.2}",
                    args.entry,
                    p.x,
                    p.y,
                    p.z,
                    player.vehicle.heading,
                    world.ground_height(p.x, p.y),
                    world.walk_height(p.x, p.y),
                    drive_spawn_z
                );
                let v = &player.vehicle;
                let wheels: Vec<String> = tyre_lows(v, &world).iter().map(|(_, d)| format!("{d:+.3}")).collect();
                log::info!("rest wheels: {} lowest tyre points against the road: [{}]", v.ty.def.type_name, wheels.join(", "));
            }
            if omsi_cfg::env::var_os("OMSI_DEBUG_HUMANS").is_some() {
                log::info!(
                    "people per cabin path link: {:?}",
                    player.vehicle.host.humans_on_path_link
                );
            }
            if omsi_cfg::env::var_os("OMSI_DEBUG_PROPS").is_some() {
                if let Some(probe) = player.vehicle.host.ground_probe.clone() {
                    log::info!(
                        "ground probe: at the origin {:+.2} m, 1 m up {:+.2}, 1 m down {:+.2}",
                        probe(0.0, 0.0, 0.0),
                        probe(0.0, 0.0, 1.0),
                        probe(0.0, 0.0, -1.0)
                    );
                }
            }
            {
                let e = DRIVE_EXTREMES.lock();
                log::info!("drive extremes: pitch {:.1} (at ({:.1}, {:.1}, {:.1}), {:.1} s) bank {:.1}, origin {:+.2}..{:+.2} m over the ground", e.0, e.4 .0.x, e.4 .0.y, e.4 .0.z, e.4 .1, e.1, e.3, e.2);
            }
            if let Ok(list) = omsi_cfg::env::var("OMSI_DEBUG_VARS") {
                for v in list.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                    log::info!(
                        "after drive: {v} = {:?} / {:?}",
                        player.vehicle.var(v),
                        player.vehicle.str_var(v)
                    );
                }
            }
            if omsi_cfg::env::var_os("OMSI_DEBUG_PROPS").is_some() {
                for v in [
                    "IBIS_mode",
                    "IBIS_RouteIndex",
                    "IBIS_TerminusIndex",
                    "IBIS_TerminusCode",
                    "IBIS_LinieKurs",
                    "elec_busbar_main",
                    "Rain_Window_Front_Wetness",
                    "PrecipRate",
                    "GivenTicket",
                    "ticketprinter_ticket_selection",
                    "ticketprinter_ticket_preselection",
                    "ticketprinter_druckt",
                    "ticketprinter_ticket_pos",
                ] {
                    log::info!("after drive: {v} = {:?}", player.vehicle.var(v));
                }
                for v in [
                    "IBIS_terminus_name",
                    "IBIS_Complex_Line",
                    "IBIS_busstop_name",
                    "act_busstop",
                    "Haltestelle",
                    "Matrix_Nr",
                    "Matrix_TerminusL1",
                    "Matrix_Terminus",
                    "Matrix_Bitmapfilename",
                ] {
                    log::info!("after drive: ${v} = {:?}", player.vehicle.str_var(v));
                }
                for v in [
                    "Matrix_RefreshCursor",
                    "matrix_steckschild_Termindex",
                    "Font_16x9",
                    "elec_busbar_main_sw",
                    "AI_target_index",
                ] {
                    log::info!("after drive: {v} = {:?}", player.vehicle.var(v));
                }
                for (i, st) in player.vehicle.host.script_textures.iter().enumerate() {
                    let lit = st.rgba.chunks_exact(4).filter(|p| p[3] > 0).count();
                    log::info!(
                        "script texture {i}: {}x{} {lit} pixels with alpha, dirty={} locked={} mipmaps={}",
                        st.width,
                        st.height,
                        st.dirty,
                        st.locked,
                        st.mipmaps
                    );
                }
                if let Ok(dir) = omsi_cfg::env::var("OMSI_DUMP_SCRIPTTEX") {
                    dump_display_textures(&player.vehicle, Path::new(&dir));
                }
                log::info!(
                    "fonts registered: {:?}",
                    player
                        .vehicle
                        .host
                        .fonts
                        .entries
                        .iter()
                        .map(|f| (f.0.clone(), f.1.is_some()))
                        .collect::<Vec<_>>()
                );
            }
            log::info!(
                "collision: {} crashes, the last {:.1} kJ; unread energy {:.1} kJ, last point {:?}",
                player.vehicle.crashes,
                player.vehicle.last_impact / 1000.0,
                player.vehicle.host.coll_energy,
                player.vehicle.host.coll_pos
            );
            if omsi_cfg::env::var_os("OMSI_DEBUG_COLLISION").is_some() {
                if let Some(cw) = player.vehicle.collision.as_ref() {
                    let p = player.vehicle.position;
                    let mut near: Vec<(f64, &omsi_sim::collision::Obb)> = cw
                        .boxes
                        .iter()
                        .map(|b| ((b.center - p.truncate()).length(), b))
                        .collect();
                    near.sort_by(|a, b| a.0.total_cmp(&b.0));
                    for (d, b) in near.iter().take(4) {
                        log::info!("  nearest obstacle {:.1} m away at ({:.1}, {:.1}) half {:.1}x{:.1} z {:.1}..{:.1}", d, b.center.x, b.center.y, b.half.x, b.half.y, b.z0, b.z1);
                    }
                    log::info!(
                        "  bus at ({:.1}, {:.1}, {:.1}) box {:?}",
                        p.x,
                        p.y,
                        p.z,
                        player.vehicle.ty.def.bounding_box
                    );
                }
            }
            if omsi_cfg::env::var_os("OMSI_DEBUG_PHYSICS").is_some() {
                let gaps = |v: &omsi_sim::VehicleInstance| {
                    v.wheel_ground_gaps()
                        .iter()
                        .map(|(f, g)| {
                            format!("{} {:+.3}", f.rsplit(['\\', '/']).next().unwrap_or(f), g)
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                log::info!("tyres over the ground: {}", gaps(&player.vehicle));
                if let Some(t) = traffic.as_ref() {
                    for c in t.cars.iter().filter(|c| {
                        (c.vehicle.position - player.vehicle.position).length() < 1500.0
                    }) {
                        let def = &c.vehicle.ty.def;
                        let name = if def.type_name.is_empty() {
                            def.path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        } else {
                            def.type_name.clone()
                        };
                        log::info!("  AI {} {}: {}", c.id, name, gaps(&c.vehicle));
                    }
                }
            }
            if let Some(rb) = &player.vehicle.rigid {
                log::info!("rigid: pos ({:.1}, {:.1}, {:.2}) heading {:.1} pitch {:.2} bank {:.2} v {:?} compressions {:?}", player.vehicle.position.x, player.vehicle.position.y, player.vehicle.position.z, player.vehicle.heading, player.vehicle.pitch, player.vehicle.bank, rb.velocity, rb.wheels.iter().map(|w| (w.compression * 1000.0).round() / 1000.0).collect::<Vec<_>>());
            }
            log::info!(
                "drove {:.1} m in {secs} s, now {:.1} km/h, engine_n={:?} M_Wheel={:?} gear={:?}",
                (player.vehicle.position - start).length(),
                player.vehicle.physics.velocity_kmh(),
                player.vehicle.var("engine_n"),
                player.vehicle.var("M_Wheel"),
                player.vehicle.var("antrieb_getr_aktugang")
            );
            if let Some(mins) = player.vehicle.repair_minutes() {
                log::info!("damage: the workshop would need {mins:.0} min (elec {:?} engine {:?} drive {:?})", player.vehicle.var("elec_failure_general"), player.vehicle.var("engine_failure_general"), player.vehicle.var("antrieb_failure_general"));
            }
        }
        if let Some(spec) = args.click.clone() {
            let v: Vec<f32> = spec
                .split(',')
                .filter_map(|x| x.trim().parse().ok())
                .collect();
            if v.len() >= 2 {
                let (w, h) = args
                    .size
                    .split_once('x')
                    .map(|(a, b)| {
                        (
                            a.parse::<u32>().unwrap_or(1600),
                            b.parse::<u32>().unwrap_or(900),
                        )
                    })
                    .unwrap_or((1600, 900));
                // the same camera the picture is taken with, head turn and all
                let cam =
                    player.camera_look(&args.view, &camera, look_of(&args), offscreen_orbit());
                let (o, d) = cursor_ray(&cam, v[0], v[1], w as f32, h as f32);
                match player.click(o, d, pixel_angle(&cam, h as f32) * 6.0) {
                    Some(i) => log::info!(
                        "click at ({}, {}) hit mesh {i} '{}'",
                        v[0],
                        v[1],
                        player.vehicle.ty.model.meshes[player.vehicle.ty.meshes[i].def_index].file
                    ),
                    None => log::info!(
                        "click at ({}, {}) hit nothing with a [mouseevent]",
                        v[0],
                        v[1]
                    ),
                }
                // where the clickable switches actually are on this screen
                let vp = cam.view_proj(w as f32 / h as f32, player.vehicle.position);
                let mut switches: Vec<(String, f32, f32)> = Vec::new();
                {
                    let (mut total, mut invisible, mut offscreen) = (0, 0, 0);
                    for (i, vm) in player.vehicle.ty.meshes.iter().enumerate() {
                        let def = &player.vehicle.ty.model.meshes[vm.def_index];
                        if def.mouse_event.is_none() {
                            continue;
                        }
                        total += 1;
                        if !player.vehicle.mesh_props[i].visible {
                            invisible += 1;
                            log::info!(
                                "  switch '{}' is invisible ([visible] {:?})",
                                def.mouse_event.as_deref().unwrap_or(""),
                                def.visible
                            );
                        } else {
                            offscreen += 1;
                        }
                    }
                    log::info!("switches on this vehicle: {total}, of them {invisible} invisible, {offscreen} visible (some off screen)");
                }
                for (i, vm) in player.vehicle.ty.meshes.iter().enumerate() {
                    let def = &player.vehicle.ty.model.meshes[vm.def_index];
                    let Some(ev) = def.mouse_event.as_ref() else {
                        continue;
                    };
                    if !player.vehicle.mesh_props[i].visible || vm.data.positions.is_empty() {
                        continue;
                    }
                    let mut c = Vec3::ZERO;
                    for q in &vm.data.positions {
                        c += *q;
                    }
                    c /= vm.data.positions.len() as f32;
                    let world = player.vehicle.mesh_local_transform(i).transform_point3(c);
                    let ndc = vp.project_point3(world);
                    if ndc.z < 0.0 || ndc.z > 1.0 || ndc.x.abs() > 1.0 || ndc.y.abs() > 1.0 {
                        continue;
                    }
                    let (sx, sy) = (
                        (ndc.x * 0.5 + 0.5) * w as f32,
                        (0.5 - ndc.y * 0.5) * h as f32,
                    );
                    log::info!(
                        "  switch '{ev}' at screen ({sx:.0}, {sy:.0}): {}",
                        describe::names(&args.root, &settings.language).control(ev)
                    );
                    switches.push((ev.clone(), sx, sy));
                }
                // Aim at each of them in turn and say which one would actually be operated:
                // a switch that cannot be hit where it is drawn, or that hands the click to
                // its neighbour, is unusable with the mouse however good the rest is.
                if omsi_cfg::env::var_os("OMSI_CLICK_ALL").is_some() {
                    let spread = pixel_angle(&cam, h as f32) * 6.0;
                    let (mut hit, mut wrong, mut missed) = (0, 0, 0);
                    for (ev, sx, sy) in &switches {
                        let (o, d) = cursor_ray(&cam, *sx, *sy, w as f32, h as f32);
                        match player.pick(o, d, spread).map(|i| {
                            player.vehicle.ty.model.meshes[player.vehicle.ty.meshes[i].def_index]
                                .mouse_event
                                .clone()
                                .unwrap_or_default()
                        }) {
                            Some(got) if &got == ev => hit += 1,
                            Some(got) => {
                                wrong += 1;
                                log::info!(
                                    "  aiming at '{ev}' ({sx:.0}, {sy:.0}) operates '{got}'"
                                );
                            }
                            None => {
                                missed += 1;
                                log::info!("  aiming at '{ev}' ({sx:.0}, {sy:.0}) hits nothing");
                            }
                        }
                    }
                    log::info!("switch test: {hit} of {} operable, {wrong} hand the click to a neighbour, {missed} unreachable", switches.len());
                }
                // Does the script answer at all? Every [mouseevent] of the model is fired
                // here (whether it is on screen or not) and the vehicle's variables are
                // compared before and after: a switch whose trigger the script does not
                // define, or that changes nothing, is a switch that does nothing when
                // clicked.
                if omsi_cfg::env::var_os("OMSI_TRIGGER_ALL").is_some() {
                    let names: Vec<String> = {
                        let mut v: Vec<String> = player
                            .vehicle
                            .ty
                            .meshes
                            .iter()
                            .filter_map(|m| {
                                player.vehicle.ty.model.meshes[m.def_index]
                                    .mouse_event
                                    .clone()
                            })
                            .collect();
                        v.sort();
                        v.dedup();
                        v
                    };
                    let (mut dead, mut silent, mut ok) = (Vec::new(), Vec::new(), 0);
                    // Each switch is tried from the same variables, and what it changed is
                    // measured against the same time passing without it: with the engine
                    // running half the variables move by themselves, and counting those made
                    // every switch look alive. Variables that differ between two identical
                    // runs (random numbers) are left out.
                    let v0 = &player.vehicle;
                    let start = (
                        v0.state.clone(),
                        v0.host.clock.clone(),
                        v0.position,
                        v0.heading,
                        v0.physics.clone(),
                        v0.rigid.clone(),
                    );
                    let restore = |v: &mut omsi_sim::VehicleInstance| {
                        v.state = start.0.clone();
                        v.host.clock = start.1.clone();
                        v.position = start.2;
                        v.heading = start.3;
                        v.physics = start.4.clone();
                        v.rigid = start.5.clone();
                    };
                    // One try: press, drag by `d`, hold, let go. Returns whether the script
                    // has the trigger, the variables while held and after letting go, and the
                    // sound events. The idle run (no switch) is the same with nothing pressed.
                    let run = |v: &mut omsi_sim::VehicleInstance,
                               name: Option<&str>,
                               d: (f32, f32)|
                               -> (bool, Vec<f32>, Vec<f32>, Vec<String>) {
                        restore(v);
                        v.host.fired_triggers.clear();
                        v.host.fired_file_triggers.clear();
                        let mut exists = false;
                        if let Some(name) = name {
                            exists = v.trigger(name);
                            v.host.mouse = d;
                            exists |= v.trigger(&format!("{name}_drag"));
                            v.host.mouse = (0.0, 0.0);
                        }
                        for _ in 0..6 {
                            v.update(0.05);
                        }
                        let held = v.state.vars.clone();
                        if let Some(name) = name {
                            v.trigger(&format!("{name}_off"));
                        }
                        for _ in 0..6 {
                            v.update(0.05);
                        }
                        let mut sounds: Vec<String> = std::mem::take(&mut v.host.fired_triggers);
                        sounds.extend(
                            std::mem::take(&mut v.host.fired_file_triggers)
                                .into_iter()
                                .map(|(t, _)| t),
                        );
                        (exists, held, v.state.vars.clone(), sounds)
                    };
                    let (_, idle_held, idle, idle_sounds) =
                        run(&mut player.vehicle, None, (0.0, 0.0));
                    let (_, idle_held2, idle2, _) = run(&mut player.vehicle, None, (0.0, 0.0));
                    let differs = |a: f32, b: f32| (a - b).abs() > 1e-4 * (1.0 + a.abs());
                    // what moves by itself between two identical runs (random numbers)
                    let noisy: Vec<bool> = (0..idle.len())
                        .map(|k| differs(idle[k], idle2[k]) || differs(idle_held[k], idle_held2[k]))
                        .collect();
                    for name in &names {
                        let mut exists = false;
                        let mut changed: Vec<usize> = Vec::new();
                        let mut played: Vec<String> = Vec::new();
                        // knobs and levers answer to one drag direction only (the driver's
                        // door and window sideways, the parking brake and the sign clamp
                        // up or down)
                        for d in [(0.0, 6.0), (0.0, -6.0), (6.0, 0.0), (-6.0, 0.0)] {
                            let (e, held, after, sounds) =
                                run(&mut player.vehicle, Some(name.as_str()), d);
                            exists |= e;
                            changed = (0..idle.len().min(after.len()))
                                .filter(|&k| {
                                    !noisy[k]
                                        && (differs(after[k], idle[k])
                                        || differs(held[k], idle_held[k]))
                                })
                                .collect();
                            played = sounds
                                .into_iter()
                                .filter(|t| !idle_sounds.contains(t))
                                .collect();
                            if !e || !changed.is_empty() || !played.is_empty() {
                                break;
                            }
                        }
                        if !exists {
                            dead.push(name.clone());
                        } else if changed.is_empty() && played.is_empty() {
                            silent.push(name.clone());
                        } else {
                            ok += 1;
                            if omsi_cfg::env::var_os("OMSI_DEBUG_TRIGGERS").is_some() {
                                let names: Vec<&str> = changed
                                    .iter()
                                    .take(6)
                                    .filter_map(|k| {
                                        player
                                            .vehicle
                                            .ty
                                            .program
                                            .var_names
                                            .get(*k)
                                            .map(|s| s.as_str())
                                    })
                                    .collect();
                                log::info!(
                                    "  {name}: {} variables, e.g. {names:?}; sounds {played:?}",
                                    changed.len()
                                );
                            }
                        }
                    }
                    restore(&mut player.vehicle);
                    log::info!("trigger test: {ok} of {} switches do something; {} have no trigger in the script: {:?}", names.len(), dead.len(), dead);
                    log::info!("  {} fire a trigger but change nothing (may need power or another switch first): {:?}", silent.len(), silent);
                }
                // and a drag, so the offscreen test can turn a knob too
                if let Some(v3) = v.get(2) {
                    player.drag(*v3, v.get(3).copied().unwrap_or(0.0));
                }
                player.release();
                // Let the switch move: the picture is taken after this, and a lever that
                // has been flipped only travels when the vehicle's scripts and animations
                // are run once more (in the window that happens on the next frame anyway).
                for _ in 0..12 {
                    player.vehicle.update(0.05);
                }
                player.sync_transforms(
                    &renderer,
                    &mut scene,
                    matches!(args.view.as_str(), "driver" | "pax"),
                );
                player.sync_driver(&renderer, &mut scene, 1.0 / 30.0, settings.driver, args.view == "driver");
                if let Ok(names) = omsi_cfg::env::var("OMSI_DEBUG_VARS") {
                    for n in names.split(',') {
                        log::info!("after click: {n} = {:?}", player.vehicle.var(n.trim()));
                    }
                }
            }
        }
        if args.cam.is_none() && args.view != "free" && args.follow.is_none() {
            camera = player.camera_look(&args.view, &camera, look_of(&args), offscreen_orbit());
            if args.view == "outside" {
                camera = player.camera_clipped(camera, &world, offscreen_orbit(), 0.0);
            }
        }
        vehicle_camera(&player, &mut camera);
        // the driver at the wheel, as the window has him every frame (not posed, he was
        // not drawn - or stood in the aisle in the file's T-pose)
        player.sync_driver(&renderer, &mut scene, 1.0 / 30.0, settings.driver, args.view == "driver");
        player_ref = Some(player);
    }
    if let Some(mut h) = humans_off.take() {
        let center = player_ref
            .as_ref()
            .map(|p| p.vehicle.position)
            .unwrap_or(camera.position);
        h.sync(&renderer, &mut scene, center);
        log::info!(
            "passengers: {} people ({}), request {:?}, paid {:?}, change due {:?}",
            h.people.len(),
            h.summary(),
            h.request,
            h.paid,
            h.change_due
        );
        if omsi_cfg::env::var_os("OMSI_DEBUG_HUMANS").is_some() {
            let centre = player_ref
                .as_ref()
                .map(|p| p.vehicle.position)
                .unwrap_or(camera.position);
            for (k, p) in h
                .positions()
                .into_iter()
                .filter(|(_, p)| (*p - centre).length() < 40.0)
            {
                log::info!("  {k} at ({:.1}, {:.1}, {:.1})", p.x, p.y, p.z);
            }
        }
        if omsi_cfg::env::var_os("OMSI_DEBUG_HUMANS").is_some() {
            for p in &h.people {
                log::info!(
                    "  {:?} at ({:.1}, {:.1}, {:.1})",
                    p.state_name(),
                    p.position().x,
                    p.position().y,
                    p.position().z
                );
            }
        }
        if let Some(p) = player_ref.as_ref() {
            log::info!(
                "player doors {:?} PAX_Entry_Open {:?} PAX_Entry_Req {:?} PAX_Exit_Open {:?} PAX_Exit_Req {:?} haltewunsch {:?} speed {:.1}",
                (0..4).map(|i| p.vehicle.var(&format!("door_{i}")).unwrap_or(0.0)).collect::<Vec<_>>(),
                (0..2).map(|i| p.vehicle.var(&format!("PAX_Entry{i}_Open")).unwrap_or(0.0)).collect::<Vec<_>>(),
                (0..2).map(|i| p.vehicle.var(&format!("PAX_Entry{i}_Req")).unwrap_or(0.0)).collect::<Vec<_>>(),
                (0..2).map(|i| p.vehicle.var(&format!("PAX_Exit{i}_Open")).unwrap_or(0.0)).collect::<Vec<_>>(),
                (0..2).map(|i| p.vehicle.var(&format!("PAX_Exit{i}_Req")).unwrap_or(0.0)).collect::<Vec<_>>(),
                p.vehicle.var("haltewunsch"),
                p.vehicle.physics.velocity_kmh()
            );
            if omsi_cfg::env::var_os("OMSI_DEBUG_HUMANS").is_some() {
                for (id, pos, rot, name) in world.bus_stops.lock().iter() {
                    log::info!(
                        "  bus stop {id} '{name}' at ({:.1}, {:.1}) heading {rot:.0}",
                        pos.x,
                        pos.y
                    );
                }
            }
            if let Some((id, pos, _, name)) = world.bus_stops.lock().iter().min_by(|a, b| (a.1 - p.vehicle.position).length().total_cmp(&(b.1 - p.vehicle.position).length())) {
                log::info!(
                    "nearest bus stop {id} '{name}' at ({:.1}, {:.1}) is {:.1} m away",
                    pos.x,
                    pos.y,
                    (*pos - p.vehicle.position).length()
                );
            }
        }
    }
    // OMSI_CHECK_ENTRIES: is there anything to stand on where a player is put down?
    if omsi_cfg::env::var_os("OMSI_CHECK_ENTRIES").is_some() {
        let mut bare = 0;
        for ep in &world.global.entry_points {
            let Some((pos, rot)) = world.entry_point_place(ep) else {
                log::info!(
                    "entry {:3} \"{}\": object {} not loaded",
                    ep.index,
                    ep.name,
                    ep.object_id
                );
                bare += 1;
                continue;
            };
            let q = ep.quat;
            let qyaw = (2.0 * q[1].atan2(q[3])).to_degrees();
            let rec = crate::spawn::recorded_entry_pos(ep, pos);
            log::info!("entry {:3} \"{}\": object heading {:.1}, record quaternion yaw {:.1} (q {:?}), object at ({:.1}, {:.1}, {:.1}), recorded at {:?}", ep.index, ep.name, rot[0], qyaw, q, pos.x, pos.y, pos.z, rec.map(|r| (r.x, r.y, r.z)));
            let road = world.ground_height(pos.x, pos.y);
            let walk = world.walk_height(pos.x, pos.y);
            let terrain = world.ground_terrain(pos.x, pos.y);
            if road.is_none() {
                bare += 1;
                log::info!("entry {:3} \"{}\" at ({:.0}, {:.0}) heading {:.0}: NO ROAD (surface {:?}, terrain {:?})", ep.index, ep.name, pos.x, pos.y, rot[0], walk, terrain);
            }
        }
        log::info!(
            "entry point check: {bare} of {} entry points have no road surface under them",
            world.global.entry_points.len()
        );
    }
    // OMSI_CHECK_OBSTACLES: sweep a bus-sized box along every driving lane and list the
    // obstacle boxes it runs into - the "invisible walls" a player meets on an open road
    if omsi_cfg::env::var_os("OMSI_CHECK_OBSTACLES").is_some() {
        if let Some(t) = traffic.as_ref() {
            let boxes = world.collision.lock().clone();
            // (parked cars stand beside the lanes by design; the traffic steers round them)
            let parked: std::collections::HashSet<i64> = world.tile_state.lock().values().flat_map(|st| st.parked_boxes.iter().map(|b| b.id).collect::<Vec<_>>()).collect();
            let mut hits: std::collections::BTreeMap<i64, (omsi_sim::collision::Obb, usize, DVec3)> = Default::default();
            let mut probes = 0usize;
            for l in t
                .net
                .lanes
                .iter()
                .filter(|l| l.kind == omsi_sim::traffic::LaneKind::Street && !l.invisible)
            {
                let len = l.length();
                let mut s = 0.0f32;
                while s < len {
                    let (p, hdg) = l.at(s);
                    s += 2.0;
                    probes += 1;
                    // a bus body: 2.5 m wide, from 0.35 m over the lane (its floor clears
                    // kerbs and covers) to 3.2 m
                    let probe = omsi_sim::collision::Obb::from_box([2.4, 2.0, 2.85, 0.0, 0.0, 1.775], p, hdg as f64);
                    // pitched with the lane, as a bus on it is
                    let (ahead, behind) = (l.at((s + 1.0).min(len)).0, l.at((s - 3.0).max(0.0)).0);
                    let fwd = (ahead - behind).try_normalize().unwrap_or(DVec3::Y);
                    let right = fwd.cross(DVec3::Z).normalize_or(DVec3::X);
                    let up = right.cross(fwd);
                    let solid = omsi_sim::collision::Box3 { center: p + up * 1.775, axes: [right, fwd, up], half: DVec3::new(1.2, 1.0, 1.425) };
                    for b in boxes.obstacles_near_solid(&probe, Some(&solid)) {
                        if b.overlaps(&probe) && !parked.contains(&b.id) {
                            hits.entry(b.id).or_insert((b, 0, p)).1 += 1;
                        }
                    }
                }
            }
            log::info!("obstacle check: {probes} lane points, {} obstacle boxes stand on a driving lane", hits.len());
            // `OMSI_LANES_NEAR=x,y,r`: the driving lanes passing there (where to put a test bus)
            if let Ok(v) = omsi_cfg::env::var("OMSI_LANES_NEAR") {
                let v: Vec<f64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                if v.len() == 3 {
                    let c = glam::DVec2::new(v[0], v[1]);
                    for (i, l) in t.net.lanes.iter().enumerate().filter(|(_, l)| l.kind == omsi_sim::traffic::LaneKind::Street) {
                        if let Some(k) = l.points.iter().position(|p| (p.truncate() - c).length() < v[2]) {
                            let p = l.points[k];
                            log::info!("  lane {i} {} at ({:.1}, {:.1}, {:.1}) heading {:.1}, {:.0} m long, width {:.1}", l.name, p.x, p.y, p.z, l.headings[k], l.length(), l.width);
                        }
                    }
                }
            }
            for (id, (b, n, p)) in &hits {
                log::info!(
                    "  obstacle key {id} at ({:.1}, {:.1}) z {:.1}..{:.1} half {:.1}x{:.1} hdg {:.0}: {n} lane points, first at ({:.1}, {:.1}, {:.1})",
                    b.center.x, b.center.y, b.z0, b.z1, b.half.x, b.half.y, b.heading.to_degrees(), p.x, p.y, p.z
                );
            }
        }
    }
    // OMSI_CHECK_WHEELS: probe the ground along the wheel tracks of every driving lane as a
    // tyre does - a face a little over the road there is an invisible wall to the wheels, a
    // ground far off the lane's height a hump or a hole
    if omsi_cfg::env::var_os("OMSI_CHECK_WHEELS").is_some() {
        if let Some(t) = traffic.as_ref() {
            let (mut points, mut walls, mut steps) = (0usize, Vec::new(), Vec::new());
            for l in t.net.lanes.iter().filter(|l| l.kind == omsi_sim::traffic::LaneKind::Street && !l.invisible) {
                let len = l.length();
                let mut s = 1.0f32;
                while s < len - 1.0 {
                    let (p, hdg) = l.at(s);
                    s += 2.0;
                    let h = (hdg as f64).to_radians();
                    let right = DVec3::new(h.cos(), -h.sin(), 0.0);
                    for side in [-1.0, 1.0] {
                        let w = p + right * side;
                        points += 1;
                        // (from the ground under the wheel, as the tyre probes: 0.8 of a
                        // half-metre radius over it)
                        let g = crate::scene::drive_probe(&world.terrains, &world.surfaces, w.x, w.y, p.z + 0.4);
                        let g = match g.below {
                            Some(b) => crate::scene::drive_probe(&world.terrains, &world.surfaces, w.x, w.y, b + 0.4),
                            None => g,
                        };
                        let on_lane = g.below.is_some_and(|b| (b - p.z).abs() <= 0.3);
                        if let (true, Some(a)) = (on_lane, g.above.filter(|a| *a < p.z + 2.0)) {
                            walls.push((w, a - p.z));
                        } else if let Some(b) = g.below.filter(|b| (b - p.z).abs() > 0.3) {
                            steps.push((w, b - p.z));
                        } else if g.below.is_none() {
                            steps.push((w, f64::NAN));
                        }
                    }
                }
            }
            log::info!("wheel check: {points} wheel points, {} under a face (a wall to the tyre), {} off the lane's height by more than 30 cm", walls.len(), steps.len());
            let mut hist = [0usize; 17];
            for (_, d) in &walls {
                hist[((d * 10.0) as usize).min(16)] += 1;
            }
            log::info!("  wall faces by height over the lane (0.1 m steps from 0): {hist:?}");
            for (w, d) in walls.iter().take(40) {
                log::info!("  wall at ({:.1}, {:.1}, {:.1}): face {d:+.2} m over the lane", w.x, w.y, w.z);
            }
            for (w, d) in steps.iter().take(40) {
                log::info!("  ground at ({:.1}, {:.1}, {:.1}): {d:+.2} m off the lane", w.x, w.y, w.z);
            }
        }
    }
    // What the map says about traffic on its roads
    if omsi_cfg::env::var_os("OMSI_CHECK_ROADS").is_some() {
        if let Some(t) = traffic.as_ref() {
            let street: Vec<&omsi_sim::traffic::Lane> = t
                .net
                .lanes
                .iter()
                .filter(|l| l.kind == omsi_sim::traffic::LaneKind::Street)
                .collect();
            let no_cars = street.iter().filter(|l| l.no_cars).count();
            let zero = street
                .iter()
                .filter(|l| !l.no_cars && l.density <= 0.001)
                .count();
            let quiet = street
                .iter()
                .filter(|l| l.density > 0.001 && l.density < 0.9)
                .count();
            log::info!("traffic rules: {} street lanes, {no_cars} closed to cars, {zero} with density 0, {quiet} quieter than normal", street.len());
        }
    }
    // OMSI_CHECK_ROADS: walk every driving lane and report where no road surface is drawn
    // under it, which is what "the road is missing here" looks like from the driver's seat
    if omsi_cfg::env::var_os("OMSI_CHECK_ROADS").is_some() {
        if let Some(t) = traffic.as_ref() {
            let mut checked = 0usize;
            let mut naked = 0usize;
            let mut worst: Vec<(DVec3, f64)> = Vec::new();
            let mut runs: Vec<(DVec3, f32)> = Vec::new();
            for l in t
                .net
                .lanes
                .iter()
                .filter(|l| l.kind == omsi_sim::traffic::LaneKind::Street && !l.invisible)
            {
                let len = l.length();
                let mut s = 0.0f32;
                let mut run_start: Option<(DVec3, f32)> = None;
                while s < len {
                    let (p, _) = l.at(s);
                    checked += 1;
                    let tx = (p.x / omsi_map::tile_size()).floor() as i32;
                    let ty = (p.y / omsi_map::tile_size()).floor() as i32;
                    let lx = (p.x - tx as f64 * omsi_map::tile_size()) as f32;
                    let ly = (p.y - ty as f64 * omsi_map::tile_size()) as f32;
                    let road = world
                        .surfaces
                        .read()
                        .get(&(tx, ty))
                        .and_then(|su| su.sample_road(lx, ly).or_else(|| su.sample(lx, ly)))
                        .map(|h| h as f64);
                    match road {
                        Some(h) if (h - p.z).abs() < 1.0 => {
                            if let Some((from, at)) = run_start.take() {
                                if s - at >= 15.0 {
                                    runs.push((from, s - at));
                                }
                            }
                        }
                        _ => {
                            naked += 1;
                            if run_start.is_none() {
                                run_start = Some((p, s));
                            }
                            if worst.len() < 6 {
                                worst.push((p, road.map(|h| h - p.z).unwrap_or(f64::NAN)));
                            }
                        }
                    }
                    s += 5.0;
                }
                if let Some((from, at)) = run_start.take() {
                    if len - at >= 15.0 {
                        runs.push((from, len - at));
                    }
                }
            }
            // And the other way a road goes missing: the ground is drawn over it. The
            // terrain is only cut where it lies within a hand's width of the surface, so
            // wherever it stands higher than that the road is buried under a mound.
            {
                let mut buried = 0usize;
                let mut checked2 = 0usize;
                let mut worst_b: Vec<(DVec3, f64)> = Vec::new();
                let mut slight: Vec<(DVec3, f64)> = Vec::new();
                for l in t
                    .net
                    .lanes
                    .iter()
                    .filter(|l| l.kind == omsi_sim::traffic::LaneKind::Street && !l.invisible)
                {
                    let len = l.length();
                    let mut s = 0.0f32;
                    while s < len {
                        let (p, _) = l.at(s);
                        s += 5.0;
                        let tx = (p.x / omsi_map::tile_size()).floor() as i32;
                        let ty = (p.y / omsi_map::tile_size()).floor() as i32;
                        let lx = (p.x - tx as f64 * omsi_map::tile_size()) as f32;
                        let ly = (p.y - ty as f64 * omsi_map::tile_size()) as f32;
                        let Some(ground) = world.ground_terrain(p.x, p.y) else {
                            continue;
                        };
                        // ground that is cut away under the road is not in the way
                        if world
                            .surfaces
                            .read()
                            .get(&(tx, ty))
                            .map(|su| su.cut_at(lx, ly, ground as f32, 0.12))
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        checked2 += 1;
                        // the lane's own height is the road the wheels run on
                        let over = ground - p.z;
                        // (a road a few centimetres under the ground: the teeth of terrain
                        // coming through the carriageway)
                        if over > 0.005 && over <= 0.15 {
                            slight.push((p, over));
                        }
                        if over > 0.15 {
                            buried += 1;
                            if worst_b.len() < 100000 {
                                worst_b.push((p, over));
                            }
                        }
                    }
                }
                log::info!("slightly buried: {} road points 0.5..15 cm under the ground; e.g. {:?}", slight.len(), slight.iter().step_by((slight.len() / 6).max(1)).take(6).map(|(p, o)| format!("({:.0}, {:.0}, {:.1}) {:.2} m", p.x, p.y, p.z, o)).collect::<Vec<_>>());
                worst_b.sort_by(|a, b| b.1.total_cmp(&a.1));
                log::info!("buried check: {buried} of {checked2} road points have ground standing over them ({:.1}%)", buried as f32 / checked2.max(1) as f32 * 100.0);
                for (lo, hi) in [
                    (0.15, 0.3),
                    (0.3, 0.6),
                    (0.6, 1.0),
                    (1.0, 2.0),
                    (2.0, 5.0),
                    (5.0, 1e9),
                ] {
                    let n = worst_b.iter().filter(|(_, o)| *o >= lo && *o < hi).count();
                    log::info!("   {lo:>4} .. {hi:>4} m: {n}");
                }
                for (p, over) in worst_b.iter().take(8) {
                    log::info!(
                        "  ground {over:.2} m over the road at ({:.0}, {:.0}, {:.1})",
                        p.x,
                        p.y,
                        p.z
                    );
                }
            }
            // And where a bus wheel (0.47 m) rolling along the lane meets a face it cannot
            // climb: a step over omsi_sim::rigid::CLIMB radii, lower than the hub and a metre
            // and a half more (up to the hub and 5 cm the wheels used to climb anyway).
            {
                let r = 0.47f64;
                let (climb, hub, overhead) = (omsi_sim::rigid::CLIMB as f64 * r, r + 0.05, r + 1.5);
                let (mut rolled, mut steep, mut high) = (0usize, 0usize, 0usize);
                let mut faces: Vec<(DVec3, f64, f32)> = Vec::new();
                // (a lane's own height is not always its road's: on some ramps it runs a metre
                // below the surface)
                let surface = |p: DVec3| {
                    scene::drive_probe(&world.terrains, &world.surfaces, p.x, p.y, p.z + 1.5).below
                };
                for l in t
                    .net
                    .lanes
                    .iter()
                    .filter(|l| l.kind == omsi_sim::traffic::LaneKind::Street && !l.invisible)
                {
                    let len = l.length();
                    let mut ground = surface(l.at(0.0).0);
                    let mut s = 0.1f32;
                    while s < len {
                        let (p, dir) = l.at(s);
                        s += 0.1;
                        let Some(g) = ground else {
                            ground = surface(p);
                            continue;
                        };
                        rolled += 1;
                        let wheel = scene::drive_probe(
                            &world.terrains,
                            &world.surfaces,
                            p.x,
                            p.y,
                            g + climb,
                        );
                        match wheel.above.filter(|z| *z < g + overhead) {
                            Some(face) => {
                                if face - g <= hub {
                                    steep += 1;
                                } else {
                                    high += 1;
                                }
                                faces.push((p, face - g, dir));
                                // stopped here; the lane goes on from its own road
                                ground = surface(p);
                            }
                            None => ground = wheel.below.or(ground),
                        }
                    }
                }
                log::info!("wheel check: {} faces a bus wheel cannot climb along {:.0} km of driving lanes: {steep} steps of {climb:.2}..{hub:.2} m (climbed before), {high} higher", steep + high, rolled as f64 * 0.1 / 1000.0);
                // one line per place (faces within 10 m of each other), lowest first
                let mut places: Vec<(DVec3, f64, f64, usize, f32)> = Vec::new();
                for (p, h, dir) in &faces {
                    match places
                        .iter_mut()
                        .find(|q| (q.0 - *p).truncate().length() < 10.0)
                    {
                        Some(q) => {
                            q.1 = q.1.min(*h);
                            q.2 = q.2.max(*h);
                            q.3 += 1;
                        }
                        None => places.push((*p, *h, *h, 1, *dir)),
                    }
                }
                places.sort_by(|a, b| a.1.total_cmp(&b.1));
                log::info!("  at {} places", places.len());
                for (p, lo, hi, n, dir) in places.iter().take(40) {
                    log::info!(
                        "  {lo:.2}..{hi:.2} m ({n}x) at ({:.1}, {:.1}, {:.1}) heading {dir:.0}",
                        p.x,
                        p.y,
                        p.z
                    );
                }
            }
            runs.sort_by(|a, b| b.1.total_cmp(&a.1));
            log::info!("road check: {naked} of {checked} points along the driving lanes have no road surface under them ({:.1}%)", naked as f32 / checked.max(1) as f32 * 100.0);
            // OMSI_CHECK_SPLINES: chained splines whose ends do not meet in height
            if omsi_cfg::env::var_os("OMSI_CHECK_SPLINES").is_some() {
                let ends = crate::scene::SPLINE_ENDS.lock();
                let mut bad: Vec<(f64, String)> = Vec::new();
                for (id, (a, b, prev, next, file)) in ends.iter() {
                    for (me, other) in [(*b, *next), (*a, *prev)] {
                        let Some((oa, ob, ..)) = ends.get(&other) else { continue };
                        // the other's end that lies at this one (a chain may run either way)
                        let there = if (oa.truncate() - me.truncate()).length() <= (ob.truncate() - me.truncate()).length() { *oa } else { *ob };
                        let (d2, dz) = ((there.truncate() - me.truncate()).length(), (there.z - me.z).abs());
                        if d2 < 1.0 && dz > 0.1 && id < &other {
                            bad.push((dz, format!("spline {id} ({file}) and {other}: {dz:.2} m apart in height at ({:.0}, {:.0}, {:.2})", me.x, me.y, me.z)));
                        }
                    }
                }
                bad.sort_by(|x, y| y.0.total_cmp(&x.0));
                log::info!("spline check: {} of {} chained ends differ in height by over 10 cm", bad.len(), ends.len());
                for (_, l) in bad.iter().take(15) {
                    log::info!("  {l}");
                }
            }
            log::info!(
                "  {} stretches longer than 15 m (a hole rather than a raster edge)",
                runs.len()
            );
            for (p, len) in runs.iter().take(8) {
                log::info!(
                    "  {len:.0} m without a road from ({:.0}, {:.0}, {:.1})",
                    p.x,
                    p.y,
                    p.z
                );
            }
            for (p, d) in worst {
                log::info!(
                    "  bare point at ({:.0}, {:.0}, {:.1}), nearest surface {d:+.2} m",
                    p.x,
                    p.y,
                    p.z
                );
            }
        }
    }
    // OMSI_PROBE_GRID=x,y,half,step: the wheels' ground on a square grid around (x, y), as
    // rows of centimetres relative to the middle ('.' where it is the same, '#' where the
    // ground there is more than 5 cm lower: a gap in the road the wheels fall through)
    if let Ok(spec) = omsi_cfg::env::var("OMSI_PROBE_GRID") {
        let v: Vec<f64> = spec.split(',').filter_map(|t| t.trim().parse().ok()).collect();
        if v.len() >= 4 {
            let (cx, cy, half, step) = (v[0], v[1], v[2], v[3].max(0.001));
            let mid = scene::drive_probe(&world.terrains, &world.surfaces, cx, cy, 1e6).below.unwrap_or(0.0);
            let n = (half / step).round() as i64;
            for j in (-n..=n).rev() {
                let row: String = (-n..=n)
                    .map(|i| {
                        let (x, y) = (cx + i as f64 * step, cy + j as f64 * step);
                        match scene::drive_probe(&world.terrains, &world.surfaces, x, y, mid + 1.0).below {
                            Some(z) if z < mid - 0.05 => '#',
                            Some(z) if (z - mid).abs() <= 0.02 => '.',
                            Some(_) => '+',
                            None => ' ',
                        }
                    })
                    .collect();
                log::info!("grid {:.3}: {row}", cy + j as f64 * step);
            }
        }
    }
    // OMSI_PROBE=x0,y0,x1,y1[,n]: print the terrain height and the road surface height
    // along a line, to see where the ground comes through a road
    if let Ok(spec) = omsi_cfg::env::var("OMSI_PROBE") {
        let v: Vec<f64> = spec
            .split(',')
            .filter_map(|t| t.trim().parse().ok())
            .collect();
        if v.len() >= 4 {
            let n = v.get(4).copied().unwrap_or(20.0).max(2.0) as usize;
            for k in 0..n {
                let t = k as f64 / (n - 1) as f64;
                let (x, y) = (v[0] + (v[2] - v[0]) * t, v[1] + (v[3] - v[1]) * t);
                let tx = (x / omsi_map::tile_size()).floor() as i32;
                let ty = (y / omsi_map::tile_size()).floor() as i32;
                let lx = (x - tx as f64 * omsi_map::tile_size()) as f32;
                let ly = (y - ty as f64 * omsi_map::tile_size()) as f32;
                let terrain = world
                    .terrains
                    .read()
                    .get(&(tx, ty))
                    .map(|t| t.sample(lx, ly));
                let surface = world
                    .surfaces
                    .read()
                    .get(&(tx, ty))
                    .and_then(|s| s.sample(lx, ly));
                let top = terrain.map(|t| t as f64 + 30.0).unwrap_or(1e6);
                let wheel = scene::drive_probe(&world.terrains, &world.surfaces, x, y, top);
                log::info!("probe ({x:.1}, {y:.1}) tile ({tx}, {ty}) local ({lx:.1}, {ly:.1}): terrain {terrain:?} surface {surface:?} wheels {:?}", wheel.below);
            }
        }
    }
    if career.path.is_some() {
        if let Err(e) = career.save() {
            log::warn!("writing the personnel file: {e}");
        }
    } else if career.metres > 1.0 {
        log::info!("this run: {}", career.summary());
    }
    let clock = {
        let mut c = start_clock(args);
        c.time += args.drive.unwrap_or(0.0) as f64 + service_seconds;
        c
    };
    if let Some(out) = &args.save_situation {
        let sit = build_situation(
            args,
            &world,
            &clock,
            args.weather.as_deref(),
            player_ref.as_ref(),
            &[],
            &camera,
            duty.as_ref(),
            "openOMSI save",
        );
        match sit.save(out) {
            Ok(()) => log::info!(
                "saved situation {} ({} vehicles)",
                out.display(),
                sit.vehicles.len()
            ),
            Err(e) => log::warn!("saving {}: {e}", out.display()),
        }
    }
    let daylight = omsi_sim::Daylight::compute(&clock, envir.as_ref());
    world.set_lamps(&renderer, &mut scene, daylight.lamps_on);
    world.update_night_modes(&renderer, &mut scene, &clock, daylight.brightness);
    // a run starts with the roads already in the state this weather would leave them
    let mut wetness = initial_wetness(&weather);
    if let Some(v) = omsi_cfg::env::var("OMSI_WETNESS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        wetness = v;
    }
    let mut lighting = weather_lighting(&daylight, &weather, cloud_drift_at(&weather, clock.time), wetness, settings.shadows);
    // the player's vehicle has moved into `player_ref` by now (after --drive): without
    // this the offscreen picture had no cab box, unlike the window
    lighting.inside = player_ref.as_ref().or(player.as_ref()).and_then(|p| {
        p.vehicle
            .ty
            .def
            .bounding_box
            .map(|bb| (p.vehicle.position, p.vehicle.heading, bb))
    });
    let puddle_surface = lighting.inside.and_then(|(o, _, _)| world.puddle_surface(o));
    lighting.puddle_ground = puddle_surface.map(|(h, _)| h);
    lighting.puddle_normal = puddle_surface.map_or(glam::Vec3::Z, |(_, n)| n);
    lighting.puddle_parts = player_ref.as_ref().or(player.as_ref()).into_iter().flat_map(|p| &p.vehicle.trailers)
        .filter_map(|t| t.ty.def.bounding_box.map(|bb| (t.position, t.heading, bb))).take(3).collect();
    lighting.detail = settings.detail_textures;
    lighting.glass_wind = player_ref.as_ref().or(player.as_ref()).map(|p| crate::lights::vehicle_velocity(&p.vehicle)).unwrap_or_default();
    // OMSI_GLASS_WIND=<m/s>: the rain on the glass as the bus would meet it at that speed
    if let (Some(v), Some(p)) = (omsi_cfg::env::var("OMSI_GLASS_WIND").ok().and_then(|v| v.parse::<f32>().ok()), player_ref.as_ref().or(player.as_ref())) {
        let h = p.vehicle.heading.to_radians();
        lighting.glass_wind = glam::Vec3::new(h.sin() as f32, h.cos() as f32, 0.0) * v;
    }
    {
        // scenery scripts: a few frames so animations settle
        let phase = |c: usize, li: usize| {
            traffic
                .as_ref()
                .map(|t| t.light_vars(c, li))
                .unwrap_or((omsi_sim::traffic::UNLINKED_PHASE as f32, 0.0))
        };
        let dt = 1.0 / 30.0;
        let mut n = 0;
        for _ in 0..(args.drive.unwrap_or(1.0) / dt).max(3.0) as usize {
            // the time of day and the departure displays' boards (the first pass says which
            // stops have displays)
            match schedule.as_mut() {
                Some(s) => s.update_boards(
                    &world,
                    traffic.as_ref(),
                    duty.as_ref(),
                    player_ref
                        .as_ref()
                        .and_then(|p| p.vehicle.host.hof.as_deref()),
                    &clock,
                ),
                None => world.timetable_boards.lock().clock = Some(clock.clone()),
            }
            n = world.update_scripted(
                &renderer,
                &mut scene,
                dt,
                camera.position,
                daylight.brightness,
                &phase,
                None,
                false,
            );
        }
        log::info!(
            "scenery scripts: {} objects updated of {}",
            n,
            world.scripted.lock().len()
        );
    }
    {
        let mut vehicles: Vec<&omsi_sim::VehicleInstance> = Vec::new();
        if let Some(p) = player_ref.as_ref() {
            vehicles.push(&p.vehicle);
        }
        if let Some(t) = traffic.as_ref() {
            vehicles.extend(t.cars.iter().map(|c| &c.vehicle));
        }
        vehicles.extend(remotes_off.remotes.values().map(|r| r.vehicle()));
        lights::set_cone_strength(weather.fog.0, precip_of(&weather).1, daylight.night);
        lights::upload_corona_textures(&mut renderer);
        world.update_light_map_atlas(&renderer, camera.position);
        lights::collect(&world, &mut scene, &daylight, camera.position, &vehicles);
        let (kind, rate) = precip_of(&weather);
        let mut rn = rain::Rain::new();
        rn.set(kind, rate);
        for _ in 0..30 {
            scene
                .coronas
                .retain(|c| c.cone_cos > -1.5 && !(c.size < 0.07 && c.brightness < 0.95));
            rn.tick(
                1.0 / 30.0,
                camera.position,
                Vec3::ZERO,
                &mut scene,
                &player_ref.as_ref().or(player.as_ref()).map(|p| rain::vehicle_boxes(&p.vehicle)).unwrap_or_default(),
            );
        }
        // a moving player's wheels through the puddles the enhanced renderer paints on wet
        // roads (OMSI_DRIVE_V0=S moves it without needing a full engine-start sequence)
        if kind == 1 {
            if let Some(p) = player_ref.as_ref() {
                let wheels = puddles::wheel_contacts(&p.vehicle);
                let speed = p.vehicle.physics.velocity_kmh().abs() / 3.6;
                let mut sp = puddles::Splashes::new();
                for _ in 0..30 {
                    scene
                        .smoke
                        .extend(sp.update(1.0 / 30.0, &wheels, speed, &|x, y| {
                            puddles::puddle_coverage(x, y, world.wet_road_at(x, y, wetness))
                        }));
                }
                log::info!(
                    "splashes: {} wheels, {:.1} km/h, {} in a puddle, {} coronas",
                    wheels.len(),
                    speed * 3.6,
                    sp.wheels_in_puddle,
                    scene.coronas.len()
                );
            }
        }
        log::info!(
            "daylight: sun altitude {:.1}°, night {:.2}, lamps {}, {} lights, {} coronas",
            daylight.altitude_deg,
            daylight.night,
            daylight.lamps_on,
            scene.lights.len(),
            scene.coronas.len()
        );
        log::info!(
            "  light A (sun) {:?} B (sky) {:?} C (ambient) {:?} sky {:?} fog {:?} density {:.5}",
            daylight.sun_color,
            daylight.secondary,
            daylight.ambient,
            daylight.sky,
            lighting.fog_color,
            lighting.fog_density
        );
        {
            let mut near: Vec<&omsi_render::PointLight> = scene.lights.iter().collect();
            near.sort_by(|a, b| (a.position - camera.position).length().total_cmp(&(b.position - camera.position).length()));
            for l in near.iter().take(4) {
                log::info!(
                    "  light {:.0} m away: colour {:?} radius {:.1} intensity {:.2}",
                    (l.position - camera.position).length(),
                    l.color,
                    l.radius,
                    l.intensity
                );
            }
        }
    }
    if let Some(p) = player_ref.as_ref() {
        let mut hud = hud::Hud::new(&mut world.fonts.lock());
        let t = clock.time;
        let mut lines = vec![
            format!(
                "{:02}:{:02}:{:02}",
                (t / 3600.0) as i32,
                ((t % 3600.0) / 60.0) as i32,
                (t % 60.0) as i32
            ),
            format!(
                "{:.0} km/h   {}",
                p.vehicle.physics.velocity_kmh().abs(),
                p.vehicle.ty.def.type_name
            ),
        ];
        if !p.vehicle.host.tt_line.is_empty() {
            let next = p
                .vehicle
                .host
                .tt_stops
                .get(p.vehicle.host.tt_busstop_index.max(0) as usize)
                .map(|s| s.0.clone())
                .unwrap_or_default();
            lines.push(format!(
                "Line {}   next: {}   {:+.0} s",
                p.vehicle.host.tt_line, next, p.vehicle.host.tt_delay
            ));
        }
        if let Some(e) = duty_error.as_ref() {
            lines.push(format!("No duty: {e}"));
        }
        if let Some(l) = lan_off.as_ref() {
            lines.extend(lan::hud_lines(l, &remotes_off, Some(p)));
        }
        let viewport = settings.hud_viewport((w, h));
        let overlay_start = scene.overlays.len();
        hud.update(&renderer, &mut scene, &lines);
        crate::ui::shift_overlays(&mut scene, overlay_start, viewport[0]);
        // the navigator, as the window shows it (its camera settled first)
        if settings.navigator {
            let mut nav = navigator::Navigator::new(true, settings.ui_opacity, &settings.navigator_corner);
            nav.schedule = omsi_cfg::env::var_os("OMSI_NAV_SCHEDULE").is_some();
            nav.show_ai = settings.nav_ai;
            if omsi_cfg::env::var_os("OMSI_NAV_MAP").is_some() {
                nav.toggle_map();
            }
            if traffic.is_none() {
                nav.add_lanes(world.lanes.lock().clone());
            }
            nav.set_map(world.navigation_map());
            let (line, terminus, stops, trip) = navigator::duty_parts(duty.as_ref());
            if let (Some((key, name)), Some(sch)) = (trip, schedule.as_ref()) {
                let lanes = sch.trip_route_in(nav.map_net().unwrap(), &name);
                let g = nav.global_version + (1 << 40);
                nav.set_route(&key, lanes, true, g);
            }
            let (outside_temp, inside_temp) = crate::app_events::vehicle_temperatures(p);
            let frame = navigator::NavFrame {
                traffic: traffic.as_ref(),
                bus: p.vehicle.position,
                heading: p.vehicle.heading,
                speed_kmh: p.vehicle.physics.velocity_kmh(),
                outside_temp,
                inside_temp,
                line,
                terminus,
                stops,
                delay: duty.as_ref().map(|_| p.vehicle.host.tt_delay as f64),
                passengers: humans_off.as_ref().map(|h| h.riding()),
                stop_requested: navigator::stop_requested(&p.vehicle),
                time: clock.time,
                weekday: clock.weekday(),
                language: &settings.language,
                screen: (viewport[2], viewport[3]),
                ui_scale: settings.ui_scale,
                follow_window: settings.ui_scale_window,
                dt: 0.1,
            };
            for _ in 0..30 {
                nav.frame_at(&renderer, &mut scene, &frame, viewport[0]);
                scene.overlays.pop();
            }
            nav.frame_at(&renderer, &mut scene, &frame, viewport[0]);
        }
    }
    // OMSI_ROAD_PHOTO: photograph the road network from above, point by point, and say
    // where the picture shows grass although the map says there is a carriageway. This is
    // the only check that asks what is actually drawn rather than what the data says.
    if omsi_cfg::env::var_os("OMSI_ROAD_PHOTO").is_some() {
        if let Some(t) = traffic.as_ref() {
            let mut points: Vec<DVec3> = Vec::new();
            let mut headings: Vec<f64> = Vec::new();
            // OMSI_ROAD_PHOTO_SLANT=<m>: from a driver's eye that far back along the lane
            // (2.6 m up) instead of from above - terrain a few millimetres over the road
            // shows only at a slant
            let slant: Option<f64> = omsi_cfg::env::var("OMSI_ROAD_PHOTO_SLANT").ok().and_then(|v| v.parse().ok());
            for l in t
                .net
                .lanes
                .iter()
                .filter(|l| l.kind == omsi_sim::traffic::LaneKind::Street && !l.invisible)
            {
                let len = l.length();
                let mut s = 3.0f32;
                // the carriageway itself, and the verge a few metres to either side, which
                // in a city street is paved too
                let side: f64 = omsi_cfg::env::var("OMSI_ROAD_PHOTO_SIDE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                while s < len {
                    let (p, h) = l.at(s);
                    let a = (h as f64).to_radians();
                    points.push(DVec3::new(p.x + side * a.cos(), p.y - side * a.sin(), p.z));
                    headings.push(h as f64);
                    s += 25.0;
                }
            }
            // a spread sample so one long street cannot dominate
            let cap: usize = omsi_cfg::env::var("OMSI_ROAD_PHOTO_N").ok().and_then(|v| v.parse().ok()).unwrap_or(400);
            let step = (points.len() / cap.max(1)).max(1);
            let sample: Vec<(DVec3, f64)> = points.iter().copied().zip(headings.iter().copied()).step_by(step).collect();
            let (mut green, mut checked) = (0usize, 0usize);
            let mut spots: Vec<(DVec3, [u8; 3])> = Vec::new();
            let mut holes: Vec<(usize, DVec3, Camera)> = Vec::new();
            for (p, h) in &sample {
                let cam = match slant {
                    Some(back) => {
                        let a = h.to_radians();
                        let eye = DVec3::new(p.x - back * a.sin(), p.y - back * a.cos(), p.z + 2.6);
                        Camera {
                            position: eye,
                            yaw: *h as f32,
                            pitch: -(2.6f64 / back).atan().to_degrees() as f32,
                            roll: 0.0,
                            fov_deg: 20.0,
                            near: 0.3,
                            far: 400.0,
                        }
                    }
                    // (only the half metre over the lane: a crown or a sign hanging over the
                    // carriageway is no hole in it)
                    None => Camera {
                        position: DVec3::new(p.x, p.y, p.z + 40.0),
                        yaw: 0.0,
                        pitch: -90.0,
                        roll: 0.0,
                        fov_deg: 40.0,
                        near: 39.5,
                        far: 41.5,
                    },
                };
                let (pw, ph) = (48u32, 48u32);
                // OMSI_HOLE_PHOTO: the same place from above down to 25 m under the lane -
                // what shows the sky there is a hole through the world
                if omsi_cfg::env::var_os("OMSI_HOLE_PHOTO").is_some() {
                    // (at a slant: the lower half of the picture only, which is all under the
                    // horizon)
                    let deep = if slant.is_some() { Camera { near: 0.3, far: 400.0, ..cam } } else { Camera { near: 20.0, far: 65.0, ..cam } };
                    if let Ok(px) = renderer.render_to_image(&mut scene, 96, 96, &deep, &lighting) {
                        let from = if slant.is_some() { 48 * 96 * 4 } else { 0 };
                        let sky = px[from..].chunks_exact(4).filter(|c| c[2] as i32 > c[0] as i32 + 30 && c[2] as i32 > c[1] as i32 + 8 && c[2] > 150).count();
                        if sky > 3 {
                            holes.push((sky, *p, deep));
                        }
                    }
                }
                let Ok(px) = renderer.render_to_image(&mut scene, pw, ph, &cam, &lighting) else {
                    continue;
                };
                // the centre pixel looks straight down at the lane
                let i = ((ph / 2) * pw + pw / 2) as usize * 4;
                let (r, g, b) = (px[i], px[i + 1], px[i + 2]);
                checked += 1;
                // grass and fields are green; asphalt, concrete and cobbles are grey
                let is_green = g as i32 > r as i32 + 12 && g as i32 > b as i32 + 12;
                if is_green {
                    green += 1;
                    if spots.len() < 12 {
                        spots.push((*p, [r, g, b]));
                    }
                }
            }
            if omsi_cfg::env::var_os("OMSI_HOLE_PHOTO").is_some() {
                holes.sort_by(|a, b| b.0.cmp(&a.0));
                log::info!("hole photo: {} of {checked} places show the sky through the ground", holes.len());
                for (n, p, c) in holes.iter().take(40) {
                    log::info!("   {n} sky pixels at ({:.1}, {:.1}, {:.2})  (--cam {:.1},{:.1},{:.1},{:.0},{:.1},{:.0})", p.x, p.y, p.z, c.position.x, c.position.y, c.position.z, c.yaw, c.pitch, c.fov_deg);
                }
            }
            log::info!("road photo: {green} of {checked} places along the carriageways show ground instead of road ({:.1} %)", green as f32 / checked.max(1) as f32 * 100.0);
            for (p, c) in &spots {
                log::info!(
                    "   green at ({:.1}, {:.1}, {:.2}) rgb {:?}  (--cam {:.0},{:.0},{:.0},0,-89)",
                    p.x,
                    p.y,
                    p.z,
                    c,
                    p.x,
                    p.y,
                    p.z + 40.0
                );
            }
        }
    }
    // OMSI_BENCH=n: the final picture drawn n more times as a window frame would be (one
    // mirror, then the view), with the median CPU time of the drawing calls and of the wait
    // for the GPU - medians shrug off what else the machine is doing
    if let Some(n) = omsi_cfg::env::var("OMSI_BENCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        let target = renderer.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bench"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: renderer.format(),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let (mut cpu, mut gpu) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for k in 0..n {
            let t = Instant::now();
            if let Some(p) = player_ref.as_ref() {
                render_mirrors(&mut renderer, &mut scene, &world, p, &lighting, Some(k), None);
            }
            renderer.render(&mut scene, &view, w, h, &camera, &lighting);
            let drawn = t.elapsed().as_secs_f64();
            let t = Instant::now();
            let _ = omsi_render::wait_gpu(&renderer.device, None);
            cpu.push(drawn * 1000.0);
            gpu.push(t.elapsed().as_secs_f64() * 1000.0);
            if omsi_cfg::env::var_os("OMSI_BENCH_FRAMES").is_some() {
                log::info!("bench frame {k}: drawing {:.2} ms, GPU wait {:.2} ms", drawn * 1000.0, gpu[k]);
            }
        }
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.total_cmp(b));
            v.get(v.len() / 2).copied().unwrap_or(0.0)
        };
        let total: Vec<f64> = cpu.iter().zip(&gpu).map(|(a, b)| a + b).collect();
        let worst = total.iter().copied().fold(0.0f64, f64::max);
        let mean = total.iter().sum::<f64>() / total.len().max(1) as f64;
        log::info!(
            "bench: {n} frames of {w}x{h}: drawing {:.2} ms, GPU wait {:.2} ms (medians), frame mean {mean:.2} ms, worst {worst:.2} ms",
            median(&mut cpu),
            median(&mut gpu)
        );
        for (k, v) in renderer.stats.borrow().iter() {
            log::info!("bench stage {k:18}: {:.2} ms/frame", v / n as f64 * 1000.0);
        }
        for (k, v) in renderer.counts.borrow().iter() {
            log::info!("bench count {k}: {:.0} a frame", v / n as f64);
        }
        // (each pass from the end of the one before it; the mirrors' frames on their own)
        for (pass, ms, frames) in renderer.gpu_pass_times() {
            log::info!("bench gpu pass {pass:12}: {ms:.2} ms ({frames} frames measured)");
        }
    }
    // the textures compressed on the workers are in the picture, as in a window after its
    // first seconds; OMSI_TEXTURE_MEMORY=<MB> applies a texture budget first
    // (OMSI_BUDGET_FROM=x,y[,MB]: the budget is met from there first, as if the camera had
    // been there, then - with the budget raised to MB - the textures that come near again
    // with the camera are read back)
    if omsi_cfg::env::var_os("OMSI_TEXTURE_MEMORY").is_some() {
        world.set_texture_budget(texture_budget(&settings));
        let from: Vec<f64> = omsi_cfg::env::var("OMSI_BUDGET_FROM")
            .unwrap_or_default()
            .split(',')
            .filter_map(|v| v.trim().parse::<f64>().ok())
            .collect();
        if from.len() >= 2 {
            let at = [DVec3::new(from[0], from[1], 0.0)];
            while world.update_texture_budget(&renderer, &mut scene, &at, true) > 0 {}
            if let Some(mb) = from.get(2) {
                world.set_texture_budget((*mb * 1e6) as u64);
            }
        }
        let centers = [camera.position];
        while world.update_texture_budget(&renderer, &mut scene, &centers, true) > 0 {
            world.finish_texture_upgrades(&renderer, &mut scene);
        }
    }
    world.finish_texture_upgrades(&renderer, &mut scene);
    // OMSI_WARM_FRAMES=n: n frames drawn before the picture, for what reads the frame
    // before it (the rain on the glass looks through the last picture)
    for _ in 0..omsi_cfg::env::var("OMSI_WARM_FRAMES").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) {
        let _ = renderer.render_to_image(&mut scene, w, h, &camera, &lighting)?;
    }
    let t0 = Instant::now();
    if let Some(p) = player_ref.as_ref() {
        render_mirrors(&mut renderer, &mut scene, &world, p, &lighting, None, None);
    }
    if let Some(p) = player_ref.as_ref() {
        let mode = omsi_cfg::env::var("OMSI_MIRROR_HUD").ok().and_then(|v| v.parse::<u8>().ok()).unwrap_or(settings.mirror_hud);
        let mut panels = crate::mirror_hud::MirrorHud::default();
        panels.set_aspects(world.mirror_aspect.lock().clone());
        panels.sync(p, mode);
        if mode != 0 {
            panels.enabled = true;
            if panels.panels.is_empty() {
                panels.toggle_edit(p);
                panels.toggle_edit(p);
            }
        }
        let viewport = settings.hud_viewport((w, h));
        let start = scene.overlays.len();
        panels.push(&mut scene, &world, viewport[2], viewport[3], (0.0, 0.0));
        crate::ui::shift_overlays(&mut scene, start, viewport[0]);
    }
    let pixels = if settings.triple.enabled && !settings.vr_requested() {
        renderer.render_triple_to_image(&mut scene, w, h, &camera, &lighting, &settings.triple)?
    } else {
        renderer.render_to_image(&mut scene, w, h, &camera, &lighting)?
    };
    log::info!(
        "rendered {} instances in {:.1} ms; GPU memory: textures {:.0} MB, meshes {:.0} MB ({} meshes, {} textures)",
        scene.instances.len(),
        t0.elapsed().as_secs_f32() * 1000.0,
        renderer.texture_bytes(&scene) as f64 / 1e6,
        renderer.mesh_bytes(&scene) as f64 / 1e6,
        scene.meshes.len(),
        scene.textures.len()
    );
    image::save_buffer(out, &pixels, w, h, image::ColorType::Rgba8)?;
    println!("wrote {}", out.display());
    Ok(())
}

/// The tyres as they are drawn: the lowest point of each wheel mesh and how far it is over
/// the road under it (negative: in the asphalt).
fn tyre_lows(v: &omsi_sim::VehicleInstance, world: &World) -> Vec<(DVec3, f64)> {
    let mut out = Vec::new();
    for (i, vm) in v.ty.meshes.iter().enumerate() {
        let def = &v.ty.model.meshes[vm.def_index];
        if !v.mesh_props[i].visible || !def.animations.iter().any(|an| an.variable.to_ascii_lowercase().starts_with("wheel_rotation_")) {
            continue;
        }
        let xf = v.mesh_local_transform(i);
        let Some(q) = vm.data.positions.iter().map(|q| xf.transform_point3(*q)).min_by(|a, b| a.z.total_cmp(&b.z)) else { continue };
        let w = v.position + q.as_dvec3();
        if let Some(g) = world.ground_height(w.x, w.y) {
            out.push((w, w.z - g));
        }
    }
    out
}

/// The worst pitch and bank of the drive, the origin's highest and lowest over the ground,
/// and where and when the worst pitch was.
static DRIVE_EXTREMES: parking_lot::Mutex<(f32, f32, f64, f64, (DVec3, f32))> = parking_lot::Mutex::new((0.0, 0.0, f64::MIN, f64::MAX, (DVec3::ZERO, 0.0)));

/// `OMSI_CAM_VEHICLE=x,y,z,yaw,pitch[,fov]`: a camera in the bus's own frame (x right,
/// y forward, z up; yaw relative to the bus) for close-ups of displays and switches - the
/// final picture and every `--snapshots` one.
/// With a triple screen, the frustum around its three panels (see `App::sight_extent`).
fn triple_extent(settings: &crate::settings::Settings, cam: &Camera, w: u32, h: u32) -> Option<(f64, f64)> {
    (settings.triple.enabled && !settings.vr_requested()).then(|| {
        let (x, y) = settings.triple.view_extent(cam, w, h);
        (x as f64, y as f64)
    })
}

fn vehicle_camera(player: &Player, camera: &mut Camera) {
    let Ok(spec) = omsi_cfg::env::var("OMSI_CAM_VEHICLE") else { return };
    let v: Vec<f32> = spec
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if v.len() >= 5 {
        camera.position = player.vehicle.position
            + player
            .vehicle
            .body_rotation()
            .transform_point3(Vec3::new(v[0], v[1], v[2]))
            .as_dvec3();
        camera.yaw = player.vehicle.heading as f32 + v[3];
        camera.pitch = v[4];
        camera.near = 0.02;
        if let Some(f) = v.get(5) {
            camera.fov_deg = *f;
        }
    }
}
