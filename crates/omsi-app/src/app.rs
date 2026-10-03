//! The window's game: `App`, its state and its per-frame work.

use super::*;

const SLOW_UPLOAD_MB_S: f64 = 300.0;

pub(crate) struct App {
    pub(crate) args: Args,
    pub(crate) instance: wgpu::Instance,
    pub(crate) window: Option<Arc<Window>>,
    pub(crate) surface: Option<SurfaceState<'static>>,
    pub(crate) renderer: Option<Renderer>,
    #[cfg(windows)]
    pub(crate) vr: Option<crate::openxr::Vr>,
    pub(crate) scene: Option<Scene>,
    pub(crate) camera: Option<Camera>,
    pub(crate) player: Option<Player>,
    /// A situation's further vehicles and those placed from the game menu, standing.
    pub(crate) placed: Vec<Player>,
    /// The game menu's vehicle chooser is open, with this vehicle chosen (index into
    /// `vehicle_list`), and the vehicles it offers (name, path).
    pub(crate) chooser: Option<usize>,
    /// The object editor, while it is on (`crate::editor`).
    pub(crate) editor: Option<crate::editor::Editor>,
    pub(crate) vehicle_list: Vec<(String, String)>,
    /// The drop-down open over a row of the settings window, if one is.
    pub(crate) dropdown: Option<crate::game_lists::Dropdown>,
    /// (manufacturer, type) of each vehicle of `vehicle_list`, by its path.
    pub(crate) vehicle_meta: std::collections::HashMap<String, (String, String)>,
    pub(crate) world: Option<Arc<World>>,
    /// Tile streaming around the camera (the window's default).
    pub(crate) streamer: Option<tiles::Streamer>,
    /// The sim date and the season's texture folder the loaded world shows (see
    /// `follow_date`).
    pub(crate) world_day: Option<(i32, Option<String>)>,
    /// The map is open but the first area is still loading: the view to start with.
    pub(crate) starting: Option<Camera>,
    pub(crate) traffic: Option<traffic::Traffic>,
    pub(crate) schedule: Option<schedule::Schedule>,
    pub(crate) humans: Option<humans::Humans>,
    pub(crate) duty: Option<schedule::PlayerDuty>,
    /// The duty was told the places of the stops beyond the loaded tiles.
    pub(crate) duty_places: bool,
    pub(crate) hud: Option<hud::Hud>,
    /// The route navigator (ETS2-style map in a corner).
    pub(crate) navigator: Option<navigator::Navigator>,
    pub(crate) vr_nav_profiles: crate::vr_navigator::Profiles,
    pub(crate) vr_nav_edit: Option<crate::vr_navigator::Editing>,
    /// The window spans the triple screen's three monitors: fullscreen would shrink it to one.
    pub(crate) spanned: bool,
    /// Chat, mouse-over names and name tags (Roboto).
    pub(crate) ui: Option<ui::Ui>,
    pub(crate) fps: f32,
    pub(crate) rain: rain::Rain,
    /// Wheel splashes through the puddles `enhanced.wgsl` paints on wet roads.
    pub(crate) splashes: puddles::Splashes,
    pub(crate) lamps_on: Option<bool>,
    pub(crate) menu: Option<menu::Menu>,
    pub(crate) populate_t: f32,
    pub(crate) humans_populate_t: f32,
    /// The player's bus radio as internet radio.
    pub(crate) radio: radio::Radio,
    /// Per-stage frame time accumulators (OMSI_PROFILE), seconds.
    pub(crate) profile: std::collections::BTreeMap<&'static str, f64>,
    /// `profile` as it was at the start of the last frame: what a slow frame spent where.
    pub(crate) profile_prev: std::collections::BTreeMap<&'static str, f64>,
    pub(crate) first_populate: bool,
    pub(crate) envir: Option<omsi_content::Envir>,
    pub(crate) weather: Option<omsi_content::weather::Weather>,
    pub(crate) clock: omsi_sim::SimClock,
    pub(crate) started: Instant,
    pub(crate) total_frames: u32,
    /// Mirror pictures due (see `MIRROR_RATE`), and which mirror is next.
    pub(crate) mirror_budget: f32,
    pub(crate) mirrors_seen: usize,
    pub(crate) mirror_turn: usize,
    /// With no real-time reflections: the bus whose mirrors are frozen (see
    /// `MIRROR_FREEZE_REDRAW`).
    pub(crate) frozen_mirrors: Option<FrozenMirrors>,
    /// The mirror panels laid over the picture (see `mirror_hud`).
    pub(crate) mirror_hud: crate::mirror_hud::MirrorHud,
    /// Cursor and view the hover was last worked out for (see the redraw).
    pub(crate) hover_key: Option<(i32, i32, i32, i32)>,
    pub(crate) view: String,
    pub(crate) audio: Option<omsi_audio::AudioEngine>,
    /// Sounds of the world around the camera (rain, footsteps).
    pub(crate) ambience: Option<ambience::Ambience>,
    pub(crate) cursor: (f32, f32),
    /// Last Windows mouse position used for the unbounded VR cockpit pointer.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) vr_cursor_physical: Option<(f32, f32)>,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) vr_cursor_warp_pending: Option<(f32, f32)>,
    pub(crate) window_focused: bool,
    /// The window lost the focus or was minimised or hidden: the keyboard and the mouse
    /// work nothing until it has the focus again (`App::input_lost` / `input_back`).
    pub(crate) input_away: bool,
    /// The window is minimised or out of sight, as its events last said.
    pub(crate) window_hidden: bool,
    pub(crate) keys: hashbrown::HashSet<KeyCode>,
    /// Door trigger groups currently held by the Shift+number shortcut. Keeping the
    /// release until physical key-up prevents latched button states and door chatter.
    pub(crate) door_key_triggers: hashbrown::HashMap<KeyCode, Vec<String>>,
    pub(crate) last: Instant,
    pub(crate) speed: f32,
    pub(crate) mouse_look: bool,
    /// The left and right mouse buttons held.
    pub(crate) buttons_held: (bool, bool),
    /// The right button (or both) held: OMSI's mouse zoom (0x82c5f8) - moving the mouse up
    /// widens the view in the bus or takes the outside camera further away, by the value at
    /// the press over 500 pixels: (the cursor's height then, the zoom or distance then).
    pub(crate) both_drag: Option<(f32, f32)>,
    /// Right mouse button toggles the headset picture zoom.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) vr_zoom_active: bool,
    /// The cockpit switch the cursor is over, shown in the HUD.
    pub(crate) hover: Option<String>,
    /// The part under the cursor when it is not a switch, so the HUD can say so.
    pub(crate) hover_part: Option<String>,
    /// A `[mouseevent]` mesh is under the cursor (named in `hover` or not): the hand cursor.
    pub(crate) hover_hand: bool,
    /// The idle head sway waiting where it is while the cursor is on a control
    /// (see `head_idle::Hold`).
    pub(crate) head_idle_hold: crate::head_idle::Hold,
    /// `OMSI_INPUT` script: (seconds after start, command), in order.
    pub(crate) input_script: Vec<(f32, String)>,
    /// `shot <file>` of the input script: the next frame is also rendered into this PNG.
    pub(crate) shot: Option<PathBuf>,
    /// The simulation stands still (OMSI's `sim_pause`, P, or the menu): nothing moves,
    /// the clock stops, the picture and the camera go on.
    pub(crate) paused: bool,
    /// The game menu (Escape, OMSI's `open_mainmenue`): the chosen line of it.
    pub(crate) game_menu: Option<usize>,
    /// The first line of the game menu (or chooser) shown, when a finger has scrolled it
    /// (in lines, fractional while dragged); `None`: the chosen line is kept in view.
    pub(crate) menu_top: Option<f32>,
    pub(crate) menu_scroll_drag: bool,
    /// The scroll bar of an open drop-down held with the mouse: where on its thumb it was
    /// taken (pixels from the thumb's top).
    pub(crate) dd_scroll_drag: Option<f32>,
    /// The same for the scroll bar of the timetable beside a line's tours.
    pub(crate) pane_scroll_drag: Option<f32>,
    /// The timetable beside the tours scrolled with the wheel: (the tour's line in the list,
    /// the first stop shown).
    pub(crate) pane_scroll: Option<(usize, usize)>,
    /// The digits of a time being typed in the world page of the game menu (None: not typing).
    pub(crate) menu_edit: Option<String>,
    pub(crate) menu_edit_icao: bool,
    /// The vehicle being chosen in "Place a vehicle" takes the place of the one driven
    /// (the game menu's "Swap for another vehicle", #728).
    pub(crate) swap_pending: bool,
    /// The line of the open list whose slider the mouse button holds (it follows the cursor).
    pub(crate) menu_drag: Option<usize>,
    /// The keyboard chose the line of the menu last (the mouse moved since: false), so the
    /// chosen line is shown lit; with the mouse only the line under it is.
    pub(crate) menu_kbd: bool,
    /// Keys pressed (true) and let go since the Lua plugins' last frame.
    pub(crate) plugin_keys: Vec<(String, bool)>,
    /// Seconds Ctrl+Shift+Page Up/Down has been held (the clock runs faster the longer).
    pub(crate) clock_hold: f32,
    /// A controller button held for looking left, right, up, down (`view_look_*`).
    pub(crate) pad_look: [bool; 4],
    /// The arrow keys turned the head (a glance that comes back when they are let go).
    pub(crate) arrow_glance: bool,
    /// The next click on the city map puts the bus there (Esc → Move the bus on the map).
    pub(crate) teleport_pick: bool,
    /// Discord's "Playing openOMSI" status, and when it was last brought up to date.
    pub(crate) discord: Option<crate::discord::Discord>,
    pub(crate) discord_t: f32,
    // Steamworks API layer and it's last updated time
    #[cfg(steam)]
    pub(crate) steam: Option<crate::steam::Steam>,
    /// Positional voice through GreenTeaSpeak in a session (`voice`).
    pub(crate) voice: Option<crate::voice::Voice>,
    /// Head tracking (Settings → head tracking), started with the first frame that wants it.
    pub(crate) headtrack: Option<crate::headtrack::HeadTracker>,
    /// When head tracking last failed to start (tried again a few seconds later).
    pub(crate) headtrack_failed: Option<std::time::Instant>,
    /// Steering wheels, pedals, joysticks and gamepads (`Inputs/gamectrler.cfg`).
    pub(crate) controllers: Option<crate::controllers::Controllers>,
    /// OMSI's mouse control (`toggel_mouse_ctrl`, O): the cursor's place steers (across) and
    /// works the pedals (up throttle, down brake).
    pub(crate) mouse_drive: bool,
    /// Mouse steering: the steering it gives (fraction of the full lock) and how long (s)
    /// it still eases in after being switched on (OMSI: a second, see app_events).
    pub(crate) mouse_steer: (f32, f32),
    /// Mouse steering past the window's edge: the lock the mouse added while the cursor stood
    /// pinned at the left or right edge (-1..1 of full lock). OMSI divides the width by the
    /// speed, and at 30 km/h the edge of the screen was a third of the lock, with nowhere
    /// further to move.
    pub(crate) mouse_edge: f32,
    /// Where the cursor steered when the right button began to look round: it goes back
    /// there when the button is let go, so the wheel does not jump to where looking left it.
    pub(crate) steer_cursor: Option<(f32, f32)>,
    /// The cursor is put in the middle of the window before the mouse steers for the first
    /// time (a game started with the mouse steering on: wherever the cursor was, the wheel
    /// turned and the bus drove off on full throttle).
    pub(crate) center_cursor: bool,
    /// The cursor hidden while a controller drives: where it stood.
    pub(crate) cursor_hidden: Option<(f32, f32)>,
    /// The wheel's place when it last counted as moved.
    pub(crate) last_ctl_steer: Option<f32>,
    /// The mouse's throttle and brake (eased in with the steering).
    pub(crate) mouse_pedals: (f32, f32),
    /// The speed mouse steering divides by, smoothed.
    pub(crate) mouse_kmh: f32,
    /// The tutorial being run (`--tutorial`), loaded on the first frame.
    pub(crate) tutorial: Option<crate::tutorial::Tutorial>,
    /// OMSI's pedestrian ("ego") view: the free camera walking at eye height on whatever
    /// people stand on (`view_set_ego`, F11).
    pub(crate) ego: bool,
    /// The player out of the seat, walking about (`on_foot`).
    pub(crate) on_foot: Option<crate::on_foot::OnFoot>,
    /// Other players on foot whose avatars are drawn (their ids).
    pub(crate) remote_walkers: Vec<u32>,
    /// The camera is in the own bus's cab this frame (see RedrawRequested).
    pub(crate) in_cab: bool,
    /// The player on foot is in this other player's bus (see `lan`: drawn from inside).
    pub(crate) inside_remote: Option<u32>,
    /// A dedicated server said we administer it (`admin`).
    pub(crate) is_admin: bool,
    /// Where the bus last stood on the ground (and facing where): it is put back there when
    /// it falls through the world (see `admin::guard_fall`).
    pub(crate) safe_pose: Option<(glam::DVec3, f64)>,
    /// Seconds since `safe_pose` was taken.
    pub(crate) safe_age: f32,
    /// The mouse wheel over the menu, notches not yet turned into lines.
    pub(crate) wheel_acc: f32,
    /// The object editor: an object dragged with the mouse; seconds to the next resend of
    /// all edits to the others (LAN host); the copies the host made, as this client shows them.
    pub(crate) editor_drag: bool,
    pub(crate) editor_sync_t: f32,
    pub(crate) remote_added: std::collections::HashMap<i64, crate::scene::TileGpu>,
    /// Placing a vehicle with the mouse (the spawner): see `placing`.
    pub(crate) placing: Option<crate::placing::Placing>,
    /// The chooser shows the administration's lines (label, action) instead of vehicles.
    pub(crate) admin_list: Option<Vec<(String, String)>>,
    /// Which of the game menu's lists `admin_list` holds (see `game_lists`).
    pub(crate) list_kind: Option<crate::game_lists::ListKind>,
    /// OMSI 2's route arrows over the road (the `nav_arrows` setting).
    pub(crate) route_arrows: crate::route_arrows::RouteArrows,
    /// OMSI's global key actions from `Inputs/keyboard.cfg` ([game]).
    pub(crate) game_keys: Vec<omsi_content::KeyBinding>,
    /// Keys (DirectInput scan codes, no modifier) the player bound on the Controls page to
    /// something the original's keyboard.cfg does not have there: a driving preset (W A S D,
    /// the arrows) leaves them alone - D bound to the gearbox is the gearbox, not "steer right".
    pub(crate) own_keys: std::collections::HashSet<i32>,
    /// The same for keys held with Shift (a Shift+number of the player's own is not a door key).
    pub(crate) own_shift: std::collections::HashSet<i32>,
    /// Whether the game stood paused before the menu opened (closing it goes back to that).
    pub(crate) menu_prev_pause: bool,
    /// OMSI's information bar (`view_toggle_informationdisplay`, Ctrl+Y): time, speed, the
    /// air and cabin temperatures, the passengers aboard, the trip and its next stop along
    /// the top of the picture.
    pub(crate) info_bar: bool,
    /// A time of day the bus's script wrote (`(S.S.Time)`), for the clock at the next frame.
    pub(crate) pending_time: Option<f64>,
    /// The play time (`clock.run_time`) the last situation was saved at.
    pub(crate) autosave_t: f64,
    /// OMSI's timetable window (`view_set_schedule`, Insert).
    pub(crate) timetable: bool,
    /// The left button is held on a switch: mouse movement turns it.
    pub(crate) dragging: bool,
    /// The left button is held on a page of the bus (an `[htmltexture]`): its script texture
    /// index and the place on it the pointer was last seen.
    pub(crate) html_pressed: Option<(usize, f32, f32)>,
    /// The same for a page of a scenery object: its map id, script texture index and place.
    pub(crate) html_object_pressed: Option<(i64, usize, f32, f32)>,
    /// Cursor movement (logical pixels) while dragging a switch, not yet handed to the
    /// script: `<event>_drag` fires once a frame with it (see `Player::drag`).
    pub(crate) drag_delta: (f32, f32),
    /// How far the player has turned the head (driver, passenger) or swung the outside
    /// camera around the bus, and how far that camera sits from it.
    pub(crate) look: (f32, f32),
    /// Each view keeps its own `look` (as OMSI's cameras do): turning the outside camera
    /// (F3) leaves the driver's head (F1) where it was. `look_view` is the view `look`
    /// belongs to now; see `App::sync_view_look`.
    pub(crate) view_looks: std::collections::HashMap<String, (f32, f32)>,
    pub(crate) look_view: String,
    /// Smooth switch between two cockpit cameras (arrow keys), see `CamBlend`.
    pub(crate) cam_blend: CamBlend,
    /// The zoom of the views inside the bus (driver, passenger): their field of view is
    /// the camera's times this (the mouse wheel, + and -, a pinch), per view.
    pub(crate) view_zoom: std::collections::HashMap<String, f32>,
    pub(crate) orbit: f32,
    pub(crate) frames: u32,
    pub(crate) fps_t: Instant,
    /// Last workshop / fuel pump / wash message, and how long it still shows.
    pub(crate) service_msg: Option<(String, f32)>,
    /// The server's notifications on the screen (`notify`), oldest first.
    pub(crate) notices: Vec<crate::ui::Notice>,
    /// What the log has said (see applog.rs).
    pub(crate) log_state: crate::applog::LogState,
    /// The driver's personnel file and this session's statistics.
    pub(crate) career: career::Career,
    /// How wet the roads are (0..1), built up by rain and dried by the sun.
    pub(crate) wetness: f32,
    /// How far the cloud cover has drifted with the wind (fractions of its tiling), summed
    /// up frame by frame so that a change of wind does not throw the sky around.
    pub(crate) cloud_drift: [f32; 2],
    /// A change of weather coming in (see `weather_cycle`).
    pub(crate) weather_blend: Option<crate::weather_cycle::Blend>,
    /// The weather cycle, when the weather chosen is `cycle`.
    pub(crate) weather_cycle: Option<crate::weather_cycle::Cycle>,
    /// The METAR sync's download under way (see `tick_metar`), and the seconds to the next one.
    pub(crate) metar_rx: Option<std::sync::mpsc::Receiver<Option<omsi_content::weather::Weather>>>,
    /// The current METAR receiver is a single manual fetch rather than the continuous sync.
    pub(crate) metar_once: bool,
    pub(crate) metar_next: f64,
    /// The mouse cursor currently shows the hand (it is over a switch).
    pub(crate) cursor_kind: u8,
    pub(crate) settings: settings::Settings,
    /// LAN session, and the other players' buses (drawn and heard like AI vehicles) with the
    /// chat line.
    pub(crate) lan: Option<omsi_net::LanSession>,
    pub(crate) remotes: lan::LanGame,
    /// Frames longer than 50 ms (stutters) and the worst frame, for the exit summary.
    pub(crate) spikes: u32,
    pub(crate) worst_ms: f32,
    /// The frame-rate governor's two-second window.
    /// Window seconds, frames, and time waiting on presentation/GPU in that window.
    pub(crate) governor: (f32, u32, f32),
    /// Readings in a row at the smallest render scale still waiting for the card.
    pub(crate) governor_low: u32,
    /// Cumulative presentation wait at the previous frame, independent of OMSI_PROFILE.
    pub(crate) governor_wait_prev: f64,
    /// Frames the window was hidden for (they are not drawn) and whether the exit is under way.
    pub(crate) hidden_frames: u32,
    pub(crate) exiting: bool,
    /// Stand-in for the window's frame while the window is hidden (OMSI_RENDER_OCCLUDED).
    pub(crate) stand_in: Option<wgpu::Texture>,
    /// OMSI_PROFILE: process CPU seconds, time and frame count once the start-up is over,
    /// for the CPU time a frame costs (the wall time says little on a busy machine).
    pub(crate) cpu_mark: Option<(f64, Instant, u32)>,
    /// The OMSI plugins (`plugins/*.opl`), loaded with the first frame.
    pub(crate) plugins: Option<omsi_plugin::Plugins>,
    /// The on-screen controls of a phone (see `touch.rs`).
    pub(crate) touch: crate::touch::Touch,
}

impl App {
    #[cfg(windows)]
    pub(crate) fn vr_active(&self) -> bool { self.vr.is_some() }

    #[cfg(not(windows))]
    pub(crate) fn vr_active(&self) -> bool { false }

    pub(crate) fn resumed_impl(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(window) = self.window.clone() {
            // back from the background (a phone): the window's surface is made again
            if self.surface.is_none() {
                if let Some(r) = self.renderer.as_ref() {
                    let size = window.inner_size();
                    let vsync = self.settings.vsync && !self.vr_active();
                    self.surface = SurfaceState::new_with(&self.instance, window.clone(), r, size.width.max(1), size.height.max(1), vsync).ok();
                    self.last = Instant::now();
                }
            }
            return;
        }
        self.create_window(event_loop, None);
    }

    /// The game's window (or the launcher's, handed over on a phone), its surface and the
    /// renderer; then the menu or, when the session is given, the world.
    pub(crate) fn create_window(&mut self, event_loop: &ActiveEventLoop, given: Option<Arc<Window>>) {
        // Steam's rich presence starts before the game window (see `steam.rs`)
        #[cfg(steam)]
        if self.steam.is_none() {
            self.steam = crate::steam::Steam::start();
        }

        // --size sets the window's size in points as well (1600x900 unless given)
        let (lw, lh) = self
            .args
            .size
            .split_once('x')
            .map(|(a, b)| {
                (
                    a.parse::<u32>().unwrap_or(1600),
                    b.parse::<u32>().unwrap_or(900),
                )
            })
            .unwrap_or((1600, 900));
        let (fit, at) = crate::startup::fit_window(event_loop, lw as f64, lh as f64);
        let mut attrs = Window::default_attributes()
            .with_title("openOMSI")
            .with_inner_size(fit)
            .with_window_icon(crate::startup::window_icon());
        if let Some(at) = at {
            attrs = attrs.with_position(at);
        }
        // the window size of the settings (pixels, #904) unless --size names one
        let resolution = crate::settings::Settings::resolution().filter(|_| self.args.size == crate::cli::DEFAULT_SIZE);
        if let Some((w, h)) = resolution {
            attrs = attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h));
            if let Some(m) = event_loop.primary_monitor().or_else(|| event_loop.available_monitors().next()) {
                let (sw, sh) = (m.size().width as i32, m.size().height as i32);
                attrs = attrs.with_position(winit::dpi::PhysicalPosition::new(m.position().x + ((sw - w as i32) / 2).max(0), m.position().y + ((sh - h as i32) * 2 / 5).max(0)));
            }
        }
        // (a Steam Deck's Gaming Mode: gamescope shows one window over the whole screen and
        // scales whatever size it has to it - a window fitted to 90 % of the screen came out
        // blurred and letterboxed. There the window is the screen's, unless a size is set)
        let gamescope = resolution.is_none() && crate::startup::under_gamescope();
        if gamescope {
            log::info!("gamescope (Steam Deck Gaming Mode): the window fills the screen");
        }
        if self.settings.fullscreen || gamescope {
            attrs = attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        }
        if self.settings.triple.enabled
            && self.settings.triple_span
            && !self.settings.vr_requested()
        {
            let mut monitors: Vec<_> = event_loop.available_monitors().collect();
            monitors.sort_by_key(|m| m.position().x);
            let row = monitors.windows(3).find(|row| {
                let size = row[0].size();
                row.iter()
                    .all(|m| m.size() == size && m.position().y == row[0].position().y)
                    && row[1].position().x == row[0].position().x + size.width as i32
                    && row[2].position().x == row[1].position().x + size.width as i32
            });
            if let Some(row) = row {
                let size = row[0].size();
                if self.settings.fullscreen || gamescope || resolution.is_some() || self.args.size != crate::cli::DEFAULT_SIZE {
                    log::info!("triple screen: spanning three monitors instead of the fullscreen / window size settings");
                }
                self.spanned = true;
                attrs = attrs
                    .with_fullscreen(None)
                    .with_decorations(false)
                    .with_position(row[0].position())
                    .with_inner_size(winit::dpi::PhysicalSize::new(size.width * 3, size.height));
                log::info!("triple screen: spanning {}x{}", size.width * 3, size.height);
            } else {
                log::warn!("triple screen: no equal horizontal monitor row found; using configured window size (Surround/Eyefinity can expose one wide display)");
            }
        }
        // OMSI_BACKGROUND=1: a test window that does not take the keyboard from whoever is
        // working at the screen (OMSI_INPUT drives the handlers directly, it needs no focus)
        if omsi_cfg::env::var_os("OMSI_BACKGROUND").is_some() {
            attrs = attrs.with_active(false);
        }
        let window = match given {
            Some(w) => w,
            // (no display to open it on, a compositor that refuses it: said so, not a panic
            // report about "window")
            None => match event_loop.create_window(attrs) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    fatal_message(&format!("The game cannot open its window: {e}"));
                    crate::platform::exit(event_loop);
                    return;
                }
            },
        };
        let mut renderer = match window_renderer(&mut self.instance, &window, self.settings.render_options()) {
            Ok(r) => r,
            Err(e) => {
                fatal_message(&format!("The game cannot draw on this computer: {e:#}"));
                crate::platform::exit(event_loop);
                return;
            }
        };
        #[cfg(windows)]
        if self.settings.vr_requested() {
            match crate::openxr::Vr::new(&renderer, self.settings.vr_scale, self.settings.vr_desktop_mirror) {
                Ok(vr) => self.vr = Some(vr),
                Err(e) => log::error!("OpenXR could not start: {e:#}"),
            }
        }
        let upload = renderer.upload_speed_mb_s();
        log::info!("graphics: {upload:.0} MB/s copied towards the card");
        if upload < SLOW_UPLOAD_MB_S {
            log::error!("graphics: the driver copies only {upload:.0} MB/s towards the card (thousands are usual); every texture and buffer the game sends waits on it, down to a few frames a second - restarting the computer usually brings it back");
            self.service_msg = Some((format!("Graphics driver is slow ({upload:.0} MB/s): the game will stutter. Restarting the computer usually fixes it."), 30.0));
        }
        crate::lights::load_smoke_texture(&mut renderer, &self.args.root);
        crate::lights::set_corona_root(&self.args.root);
        let size = window.inner_size();
        let surface = match SurfaceState::new_with(
            &self.instance,
            window.clone(),
            &renderer,
            size.width,
            size.height,
            self.settings.vsync && !self.vr_active(),
        ) {
            Ok(s) => s,
            Err(e) => {
                fatal_message(&format!("The game's window cannot be drawn into: {e:#}"));
                crate::platform::exit(event_loop);
                return;
            }
        };
        let (sw, sh) = renderer.scene_size(size.width, size.height);
        log::info!(
            "window: {}x{} pixels (scale factor {:.2}), 3D picture {sw}x{sh}, present mode {:?}",
            size.width,
            size.height,
            window.scale_factor(),
            surface.config.present_mode
        );
        let scene = renderer.new_scene();
        self.window = Some(window);
        self.surface = Some(surface);
        self.renderer = Some(renderer);
        self.scene = Some(scene);
        // fully specified runs skip the menu
        if self.args.bus.is_some() || self.args.cam.is_some() || self.args.no_menu {
            self.load_world_now(event_loop);
        } else {
            let mut fonts = omsi_sim::texttex::FontLibrary::new(&self.args.root);
            self.hud = Some(hud::Hud::new(&mut fonts));
            self.menu = Some(menu::Menu::new(&self.args.root, &self.args.map));
        }
    }

    /// Load the map, vehicle and traffic according to `args`.
    pub(crate) fn load_world_now(&mut self, event_loop: &ActiveEventLoop) {
        // a joining player loads the host's date, time, weather and season (after the menu)
        if let Some(l) = self.lan.as_mut() {
            lan::adopt_host_world(&mut self.args, l, &mut self.remotes);
        }
        let renderer = self.renderer.take().expect("renderer");
        let mut scene = renderer.new_scene();
        self.envir = omsi_content::Envir::load(&self.args.root.join("envir.cfg")).ok();
        // the weather cycle: a first weather that suits the month, the others after it
        if crate::weather_cycle::is_cycle(self.args.weather.as_deref()) {
            let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(7);
            let mut c = crate::weather_cycle::Cycle::new(seed);
            let month = start_clock(&self.args).day_month().1;
            let all = crate::weather_cycle::installed();
            let clear = omsi_content::weather::Weather { fog: (50000.0, 1.0), ..Default::default() };
            let r = c.rand();
            self.args.weather = crate::weather_cycle::pick(&all, &clear, "", month, r);
            log::info!("weather cycle: starting with {:?}", self.args.weather);
            self.weather_cycle = Some(c);
        }
        self.weather = Some(load_weather(&self.args));
        // the roads start in the state this weather has already left them in, as they do
        // offscreen: a session begun in the rain used to open on a bone-dry street
        self.wetness = self.weather.as_ref().map(initial_wetness).unwrap_or(0.0);
        self.clock = start_clock(&self.args);
        setup_sky(&self.args, &renderer, &mut scene, self.envir.as_ref(), self.weather.as_ref());
        // the window streams the tiles around the camera unless a fixed area was asked for
        if !self.args.all && self.args.radius.is_none() {
            match open_world(&self.args) {
                Ok((w, cam, _)) => {
                    let w = Arc::new(w);
                    let distance = self
                        .args
                        .view_distance
                        .or_else(settings::view_distance)
                        .unwrap_or(1200.0)
                        .max(omsi_map::tile_size());
                    w.set_fast_texture_loads(true);
                    w.set_texture_budget(texture_budget(&self.settings));
                    log::info!(
                        "texture budget: {:.0} MB",
                        texture_budget(&self.settings) as f64 / 1e6
                    );
                    self.streamer = Some(tiles::Streamer::new(
                        w.clone(),
                        &start_centers(&self.args, &cam, Some(&w)),
                        distance,
                        700.0,
                    ));
                    self.world = Some(w);
                    self.starting = Some(cam);
                }
                Err(e) => {
                    log::error!("{e:#}");
                    crate::platform::exit(event_loop);
                }
            }
            self.renderer = Some(renderer);
            self.scene = Some(scene);
            self.last = Instant::now();
            return;
        }
        match load_world(&self.args, &renderer, &mut scene) {
            Ok((w, cam)) => self.start_world(Arc::new(w), cam, &renderer, &mut scene),
            Err(e) => {
                log::error!("{e:#}");
                crate::platform::exit(event_loop);
            }
        }
        self.renderer = Some(renderer);
        self.scene = Some(scene);
        self.last = Instant::now();
    }

    /// Everything that needs the map to stand: the player's bus, the passengers, the traffic
    /// and the timetable.
    pub(crate) fn start_world(
        &mut self,
        w: Arc<World>,
        cam: Camera,
        renderer: &Renderer,
        mut scene: &mut Scene,
    ) {
        report_missing_content(&w, &mut self.service_msg);
        {
            {
                // (once more when it fails: a file read while the start was still reading
                // others; a failure is said on the screen - the game went on without a bus
                // and the player found himself on foot, with no word why)
                let first = spawn_player(&self.args, &w, &renderer, &mut scene);
                let spawned = match first {
                    Err(e) if self.args.bus.is_some() => {
                        log::warn!("the bus could not be put down ({e:#}); trying again");
                        spawn_player(&self.args, &w, &renderer, &mut scene).map_err(|e2| {
                            self.service_msg = Some((format!("The bus could not be loaded: {}", format!("{e2:#}").lines().next().unwrap_or_default()), 15.0));
                            e2
                        })
                    }
                    other => other,
                };
                match spawned {
                    Ok(mut p) => {
                        let audio = omsi_audio::AudioEngine::new();
                        if let Some(p) = p.as_mut() {
                            p.vehicle.host.auto_clutch = if self.settings.auto_clutch { 1.0 } else { 0.0 };
                            p.load_sounds(&audio);
                            p.ibis_background = true;
                            // --autostart applies in the window too, not only offscreen
                            if self.args.autostart && !self.args.is_resuming() {
                                let msg = p.start_up();
                                self.service_msg = Some((msg, 6.0));
                            }
                            // a pack this bus borrows parts from is not installed: say so
                            // once, it explains dark displays and missing devices
                            if !p.vehicle.ty.missing_packs.is_empty() {
                                let packs: Vec<String> = p.vehicle.ty.missing_packs.iter().map(|(n, _)| n.clone()).collect();
                                let msg = format!(
                                    "This bus takes parts from vehicle pack(s) that are not installed: {} (install them for its displays and devices)",
                                    packs.join(", ")
                                );
                                self.service_msg = Some(match self.service_msg.take() {
                                    Some((m, _)) => (format!("{m}   |   {msg}"), 12.0),
                                    None => (msg, 12.0),
                                });
                            }
                        }
                        self.ambience = Some(ambience::Ambience::load(&audio, &self.args.root));
                        self.audio = Some(audio);
                        if let Some(p) = &p {
                            if self.args.cam.is_none() && self.args.view != "free" {
                                self.camera = Some(p.camera(&self.args.view, &cam));
                            }
                        }
                        self.player = p;
                    }
                    Err(e) => log::error!("{e:#}"),
                }
                // a situation's further vehicles, each as it was saved
                for o in self.args.situation_others.clone() {
                    let one = Args {
                        bus: Some(o.bus.clone()),
                        spawn: Some(o.spawn.clone()),
                        hof: o.hof.clone(),
                        paint: o.paint.clone(),
                        situation_vars: o.vars.clone(),
                        situation_strvars: o.strvars.clone(),
                        situation_others: Vec::new(),
                        line: None,
                        tour: None,
                        trip: None,
                        autostart: false,
                        situation_next_stop: None,
                        ..self.args.clone()
                    };
                    match spawn_player(&one, &w, &renderer, &mut scene) {
                        Ok(Some(q)) => {
                            log::info!("situation: {} placed at {}", o.bus, o.spawn);
                            self.placed.push(q);
                        }
                        Ok(None) => {}
                        Err(e) => log::warn!("situation vehicle {}: {e:#}", o.bus),
                    }
                }
                if self.camera.is_none() {
                    self.camera = Some(cam);
                }
                self.navigator = Some(navigator::Navigator::new(
                    self.settings.navigator,
                    self.settings.ui_opacity,
                    &self.settings.navigator_corner,
                ));
                if let Some(n) = self.navigator.as_mut() {
                    n.arrows = self.settings.nav_arrows;
                    n.show_ai = self.settings.nav_ai;
                }
                if let Some(d) = self.args.driver.as_deref() {
                    self.career = career::Career::load(&self.args.root, d);
                }
                // (and a player who joins another's game sees the host's people)
                if self.args.passengers || self.args.lan_join.is_some() {
                    let mut h = humans::Humans::new(&self.args.root);
                    if let Some(lan) = self.lan.as_ref() {
                        h.set_lan_seed(lan::population_seed(lan));
                    }
                    h.exact_fare = self.settings.exact_fare;
                    h.boarding = self.settings.boarding.clone();
                    h.voices = match self.settings.pax_voices.as_str() { "off" => 2, "tickets" => 1, _ => 0 };
                    if let Some(p) = self.player.as_mut() {
                        h.set_cabin(&mut p.vehicle);
                        h.ticket_key = ticket_key_name(&self.args.root, &p.bindings);
                        h.tickets = p.vehicle.host.tickets.clone();
                        if !w.global.money_system.trim().is_empty() {
                            h.money =
                                Some(money::Money::new(&self.args.root, &w.global.money_system));
                        }
                    }
                    if let Some(p) = self.player.as_ref() {
                        if self.args.riders > 0 {
                            let centre = p.vehicle.position;
                            h.populate(&w, &renderer, &mut scene, centre);
                            h.seed_riders(self.args.riders, &p.vehicle, &w, &renderer, &mut scene);
                        }
                    }
                    self.humans = Some(h);
                }
                // (a player who joins draws the host's traffic in it, whatever their own count
                // says: the host's cars had nowhere to go without it)
                let populated = self.args.traffic > 0 || self.args.schedule || crate::rail_drive::args_rail(&self.args) || self.args.lan_join.is_some();
                // Without traffic it still runs the light programs and switches the lamps:
                // they stood frozen with red, yellow and green all lit (#727).
                {
                    match traffic::Traffic::new(&self.args.root, &w, self.args.traffic) {
                        Ok(mut t) => {
                            t.lights_only = !populated;
                            if let Some(lan) = self.lan.as_ref() {
                                t.set_lan_seed(lan::population_seed(lan));
                            }
                            if self.args.traffic > 0 {
                                t.precache_random(&w, &renderer, &mut scene);
                            }
                            t.day_time = parse_time(&self.args.time);
                            self.traffic = Some(t);
                        }
                        Err(e) => log::error!("traffic: {e:#}"),
                    }
                    if self.args.schedule {
                        let mut sch =
                            schedule::Schedule::new(&self.args.root, &w, &start_clock(&self.args));
                        sch.precache(
                            &w,
                            &renderer,
                            &mut scene,
                            self.traffic.as_mut(),
                            parse_time(&self.args.time),
                        );
                        if let (Some(line), Some(p)) = (&self.args.line, self.player.as_mut()) {
                            self.duty = match sch.player_duty(
                                &w,
                                line,
                                self.args.tour.as_deref().unwrap_or(""),
                                parse_time(&self.args.time),
                                self.args.trip.as_deref(),
                                self.args.whole_tour,
                            ) {
                                Ok(mut d) => {
                                    if let Some(k) = self.args.duty_trip {
                                        d.start_at(k, self.args.duty_first_stop);
                                    }
                                    if self.args.is_resuming() {
                                        d.resume(&mut p.vehicle, parse_time(&self.args.time), self.args.situation_next_stop);
                                        // the IBIS keeps its saved trip; with --autostart
                                        // the duty's next trips are typed into it again
                                        p.duty_typed = self.args.autostart;
                                    }
                                    Some(d)
                                }
                                Err(e) => {
                                    log::warn!("no player duty: {e}");
                                    self.service_msg = Some((format!("No duty: {e}"), 20.0));
                                    None
                                }
                            };
                            // --autostart in the window puts the duty on the IBIS as well
                            // (it only ever did offscreen: the duty did not exist yet when
                            // the start-up began, and the displays stayed dark)
                            if let (true, Some(d)) = (
                                self.args.autostart && !self.args.is_resuming(),
                                self.duty.as_mut(),
                            ) {
                                d.update(&mut p.vehicle, parse_time(&self.args.time));
                                let (trip, stop) = d.trip_for_ibis();
                                p.set_duty_destination(trip, stop);
                            }
                            if let Some(d) = self.duty.as_ref() {
                                let mut fonts = w.fonts.lock();
                                if let Err(e) = crate::schedule_paper::update_vehicle(
                                    &mut p.vehicle,
                                    d,
                                    &mut fonts,
                                ) {
                                    log::warn!("driver timetable paper: {e:#}");
                                }
                            }
                        }
                        self.schedule = Some(sch);
                    }
                }
                if self.traffic.is_none() {
                    if let Some(n) = self.navigator.as_mut() {
                        n.add_lanes(std::mem::take(&mut *w.lanes.lock()));
                    }
                }
                if let (Some(lan), Some(p)) = (self.lan.as_mut(), self.player.as_mut()) {
                    lan::settle_spawn(
                        lan,
                        &mut self.remotes,
                        p,
                        &self.args,
                        &w,
                        self.traffic.as_ref().map(|t| &t.net),
                        // (still loading: a moment's wait for the host's list puts the bus
                        // where it is free at once - without it the bus stood inside another
                        // player's for the first frames and then jumped out of it)
                        std::time::Duration::from_millis(1500),
                    );
                }
                self.world = Some(w);
            }
        }
        self.last = Instant::now();
    }

    /// The first area of a streamed map is loading: show how far it got, and start the rest
    /// of the world once it is there. Returns false while the loading screen is up.
    pub(crate) fn drive_start(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let Some(cam) = self.starting.take() else {
            return true;
        };
        let (Some(renderer), Some(mut scene)) = (self.renderer.take(), self.scene.take()) else {
            self.starting = Some(cam);
            return true;
        };
        let centers = start_centers(&self.args, &cam, self.world.as_deref());
        let progress = match self.streamer.as_mut() {
            Some(streamer) => {
                streamer.update(
                    &renderer,
                    &mut scene,
                    &centers,
                    std::time::Duration::from_millis(30),
                    None,
                );
                streamer.initial_progress()
            }
            None => None,
        };
        let Some((done, total)) = progress else {
            let w = self.world.clone().expect("world");
            log::info!(
                "map ready after {:.2} s: {} tiles loaded",
                self.started.elapsed().as_secs_f64(),
                w.loaded_tiles().len()
            );
            self.start_world(w, cam, &renderer, &mut scene);
            self.renderer = Some(renderer);
            self.scene = Some(scene);
            return true;
        };
        let name = self
            .world
            .as_ref()
            .map(|w| {
                if w.global.friendly_name.trim().is_empty() {
                    w.global.name.clone()
                } else {
                    w.global.friendly_name.clone()
                }
            })
            .unwrap_or_default();
        let mut reconfigure = false;
        if let (Some(ui), Some(s), Some(win)) = (
            self.ui.as_mut(),
            self.surface.as_ref(),
            self.window.as_ref(),
        ) {
            scene.overlays.clear();
            let dpi = win.scale_factor() as f32;
            let scale = dpi * crate::ui::size_factor(s.config.height as f32, dpi, self.settings.ui_scale, self.settings.ui_scale_window);
            ui.loading(
                &renderer,
                &mut scene,
                s.config.width as f32,
                s.config.height as f32,
                scale,
                name.trim(),
                "",
                done as f32 / total.max(1) as f32,
            );
            let acquired = s.surface.get_current_texture();
            // a swapchain that no longer fits the window (Vulkan says so after the switch
            // to full screen, without a resize event) is made again, as the game's own
            // frames do: left as it was, every later frame of the loading screen failed
            // the same way and its picture stood still until the map was there (#776)
            reconfigure = matches!(acquired, wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost);
            if let wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) = acquired
            {
                let view = frame.texture.create_view(&Default::default());
                // the tiles loaded so far stay out of the picture: the camera looks at nothing
                let blank = Camera {
                    position: DVec3::new(0.0, 0.0, -1.0e6),
                    yaw: 0.0,
                    pitch: -89.0,
                    roll: 0.0,
                    fov_deg: 60.0,
                    near: 0.5,
                    far: 10.0,
                };
                let lighting = omsi_render::Lighting {
                    sky_color: glam::Vec3::new(0.08, 0.10, 0.14),
                    ..Default::default()
                };
                let mut renderer = renderer;
                renderer.render(
                    &mut scene,
                    &view,
                    s.config.width,
                    s.config.height,
                    &blank,
                    &lighting,
                );
                win.pre_present_notify();
                frame.present();
                self.renderer = Some(renderer);
            } else {
                self.renderer = Some(renderer);
            }
            win.request_redraw();
        } else {
            self.renderer = Some(renderer);
        }
        if reconfigure {
            if let (Some(s), Some(r), Some(win)) = (self.surface.as_mut(), self.renderer.as_ref(), self.window.as_ref()) {
                let size = win.inner_size();
                s.resize(r, size.width, size.height);
            }
        }
        self.scene = Some(scene);
        self.starting = Some(cam);
        if let Some(limit) = self.args.exit_after {
            if self.started.elapsed().as_secs_f32() > limit {
                log::info!("exit after {limit} s while loading: {done} of {total} tiles");
                crate::platform::exit(event_loop);
            }
        }
        false
    }

    /// Stream the tiles around the camera and the player's bus (whose ground must stay when
    /// the free camera flies off); when the loaded set changed, hand the new data to whatever
    /// keeps its own copy (the bus's obstacles, the traffic network, the navigator, the
    /// street lamps).
    pub(crate) fn drive_streaming(&mut self) {
        let mut centers: Vec<DVec3> = self.camera.iter().map(|c| c.position).collect();
        centers.extend(self.player.iter().map(|p| p.vehicle.position));
        // a LAN host simulates the world around every player: the ground and the roads there
        if self.lan.as_ref().map(|l| l.role == omsi_net::Role::Host).unwrap_or(false) {
            centers.extend(self.remotes.remotes.values().map(|r| r.vehicle().position));
        }
        let (Some(streamer), Some(w), Some(r), Some(scene)) = (
            self.streamer.as_mut(),
            self.world.as_ref(),
            self.renderer.as_ref(),
            self.scene.as_mut(),
        ) else {
            return;
        };
        // textures uploaded as RGBA to spare a frame, compressed on the workers since, and the
        // texture budget
        w.apply_texture_upgrades(
            r,
            scene,
            Some(Instant::now() + std::time::Duration::from_millis(2)),
        );
        w.update_texture_budget(r, scene, &centers, false);
        if centers.is_empty()
            || !streamer.update(
            r,
            scene,
            &centers,
            std::time::Duration::from_millis(6),
            self.audio.as_ref(),
        )
        {
            return;
        }
        if let Some(p) = self.player.as_mut() {
            // (OMSI's [no_collision]: no solid object stops the bus)
            p.vehicle.collision = self.settings.collision_objects.then(|| w.collision.lock().clone());
            p.vehicle.wheel_walls = self.settings.collision_objects;
        }
        match self.traffic.as_mut() {
            Some(t) => {
                t.add_tiles(w);
            }
            None => {
                // no traffic system: the navigator keeps the roads for its map
                let lanes = std::mem::take(&mut *w.lanes.lock());
                if let Some(n) = self.navigator.as_mut() {
                    n.add_lanes(lanes);
                }
            }
        }
        if let Some(on) = self.lamps_on {
            w.set_lamps(r, scene, on);
        }
    }
}

/// Where a streamed map loads around while it starts: the vehicle's spawn point (a bus must
/// stand on ground when it appears, whatever `--cam` says), and the camera unless the view
/// follows the bus (every view but the free camera does, `--cam` or not).
pub(crate) fn start_centers(args: &Args, cam: &Camera, world: Option<&World>) -> Vec<DVec3> {
    let mut out: Vec<DVec3> = spawn_point(args, world).into_iter().collect();
    let view_in_bus = args.bus.is_some() && args.view != "free";
    if out.is_empty() || !view_in_bus {
        out.push(cam.position);
    }
    out
}

/// What the map needs and this installation lacks, said on the screen and written to
/// `~/.openomsi/missing_content.txt` by add-on folder: a map short of an add-on showed
/// holes, bare roads and white objects, and nobody could tell that from a fault of the game.
pub(crate) fn report_missing_content(w: &World, msg: &mut Option<(String, f32)>) {
    let (files, textures) = w.missing_content();
    if files.is_empty() && textures.is_empty() {
        return;
    }
    // the add-on a file comes with: the folder under Sceneryobjects / Splines / Vehicles
    let addon = |f: &str| f.split('/').take(2).collect::<Vec<_>>().join("/");
    let mut by_addon: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (f, what) in &files {
        by_addon.entry(addon(f)).or_default().push(format!("{what}: {f}"));
    }
    let mut text = format!("openOMSI: content this map uses that is not installed\nmap: {}\n\n", w.map_dir.display());
    for (a, list) in &by_addon {
        text.push_str(&format!("{a} ({} files)\n", list.len()));
        for l in list {
            text.push_str(&format!("  {l}\n"));
        }
    }
    if !textures.is_empty() {
        text.push_str(&format!("\ntextures not found ({}):\n", textures.len()));
        for t in &textures {
            text.push_str(&format!("  {t}\n"));
        }
    }
    let Some(dir) = crate::lan::data_dir() else { return };
    let path = dir.join("missing_content.txt");
    let _ = std::fs::write(&path, text);
    let objects = files.iter().filter(|(_, w)| *w != "spline").count();
    let splines = files.len() - objects;
    let addons: Vec<&String> = by_addon.keys().take(4).collect();
    let more = if by_addon.len() > 4 { format!(" and {} more", by_addon.len() - 4) } else { String::new() };
    log::warn!("missing content: {objects} objects, {splines} splines, {} textures (list: {})", textures.len(), path.display());
    if !files.is_empty() {
        *msg = Some((
            format!(
                "This map uses {objects} objects and {splines} splines that are not installed (add-ons: {}{more}). The list is in {}",
                addons.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(", "),
                path.display()
            ),
            15.0,
        ));
    }
}

/// How long the glide between two cockpit cameras takes (seconds). The eye, the turn of the
/// view and the field of view all follow the same curve over this time. 0 = hard cut.
pub(crate) const CAM_BLEND_SECS: f32 = 0.6;
/// The longest step of time one frame adds to the glide (seconds): a frame that hitches at
/// the start of a switch does not skip ahead in it.
pub(crate) const CAM_BLEND_MAX_DT: f32 = 1.0 / 30.0;

fn wrap_deg(a: f32) -> f32 {
    (a + 180.0).rem_euclid(360.0) - 180.0
}

/// `a` (k = 0) to `b` (k = 1), both cameras fixed in the bus's frame: the eye, the turn of the
/// view and the field of view on a straight way. The bus's own motion (its pitch, bank, the
/// head) is put on the result afterwards, so the glide is the same standing and driving.
pub(crate) fn blend_local(a: &omsi_vehicle::Camera, b: &omsi_vehicle::Camera, k: f32) -> omsi_vehicle::Camera {
    // (measured from `b`: at k = 1 every value is exactly `b`'s - no 360 degree residue of a
    // yaw that went the short way round, no rounding left over for the hand-over to the
    // plain camera to show)
    let k = k.clamp(0.0, 1.0);
    if k >= 1.0 {
        return b.clone();
    }
    let rest = 1.0 - k;
    let l = |x: f32, y: f32| y + (x - y) * rest;
    // The view direction turns along the great circle between the two (a slerp of the
    // directions), not yaw and pitch each on their own straight line: that swept the view
    // out in a bow - up and across at once - while the eye went straight, which looked like
    // a zigzag in the glide.
    let dir = |c: &omsi_vehicle::Camera| {
        let (sy, cy) = c.yaw.to_radians().sin_cos();
        let (sp, cp) = c.pitch.to_radians().sin_cos();
        glam::Vec3::new(sy * cp, cy * cp, sp)
    };
    let (fa, fb) = (dir(a), dir(b));
    let dot = fa.dot(fb).clamp(-1.0, 1.0);
    let (yaw, pitch) = if dot < -0.9995 {
        // (turned right round: no one great circle, so the plain way)
        (b.yaw - wrap_deg(b.yaw - a.yaw) * rest, l(a.pitch, b.pitch))
    } else {
        let f = if dot > 0.9995 {
            (fa * rest + fb * k).normalize_or(fb)
        } else {
            let theta = dot.acos();
            let s = theta.sin();
            ((fa * ((rest * theta).sin() / s)) + (fb * ((k * theta).sin() / s))).normalize_or(fb)
        };
        (f.x.atan2(f.y).to_degrees(), f.z.clamp(-1.0, 1.0).asin().to_degrees())
    };
    omsi_vehicle::Camera {
        pos: [l(a.pos[0], b.pos[0]), l(a.pos[1], b.pos[1]), l(a.pos[2], b.pos[2])],
        dist: l(a.dist, b.dist),
        fov: l(a.fov, b.fov),
        yaw,
        pitch,
        extra: b.extra,
    }
}

/// A bus whose mirrors are frozen, and the seconds since they were first drawn.
pub(crate) struct FrozenMirrors {
    pub(crate) bus: u64,
    pub(crate) since: f32,
}

#[derive(Default)]
pub(crate) struct CamBlend {
    /// (view, camera numbers) of the last frame: a change of the numbers inside the same
    /// view is a camera switch.
    pub key: Option<(String, (usize, usize))>,
    /// The camera the glide started from (in the bus's frame), while one is under way.
    pub from: Option<omsi_vehicle::Camera>,
    /// The cockpit camera as it was drawn last frame (in the bus's frame): where the next
    /// glide starts from.
    pub shown: Option<omsi_vehicle::Camera>,
    /// Just sat down at the wheel (from on foot): the next cockpit frame glides in from where
    /// the walker's eyes were.
    pub entering: bool,
    /// 0..1 progress of the glide.
    pub t: f32,
    /// What the glide's last picture was off from the plain camera by, let go of over a
    /// fraction of a second after the hand-over (so that nothing is left to jump).
    pub carry: Option<CamCarry>,
}

/// A small difference between two pictures of the camera (world position, angles in degrees,
/// field of view) that is eased out instead of being cut.
#[derive(Clone, Copy)]
pub(crate) struct CamCarry {
    pub pos: glam::DVec3,
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    pub fov: f32,
}

impl CamCarry {
    /// `a` minus `b`.
    pub fn between(a: &omsi_render::Camera, b: &omsi_render::Camera) -> Self {
        Self {
            pos: a.position - b.position,
            yaw: wrap_deg(a.yaw - b.yaw),
            pitch: a.pitch - b.pitch,
            roll: wrap_deg(a.roll - b.roll),
            fov: a.fov_deg - b.fov_deg,
        }
    }

    /// Put on a camera.
    pub fn apply(&self, c: &mut omsi_render::Camera) {
        c.position += self.pos;
        c.yaw += self.yaw;
        c.pitch = (c.pitch + self.pitch).clamp(-89.0, 89.0);
        c.roll += self.roll;
        c.fov_deg += self.fov;
    }

    /// Ease out by one frame; false once nothing is left.
    pub fn decay(&mut self, dt: f32) -> bool {
        let k = (-dt.clamp(0.0, 0.1) * 12.0).exp();
        self.pos *= k as f64;
        self.yaw *= k;
        self.pitch *= k;
        self.roll *= k;
        self.fov *= k;
        self.pos.length() > 1e-4 || self.yaw.abs() > 0.01 || self.pitch.abs() > 0.01 || self.roll.abs() > 0.01 || self.fov.abs() > 0.01
    }
}

impl CamBlend {
    /// How far along the way from the old camera to the new one: smootherstep of the time
    /// (no jolt in speed or acceleration at either end).
    pub fn progress(&self) -> f32 {
        let t = self.t.clamp(0.0, 1.0);
        t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
    }
}
