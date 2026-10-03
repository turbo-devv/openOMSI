//! `omsi` - load an OMSI 2 map and render it.
//!
//! ```text
//! omsi --root "/path/to/OMSI 2" --map maps/Grundorf/global.cfg            # window
//! omsi --root ... --map ... --offscreen out.png --size 1600x900            # PNG
//! ```
//!
//! The game is this library; `main.rs` calls [`run`], and on Android the NativeActivity
//! calls `android_main` (see `android.rs`), which runs the launcher and the game in one
//! window of one process.

mod admin;
mod discord;
#[cfg(steam)]
mod steam;
mod voice;
mod head_idle;
mod headtrack;
#[cfg(windows)]
mod openxr;
#[cfg(target_os = "macos")]
mod mac_hid;
#[cfg(target_os = "android")]
mod android;
mod platform;
mod touch;
mod placing;
mod mt;
mod updater;
mod ambience;
mod camera_arm;
mod career;
mod describe;
mod editor;
mod game_lists;
mod rail_drive;
mod driver;
mod export;
mod hud;
mod humans;
mod keys;
mod lan;
mod lan_world;
mod lights;
mod launcher;
mod menu;
mod mirror_hud;
mod navigator;
mod vr_navigator;
mod money;
mod radio;

mod puddles;
mod quit;
mod rain;
mod scene;
mod schedule;
mod schedule_paper;
mod real_time;
mod settings;
mod threads;
mod tiles;
mod traffic;
mod ui;

// the game itself, split by what each part does
mod app;
mod applog;
mod app_events;
mod bus_service;
mod camera_util;
mod controllers;
mod ffb_calibration;
#[cfg(windows)]
mod dinput;
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod evdev_ff;
mod cli;
mod diagnostics;
mod duty_start;
mod input_script;
mod launcher_link;
mod lan_mods;
mod memory;
mod offscreen;
mod ground_gap;
mod on_foot;
mod route_arrows;
mod server;
mod player;
mod plugins;
mod services;
mod situation;
mod spawn;
mod stock_keys;
mod startup;
mod traffic_link;
mod tutorial;
mod weather_setup;
mod weather_cycle;
mod world_load;

// the interface's translations (locales/app.yml; the English text is the key)
rust_i18n::i18n!("locales");
// (the tables are read when this crate compiles: this makes cargo compile it again when they
// change - the macro alone left the old texts in the program)
const _LOCALES: &str = include_str!("../locales/app.yml");

/// Show the interface in `code` (the settings' ENG / DEU / FRA / RUS).
pub(crate) fn ui_language(code: &str) {
    omsi_ui::i18n::set_lookup(|lang, text| _rust_i18n_try_translate(lang, text).map(|t| t.into_owned()));
    let iso = omsi_launcher_lib::language_iso(code);
    omsi_ui::i18n::set_language(iso);
    omsi_sim::vehicle_api::set_locale(iso);
}

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use glam::{DVec3, Vec3};
use omsi_render::{Camera, Renderer, Scene, SurfaceState};
use scene::World;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};
use app::*;
use camera_util::*;
use cli::*;
use diagnostics::*;
use duty_start::*;
use input_script::*;
use launcher_link::*;
use memory::*;
use offscreen::*;
use player::*;
use services::*;
use situation::*;
use spawn::*;
use startup::*;
use traffic_link::*;
use weather_setup::*;
use world_load::*;

/// The game (and its launcher) from the command line: what `main` does.
pub fn run() -> Result<()> {
    omsi_cfg::migrate_legacy_data_dir();
    #[cfg(target_os = "macos")]
    restart_with_allocator_settings();
    #[cfg(windows)]
    attach_parent_console();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // a panic goes into the log (which the launcher keeps per session) with where it
    // happened and a backtrace, not only to a terminal that may not be there
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if omsi_render::catching() {
            log::warn!("caught by the renderer: {info}");
            return;
        }
        log::error!(
            "the game stopped on an error (build {BUILD}): {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        default_hook(info);
    }));
    log::info!(
        "openOMSI {VERSION}, build {BUILD}{}",
        if std::env::var_os("MallocLargeCache").is_some() {
            " (large allocations returned at once)"
        } else {
            ""
        }
    );
    let args = Args::parse();
    // Started by a double click or with no arguments at all: that is the launcher's job.
    // The launcher itself runs the game with a full command line (--no-menu, --map, ...).
    let bare = std::env::args().len() == 1;
    let Some((args, server_cfg)) = prepare(args, bare)? else { return Ok(()) };
    if args.launcher || (bare && !args.menu) {
        // the launcher window (the game started again by it with a full command line);
        // OMSI_LAUNCHER=<program> still opens another launcher instead
        if omsi_cfg::env::var_os("OMSI_LAUNCHER").is_some() && open_launcher()? {
            return Ok(());
        }
        launcher_statics();
        return launcher::run(graphics_instance());
    }
    let Some(app) = make_app(args, server_cfg)? else { return Ok(()) };
    let event_loop = EventLoop::new()?;
    // SIGTERM (the launcher's Stop) and Ctrl+C end the session the way Escape does
    let proxy = event_loop.create_proxy();
    quit::install(move |_| {
        let _ = proxy.send_event(());
    });
    let mut app = app;
    let r = event_loop.run_app(&mut app);
    // the host's mods of this session go with it
    lan_mods::clean_up();
    r?;
    Ok(())
}

/// The showroom is drawn the way the game will be.
pub(crate) fn launcher_statics() {
    let s = settings::Settings::load();
    ENHANCED.store(s.enhanced || omsi_cfg::env::var_os("OMSI_ENHANCED").is_some(), std::sync::atomic::Ordering::Relaxed);
    CLASSIC.store(s.classic(), std::sync::atomic::Ordering::Relaxed);
    CLOUDS.store(s.clouds && omsi_cfg::env::var_os("OMSI_NO_CLOUDS").is_none(), std::sync::atomic::Ordering::Relaxed);
}

/// Everything before a window: the language, the session's random seed, the original
/// installation and the content roots (mods, archives). None when the program has
/// nothing more to do (a fatal error was shown).
pub(crate) fn prepare(mut args: Args, bare: bool) -> Result<Option<(Args, Option<server::ServerCfg>)>> {
    ui_language(&settings::Settings::load().language);
    // (a server has no interface to translate)
    mt::enable(settings::Settings::load().machine_translation && args.server.is_none());
    // the dedicated server: server.cfg decides the world, the rest is a host without a window
    let server_cfg = match args.server.clone() {
        Some(p) => match server::prepare(&mut args, &p) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("server: {e:#}");
                std::process::exit(2);
            }
        },
        None => None,
    };
    // the scripts' `random` differs from session to session (starting air pressure, part
    // lifetimes ...); OMSI_SEED=n repeats a session's numbers
    let seed = omsi_cfg::env::var("OMSI_SEED")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1);
            (t ^ ((std::process::id() as u64) << 32)) % 1_000_000_000
        });
    omsi_script::set_session_seed(seed);
    log::info!("script random seed {seed} (OMSI_SEED={seed} repeats it)");
    let launcher_mode = args.launcher || (bare && !args.menu);
    if !is_omsi_root(&args.root) {
        match find_root() {
            Some(p) => {
                log::info!("OMSI 2 found at {}", p.display());
                args.root = p;
            }
            None if launcher_mode => {
                // the launcher opens without the game: its Setup page is where the folder is
                // chosen (only a session needs the original installation)
                log::warn!("the original OMSI 2 was not found; the launcher asks for it");
            }
            None => {
                // openOMSI plays on the original game's content: without a complete
                // installation of it there is nothing to play on, and every player must have
                // the same base whatever copy of OMSI 2 they own
                let missing = omsi_cfg::missing_original_essentials(&args.root);
                let text = format!(
                    "The original OMSI 2 was not found.\n\n\
                     openOMSI needs a complete installation of the original game (any version). \
                     Choose its folder in the launcher (Setup), or start once with \
                     --root \"/path/to/OMSI 2\", the folder with Omsi.exe, maps and Vehicles in it.\n\n\
                     Missing in {}: {}",
                    args.root.display(),
                    missing.join(", ")
                );
                if server_cfg.is_some() {
                    eprintln!("{text}");
                } else {
                    fatal_dialog("openOMSI cannot start", &text);
                }
                if cfg!(target_os = "android") {
                    // (a phone's app is not ended from inside: back to the launcher)
                    return Ok(None);
                }
                std::process::exit(1);
            }
        }
    }
    if let Some(memo) = root_memo().filter(|_| is_omsi_root(&args.root)) {
        let _ = std::fs::write(memo, args.root.to_string_lossy().as_bytes());
    }
    // content roots: the game's own folder (mods) first, then the original installation
    if let Some(c) = content_dir() {
        match omsi_cfg::ensure_content_layout(&c) {
            Ok(()) => {
                omsi_cfg::add_content_root(c.clone());
                log::info!("content folder (mods): {}", c.display());
            }
            Err(e) => log::warn!("content folder {}: {e}", c.display()),
        }
    }
    // archives read in place: searched after the content folder, before the installation
    // (`--content-zip`, `OMSI_CONTENT_ZIP`, and every .zip in the content folder's `Archives`)
    for z in &args.content_zip {
        if let Err(e) = omsi_cfg::add_content_zip(z) {
            log::warn!("content zip {}: {e}", z.display());
        }
    }
    omsi_cfg::vfs::mount_env_zips();
    if let Some(c) = content_dir() {
        omsi_cfg::vfs::mount_dir_zips(&c.join("Archives"));
    }
    omsi_cfg::add_content_root(args.root.clone());
    Ok(Some((args, server_cfg)))
}

/// The game for `args`, ready for its window; None when there is no window to open (a
/// picture or a model was written instead).
pub(crate) fn make_app(mut args: Args, server_cfg: Option<server::ServerCfg>) -> Result<Option<App>> {
    let _lan_status = lan::StatusFileGuard;
    // OMSI's tutorials: the lesson's own situation
    if let Some(n) = args.tutorial.filter(|n| (1..=4).contains(n)) {
        args.situation = Some(tutorial::SITUATIONS[n - 1].to_string());
    }
    apply_situation(&mut args)?;
    // `openomsi`: the official server, wherever its tunnel is today (see omsi_net::official)
    if let Some(t) = args.lan_join.clone().filter(|t| omsi_net::official::is_alias(t)) {
        match omsi_net::official::resolve_target(&t) {
            Ok(url) => {
                log::info!("LAN: the official server is at {url}");
                args.lan_join = Some(url);
            }
            Err(e) => log::warn!("LAN: {e}"),
        }
    }
    // a duty starts at its trip, as in OMSI (not at the map's entry point); a joining
    // player's once the host's world is known (below): it was never placed at all, and
    // "Automatic" put it at the map's first entry point, the depot
    // real-time sync: the game starts at this device's date and time (a joining player's
    // clock is the host's, a server's is its server.cfg's); a duty does not move it
    if settings::Settings::load().time_sync && args.lan_join.is_none() && args.server.is_none() && args.offscreen.is_none() {
        real_time::start_at_now(&mut args);
    }
    if args.export_glb.is_none() && args.lan_join.is_none() {
        place_on_duty(&mut args);
    }
    let settings = settings::Settings::load();
    applog::log_system(&settings);
    if args.drive_keys.eq_ignore_ascii_case("simple")
        && !settings.drive_keys.eq_ignore_ascii_case("simple")
    {
        args.drive_keys = settings.drive_keys.clone();
    }
    ENHANCED.store(
        settings.enhanced || args.enhanced || omsi_cfg::env::var_os("OMSI_ENHANCED").is_some(),
        std::sync::atomic::Ordering::Relaxed,
    );
    CLOUDS.store(settings.clouds && omsi_cfg::env::var_os("OMSI_NO_CLOUDS").is_none(), std::sync::atomic::Ordering::Relaxed);
    SOUND_AI.store(settings.vol_ai.to_bits(), std::sync::atomic::Ordering::Relaxed);
    SOUND_SCENERY.store(settings.vol_scenery.to_bits(), std::sync::atomic::Ordering::Relaxed);
    MIRROR_SIZE.store(settings.mirror_size, std::sync::atomic::Ordering::Relaxed);
    omsi_audio::DOPPLER.store(settings.doppler, std::sync::atomic::Ordering::Relaxed);
    CLASSIC.store(
        settings.classic() && !ENHANCED.load(std::sync::atomic::Ordering::Relaxed),
        std::sync::atomic::Ordering::Relaxed,
    );
    // the LAN session (offscreen too, so that one game's view of another can be rendered);
    // a joining player's world is the host's (taken over again when the window loads it)
    let mut lan = if args.export_glb.is_none() {
        lan::start(&args)
    } else {
        None
    };
    // the host's mods: served by the host, fetched by a joining player before its world is
    // made (see `lan_mods`)
    lan_mods::remove_stale();
    if let Some(l) = lan.as_mut() {
        lan::share_mods(&mut args, l);
        lan::take_host_map(&mut args, l);
    }
    if let (Some(cfg), Some(l)) = (server_cfg.as_ref(), lan.as_ref()) {
        lan::open_public_gateway(l, server::info_of(cfg), cfg.web_port, cfg.tunnel);
        lan::publish_vehicles(args.root.clone(), cfg.vehicles.clone());
        if cfg.tunnel {
            // the address to give the players, as soon as cloudflared says it
            std::thread::spawn(|| {
                for _ in 0..300 {
                    if let Some(u) = lan::tunnel_url() {
                        println!("\n  Server address for the players: {u}\n  (Multiplayer -> Servers -> Add)\n");
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                println!("  No tunnel address (is cloudflared installed?): players join at this machine's address and the UDP port");
            });
        }
    }
    // a host's clock runs at its time speed (a server's: its server.cfg)
    if let (Some(l), None) = (lan.as_mut(), server_cfg.as_ref()) {
        if l.role == omsi_net::Role::Host {
            l.clock_speed = if settings.time_sync { 1.0 } else { settings.time_speed.clamp(1.0, 30.0) };
        }
    }
    let mut lan_game = lan::LanGame::default();
    if args.export_glb.is_none() && args.lan_join.is_some() {
        if let Some(t) = lan.as_ref().and_then(lan::host_time_now) {
            args.time = t;
        }
        place_on_duty(&mut args);
    }
    if let Some(out) = args.export_glb.clone() {
        return run_export(&args, &out).map(|_| None);
    }
    if let Some(out) = args.offscreen.clone() {
        if let Some(l) = lan.as_mut() {
            lan::adopt_host_world(&mut args, l, &mut lan_game);
        }
        let r = run_offscreen(&args, &out, lan, lan_game);
        lan_mods::clean_up();
        return r.map(|_| None);
    }
    let view = args.view.clone();
    let args_root_for_keys = args.root.clone();
    let clock_note = args.clock_moved.clone();
    let mut app = App {
        args,
        instance: graphics_instance(),
        window: None,
        surface: None,
        renderer: None,
        #[cfg(windows)]
        vr: None,
        scene: None,
        camera: None,
        player: None,
        placed: Vec::new(),
        chooser: None,
        editor: None,
        vehicle_list: Vec::new(),
        dropdown: None,
        vehicle_meta: std::collections::HashMap::new(),
        world: None,
        streamer: None,
        starting: None,
        traffic: None,
        schedule: None,
        humans: None,
        duty: None,
        duty_places: false,
        hud: None,
        navigator: None,
        vr_nav_profiles: crate::vr_navigator::Profiles::load(),
        vr_nav_edit: None,
        spanned: false,
        ui: ui::Ui::new(),
        fps: 0.0,
        rain: rain::Rain::new(),
        splashes: puddles::Splashes::new(),
        lamps_on: None,
        menu: None,
        populate_t: 0.0,
        humans_populate_t: 0.0,
        radio: radio::Radio::load(&args_root_for_keys),
        profile: Default::default(),
        profile_prev: Default::default(),
        first_populate: true,
        envir: None,
        weather: None,
        clock: omsi_sim::SimClock::default(),
        started: Instant::now(),
        total_frames: 0,
        mirror_budget: 1.0,
        mirrors_seen: 2,
        mirror_turn: 0,
        frozen_mirrors: None,
        mirror_hud: Default::default(),
        hover_key: None,
        view,
        audio: None,
        ambience: None,
        cursor: (0.0, 0.0),
        vr_cursor_physical: None,
        vr_cursor_warp_pending: None,
        window_focused: false,
        input_away: false,
        window_hidden: false,
        keys: Default::default(),
        door_key_triggers: Default::default(),
        last: Instant::now(),
        speed: 30.0,
        mouse_look: false,
        buttons_held: (false, false),
        both_drag: None,
        vr_zoom_active: false,
        hover: None,
        hover_part: None,
        hover_hand: false,
        head_idle_hold: Default::default(),
        input_script: parse_input_script(),
        shot: None,
        paused: false,
        game_menu: None,
        menu_top: None,
        menu_scroll_drag: false,
        dd_scroll_drag: None,
        pane_scroll_drag: None,
        pane_scroll: None,
        plugin_keys: Vec::new(),
        clock_hold: 0.0,
        pad_look: [false; 4],
        arrow_glance: false,
        teleport_pick: false,
        discord: None,
        discord_t: 0.0,
        #[cfg(steam)]
        steam: None,
        voice: None,
        headtrack: None,
        headtrack_failed: None,
        controllers: None,
        mouse_drive: false,
        mouse_steer: (0.0, 0.0),
        mouse_edge: 0.0,
        steer_cursor: None,
        center_cursor: false,
        cursor_hidden: None,
        last_ctl_steer: None,
        mouse_pedals: (0.0, 0.0),
        mouse_kmh: 0.0,
        tutorial: None,
        ego: false,
        on_foot: None,
        remote_walkers: Vec::new(),
        in_cab: false,
        inside_remote: None,
        is_admin: false,
        safe_pose: None,
        safe_age: 0.0,
        wheel_acc: 0.0,
        editor_drag: false,
        editor_sync_t: 0.0,
        remote_added: Default::default(),
        placing: None,
        admin_list: None,
        list_kind: None,
        route_arrows: Default::default(),
        game_keys: omsi_content::KeyboardCfg::load(&crate::startup::keyboard_cfg(&args_root_for_keys)).unwrap_or_default().with_game_defaults().with_vr_defaults().game,
        own_keys: crate::startup::own_keys(&args_root_for_keys),
        own_shift: crate::startup::own_bindings(&args_root_for_keys, omsi_content::input::KEY_SHIFT),
        menu_prev_pause: false,
        info_bar: false,
        pending_time: None,
        world_day: None,
        autosave_t: 0.0,
        timetable: false,
        dragging: false,
        html_pressed: None,
        html_object_pressed: None,
        drag_delta: (0.0, 0.0),
        look: (0.0, 0.0),
        view_looks: Default::default(),
        look_view: String::new(),
        cam_blend: Default::default(),
        view_zoom: Default::default(),
        orbit: ORBIT_DEFAULT,
        frames: 0,
        fps_t: Instant::now(),
        service_msg: clock_note.map(|m| (m, 10.0)),
        notices: Vec::new(),
        log_state: Default::default(),
        plugins: None,
        career: Default::default(),
        wetness: 0.0,
        cloud_drift: [0.0; 2],
        menu_edit: None,
        menu_edit_icao: false,
        swap_pending: false,
        menu_drag: None,
        menu_kbd: true,
        weather_blend: None,
        weather_cycle: None,
        metar_rx: None,
        metar_once: false,
        metar_next: 0.0,
        cursor_kind: 0,
        settings,
        lan: None,
        remotes: Default::default(),
        spikes: 0,
        worst_ms: 0.0,
        governor: (0.0, 0, 0.0),
        governor_low: 0,
        governor_wait_prev: 0.0,
        hidden_frames: 0,
        exiting: false,
        stand_in: None,
        cpu_mark: None,
        touch: touch::Touch::new(),
    };
    app.lan = lan;
    app.remotes = lan_game;
    // mouse steering as the player left it (the wheel eases to the cursor for a second)
    if app.settings.mouse_steering {
        app.mouse_drive = true;
        app.mouse_steer = (0.0, 1.0);
        app.center_cursor = true;
    }
    // (the LAN status file stays while the game runs; `exiting` removes it)
    std::mem::forget(_lan_status);
    Ok(Some(app))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_starts_around_the_bus() {
        let cam = Camera {
            position: DVec3::new(5000.0, 6000.0, 50.0),
            yaw: 0.0,
            pitch: 0.0,
            roll: 0.0,
            fov_deg: 60.0,
            near: 0.5,
            far: 100.0,
        };
        let args = |extra: &[&str]| Args::parse_from(["omsi"].iter().chain(extra.iter()).copied());
        // the camera alone
        assert_eq!(start_centers(&args(&[]), &cam, None), vec![cam.position]);
        // a bus viewed from its seat: only where it stands, even with a camera given (the
        // view follows the bus)
        let seat = vec![DVec3::new(10.0, 20.0, 0.0)];
        assert_eq!(
            start_centers(
                &args(&["--bus", "Vehicles/x.bus", "--spawn", "10,20,90"]),
                &cam,
                None
            ),
            seat
        );
        assert_eq!(
            start_centers(
                &args(&[
                    "--bus",
                    "Vehicles/x.bus",
                    "--spawn",
                    "10,20,90",
                    "--cam",
                    "5000,6000,50,0,0"
                ]),
                &cam,
                None
            ),
            seat
        );
        // ... and with the free camera: both places
        let both = start_centers(
            &args(&[
                "--bus",
                "Vehicles/x.bus",
                "--spawn",
                "10,20,90",
                "--cam",
                "5000,6000,50,0,0",
                "--view",
                "free",
            ]),
            &cam,
            None,
        );
        assert_eq!(both, vec![DVec3::new(10.0, 20.0, 0.0), cam.position]);
    }
}