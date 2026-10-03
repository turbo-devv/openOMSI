//! The user's settings: graphics and gameplay switches, kept as `key=value` lines in
//! `~/.openomsi/settings.cfg` (the launcher writes the same file). Anything missing
//! keeps its default, so an old file never breaks a new build.

use std::path::PathBuf;

/// Version of the settings file (`version=`); files without it are version 1.
pub const SETTINGS_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Samples per pixel: 1, 2, 4 or 8.
    pub msaa: u32,
    /// Anisotropic filtering 1..16.
    pub anisotropy: u16,
    pub ssao: bool,
    pub shadows: bool,
    pub shadow_size: u32,
    /// Draw the models' `[isshadow]` shadow meshes: OMSI's flat blob under a vehicle,
    /// standing in for the sky light the body keeps off the road. Off, only the sun shadow
    /// map shades under a vehicle (the blob is a fake OMSI draws whatever the depth).
    pub shadow_blobs: bool,
    /// The route navigator in the lower right corner.
    pub navigator: bool,
    /// How much of the interface's backgrounds shows - the navigator's, the menu's, the
    /// timetable's, the notes' plates - 0.2 .. 1, the texts staying solid: 0.85 (the default)
    /// as designed (`ui::backdrop`). `navigator_opacity` in older files: it was the
    /// navigator's alone.
    pub ui_opacity: f32,
    /// Which corner the navigator sits in: `bottom-left` (default), `bottom-right`,
    /// `top-left` or `top-right`.
    pub navigator_corner: String,
    /// How passengers board: `auto` - they pay at the cash desk and take the ticket
    /// themselves; `pay` - they wait at the desk for the driver to sell the ticket (the
    /// ticket key or the printer); `walk` - they just walk into the saloon (a flat-fare
    /// or ticket-machine service).
    pub boarding: String,
    /// Procedural detail (fractal) texturing of the ground and large walls when close.
    pub detail_textures: bool,
    /// Passengers pay the exact fare (no change to give at the cash desk).
    pub exact_fare: bool,
    /// Enhanced graphics: the physically based renderer (its own lighting, sky, exposure).
    pub enhanced: bool,
    /// The graphics: `vanilla` (as OMSI 2 draws it: no sun shadows, no ambient occlusion,
    /// no detail grain, no snow cover or rain drops of our own), `vanilla_plus` (the same
    /// renderer with those extras, the default) or `enhanced` (`enhanced` follows it).
    pub graphics: String,
    /// Start a PC OpenXR headset session when the game starts (Windows only).
    pub vr: bool,
    /// Fraction of the OpenXR runtime's recommended eye resolution.
    pub vr_scale: f32,
    /// Optional VR head pose smoothing time in milliseconds; zero uses raw tracking.
    pub vr_head_smoothing_ms: f32,
    /// Total bus mirror redraws per second in VR; zero freezes them,
    /// and -1 redraws every mirror once per game frame.
    pub vr_mirror_rate: f32,
    /// Copy the left eye to the desktop while VR is active.
    pub vr_desktop_mirror: bool,
    pub triple: omsi_render::TripleScreen,
    pub triple_span: bool,
    pub triple_hud_center: bool,
    pub fullscreen: bool,
    pub vsync: bool,
    /// Master volume 0..1.
    pub volume: f32,
    /// Control preset: "simple", "wasd", "arrows" or "omsi".
    pub drive_keys: String,
    /// Anti-aliasing of the enhanced picture after tone mapping: `fxaa` (default) or `off`.
    pub post_aa: String,
    /// OMSI's maintenance condition (`[wear_lifespan]`): 0 infinite (no wear), 1 very bad,
    /// 2 bad, 3 normal, 4 good - the player's bus's `wearlifespan` 1.5e6, 0.01, 0.1, 1, 10
    ///; AI vehicles never wear.
    pub maintenance: u8,
    /// `[AIUnschedFactor]`: the share of the random traffic (percent of the map's density).
    pub ai_unsched_factor: f32,
    /// `[AIMaxCountScheduled]`: timetable vehicles on the road at once (0 = no limit).
    pub ai_max_scheduled: u32,
    /// `[AIMaxCountParked]`: parked cars placed in the loaded tiles (0 = every space, -1 =
    /// none at all).
    pub ai_max_parked: i32,
    /// `[AIMaxCountRandom]`'s second line: Omsi.exe makes this many people at the start and
    /// no more (0x709274, "Create Humans") - everybody waiting, walking and riding comes out
    /// of them, and at most half walk the pavements (0x62463c).
    pub ai_max_humans: u32,
    /// `[no_collision_vehToVeh]` off: the player's bus collides with the traffic.
    pub collision_vehicles: bool,
    /// `[no_collision]` off: the player's bus collides with the map's solid objects.
    pub collision_objects: bool,
    /// `[no_collision_pedastrians]` off: people are knocked down.
    pub collision_pedestrians: bool,
    /// `[driverview_moving]`: the driver's head moves with the bus (braking, bends, bumps).
    pub head_movement: bool,
    /// The interior camera glides between viewpoints (OMSI's `[driverview_smooth]`).
    pub driverview_smooth: bool,
    /// The driver's hands on the wheel in the cab view (the rest of the figure left out).
    pub hands_in_cab: bool,
    /// OMSI's `[altView]` (options.cfg): the right mouse button turns the view and
    /// Shift+right zooms. Off, the right button zooms and the middle one turns the view
    /// (TForm_main.Panel1MouseMove 0x82c5f8) - OMSI's default, but on here by default: a
    /// laptop's touchpad has no middle button to look round with, and right-drag looking
    /// is what openOMSI always did.
    pub alt_view: bool,
    /// The 3D picture drawn at this fraction of the window's size and scaled up (0.5..1),
    /// 0 = automatic (full size unless the window has more pixels than a 2560x1080 screen,
    /// as a Retina window does). The HUD is always drawn at full size.
    pub render_scale: f32,
    /// Language of the texts the game shows about the cockpit: `ENG` (default), `DEU` or
    /// `FRA` - OMSI's own language file codes.
    pub language: String,
    /// What passengers say: `all`, `tickets` (only what they ask for) or `off`.
    pub pax_voices: String,
    /// OMSI 2's route arrows over the road (as well as or instead of the navigator).
    pub nav_arrows: bool,
    /// The other (AI) vehicles as dots on the navigator's and the city map.
    pub nav_ai: bool,
    /// The driver may get up from the seat and walk about (Ctrl+Shift+G).
    pub get_up: bool,
    /// Uncompressed texture files are compressed on loading where that leaves the picture
    /// close (DXT files always stay compressed on a GPU that takes them).
    pub texture_compression: bool,
    /// Texture memory the scenery may take (MB) before far textures lose their finest mip
    /// levels, like OMSI's `[texmemlimit]`; 0 = automatic (a share of the machine's memory).
    pub texture_memory: u32,
    /// OMSI's automatic clutch (`AutoClutch`, on unless `[no_automaticClutch]`): the
    /// manual-gearbox scripts work the clutch themselves while it is on.
    pub auto_clutch: bool,
    /// Manual gate buttons return the gearbox to neutral when released.
    pub momentary_gears: bool,
    /// The automated manual: a gear lever's gates are worked by the engine speed (#713).
    pub auto_shift: bool,
    /// The original's `performance_minObjSize`: objects smaller on the screen than this are
    /// not drawn (its presets say 0.013; 0.020 for slow machines, smaller keeps more).
    pub min_obj_size: f32,
    /// The original's `performance_maxObjDist` (m): objects farther away are not drawn
    /// (0 = no limit). `auto` (-1) takes `view_distance` when the file sets one, else 900 m
    /// (the original's high presets).
    pub max_obj_dist: f32,
    /// Frames a second at most (the original's `[maxFPS]`); 0 = no limit. The frame waits
    /// asleep, so a limit also saves power and heat, and the CPU time for the rest.
    pub max_fps: u32,
    /// The chat of a LAN session (V shows and hides it, / types); off leaves it out altogether.
    pub chat: bool,
    /// The name of what the cursor points at, shown next to the cursor.
    pub tooltips: bool,
    /// The other players' names above their buses.
    pub name_tags: bool,
    /// The driver sits in the player's bus, turning the wheel, seen from outside and the
    /// passengers' seats and in the mirrors (`driver`), never in the driver's own view.
    pub driver: bool,
    /// The frame rate in the HUD.
    pub show_fps: bool,
    /// The notes in the top left corner: why the bus does not move, the change due, what a
    /// service did (off, none are shown).
    pub notes: bool,
    /// The size of the game's interface over the picture - texts, the menu, the timetable,
    /// the navigator - on top of the screen's own scale and the window's height (see
    /// `ui::size_factor`): 0.5 .. 2 (1 = as designed).
    pub ui_scale: f32,
    /// The interface grows with a window taller than 1080 lines (`ui::size_factor`); off,
    /// it keeps its size on any window, as before.
    pub ui_scale_window: bool,
    /// Clouds in the sky (volumetric with enhanced graphics, OMSI's cloud layer without).
    pub clouds: bool,
    /// How many people wait and ride, against the map's own numbers (OMSI's `AIPassFactor`,
    /// 1 = 100 %).
    pub pax_density: f32,
    /// Volume of the AI vehicles and of the scenery's sounds (OMSI's `sound_ai`,
    /// `sound_scenery`), 0..1.
    pub vol_ai: f32,
    pub vol_scenery: f32,
    /// Edge of the mirrors' pictures in pixels (OMSI's `performance_reflTexSize`, 2^n); 0 disables mirror rendering.
    pub mirror_size: u32,
    /// How often the mirrors are drawn (OMSI's `performance_realreflexions`): `off` once
    /// for each bus and then still, `eco` 15 times a second at most, `full` 30 (the default).
    pub mirror_refresh: String,
    /// Copies of the main side mirrors in the corners of the window while in the cab: 0 off, 1 the right one, 2 the left, 3 both.
    pub mirror_hud: u8,
    /// OMSI's `sound_doppler`: approaching sounds higher, receding ones lower.
    pub doppler: bool,
    /// How fast the clock runs (1 real time .. 30); in LAN play the host's decides.
    pub time_speed: f64,
    /// The game's date and time follow this device's clock (the host's in LAN play); the
    /// time cannot be changed while it is on.
    pub time_sync: bool,
    /// The weather follows the METAR report of the airport nearest the map (refreshed every
    /// few minutes); the weather cannot be changed while it is on.
    pub metar_sync: bool,
    /// The airport (ICAO) whose report the METAR sync follows; empty: the one nearest the map.
    pub metar_station: String,
    /// Texts the interface has no translation of are translated on this machine by a
    /// neural translation model (downloaded once, ~620 MB), for the languages OMSI has no
    /// language files of.
    pub machine_translation: bool,
    /// Which meshes cast sun shadows: "all" solid ones, or "omsi" - only those the models
    /// mark `[shadow]`, as OMSI 2's shadows do.
    pub shadow_casters: String,
    /// Dead zone round the centre of a set-up game controller's axes (0..0.3).
    pub ctrl_deadzone: f32,
    /// Game controllers switched off, by name (`|` between them).
    pub ctrl_off: String,
    /// Keyboard steering at OMSI's steady pace (`KeyboardAxes::linear`).
    pub steering_linear: bool,
    /// The wheel stays where the keys left it (`KeyboardAxes::old_steering`).
    pub old_steering: bool,
    /// OMSI's `[redSteerSpd]`: the steering keys slower at speed (`KeyboardAxes::red_steer_spd`).
    pub red_steer_spd: bool,
    /// The materials' reflection maps (`RenderOptions::reflections`).
    pub reflections: bool,
    /// How bright an LED panel's dots burn (`Lighting::led_glow`): 0 (off) .. 15, 16 levels.
    pub led_glow: u8,
    /// How much of the mip chain an LED panel is held at (`Lighting::led_mips`): its own
    /// picture and its `\S:n` mask are sampled at the level their screen footprint asks
    /// for, never coarser than this. 0 point-samples them (the sharpest dots, and the
    /// worst shimmer); 1.3 (the default) leaves a matrix's dots a couple of pixels across
    /// when the panel is small; 4 is near the calm of the full chain.
    pub led_mips: f32,
    /// Mouse steering: how far the wheel turns for the same hand movement (1 = OMSI's: the
    /// window's width is the full lock).
    pub mouse_sens: f32,
    /// The graphics interface: `auto` (Vulkan, else DirectX 12, else OpenGL), `vulkan`,
    /// `dx12` or `gl` (see `startup::graphics_instance`).
    pub graphics_api: String,
    /// Default motor polarity for wheels without a saved per-device direction.
    pub ff_invert: bool,
    /// Force feedback and rumble at all (off: the controller neither pushes nor shakes).
    pub ff_enabled: bool,
    /// OMSI's held brake on the keyboard (see `KeyboardAxes::pedal_hold`); `brake_hold` in
    /// the file - the old `pedal_hold` (off unless set, and holding the throttle as well)
    /// is left behind.
    pub brake_hold: bool,
    /// Mouse steering as it was left last time (O or the menu switches it; the game
    /// starts with it as the player had it).
    pub mouse_steering: bool,
    /// A right click ends the mouse steering, as in Omsi.exe (0x6f44aa: the right button
    /// held while the mouse steers fires `toggel_mouse_ctrl`). Off by default: looking
    /// round with the right button dragged switched it off every time.
    pub mouse_right_off: bool,
    /// How fast the view turns for the mouse's way while looking round (1 = Omsi.exe's
    /// field of view / 78.75 degrees per pixel in the views of the bus; #859).
    pub look_sens: f32,
    /// The bus's script may cancel the indicator after a turn (#451: off, an indicator set
    /// stays on until the player turns it off).
    pub blinker_cancel: bool,
    /// The steering wheel's own rotation, lock to lock (degrees; a G29 turns 900).
    pub wheel_range: f32,
    /// How far the wheel is turned, lock to lock, for the bus's full lock (degrees); 0 = the
    /// whole of the wheel's rotation, as OMSI.
    pub wheel_lock: f32,
    /// Field of view of the views from the bus (degrees; 0 = the bus's own cameras).
    pub fov: f32,
    /// The outside camera is pulled in in front of what stands between it and the bus
    /// (off: it goes through everything, as in OMSI).
    pub camera_collision: bool,
    /// The driver's view turns a little into the steering (off: it stays fixed to the bus,
    /// as in OMSI, which has no such thing).
    pub steer_look: bool,
    /// Maximum automatic driver's-view yaw at full steering lock (degrees).
    pub steer_look_angle: f32,
    /// Steering-view smoothing time constant (seconds).
    pub steer_look_response: f32,
    /// How much the view sways on its own when nothing is done to it: a head at rest breathes
    /// and shifts its weight, and most of that is seen while the bus waits at a stop. 0 is not
    /// at all (as OMSI, and the default); 1 is the whole of the sway `head_idle` makes.
    pub head_idle: f32,
    /// How fast that sway moves, against the pace it is designed at (1: as designed; below 1
    /// slower, above it quicker).
    pub head_idle_pace: f32,
    /// How strongly the analog throttle and brake pedals act: the response curve's
    /// strength (1 = linear, below 1 softer at the start, above 1 stronger).
    pub pedal_throttle: f32,
    pub pedal_brake: f32,
    /// The driver's eye moved from the bus's own camera (m, bus frame: right, forward, up).
    pub seat: [f32; 3],
    /// Head tracking through opentrack's UDP output (TrackIR, webcams, phones), and its port.
    pub head_tracking: bool,
    pub head_tracking_port: u16,
    /// Axes of the tracker turned the other way (`yaw,pitch,roll`): trackers disagree.
    pub head_tracking_invert: String,
    /// Discord's "Playing" status (Rich Presence) and the Discord application it shows as.
    pub discord_status: bool,
    pub discord_app_id: String,
    /// Positional voice in multiplayer through GreenTeaSpeak's openOMSI plugin (`voice`).
    pub voice_chat: bool,
}

/// A pedal's last few per cent of travel are its end: a wheel's pedal on the floor reads
/// 0.93..0.99, and the scripts ask for the ends exactly - the LiAZ/PAZ gearboxes put a gear
/// in only at `(L.L.clutch) 1 =` and part the engine from the wheels only above 0.95, so a
/// clutch held down to the floor still dragged and the engine died at every stop.
pub fn pedal_ends(v: f32) -> f32 {
    if v >= 0.96 {
        1.0
    } else if v <= 0.02 {
        0.0
    } else {
        v
    }
}

/// A pedal as the settings shape it: `v` 0..1 through the response curve of `strength`.
pub fn pedal_curve(v: f32, strength: f32) -> f32 {
    let g = strength.clamp(0.25, 4.0);
    v.clamp(0.0, 1.0).powf(1.0 / g)
}

impl Default for Settings {
    fn default() -> Self {
        if crate::platform::MOBILE {
            // a phone's graphics chip and battery: 2x MSAA (cheap on a tiled GPU), no
            // ambient occlusion, a smaller shadow map and mirrors, a shorter view
            return Self { msaa: 2, anisotropy: 4, ssao: false, shadow_size: 1024, mirror_size: 128, max_fps: 60, max_obj_dist: 900.0, pax_density: 0.7, navigator_corner: "top-center".into(), ..Self::desktop() };
        }
        Self::desktop()
    }
}

impl Settings {
    /// HUD coordinates occupy the centre panel by default; world picking always
    /// keeps the full window's physical coordinates.
    pub fn hud_viewport(&self, size: (u32, u32)) -> [f32; 4] {
        if self.triple.enabled && self.triple_hud_center && !self.vr_requested() && size.0 >= 3 {
            // (the centre panel, as the renderer draws it)
            let w = omsi_render::panel_width(size.0);
            [w as f32, 0.0, w as f32, size.1 as f32]
        } else {
            [0.0, 0.0, size.0 as f32, size.1 as f32]
        }
    }
    /// The defaults of a computer.
    fn desktop() -> Self {
        Self { msaa: 4, anisotropy: 8, ssao: true, shadows: true, shadow_size: 2048, shadow_blobs: true, navigator: true, ui_opacity: 0.85, notes: true, ui_scale: 1.0, ui_scale_window: true, navigator_corner: "bottom-left".into(), boarding: "auto".into(), detail_textures: true, exact_fare: true, enhanced: false, graphics: "vanilla_plus".into(), triple: Default::default(), triple_span: true, triple_hud_center: true, vr: false, vr_scale: 0.65, vr_head_smoothing_ms: 0.0, vr_mirror_rate: 16.0, vr_desktop_mirror: true, fullscreen: false, vsync: true, volume: 0.6, drive_keys: "simple".into(), post_aa: "fxaa".into(), render_scale: 0.0, language: "ENG".into(), pax_voices: "all".into(), nav_arrows: false, nav_ai: true, get_up: false, texture_compression: true, texture_memory: 0, auto_clutch: true, momentary_gears: false, auto_shift: false, min_obj_size: 0.013, max_obj_dist: -1.0, max_fps: 0, chat: true, tooltips: true, name_tags: true, show_fps: false, clouds: true, pax_density: 1.0, vol_ai: 1.0, vol_scenery: 1.0, mirror_size: 256, mirror_hud: 0, mirror_refresh: "full".into(), doppler: true, driver: true, maintenance: 0, ai_unsched_factor: 1.0, ai_max_scheduled: 0, ai_max_parked: 0, ai_max_humans: 200, collision_vehicles: true, collision_objects: true, collision_pedestrians: true, head_movement: true, driverview_smooth: true, hands_in_cab: false, alt_view: true, time_speed: 1.0, time_sync: false, metar_sync: false, metar_station: String::new(), machine_translation: false, shadow_casters: "all".into(), ctrl_deadzone: 0.0, ctrl_off: String::new(), steering_linear: false, old_steering: false, red_steer_spd: false, reflections: true, led_glow: 6, led_mips: 1.3, mouse_sens: 1.0, graphics_api: "auto".into(), ff_invert: false, ff_enabled: true, brake_hold: true, mouse_steering: false, mouse_right_off: false, look_sens: 1.0, blinker_cancel: true, wheel_range: 900.0, wheel_lock: 0.0, fov: 0.0, camera_collision: true, steer_look: false, steer_look_angle: 30.0, steer_look_response: 0.25, head_idle: 0.0, head_idle_pace: 1.0, pedal_throttle: 1.0, pedal_brake: 1.0, seat: [0.0; 3], head_tracking: false, head_tracking_port: 4242, head_tracking_invert: String::new(), discord_status: true, discord_app_id: String::new(), voice_chat: true }
    }
}

impl Settings {
    /// The launcher setting, with the old environment switch kept for existing VR runs.
    pub fn vr_requested(&self) -> bool {
        cfg!(windows) && (self.vr || omsi_cfg::env::var_os("OMSI_OPENXR").is_some())
    }

    /// `~/.openomsi/settings.cfg` (or `%USERPROFILE%` on Windows).
    pub fn path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
        Some(PathBuf::from(home).join(".openomsi").join("settings.cfg"))
    }

    /// The window's size in pixels the settings ask for (`resolution=WxH`, #904); None:
    /// automatic (fitted to the screen, the whole of it under gamescope).
    pub fn resolution() -> Option<(u32, u32)> {
        let text = std::fs::read_to_string(Self::path()?).ok()?;
        resolution_of(&text)
    }

    pub fn load() -> Settings {
        let Some(p) = Self::path() else { return Settings::default() };
        let mut text = std::fs::read_to_string(&p).unwrap_or_default();
        // OMSI_GRAPHICS=vanilla|vanilla_plus|enhanced: another renderer for one run
        if let Ok(g) = omsi_cfg::env::var("OMSI_GRAPHICS") {
            text.push_str(&format!("\ngraphics={g}\n"));
        }
        let mut s = Self::from_text(&text);
        // OMSI_SAFE_GPU=<n>: the game was started again after its graphics device was lost
        // (see `App::restart_after_device_loss`): lighter on the card each time
        if let Some(n) = omsi_cfg::env::var("OMSI_SAFE_GPU").ok().and_then(|v| v.parse::<u32>().ok()).filter(|n| *n > 0) {
            s.apply_safe_gpu(n);
        }
        log::info!("settings from {}: msaa {} af {} ssao {} shadows {} ({}) navigator {} graphics {} post aa {} vsync {} render scale {} boarding {} min object size {} max object distance {} max fps {}", p.display(), s.msaa, s.anisotropy, s.ssao, s.shadows, s.shadow_size, s.navigator, s.graphics, s.post_aa, s.vsync, s.render_scale_text(), s.boarding, s.min_obj_size, s.object_distance(), s.max_fps);
        s
    }

    /// The settings a `settings.cfg` text describes (anything missing keeps its default).
    pub fn from_text(text: &str) -> Settings {
        let mut s = Settings::default();
        let mut version = 0u32;
        let mut graphics: Option<String> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            let b = |v: &str| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes");
            match k.as_str() {
                "version" => version = v.parse().unwrap_or(0),
                "msaa" => s.msaa = v.parse().unwrap_or(s.msaa),
                "anisotropy" | "af" => s.anisotropy = v.parse().unwrap_or(s.anisotropy),
                "ssao" | "ambient_occlusion" => s.ssao = b(v),
                "shadows" => s.shadows = b(v),
                "shadow_size" => s.shadow_size = v.parse().unwrap_or(s.shadow_size),
                "shadow_blobs" => s.shadow_blobs = b(v),
                "navigator" => s.navigator = b(v),
                "ui_opacity" | "navigator_opacity" => s.ui_opacity = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.2, 1.0)).unwrap_or(s.ui_opacity),
                "navigator_corner" => s.navigator_corner = v.to_ascii_lowercase(),
                "boarding" => s.boarding = v.to_ascii_lowercase(),
                "detail_textures" | "fractal" => s.detail_textures = b(v),
                "exact_fare" => s.exact_fare = b(v),
                "enhanced" => s.enhanced = b(v),
                "graphics" | "renderer" => graphics = Some(graphics_mode(v).to_string()),
                "vr" => s.vr = b(v),
                "vr_scale" => s.vr_scale = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.5, 1.0)).unwrap_or(s.vr_scale),
                "vr_head_smoothing_ms" => s.vr_head_smoothing_ms = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 30.0)).unwrap_or(s.vr_head_smoothing_ms),
                "vr_mirror_rate" => s.vr_mirror_rate = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(-1.0, 360.0)).unwrap_or(s.vr_mirror_rate),
                "vr_desktop_mirror" => s.vr_desktop_mirror = b(v),
                "triple_screen" => s.triple.enabled = b(v),
                "triple_span" => s.triple_span = b(v),
                "triple_hud_center" => s.triple_hud_center = b(v),
                "triple_fov_deg" => {
                    s.triple.fov_deg = v
                        .parse::<f32>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .map(|x| if x < 20.0 { 0.0 } else { x.min(120.0) })
                        .unwrap_or(s.triple.fov_deg)
                }
                "triple_width_mm" => {
                    s.triple.width_mm = v
                        .parse::<f32>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .map(|x| x.clamp(200.0, 2000.0))
                        .unwrap_or(s.triple.width_mm)
                }
                "triple_distance_mm" => {
                    s.triple.distance_mm = v
                        .parse::<f32>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .map(|x| x.clamp(200.0, 3000.0))
                        .unwrap_or(s.triple.distance_mm)
                }
                "triple_bezel_mm" => {
                    s.triple.bezel_mm = v
                        .parse::<f32>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .map(|x| x.clamp(0.0, 100.0))
                        .unwrap_or(s.triple.bezel_mm)
                }
                "triple_left_angle_deg" => {
                    s.triple.left_angle_deg = v
                        .parse::<f32>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .map(|x| x.clamp(0.0, 90.0))
                        .unwrap_or(s.triple.left_angle_deg)
                }
                "triple_right_angle_deg" => {
                    s.triple.right_angle_deg = v
                        .parse::<f32>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .map(|x| x.clamp(0.0, 90.0))
                        .unwrap_or(s.triple.right_angle_deg)
                }
                "triple_eye_height_mm" => {
                    s.triple.eye_height_mm = v
                        .parse::<f32>()
                        .ok()
                        .filter(|x| x.is_finite())
                        .map(|x| x.clamp(-500.0, 500.0))
                        .unwrap_or(s.triple.eye_height_mm)
                }
                "fullscreen" => s.fullscreen = b(v),
                "vsync" => s.vsync = b(v),
                "volume" => s.volume = v.parse().unwrap_or(s.volume),
                // "auto", a fraction (0.75) or a percentage (75)
                "render_scale" => {
                    s.render_scale = if v.eq_ignore_ascii_case("auto") {
                        0.0
                    } else {
                        match v.trim_end_matches('%').parse::<f32>() {
                            Ok(x) if x > 1.5 => (x / 100.0).clamp(0.5, 1.0),
                            Ok(x) if x > 0.0 => x.clamp(0.5, 1.0),
                            Ok(_) => 0.0,
                            Err(_) => s.render_scale,
                        }
                    }
                }
                "language" | "lang" => s.language = crate::describe::language_code(v),
                "pax_voices" => s.pax_voices = match v.to_ascii_lowercase().as_str() { "tickets" => "tickets".into(), "off" | "0" | "none" => "off".into(), _ => "all".into() },
                "nav_arrows" => s.nav_arrows = b(v),
                "nav_ai" => s.nav_ai = b(v),
                "get_up" => s.get_up = b(v),
                "texture_compression" => s.texture_compression = b(v),
                "auto_clutch" | "automatic_clutch" => s.auto_clutch = b(v),
                "momentary_gears" | "gear_buttons_hold" => s.momentary_gears = b(v),
                "auto_shift" => s.auto_shift = b(v),
                "min_obj_size" | "performance_minobjsize" => s.min_obj_size = v.parse::<f32>().map(|x| x.clamp(0.0, 0.2)).unwrap_or(s.min_obj_size),
                "max_obj_dist" | "performance_maxobjdist" => s.max_obj_dist = if v.eq_ignore_ascii_case("off") { 0.0 } else if v.eq_ignore_ascii_case("auto") { -1.0 } else { v.parse::<f32>().map(|x| x.max(0.0)).unwrap_or(s.max_obj_dist) },
                "max_fps" | "maxfps" => {
                    s.max_fps = v.parse::<f32>().map(|x| x.max(0.0) as u32).unwrap_or(s.max_fps);
                    // a phone given the PC OMSI's 30 by the settings import: 60
                    if cfg!(target_os = "android") && s.max_fps == 30 {
                        s.max_fps = 60;
                    }
                }
                "chat" => s.chat = b(v),
                "tooltips" | "mouseover" => s.tooltips = b(v),
                "name_tags" | "nametags" => s.name_tags = b(v),
                "driver" => s.driver = b(v),
                "show_fps" | "fps" => s.show_fps = b(v),
                "clouds" => s.clouds = b(v),
                "pax_density" | "aipassfactor" => s.pax_density = v.trim_end_matches('%').parse::<f32>().map(|x| if x > 5.0 { x / 100.0 } else { x }).map(|x| x.clamp(0.0, 3.0)).unwrap_or(s.pax_density),
                "vol_ai" => s.vol_ai = v.parse::<f32>().map(|x| x.clamp(0.0, 1.0)).unwrap_or(s.vol_ai),
                "vol_scenery" => s.vol_scenery = v.parse::<f32>().map(|x| x.clamp(0.0, 1.0)).unwrap_or(s.vol_scenery),
                "doppler" | "sound_doppler" => s.doppler = b(v),
                "mirror_hud" => s.mirror_hud = v.parse::<u8>().map(|x| x.min(3)).unwrap_or(s.mirror_hud),
                "mirror_size" => s.mirror_size = v.parse::<u32>().map(|x| if x == 0 { 0 } else { x.clamp(64, 2048).next_power_of_two() }).unwrap_or(s.mirror_size),
                "mirror_refresh" | "performance_realreflexions" => s.mirror_refresh = match v.trim().to_ascii_lowercase().as_str() {
                    "off" | "none" => "off",
                    "eco" | "economy" => "eco",
                    _ => "full",
                }
                .into(),
                "texture_memory" | "texmemlimit" => s.texture_memory = v.parse::<f32>().map(|x| x.max(0.0) as u32).unwrap_or(s.texture_memory),
                "maintenance" | "wear_lifespan" => s.maintenance = v.parse::<u8>().map(|x| x.min(4)).unwrap_or(s.maintenance),
                "ai_unsched_factor" | "aiunschedfactor" => s.ai_unsched_factor = v.trim_end_matches('%').parse::<f32>().map(|x| (x / 100.0).clamp(0.0, 3.0)).unwrap_or(s.ai_unsched_factor),
                "ai_max_scheduled" | "aimaxcountscheduled" => s.ai_max_scheduled = v.parse().unwrap_or(s.ai_max_scheduled),
                "ai_max_humans" | "aimaxhumans" => s.ai_max_humans = v.parse::<u32>().map(|x| x.max(1)).unwrap_or(s.ai_max_humans),
                "ai_max_parked" | "aimaxcountparked" => s.ai_max_parked = v.parse::<i32>().map(|x| x.max(-1)).unwrap_or(s.ai_max_parked),
                "collision_vehicles" => s.collision_vehicles = b(v),
                "collision_objects" => s.collision_objects = b(v),
                "collision_pedestrians" => s.collision_pedestrians = b(v),
                "head_movement" | "driverview_moving" => s.head_movement = b(v),
                "driverview_smooth" => s.driverview_smooth = b(v),
                "hands_in_cab" => s.hands_in_cab = b(v),
                "alt_view" => s.alt_view = b(v),
                "time_speed" => s.time_speed = v.trim_start_matches(['x', 'X']).parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(1.0, 30.0)).unwrap_or(s.time_speed),
                "time_sync" | "real_time_sync" => s.time_sync = b(v),
                "metar_sync" => s.metar_sync = b(v),
                "metar_station" => s.metar_station = v.trim().chars().filter(|c| c.is_ascii_alphabetic()).take(4).collect::<String>().to_ascii_uppercase(),
                "machine_translation" => s.machine_translation = b(v),
                "ctrl_deadzone" => s.ctrl_deadzone = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 0.3)).unwrap_or(s.ctrl_deadzone),
                "reflections" | "envmap" => s.reflections = b(v),
                "led_glow" => s.led_glow = v.trim().parse::<i32>().map(|x| x.clamp(0, 15) as u8).unwrap_or(s.led_glow),
                "led_mips" => s.led_mips = v.trim().parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 4.0)).unwrap_or(s.led_mips),
                "graphics_api" => s.graphics_api = v.trim().to_ascii_lowercase(),
                "ctrl_off" => s.ctrl_off = v.trim().to_string(),
                "steering_linear" => s.steering_linear = b(v),
                "old_steering" => s.old_steering = b(v),
                "red_steer_spd" => s.red_steer_spd = b(v),
                "ff_invert" => s.ff_invert = b(v),
                "ff_enabled" => s.ff_enabled = b(v),
                "brake_hold" => s.brake_hold = b(v),
                "mouse_steering" => s.mouse_steering = b(v),
                "mouse_right_off" => s.mouse_right_off = b(v),
                "blinker_cancel" => s.blinker_cancel = b(v),
                "look_sens" => s.look_sens = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.1, 2.0)).unwrap_or(s.look_sens),
                "wheel_range" => s.wheel_range = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(90.0, 2880.0)).unwrap_or(s.wheel_range),
                "wheel_lock" => s.wheel_lock = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| if x < 45.0 { 0.0 } else { x.min(2880.0) }).unwrap_or(s.wheel_lock),
                "camera_collision" => s.camera_collision = b(v),
                "steer_look" => s.steer_look = b(v),
                "steer_look_angle" => s.steer_look_angle = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 60.0)).unwrap_or(s.steer_look_angle),
                "steer_look_response" => s.steer_look_response = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.05, 1.0)).unwrap_or(s.steer_look_response),
                "head_idle" => s.head_idle = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 1.0)).unwrap_or(s.head_idle),
                "head_idle_pace" => s.head_idle_pace = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.5, 2.0)).unwrap_or(s.head_idle_pace),
                "head_tracking" => s.head_tracking = b(v),
                "head_tracking_invert" => s.head_tracking_invert = v.to_ascii_lowercase(),
                "discord_status" => s.discord_status = b(v),
                "discord_app_id" => s.discord_app_id = v.trim().to_string(),
                "voice_chat" => s.voice_chat = b(v),
                "head_tracking_port" => s.head_tracking_port = v.parse::<u16>().ok().filter(|p| *p > 0).unwrap_or(s.head_tracking_port),
                "pedal_throttle" => s.pedal_throttle = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.25, 4.0)).unwrap_or(s.pedal_throttle),
                "pedal_brake" => s.pedal_brake = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.25, 4.0)).unwrap_or(s.pedal_brake),
                "seat_x" | "seat_y" | "seat_z" => {
                    let k = (k.as_bytes()[5] - b'x') as usize;
                    s.seat[k] = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(-1.5, 1.5)).unwrap_or(0.0);
                }
                "fov" => s.fov = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| if x < 20.0 { 0.0 } else { x.min(120.0) }).unwrap_or(s.fov),
                "mouse_sens" => s.mouse_sens = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.1, 3.0)).unwrap_or(s.mouse_sens),
                "ui_scale_window" => s.ui_scale_window = b(v),
                "notes" => s.notes = b(v),
                "ui_scale" => s.ui_scale = v.parse::<f32>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.5, 2.0)).unwrap_or(s.ui_scale),
                "shadow_casters" => s.shadow_casters = if v.eq_ignore_ascii_case("omsi") { "omsi".into() } else { "all".into() },
                "post_aa" => s.post_aa = if matches!(v.to_ascii_lowercase().as_str(), "off" | "0" | "none" | "false") { "off".into() } else { "fxaa".into() },
                "drive_keys" => s.drive_keys = match v.to_ascii_lowercase().as_str() { "wasd" | "arrows" | "omsi" | "simple" => v.to_ascii_lowercase(), _ => s.drive_keys },
                _ => {}
            }
        }
        // `graphics` decides; a file without it (older builds) says only `enhanced`, and
        // its vanilla renderer is what is now called Vanilla+
        s.graphics = graphics.unwrap_or_else(|| if s.enhanced { "enhanced" } else { "vanilla_plus" }.to_string());
        s.enhanced = s.graphics == "enhanced";
        if s.classic() {
            s.shadows = false;
            s.ssao = false;
            s.detail_textures = false;
        }
        // Files older than version 2 say `boarding=pay` because that was the launcher's
        // default, not because anybody chose it: passengers then stood at the cash desk
        // waiting for a driver who did not know he had to sell them a ticket.
        if version < SETTINGS_VERSION && s.boarding == "pay" {
            log::info!("settings: boarding=pay from an old settings file taken as auto (choose pay again in the launcher to keep it)");
            s.boarding = "auto".into();
        }
        s
    }

    /// The settings as the file holds them. The game only ever reads the file - the
    /// launcher's settings page writes it - so this is here for the round-trip test that
    /// every key read is written back.
    #[cfg(test)]
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "# openOMSI settings\nversion={}\nmsaa={}\nanisotropy={}\nssao={}\nshadows={}\nshadow_size={}\nshadow_blobs={}\nnavigator={}\nui_opacity={}\nnavigator_corner={}\nboarding={}\ndetail_textures={}\nexact_fare={}\nenhanced={}\ngraphics={}\nvr={}\nvr_scale={}\nfullscreen={}\nvsync={}\nvolume={}\ndrive_keys={}\npost_aa={}\nrender_scale={}\nlanguage={}\ntexture_compression={}\ntexture_memory={}\nauto_clutch={}\nmomentary_gears={}\nmin_obj_size={}\nmax_obj_dist={}\nmax_fps={}\nchat={}\ntooltips={}\nname_tags={}\nshow_fps={}\nclouds={}\npax_density={}\nvol_ai={}\nvol_scenery={}\nmirror_size={}\nmirror_hud={}\nmirror_refresh={}\ndoppler={}\ndriver={}\ndriverview_smooth={}\n",
            SETTINGS_VERSION, self.msaa, self.anisotropy, self.ssao as u8, self.shadows as u8, self.shadow_size, self.shadow_blobs as u8, self.navigator as u8, self.ui_opacity, self.navigator_corner, self.boarding, self.detail_textures as u8, self.exact_fare as u8, self.enhanced as u8, self.graphics, self.vr as u8, self.vr_scale, self.fullscreen as u8, self.vsync as u8, self.volume, self.drive_keys, self.post_aa, self.render_scale_text(), self.language, self.texture_compression as u8, self.texture_memory, self.auto_clutch as u8, self.momentary_gears as u8, self.min_obj_size, if self.max_obj_dist < 0.0 { "auto".to_string() } else { self.max_obj_dist.to_string() }, self.max_fps, self.chat as u8, self.tooltips as u8, self.name_tags as u8, self.show_fps as u8, self.clouds as u8, self.pax_density, self.vol_ai, self.vol_scenery, self.mirror_size, self.mirror_hud, self.mirror_refresh, self.doppler as u8, self.driver as u8, self.driverview_smooth as u8
        );
        text.push_str(&format!(
            "vr_head_smoothing_ms={}\nvr_mirror_rate={}\nvr_desktop_mirror={}\nled_glow={}\nled_mips={}\nui_scale={}\nui_scale_window={}\nnotes={}\n",
            self.vr_head_smoothing_ms, self.vr_mirror_rate, self.vr_desktop_mirror as u8, self.led_glow, self.led_mips, self.ui_scale, self.ui_scale_window as u8, self.notes as u8,
        ));
        text.push_str(&format!(
            "triple_hud_center={}\ntriple_fov_deg={}\nfov={}\n",
            self.triple_hud_center as u8, self.triple.fov_deg, self.fov
        ));
        text.push_str(&format!("triple_screen={}\ntriple_span={}\ntriple_width_mm={}\ntriple_distance_mm={}\ntriple_bezel_mm={}\ntriple_left_angle_deg={}\ntriple_right_angle_deg={}\ntriple_eye_height_mm={}\n", self.triple.enabled as u8, self.triple_span as u8, self.triple.width_mm, self.triple.distance_mm, self.triple.bezel_mm, self.triple.left_angle_deg, self.triple.right_angle_deg, self.triple.eye_height_mm));
        text.push_str(&format!("steer_look={}\nsteer_look_angle={}\nsteer_look_response={}\nhead_idle={}\nhead_idle_pace={}\nlook_sens={}\nblinker_cancel={}\n", self.steer_look as u8, self.steer_look_angle, self.steer_look_response, self.head_idle, self.head_idle_pace, self.look_sens, self.blinker_cancel as u8));
        text.push_str(&format!("discord_status={}\ndiscord_app_id={}\n", self.discord_status as u8, self.discord_app_id));
        text.push_str(&format!("auto_shift={}\n", self.auto_shift as u8));
        text.push_str(&format!("voice_chat={}\n", self.voice_chat as u8));
        text
    }

    /// Vanilla graphics: the picture as OMSI 2 draws it.
    pub fn classic(&self) -> bool {
        self.graphics == "vanilla"
    }

    /// How far objects are drawn (m, 0 = no limit): `max_obj_dist`, or when that is `auto`
    /// the visible distance the launcher sets, else the original's 900 m.
    pub fn object_distance(&self) -> f32 {
        if self.max_obj_dist >= 0.0 {
            self.max_obj_dist
        } else {
            view_distance().map(|v| v as f32).unwrap_or(900.0)
        }
    }

    /// `auto` or the fraction, as the file and the log write it.
    pub fn render_scale_text(&self) -> String {
        if self.render_scale > 0.0 { format!("{}", self.render_scale) } else { "auto".into() }
    }

    /// Lighter graphics after the graphics device was lost `n` times this session: no
    /// multisampling, no SSAO, smaller shadow and mirror maps, fewer textures kept; a second
    /// loss also a smaller picture and no shadows.
    pub fn apply_safe_gpu(&mut self, n: u32) {
        self.msaa = 1;
        self.ssao = false;
        self.shadow_size = self.shadow_size.min(2048);
        self.mirror_size = self.mirror_size.min(256);
        let budget = if self.texture_memory > 0 { self.texture_memory } else { 1200 };
        self.texture_memory = (budget * 2 / 3).max(400);
        if n >= 2 {
            self.shadows = false;
            self.shadow_size = 1024;
            self.render_scale = if self.render_scale > 0.0 { self.render_scale.min(0.75) } else { 0.75 };
            self.texture_memory = self.texture_memory.min(700);
            self.mirror_size = self.mirror_size.min(128);
        }
        log::warn!("safer graphics after a lost graphics device ({n}): msaa 1, SSAO off, shadows {} ({}), textures {} MB, render scale {}", self.shadows, self.shadow_size, self.texture_memory, self.render_scale_text());
    }

    pub fn render_options(&self) -> omsi_render::RenderOptions {
        omsi_render::RenderOptions { msaa: self.msaa, anisotropy: self.anisotropy, shadow_size: self.shadow_size, ssao: self.ssao, render_scale: self.render_scale, compress_textures: self.texture_compression, fxaa: self.post_aa != "off", min_obj_size: self.min_obj_size, max_obj_dist: self.object_distance(), omsi_shadow_casters: self.shadow_casters == "omsi", shadow_blobs: self.shadow_blobs, reflections: self.reflections, no_enhanced: graphics_mode(&self.graphics) != "enhanced" }
    }
}

/// `vanilla`, `vanilla_plus` or `enhanced` from the ways a file may spell them.
pub fn graphics_mode(v: &str) -> &'static str {
    match v.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
        "enhanced" | "1" => "enhanced",
        "vanilla" | "classic" | "original" | "omsi" | "omsi2" | "omsi_2" => "vanilla",
        _ => "vanilla_plus",
    }
}

/// `view_distance=<metres>` of the settings file: how far around the camera the map's tiles
/// are kept loaded (OMSI's "visible distance"). None when the file does not say.
pub fn view_distance() -> Option<f64> {
    let text = std::fs::read_to_string(Settings::path()?).ok()?;
    text.lines().filter_map(|l| l.trim().split_once('=')).find(|(k, _)| k.trim().eq_ignore_ascii_case("view_distance")).and_then(|(_, v)| v.trim().parse::<f64>().ok()).filter(|v| *v > 0.0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn triple_hud_defaults_to_the_centre_panel_and_can_span() {
        let mut s = super::Settings::from_text("triple_screen=1\n");
        assert_eq!(s.hud_viewport((5760, 1080)), [1920.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(s.hud_viewport((5761, 1080)), [1921.0, 0.0, 1921.0, 1080.0]);
        s.triple_hud_center = false;
        assert_eq!(s.hud_viewport((5761, 1080)), [0.0, 0.0, 5761.0, 1080.0]);
        let s = super::Settings::from_text("triple_screen=0\n");
        assert_eq!(s.hud_viewport((1920, 1080)), [0.0, 0.0, 1920.0, 1080.0]);
    }

    #[test]
    fn triple_fov_and_hud_preferences_round_trip() {
        let s = super::Settings::from_text(
            "triple_screen=1\ntriple_hud_center=0\ntriple_fov_deg=75\nfov=50\n",
        );
        assert_eq!(super::Settings::from_text(&s.to_text()), s);
        assert_eq!(s.triple.fov_deg, 75.0);
        assert_eq!(s.fov, 50.0);
        assert_eq!(
            super::Settings::from_text("triple_fov_deg=NaN\n")
                .triple
                .fov_deg,
            0.0
        );
    }
    #[test]
    fn triple_settings_round_trip_and_reject_nonfinite_geometry() {
        let s = super::Settings::from_text("triple_screen=1\ntriple_span=0\ntriple_width_mm=620\ntriple_distance_mm=700\ntriple_bezel_mm=18\ntriple_left_angle_deg=50\ntriple_right_angle_deg=40\ntriple_eye_height_mm=60\n");
        assert_eq!(super::Settings::from_text(&s.to_text()), s);
        assert!(s.triple.enabled);
        assert!(!s.triple_span);
        let invalid = super::Settings::from_text("triple_width_mm=NaN\ntriple_distance_mm=inf\ntriple_left_angle_deg=-30\ntriple_bezel_mm=-20\n");
        assert_eq!(invalid.triple.width_mm, 600.0);
        assert_eq!(invalid.triple.distance_mm, 650.0);
        assert_eq!(invalid.triple.left_angle_deg, 0.0);
        assert_eq!(invalid.triple.bezel_mm, 0.0);
    }
    use super::*;

    #[test]
    fn old_files_board_automatically() {
        // the launcher's old default, written without a version
        assert_eq!(Settings::from_text("msaa=1\nboarding=pay\n").boarding, "auto");
        // chosen again in a current file, it stays
        assert_eq!(Settings::from_text("version=2\nboarding=pay\n").boarding, "pay");
        assert_eq!(Settings::from_text("boarding=walk\n").boarding, "walk");
        // what we write reads back the same
        let s = Settings { boarding: "pay".into(), ..Default::default() };
        assert_eq!(Settings::from_text(&s.to_text()), s);
        let s = Settings { texture_compression: false, texture_memory: 1500, ..Default::default() };
        assert_eq!(Settings::from_text(&s.to_text()), s);
        assert_eq!(Settings::from_text("texmemlimit=401.0\n").texture_memory, 401);
    }

    #[test]
    fn graphics_modes() {
        // an old file: its vanilla renderer is Vanilla+ now, enhanced stays enhanced
        assert_eq!(Settings::from_text("enhanced=0\n").graphics, "vanilla_plus");
        assert_eq!(Settings::from_text("enhanced=1\n").graphics, "enhanced");
        let v = Settings::from_text("graphics=vanilla\nshadows=1\nssao=1\n");
        assert!(v.classic() && !v.shadows && !v.ssao && !v.detail_textures && !v.enhanced);
        assert!(Settings::from_text("graphics=enhanced\nenhanced=0\n").enhanced);
        assert_eq!(graphics_mode("Vanilla+"), "vanilla_plus");
        assert_eq!(graphics_mode("OMSI 2"), "vanilla");
        let s = Settings { graphics: "enhanced".into(), enhanced: true, ..Default::default() };
        assert_eq!(Settings::from_text(&s.to_text()), s);
    }

    #[test]
    fn mirror_size_zero_disables_mirror_rendering() {
        assert_eq!(Settings::from_text("mirror_size=0\n").mirror_size, 0);
        assert_eq!(Settings::from_text("mirror_size=32\n").mirror_size, 64);
        let off = Settings { mirror_size: 0, ..Default::default() };
        assert_eq!(Settings::from_text(&off.to_text()).mirror_size, 0);
    }

    #[test]
    fn mirror_refresh_reads_omsi_realreflexions_and_round_trips() {
        assert_eq!(Settings::default().mirror_refresh, "full");
        assert_eq!(Settings::from_text("performance_realreflexions=none\n").mirror_refresh, "off");
        assert_eq!(Settings::from_text("performance_realreflexions=economy\n").mirror_refresh, "eco");
        assert_eq!(Settings::from_text("performance_realreflexions=full\n").mirror_refresh, "full");
        assert_eq!(Settings::from_text("mirror_refresh=nonsense\n").mirror_refresh, "full");
        for mode in ["off", "eco", "full"] {
            let s = Settings { mirror_refresh: mode.into(), ..Default::default() };
            assert_eq!(Settings::from_text(&s.to_text()).mirror_refresh, mode);
        }
    }

    #[test]
    fn steering_view_settings_round_trip_and_reject_invalid_values() {
        let settings = Settings { steer_look: true, steer_look_angle: 45.0, steer_look_response: 0.5, ..Default::default() };
        assert_eq!(Settings::from_text(&settings.to_text()), settings);
        let invalid = Settings::from_text("steer_look_angle=NaN\nsteer_look_response=NaN\n");
        assert_eq!(invalid.steer_look_angle, 30.0);
        assert_eq!(invalid.steer_look_response, 0.25);
        let clamped = Settings::from_text("steer_look_angle=999\nsteer_look_response=-1\n");
        assert_eq!(clamped.steer_look_angle, 60.0);
        assert_eq!(clamped.steer_look_response, 0.05);
    }

    #[test]
    fn vr_mirror_rates_round_trip_including_every_frame() {
        for rate in [-1.0, 0.0, 16.0, 60.0, 120.0, 240.0, 360.0] {
            let s = Settings { vr_mirror_rate: rate, ..Default::default() };
            assert_eq!(Settings::from_text(&s.to_text()).vr_mirror_rate, rate);
        }
        assert_eq!(Settings::from_text("vr_mirror_rate=NaN\n").vr_mirror_rate, 16.0);
        assert_eq!(Settings::from_text("vr_mirror_rate=999\n").vr_mirror_rate, 360.0);
    }

    #[test]
    fn interface_size_is_read_within_its_range() {
        assert_eq!(Settings::default().ui_scale, 1.0);
        assert_eq!(Settings::from_text("ui_scale=1.25\n").ui_scale, 1.25);
        assert_eq!(Settings::from_text("ui_scale=5\n").ui_scale, 2.0);
        assert_eq!(Settings::from_text("ui_scale=0\n").ui_scale, 0.5);
        assert_eq!(Settings::from_text("ui_scale=NaN\n").ui_scale, 1.0);
        let s = Settings { ui_scale: 1.5, ui_scale_window: false, ui_opacity: 0.4, notes: false, ..Default::default() };
        assert_eq!(Settings::from_text(&s.to_text()), s);
        // (the opacity under its old name, the navigator's)
        assert_eq!(Settings::from_text("navigator_opacity=0.5\n").ui_opacity, 0.5);
        assert_eq!(Settings::from_text("ui_opacity=7\n").ui_opacity, 1.0);
    }

    #[test]
    fn discord_settings_round_trip_and_default_enabled() {
        assert!(Settings::default().discord_status);
        let settings = Settings { discord_status: false, discord_app_id: "123456".into(), ..Default::default() };
        assert_eq!(Settings::from_text(&settings.to_text()), settings);
    }

    #[test]
    fn voice_chat_round_trips_and_is_on_by_default() {
        assert!(Settings::default().voice_chat);
        let s = Settings { voice_chat: false, ..Default::default() };
        assert_eq!(Settings::from_text(&s.to_text()), s);
    }

    #[test]
    fn post_aa_is_read_and_written() {
        assert_eq!(Settings::from_text("enhanced=1\n").post_aa, "fxaa");
        let off = Settings::from_text("enhanced=1\npost_aa=off\n");
        assert_eq!(off.post_aa, "off");
        assert!(!off.render_options().fxaa);
        assert!(Settings::from_text("post_aa=FXAA").render_options().fxaa);
        assert_eq!(Settings::from_text(&off.to_text()), off);
    }
}

impl Settings {
    /// The player's bus's `wearlifespan` for the maintenance condition (OMSI's table).
    pub fn wear_lifespan(&self) -> f32 {
        [1.5e6, 0.01, 0.1, 1.0, 10.0][self.maintenance.min(4) as usize]
    }
}

/// The player's own turn of a bus's mirrors (degrees yaw, pitch per `[add_camera_reflexion]`),
/// kept per `.bus` file in `~/.openomsi/mirrors.cfg` as `<bus file>|<mirror>=<yaw>,<pitch>`.
pub fn mirror_offsets(bus: &std::path::Path) -> Vec<[f32; 2]> {
    let key = bus.to_string_lossy().to_ascii_lowercase();
    let Some(p) = Settings::path().map(|p| p.with_file_name("mirrors.cfg")) else { return Vec::new() };
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let mut out: Vec<[f32; 2]> = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.rsplit_once('=') else { continue };
        let Some((file, i)) = k.rsplit_once('|') else { continue };
        let (Ok(i), Some((y, p))) = (i.trim().parse::<usize>(), v.split_once(',')) else { continue };
        if file.trim().to_ascii_lowercase() != key || i > 64 {
            continue;
        }
        if out.len() <= i {
            out.resize(i + 1, [0.0; 2]);
        }
        out[i] = [y.trim().parse().unwrap_or(0.0), p.trim().parse().unwrap_or(0.0)];
    }
    out
}

/// A bus's mirror shifts (bus frame, m: across, along, up), kept in the same file as the
/// turns on lines `<bus>|s<n>=x,y,z` (a build that knows only turns skips them).
pub fn mirror_shifts(bus: &std::path::Path) -> Vec<[f32; 3]> {
    let key = bus.to_string_lossy().to_ascii_lowercase();
    let Some(p) = Settings::path().map(|p| p.with_file_name("mirrors.cfg")) else { return Vec::new() };
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let mut out: Vec<[f32; 3]> = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.rsplit_once('=') else { continue };
        let Some((file, i)) = k.rsplit_once('|') else { continue };
        let Some(i) = i.trim().strip_prefix('s').and_then(|n| n.parse::<usize>().ok()) else { continue };
        let xyz: Vec<f32> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        if file.trim().to_ascii_lowercase() != key || i > 64 || xyz.len() != 3 {
            continue;
        }
        if out.len() <= i {
            out.resize(i + 1, [0.0; 3]);
        }
        out[i] = [xyz[0], xyz[1], xyz[2]];
    }
    out
}

/// A bus's mirror field-of-view changes (degrees added to the mirror camera's), on lines
/// `<bus>|f<n>=degrees` of the same file.
pub fn mirror_fovs(bus: &std::path::Path) -> Vec<f32> {
    let key = bus.to_string_lossy().to_ascii_lowercase();
    let Some(p) = Settings::path().map(|p| p.with_file_name("mirrors.cfg")) else { return Vec::new() };
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let mut out: Vec<f32> = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.rsplit_once('=') else { continue };
        let Some((file, i)) = k.rsplit_once('|') else { continue };
        let Some(i) = i.trim().strip_prefix('f').and_then(|n| n.parse::<usize>().ok()) else { continue };
        let Ok(d) = v.trim().parse::<f32>() else { continue };
        if file.trim().to_ascii_lowercase() != key || i > 64 {
            continue;
        }
        if out.len() <= i {
            out.resize(i + 1, 0.0);
        }
        out[i] = d.clamp(-60.0, 60.0);
    }
    out
}

/// Keep a bus's mirror turns, shifts and field-of-view changes (see [`mirror_offsets`],
/// [`mirror_shifts`], [`mirror_fovs`]).
pub fn save_mirror_state(bus: &std::path::Path, offsets: &[[f32; 2]], shifts: &[[f32; 3]], fovs: &[f32]) {
    let key = bus.to_string_lossy().to_ascii_lowercase();
    let Some(p) = Settings::path().map(|p| p.with_file_name("mirrors.cfg")) else { return };
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| l.rsplit_once('=').and_then(|(k, _)| k.rsplit_once('|')).is_none_or(|(f, _)| f.trim().to_ascii_lowercase() != key))
        .map(str::to_string)
        .collect();
    for (i, o) in offsets.iter().enumerate() {
        if o[0].abs() > 0.01 || o[1].abs() > 0.01 {
            lines.push(format!("{}|{i}={:.1},{:.1}", bus.to_string_lossy(), o[0], o[1]));
        }
    }
    for (i, s) in shifts.iter().enumerate() {
        if s.iter().any(|v| v.abs() > 0.002) {
            lines.push(format!("{}|s{i}={:.3},{:.3},{:.3}", bus.to_string_lossy(), s[0], s[1], s[2]));
        }
    }
    for (i, d) in fovs.iter().enumerate() {
        if d.abs() > 0.05 {
            lines.push(format!("{}|f{i}={:.1}", bus.to_string_lossy(), d));
        }
    }
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&p, lines.join("\n") + "\n");
}
/// `resolution=` (or `window_size=`) of a settings text, the last one written.
pub fn resolution_of(text: &str) -> Option<(u32, u32)> {
    let v = text.lines().filter_map(|l| l.split_once('=')).filter(|(k, _)| matches!(k.trim().to_ascii_lowercase().as_str(), "resolution" | "window_size")).last()?.1;
    let (w, h) = omsi_launcher_lib::resolution_text(v).split_once('x').map(|(w, h)| (w.parse().ok(), h.parse().ok()))?;
    Some((w?, h?))
}

#[cfg(test)]
mod resolution_tests {
    #[test]
    fn the_window_size_is_read_from_the_settings() {
        assert_eq!(super::resolution_of("msaa=4\nresolution=1280x800\n"), Some((1280, 800)));
        assert_eq!(super::resolution_of("resolution=auto\n"), None);
        assert_eq!(super::resolution_of("window_size=1920*1080\n"), Some((1920, 1080)));
        assert_eq!(super::resolution_of("vsync=1\n"), None);
    }
}
